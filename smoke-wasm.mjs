// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Functional smoke test of the wasm-bindgen Model API (the same calls the web
// worker makes). Run: node smoke-wasm.mjs
import { readFileSync } from "node:fs";
// fflate lives under web/node_modules (not resolvable bare from repo root).
import { unzipSync } from "./web/node_modules/fflate/esm/browser.js";
import init, {
  Model,
  set_cancel_flag,
  set_progress_buffer,
  project_manifest,
  project_model,
} from "./web/src/wasm/filasim_wasm.js";

// --- build a binary STL box (matches filasim-core primitives::boxx layout) ---
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

const wasmBytes = readFileSync(new URL("./web/src/wasm/filasim_wasm_bg.wasm", import.meta.url));
await init({ module_or_path: wasmBytes });

const assert = (cond, msg) => {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
  console.log(`ok: ${msg}`);
};

// The display/analysis mesh is subdivided at load, so BC selections are made
// like the UI does it: whole segmentation patches, found geometrically.
function patchSelector(model) {
  const pids = model.patch_ids();
  const pos = model.positions();
  return (axis, extreme) => {
    const sum = new Map();
    const cnt = new Map();
    for (let t = 0; t < pids.length; t++) {
      const c = (pos[9 * t + axis] + pos[9 * t + 3 + axis] + pos[9 * t + 6 + axis]) / 3;
      sum.set(pids[t], (sum.get(pids[t]) ?? 0) + c);
      cnt.set(pids[t], (cnt.get(pids[t]) ?? 0) + 1);
    }
    let best = null;
    let bestVal = extreme === "min" ? Infinity : -Infinity;
    for (const [p, s] of sum) {
      const m = s / cnt.get(p);
      if (extreme === "min" ? m < bestVal : m > bestVal) {
        bestVal = m;
        best = p;
      }
    }
    const out = [];
    for (let t = 0; t < pids.length; t++) if (pids[t] === best) out.push(t);
    return new Uint32Array(out);
  };
}

const stl = boxStl([0, 0, 0], [40, 6, 6]);
const model = new Model(stl, "beam");
const nTri = model.triangle_count();

assert(nTri > 5000 && nTri <= 160_000, `coarse STL subdivided for display (${nTri} tris)`);
assert(model.patch_count() === 6, `6 patches segmented (got ${model.patch_count()})`);
assert(model.positions().length === nTri * 9, "positions buffer 9 floats/tri");
assert(model.original_positions().length === 12 * 9,
  "original (pre-refinement, conforming) soup exposed for viewport edge detection");
assert(model.body_count() === 1, `single box is one body (got ${model.body_count()})`);

// Multi-body detection: two disconnected boxes in one STL → UI warns that the
// solver can't connect separate bodies.
{
  const a = boxStl([0, 0, 0], [10, 10, 10]);
  const b = boxStl([30, 0, 0], [40, 10, 10]);
  const two = new Uint8Array(a.length + b.length - 84); // drop b's header
  two.set(a);
  two.set(b.subarray(84), a.length);
  new DataView(two.buffer).setUint32(80, 24, true); // triangle count 12+12
  const m2 = new Model(two, "two boxes");
  assert(m2.body_count() === 2, `two separated boxes detected (got ${m2.body_count()})`);
  m2.free();
}

const bbox = Array.from(model.bbox());
assert(Math.abs(bbox[3] - 40) < 1e-4, "bbox hi.x = 40");

const sel = patchSelector(model);
model.set_material(2000, 0.3, 1.24, 50, 35);
model.set_resolution(50000);

// Under-constrained: force only.
model.add_force(sel(0, "max"), 0, 0, -5);
let report = JSON.parse(model.check());
assert(report.ok === false, "force-only setup flagged under-constrained");
assert(report.components[0].mode !== null, "free rigid-body mode reported");
const mode = report.components[0].mode;
assert(Array.isArray(mode.t) && Array.isArray(mode.r), "mode has t and r vectors");

// Proper cantilever.
model.clear_bcs();
model.add_fixed(sel(0, "min"));
model.add_force(sel(0, "max"), 0, 0, -5);
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
// Residual trace for the nerd-log convergence chart: element 0 is the
// initial residual, one entry per CG iteration after that, monotone-ish down.
assert(Array.isArray(stats.residuals) && stats.residuals.length === stats.iterations + 1,
  `residual trace has ${stats.iterations + 1} entries (got ${stats.residuals?.length})`);
assert(stats.residuals[stats.residuals.length - 1] <= stats.residuals[0],
  "residual trace decreases");

const disp = model.vertex_displacements();
assert(disp.length === nTri * 9, "per-vertex displacement buffer");

// Live residual streaming: install the shared progress buffer the way the
// worker does and re-solve. Regression guard — the sink copied the growing
// trace into the full-capacity view, tripping copy_from's dest.len()==src.len()
// assert and trapping as "unreachable". It must now fill without panicking.
{
  const CAP = 1024;
  const psab = new SharedArrayBuffer(4 + CAP * 4);
  const pcount = new Int32Array(psab, 0, 1);
  const pdata = new Float32Array(psab, 4, CAP);
  set_progress_buffer(pcount, pdata);
  const ps = JSON.parse(model.solve());
  assert(pcount[0] >= 1 && pcount[0] <= ps.iterations + 1,
    `live residual trace streamed without panicking (${pcount[0]} points)`);
  assert(pdata[0] > 0 && Number.isFinite(pdata[Math.max(0, pcount[0] - 1)]),
    "streamed residuals finite, starting at the initial residual");
}
console.log("ok: live residual progress streaming (shared buffer)");

// Stress/strain result fields.
const fmax = (a) => a.reduce((m, v) => Math.max(m, v), -Infinity);
const fmin = (a) => a.reduce((m, v) => Math.min(m, v), Infinity);
const vmf = model.result_field("vm");
assert(vmf.length === nTri * 3 && vmf.every((v) => Number.isFinite(v)), "von Mises field per vertex");
assert(fmax(vmf) > 0, "von Mises has nonzero peak");
const sxxf = model.result_field("sxx");
assert(fmin(sxxf) < 0 && fmax(sxxf) > 0, "bending: sigma_xx tension + compression present");
const svmf = model.result_field("svm");
assert(svmf.length === nTri * 3 && svmf.every((v, i) => Math.abs(Math.abs(v) - vmf[i]) < 1e-3),
  "signed von Mises has the von Mises magnitude");
assert(fmin(svmf) < 0 && fmax(svmf) > 0, "bending: signed von Mises spans compression + tension");
const ezzf = model.result_field("ezz");
assert(ezzf.length === nTri * 3, "strain field per vertex");
// Safety factor: sigma_t·rel(rho) / sigma_vM, capped at 10 (SF_CAP).
const sff = model.result_field("sf");
assert(sff.length === nTri * 3 && sff.every((v) => Number.isFinite(v) && v > 0 && v <= 10),
  "safety factor field per vertex (finite, positive, capped)");
assert(fmin(sff) > 1 && fmin(sff) <= 10,
  `min safety factor sensible for a lightly loaded beam (${fmin(sff).toFixed(1)})`);

// --- orientation sweep (DESIGN §15): single-solve pitch/roll layer-SF grid ---
{
  const meta = JSON.parse(model.orientation_sweep_begin([], 15)); // coarse for speed
  assert(meta.n === 13 && meta.pixels === 169, `15° hemisphere grid is 13×13 (${meta.n})`);
  assert(meta.cellsKept > 0 && meta.cellsKept <= meta.cellsSeen,
    `prune keeps a sane subset (${meta.cellsKept}/${meta.cellsSeen})`);
  assert(meta.scoredCells <= meta.cellsKept, "ring cells counted apart from scored");
  const scored = new Float32Array(meta.pixels);
  const all = new Float32Array(meta.pixels);
  for (let s = 0; s < meta.pixels; s += 40) { // chunked like the worker will
    const [sc, al] = model.orientation_sweep_rows(s, 40);
    scored.set(sc, s);
    all.set(al, s);
  }
  assert(scored.every((v) => Number.isFinite(v) && v > 0 && v <= 10) &&
         all.every((v) => Number.isFinite(v) && v > 0 && v <= 10),
    "sweep values finite, positive, capped");
  // The scored value ignores the constraint ring, so per pixel it can only be
  // the same or SAFER than the hide-nothing value.
  assert(scored.every((v, i) => v >= all[i] - 1e-6), "scored >= all per pixel");
  // Flip symmetry: roll −90° and +90° are the same layer plane (n vs −n).
  for (let ip = 0; ip < meta.n; ip++) {
    const l = all[ip * meta.n];
    const r = all[ip * meta.n + meta.n - 1];
    assert(Math.abs(l - r) < 1e-4, `flip symmetry at pitch row ${ip} (${l} vs ${r})`);
  }
  // Center pixel is n = ẑ — the same criterion the sfz field paints, but as a
  // min over ALL cells (interior included), so it can only be <= the
  // surface-sampled field minimum.
  const center = all[Math.floor(meta.pixels / 2)];
  const sfzField = model.result_field("sfz");
  const sfzMin = fmin(sfzField);
  assert(center <= sfzMin + 1e-3, `center pixel ${center.toFixed(3)} <= min sfz ${sfzMin.toFixed(3)}`);
  // The preview field at n = ẑ IS the sfz field: same criterion, same surface
  // sampling (SF is invariant to the display stress-factor choice).
  const previewZ = model.layer_sf_field(0, 0, 1, []);
  assert(previewZ.length === sfzField.length &&
         previewZ.every((v, i) => Math.abs(v - sfzField[i]) < 1e-3 * Math.max(1, sfzField[i])),
    "layer_sf_field(z) matches the sfz result field");
  // Shear toggle: OFF = pure tension-across-layers — dropping a failure mode
  // can only make sfz safer, never more critical.
  model.set_layer_shear(false);
  const sfzNoShear = model.result_field("sfz");
  assert(sfzNoShear.every((v, i) => v >= sfzField[i] - 1e-4),
    "shear off => sfz never more critical");
  model.set_layer_shear(true);
  // Physics: a cantilever bends about Y, so layers ⊥ X (roll ±90°) see the full
  // σxx as inter-layer tension — strictly worse than printing flat (n = ẑ).
  let edgeMin = Infinity;
  for (let ip = 0; ip < meta.n; ip++) edgeMin = Math.min(edgeMin, all[ip * meta.n]);
  assert(edgeMin < center, `layers ⊥ X (${edgeMin.toFixed(2)}) worse than flat (${center.toFixed(2)})`);
  model.orientation_sweep_end();
  let threw = false;
  try { model.orientation_sweep_rows(0, 1); } catch { threw = true; }
  assert(threw, "rows after end rejects");
}
console.log("ok: orientation sweep (begin / chunked rows / end)");

