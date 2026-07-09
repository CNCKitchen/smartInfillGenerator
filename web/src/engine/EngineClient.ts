// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Promise wrapper around the engine worker.

import type { Bc, CheckReport, CylFit, SolveStats, VoxelInfo } from "../types";
import { EngineError } from "./EngineProtocol";
import type {
  BcPayload,
  EngineRequests,
  EngineResponses,
  EngineWorkerMessage,
  LoadedModelData,
  Op,
  OrientationSweepResult,
  SectionVolume,
  SweepProgress,
} from "./EngineProtocol";

interface Pending {
  // Method syntax on purpose: the map stores one entry per in-flight op, so a
  // specific op's resolve is held as the all-ops union (bivariance applies).
  resolve(v: EngineResponses[Op]): void;
  reject(e: EngineError): void;
  onProgress?(
    data: unknown,
    density?: Float32Array,
    skelPositions?: Float32Array,
    skelIndices?: Uint32Array,
    skelDensity?: Float32Array
  ): void;
}

/** Capacity of the live residual buffer (f32 slots). Caps the streamed
 *  preview length; the final exact trace comes back in the solve stats and is
 *  not limited by this. 2048 covers the 2000-iteration MGCG cap. */
const PROGRESS_CAP = 2048;

export class EngineClient {
  private worker: Worker;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  /** Cancellation flag shared with the worker — settable while the worker
   *  is blocked inside a solve (a postMessage could never arrive mid-call).
   *  Needs cross-origin isolation (same requirement as threaded wasm). */
  private cancelFlag: Int32Array | null = null;
  /** Live MGCG residual trace shared with the worker, written DURING a solve
   *  (same mid-call constraint as the cancel flag). `progressCount[0]` is the
   *  number of valid residuals; `progressData[0..count]` are the relative
   *  residuals. Null without cross-origin isolation — the plot then just
   *  appears at the end as before. */
  private progressCount: Int32Array | null = null;
  private progressData: Float32Array | null = null;

  constructor() {
    this.worker = new Worker(new URL("../worker/engine.worker.ts", import.meta.url), {
      type: "module",
    });
    if (typeof SharedArrayBuffer !== "undefined" && self.crossOriginIsolated) {
      const buf = new SharedArrayBuffer(4);
      this.cancelFlag = new Int32Array(buf);
      void this.call({ op: "setCancelBuffer", buf });
      // count (i32) + up to PROGRESS_CAP residuals (f32); the worst-case MGCG
      // iteration count (~290 at the fine preset) fits with room to spare.
      const pbuf = new SharedArrayBuffer(4 + PROGRESS_CAP * 4);
      this.progressCount = new Int32Array(pbuf, 0, 1);
      this.progressData = new Float32Array(pbuf, 4, PROGRESS_CAP);
      void this.call({ op: "setProgressBuffer", buf: pbuf });
    }
    this.worker.onmessage = (ev: MessageEvent<EngineWorkerMessage>) => {
      const msg = ev.data;
      const p = this.pending.get(msg.id);
      if (!p) return;
      if ("progress" in msg) {
        p.onProgress?.(msg.data, msg.density, msg.skelPositions, msg.skelIndices, msg.skelDensity);
        return;
      }
      this.pending.delete(msg.id);
      if (msg.ok) p.resolve(msg.data);
      else p.reject(new EngineError(msg.error));
    };
  }

  private call<O extends Op>(
    msg: { op: O } & EngineRequests[O],
    transfer: Transferable[] = [],
    onProgress?: Pending["onProgress"]
  ): Promise<EngineResponses[O]> {
    const id = this.nextId++;
    return new Promise<EngineResponses[O]>((resolve, reject) => {
      // One type-erasure point: the map holds every op's entry, so the
      // op-specific resolve widens to the union here (call sites stay typed).
      this.pending.set(id, { resolve, reject, onProgress } as Pending);
      this.worker.postMessage({ id, ...msg }, transfer);
    });
  }

