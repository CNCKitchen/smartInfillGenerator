// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! The STEP → StepMeshPayload conversion (DESIGN §18), extracted from the
//! import worker so it can ALSO run under plain Node (type stripping) — the
//! sample-model build script pre-tessellates the bundled part with exactly
//! this code, guaranteeing the shipped cache is bit-identical to what a live
//! import would produce. Environment-agnostic on purpose: crypto.subtle,
//! TextDecoder and meshStep only — no worker or DOM APIs. Relative imports
//! carry the .ts extension (Node ESM resolution; Vite handles them fine).

import {
  autoTessellation,
  estimateStepSize,
  importStep,
  VERSION,
  type ImportProgress,
} from "meshstep";
import type {
  AssemblyTreeNode,
  StepMeshPayload,
  StepTessOpts,
} from "./StepImporter.ts";
import { computeFeatureEdges, type StepFaceInfo } from "./stepSelection.ts";

/** Structural view of meshStep's PartNode tree (not re-exported by the
 *  package index) — only the fields the name walk reads. */
interface PartNodeLike {
  name: string;
  occurrences?: number;
  bodies: { id: number; name: string }[];
  children: PartNodeLike[];
}

/** meshStep's product tree (solid ENTITY ids) → the payload's dense-id tree.
 *  Nodes with no meshed solid anywhere below them are pruned; solids the
 *  product graph missed are appended to the root so every list row has a
 *  home in the hierarchy. Null when there is no tree or nothing survives. */
function toDenseTree(
  root: PartNodeLike | undefined,
  entityToDense: Map<number, number>,
  denseCount: number
): AssemblyTreeNode | null {
  if (!root) return null;
  const walk = (n: PartNodeLike): AssemblyTreeNode | null => {
    const bodies: number[] = [];
    for (const b of n.bodies ?? []) {
      const d = entityToDense.get(b.id);
      if (d !== undefined && !bodies.includes(d)) bodies.push(d);
    }
    const children: AssemblyTreeNode[] = [];
    for (const c of n.children ?? []) {
      const t = walk(c);
      if (t) children.push(t);
    }
    if (bodies.length === 0 && children.length === 0) return null;
    return { name: (n.name || "").trim(), occurrences: n.occurrences ?? 1, bodies, children };
  };
  const tree = walk(root);
  if (!tree) return null;
  const seen = new Set<number>();
  const collect = (n: AssemblyTreeNode) => {
    for (const d of n.bodies) seen.add(d);
    for (const c of n.children) collect(c);
  };
  collect(tree);
  for (let d = 0; d < denseCount; d++) if (!seen.has(d)) tree.bodies.push(d);
  return tree;
}

/** Solid ENTITY id → component name from the STEP product structure. CAD
 *  exports (Fusion etc.) name the PART meaningfully ("tension_knob") but its
 *  bodies generically ("Body1"), so a generic body name defers to the owning
 *  part's name; a part with several real bodies keeps "part · body". */
function collectSolidNames(root: PartNodeLike | undefined): Map<number, string> {
  const out = new Map<number, string>();
  const generic = /^body\s*\d*$/i;
  const walk = (node: PartNodeLike) => {
    const nodeName = (node.name || "").trim();
    const bodies = node.bodies ?? [];
    for (const b of bodies) {
      const bodyName = (b.name || "").trim();
      let name: string;
      if (!bodyName || (generic.test(bodyName) && nodeName)) {
        name = bodies.length > 1 && bodyName ? `${nodeName} · ${bodyName}` : nodeName || bodyName;
      } else {
        name = bodyName;
      }
      if (name && !out.has(b.id)) out.set(b.id, name);
    }
    for (const c of node.children ?? []) walk(c);
  };
  if (root) walk(root);
  return out;
}

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

/** SHA-256 (hex) of the ORIGINAL STEP bytes (before any decoding): entity ids
 *  are stable per file, so this is the key face-id selections bind to
 *  (DESIGN §18 dec. 5c) — and the key the pre-tessellated cache validates
 *  against. */
export async function stepSha256(bytes: ArrayBuffer): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** Convert STEP bytes to the engine- and persistence-ready mesh payload.
 *  `opts` overrides the file-derived tessellation defaults (project reopen /
 *  cache replay); same bytes + same opts + same meshStep version ⇒
 *  bit-identical output (the determinism every saved selection relies on). */
export async function convertStepBytes(
  bytes: ArrayBuffer,
  optsIn?: StepTessOpts,
  onProgress?: (p: ImportProgress) => void
): Promise<StepMeshPayload> {
  const sha256 = await stepSha256(bytes);
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
  let opts = optsIn;
  if (!opts) {
    const est = estimateStepSize(text);
    const auto = autoTessellation(est ? est.diag : 100);
    opts = {
      surfaceDeviation: auto.surfaceDeviation,
      normalDeviation: 15,
      maxEdge: auto.maxEdge,
    };
  }
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
  // CAD feature edges (viewport overlay): every mesh edge whose two
  // triangles belong to DIFFERENT CAD faces, taken from meshStep's
  // original welded mesh — it is conforming, so adjacency is exact. The
  // engine's display refinement is non-conforming (T-junctions), which is
  // why the overlay must NOT be derived from the working mesh. Shared with
  // the assembly active-set rebuild (filtered model) in the store.
  const positions = new Float32Array(r.mesh.positions); // f64 mm → f32 for GPU/wasm
  const { segments: featureEdges, segFace: featureEdgeFaces } = computeFeatureEdges(
    positions,
    r.mesh.indices,
    face.dense
  );
  // Component names for the assembly list, per dense solid id.
  const structRoot = (r as unknown as { structure?: PartNodeLike }).structure;
  const nameByEntity = collectSolidNames(structRoot);
  const solidNames = Array.from(solid.table, (entity) => nameByEntity.get(entity) ?? null);
  // Assembly hierarchy (dense solid ids) for the component list.
  const entityToDense = new Map<number, number>();
  solid.table.forEach((entity, d) => entityToDense.set(entity, d));
  const structure = toDenseTree(structRoot, entityToDense, solid.table.length);
  return {
    positions,
    indices: r.mesh.indices,
    faceOfTri: face.dense,
    solidOfTri: solid.dense,
    faceEntityIds: face.table,
    solidEntityIds: solid.table,
    openSolids,
    solidNames,
    structure,
    faces,
    palette,
    faceColorIdx,
    featureEdges,
    featureEdgeFaces,
    diagnostics: r.diagnostics,
    stats: r.stats,
    units: r.units,
    meshstepVersion: VERSION,
    opts,
    sha256,
  };
}
