// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Single source of truth for the main-thread ⇄ engine-worker protocol.
//! `EngineRequests` maps each op name to the payload that accompanies
//! `{ id, op }` on the wire; `EngineResponses` maps it to the `data` of the
//! ok reply. The worker types its incoming message as `EngineWorkerRequest`
//! (with an exhaustiveness check on the switch) and its replies against the
//! response map; `EngineClient.call` is generic over both. Adding an op
//! therefore means: one entry in EACH map (a missing response entry is a
//! compile error at the typed reply helpers), one switch case in the worker
//! (the `never` default catches a forgotten one), and one one-line method on
//! EngineClient.

import type { CheckReport, CylFit, LoadedModel, SolveStats, VoxelInfo } from "../types";
import type {
  BuildSimOptions,
  BuildSimStats,
  ModalAnalysisResult,
  ModalOptions,
  ModalProgress,
  OptimizeOptions,
  OptimizeOutput,
  OptProgress,
  OptRegion,
  PrintedOptions,
  PrintedStats,
  SlicerFlavor,
} from "./EngineClient";

/** Op with no payload beyond `{ id, op }`. */
export type Empty = Record<never, never>;

/** Wire form of one boundary condition (`setBcs`): a `Bc` with the UI-only
 *  fields dropped and the tris copied so the originals stay with the UI. */
export interface BcPayload {
  kind: string;
  tris: Uint32Array;
  force?: number[];
  pressure?: number;
  stiffness?: number;
  axes?: boolean[];
  disp?: number[];
  moment?: number[];
}

/** Request payloads — the fields that travel WITH `{ id, op }`. */
export interface EngineRequests {
  load: { bytes: ArrayBuffer; name: string };
  /** Rigid transform: `matrix` = [r00..r22 row-major, tx, ty, tz] in mm. */
  transform: { matrix: number[] };
  resegment: { angle: number };
  useCadFaces: Empty;
  setMaterial: { e0: number; nu: number; density: number; strength: number; strengthZ: number };
  setGravity: { on: boolean };
  setResolution: { cells: number };
  setVoxelSize: { h: number };
  setBcs: { bcs: BcPayload[] };
  fitCylinder: { tris: Uint32Array };
  voxelInfo: Empty;
  voxelMesh: Empty;
  voxelMeshCut: {
    plane: { normal: [number, number, number]; constant: number } | null;
    wall: number;
    /** Top/bottom shell thickness in mm (layers × layer height). */
    topBottomMm: number;
    /** Uniform infill % for interior-cell density (optimized densities
     *  win when an optimization result exists). */
    infillPct: number;
  };
  check: Empty;
  solve: Empty;
  solveOptimized: Empty;
  setSnapWall: { wall: number };
  setCompositeSkin: { on: boolean };
  setSmoothStress: { on: boolean };
  setMaterialStress: { on: boolean };
  setCancelBuffer: { buf: SharedArrayBuffer };
  setProgressBuffer: { buf: SharedArrayBuffer };
  /** PrintedOpts object — serialized to JSON for the wasm API. */
  solvePrinted: { opts: PrintedOptions };
  /** BuildSimOpts object — serialized to JSON for the wasm API. */
  buildSim: { opts: BuildSimOptions };
  /** ModalOpts object — serialized to JSON for the wasm API. */
  modalAnalysis: { opts: ModalOptions };
  /** "bonded" (on bed) | "released" (off bed). */
  setBuildState: { state: "released" | "bonded" };
  /** OptimizeOptions object — serialized to JSON for the wasm API. */
  optimize: { opts: OptimizeOptions };
  densityShape: { threshold: number };
  resmooth: { iters: number };
  setIsoThreshold: { threshold: number; smoothIters: number };
  resultField: { kind: string };
  peelField: { kind: "peel" | "peelshear" };
  peelMap: { kind: "peel" | "peelshear" };
  inherentStrainVoxels: { layerMax: number; shrinkXy: number; shrinkZ: number };
  voxelResults: Empty;
  voxelResultField: { kind: string };
  stashResult: { resultId: string };
  activateResult: { resultId: string };
  clearResults: Empty;
  clearLoadCases: Empty;
  addLoadCase: { weight: number };
  transformMatrix: Empty;
  exportProject: { manifest: string; modelEntry: string; includeResults: boolean };
  openProjectModel: { bytes: ArrayBuffer };
  openProjectRestore: Empty;
  vertexDensity: Empty;
  exportThreeMf: { slicer: SlicerFlavor; thumbnail: Uint8Array | null };
  exportColorThreeMf: {
    kind: string;
    lo: number;
    hi: number;
    steps: number;
    colors: string[];
    thumbnail: Uint8Array | null;
  };
  exportStls: Empty;
  exportSolidStl: Empty;
}

export type Op = keyof EngineRequests;

/** Loaded-model payload as the worker actually builds it: `LoadedModel`
 *  plus the mesh-object count (UI warns when > 1). */
export type LoadedModelData = LoadedModel & { meshObjects: number };

/** Patch segmentation update (resegment / useCadFaces). */
export interface PatchUpdate {
  patchIds: Uint32Array;
  patchCount: number;
}

/** solve-family reply: stats + per-soup-vertex displacements. */
export interface SolveResult<S = SolveStats> {
  stats: S;
  displacements: Float32Array;
}