// --- color 3MF export: the active field painted into discrete filament bands ---
// Decode a whole-triangle Bambu/Orca paint_color leaf code back to its 1-based
// filament state (mirror of the verified Rust encoder) to confirm the painted
// bands round-trip and stay within 1..steps.
function decodePaintLeaf(code) {
  const nibs = [...code].map((c) => parseInt(c, 16)).reverse();
  const bits = [];
  for (const v of nibs) for (let i = 0; i < 4; i++) bits.push((v >> i) & 1);
  let i = 0;
  const take = (n) => { let x = 0; for (let k = 0; k < n; k++) x |= bits[i++] << k; return x; };
  if (take(2) !== 0) return null; // not a leaf
  const s = take(2);
  if (s < 3) return s;
  let val = 0, sh = 0;
  for (;;) { val |= take(3) << sh; sh += 3; if (take(1) === 0) break; }
  return 3 + val;
}
{
  const lo = fmin(vmf), hi = fmax(vmf);
  const steps = 5;
  const colors = ["#0000ff", "#2bd6c0", "#7bff45", "#ffd400", "#de4343"];
  const cmf = model.export_color_3mf(
    "vm", lo, hi, steps, JSON.stringify(colors), new Uint8Array());
  assert(cmf.length > 500 && cmf[0] === 0x50 && cmf[1] === 0x4b, "color 3MF export is a zip");
  // The model/config parts are deflate-compressed — unzip to read them.
  const parts = unzipSync(cmf);
  const objText = Buffer.from(parts["3D/Objects/object_1.model"]).toString("latin1");
  const projText = Buffer.from(parts["Metadata/project_settings.config"]).toString("latin1");
  assert(objText.includes("paint_color="), "color 3MF carries painted triangles");
  assert(projText.includes("filament_colour"), "color 3MF defines filaments");
  for (const c of colors)
    assert(projText.includes(c.toUpperCase()) || projText.includes(c),
      `filament color ${c} embedded`);
  // Every painted code decodes to a valid band state in 2..steps (band 0 = base,
  // left unpainted), and the field's variation yields more than one band.
  const codes = [...objText.matchAll(/paint_color="([0-9A-Fa-f]+)"/g)].map((m) => m[1]);
  assert(codes.length > 0, "color 3MF has painted triangles to decode");
  const states = new Set();
  let bad = 0;
  for (const c of codes) {
    const st = decodePaintLeaf(c);
    if (st === null || st < 2 || st > steps) bad++;
    else states.add(st);
  }
  assert(bad === 0, `all ${codes.length} paint codes decode to states in 2..${steps}`);
  assert(states.size >= 1, `the von Mises field is banded (${states.size} band(s))`);

  // Watertightness: the export refines mesh_orig conformingly (watertight) and
  // cuts it along the iso-lines. Because a shared edge's crossing depends only on
  // its endpoint scalars, the cut stays watertight: for the closed box every edge
  // of the core-welded exported mesh must be shared by exactly 2 triangles.
  const tris = [...objText.matchAll(/<triangle v1="(\d+)" v2="(\d+)" v3="(\d+)"/g)]
    .map((m) => [+m[1], +m[2], +m[3]]);
  const edgeCount = new Map();
  for (const [a, b, c] of tris)
    for (const [u, v] of [[a, b], [b, c], [c, a]]) {
      const e = u < v ? `${u}_${v}` : `${v}_${u}`;
      edgeCount.set(e, (edgeCount.get(e) ?? 0) + 1);
    }
  let boundary = 0, nonmanifold = 0;
  for (const n of edgeCount.values()) { if (n === 1) boundary++; else if (n > 2) nonmanifold++; }
  assert(boundary === 0, `iso-cut export is watertight — 0 boundary edges (${tris.length} tris)`);
  assert(nonmanifold === 0, `iso-cut export is manifold — 0 over-shared edges`);
}

// Tip vertices (x=40) deflect downward; root (x=0) stays.
let tipUz = 0, tipN = 0, rootUz = 0, rootN = 0;
const pos = model.positions();
for (let v = 0; v < nTri * 3; v++) {
  if (Math.abs(pos[3 * v] - 40) < 1e-3) { tipUz += disp[3 * v + 2]; tipN++; }
  if (Math.abs(pos[3 * v]) < 1e-3) { rootUz += Math.abs(disp[3 * v + 2]); rootN++; }
}
tipUz /= tipN;
rootUz /= rootN;
assert(tipUz < -0.05, `tip deflects down (${tipUz.toFixed(4)} mm)`);
assert(rootUz < 1e-3, `root stays put (${rootUz.toExponential(1)} mm)`);

// Results on the voxel mesh: hull + exact nodal displacements + per-cell field.
{
  const vr = model.voxel_results(false);
  const vpos = vr[0], vdisp = vr[1], vedges = vr[2], vedisp = vr[3];
  assert(vpos.length > 0 && vpos.length % 9 === 0, `voxel result hull (${vpos.length / 9} tris)`);
  assert(vdisp.length === vpos.length, "hull displacement per vertex");
  assert(vedges.length > 0 && vedisp.length === vedges.length, "edge displacements match edges");
  let vmax = 0;
  for (let i = 0; i < vdisp.length; i += 3)
    vmax = Math.max(vmax, Math.hypot(vdisp[i], vdisp[i + 1], vdisp[i + 2]));
  assert(Math.abs(vmax - stats.maxDisplacement) < 0.05 * stats.maxDisplacement + 1e-6,
    `voxel nodal max |u| matches solve (${vmax.toFixed(4)} vs ${stats.maxDisplacement.toFixed(4)})`);
  const vvm = model.voxel_result_field("vm", false);
  assert(vvm.length === vpos.length / 3, "voxel field value per hull vertex");
  assert(vvm.every((v) => Number.isFinite(v)) && fmax(vvm) > 0, "voxel von Mises sane");
  // Flat per-cell coloring: all 3 vertices of a triangle share one value.
  for (let t = 0; t < 30; t++)
    assert(vvm[3 * t] === vvm[3 * t + 1] && vvm[3 * t] === vvm[3 * t + 2],
      "per-cell flat values on the voxel hull");
}
console.log("ok: voxel-mesh result view (nodal displacements + per-cell fields)");

// Frictionless + pressure paths execute.
model.add_frictionless(sel(1, "min"));
model.add_pressure(sel(2, "max"), 0.05);
JSON.parse(model.check());
JSON.parse(model.solve());
console.log("ok: frictionless + pressure paths solve");

// Elastic (Winkler) support: springs-only constraint passes the RBM check
// and solves — no Dirichlet nodes anywhere.
model.clear_bcs();
model.add_elastic(sel(2, "min"), 50);
model.add_force(sel(2, "max"), 0, 0, -5);
report = JSON.parse(model.check());
assert(report.ok === true, "elastic springs alone constrain the part");
{
  const estats = JSON.parse(model.solve());
  assert(estats.converged && estats.maxDisplacement > 0, "elastic-support solve converges");
}
console.log("ok: elastic foundation path solves");

