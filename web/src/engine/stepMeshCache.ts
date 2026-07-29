// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Binary container for a pre-tessellated StepMeshPayload — the sample-model
//! fast path. The build script (`npm run demo:mesh`) encodes the payload once
//! at build time; the import worker decodes it and skips tessellation when it
//! still matches the request (same STEP sha-256, same meshStep version, same
//! opts — validated by the WORKER, not trusted blindly). Layout:
//!
//!   bytes 0..8   magic "FSMCACHE"
//!   u32 LE       container version (1)
//!   u32 LE       JSON header byte length
//!   …            UTF-8 JSON header (meta + array table), padded to 4 bytes
//!   …            typed-array blobs, in table order, each 4-byte aligned
//!
//! Pure functions, no DOM/worker/meshStep deps — runs in Node and the worker.

import type { StepMeshPayload } from "./StepImporter.ts";

const MAGIC = "FSMCACHE";
const CONTAINER_VERSION = 1;

type ArrayKind = "f32" | "u32" | "i32";

/** The typed-array fields of StepMeshPayload, in serialization order. */
const ARRAY_FIELDS: { key: string; kind: ArrayKind }[] = [
  { key: "positions", kind: "f32" },
  { key: "indices", kind: "u32" },
  { key: "faceOfTri", kind: "u32" },
  { key: "solidOfTri", kind: "u32" },
  { key: "faceEntityIds", kind: "u32" },
  { key: "solidEntityIds", kind: "u32" },
  { key: "featureEdges", kind: "f32" },
  { key: "featureEdgeFaces", kind: "u32" },
  { key: "faceColorIdx", kind: "i32" }, // optional — length 0 + meta flag when null
];

interface HeaderJson {
  meta: Omit<
    StepMeshPayload,
    | "positions"
    | "indices"
    | "faceOfTri"
    | "solidOfTri"
    | "faceEntityIds"
    | "solidEntityIds"
    | "featureEdges"
    | "featureEdgeFaces"
    | "faceColorIdx"
  > & { hasFaceColorIdx: boolean };
  arrays: { key: string; kind: ArrayKind; count: number }[];
}

const align4 = (n: number) => (n + 3) & ~3;

/** Serialize a payload into one self-contained ArrayBuffer. */
export function encodeStepMesh(p: StepMeshPayload): ArrayBuffer {
  const arrays = ARRAY_FIELDS.map(({ key, kind }) => {
    const a = (p as unknown as Record<string, { length: number } | null>)[key];
    return { key, kind, count: a ? a.length : 0 };
  });
  const header: HeaderJson = {
    meta: {
      openSolids: p.openSolids,
      solidNames: p.solidNames,
      structure: p.structure,
      faces: p.faces,
      palette: p.palette,
      diagnostics: p.diagnostics,
      stats: p.stats,
      units: p.units,
      meshstepVersion: p.meshstepVersion,
      opts: p.opts,
      sha256: p.sha256,
      hasFaceColorIdx: p.faceColorIdx !== null,
    },
    arrays,
  };
  const json = new TextEncoder().encode(JSON.stringify(header));
  let off = align4(16 + json.byteLength);
  const offsets: number[] = [];
  for (const a of arrays) {
    offsets.push(off);
    off += align4(a.count * 4); // every element kind is 4 bytes wide
  }
  const buf = new ArrayBuffer(off);
  const u8 = new Uint8Array(buf);
  const dv = new DataView(buf);
  for (let i = 0; i < MAGIC.length; i++) u8[i] = MAGIC.charCodeAt(i);
  dv.setUint32(8, CONTAINER_VERSION, true);
  dv.setUint32(12, json.byteLength, true);
  u8.set(json, 16);
  arrays.forEach((a, i) => {
    const src = (p as unknown as Record<string, ArrayLike<number> | null>)[a.key];
    if (!src || a.count === 0) return;
    const dst =
      a.kind === "f32"
        ? new Float32Array(buf, offsets[i], a.count)
        : a.kind === "i32"
          ? new Int32Array(buf, offsets[i], a.count)
          : new Uint32Array(buf, offsets[i], a.count);
    dst.set(src as ArrayLike<number> & { length: number });
  });
  return buf;
}

/** Deserialize a container back into a payload. Every array is COPIED out
 *  into its own buffer — payload consumers (engine.loadMesh, the store)
 *  transfer individual buffers and must never find them aliased to one
 *  shared container. Throws on a malformed/foreign buffer (callers treat
 *  that as "no cache"). */
export function decodeStepMesh(buf: ArrayBuffer): StepMeshPayload {
  if (buf.byteLength < 16) throw new Error("mesh cache: truncated");
  const u8 = new Uint8Array(buf);
  for (let i = 0; i < MAGIC.length; i++) {
    if (u8[i] !== MAGIC.charCodeAt(i)) throw new Error("mesh cache: bad magic");
  }
  const dv = new DataView(buf);
  if (dv.getUint32(8, true) !== CONTAINER_VERSION) throw new Error("mesh cache: unknown version");
  const jsonLen = dv.getUint32(12, true);
  if (16 + jsonLen > buf.byteLength) throw new Error("mesh cache: truncated header");
  const header = JSON.parse(
    new TextDecoder().decode(new Uint8Array(buf, 16, jsonLen))
  ) as HeaderJson;
  let off = align4(16 + jsonLen);
  const views: Record<string, Float32Array | Uint32Array | Int32Array> = {};
  for (const a of header.arrays) {
    const end = off + a.count * 4;
    if (end > buf.byteLength) throw new Error("mesh cache: truncated arrays");
    const view =
      a.kind === "f32"
        ? new Float32Array(buf, off, a.count)
        : a.kind === "i32"
          ? new Int32Array(buf, off, a.count)
          : new Uint32Array(buf, off, a.count);
    views[a.key] = view.slice(); // own buffer (see doc comment)
    off += align4(a.count * 4);
  }
  const { hasFaceColorIdx, ...meta } = header.meta;
  return {
    ...meta,
    positions: views.positions as Float32Array,
    indices: views.indices as Uint32Array,
    faceOfTri: views.faceOfTri as Uint32Array,
    solidOfTri: views.solidOfTri as Uint32Array,
    faceEntityIds: views.faceEntityIds as Uint32Array,
    solidEntityIds: views.solidEntityIds as Uint32Array,
    featureEdges: views.featureEdges as Float32Array,
    featureEdgeFaces: views.featureEdgeFaces as Uint32Array,
    faceColorIdx: hasFaceColorIdx ? (views.faceColorIdx as Int32Array) : null,
  };
}
