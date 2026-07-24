// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

/// <reference lib="webworker" />
// meshStep STEP tessellation (DESIGN §18). Runs in its OWN worker, apart from
// the engine: `importStep` is synchronous and CPU-bound, so this thread blocks
// for the whole conversion — progress posts flow OUT fine, but inbound
// messages only queue until the run ends. Cancellation is therefore worker
// TERMINATION (StepImporter recreates the worker for the next import).

import {
  autoTessellation,
  estimateStepSize,
  importStep,
  VERSION,
  type ImportProgress,
} from "meshstep";
import type {
  StepImportRequest,
  StepImportWorkerMessage,
  StepMeshPayload,
} from "../engine/StepImporter";
import type { StepFaceInfo } from "../engine/stepSelection";

/** Densify sparse STEP entity record numbers into 0..n-1 indices. The engine
 *  sizes patch arrays as max-id+1 (`cad_segmentation`), so raw entity numbers
 *  (arbitrarily large) must never cross the wasm boundary. The dense→entity
 *  table goes back to the main thread — it is the persistence identity
 *  (DESIGN §18 dec. 5): entity ids survive meshStep upgrades, indices don't. */
function densify(ids: Uint32Array): { dense: Uint32Array; table: Uint32Array } {
  const dense = new Uint32Array(ids.length);
  const map = new Map<number, number>();
  const table: number[] = [];
  for (let i = 0; i < ids.length; i++) {
    let d = map.get(ids[i]);
    if (d === undefined) {
      d = table.length;
      map.set(ids[i], d);
      table.push(ids[i]);
    }
    dense[i] = d;
  }
  return { dense, table: Uint32Array.from(table) };
}

self.onmessage = async (ev: MessageEvent<StepImportRequest>) => {
  const { id, bytes } = ev.data;
  const post = (msg: StepImportWorkerMessage, transfer: Transferable[] = []) =>
    (self as unknown as Worker).postMessage(msg, transfer);
  try {
    // Identity hash of the ORIGINAL bytes (before any decoding): entity ids
    // are stable per file, so this is the key face-id selections bind to
    // (DESIGN §18 dec. 5c).
    const digest = await crypto.subtle.digest("SHA-256", bytes);
    const sha256 = [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
    // STEP is ASCII (ISO-10303-21); latin1 maps bytes 1:1 and never throws,
    // so stray 8-bit characters in names/comments can't corrupt the parse.
    const text = new TextDecoder("iso-8859-1").decode(bytes);
    // Tessellation options derive from the FILE ALONE (size-adaptive
    // defaults), never from session state: project open re-runs this import
    // and must reproduce the identical mesh so saved triangle-index
    // selections stay valid (same input + same opts + pinned meshStep
    // version = bit-identical output). The auto maxEdge (~diag/100) tracks
    // the default-resolution voxel pitch closely enough that no extra
    // pitch-coupled clamp is warranted — coupling one in would break this
    // determinism for anyone who changes the resolution preset.
    // Project reopen passes the SAVED opts instead, in case a later meshStep
    // release re-anchors the auto derivation.
    let opts = ev.data.opts;
    if (!opts) {
      const est = estimateStepSize(text);
      const auto = autoTessellation(est ? est.diag : 100);
      opts = {
        surfaceDeviation: auto.surfaceDeviation,
        normalDeviation: 15,
        maxEdge: auto.maxEdge,
      };
    }
    // Throttle progress posts: once per phase change or ≥1% advance.
    let lastPhase = "";
    let lastFrac = -1;
    const onProgress = (p: ImportProgress) => {
      const frac = p.total > 0 ? p.done / p.total : 0;
      if (p.phase !== lastPhase || frac - lastFrac >= 0.01) {
        lastPhase = p.phase;
        lastFrac = frac;
        post({ id, progress: true, phase: p.phase, done: p.done, total: p.total });
      }
    };
    const r = importStep(text, { ...opts, onProgress });

    const face = densify(r.faceOfTri);
    const solid = densify(r.solidOfTri);
    // openSolids carries solid ENTITY ids — remap onto the dense indices.
    const openSet = new Set(r.openSolids);
    const openSolids: number[] = [];
    solid.table.forEach((entity, denseId) => {
      if (openSet.has(entity)) openSolids.push(denseId);
    });
    // Per-face metadata for analytic BCs (DESIGN §18 M3), dense-indexed.
    // Analytic identity is PART-LOCAL; only when every instance is meshed in
    // place (frame null — single parts, in-place assemblies) does it equal
    // world space, so gate the whole block on that.
    const inPlace = r.instances.every((i) => !i.frame);
    let faces: StepFaceInfo[] | null = null;
    if (inPlace) {
      faces = Array.from(face.table, (entity) => {
        const f = r.faces.get(entity);
        if (!f) {
          // Repair-fill ids always adopt a real face; a miss should not
          // happen, but a placeholder beats dropping the whole payload.
          return { entityId: entity, type: "other", area: 0, meanNormal: [0, 0, 1] as [number, number, number] };
        }
        const s = f.surface;
        return {
          entityId: entity,
          type: f.type,
          area: f.area,
          meanNormal: [f.meanNormal[0], f.meanNormal[1], f.meanNormal[2]] as [number, number, number],
          origin: s.origin ? ([s.origin[0], s.origin[1], s.origin[2]] as [number, number, number]) : undefined,
          axis: s.axis ? ([s.axis[0], s.axis[1], s.axis[2]] as [number, number, number]) : undefined,
          radius: s.radius,
          semiAngle: s.semiAngle,
        };
      });
    }
    // CAD presentation colors (DESIGN §18 M4): remap the per-entity palette
    // indices onto the dense face ids. Face-level entries already include
    // composed body colors (meshStep contract), so one lookup suffices.
    let palette: [number, number, number][] | null = null;
    let faceColorIdx: Int32Array | null = null;
    if (r.colors) {
      palette = r.colors.palette.map((c) => [c[0], c[1], c[2]] as [number, number, number]);
      faceColorIdx = new Int32Array(face.table.length).fill(-1);
      face.table.forEach((entity, d) => {
        faceColorIdx![d] = r.colors!.faceColor.get(entity) ?? -1;
      });
    }
    const payload: StepMeshPayload = {
      positions: new Float32Array(r.mesh.positions), // f64 mm → f32 for GPU/wasm
      indices: r.mesh.indices,
      faceOfTri: face.dense,
      solidOfTri: solid.dense,
      faceEntityIds: face.table,
      solidEntityIds: solid.table,
      openSolids,
      faces,
      palette,
      faceColorIdx,
      diagnostics: r.diagnostics,
      stats: r.stats,
      units: r.units,
      meshstepVersion: VERSION,
      opts,
      sha256,
    };
    post({ id, ok: true, payload }, [
      payload.positions.buffer,
      payload.indices.buffer,
      payload.faceOfTri.buffer,
      payload.solidOfTri.buffer,
      payload.faceEntityIds.buffer,
      payload.solidEntityIds.buffer,
    ]);
  } catch (e) {
    post({ id, ok: false, error: e instanceof Error ? e.message : String(e) });
  }
};