// ---- as-printed verify: voxel snap + skin/infill solve ----
model.clear_bcs();
model.add_fixed(sel(0, "min"));
model.add_force(sel(0, "max"), 0, 0, -5);
{
  const sSolid = JSON.parse(model.solve());
  const sfSolidMin = fmin(model.result_field("sf"));

  // Snap the voxel size: 2 x 0.45 mm wall -> h = wall/k exactly.
  model.set_snap_wall(0.9);
  const infoSnap = JSON.parse(model.voxel_info());
  const k = Math.round(0.9 / infoSnap.h);
  assert(k >= 1 && Math.abs(k * infoSnap.h - 0.9) < 1e-9,
    `voxel size snapped to wall/${k} (h=${infoSnap.h.toFixed(3)} mm)`);

  const t2 = performance.now();
  const ps = JSON.parse(model.solve_printed(JSON.stringify({
    infillPct: 25, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
  })));
  console.log(
    `   printed solve: ${ps.iterations} iters in ${((performance.now() - t2) / 1000).toFixed(1)} s, ` +
    `max |u| ${ps.maxDisplacement.toFixed(4)} mm, mass ${ps.massGrams.toFixed(1)} g of ${ps.massSolidGrams.toFixed(1)} g solid`);
  assert(ps.converged && ps.maxDisplacement > 0, "printed solve converges");
  assert(ps.skinLayers === k, `skin resolved by exactly ${k} cell layers (got ${ps.skinLayers})`);
  assert(ps.interiorCells > 0 && ps.skinCells > 0, "skin and interior both present");
  assert(ps.massGrams < ps.massSolidGrams, "printed part lighter than solid");
  assert(ps.maxDisplacement > sSolid.maxDisplacement * 1.05,
    `25% infill bends more than solid (${ps.maxDisplacement.toFixed(4)} vs ${sSolid.maxDisplacement.toFixed(4)} mm)`);
  assert(Array.isArray(ps.residuals) && ps.residuals.length === ps.iterations + 1,
    "printed solve carries the residual trace");

  // Stress/SF on the printed solution use the homogenized eps.
  const sfPrinted = model.result_field("sf");
  const sfPrintedMin = fmin(sfPrinted);
  assert(sfPrinted.every((v) => Number.isFinite(v) && v > 0 && v <= 10), "printed SF field sane");
  assert(sfPrintedMin < sfSolidMin,
    `printed min SF below solid's (${sfPrintedMin.toFixed(1)} < ${sfSolidMin.toFixed(1)})`);

  // Anisotropic strength: sf = elementwise worst of material (sigma_vM vs
  // sigma_t) and layer adhesion (tension sigma_zz vs sigma_t_z).
  const sfm = model.result_field("sfm");
  const sfz = model.result_field("sfz");
  let worstOk = sfm.length === sfPrinted.length && sfz.length === sfPrinted.length;
  for (let i = 0; worstOk && i < sfPrinted.length; i++) {
    if (Math.abs(sfPrinted[i] - Math.min(sfm[i], sfz[i])) > 1e-3) worstOk = false;
  }
  assert(worstOk, "sf = elementwise min(sfm, sfz)");
  assert(fmin(sfm) >= sfPrintedMin - 1e-6 && fmin(sfz) >= sfPrintedMin - 1e-6,
    `worst SF is the most conservative (m ${fmin(sfm).toFixed(1)}, z ${fmin(sfz).toFixed(1)}, worst ${sfPrintedMin.toFixed(1)})`);

  // Voxel mesh with element density + voxel-true section cut: skin cells
  // carry 1.0, exposed interior cells the uniform infill ratio (25%).
  const full = model.voxel_mesh_cut(false, 0, 0, 0, 0, 0.9, 1.0, 25);
  const fullPos = full[0], fullDensity = full[1], fullEdges = full[2];
  assert(fullPos.length > 0 && fullPos.length % 9 === 0, "voxel mesh positions (9 floats/tri)");
  assert(fullDensity.length === fullPos.length / 3, "element density one value per vertex");
  assert(fullEdges.length > 0 && fullEdges.length % 6 === 0, "voxel mesh edges");
  assert(fullDensity.every((v) => v > 0.5), "uncut hull shows only skin cells (all faces touch the surface)");
  // Drop the half with x > 20 (three.js plane convention: keep n·p + c >= 0).
  const cutArr = model.voxel_mesh_cut(true, -1, 0, 0, 20, 0.9, 1.0, 25);
  const cutPos = cutArr[0], cutDensity = cutArr[1];
  assert(cutPos.length > 0 && cutPos.length < fullPos.length, "cut mesh is a strict subset");
  const interior = cutDensity.reduce((a, v) => a + (Math.abs(v - 0.25) < 1e-6 ? 1 : 0), 0);
  const interiorShare = interior / cutDensity.length;
  assert(interiorShare > 0.02,
    `voxel cut exposes interior cells at the infill density (${(100 * interiorShare).toFixed(1)}% of cut-mesh vertices)`);

  // Cooperative cancellation: a pre-set flag makes the next solve bail at
  // its first CG iteration with "cancelled"; clearing it restores solving.
  const cancelFlag = new Int32Array(new SharedArrayBuffer(4));
  set_cancel_flag(cancelFlag);
  Atomics.store(cancelFlag, 0, 1);
  let cancelled = false;
  try {
    model.solve();
  } catch (e) {
    cancelled = /cancelled/i.test(String(e));
  }
  assert(cancelled, "pre-set cancel flag aborts the solve with 'cancelled'");
  Atomics.store(cancelFlag, 0, 0);
  const after = JSON.parse(model.solve());
  assert(after.converged, "solving works again after the flag clears");

  // Smoothed stress display: same field nodal-averaged + surface-sampled —
  // same length, finite everywhere, and averaging never raises the peak.
  model.set_smooth_stress(true);
  const vmSmooth = model.result_field("vm");
  model.set_smooth_stress(false);
  const vmFlat = model.result_field("vm");
  assert(vmSmooth.length === vmFlat.length, "smoothed field has the same vertex count");
  assert(vmSmooth.every((v) => Number.isFinite(v)), "smoothed field finite everywhere");
  assert(fmax(vmSmooth) <= fmax(vmFlat) + 1e-6,
    `nodal averaging never raises the peak (${fmax(vmSmooth).toFixed(2)} vs ${fmax(vmFlat).toFixed(2)} MPa)`);
  console.log("ok: smoothed stress (nodal recovery + surface sampling)");

  model.set_snap_wall(0); // back to nominal sizing for the remaining sections
}
console.log("ok: as-printed verify (snap + skin/infill solve + SF)");

// Resegmentation.
model.resegment(60);
assert(model.patch_count() === 6, "resegment at 60 deg still 6 patches");

// ---- full optimization pipeline ----
// Roomy beam (60x12x12 mm): enough interior beyond the skin to optimize.
const optModel = new Model(boxStl([0, 0, 0], [60, 12, 12]), "beam2");
const optTri = optModel.triangle_count();
const osel = patchSelector(optModel);
optModel.set_material(2400, 0.35, 1.24, 50, 35);
optModel.set_resolution(60000);
optModel.add_fixed(osel(0, "min"));
optModel.add_force(osel(0, "max"), 0, 0, -40);

let progressCalls = 0;
let lastDensityLen = 0;
let skelTris = 0;
let skelColored = false;
let progressTelemetryOk = false;
const phasesSeen = new Set();
const t1 = performance.now();
const summary = JSON.parse(
  // 35% infill budget (mean interior density); gyroid law E = 1.0*E0*rho^1.5;
  // 2 perimeters x 0.45 mm line width; 8 smoothing passes
  optModel.optimize(JSON.stringify({
    budgetPct: 35, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
    smoothIters: 8, nBins: 3, floorPct: 10, capPct: 70, levelsPct: null,
    binary: false, solidPattern: null, goal: "budget",
  }), (json, density, skelPos, skelIdx, skelDen) => {
    const p = JSON.parse(json);
    // Buffer-less {phase: ...} pushes narrate the silent pipeline stages
    // (verification, baselines, regions) for the UI busy chip.
    if (p.phase) { phasesSeen.add(p.phase); return; }
    progressCalls++;
    lastDensityLen = density.length;
    if (skelIdx && skelIdx.length) skelTris = skelIdx.length / 3;
    if (skelPos && skelDen && skelDen.length * 3 === skelPos.length) skelColored = true;
    // Nerd-log telemetry: every iteration reports the inner solve + infill.
    if (p.meanInfill > 0 && p.meanInfill < 1 && Number.isFinite(p.innerRes) && p.innerIters >= 0)
      progressTelemetryOk = true;
    if (p.iteration % 10 === 0)
      console.log(
        `   opt iter ${p.iteration}/${p.maxIter}, infill ${(p.meanInfill * 100).toFixed(1)}%, CG ${p.innerIters}`
      );
  })
);
console.log(
  `   optimize: ${summary.iterations} iters (converged=${summary.converged}) in ${((performance.now() - t1) / 1000).toFixed(1)} s; ` +
    `mass ${summary.massGrams.toFixed(1)}/${summary.massSolidGrams.toFixed(1)} g, ` +
    `stiffness vs solid ${(summary.stiffnessVsSolid * 100).toFixed(0)}%, ` +
    `vs uniform +${(summary.gainVsUniform * 100).toFixed(1)}%, bins ${summary.bins.map((b) => b.density).join("/")}`
);
assert(progressCalls >= 5, `progress callback fired (${progressCalls}x)`);
// The pipeline narrates every silent stage so the UI never looks hung.
for (const ph of ["assemble", "optimize_pass", "binning", "verify", "uniform", "solid_ref", "regions", "smoothing", "finalize"])
  assert(phasesSeen.has(ph), `phase push "${ph}" narrated`);
