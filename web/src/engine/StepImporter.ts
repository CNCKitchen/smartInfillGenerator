// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Main-thread wrapper around the meshStep import worker (DESIGN §18).
//! One import at a time (the app loads one model at a time); `cancel()`
//! TERMINATES the worker — the conversion is synchronous inside it, so a
//! posted abort message could never be observed mid-run. The next import
//! spawns a fresh worker.

import type { ImportDiagnostics, ImportProgress, MeshResult } from "meshstep";
import type { StepFaceInfo } from "./stepSelection";

/** Tessellation options — file-derived on fresh imports; passed back in
 *  verbatim on project reopen so the mesh reproduces even if the auto
 *  derivation changes in a later meshStep release (DESIGN §18 M2). */
export interface StepTessOpts {
  surfaceDeviation: number;
  normalDeviation: number;
  maxEdge: number;
}

/** Main thread → worker: one STEP file to convert. `opts` overrides the
 *  file-derived tessellation defaults (project reopen). */
export interface StepImportRequest {
  id: number;
  bytes: ArrayBuffer;
  opts?: StepTessOpts;
}

/** The converted mesh, engine- and persistence-ready. `faceOfTri`/
 *  `solidOfTri` are DENSE indices (what the wasm boundary needs);
 *  `faceEntityIds`/`solidEntityIds` map them back to the STEP file's own
 *  entity record numbers — the version-stable selection identity that M2
 *  persists (DESIGN §18 dec. 5). */
export interface StepMeshPayload {
  positions: Float32Array;
  indices: Uint32Array;
  faceOfTri: Uint32Array;
  solidOfTri: Uint32Array;
  faceEntityIds: Uint32Array;
  solidEntityIds: Uint32Array;
  /** Dense solid indices of open-by-design (OPEN_SHELL) bodies. Any entry ⇒
   *  the winding-number voxelization cannot be trusted — hard warning. */
  openSolids: number[];
  /** Per-face metadata (surface class, analytic identity, area, normal),
   *  dense-indexed like `faceEntityIds`; null when assembly instances carry
   *  placements (part-local geometry would be ambiguous in world space) —
   *  DESIGN §18 M3. */
  faces: StepFaceInfo[] | null;
  diagnostics: ImportDiagnostics;
  stats: MeshResult["stats"];
  /** Display-only unit label from the file; coordinates are always mm. */
  units: string;
  /** Runtime meshstep VERSION (not the lockfile's) — recorded with the mesh;
   *  a version change invalidates cached tessellations (M2). */
  meshstepVersion: string;
  /** The tessellation options the mesh was built with (file-derived on fresh
   *  imports, or the caller's override) — persisted so re-imports reproduce
   *  the mesh. */
  opts: StepTessOpts;
  /** SHA-256 (hex) of the original STEP bytes. Entity ids are stable per
   *  FILE — a CAD re-export renumbers them, so a changed hash means face-id
   *  selections must be re-bound by the user, not trusted (DESIGN §18). */
  sha256: string;
}

export type StepImportWorkerMessage =
  | { id: number; progress: true; phase: ImportProgress["phase"]; done: number; total: number }
  | { id: number; ok: true; payload: StepMeshPayload }
  | { id: number; ok: false; error: string };

export type StepProgressHandler = (
  phase: ImportProgress["phase"],
  done: number,
  total: number
) => void;

class StepImporter {
  private worker: Worker | null = null;
  private nextId = 1;
  private active: { id: number; reject: (e: Error) => void } | null = null;

  /** Convert a STEP file to a mesh payload. A still-running previous import
   *  is cancelled first. Rejects with "cancelled" on `cancel()`. `opts`
   *  overrides the file-derived tessellation defaults (project reopen). */
  import(
    bytes: ArrayBuffer,
    onProgress?: StepProgressHandler,
    opts?: StepTessOpts
  ): Promise<StepMeshPayload> {
    if (this.active) this.cancel();
    const id = this.nextId++;
    if (!this.worker) {
      this.worker = new Worker(new URL("../worker/import.worker.ts", import.meta.url), {
        type: "module",
      });
    }
    const worker = this.worker;
    return new Promise<StepMeshPayload>((resolve, reject) => {
      this.active = { id, reject };
      worker.onmessage = (ev: MessageEvent<StepImportWorkerMessage>) => {
        const msg = ev.data;
        if (msg.id !== id) return; // stale post from a cancelled run
        if ("progress" in msg) {
          onProgress?.(msg.phase, msg.done, msg.total);
          return;
        }
        this.active = null;
        if (msg.ok) resolve(msg.payload);
        else reject(new Error(msg.error));
      };
      worker.onerror = (ev) => {
        this.active = null;
        reject(new Error(ev.message || "STEP import worker crashed"));
      };
      worker.postMessage({ id, bytes, opts } satisfies StepImportRequest, [bytes]);
    });
  }

  /** Hard-stop the running conversion (worker termination — see module doc).
   *  The pending promise rejects with "cancelled". */
  cancel(): void {
    if (!this.active) return;
    this.worker?.terminate();
    this.worker = null;
    const { reject } = this.active;
    this.active = null;
    reject(new Error("cancelled"));
  }

  /** True while a conversion is running. */
  get busy(): boolean {
    return this.active !== null;
  }
}

export const stepImporter = new StepImporter();

/** STEP sniffing shared by the load paths: extension, or the ISO-10303-21
 *  header for extension-less byte blobs (embedded project models). */
export function isStepName(name: string): boolean {
  return /\.(step|stp)$/i.test(name);
}

export function looksLikeStep(bytes: ArrayBuffer): boolean {
  const head = new Uint8Array(bytes, 0, Math.min(256, bytes.byteLength));
  const text = new TextDecoder("iso-8859-1").decode(head);
  return text.includes("ISO-10303-21");
}

/** Import-health notices for the UI (DESIGN §18 dec. 7), worst first. Empty =
 *  clean conversion. Open bodies are the HARD warning: the winding-number
 *  inside-test needs closed solids, so voxelization of open geometry is
 *  untrustworthy — not just cosmetic. */
export function stepImportNotices(p: StepMeshPayload): string[] {
  const d = p.diagnostics;
  const out: string[] = [];
  const missing = d.facesDropped + d.facesSkipped;
  if (missing > 0) {
    out.push(
      `${missing} CAD face${missing > 1 ? "s" : ""} could not be converted — that geometry is missing from the model. If the part looks wrong, export an STL from CAD instead.`
    );
  }
  if (p.openSolids.length > 0) {
    out.push(
      "The file contains open (sheet) bodies. filaSim needs closed solids — open geometry can voxelize wrong. Check the part carefully, or export closed solids from CAD."
    );
  } else if (d.openEdges > 0) {
    out.push(
      `The converted mesh has ${d.openEdges} open edge${d.openEdges > 1 ? "s" : ""} (cracks/holes) — the simulation may leak through them. Inspect the part, or export an STL from CAD.`
    );
  }
  if (!d.ok && out.length === 0) {
    out.push(
      "Some CAD faces were reconstructed heuristically during conversion — check that the shape looks right."
    );
  }
  return out;
}
