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

export class EngineClient {
  private worker: Worker;
  private nextId = 1;
  private pending = new Map<number, Pending>();

  constructor() {
    this.worker = new Worker(new URL("../worker/engine.worker.ts", import.meta.url), {
      type: "module",
    });
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

  load(bytes: ArrayBuffer, name: string): Promise<LoadedModel> {
    return this.call<LoadedModel>({ op: "load", bytes, name }, [bytes]);
  }

  resegment(angle: number): Promise<{ patchIds: Uint32Array; patchCount: number }> {
    return this.call({ op: "resegment", angle });
  }

  setMaterial(e0: number, nu: number, density: number): Promise<void> {
    return this.call({ op: "setMaterial", e0, nu, density });
  }

  setGravity(on: boolean): Promise<void> {
    return this.call({ op: "setGravity", on });
  }

  setResolution(cells: number): Promise<void> {
    return this.call({ op: "setResolution", cells });
  }

  setBcs(bcs: Bc[]): Promise<void> {
    // Copy tri arrays: the originals stay with the UI.
    const payload = bcs.map((bc) => ({
      kind: bc.kind,
      tris: new Uint32Array(bc.tris),
      force: bc.force,
      pressure: bc.pressure,
      stiffness: bc.stiffness,
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
    return this.call({ op: "solve" });
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

  exportThreeMf(): Promise<Uint8Array> {
    return this.call({ op: "exportThreeMf" });
  }

  exportStls(): Promise<Uint8Array> {
    return this.call({ op: "exportStls" });
  }
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
  /** Object-level internal_solid_infill_pattern for the export. */
  solidPattern: string | null;
}

export interface OptProgress {
  iteration: number;
  maxIter: number;
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
  seconds: number;
}

export interface OptimizeOutput {
  summary: OptSummary;
  regions: OptRegion[];
  vertexDensity: Float32Array;
  displacements: Float32Array;
}

export const engine = new EngineClient();
