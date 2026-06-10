// Functional smoke test of the wasm-bindgen Model API (the same calls the web
// worker makes). Run: node smoke-wasm.mjs
import { readFileSync } from "node:fs";
import init, { Model } from "./web/src/wasm/sig_wasm.js";

// --- build a binary STL box (matches sig-core primitives::boxx layout) ---
function boxStl(lo, hi) {
  const v = (x, y, z) => [x ? hi[0] : lo[0], y ? hi[1] : lo[1], z ? hi[2] : lo[2]];
  const faces = [
    [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]], // -x
    [[1, 0, 0], [1, 1, 0], [1, 1, 1], [1, 0, 1]], // +x
    [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]], // -y
    [[0, 1, 0], [0, 1, 1], [1, 1, 1], [1, 1, 0]], // +y
    [[0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 0, 0]], // -z
    [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]], // +z
  ];
  const tris = [];
  for (const f of faces) {
    const c = f.map(([x, y, z]) => v(x, y, z));
    tris.push([c[0], c[1], c[2]], [c[0], c[2], c[3]]);
  }
  const buf = new ArrayBuffer(84 + 50 * tris.length);
  const dv = new DataView(buf);
  dv.setUint32(80, tris.length, true);
  let off = 84;
  for (const t of tris) {
    off += 12; // skip normal
    for (const p of t) for (const c of p) {
      dv.setFloat32(off, c, true);
      off += 4;
    }
    off += 2;
  }
  return new Uint8Array(buf);
}

const wasmBytes = readFileSync(new URL("./web/src/wasm/sig_wasm_bg.wasm", import.meta.url));
await init({ module_or_path: wasmBytes });

const stl = boxStl([0, 0, 0], [40, 6, 6]);
const model = new Model(stl);

const assert = (cond, msg) => {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
  console.log(`ok: ${msg}`);
};

assert(model.triangle_count() === 12, "12 triangles parsed");
assert(model.patch_count() === 6, `6 patches segmented (got ${model.patch_count()})`);
assert(model.positions().length === 108, "positions buffer 12*9");

const bbox = Array.from(model.bbox());
assert(Math.abs(bbox[3] - 40) < 1e-6, "bbox hi.x = 40");

// face order: -x tris [0,1], +x tris [2,3]
model.set_material(2000, 0.3, 1.24);
model.set_resolution(50000);

// Under-constrained: force only.
model.add_force(new Uint32Array([2, 3]), 0, 0, -5);
let report = JSON.parse(model.check());
assert(report.ok === false, "force-only setup flagged under-constrained");
assert(report.components[0].mode !== null, "free rigid-body mode reported");
const mode = report.components[0].mode;
assert(Array.isArray(mode.t) && Array.isArray(mode.r), "mode has t and r vectors");

// Proper cantilever.
model.clear_bcs();
model.add_fixed(new Uint32Array([0, 1]));
model.add_force(new Uint32Array([2, 3]), 0, 0, -5);
report = JSON.parse(model.check());
assert(report.ok === true, "cantilever setup passes check");
assert(report.islandCount === 1, "one body");

const info = JSON.parse(model.voxel_info());
console.log(`   grid ${info.nx}x${info.ny}x${info.nz}, h=${info.h.toFixed(3)} mm, ${info.solid} solid cells`);

const t0 = performance.now();
const stats = JSON.parse(model.solve());
const dt = ((performance.now() - t0) / 1000).toFixed(2);
console.log(`   solve: ${stats.iterations} iters, res ${stats.relResidual.toExponential(1)}, ${dt} s`);
assert(stats.maxDisplacement > 0.01 && stats.maxDisplacement < 10, `sane max displacement (${stats.maxDisplacement.toFixed(4)} mm)`);

const disp = model.vertex_displacements();
assert(disp.length === 108, "per-vertex displacement buffer");
// Tip vertices (x=40) deflect downward; root (x=0) stays.
let tipUz = 0, tipN = 0, rootUz = 0, rootN = 0;
const pos = model.positions();
for (let v = 0; v < 36; v++) {
  if (Math.abs(pos[3 * v] - 40) < 1e-3) { tipUz += disp[3 * v + 2]; tipN++; }
  if (Math.abs(pos[3 * v]) < 1e-3) { rootUz += Math.abs(disp[3 * v + 2]); rootN++; }
}
tipUz /= tipN;
rootUz /= rootN;
assert(tipUz < -0.05, `tip deflects down (${tipUz.toFixed(4)} mm)`);
assert(rootUz < 1e-4, `root stays put (${rootUz.toExponential(1)} mm)`);

// Gravity + frictionless paths execute.
model.set_gravity(true);
model.add_frictionless(new Uint32Array([4, 5]));
model.add_pressure(new Uint32Array([10, 11]), 0.05);
JSON.parse(model.check());
JSON.parse(model.solve());
console.log("ok: gravity + frictionless + pressure paths solve");

// Resegmentation.
model.resegment(60);
assert(model.patch_count() === 6, "resegment at 60 deg still 6 patches");

console.log("\nALL SMOKE TESTS PASSED");
