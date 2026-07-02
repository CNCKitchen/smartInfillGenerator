// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Owns the RESULT-FIELD display pipeline: given the current display state,
//! decide which of the four fetch/compute paths produces the scalar painted
//! on the deformed view, and whether a resolved fetch is still current when
//! it lands. The four paths (previously inlined in the store's
//! `pushScalarField`):
//!
//!   1. envelope     — worst case across a kind's load steps, reduced
//!                     client-side from the per-step stashes (DESIGN §13);
//!   2. peel maps    — build-sim bed-peel / peel-shear heatmap on the plate;
//!   3. displacement — |u| or a signed X/Y/Z component, colored client-side
//!                     from the displacement buffer (no engine fetch);
//!   4. engine field — any stress/strain/SF scalar fetched from the worker,
//!                     sized for the active result surface (STL vs voxel).
//!
//! Staleness: every async path re-reads the LIVE display state after each
//! await and applies its result only while its validity predicate still
//! holds. The predicates are intentionally NOT uniform — they mirror the
//! store's historical inline guards exactly: a plain field/peel fetch dies
//! only when `resultField` moves on (a result switch re-pushes anyway),
//! while an envelope reduction also dies when the active result stops being
//! that kind's envelope.
//!
//! The server never writes zustand state and never talks to the scene — the
//! store adapts it through the narrow `FieldSink` and stays the only writer.

import { engine } from "./EngineClient";
import type { EngineSession } from "./EngineSession";

/** Sentinel `loadStepId` for the envelope pseudo-step (DESIGN §13): the worst
 *  case across all of a kind's load steps. Not a real load step — it has no
 *  stashed solution; its field is reduced client-side from the steps. */
export const ENVELOPE_STEP = "__envelope__";

export function isEnvelope(e: { loadStepId: string }): boolean {
  return e.loadStepId === ENVELOPE_STEP;
}

/** The slice of a retained result the field pipeline needs (structural
 *  subset of the store's `ResultEntry`). */
export interface FieldResultRef {
  id: string;
  kind: string;
  loadStepId: string;
}

/** Snapshot of the display state the pipeline dispatches on. Re-read (live)
 *  after every await for the staleness checks. */
export interface FieldDisplayState {
  activeResultId: string | null;
  resultField: string;
  resultSurface: "stl" | "voxel";
  results: readonly FieldResultRef[];
}

/** Narrow apply surface the store hands in: state writes (`fieldRange`) and
 *  scene pushes stay the store's job; the server only decides WHAT to apply
 *  and WHEN a fetch is stale. */
export interface FieldSink {
  /** `set({ fieldRange })` — legend min/max of the active field. */
  setFieldRange(range: { min: number; max: number }): void;
  /** `sceneEvents.onScalarField` — contour values (null = |u| colors);
   *  flip inverts the colormap (safety factor: red = critical LOW);
   *  signed centers the color scale on 0 (signed von Mises: ±tension). */
  scalarField(values: Float32Array | null, flip?: boolean, signed?: boolean): void;
  /** `sceneEvents.onPeelMap` — bed-peel heatmap on the plate (nulls clear). */
  peelMap(positions: Float32Array | null, values: Float32Array | null, max: number): void;
  /** `sceneEvents.onDispComponent` — -1 = |u|, 0/1/2 = signed X/Y/Z. */
  dispComponent(comp: number): void;
}

/** Displacement component index for a field, or null for an engine field. */
function dispCompOf(field: string): number | null {
  return field === "u" ? -1 : field === "ux" ? 0 : field === "uy" ? 1 : field === "uz" ? 2 : null;
}

// ---- staleness discipline ----

/** Validity predicate of one in-flight fetch: built from the display state
 *  the fetch was issued FOR, evaluated against the live state after each
 *  await. A fetch whose predicate fails is dropped without touching the
 *  sink. */
type StillValid = (s: FieldDisplayState) => boolean;

/** Plain field/peel fetch: only a field switch invalidates it ("user moved
 *  on mid-fetch"). Deliberately survives result/surface switches — those
 *  re-push the field themselves. */
const sameField =
  (field: string): StillValid =>
  (s) =>
    s.resultField === field;

/** Envelope reduction: the active result must still be THIS kind's envelope
 *  and the field unchanged (the user switching result or field mid-reduction
 *  bails). */
const sameEnvelope =
  (kind: string, field: string): StillValid =>
  (s) => {
    const active = s.results.find((r) => r.id === s.activeResultId);
    return !!active && isEnvelope(active) && active.kind === kind && s.resultField === field;
  };

// ---- envelope reduction strategy ----

/** Multi-step aggregate strategy: fold one load step's per-vertex field into
 *  the running aggregate, in place, over the common prefix. A future
 *  aggregate (mean, range, …) is one new reducer. */
export type EnvelopeReducer = (acc: Float32Array, vals: Float32Array) => void;

/** Worst case per point: MIN for safety factors ("does it survive any
 *  load"), MAX otherwise. */
export function worstCaseReducer(isSf: boolean): EnvelopeReducer {
  return (acc, vals) => {
    const n = Math.min(acc.length, vals.length);
    for (let i = 0; i < n; i++) acc[i] = isSf ? Math.min(acc[i], vals[i]) : Math.max(acc[i], vals[i]);
  };
}

export class FieldServer {
  /** Reduced envelope fields, keyed `${kind}::${field}`. Cleared whenever the
   *  result set is rebuilt (new solve / grid drop) — the stashes it reduces
   *  over would no longer match. */
  private envelopeFields = new Map<string, Float32Array>();

  /** `session` carries the per-solution engine-field caches (STL + voxel
   *  surfaces) the plain-field path reads through. */
  constructor(private session: EngineSession) {}