/** Response `data` per op (`void` = the generic `{ id, ok: true }` ack). */
export interface EngineResponses {
  load: LoadedModelData;
  transform: { positions: Float32Array; bbox: number[] };
  resegment: PatchUpdate;
  useCadFaces: PatchUpdate;
  setMaterial: void;
  setGravity: void;
  setResolution: void;
  setVoxelSize: void;
  setBcs: void;
  fitCylinder: CylFit;
  voxelInfo: VoxelInfo;
  voxelMesh: { hull: Float32Array; edges: Float32Array; info: VoxelInfo };
  voxelMeshCut: { hull: Float32Array; density: Float32Array; edges: Float32Array; info: VoxelInfo };
  check: CheckReport;
  solve: SolveResult;
  solveOptimized: SolveResult;
  setSnapWall: void;
  setCompositeSkin: void;
  setSmoothStress: void;
  setMaterialStress: void;
  setCancelBuffer: void;
  setProgressBuffer: void;
  solvePrinted: SolveResult<PrintedStats>;
  buildSim: SolveResult<BuildSimStats>;
  modalAnalysis: { result: ModalAnalysisResult; displacements: Float32Array };
  setBuildState: SolveResult<{ maxDisplacement: number }>;
  optimize: OptimizeOutput;
  densityShape: { positions: Float32Array; indices: Uint32Array; density: Float32Array };
  resmooth: { regions: OptRegion[] };
  setIsoThreshold: { regions: OptRegion[] };
  resultField: Float32Array;
  peelField: Float32Array;
  peelMap: { positions: Float32Array; values: Float32Array };
  inherentStrainVoxels: {
    hull: Float32Array;
    values: Float32Array;
    edges: Float32Array;
    max: number;
    nz: number;
  };
  voxelResults: {
    positions: Float32Array;
    displacements: Float32Array;
    edges: Float32Array;
    edgeDisplacements: Float32Array;
  };
  voxelResultField: Float32Array;
  stashResult: void;
  activateResult: Float32Array;
  clearResults: void;
  clearLoadCases: void;
  addLoadCase: void;
  transformMatrix: number[];
  exportProject: Uint8Array;
  openProjectModel: { manifest: string; model: LoadedModelData };
  openProjectRestore: { restoredResults: string[]; hasDesign: boolean };
  vertexDensity: Float32Array;
  exportThreeMf: Uint8Array;
  exportColorThreeMf: Uint8Array;
  exportStls: Uint8Array;
  exportSolidStl: Uint8Array;
}

// ---- wire envelopes ----

/** Main thread → worker. */
export type WorkerRequest<O extends Op> = { id: number; op: O } & EngineRequests[O];

/** Union of every request — the worker's incoming message type. */
export type EngineWorkerRequest = { [O in Op]: WorkerRequest<O> }[Op];

/** Success reply; `data` is absent for void ops (the generic ack). */
export interface WorkerOkMessage {
  id: number;
  ok: true;
  data?: EngineResponses[Op];
}

/** Failure reply; `error` is the thrown message (matched for "cancelled"). */
export interface WorkerErrorMessage {
  id: number;
  ok: false;
  error: string;
}

// ---- progress pushes (worker → main thread, outside request/response) ----

/** Per-layer build-sim progress payload. */
export interface BuildSimProgress {
  done: number;
  total: number;
  maxU: number;
}

/** optimize: per-iteration stats + evolving density/skeleton buffers. */
export interface OptimizeProgressMessage {
  id: number;
  progress: true;
  data: OptProgress;
  density: Float32Array;
  skelPositions: Float32Array;
  skelIndices: Uint32Array;
  skelDensity: Float32Array;
}

/** buildSim: per-layer progress + (on throttled frames) the deformed
 *  activated voxel hull (positions in `density`, normalised |u| in
 *  `skelPositions` — the field names are the shared wire slots). */
export interface BuildSimProgressMessage {
  id: number;
  progress: true;
  data: BuildSimProgress;
  density: Float32Array;
  skelPositions: Float32Array;
}

/** modalAnalysis: per-outer-iteration progress + current frequencies. */
export interface ModalProgressMessage {
  id: number;
  progress: true;
  data: ModalProgress;
}

/** What the client's onmessage sees: any progress push. Every specific
 *  progress message above is assignable to this loose envelope. */
export interface WorkerProgressMessage {
  id: number;
  progress: true;
  data: OptProgress | BuildSimProgress | ModalProgress;
  density?: Float32Array;
  skelPositions?: Float32Array;
  skelIndices?: Uint32Array;
  skelDensity?: Float32Array;
}

/** Worker → main thread: every message the client can receive. */
export type EngineWorkerMessage = WorkerOkMessage | WorkerErrorMessage | WorkerProgressMessage;

/** Rejection type for engine calls. `message` is the worker's error string
 *  verbatim (store code keeps matching on it); `code` classifies it —
 *  "cancelled" when the wasm solve bailed on the user's stop request (the
 *  Rust loops throw a message containing "cancelled", matched exactly like
 *  the store's `/cancelled/i` tests), "internal" for everything else. */
export class EngineError extends Error {
  readonly code: "cancelled" | "internal";

  constructor(message: string) {
    super(message);
    this.code = /cancelled/i.test(message) ? "cancelled" : "internal";
  }
}
