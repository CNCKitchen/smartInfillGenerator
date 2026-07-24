// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! CAD-face selection identity (DESIGN §18 M2). Selections live in the store
//! as WORKING-mesh triangle indices, which are only valid for one exact
//! tessellation (bit-identical within a meshStep version). STEP entity record
//! numbers are the durable identity — stable across meshStep versions for the
//! same file bytes — so projects persist a selection as its CAD-face entity-id
//! set whenever it is exactly a union of faces, and re-derive triangles from
//! those ids when the tessellation changed. Pure functions; unit-testable.

/** The step block persisted in a project manifest (additive, schema stays 1). */
export interface StepManifestInfo {
  /** Runtime meshStep VERSION the embedded model was tessellated with. */
  meshstepVersion: string;
  /** Tessellation options — replayed verbatim on reopen. */
  opts: { surfaceDeviation: number; normalDeviation: number; maxEdge: number };
  /** SHA-256 (hex) of the embedded STEP bytes (entity ids are per-file). */
  sha256: string;
}

/** Per-face metadata from meshStep, dense-indexed like `faceEntityIds`
 *  (DESIGN §18 M3). Analytic fields (`origin`/`axis`/`radius`/`semiAngle`)
 *  and `meanNormal` are in the IMPORT frame — compose with the model's
 *  `toWorld` transform before use. Absent entirely (payload `faces: null`)
 *  when assembly instances carry placements, because part-local geometry is
 *  then ambiguous in world space. */
export interface StepFaceInfo {
  entityId: number;
  /** Normalized surface class ("plane" | "cylinder" | "cone" | …). */
  type: string;
  /** Face area, mm² (tessellation area of the face). */
  area: number;
  /** Unit mean outward normal (import frame). */
  meanNormal: [number, number, number];
  origin?: [number, number, number];
  axis?: [number, number, number];
  radius?: number;
  semiAngle?: number;
}

/** If `tris` is EXACTLY a union of whole CAD faces, return their DENSE face
 *  ids; otherwise null. The building block for every whole-face question
 *  (persist ids, exact cylinder, CAD area). */
export function wholeFaceDenseIds(tris: Uint32Array, cadPatchIds: Uint32Array): number[] | null {
  if (tris.length === 0) return null;
  const selected = new Map<number, number>(); // dense face id → selected-tri count
  for (const t of tris) {
    if (t >= cadPatchIds.length) return null; // stale index — never fabricate ids
    const f = cadPatchIds[t];
    selected.set(f, (selected.get(f) ?? 0) + 1);
  }
  // Whole-face test: every touched face must be fully covered.
  const total = new Map<number, number>(); // face id → triangle count on the mesh
  for (const f of cadPatchIds) if (selected.has(f)) total.set(f, (total.get(f) ?? 0) + 1);
  for (const [f, n] of selected) if (total.get(f) !== n) return null;
  return [...selected.keys()];
}

/** If `tris` is EXACTLY a union of whole CAD faces, return those faces' STEP
 *  entity record numbers (sorted, deduped); otherwise null (brush/partial
 *  selections keep the triangle-index fallback). `cadPatchIds` = dense CAD
 *  face id per working-mesh triangle (the load-time CAD segmentation);
 *  `faceEntityIds` maps dense id → entity record number. */
export function selectionFaceIds(
  tris: Uint32Array,
  cadPatchIds: Uint32Array,
  faceEntityIds: Uint32Array
): number[] | null {
  const dense = wholeFaceDenseIds(tris, cadPatchIds);
  if (!dense) return null;
  return dense.map((f) => faceEntityIds[f]).sort((a, b) => a - b);
}

// ---- analytic surface data (DESIGN §18 M3) ----

const dot = (a: number[], b: number[]) => a[0] * b[0] + a[1] * b[1] + a[2] * b[2];

/** Exact CAD cylinder of a selection: non-null iff the selection is exactly a
 *  union of CYLINDRICAL faces sharing one axis and radius (the common
 *  split-cylinder case included — CAD often models a bore as two halves).
 *  Result is in the IMPORT frame. */
export function exactCylinderForSelection(
  tris: Uint32Array,
  cadPatchIds: Uint32Array,
  faces: StepFaceInfo[] | null
): { origin: [number, number, number]; axis: [number, number, number]; radius: number } | null {
  if (!faces) return null;
  const dense = wholeFaceDenseIds(tris, cadPatchIds);
  if (!dense || dense.length === 0) return null;
  let ref: StepFaceInfo | null = null;
  for (const d of dense) {
    const f = faces[d];
    if (!f || f.type !== "cylinder" || !f.axis || !f.origin || !(f.radius! > 0)) return null;
    if (!ref) {
      ref = f;
      continue;
    }
    // Same axis direction (sign-insensitive), same radius, origins colinear
    // with the axis (perpendicular offset ~0).
    if (Math.abs(dot(f.axis, ref.axis!)) < 1 - 1e-6) return null;
    if (Math.abs(f.radius! - ref.radius!) > 1e-4 * Math.max(1, ref.radius!)) return null;
    const d0 = [
      f.origin[0] - ref.origin![0],
      f.origin[1] - ref.origin![1],
      f.origin[2] - ref.origin![2],
    ];
    const along = dot(d0, ref.axis!);
    const perp2 = dot(d0, d0) - along * along;
    if (perp2 > 1e-6) return null;
  }
  const o = ref!.origin!;
  const a = ref!.axis!;
  return { origin: [o[0], o[1], o[2]], axis: [a[0], a[1], a[2]], radius: ref!.radius! };
}

/** CAD area (mm²) of a whole-face selection; null for brush/partial
 *  selections or when face metadata is unavailable. */