assert(lastDensityLen === optTri * 3, "live vertex density (1 scalar per soup vertex)");
assert(typeof summary.converged === "boolean", "summary reports convergence");
assert(progressTelemetryOk, "progress carries meanInfill + inner-solve telemetry");
assert(skelTris > 0, `live skeleton isosurface streamed (${skelTris} tris last)`);
assert(skelColored, "skeleton carries per-vertex density for legend coloring");
assert(summary.bins.length >= 2, "at least 2 density bins");
assert(summary.massFrac > 0.2 && summary.massFrac < 1.0, `sane mass fraction ${summary.massFrac.toFixed(2)}`);
// Infill-budget semantics: the achieved mean infill lands near the request
// (binning shifts it slightly), and the clamped target echoes the input.
assert(Math.abs(summary.targetInfill - 0.35) < 1e-9, `target infill echoes request (${summary.targetInfill})`);
assert(Math.abs(summary.meanInfill - 0.35) < 0.08, `mean infill near budget (${(summary.meanInfill * 100).toFixed(1)}%)`);
assert(summary.stiffnessVsSolid > 0.1 && summary.stiffnessVsSolid <= 1.05, "sane stiffness ratio");
assert(summary.gainVsUniform > -0.02, `binned not worse than uniform (${(summary.gainVsUniform * 100).toFixed(2)}%)`);

assert(optModel.region_count() >= 1, `modifier regions extracted (${optModel.region_count()})`);
const rpos = optModel.region_positions(0);
const ridx = optModel.region_indices(0);
assert(rpos.length > 0 && ridx.length % 3 === 0, "region mesh arrays");

// Live re-smoothing keeps topology, moves vertices.
optModel.resmooth_regions(0);
const rposRaw = optModel.region_positions(0);
assert(rposRaw.length === rpos.length, "resmooth keeps vertex count");
let moved = false;
for (let i = 0; i < rpos.length; i++) {
  if (Math.abs(rpos[i] - rposRaw[i]) > 1e-6) { moved = true; break; }
}
assert(moved, "smoothing actually moved vertices vs raw");
optModel.resmooth_regions(8);

const vd = optModel.vertex_density();
assert(vd.length === optTri * 3, "final vertex density buffer");

// ---- result switcher: stash + activate the kept baselines ----
// The optimizer already stashed the equal-mass uniform + solid baselines (their
// displacement fields used to be discarded); the optimized solution is stashed
// here as the store does. Activating one swaps it in for displacement/stress
// queries; clearing drops them all.
assert(summary.hasBaselines === true, "optimize exposes the uniform + solid baselines");
assert(summary.uniformMaxDisp > summary.maxDisplacement,
  `uniform deflects more than the optimized design at equal mass (${summary.uniformMaxDisp.toFixed(4)} > ${summary.maxDisplacement.toFixed(4)} mm)`);
assert(summary.solidMaxDisp < summary.maxDisplacement,
  `solid is stiffer than the optimized design (${summary.solidMaxDisp.toFixed(4)} < ${summary.maxDisplacement.toFixed(4)} mm)`);
{
  const maxOf = (a) => {
    let m = 0;
    for (let i = 0; i < a.length; i += 3) m = Math.max(m, Math.hypot(a[i], a[i + 1], a[i + 2]));
    return m;
  };
  optModel.stash_result("optimized");
  const dispUniform = optModel.activate_result("uniform");
  assert(dispUniform.length === optTri * 9, "activate_result returns per-soup-vertex displacements");
  assert(Math.abs(maxOf(dispUniform) - summary.uniformMaxDisp) < 0.05 * summary.uniformMaxDisp + 1e-6,
    `activated uniform max |u| matches the summary (${maxOf(dispUniform).toFixed(4)} mm)`);
  const vmUniform = optModel.result_field("vm");
  assert(vmUniform.length === optTri * 3 && vmUniform.every((v) => Number.isFinite(v)),
    "stress field resolves on the activated uniform result");
  const dispSolid = optModel.activate_result("solid");
  assert(Math.abs(maxOf(dispSolid) - summary.solidMaxDisp) < 0.05 * summary.solidMaxDisp + 1e-6,
    `activated solid max |u| matches the summary (${maxOf(dispSolid).toFixed(4)} mm)`);
  const dispOpt = optModel.activate_result("optimized");
  assert(Math.abs(maxOf(dispOpt) - summary.maxDisplacement) < 0.05 * summary.maxDisplacement + 1e-6,
    `re-activating the optimized result restores its field (${maxOf(dispOpt).toFixed(4)} mm)`);
  optModel.clear_results();
  let cleared = false;
  try {
    optModel.activate_result("uniform");
  } catch (e) {
    cleared = /no such result/i.test(String(e));
  }
  assert(cleared, "clear_results drops the stash (activate then fails)");
}
console.log("ok: result switcher (stash / activate / clear)");

// Analysis-mesh display buffers.
const hull = optModel.voxel_hull();
const hedges = optModel.voxel_edges();
assert(hull.length > 0 && hull.length % 9 === 0, `voxel hull triangle soup (${hull.length / 9} tris)`);
assert(hedges.length > 0 && hedges.length % 6 === 0, `voxel edge segments (${hedges.length / 6})`);

// Density-threshold cutaway isosurface (continuous field + colors).
const iso = optModel.density_isosurface(0.3);
assert(iso.length === 3 && iso[0].length > 0 && iso[1].length % 3 === 0,
  `density isosurface at 30% (${iso[1].length / 3} tris)`);
assert(iso[2].length * 3 === iso[0].length, "cutaway carries per-vertex density");

const threeMf = optModel.export_3mf("orca", new Uint8Array());
assert(threeMf.length > 500 && threeMf[0] === 0x50 && threeMf[1] === 0x4b, "3MF export is a zip");
// The part carries the perimeter count from the optimize call (2 above);
// modifiers override ONLY the infill density — walls inherit from the part.
{
  const td = new TextDecoder("latin1");
  const raw = td.decode(threeMf);
  const hits = raw.split("wall_loops").length - 1;
  assert(hits === 1, "wall_loops written exactly once (object level)");
  assert(raw.includes('wall_loops" value="2"'), "wall_loops = perimeters set in optimize");
  assert(raw.indexOf("wall_loops") < raw.indexOf("<part "), "wall_loops at object level, not in a part");
  assert(raw.includes("sparse_infill_density"), "densities present");
}
// PrusaSlicer flavor: one object, volumes by triangle range in
// Slic3r_PE_model.config, perimeters + base fill_density at object scope.
{
  const prusaMf = optModel.export_3mf("prusa", new Uint8Array());
  const raw = new TextDecoder("latin1").decode(prusaMf);
  assert(raw.includes("Slic3r_PE_model.config"), "prusa export carries the PE model config");
  assert(raw.includes("slic3rpe:Version3mf"), "prusa flavor marker present");
  assert(raw.includes("ParameterModifier"), "modifier volumes declared");
  assert(raw.includes('key="perimeters" value="2"'), "perimeters at object scope");
  assert(raw.includes('key="fill_density"'), "fill densities present");
  assert(!raw.includes("wall_loops"), "no bambu keys in the prusa flavor");
}
const stlZip = optModel.export_stls();
assert(stlZip.length > 100 && stlZip[0] === 0x50, "STL zip export");
// Re-import our own 3MF (full circle; re-import subdivides for display again).
const reimported = new Model(threeMf, "roundtrip");
assert(reimported.triangle_count() >= 12, "exported 3MF re-imports (part wins by bbox)");

// ---- binary (hollow/solid) mode ----
// Coarse grid: the point is the pipeline, not the physics here — and it's fast.
// NOTE: keep this a COARSE grid (~h=1 mm on this 60×12×12 beam). Binary gain vs
// uniform is strongly positive at h≈1.0 (+31%) and h≈0.5 (+33%) but dips
// NEGATIVE at h≈0.7 (a narrow, resolution-specific optimizer artifact — likely
// wall/skin thickness rounding to a bad cell count). res≈8k lands near h=1.0;
// res≈25k lands on the h≈0.7 pothole and made `gainVsUniform > 0` fail.
const binModel = new Model(boxStl([0, 0, 0], [60, 12, 12]), "beam3");
const bsel = patchSelector(binModel);
binModel.set_material(2400, 0.35, 1.24, 50, 35);
binModel.set_resolution(8000);
binModel.add_fixed(bsel(0, "min"));
binModel.add_force(bsel(0, "max"), 0, 0, -40);
const t2 = performance.now();
const binSummary = JSON.parse(
  binModel.optimize(JSON.stringify({
    budgetPct: 30, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
    smoothIters: 4, nBins: 2, floorPct: 5, capPct: 100, levelsPct: [5, 100],
    binary: true, solidPattern: "concentric", goal: "budget",
  }), () => {})
);
console.log(
  `   binary: ${binSummary.iterations} iters in ${((performance.now() - t2) / 1000).toFixed(1)} s; ` +
    `bins ${binSummary.bins.map((b) => b.density).join("/")}, ` +
    `mean ${(binSummary.meanInfill * 100).toFixed(1)}%, vs uniform +${(binSummary.gainVsUniform * 100).toFixed(1)}%`
);
assert(binSummary.binary === true, "summary flags binary mode");
assert(binSummary.bins.length === 2, "binary = exactly two levels");
assert(Math.abs(binSummary.bins[0].density - 0.05) < 1e-9, "bottom level = 5% printability floor");
assert(Math.abs(binSummary.bins[1].density - 1.0) < 1e-9, "top level = solid");
assert(Math.abs(binSummary.meanInfill - 0.3) < 0.05, `binary mean tracks budget (${binSummary.meanInfill})`);
assert(binSummary.gainVsUniform > 0.0, "binary core beats uniform infill");
{
  const binMf = binModel.export_3mf("orca", new Uint8Array());
  const raw = new TextDecoder("latin1").decode(binMf);
  assert(raw.includes('sparse_infill_pattern" value="concentric"'),
    "binary export carries the solid-fill pattern on the modifier");
  assert(!raw.includes("internal_solid_infill_pattern"),
    "deprecated object-level key never written (Bambu renamed rectilinear -> zig-zag)");
  // DESIGN.md #10 (2026-06): the pattern is pinned at BOTH the object level
  // (the GENERAL sparse_infill_pattern, so the whole part prints in it) AND on
  // the solid-region modifier — so the key appears at least twice.
  assert((raw.split("sparse_infill_pattern").length - 1) >= 2,
    "pattern pinned at object level and on the modifier (whole part prints in it)");
  assert(raw.includes('sparse_infill_density" value="100%"'), "solid region modifier at 100%");
  assert(raw.includes('sparse_infill_density" value="5%"'), "base density 5%");
}
console.log("ok: binary mode pipeline (optimize + export)");