  /** True when stop/cancel is available (cross-origin isolated context). */
  get canCancel(): boolean {
    return this.cancelFlag !== null;
  }

  /** Request the running solve/optimization to stop at its next checkpoint
   *  (each CG iteration polls the flag). The pending call rejects with
   *  "cancelled". No-op outside cross-origin isolation. */
  cancel() {
    if (this.cancelFlag) Atomics.store(this.cancelFlag, 0, 1);
  }

  /** Clear the live residual trace. Called on the main thread just before a
   *  solve starts (synchronously, before polling begins) so the plot never
   *  shows the previous solve's curve. */
  private resetProgress() {
    if (this.progressCount) this.progressCount[0] = 0;
  }

  /** Snapshot of the residual trace streamed so far by the running solve, or
   *  null when live streaming is unavailable (no cross-origin isolation).
   *  Poll this while a solve is in flight to animate the convergence plot. */
  readSolveProgress(): number[] | null {
    if (!this.progressCount || !this.progressData) return null;
    const n = Math.min(this.progressCount[0], this.progressData.length);
    return n > 0 ? Array.from(this.progressData.subarray(0, n)) : [];
  }

  load(bytes: ArrayBuffer, name: string): Promise<LoadedModelData> {
    return this.call({ op: "load", bytes, name }, [bytes]);
  }

  resegment(angle: number): Promise<{ patchIds: Uint32Array; patchCount: number }> {
    return this.call({ op: "resegment", angle });
  }

  /** Switch surface patches to the STEP file's exact BREP faces. No-op (returns
   *  the current patches) for STL/3MF models. */
  useCadFaces(): Promise<{ patchIds: Uint32Array; patchCount: number }> {
    return this.call({ op: "useCadFaces" });
  }

  /** Rigid-transform the part (matrix = [r00..r22 row-major, tx, ty, tz]).
   *  Patches and BCs survive; grid/results drop. Returns the moved display
   *  mesh and its new bbox. */
  transform(matrix: number[]): Promise<{ positions: Float32Array; bbox: number[] }> {
    return this.call({ op: "transform", matrix });
  }

  setMaterial(
    e0: number,
    nu: number,
    density: number,
    strength: number,
    strengthZ: number,
    shearStrengthZ?: number
  ): Promise<void> {
    return this.call({ op: "setMaterial", e0, nu, density, strength, strengthZ, shearStrengthZ });
  }

  setGravity(on: boolean): Promise<void> {
    return this.call({ op: "setGravity", on });
  }

  setResolution(cells: number): Promise<void> {
    return this.call({ op: "setResolution", cells });
  }

  /** Custom resolution: pin the analysis cell size to exactly `h` mm. */
  setVoxelSize(h: number): Promise<void> {
    return this.call({ op: "setVoxelSize", h });
  }

  /** Snap the voxel size to wall/k so the skin is k cell layers (0 = off). */
  setSnapWall(wall: number): Promise<void> {
    return this.call({ op: "setSnapWall", wall });
  }

  /** Composite skin: surface cells the wall only partially covers get a
   *  blended (part-wall, part-infill) stiffness instead of rounding the
   *  skin to whole voxel layers — thin walls stay representable on coarse
   *  grids. Off = legacy whole-layer skin. */
  setCompositeSkin(on: boolean): Promise<void> {
    return this.call({ op: "setCompositeSkin", on });
  }

  /** Smoothed stress display: result fields are volume-averaged to the grid
   *  nodes and evaluated at the true surface, instead of painting each
   *  cell's center value flat — removes the staircase checkerboard. Pure
   *  post-processing; the solution is untouched. */
  setSmoothStress(on: boolean): Promise<void> {
    return this.call({ op: "setSmoothStress", on });
  }

