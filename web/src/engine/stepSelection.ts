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
  return [...selected.keys()].map((f) => faceEntityIds[f]).sort((a, b) => a - b);
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