// ---- Part Topo (solid topology): validation results on the optimized shape ----
// The masked voxel-result hull covers only the RETAINED cells (anchors +
// connected-keep above iso_threshold), so stress/displacement display on the
// carved body instead of the original envelope.
{
  const topo = new Model(boxStl([0, 0, 0], [60, 12, 12]), "topobeam");
  const tsel = patchSelector(topo);
  topo.set_material(2400, 0.35, 1.24, 50, 35);
  topo.set_resolution(8000);
  topo.add_fixed(tsel(0, "min"));
  topo.add_force(tsel(0, "max"), 0, 0, -40);
  const ts = JSON.parse(topo.optimize(JSON.stringify({
    budgetPct: 40, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
    smoothIters: 4, nBins: 2, floorPct: 5, capPct: 100, levelsPct: null,
    binary: false, solid: true, solidPattern: null, goal: "budget",
  }), () => {}));
  assert(ts.solid === true, "summary flags solid topology mode");
  const full = topo.voxel_results(false);
  const masked = topo.voxel_results(true);
  assert(masked[0].length > 0 && masked[0].length % 9 === 0,
    `masked result hull (${masked[0].length / 9} tris vs full ${full[0].length / 9})`);
  assert(masked[0].length !== full[0].length,
    "masked hull differs from the full envelope hull (carved faces exposed)");
  assert(masked[1].length === masked[0].length, "masked hull displacement per vertex");
  assert(masked[2].length > 0 && masked[3].length === masked[2].length,
    "masked edge displacements match edges");
  // The load anchor is frozen solid, so the deflection peak survives the mask.
  let mmax = 0;
  for (let i = 0; i < masked[1].length; i += 3)
    mmax = Math.max(mmax, Math.hypot(masked[1][i], masked[1][i + 1], masked[1][i + 2]));
  assert(Math.abs(mmax - ts.maxDisplacement) < 0.05 * ts.maxDisplacement + 1e-6,
    `masked nodal max |u| matches the verification solve (${mmax.toFixed(4)} mm)`);
  // Field values ride the SAME mask: one value per masked-hull vertex, flat
  // per cell (all 3 vertices of a triangle share the owning cell's value).
  const mvm = topo.voxel_result_field("vm", true);
  assert(mvm.length === masked[0].length / 3, "masked field value per masked hull vertex");
  assert(mvm.every((v) => Number.isFinite(v)) && fmax(mvm) > 0, "masked von Mises sane");
  for (let t = 0; t < 30; t++)
    assert(mvm[3 * t] === mvm[3 * t + 1] && mvm[3 * t] === mvm[3 * t + 2],
      "per-cell flat values on the masked hull");
  // Moving the iso slider moves the mask: a much higher threshold keeps fewer
  // cells, and the hull follows the stored level.
  topo.set_iso_threshold(0.9, 4);
  const tight = topo.voxel_results(true);
  assert(tight[0].length !== masked[0].length,
    `hull follows iso_threshold (${tight[0].length / 9} tris at 0.9)`);
}
console.log("ok: Part Topo masked result hull (fields on the optimized shape)");

// ---- stiffness-match goal ----
// Lightest design as stiff as a uniform 35% print: the secant on the budget
// must land the BINNED compliance within tolerance of the uniform reference,
// at less mass than that reference.
const matchModel = new Model(boxStl([0, 0, 0], [60, 12, 12]), "beam4");
const msel = patchSelector(matchModel);
matchModel.set_material(2400, 0.35, 1.24, 50, 35);
matchModel.set_resolution(25000);
matchModel.add_fixed(msel(0, "min"));
matchModel.add_force(msel(0, "max"), 0, 0, -40);
const t3 = performance.now();
let maxPassSeen = 0;
const matchSummary = JSON.parse(
  matchModel.optimize(JSON.stringify({
    budgetPct: 35, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
    smoothIters: 4, nBins: 3, floorPct: 10, capPct: 70, levelsPct: null,
    binary: false, solidPattern: null, goal: "match",
  }), (json) => {
    const p = JSON.parse(json);
    if (p.phase) return; // phase narration — pass counting reads iteration pushes
    maxPassSeen = Math.max(maxPassSeen, p.pass);
  })
);
console.log(
  `   match: ${matchSummary.passes} passes (${matchSummary.iterations} iters total) in ${((performance.now() - t3) / 1000).toFixed(1)} s; ` +
    `target C ${matchSummary.targetCompliance.toExponential(3)} achieved ${matchSummary.achievedCompliance.toExponential(3)} ` +
    `(dev ${(matchSummary.matchDeviation * 100).toFixed(1)}%); mass ${matchSummary.massGrams.toFixed(1)} g vs uniform ${matchSummary.massUniformRefGrams.toFixed(1)} g`
);
assert(matchSummary.goal === "match", "summary flags match goal");
assert(matchSummary.passes >= 2 && maxPassSeen >= 2, "secant ran multiple warm passes");
assert(Math.abs(matchSummary.matchDeviation) <= 0.05,
  `binned stiffness within tolerance of the uniform reference (${(matchSummary.matchDeviation * 100).toFixed(1)}%)`);
assert(matchSummary.massGrams < matchSummary.massUniformRefGrams,
  "matched design is lighter than the uniform reference");
console.log("ok: stiffness-match goal (lighter at equal stiffness)");

// ---- strength goal (DESIGN §17) ----
// Min material s.t. SF_crit ≥ target. Two runs on one model: an unreachable
// target must take the honest infeasible path (all-at-cap delivery + best
// achievable + binding diagnosis, NO optimization loop), a reachable one must
// deliver AT/ABOVE the target with less material than the cap design.
const sfModel = new Model(boxStl([0, 0, 0], [60, 12, 12]), "beam5");
const ssel = patchSelector(sfModel);
sfModel.set_material(2400, 0.35, 1.24, 50, 35);
sfModel.set_resolution(25000);
sfModel.add_fixed(ssel(0, "min"));
sfModel.add_force(ssel(0, "max"), 0, 0, -40);
const sfOpts = {
  budgetPct: 35, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
  smoothIters: 4, nBins: 3, floorPct: 10, capPct: 70, levelsPct: null,
  binary: false, solidPattern: null, goal: "strength", sfMeasure: "both",
};
const t4 = performance.now();
const sfInf = JSON.parse(
  sfModel.optimize(JSON.stringify({ ...sfOpts, sfTarget: 9.5 }), () => {})
);
console.log(
  `   strength (infeasible): best achievable SF ${sfInf.sfBest.toFixed(2)} at cap, ` +
    `${sfInf.bindingCellCount} binding cells (skin share ${(sfInf.bindingSkinShare * 100).toFixed(0)}%)`
);
assert(sfInf.goal === "strength", "summary flags strength goal");
assert(sfInf.sfFeasible === false, "SF 9.5 on this beam is (honestly) infeasible");
assert(sfInf.sfBest > 0 && sfInf.sfBest < 9.5, `best-achievable ceiling reported (${sfInf.sfBest})`);
assert(Math.abs(sfInf.meanInfill - 0.7) < 0.02,
  `infeasible path delivers the all-at-cap design (mean ${(sfInf.meanInfill * 100).toFixed(1)}%)`);
assert(sfInf.iterations === 0, "infeasible path skips the optimization loop");
assert(sfInf.bindingCellCount > 0, "binding region identified for the diagnosis");
const sfTargetVal = Math.max(1.2, Math.min(3, sfInf.sfBest * 0.55));
const sfOk = JSON.parse(
  sfModel.optimize(JSON.stringify({ ...sfOpts, sfTarget: sfTargetVal }), () => {})
);
console.log(
  `   strength (feasible): target ${sfTargetVal.toFixed(2)} → SF ${sfOk.sfAchieved.toFixed(2)} at ` +
    `mean ${(sfOk.meanInfill * 100).toFixed(1)}% in ${sfOk.passes} pass(es), ` +
    `${((performance.now() - t4) / 1000).toFixed(1)} s total`
);
assert(sfOk.sfFeasible === true, "reachable target is feasible");
assert(sfOk.sfAchieved >= sfTargetVal - 1e-9,
  `delivered design meets the target (SF ${sfOk.sfAchieved.toFixed(2)} ≥ ${sfTargetVal.toFixed(2)})`);
assert(sfOk.meanInfill < 0.7 - 0.02, "feasible delivery saves material vs the cap design");
assert(Array.isArray(sfOk.sfTrace) && sfOk.sfTrace.length >= 2,
  "pre-flight + passes traced (budget vs SF)");