  /** Material (occupancy-decoupled) stress display: report the TRUE material
   *  stress at finite-cell cut cells instead of the occupancy-scaled value —
   *  removes the curved-skin staircase stripes. Pure post-processing; the
   *  solution and the safety factor are untouched. */
  setMaterialStress(on: boolean): Promise<void> {
    return this.call({ op: "setMaterialStress", on });
  }

  setBcs(bcs: Bc[]): Promise<void> {
    // Copy tri arrays: the originals stay with the UI.
    const payload: BcPayload[] = bcs.map((bc) => ({
      kind: bc.kind,
      tris: new Uint32Array(bc.tris),
      force: bc.force,
      pressure: bc.pressure,
      stiffness: bc.stiffness,
      axes: bc.axes,
      disp: bc.disp,
      moment: bc.moment,
    }));
    return this.call(
      { op: "setBcs", bcs: payload },
      payload.map((b) => b.tris.buffer)
    );
  }

  /** Fit a triangle selection to a cylinder (for bearing-load validation).
   *  Returns the axis/radius and a cylindricity `ok` flag. */
  fitCylinder(tris: Uint32Array): Promise<CylFit> {
    const copy = new Uint32Array(tris);
    return this.call({ op: "fitCylinder", tris: copy }, [copy.buffer]);
  }

  voxelInfo(): Promise<VoxelInfo> {
    return this.call({ op: "voxelInfo" });
  }

  check(): Promise<CheckReport> {
    return this.call({ op: "check" });
  }

  solve(): Promise<{ stats: SolveStats; displacements: Float32Array }> {
    this.resetProgress();
    return this.call({ op: "solve" });
  }

  /** Re-solve the optimized design under the CURRENT BCs (DESIGN §13): the
   *  per-step pass that evaluates one multi-load design under every load step.
   *  Reuses the optimized stress eps, so it's a cheap RHS swap when the step
   *  shares fixtures. Requires a prior optimize(). */
  solveOptimized(): Promise<{ stats: SolveStats; displacements: Float32Array }> {
    this.resetProgress();
    return this.call({ op: "solveOptimized" });
  }

  /** Analyze the part AS PRINTED: solid skin + uniform infill through the
   *  calibrated pattern law. Same fields as solve() plus print-mass stats. */
  solvePrinted(
    opts: PrintedOptions
  ): Promise<{ stats: PrintedStats; displacements: Float32Array }> {
    this.resetProgress();
    return this.call({ op: "solvePrinted", opts });
  }

  /** FDM build simulation (inherent strain): warping + bed peel. Leaves the
   *  chosen state (released/bonded) as the live solution, so the returned
   *  displacements deform the real mesh in the existing deformed view.
   *  `onProgress(p, displacements)` fires per activated layer — `p` is the
   *  layer count (done/total) for the progress bar, `displacements` is the
   *  accumulating warp on throttled frames (length 0 on progress-only frames). */
  buildSim(
    opts: BuildSimOptions,
    onProgress?: (
      p: { done: number; total: number; maxU: number },
      positions: Float32Array,
      mags: Float32Array
    ) => void
  ): Promise<{ stats: BuildSimStats; displacements: Float32Array }> {
    this.resetProgress();
    return this.call({ op: "buildSim", opts }, [], onProgress as Pending["onProgress"]);
  }

  /** Constrained undamped modal analysis: the lowest `numModes` natural
   *  frequencies + mode shapes on the CURRENT supports (the store sets these to
   *  the first load case). Stashes each mode as `modal::mode-i` and leaves mode
   *  0 live, returning its mesh displacements for the deformed view. */
  modalAnalysis(
    opts: ModalOptions,
    onProgress?: (p: ModalProgress) => void
  ): Promise<{ result: ModalAnalysisResult; displacements: Float32Array }> {
    this.resetProgress();
    return this.call(
      { op: "modalAnalysis", opts },
      [],
      onProgress ? (data: unknown) => onProgress(data as ModalProgress) : undefined
    );
  }

