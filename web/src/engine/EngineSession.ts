// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { engine } from "./EngineClient";

//! Owns the per-solution engine-session STATE that used to live as loose
//! module-level `let`s in the store: the fetched-field caches (one for the STL
//! result surface, one for the voxel hull), whether the voxel-result geometry
//! for the current solution is in the scene, and the live MGCG residual-poll
//! lifecycle. Consolidating it gives ONE invalidation path (instead of six
//! scattered `fieldCache.clear()` calls) and makes the caching/poll logic
//! reachable without React. The heavy solve/optimize choreography stays in the
//! store (it's interleaved with React state writes); this is the state it
//! leans on.

/** Sink for the voxel-result hull + nodal displacements (the viewport). */
export type VoxelResultEmit = (
  positions: Float32Array | null,
  displacements: Float32Array | null,
  edges: Float32Array | null,
  edgeDisplacements: Float32Array | null
) => void;

export class EngineSession {
  /** Per-kind cache of fetched stress/strain fields for the STL result surface. */
  private fieldCache = new Map<string, Float32Array>();
  /** Same, sized for the voxel-hull surface. */
  private voxFieldCache = new Map<string, Float32Array>();
  /** Is the voxel-hull geometry for the CURRENT solution in the scene? */
  private voxelLoaded = false;

  constructor(private emitVoxelResult: VoxelResultEmit) {}

  // ---- field caches ----

  fieldOf(kind: string, vox: boolean): Float32Array | undefined {
    return (vox ? this.voxFieldCache : this.fieldCache).get(kind);
  }

  setField(kind: string, vox: boolean, values: Float32Array) {
    (vox ? this.voxFieldCache : this.fieldCache).set(kind, values);
  }

  /** Drop the STL-surface fields (stress belongs to the previous solution). */
  clearFields() {
    this.fieldCache.clear();
  }

  /** Drop BOTH surface caches (post-processing toggle re-fetches the field). */
  clearAllFields() {
    this.fieldCache.clear();
    this.voxFieldCache.clear();
  }

  // ---- voxel-result hull ----

  get isVoxelLoaded(): boolean {
    return this.voxelLoaded;
  }

  /** New solution / model: voxel-result geometry + voxel field cache are stale. */
  invalidateVoxelResult() {
    this.voxelLoaded = false;
    this.voxFieldCache.clear();
    this.emitVoxelResult(null, null, null, null);
  }

  /** The full "new solution" reset: STL fields + voxel result both stale. */
  invalidateSolution() {
    this.fieldCache.clear();
    this.invalidateVoxelResult();
  }

  /** Fetch the voxel hull + nodal displacements once per solution. */
  async loadVoxelResult() {
    if (this.voxelLoaded) return;
    const r = await engine.voxelResults();
    this.emitVoxelResult(r.positions, r.displacements, r.edges, r.edgeDisplacements);
    this.voxelLoaded = true;
  }

  // ---- live residual poll ----

  /** Stream the live MGCG residual trace into `onTrace` a few times a second
   *  while a solve runs. Returns a stop function; a no-op when live streaming
   *  isn't available (no cross-origin isolation → the plot fills in at the end). */
  startResidualPoll(onTrace: (residuals: number[]) => void): () => void {
    if (engine.readSolveProgress() === null) return () => {};
    const timer = setInterval(() => {
      const live = engine.readSolveProgress();
      if (live && live.length) onTrace(live);
    }, 120);
    return () => clearInterval(timer);
  }
}