assert(sfOk.sfMeasure === "both", "SF measure echoed in the summary");
console.log("ok: strength goal (SF target met at minimum material; honest infeasibility)");

// ---- project (.filasim) save / load round-trip ----
// Orient → optimize → export project → re-import the original file + replay the
// transform + restore: the design (regions, density, stress eps) and the result
// displacements must come back identical, with no re-optimization.
{
  const maxAbsDiff = (a, b) => {
    let d = 0;
    for (let i = 0; i < a.length; i++) d = Math.max(d, Math.abs(a[i] - b[i]));
    return d;
  };
  const maxAbs = (a) => {
    let m = 0;
    for (let i = 0; i < a.length; i++) m = Math.max(m, Math.abs(a[i]));
    return m;
  };
  const pStl = boxStl([0, 0, 0], [50, 10, 10]);
  const pModel = new Model(pStl, "projbeam");
  // Orient first (90° about Y) — exercises the saved-transform replay. Then set
  // up + optimize on the oriented mesh, as the real workflow does.
  pModel.transform(new Float64Array([0, 0, 1, 0, 1, 0, -1, 0, 0, 0, 0, 0]));
  const psel = patchSelector(pModel);
  pModel.set_material(2400, 0.35, 1.24, 50, 35);
  pModel.set_resolution(20000);
  pModel.add_fixed(psel(2, "min")); // after the rotation, the long axis is Z
  pModel.add_force(psel(2, "max"), 0, 0, -30);
  JSON.parse(
    pModel.optimize(
      JSON.stringify({
        budgetPct: 30, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
        smoothIters: 4, nBins: 3, floorPct: 10, capPct: 70, levelsPct: null,
        binary: false, solidPattern: null, goal: "budget",
      }),
      () => {}
    )
  );
  pModel.stash_result("optimized");
  const regBefore = pModel.region_count();
  const vdBefore = pModel.vertex_density();
  const dispBefore = pModel.vertex_displacements();
  const vmBefore = pModel.result_field("vm");
  const accum = Array.from(pModel.transform_matrix());

  const manifest = JSON.stringify({ app: "filaSim", fileName: "projbeam.stl", transform: accum });

  // Save WITH results, then read the pieces back out.
  const proj = pModel.export_project(pStl, "model.stl", manifest, true);
  assert(proj.length > 200 && proj[0] === 0x50 && proj[1] === 0x4b, "project export is a zip");
  assert(project_manifest(proj) === manifest, "manifest round-trips out of the project");
  const embedded = project_model(proj);
  assert(embedded.length === pStl.length, "original model bytes embedded verbatim");

  // Re-open: fresh model from the embedded file, replay settings + orientation,
  // restore the design + results.
  const q = new Model(project_model(proj), "reopened");
  q.set_material(2400, 0.35, 1.24, 50, 35);
  q.set_resolution(20000);
  q.transform(new Float64Array(accum));
  const summary = JSON.parse(q.restore_project(proj));
  assert(summary.hasDesign === true, "restore reports the design");
  assert(summary.restoredResults.includes("optimized"), "optimized result restored");
  assert(q.region_count() === regBefore, `regions match (${q.region_count()} vs ${regBefore})`);
  assert(maxAbsDiff(q.vertex_density(), vdBefore) < 1e-6, "vertex density matches the saved design");
  const dispAfter = q.activate_result("optimized");
  assert(maxAbsDiff(dispAfter, dispBefore) < 1e-4 * (1 + maxAbs(dispBefore)),
    "restored optimized displacement matches");
  assert(maxAbsDiff(q.result_field("vm"), vmBefore) < 1e-2 * (1 + maxAbs(vmBefore)),
    "restored stress field matches (eps re-derived)");
  // Re-export the 3MF from the restored design — the tool's main output survives.
  const reMf = q.export_3mf("orca", new Uint8Array());
  assert(reMf.length > 500 && reMf[0] === 0x50, "restored design re-exports a 3MF");

  // Save WITHOUT results: design restores, but no result stash.
  const projNo = pModel.export_project(pStl, "model.stl", manifest, false);
  const q2 = new Model(project_model(projNo), "reopened2");
  q2.set_material(2400, 0.35, 1.24, 50, 35);
  q2.set_resolution(20000);
  q2.transform(new Float64Array(accum));
  const summary2 = JSON.parse(q2.restore_project(projNo));
  assert(summary2.hasDesign === true, "design-only project restores the design");
  assert(summary2.restoredResults.length === 0, "design-only project carries no FEA results");
  assert(q2.region_count() === regBefore, "design-only regions match");
}
console.log("ok: project save / load round-trip (with + without results)");

// ---- per-step optimized evaluation (DESIGN §13): solve_optimized ----
// The Results roster after a multi-load optimize evaluates the ONE optimized
// design under every load step. solve_optimized must keep that design across BC
// changes (its stiffness field outlives the live solution) and re-solve under
// each step's loads — the deflection follows the load, and the result differs.
{
  const so = new Model(boxStl([0, 0, 0], [60, 12, 12]), "stepbeam");
  const ssel = patchSelector(so);
  so.set_material(2400, 0.35, 1.24, 50, 35);
  so.set_resolution(30000);
  so.add_fixed(ssel(0, "min"));
  so.add_force(ssel(0, "max"), 0, 0, -40); // load A: −Z tip load
  let errored = false;
  try { so.solve_optimized(); } catch (e) { errored = /no optimized design/i.test(String(e)); }
  assert(errored, "solve_optimized errors before any optimization");

  const sum = JSON.parse(so.optimize(JSON.stringify({
    budgetPct: 35, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
    smoothIters: 4, nBins: 3, floorPct: 10, capPct: 70, levelsPct: null,
    binary: false, solidPattern: null, goal: "budget",
  }), () => {}));
  const tipMean = (disp) => {
    const pos = so.positions();
    let uy = 0, uz = 0, n = 0;
    for (let v = 0; v < pos.length / 3; v++)
      if (Math.abs(pos[3 * v] - 60) < 1e-3) { uy += disp[3 * v + 1]; uz += disp[3 * v + 2]; n++; }
    return { uy: uy / n, uz: uz / n };
  };
  const maxU = (a) => { let m = 0; for (let i = 0; i < a.length; i += 3) m = Math.max(m, Math.hypot(a[i], a[i + 1], a[i + 2])); return m; };

  // Load A = the optimized load: reproduces the run's primary result AND is
  // softer than solid — proof it evaluates the OPTIMIZED design, not a re-solve.
  const sA = JSON.parse(so.solve_optimized());
  assert(Math.abs(sA.maxDisplacement - sum.maxDisplacement) < 0.05 * sum.maxDisplacement + 1e-6,
    `solve_optimized under the optimized load matches the run (${sA.maxDisplacement.toFixed(4)} mm)`);
  assert(sA.maxDisplacement > sum.solidMaxDisp * 1.05, "optimized design is softer than solid");
  const tA = tipMean(so.vertex_displacements());
  assert(Math.abs(tA.uz) > 3 * Math.abs(tA.uy), "under −Z the tip deflects mainly in Z");
  so.stash_result("optimized::A");

  // Load B = a DIFFERENT step (−Y). Changing BCs normally drops the design; the
  // kept stiffness field lets solve_optimized re-evaluate it. Result must follow
  // the new load direction and genuinely differ from A.
  so.clear_bcs();
  so.add_fixed(ssel(0, "min"));
  so.add_force(ssel(0, "max"), 0, -40, 0);
  const sB = JSON.parse(so.solve_optimized());
  const tB = tipMean(so.vertex_displacements());
  assert(Math.abs(tB.uy) > 3 * Math.abs(tB.uz), "under −Y the tip deflects mainly in Y");
  assert(Math.abs(sB.maxDisplacement - sA.maxDisplacement) > 0.02 * sA.maxDisplacement,
    `the two load steps give different results (A ${sA.maxDisplacement.toFixed(4)} vs B ${sB.maxDisplacement.toFixed(4)} mm)`);
  so.stash_result("optimized::B");

  // Per-step stashes (`optimized::stepId`) round-trip through activate_result.
  assert(Math.abs(maxU(so.activate_result("optimized::A")) - sA.maxDisplacement) < 0.05 * sA.maxDisplacement + 1e-6,
    "stash/activate round-trips for optimized::A");
  assert(Math.abs(maxU(so.activate_result("optimized::B")) - sB.maxDisplacement) < 0.05 * sB.maxDisplacement + 1e-6,
    "stash/activate round-trips for optimized::B");

  // Orientation sweep across load steps (DESIGN §15 dec. 5): folding both
  // stashes must equal the per-pixel elementwise MIN of the individual
  // sweeps — a min over the union of cells is a min of the mins.
  {
    const sweepAll = (ids) => {
      const meta = JSON.parse(so.orientation_sweep_begin(ids, 30));
      const out = new Float32Array(meta.pixels);
      for (let s = 0; s < meta.pixels; s += 25) out.set(so.orientation_sweep_rows(s, 25)[1], s);
      so.orientation_sweep_end();
      return out;
    };
    const a = sweepAll(["optimized::A"]);
    const b = sweepAll(["optimized::B"]);
    const ab = sweepAll(["optimized::A", "optimized::B"]);
    assert(ab.every((v, i) => Math.abs(v - Math.min(a[i], b[i])) < 1e-4),
      "multi-step sweep = elementwise min of per-step sweeps");
    let unknown = false;
    try { so.orientation_sweep_begin(["nope"], 30); } catch { unknown = true; }
    assert(unknown, "sweep rejects an unknown stash id");
  }
  console.log("ok: orientation sweep folds load steps worst-case");

  // A grid change retires the kept design → clean error (no stale evaluation).
  so.set_resolution(80000);
  let stale = false;
  try { so.solve_optimized(); } catch (e) { stale = /no optimized design|predates the current grid/i.test(String(e)); }
  assert(stale, "solve_optimized refuses a design that predates a grid change");
}
console.log("ok: per-step optimized evaluation (solve_optimized across load steps)");