  /** Flip the saved build-sim result between on-bed and released without a
   *  re-solve; returns the new max displacement + the re-mapped mesh
   *  displacements for the deformed view. */
  setBuildState(
    state: "released" | "bonded"
  ): Promise<{ stats: { maxDisplacement: number }; displacements: Float32Array }> {
    return this.call({ op: "setBuildState", state });
  }

  /** Orientation sweep (DESIGN §15): min layer-adhesion SF per pitch/roll
   *  pixel over the ±90° hemisphere, folding `ids` result stashes worst-case
   *  ([] = current solution). `onProgress` fires per row chunk. */
  orientationSweep(
    ids: string[],
    stepDeg: number,
    onProgress?: (p: SweepProgress) => void
  ): Promise<OrientationSweepResult> {
    return this.call(
      { op: "orientationSweep", ids, stepDeg },
      [],
      onProgress ? (data: unknown) => onProgress(data as SweepProgress) : undefined
    );
  }

  /** Per-vertex layer-adhesion SF for one build direction — the heatmap
   *  click preview recolor. Folds the same result set as the sweep.
   *  `surface` "voxel" samples the voxel hull per cell, with constraint-ring
   *  cells NaN (painted grey). */
  layerSfField(
    dir: [number, number, number],
    ids: string[],
    surface: "stl" | "voxel"
  ): Promise<Float32Array> {
    return this.call({ op: "layerSfField", dir, ids, surface });
  }

  /** Toggle the shear term of the layer criterion (sfz/sf fields, sweep,
   *  preview — display-side derived, the solution stays valid). */
  setLayerShear(on: boolean): Promise<void> {
    return this.call({ op: "setLayerShear", on });
  }

  optimize(
    opts: OptimizeOptions,
    onProgress: (
      p: OptProgress,
      density: Float32Array,
      skelPositions?: Float32Array,
      skelIndices?: Uint32Array,
      skelDensity?: Float32Array
    ) => void
  ): Promise<OptimizeOutput> {
    return this.call({ op: "optimize", opts }, [], onProgress as Pending["onProgress"]);
  }

  /** Exposed-face hull + cell edges of the analysis voxel grid. */
  voxelMesh(): Promise<{ hull: Float32Array; edges: Float32Array; info: VoxelInfo }> {
    return this.call({ op: "voxelMesh" });
  }

  /** Voxel mesh with a per-vertex element DENSITY (0..1: skin = 1, interior
   *  = its infill ratio — optimized densities when available, composite
   *  cells blended), optionally cut by a plane — cells on the dropped side
   *  vanish entirely (voxel-true section). Plane in three.js convention:
   *  kept side is normal·p + constant ≥ 0. */
  voxelMeshCut(
    plane: { normal: [number, number, number]; constant: number } | null,
    wall: number,
    topBottomMm: number,
    infillPct: number
  ): Promise<{ hull: Float32Array; density: Float32Array; edges: Float32Array; info: VoxelInfo }> {
    return this.call({ op: "voxelMeshCut", plane, wall, topBottomMm, infillPct });
  }

  /** Isosurface of the final continuous density field at `threshold` (0..1). */
  densityShape(
    threshold: number
  ): Promise<{ positions: Float32Array; indices: Uint32Array; density: Float32Array }> {
    return this.call({ op: "densityShape", threshold });
  }

  /** Re-smooth the extracted regions live (affects display + exports). */
  resmoothRegions(iters: number): Promise<{ regions: OptRegion[] }> {
    return this.call({ op: "resmooth", iters });
  }

  /** Re-extract the exported geometry at a new isosurface density (Part Topo /
   *  binary). Returns the new regions for display; affects later exports. */
  setIsoThreshold(threshold: number, smoothIters: number): Promise<{ regions: OptRegion[] }> {
    return this.call({ op: "setIsoThreshold", threshold, smoothIters });
  }

