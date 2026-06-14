// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Promise wrapper around the engine worker.

import type { Bc, CheckReport, LoadedModel, SolveStats, VoxelInfo } from "../types";

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  onProgress?: (
    data: unknown,
    density: Float32Array,
    skelPositions?: Float32Array,
    skelIndices?: Uint32Array,
    skelDensity?: Float32Array
  ) => void;
}

/** Capacity of the live residual buffer (f32 slots). Caps the streamed
 *  preview length; the final exact trace comes back in the solve stats and is
 *  not limited by this. 1024 ≫ the worst-case MGCG iteration count. */
const PROGRESS_CAP = 1024;

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
    this.worker.onmessage = (ev) => {
      const { id, ok, data, error, progress, density, skelPositions, skelIndices, skelDensity } =
        ev.data;
      const p = this.pending.get(id);
      if (!p) return;
      if (progress) {
        p.onProgress?.(data, density, skelPositions, skelIndices, skelDensity);
        return;
      }
      this.pending.delete(id);
      if (ok) p.resolve(data);
      else p.reject(new Error(error));
    };
  }

  private call<T>(
    msg: Record<string, unknown>,
    transfer: Transferable[] = [],
    onProgress?: (data: unknown, density: Float32Array) => void
  ): Promise<T> {
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, onProgress });
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

  load(bytes: ArrayBuffer, name: string): Promise<LoadedModel> {
    return this.call<LoadedModel>({ op: "load", bytes, name }, [bytes]);
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
    strengthZ: number
  ): Promise<void> {
    return this.call({ op: "setMaterial", e0, nu, density, strength, strengthZ });
  }

  setGravity(on: boolean): Promise<void> {
    return this.call({ op: "setGravity", on });
  }

  setResolution(cells: number): Promise<void> {
    return this.call({ op: "setResolution", cells });
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
    const payload = bcs.map((bc) => ({
      kind: bc.kind,
      tris: new Uint32Array(bc.tris),
      force: bc.force,
      pressure: bc.pressure,
      stiffness: bc.stiffness,
      axes: bc.axes,
    }));
    return this.call(
      { op: "setBcs", bcs: payload },
      payload.map((b) => b.tris.buffer)
    );
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

  /** Analyze the part AS PRINTED: solid skin + uniform infill through the
   *  calibrated pattern law. Same fields as solve() plus print-mass stats. */
  solvePrinted(
    opts: PrintedOptions
  ): Promise<{ stats: PrintedStats; displacements: Float32Array }> {
    this.resetProgress();
    return this.call({ op: "solvePrinted", opts: opts as unknown as Record<string, unknown> });
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

  /** Stress/strain scalar per surface vertex (kind: vm|sxx|...|gzx). */
  resultField(kind: string): Promise<Float32Array> {
    return this.call({ op: "resultField", kind });
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

  /** Project 3MF in the chosen slicer's flavor. */
  exportThreeMf(slicer: SlicerFlavor): Promise<Uint8Array> {
    return this.call({ op: "exportThreeMf", slicer });
  }

  exportStls(): Promise<Uint8Array> {
    return this.call({ op: "exportStls" });
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
  /** True when the run was binary (hollow/solid) mode. */
  binary: boolean;
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