// ---- acceleration loads + remote point masses (DESIGN §16) ----
{
  const accBeam = boxStl([0, 0, 0], [40, 6, 6]);
  const tipMeanUz = (model) => {
    const pos = model.positions();
    const d = model.vertex_displacements();
    let uz = 0, n = 0;
    for (let v = 0; v < pos.length / 3; v++)
      if (Math.abs(pos[3 * v] - 40) < 1e-3) { uz += d[3 * v + 2]; n++; }
    return n ? uz / n : 0;
  };
  const massT = 1e-4; // 100 g in tonne
  const g0 = 9810; // 1 g in mm/s²
  const F = massT * g0; // 0.981 N
  const r = 80; // CG offset in +x, beyond the 40 mm tip

  // (a) LEVER ARM: a tip mass whose CG is offset r beyond the tip ≡ the
  //     hand-composed tip force F + transported moment r·F. Material density 0
  //     isolates the mass (no self-weight), so the two solves must match.
  const gm = new Model(accBeam, "massbeam");
  const gsel = patchSelector(gm);
  gm.set_material(2000, 0.3, 0, 50, 35);
  gm.set_resolution(50000);
  gm.add_fixed(gsel(0, "min"));
  gm.add_mass(gsel(0, "max"), 40 + r, 3, 3, massT);
  gm.set_accel(0, 0, -g0);
  assert(JSON.parse(gm.check()).ok, "remote-mass cantilever passes the check");
  JSON.parse(gm.solve());
  const uzMass = tipMeanUz(gm);
  gm.clear_bcs();
  gm.set_accel(0, 0, 0); // isolate the explicit loads (no self-weight)
  gm.add_fixed(gsel(0, "min"));
  gm.add_force(gsel(0, "max"), 0, 0, -F);
  gm.add_moment(gsel(0, "max"), 0, r * F, 0);
  JSON.parse(gm.solve());
  const uzFM = tipMeanUz(gm);
  assert(Math.abs(uzFM) > 0.02, `force + moment gives a measurable tip deflection (${uzFM.toFixed(4)} mm)`);
  assert(Math.abs(uzMass - uzFM) < 0.03 * Math.abs(uzFM) + 1e-6,
    `remote mass ≡ tip force + transported couple (${uzMass.toFixed(4)} vs ${uzFM.toFixed(4)} mm)`);

  // Lever-arm SANITY: the offset mass bends the tip far more than the same
  // weight smeared over the face (CG at the patch centre ≈ a pure force).
  gm.clear_bcs();
  gm.set_accel(0, 0, -g0);
  gm.add_fixed(gsel(0, "min"));
  gm.add_mass(gsel(0, "max"), 40, 3, 3, massT);
  JSON.parse(gm.solve());
  const uzCentered = tipMeanUz(gm);
  assert(Math.abs(uzMass) > 1.8 * Math.abs(uzCentered),
    `offset CG bends more than a centred mass (${uzMass.toFixed(4)} vs ${uzCentered.toFixed(4)} mm)`);
  gm.free();
  console.log("ok: remote point mass (lever-arm couple transport)");

  // (b) SELF-WEIGHT: 1 g on the bare beam (real density) sags it down; no accel
  //     leaves it at rest. Confirms set_accel drives the per-cell body force.
  const gb = new Model(accBeam, "gravitybeam");
  const bsel2 = patchSelector(gb);
  gb.set_material(2000, 0.3, 1.24, 50, 35);
  gb.set_resolution(50000);
  gb.add_fixed(bsel2(0, "min"));
  gb.set_accel(0, 0, 0);
  const rest = JSON.parse(gb.solve());
  assert(rest.maxDisplacement < 1e-6, "no acceleration + no load → beam at rest");
  gb.set_accel(0, 0, -g0);
  const grav = JSON.parse(gb.solve());
  assert(grav.maxDisplacement > 1e-5,
    `self-weight under 1 g deflects the beam (${grav.maxDisplacement.toExponential(2)} mm)`);
  assert(tipMeanUz(gb) < 0, "the beam sags downward under gravity");
  gb.free();
  console.log("ok: acceleration self-weight (per-cell body force)");

  // (c) SCHEMA ROUND-TRIP: accel + mass entities survive the .filasim manifest
  //     (persistence is strictly additive — the container echoes it verbatim).
  {
    const pm = new Model(accBeam, "schemabeam");
    const manifest = JSON.stringify({
      app: "filaSim", fileName: "beam.stl", transform: [1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],
      bcs: [
        { kind: "accel", tris: [], accel: [0, 0, -9810], accelMode: "direction", accelDir: [0, 0, -1], accelMag: 9810 },
        { kind: "mass", tris: [1, 2, 3], massGrams: 100, point: [40, 3, 3], behavior: "deformable" },
      ],
    });
    const proj = pm.export_project(accBeam, "model.stl", manifest, false);
    assert(project_manifest(proj) === manifest, "accel/mass manifest round-trips through the project zip");
    const back = JSON.parse(project_manifest(proj));
    assert(back.bcs[0].kind === "accel" && back.bcs[0].accel[2] === -9810,
      "accel entity (vector + mode) survives save/load");
    assert(back.bcs[1].kind === "mass" && back.bcs[1].massGrams === 100 &&
      back.bcs[1].behavior === "deformable" && back.bcs[1].point[0] === 40,
      "mass entity (grams + CG + behavior) survives save/load");
    pm.free();
  }
  console.log("ok: accel/mass schema round-trip (additive persistence)");

  // (d) MODAL INERTIA: a remote point mass adds its inertia to the modal mass
  //     matrix (DESIGN §16), so a heavy tip payload drags the natural
  //     frequencies DOWN. Without the wiring, f1 would be unchanged — this
  //     guards the whole store→worker→wasm→core modal-mass path end to end.
  {
    const md = new Model(accBeam, "modalmass");
    const dsel = patchSelector(md);
    md.set_material(2000, 0.3, 1.24, 50, 35);
    md.set_resolution(20000);
    md.add_fixed(dsel(0, "min"));
    const modalOpts = JSON.stringify({ numModes: 1, solid: true, free: false });
    const bare = JSON.parse(md.modal_analysis(modalOpts, () => {}));
    const f1Bare = bare.modes[0].freqHz;
    assert(f1Bare > 0 && Number.isFinite(f1Bare),
      `bare cantilever has a positive f1 (${f1Bare.toFixed(1)} Hz)`);
    // Bolt a 20 g mass (≈10× the 1.8 g beam) to the free tip. The CG offset is
    // irrelevant to the modal mass — a translational-DOF mesh can't hold the
    // offset's rotatory inertia — so any point reproduces the same M.
    md.add_mass(dsel(0, "max"), 40, 3, 3, 2e-5); // 20 g in tonne
    const loaded = JSON.parse(md.modal_analysis(modalOpts, () => {}));
    const f1Load = loaded.modes[0].freqHz;
    assert(f1Load > 0 && Number.isFinite(f1Load),
      `loaded cantilever still solves (${f1Load.toFixed(1)} Hz)`);
    assert(f1Load < 0.7 * f1Bare,
      `tip mass drags f1 down (${f1Bare.toFixed(1)} → ${f1Load.toFixed(1)} Hz, ` +
      `×${(f1Load / f1Bare).toFixed(3)})`);
    md.free();
    console.log("ok: modal analysis accounts for remote point-mass inertia");
  }

  // (e) SELF-WEIGHT-DRIVEN OPTIMIZATION (DESIGN §16 dec. 4/10): the optimizer now
  //     includes acceleration steps — a part optimized under its OWN weight (1 g,
  //     no external load) must converge, flag the run as self-weight, and expose
  //     each design's own-weight baselines (dec. 10). Exercises the full
  //     store→worker→wasm→core self-weight optimizer path.
  {
    const sw = new Model(boxStl([0, 0, 0], [60, 12, 12]), "selfweightbeam");
    const swsel = patchSelector(sw);
    sw.set_material(2400, 0.35, 1.24, 50, 35); // real PLA density → real self-weight
    sw.set_resolution(20000);
    sw.add_fixed(swsel(0, "min"));
    sw.set_accel(0, 0, -9810); // 1 g down — the ONLY load is the part's own weight
    assert(JSON.parse(sw.check()).ok, "self-weight cantilever passes the check (accel loads it)");
    const swSummary = JSON.parse(
      sw.optimize(
        JSON.stringify({
          budgetPct: 35, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
          smoothIters: 4, nBins: 3, floorPct: 10, capPct: 70, levelsPct: null,
          binary: false, solidPattern: null, goal: "budget",
        }),
        () => {}
      )
    );
    assert(swSummary.selfWeight === true, "summary flags a self-weight optimization (dec. 10 note)");
    assert(typeof swSummary.converged === "boolean" && swSummary.massGrams > 0,
      "self-weight optimize produced a design");
    assert(swSummary.meanInfill > 0.2 && swSummary.meanInfill < 0.5,
      `self-weight optimize respects the budget (${(swSummary.meanInfill * 100).toFixed(1)}%)`);
    // dec. 10: each baseline carries its OWN weight — the uniform + fully-solid
    // reference solves both run under their true self-weight and deflect.
    assert(swSummary.uniformMaxDisp > 0 && swSummary.solidMaxDisp > 0,
      "own-weight baselines solve (each design carries its true self-weight)");
    sw.free();
    console.log("ok: self-weight-driven optimization (DESIGN §16 dec. 4/10)");
  }

  // (f) RIGID remote-mass mount (DESIGN §16 milestone 4): a rigid boss stiffens
  //     its mounting patch by tying it to a 6-DOF master at the CG — a NEW solver
  //     term (penalty rank-6 coupling). The headline risk is convergence: the
  //     MGCG must still converge with the stiff penalty. Both rigid and
  //     deformable mounts must solve; the rigid stress field must be finite/sane.
  {
    const rm = new Model(accBeam, "rigidbeam");
    const rsel = patchSelector(rm);
    rm.set_material(2000, 0.3, 0, 50, 35); // density 0 → isolate the mass load
    rm.set_resolution(50000);
    const face = rsel(0, "max"); // +x tip face
    const solveMount = (rigid) => {
      rm.clear_bcs();
      rm.add_fixed(rsel(0, "min"));
      rm.add_mass(face, 80, 3, 3, 1e-4, rigid); // 100 g, CG offset 40 mm beyond tip
      rm.set_accel(0, 0, -9810); // 1 g down
      assert(JSON.parse(rm.check()).ok, `${rigid ? "rigid" : "deformable"} mount passes the check`);
      return JSON.parse(rm.solve());
    };
    const rstats = solveMount(true);
    assert(
      rstats.converged && rstats.maxDisplacement > 0 && Number.isFinite(rstats.maxDisplacement),
      `rigid mount solve CONVERGES with the penalty term (${rstats.iterations} iters, ` +
        `max|u| ${rstats.maxDisplacement.toFixed(4)} mm)`
    );
    const rvm = rm.result_field("vm");
    assert(rvm.every((v) => Number.isFinite(v)) && fmax(rvm) > 0, "rigid mount stress field is finite + nonzero");
    const dstats = solveMount(false);
    assert(dstats.converged && dstats.maxDisplacement > 0, "deformable mount also solves");
    // Far-field tip deflection is Saint-Venant close (the rigid stiffening is a
    // LOCAL patch effect); both must deflect downward under the offset weight.
    assert(rstats.maxDisplacement > 0 && dstats.maxDisplacement > 0,
      "both mounts deflect under the offset weight");
    // Toggling the mount rebuilds the solver hierarchy (rigid changes the
    // operator, not just the RHS), so the two solves are genuinely different runs.
    rm.free();
    console.log("ok: rigid remote-mass mount solves — penalty converges (DESIGN §16 M4)");
  }
}