  /** Stress/strain scalar per surface vertex (kind: vm|sxx|...|gzx). */
  resultField(kind: string): Promise<Float32Array> {
    return this.call({ op: "resultField", kind });
  }

  /** Volumetric section payload (nodal field over the full solution grid +
   *  nodal displacements + interior extremes) for the capped section view.
   *  Kinds as in `resultField`, plus "u"|"ux"|"uy"|"uz" (disp-only). */
  sectionVolume(kind: string): Promise<SectionVolume> {
    return this.call({ op: "sectionVolume", kind });
  }

  /** Build-sim bed-peel traction per surface vertex: "peel" = upward lift,
   *  "peelshear" = bed shear. MPa, mesh-independent, uncalibrated indicator. */
  peelField(kind: "peel" | "peelshear"): Promise<Float32Array> {
    return this.call({ op: "peelField", kind });
  }

  /** Build-sim bed-peel as a flat heatmap on the plate: triangle-soup positions
   *  over the footprint + a per-vertex traction (lift or shear, MPa). */
  peelMap(
    kind: "peel" | "peelshear"
  ): Promise<{ positions: Float32Array; values: Float32Array }> {
    return this.call({ op: "peelMap", kind });
  }

  /** Inherent-strain preview: the voxel hull up to build layer `layerMax`, with
   *  a per-vertex source value (0..1 normalised) + edges, peak (MPa) and the
   *  layer count `nz`. */
  inherentStrainVoxels(
    layerMax: number,
    shrinkXy: number,
    shrinkZ: number
  ): Promise<{ hull: Float32Array; values: Float32Array; edges: Float32Array; max: number; nz: number }> {
    return this.call({ op: "inherentStrainVoxels", layerMax, shrinkXy, shrinkZ });
  }

  /** Voxel hull with exact nodal displacements (results-on-voxel-mesh view). */
  voxelResults(): Promise<{
    positions: Float32Array;
    displacements: Float32Array;
    edges: Float32Array;
    edgeDisplacements: Float32Array;
  }> {
    return this.call({ op: "voxelResults" });
  }

  /** Result field per voxel-hull vertex (owning cell's value, flat per cell). */
  voxelResultField(kind: string): Promise<Float32Array> {
    return this.call({ op: "voxelResultField", kind });
  }

  /** Snapshot the current solution under `id` for the Results-view switcher. */
  stashResult(id: string): Promise<void> {
    return this.call({ op: "stashResult", resultId: id });
  }

  /** Make a stashed result the live solution; returns its per-soup-vertex
   *  displacements so the viewport can re-deform in one round-trip. */
  activateResult(id: string): Promise<Float32Array> {
    return this.call({ op: "activateResult", resultId: id });
  }

  /** Drop every stashed result (geometry/grid change — all stale). */
  clearResults(): Promise<void> {
    return this.call({ op: "clearResults" });
  }

  /** Drop all registered multi-load optimization cases (DESIGN §13). */
  clearLoadCases(): Promise<void> {
    return this.call({ op: "clearLoadCases" });
  }

  /** Snapshot the CURRENT BCs as a weighted load case for the multi-load
   *  optimizer. Call after `setBcs` has pushed this step's effective BCs. */
  addLoadCase(weight: number): Promise<void> {
    return this.call({ op: "addLoadCase", weight });
  }

  /** Cumulative orientation transform since import (for project save). */
  transformMatrix(): Promise<number[]> {
    return this.call({ op: "transformMatrix" });
  }

  /** Assemble a `.filasim` project zip (model + manifest + design + results). */
  exportProject(manifest: string, modelEntry: string, includeResults: boolean): Promise<Uint8Array> {
    return this.call({ op: "exportProject", manifest, modelEntry, includeResults });
  }

