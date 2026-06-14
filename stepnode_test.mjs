// Dev harness for the STEP importer. Two parts:
//  1) BASE tessellation sweep (truck surface-deviation knob) -> writes STLs.
//  2) END-TO-END: build a Model from the STEP bytes and from the reference STL
//     and compare the REFINED working meshes — STEP should now be refined by the
//     exact same path as STL (diag/60 subdivision in Model::new).
// Build the Node wasm pkg first (from repo root):
//   wasm-pack build crates/sig-wasm --target nodejs --release --out-dir pkg-stepnode -- --features step
// Then run:  node stepnode_test.mjs
import { readFileSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const wasm = require('./crates/sig-wasm/pkg-stepnode/sig_wasm.js');

const stepPath = process.argv[2] ?? 'hook5 v3.step';
const stlPath = process.argv[3] ?? 'hook5 v3.stl';
const stepBytes = readFileSync(stepPath);
const stlBytes = readFileSync(stlPath);

function parseStl(buf) {
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const n = dv.getUint32(80, true);
  const tris = [];
  for (let i = 0; i < n; i++) {
    const b = 84 + 50 * i + 12;
    const t = new Array(9);
    for (let k = 0; k < 9; k++) t[k] = dv.getFloat32(b + 4 * k, true);
    tris.push(t);
  }
  return tris;
}
// Float32Array of 9-per-tri positions -> triangle array.
function trisFromPositions(pos) {
  const tris = [];
  for (let i = 0; i < pos.length; i += 9) tris.push(Array.from(pos.subarray(i, i + 9)));
  return tris;
}
function writeStlFile(path, tris) {
  const buf = Buffer.alloc(84 + 50 * tris.length);
  buf.writeUInt32LE(tris.length, 80);
  let o = 84;
  for (const t of tris) {
    o += 12; // normal left zero
    for (let k = 0; k < 9; k++) { buf.writeFloatLE(t[k], o); o += 4; }
    o += 2; // attribute byte count
  }
  writeFileSync(path, buf);
}

function stats(tris) {
  const lo = [Infinity, Infinity, Infinity], hi = [-Infinity, -Infinity, -Infinity];
  const edges = [], minAng = [];
  const d = (u, v) => Math.hypot(u[0] - v[0], u[1] - v[1], u[2] - v[2]);
  for (const t of tris) {
    const a = [t[0], t[1], t[2]], b = [t[3], t[4], t[5]], c = [t[6], t[7], t[8]];
    for (const p of [a, b, c]) for (let k = 0; k < 3; k++) { lo[k] = Math.min(lo[k], p[k]); hi[k] = Math.max(hi[k], p[k]); }
    const la = d(b, c), lb = d(a, c), lc = d(a, b);
    edges.push(la, lb, lc);
    const ang = (opp, x, y) => (x < 1e-12 || y < 1e-12) ? 0
      : Math.acos(Math.min(1, Math.max(-1, (x * x + y * y - opp * opp) / (2 * x * y)))) * 180 / Math.PI;
    minAng.push(Math.min(ang(la, lb, lc), ang(lb, la, lc), ang(lc, la, lb)));
  }
  const diag = Math.hypot(hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]);
  const mean = edges.reduce((s, e) => s + e, 0) / Math.max(1, edges.length);
  const variance = edges.reduce((s, e) => s + (e - mean) ** 2, 0) / Math.max(1, edges.length);
  const cov = mean > 0 ? Math.sqrt(variance) / mean : 0;
  const sorted = [...edges].sort((x, y) => x - y);
  minAng.sort((x, y) => x - y);
  const pct = (arr, p) => arr.length ? arr[Math.round((arr.length - 1) * p)] : 0;
  return {
    tris: tris.length, diag, edgeMean: mean, cov,
    edgeMin: sorted[0] ?? 0, edgeMax: sorted[sorted.length - 1] ?? 0,
    angP5: pct(minAng, 0.05), angMed: pct(minAng, 0.5),
    sliver: minAng.filter(a => a < 15).length / Math.max(1, minAng.length),
  };
}