// ---- pre-tessellated import (DESIGN §18): Model.from_mesh ----
// STEP now tessellates in the JS meshStep worker; the engine receives an
// INDEXED mesh + DENSE per-triangle CAD-face/solid ids via from_mesh. It must
// land exactly where the bytes path lands (CAD-face segmentation, refinement,
// solvable), and a .step project must round-trip the way the open path does:
// original bytes embedded verbatim, model rebuilt via from_mesh + transform
// replay, design restored onto it.
{
  // Indexed 40×6×6 box: 8 vertices, 12 triangles, one CAD "face" per side,
  // outward winding (the winding-number voxelizer needs it).
  const lo = [0, 0, 0], hi = [40, 6, 6];
  const positions = new Float32Array(24);
  for (let c = 0; c < 8; c++) {
    positions[3 * c] = c & 1 ? hi[0] : lo[0];
    positions[3 * c + 1] = c & 2 ? hi[1] : lo[1];
    positions[3 * c + 2] = c & 4 ? hi[2] : lo[2];
  }
  const V = (x, y, z) => x + 2 * y + 4 * z;
  const sides = [
    [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]], // -x
    [[1, 0, 0], [1, 1, 0], [1, 1, 1], [1, 0, 1]], // +x
    [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]], // -y
    [[0, 1, 0], [0, 1, 1], [1, 1, 1], [1, 1, 0]], // +y
    [[0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 0, 0]], // -z
    [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]], // +z
  ];
  const idx = [];
  const fot = [];
  sides.forEach((s, f) => {
    const c = s.map(([x, y, z]) => V(x, y, z));
    idx.push(c[0], c[1], c[2], c[0], c[2], c[3]);
    fot.push(f, f);
  });
  const indices = Uint32Array.from(idx);
  const faceOfTri = Uint32Array.from(fot);
  const solidOfTri = new Uint32Array(faceOfTri.length); // one solid, dense id 0

  // Malformed inputs reject instead of building a broken model.
  let bad = false;
  try { Model.from_mesh(positions, indices, faceOfTri.subarray(0, 3), solidOfTri, "bad"); }
  catch { bad = true; }
  assert(bad, "from_mesh rejects a face_of_tri length mismatch");

  const fm = Model.from_mesh(positions, indices, faceOfTri, solidOfTri, "meshbeam");
  assert(fm.has_cad_faces() === true, "from_mesh model exposes CAD faces");
  assert(fm.patch_count() === 6, `CAD-face segmentation: one patch per side (got ${fm.patch_count()})`);
  const fmTri = fm.triangle_count();
  assert(fmTri > 5000 && fmTri <= 160_000, `from_mesh refines for display like the bytes path (${fmTri} tris)`);
  assert(fm.body_count() === 1, "from_mesh box is one body");
  assert(fm.mesh_object_count() === 1, "one solid id → one mesh object");
  const fmBbox = Array.from(fm.bbox());
  assert(Math.abs(fmBbox[3] - 40) < 1e-4, "from_mesh bbox hi.x = 40");

  const fsel = patchSelector(fm);
  fm.set_material(2400, 0.35, 1.24, 50, 35);
  fm.set_resolution(20000);
  fm.add_fixed(fsel(0, "min"));
  fm.add_force(fsel(0, "max"), 0, 0, -5);
  assert(JSON.parse(fm.check()).ok === true, "from_mesh cantilever passes the check");
  const fmStats = JSON.parse(fm.solve());
  assert(fmStats.converged && fmStats.maxDisplacement > 0.001 && fmStats.maxDisplacement < 10,
    `from_mesh model solves (max |u| ${fmStats.maxDisplacement.toFixed(4)} mm)`);

  // .step project round-trip, exactly as the web open path performs it: the
  // original STEP text embeds verbatim under model.step; open re-tessellates
  // (same pinned meshStep + same file-derived opts ⇒ identical arrays) and
  // rebuilds via from_mesh + transform replay, then restores the design.
  const stepText = "ISO-10303-21;\nHEADER;ENDSEC;\nDATA;\n/* fixture stand-in */\nENDSEC;\nEND-ISO-10303-21;\n";
  const stepBytes = new TextEncoder().encode(stepText);
  JSON.parse(fm.optimize(JSON.stringify({
    budgetPct: 30, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
    smoothIters: 4, nBins: 3, floorPct: 10, capPct: 70, levelsPct: null,
    binary: false, solidPattern: null, goal: "budget",
  }), () => {}));
  fm.stash_result("optimized");
  const fmRegions = fm.region_count();
  const fmVd = fm.vertex_density();
  const manifest = JSON.stringify({ app: "filaSim", fileName: "meshbeam.step", transform: Array.from(fm.transform_matrix()) });
  const proj = fm.export_project(stepBytes, "model.step", manifest, true);
  assert(proj[0] === 0x50 && proj[1] === 0x4b, ".step project export is a zip");
  const embedded = project_model(proj);
  assert(embedded.length === stepBytes.length && embedded.every((b, i) => b === stepBytes[i]),
    "original STEP bytes embedded verbatim under model.step");
  assert(new TextDecoder("latin1").decode(embedded.subarray(0, 32)).includes("ISO-10303-21"),
    "embedded model sniffs as STEP (the worker's open-path dispatch)");

  const q = Model.from_mesh(positions, indices, faceOfTri, solidOfTri, "meshbeam");
  q.set_material(2400, 0.35, 1.24, 50, 35);
  q.set_resolution(20000);
  const qSummary = JSON.parse(q.restore_project(proj));
  assert(qSummary.hasDesign === true, "restore onto a from_mesh model reports the design");
  assert(qSummary.restoredResults.includes("optimized"), "optimized result restored onto from_mesh model");
  assert(q.region_count() === fmRegions, `regions match after from_mesh reopen (${q.region_count()} vs ${fmRegions})`);
  const qVd = q.vertex_density();
  let vdDiff = 0;
  for (let i = 0; i < qVd.length; i++) vdDiff = Math.max(vdDiff, Math.abs(qVd[i] - fmVd[i]));
  assert(vdDiff < 1e-6, "vertex density identical after from_mesh reopen");
  q.free();
  fm.free();
}
console.log("ok: pre-tessellated import (from_mesh + CAD patches + .step project round-trip)");

console.log("\nALL SMOKE TESTS PASSED");
