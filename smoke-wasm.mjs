// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Functional smoke test of the wasm-bindgen Model API (the same calls the web
// worker makes). Run: node smoke-wasm.mjs
import { readFileSync } from "node:fs";
import init, {
  Model,
  set_cancel_flag,
  set_progress_buffer,
  project_manifest,
  project_model,
} from "./web/src/wasm/sig_wasm.js";

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
// Safety factor: sigma_t·rel(rho) / sigma_vM, capped at 99.
const sff = model.result_field("sf");
assert(sff.length === nTri * 3 && sff.every((v) => Number.isFinite(v) && v > 0 && v <= 99),
  "safety factor field per vertex (finite, positive, capped)");
assert(fmin(sff) > 1 && fmin(sff) < 99,
  `min safety factor sensible for a lightly loaded beam (${fmin(sff).toFixed(1)})`);
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
  const vr = model.voxel_results();
  const vpos = vr[0], vdisp = vr[1], vedges = vr[2], vedisp = vr[3];
  assert(vpos.length > 0 && vpos.length % 9 === 0, `voxel result hull (${vpos.length / 9} tris)`);
  assert(vdisp.length === vpos.length, "hull displacement per vertex");
  assert(vedges.length > 0 && vedisp.length === vedges.length, "edge displacements match edges");
  let vmax = 0;
  for (let i = 0; i < vdisp.length; i += 3)
    vmax = Math.max(vmax, Math.hypot(vdisp[i], vdisp[i + 1], vdisp[i + 2]));
  assert(Math.abs(vmax - stats.maxDisplacement) < 0.05 * stats.maxDisplacement + 1e-6,
    `voxel nodal max |u| matches solve (${vmax.toFixed(4)} vs ${stats.maxDisplacement.toFixed(4)})`);
  const vvm = model.voxel_result_field("vm");
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
  assert(sfPrinted.every((v) => Number.isFinite(v) && v > 0 && v <= 99), "printed SF field sane");
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
const t1 = performance.now();
const summary = JSON.parse(
  // 35% infill budget (mean interior density); gyroid law E = 1.0*E0*rho^1.5;
  // 2 perimeters x 0.45 mm line width; 8 smoothing passes
  optModel.optimize(JSON.stringify({
    budgetPct: 35, exponent: 1.5, coeff: 1.0, perimeters: 2, lineWidth: 0.45,
    smoothIters: 8, nBins: 3, floorPct: 10, capPct: 70, levelsPct: null,
    binary: false, solidPattern: null, goal: "budget",
  }), (json, density, skelPos, skelIdx, skelDen) => {
    progressCalls++;
    lastDensityLen = density.length;
    if (skelIdx && skelIdx.length) skelTris = skelIdx.length / 3;
    if (skelPos && skelDen && skelDen.length * 3 === skelPos.length) skelColored = true;
    const p = JSON.parse(json);
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
// Smaller grid for speed: the point is the pipeline, not the physics here.
const binModel = new Model(boxStl([0, 0, 0], [60, 12, 12]), "beam3");
const bsel = patchSelector(binModel);
binModel.set_material(2400, 0.35, 1.24, 50, 35);
binModel.set_resolution(25000);
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

// ---- project (.infeall) save / load round-trip ----
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

  const manifest = JSON.stringify({ app: "InFEAll", fileName: "projbeam.stl", transform: accum });

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

  // A grid change retires the kept design → clean error (no stale evaluation).
  so.set_resolution(80000);
  let stale = false;
  try { so.solve_optimized(); } catch (e) { stale = /no optimized design|predates the current grid/i.test(String(e)); }
  assert(stale, "solve_optimized refuses a design that predates a grid change");
}
console.log("ok: per-step optimized evaluation (solve_optimized across load steps)");

console.log("\nALL SMOKE TESTS PASSED");