export function selectionCadArea(
  tris: Uint32Array,
  cadPatchIds: Uint32Array,
  faces: StepFaceInfo[] | null
): number | null {
  if (!faces) return null;
  const dense = wholeFaceDenseIds(tris, cadPatchIds);
  if (!dense) return null;
  let area = 0;
  for (const d of dense) {
    const f = faces[d];
    if (!f || !(f.area > 0)) return null;
    area += f.area;
  }
  return area;
}

/** True when the selection is exactly a union of cylindrical CAD faces on one
 *  axis — the "this is a bore/boss, consider a bearing load" signal. */
export function isCylindricalSelection(
  tris: Uint32Array,
  cadPatchIds: Uint32Array,
  faces: StepFaceInfo[] | null
): boolean {
  return exactCylinderForSelection(tris, cadPatchIds, faces) !== null;
}

// ---- CAD presentation colors (DESIGN §18 M4) ----

const srgbToLinear = (c: number) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
/** The viewer's default part grey (0x9aa3ad) in linear space — unstyled faces
 *  of a colored model fall back to it so they match uncolored models. */
const BASE_GREY_LINEAR = [0x9a, 0xa3, 0xad].map((v) => srgbToLinear(v / 255)) as [
  number,
  number,
  number,
];

/** Bake the STEP palette into per-WORKING-triangle linear RGB (3 floats/tri)
 *  for the viewer's vertex colors. Null when the model has no styled face at
 *  all (plain grey then, no buffer to carry around). */
export function cadTriangleColors(
  cadPatchIds: Uint32Array,
  faceColorIdx: Int32Array | null,
  palette: [number, number, number][] | null
): Float32Array | null {
  if (!faceColorIdx || !palette || !faceColorIdx.some((i) => i >= 0)) return null;
  const lin = palette.map(
    ([r, g, b]) => [srgbToLinear(r), srgbToLinear(g), srgbToLinear(b)] as [number, number, number]
  );
  const out = new Float32Array(cadPatchIds.length * 3);
  for (let t = 0; t < cadPatchIds.length; t++) {
    const idx = faceColorIdx[cadPatchIds[t]] ?? -1;
    const c = idx >= 0 && idx < lin.length ? lin[idx] : BASE_GREY_LINEAR;
    out[3 * t] = c[0];
    out[3 * t + 1] = c[1];
    out[3 * t + 2] = c[2];
  }
  return out;
}

// ---- rigid-transform bookkeeping (import frame → current world) ----

/** Compose 12-value rigid transforms (row-major 3×3 + translation), applying
 *  `m` AFTER `a` — the same composition `Model::transform` accumulates. */
export function composeTransform(m: number[], a: number[]): number[] {
  const out = new Array<number>(12);
  for (let r = 0; r < 3; r++) {
    for (let c = 0; c < 3; c++) {
      out[3 * r + c] = m[3 * r] * a[c] + m[3 * r + 1] * a[3 + c] + m[3 * r + 2] * a[6 + c];
    }
    out[9 + r] = m[3 * r] * a[9] + m[3 * r + 1] * a[10] + m[3 * r + 2] * a[11] + m[9 + r];
  }
  return out;
}

export const IDENTITY_TRANSFORM: number[] = [1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0];

export function transformPoint(m: number[], p: [number, number, number]): [number, number, number] {
  return [
    m[0] * p[0] + m[1] * p[1] + m[2] * p[2] + m[9],
    m[3] * p[0] + m[4] * p[1] + m[5] * p[2] + m[10],
    m[6] * p[0] + m[7] * p[1] + m[8] * p[2] + m[11],
  ];
}

/** Transform a packed xyz point array (e.g. edge segments) — returns a new
 *  array, the import-frame source stays untouched for later poses. */
export function transformPoints(m: number[], pts: Float32Array): Float32Array {
  const out = new Float32Array(pts.length);
  for (let i = 0; i + 2 < pts.length; i += 3) {
    const x = pts[i];
    const y = pts[i + 1];
    const z = pts[i + 2];
    out[i] = m[0] * x + m[1] * y + m[2] * z + m[9];
    out[i + 1] = m[3] * x + m[4] * y + m[5] * z + m[10];
    out[i + 2] = m[6] * x + m[7] * y + m[8] * z + m[11];
  }
  return out;
}

export function transformDir(m: number[], v: [number, number, number]): [number, number, number] {
  const out: [number, number, number] = [
    m[0] * v[0] + m[1] * v[1] + m[2] * v[2],
    m[3] * v[0] + m[4] * v[1] + m[5] * v[2],
    m[6] * v[0] + m[7] * v[1] + m[8] * v[2],
  ];
  const n = Math.hypot(out[0], out[1], out[2]) || 1;
  return [out[0] / n, out[1] / n, out[2] / n];
}

/** Working-mesh triangles of the given CAD faces (by entity record number) on
 *  a fresh tessellation. Unknown entity ids are skipped — a face the new
 *  meshStep version failed to mesh simply contributes no triangles (coverage
 *  can also GROW between versions; ids never renumber). */
export function trisForFaceIds(
  faceIds: number[],
  cadPatchIds: Uint32Array,
  faceEntityIds: Uint32Array
): Uint32Array {
  const dense = new Set<number>();
  const byEntity = new Map<number, number>();
  faceEntityIds.forEach((entity, d) => byEntity.set(entity, d));
  for (const id of faceIds) {
    const d = byEntity.get(id);
    if (d !== undefined) dense.add(d);
  }
  const out: number[] = [];
  for (let t = 0; t < cadPatchIds.length; t++) if (dense.has(cadPatchIds[t])) out.push(t);
  return Uint32Array.from(out);
}