  clearEnvelopeCache() {
    this.envelopeFields.clear();
  }

  /** Push the active result field, sized for the active result surface
   *  ("u" = displacement coloring straight from the displacement arrays).
   *  `read` returns the LIVE display state (called again after every await
   *  for the staleness predicates); results land through `sink`. */
  async pushActiveField(read: () => FieldDisplayState, sink: FieldSink): Promise<void> {
    const s = read();
    const active = s.results.find((r) => r.id === s.activeResultId);
    if (active && isEnvelope(active)) {
      await this.pushEnvelopeField(read, sink, active.kind, s.resultField);
      return;
    }
    const kind = s.resultField;
    // Displacement fields are colored client-side from the displacement buffer:
    // |u| magnitude (-1) or a signed X/Y/Z component (0/1/2). No engine fetch.
    // Build-sim bed-peel: shown as a flat heatmap lying ON the plate (visible
    // from above), NOT painted on the part — so leave the part in its plain
    // deformed shade and drop any stress coloring. Anchored at 0, N, uncalibrated.
    if (kind === "peel" || kind === "peelshear") {
      sink.scalarField(null);
      const { positions, values } = await engine.peelMap(kind as "peel" | "peelshear");
      if (!sameField(kind)(read())) return;
      let max = 0;
      for (let i = 0; i < values.length; i++) if (values[i] > max) max = values[i];
      sink.setFieldRange({ min: 0, max });
      sink.peelMap(positions, values, max);
      return;
    }
    // Any non-peel field: make sure a previous bed-peel heatmap is gone.
    sink.peelMap(null, null, 0);
    const dispComp = dispCompOf(kind);
    if (dispComp !== null) {
      sink.scalarField(null);
      // Both |u| (anchored [0, max]) and the signed components report their auto
      // range back from the scene via onResultRange → fieldRange, so the legend
      // follows the ACTIVE result instead of a stale solve stat. Don't null it
      // here — the scene repopulates it synchronously as it colors.
      sink.dispComponent(dispComp);
      return;
    }
    const vox = s.resultSurface === "voxel";
    let values = this.session.fieldOf(kind, vox);
    if (!values) {
      values = vox ? await engine.voxelResultField(kind) : await engine.resultField(kind);
      this.session.setField(kind, vox, values);
    }
    if (!sameField(kind)(read())) return; // user moved on mid-fetch
    let min = Infinity;
    let max = -Infinity;
    for (let i = 0; i < values.length; i++) {
      min = Math.min(min, values[i]);
      max = Math.max(max, values[i]);
    }
    // Signed von Mises is a diverging field: center the scale on 0 so red =
    // tension, blue = compression, green ≈ unloaded (and the legend reads ±M).
    const signed = kind === "svm";
    if (signed) {
      const m = Math.max(Math.abs(min), Math.abs(max), 1e-12);
      min = -m;
      max = m;
    }
    sink.setFieldRange({ min, max });
    // Safety factor: invert the colormap so red marks the critical LOW.
    sink.scalarField(values, kind.startsWith("sf"), signed);
  }

  /** Push the envelope's worst-case `field` as a scalar contour on the
   *  undeformed part (it has no single displacement). Reduces the kind's
   *  steps client-side; every field — including |u| — goes through the
   *  scalar-field path. */
  private async pushEnvelopeField(
    read: () => FieldDisplayState,
    sink: FieldSink,
    kind: string,
    field: string
  ): Promise<void> {
    const values = await this.computeEnvelopeField(read, kind, field);
    // Bail if the user moved on (switched result or field) during the reduction.
    if (!sameEnvelope(kind, field)(read())) return;
    if (!values) {
      sink.scalarField(null);
      return;
    }
    let min = Infinity;
    let max = -Infinity;
    for (let i = 0; i < values.length; i++) {
      if (values[i] < min) min = values[i];
      if (values[i] > max) max = values[i];
    }
    const signed = field === "svm";
    if (signed) {
      const m = Math.max(Math.abs(min), Math.abs(max), 1e-12);
      min = -m;
      max = m;
    } else if (field === "u") {
      min = 0; // |u| anchors at zero like the per-step view
    }
    sink.setFieldRange({ min, max });
    sink.scalarField(values, field.startsWith("sf"), signed);
  }

  /** Worst case of `field` across every real load step of `kind`, per surface
   *  vertex (via `worstCaseReducer`). Activates each step's stash in turn and
   *  reduces client-side; cached. The fields are NOT written to the shared
   *  field cache (they'd shadow a real step's). */
  private async computeEnvelopeField(
    read: () => FieldDisplayState,
    kind: string,
    field: string
  ): Promise<Float32Array | null> {
    const key = `${kind}::${field}`;
    const hit = this.envelopeFields.get(key);
    if (hit) return hit;
    const steps = read().results.filter((r) => r.kind === kind && !isEnvelope(r));
    if (!steps.length) return null;
    const comp = dispCompOf(field);
    const reduce = worstCaseReducer(field.startsWith("sf"));
    let acc: Float32Array | null = null;
    for (const step of steps) {
      const disp = await engine.activateResult(step.id);
      let vals: Float32Array;
      if (comp !== null) {
        const n = disp.length / 3;
        vals = new Float32Array(n);
        for (let i = 0; i < n; i++) {
          vals[i] = comp < 0 ? Math.hypot(disp[3 * i], disp[3 * i + 1], disp[3 * i + 2]) : disp[3 * i + comp];
        }
      } else {
        vals = (await engine.resultField(field)).slice();
      }
      if (!acc) {
        acc = comp !== null ? vals : vals.slice();
      } else {
        reduce(acc, vals);
      }
    }
    if (acc) this.envelopeFields.set(key, acc);
    return acc;
  }
}
