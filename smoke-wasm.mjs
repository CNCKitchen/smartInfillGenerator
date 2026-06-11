// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

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
model.set_material(2000, 0.3, 1.24);
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

// Stress/strain result fields.
const fmax = (a) => a.reduce((m, v) => Math.max(m, v), -Infinity);
const fmin = (a) => a.reduce((m, v) => Math.min(m, v), Infinity);
const vmf = model.result_field("vm");
assert(vmf.length === nTri * 3 && vmf.every((v) => Number.isFinite(v)), "von Mises field per vertex");
assert(fmax(vmf) > 0, "von Mises has nonzero peak");
const sxxf = model.result_field("sxx");
assert(fmin(sxxf) < 0 && fmax(sxxf) > 0, "bending: sigma_xx tension + compression present");
const ezzf = model.result_field("ezz");
assert(ezzf.length === nTri * 3, "strain field per vertex");
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

// Resegmentation.
model.resegment(60);
assert(model.patch_count() === 6, "resegment at 60 deg still 6 patches");

// ---- full optimization pipeline ----
// Roomy beam (60x12x12 mm): enough interior beyond the skin to optimize.
const optModel = new Model(boxStl([0, 0, 0], [60, 12, 12]), "beam2");
const optTri = optModel.triangle_count();
const osel = patchSelector(optModel);
optModel.set_material(2400, 0.35, 1.24);
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

const threeMf = optModel.export_3mf();
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
const stlZip = optModel.export_stls();
assert(stlZip.length > 100 && stlZip[0] === 0x50, "STL zip export");
// Re-import our own 3MF (full circle; re-import subdivides for display again).
const reimported = new Model(threeMf, "roundtrip");
assert(reimported.triangle_count() >= 12, "exported 3MF re-imports (part wins by bbox)");

// ---- binary (hollow/solid) mode ----
// Smaller grid for speed: the point is the pipeline, not the physics here.
const binModel = new Model(boxStl([0, 0, 0], [60, 12, 12]), "beam3");
const bsel = patchSelector(binModel);
binModel.set_material(2400, 0.35, 1.24);
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
  const binMf = binModel.export_3mf();
  const raw = new TextDecoder("latin1").decode(binMf);
  assert(raw.includes('internal_solid_infill_pattern" value="concentric"'),
    "binary export carries the solid-fill pattern");
  assert(raw.indexOf("internal_solid_infill_pattern") < raw.indexOf("<part "),
    "solid pattern at object level");
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
matchModel.set_material(2400, 0.35, 1.24);
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

console.log("\nALL SMOKE TESTS PASSED");
