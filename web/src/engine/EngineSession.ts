// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

import { engine } from "./EngineClient";
import type { SectionVolume } from "./EngineProtocol";

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
  /** Per-kind volumetric section payloads (nodal field + displacements) for
   *  the capped section view — invalidated with the STL field cache. */
  private sectionVolumeCache = new Map<string, SectionVolume>();
  /** Is the voxel-hull geometry for the CURRENT solution in the scene? */
  private voxelLoaded = false;

  /** `solidBody` reports whether the ACTIVE result is the Part Topo optimized
   *  body (read at fetch time): the voxel hull + its fields are then masked to
   *  the retained cells so results display on the optimized shape. Any flip of
   *  the flag goes through a result switch or threshold change, both of which
   *  invalidate the hull + voxel field cache, so cached data never crosses. */
  constructor(
    private emitVoxelResult: VoxelResultEmit,
    private solidBody: () => boolean = () => false
  ) {}

  // ---- field caches ----

  fieldOf(kind: string, vox: boolean): Float32Array | undefined {
    return (vox ? this.voxFieldCache : this.fieldCache).get(kind);
  }

  setField(kind: string, vox: boolean, values: Float32Array) {
    (vox ? this.voxFieldCache : this.fieldCache).set(kind, values);
  }

  sectionVolumeOf(kind: string): SectionVolume | undefined {
    return this.sectionVolumeCache.get(kind);
  }

  setSectionVolume(kind: string, data: SectionVolume) {
    this.sectionVolumeCache.set(kind, data);
  }

  /** Drop the STL-surface fields (stress belongs to the previous solution). */
  clearFields() {
    this.fieldCache.clear();
    this.sectionVolumeCache.clear();
  }

  /** Drop BOTH surface caches (post-processing toggle re-fetches the field). */
  clearAllFields() {
    this.fieldCache.clear();
    this.voxFieldCache.clear();
    this.sectionVolumeCache.clear();
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
    this.clearFields();
    this.invalidateVoxelResult();
  }

  /** Fetch the voxel hull + nodal displacements once per solution. */
  async loadVoxelResult() {
    if (this.voxelLoaded) return;
    const r = await engine.voxelResults(this.solidBody());
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

  // ---- run re-entrancy lock ----

  /** The heavy runs (check / solve / optimize) drive ONE worker holding ONE Rust
   *  model through a sequence of stateful calls (pushBcs → solve …), so letting
   *  two overlap interleaves those messages and corrupts the model. The
   *  busy-disabled buttons race a render frame and miss programmatic callers,
   *  and `busy` clears before the result tail (voxel hull, safety factors)
   *  finishes — so the run orchestrations take this lock instead. */
  private running = false;

  /** Claim the run lock; returns false if a run is already in flight so the
   *  caller bails without starting a second one. */
  beginRun(): boolean {
    if (this.running) return false;
    this.running = true;
    return true;
  }

  /** Release the run lock. Call from a `finally` so every exit path frees it. */
  endRun() {
    this.running = false;
  }
}