  /** Phase 1 of open: extract the manifest + rebuild the Model from the embedded
   *  file. The caller then pushes settings/orientation/BCs and calls
   *  openProjectRestore. */
  openProjectModel(
    bytes: ArrayBuffer
  ): Promise<{ manifest: string; model: LoadedModelData }> {
    return this.call({ op: "openProjectModel", bytes }, [bytes]);
  }

  /** Phase 2 of open: restore the design + result buffers into the configured
   *  Model. Returns which results came back. */
  openProjectRestore(): Promise<{ restoredResults: string[]; hasDesign: boolean }> {
    return this.call({ op: "openProjectRestore" });
  }

  /** Final binned density per soup vertex (Density view) — standalone fetch
   *  for restoring a project's design without re-optimizing. */
  vertexDensity(): Promise<Float32Array> {
    return this.call({ op: "vertexDensity" });
  }

  /** Project 3MF in the chosen slicer's flavor. `thumbnail` (PNG bytes) becomes
   *  the plate preview; null falls back to an embedded placeholder. */
  exportThreeMf(slicer: SlicerFlavor, thumbnail?: Uint8Array | null): Promise<Uint8Array> {
    return this.call({ op: "exportThreeMf", slicer, thumbnail: thumbnail ?? null });
  }

  /** Standalone colored 3MF of the active result field, painted into `steps`
   *  discrete Bambu/Orca filament bands. `kind` is the on-screen field, `lo`/`hi`
   *  the active contour min/max, `colors` the per-band `#RRGGBB` ramp (low first). */
  exportColorThreeMf(
    kind: string,
    lo: number,
    hi: number,
    steps: number,
    colors: string[],
    thumbnail?: Uint8Array | null
  ): Promise<Uint8Array> {
    return this.call({
      op: "exportColorThreeMf",
      kind,
      lo,
      hi,
      steps,
      colors,
      thumbnail: thumbnail ?? null,
    });
  }

  exportStls(): Promise<Uint8Array> {
    return this.call({ op: "exportStls" });
  }

  /** Solid topology mode: the single optimized body as one binary STL. */
  exportSolidStl(): Promise<Uint8Array> {
    return this.call({ op: "exportSolidStl" });
  }
}

/** Target slicer for the project 3MF export. "bambu" maps the rectilinear
 *  pattern value to Bambu Studio's renamed "zig-zag"; "prusa" writes the
 *  PrusaSlicer volume/config format. */
export type SlicerFlavor = "orca" | "bambu" | "prusa";

/** Mirrors the wasm PrintedOpts (serialized to JSON in the worker). */
export interface PrintedOptions {
  /** Uniform interior infill in percent — the slicer setting. */
  infillPct: number;
  /** Calibrated pattern law E/E₀ = coeff·ρ^exponent. */
  exponent: number;
  coeff: number;
  perimeters: number;
  lineWidth: number;
  /** Solid top/bottom shells: layers × layer height; 0 = none. */
  topBottomLayers: number;
  layerHeight: number;
}

/** solve() stats plus the as-printed extras. */
export interface PrintedStats extends SolveStats {
  /** Part mass at these print settings (solid skin + infill interior). */
  massGrams: number;
  /** Mass if the part printed fully dense. */
  massSolidGrams: number;
  skinCells: number;
  interiorCells: number;
  /** Cell layers the skin is modeled with. Legacy mode: rounded, minimum 1.
   *  Composite mode: exact wall/h — fractional (and < 1) values are real
   *  and handled by blending. */
  skinLayers: number;
  /** True when the solve used the composite (blended) skin model. */
  compositeSkin: boolean;
}

/** Live per-outer-iteration progress of a modal run. */
export interface ModalProgress {
  outer: number;
  maxOuter: number;
  /** Current Ritz frequency estimates (Hz) for the requested modes. */
  freqs: number[];
}

