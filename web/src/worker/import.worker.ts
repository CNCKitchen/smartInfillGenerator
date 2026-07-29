// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

/// <reference lib="webworker" />
// meshStep STEP tessellation (DESIGN §18). Runs in its OWN worker, apart from
// the engine: the conversion is synchronous and CPU-bound, so this thread
// blocks for the whole run — progress posts flow OUT fine, but inbound
// messages only queue until the run ends. Cancellation is therefore worker
// TERMINATION (StepImporter recreates the worker for the next import).
//
// A request may carry a PRE-TESSELLATED cache blob (the bundled sample model).
// The worker — not the caller — validates it against the actual bytes: same
// sha-256, same meshStep VERSION, same opts ⇒ the cache IS what a live import
// would produce (conversion is deterministic), so tessellation is skipped.
// Any mismatch or decode error falls through to the normal conversion.

import { VERSION, type ImportProgress } from "meshstep";
import { convertStepBytes, stepSha256 } from "../engine/stepConvert.ts";
import { decodeStepMesh } from "../engine/stepMeshCache.ts";
import type { StepImportRequest, StepImportWorkerMessage } from "../engine/StepImporter";

self.onmessage = async (ev: MessageEvent<StepImportRequest>) => {
  const { id, bytes, opts, cached } = ev.data;
  const post = (msg: StepImportWorkerMessage, transfer: Transferable[] = []) =>
    (self as unknown as Worker).postMessage(msg, transfer);
  try {
    if (cached) {
      try {
        const payload = decodeStepMesh(cached);
        const sha256 = await stepSha256(bytes);
        const optsOk =
          !opts ||
          (opts.surfaceDeviation === payload.opts.surfaceDeviation &&
            opts.normalDeviation === payload.opts.normalDeviation &&
            opts.maxEdge === payload.opts.maxEdge);
        if (payload.sha256 === sha256 && payload.meshstepVersion === VERSION && optsOk) {
          payload.fromCache = true;
          // decodeStepMesh copied every array out, so the transfer list is
          // the same as the live path's (loadMesh transfers buffers one by
          // one — aliasing them to one container would break that).
          post({ id, ok: true, payload }, [
            payload.positions.buffer,
            payload.indices.buffer,
            payload.faceOfTri.buffer,
            payload.solidOfTri.buffer,
            payload.faceEntityIds.buffer,
            payload.solidEntityIds.buffer,
            payload.featureEdges.buffer,
            payload.featureEdgeFaces.buffer,
          ]);
          return;
        }
      } catch {
        // malformed cache — treat as absent
      }
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
    const payload = await convertStepBytes(bytes, opts, onProgress);
    post({ id, ok: true, payload }, [
      payload.positions.buffer,
      payload.indices.buffer,
      payload.faceOfTri.buffer,
      payload.solidOfTri.buffer,
      payload.faceEntityIds.buffer,
      payload.solidEntityIds.buffer,
      payload.featureEdges.buffer,
      payload.featureEdgeFaces.buffer,
    ]);
  } catch (e) {
    post({ id, ok: false, error: e instanceof Error ? e.message : String(e) });
  }
};