const HEAD = 'case'.padEnd(24) + 'tris'.padStart(8) + 'edgeMean'.padStart(10) + 'CoV'.padStart(7) +
  'edgeMin'.padStart(8) + 'edgeMax'.padStart(9) + 'angP5'.padStart(8) + 'angMed'.padStart(8) + 'slivers'.padStart(9);
function row(label, s) {
  console.log(label.padEnd(24) + String(s.tris).padStart(8) + s.edgeMean.toFixed(3).padStart(10) +
    s.cov.toFixed(2).padStart(7) + s.edgeMin.toFixed(3).padStart(8) + s.edgeMax.toFixed(2).padStart(9) +
    s.angP5.toFixed(1).padStart(8) + s.angMed.toFixed(1).padStart(8) + (s.sliver * 100).toFixed(1).padStart(8) + '%');
}

console.log(`STEP: ${stepPath} (${(stepBytes.length / 1024) | 0} KB)\n`);
console.log('== 1) BASE tessellation (truck surface-deviation knob, pre-refinement) ==');
console.log(HEAD);
for (const [label, dev, fname] of [
  ['auto', 0, 'hook_truck_auto.stl'],
  ['dev=0.02 mm', 0.02, 'hook_truck_dev0.02.stl'],
  ['dev=0.005 mm', 0.005, 'hook_truck_dev0.005.stl'],
]) {
  try {
    const info = JSON.parse(wasm.step_import_info(stepBytes, dev));
    const stl = Buffer.from(wasm.step_import_stl(stepBytes, dev));
    row(label, stats(parseStl(stl)));
    writeFileSync(fname, stl);
    if (label === 'auto') console.log(`  [shells=${info.shells} faces=${info.faces} tol=${info.tolerance.toFixed(4)} mm]`);
  } catch (e) { console.log(label.padEnd(24) + ' ERROR: ' + e.message); }
}

console.log('\n== 2) END-TO-END Model (refined working mesh — STEP should match STL handling) ==');
console.log(HEAD);
const mStep = new wasm.Model(stepBytes, 'hook-step');
const trisStep = trisFromPositions(mStep.positions());
row('Model(STEP)', stats(trisStep));
writeStlFile('hook_model_step_refined.stl', trisStep);
const trisStl = trisFromPositions(new wasm.Model(stlBytes, 'hook-stl').positions());
row('Model(STL ref)', stats(trisStl));
writeStlFile('hook_model_stl_refined.stl', trisStl);
console.log('  wrote hook_model_step_refined.stl / hook_model_stl_refined.stl (the actual working meshes)');

console.log('\n== 3) Surface-patch source (CAD faces vs crease angle) ==');
console.log(`has_cad_faces: ${mStep.has_cad_faces()}`);
// Default for STEP should already be CAD faces.
console.log(`default patches (CAD faces): ${mStep.patch_count()}`);
mStep.resegment(10);
console.log(`after resegment(10°):        ${mStep.patch_count()} patches`);
mStep.use_cad_faces();
console.log(`after use_cad_faces():       ${mStep.patch_count()} patches`);
// A picked triangle's patch id = its CAD face; count tris in the largest face.
const ids = mStep.patch_ids();
const hist = new Map();
for (const id of ids) hist.set(id, (hist.get(id) ?? 0) + 1);
const sizes = [...hist.values()].sort((a, b) => b - a);
console.log(`distinct CAD-face patches touched: ${hist.size}, largest face = ${sizes[0]} tris, smallest = ${sizes[sizes.length - 1]}`);

console.log(
  '\nLegend: edge_* in mm. CoV = stddev/mean edge length (lower = more uniform).\n' +
  'angP5/angMed = 5th-pct / median per-triangle MIN angle (deg). slivers = % min-angle < 15.\n' +
  'Both Model rows use the identical diag/60 refinement; STEP now flows through it like STL.'
);