/** Mirrors the wasm ModalOpts (serialized to JSON in the worker). */
export interface ModalOptions {
  /** Number of natural frequencies / mode shapes to compute (1–20). */
  numModes: number;
  /** true = solid reference (E₀ + full density); false = as-printed. */
  solid: boolean;
  /** Free-free: run unconstrained (soft-anchored), discarding the 6 rigid-body
   *  modes — for a part with no supports. false = constrained. */
  free: boolean;
  /** As-printed model params (ignored when `solid`) — mirror PrintedOptions. */
  infillPct: number;
  exponent: number;
  coeff: number;
  perimeters: number;
  lineWidth: number;
  topBottomLayers: number;
  layerHeight: number;
}

/** Result of a modal run: one entry per computed mode + convergence info. */
export interface ModalAnalysisResult {
  converged: boolean;
  outerIters: number;
  /** Total inner MGCG iterations (≈ multigrid V-cycles) — the dominant cost. */
  totalInnerIters: number;
  seconds: number;
  /** Per mode: the stash id (`modal::mode-i`) and natural frequency in Hz,
   *  ascending. The store builds one ResultEntry per mode from this. */
  modes: { id: string; freqHz: number }[];
}

/** Mirrors the wasm BuildSimOpts (serialized to JSON in the worker). */
export interface BuildSimOptions {
  /** In-plane (XY) per-layer shrink fraction (negative = shrink). */
  shrink: number;
  /** Through-layer (Z) per-layer shrink fraction (transverse isotropy). */
  shrinkZ: number;
  /** Which state to deform the mesh by: off-bed sprung shape or held on bed. */
  state: "released" | "bonded";
  /** Display exaggeration baked into the live preview hull positions. */
  exaggeration: number;
  /** Material yield stress (MPa); >0 enables the plastic step so the released
   *  warp depends on infill density. 0/omitted = pure-elastic (density-blind). */
  yieldStrength?: number;
  /** Locking temperature (°C): Tg amorphous / Tc semi-crystalline. All four
   *  temperatures must be present to enable the temperature ladder; omitting
   *  any keeps the legacy uniform-shrink behavior. */
  tLock?: number;
  /** Bed temperature (°C). */
  tBed?: number;
  /** Chamber/ambient temperature (°C). */
  tChamber?: number;
  /** Final (room) temperature (°C) after removal from the bed. */
  tFinal?: number;
  /** Bed heat-penetration depth in mm (engine default 3.0). */
  decayMm?: number;
}

/** Build-sim stats. `maxDisplacement` is the shown state; bonded/released give
 *  both states' peak, peak* are the bed-peel TRACTION maxima (MPa, uncalibrated). */
export interface BuildSimStats {
  maxDisplacement: number;
  bondedMax: number;
  releasedMax: number;
  peakLift: number;
  peakShear: number;
  layers: number;
  itersMax: number;
  itersMean: number;
  cells: number;
  seconds: number;
  /** True when the sim used the optimized as-printed infill density; false = solid hull. */
  densityAware: boolean;
  /** Coarse build-grid dimensions (distinct from the analysis grid). */
  nx: number;
  ny: number;
  nz: number;
  h: number;
}

/** Mirrors the wasm OptimizeOpts (serialized to JSON in the worker). */
export interface OptimizeOptions {
  /** Target mean interior infill density in percent. */
  budgetPct: number;
  /** Calibrated pattern law E/E₀ = coeff·ρ^exponent — used for evaluation. */
  exponent: number;
  coeff: number;
  perimeters: number;
  lineWidth: number;
  smoothIters: number;
  nBins: number;
  /** Printable density band in percent. */
  floorPct: number;
  capPct: number;
  /** Manual level override in percent; null = auto placement. */
  levelsPct: number[] | null;
  /** Binary (hollow/solid) mode — optimizer runs SIMP-penalized (p=3). */
  binary: boolean;
  /** Solid topology mode — material removal (no skin); budget = retained
   *  volume fraction; output is one optimized shape. Overrides `binary`. */
  solid: boolean;
  /** Solid mode: keep the cells under loads/supports solid (default true). */
  retainBc: boolean;
  /** Self-supporting overhang filter. */
  selfSupport: boolean;
  /** Overhang angle from horizontal in degrees for the self-supporting filter. */
  overhangDeg: number;
  /** Per-modifier sparse_infill_pattern for the export (binary mode). */
  solidPattern: string | null;
  /** "budget" = stiffest at the given mean infill; "match" = lightest design
   *  as stiff as a uniform print at budgetPct (secant on the budget). */
  goal: "budget" | "match";
  /** Planar symmetry constraint: [nx, ny, nz, c] of the plane n·p = c
   *  (world mm). null = unconstrained. */
  symmetry: number[] | null;
  /** Solid top/bottom shells: layers × layer height; 0 = none. */
  topBottomLayers: number;
  layerHeight: number;
  /** Minimum member size in mm (printability length scale) driving the
   *  density-filter radius; 0 = off (numerical floor only). Resolved from the
   *  store's auto/override before the call. */
  minMemberMm: number;
}

export interface OptProgress {
  iteration: number;
  maxIter: number;
  /** Outer pass (stiffness-match mode runs several warm-started passes). */
  pass: number;
  passes: number;
  /** Budget the current pass is running at (0..1). */
  budgetNow: number;
  /** Compliance estimate from the (inexact, warm-started) inner solve. */
  compliance: number;
  /** Total mass fraction of solid (skin + interior). */
  massFrac: number;
  /** Mean infill density over the interior cells. */
  meanInfill: number;
  /** Max per-cell density change of this design update. */
  change: number;
  /** Mean per-cell density change (the convergence signal, threshold 0.005). */
  meanChange: number;
  /** MGCG iterations the inner solve spent this iteration. */
  innerIters: number;
  /** Relative residual the inner solve reached. */
  innerRes: number;
}

export interface OptRegion {
  density: number;
  positions: Float32Array;
  indices: Uint32Array;
}

export interface OptSummary {
  iterations: number;
  converged: boolean;
  bins: { density: number; cells: number }[];
  baseDensity: number;
  regionCount: number;
  massGrams: number;
  massSolidGrams: number;
  massFrac: number;
  /** Achieved mean infill of the binned layout (0..1) — the uniform-print
   *  percentage the comparison references ("vs X% uniform, same weight"). */
  meanInfill: number;
  /** Requested infill budget after printable-floor/cap clamping (0..1). */
  targetInfill: number;
  stiffnessVsSolid: number;
  gainVsUniform: number;
  maxDisplacement: number;
  /** Max |u| of the equal-mass uniform + fully-solid baseline solves (the
   *  optimizer already solved them) — for the Results-view switcher. Present
   *  only when `hasBaselines` (infill modes). */
  uniformMaxDisp?: number;
  solidMaxDisp?: number;
  /** Infill modes expose the uniform + solid baselines as selectable results. */
  hasBaselines?: boolean;
  /** True when the run was binary (hollow/solid) mode. */
  binary: boolean;
  /** True when the run was solid topology (material-removal) mode. */
  solid: boolean;
  /** Optimization goal of the run. */
  goal: "budget" | "match";
  /** Outer passes executed (1 for budget mode). */
  passes: number;
  // ---- match mode only ----
  /** Reference uniform infill the stiffness was matched to (percent). */
  refUniformPct?: number;
  targetCompliance?: number;
  achievedCompliance?: number;
  /** achieved/target − 1; positive = slightly more compliant than target. */
  matchDeviation?: number;
  /** Mass of the uniform reference print (same skin, ref% interior). */
  massUniformRefGrams?: number;
  passTrace?: { budget: number; compliance: number }[];
  seconds: number;
}

export interface OptimizeOutput {
  summary: OptSummary;
  regions: OptRegion[];
  vertexDensity: Float32Array;
  displacements: Float32Array;
}

export const engine = new EngineClient();
