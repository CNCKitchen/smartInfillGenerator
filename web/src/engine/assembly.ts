// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Assembly bodies: pure helpers behind the component list (activate /
//! deactivate parts of a multi-body model). The engine has no exclusion API —
//! a body toggle reloads a JS-side-filtered mesh — so everything here works
//! on plain triangle arrays. Two mesh spaces matter:
//!
//!  - SOURCE tris: what was loaded (STEP import triangles / STL facets, minus
//!    the engine's degenerate drops). Body membership lives here.
//!  - DISPLAY tris: the engine's refined working soup (BC selections & the
//!    viewport index it). Display → source comes from the engine's own
//!    refinement parent map (`engine.refinementParents()`) — never from a
//!    geometric reconstruction, which proved order-dependent and wrong.

/** Connected-component labeling of a triangle soup (9 floats/tri) by EXACT
 *  shared vertex coordinates. Refinement midpoints stay inside their parent
 *  and parent corners are copied verbatim, so exact welding is the right
 *  equality here (same rule the engine's own body_count uses).
 *
 *  Interior cavity shells (negative signed volume, bbox nested inside another
 *  component) are merged into their enclosing body — a hollow part is ONE
 *  body, not "outer shell + cavity". Labels are numbered in first-triangle
 *  order, which matches the engine's original mesh order. */
export function connectedBodies(soup: Float32Array): { bodyOfTri: Uint32Array; count: number } {
  const nt = Math.floor(soup.length / 9);
  const vertOfCorner = new Int32Array(nt * 3);
  const weld = new Map<string, number>();
  let nv = 0;
  for (let i = 0; i < nt * 3; i++) {
    const key = `${soup[3 * i]},${soup[3 * i + 1]},${soup[3 * i + 2]}`;
    let id = weld.get(key);
    if (id === undefined) {
      id = nv++;
      weld.set(key, id);
    }
    vertOfCorner[i] = id;
  }
  // Union-find over welded vertices.
  const parent = new Int32Array(nv);
  for (let i = 0; i < nv; i++) parent[i] = i;
  const find = (x: number): number => {
    let r = x;
    while (parent[r] !== r) r = parent[r];
    while (parent[x] !== r) {
      const n = parent[x];
      parent[x] = r;
      x = n;
    }
    return r;
  };
  for (let t = 0; t < nt; t++) {
    const a = find(vertOfCorner[3 * t]);
    const b = find(vertOfCorner[3 * t + 1]);
    const c = find(vertOfCorner[3 * t + 2]);
    parent[b] = a;
    parent[find(c)] = find(a);
  }
  // Component label per tri, in first-occurrence order.
  const labelOfRoot = new Map<number, number>();
  const bodyOfTri = new Uint32Array(nt);
  for (let t = 0; t < nt; t++) {
    const r = find(vertOfCorner[3 * t]);
    let l = labelOfRoot.get(r);
    if (l === undefined) {
      l = labelOfRoot.size;
      labelOfRoot.set(r, l);
    }
    bodyOfTri[t] = l;
  }
  let count = labelOfRoot.size;
  if (count <= 1) return { bodyOfTri, count };
  // Cavity merge: signed volume + bbox per component.
  const vol = new Float64Array(count);
  const bbox = new Float64Array(count * 6);
  for (let c = 0; c < count; c++) {
    bbox[6 * c] = bbox[6 * c + 1] = bbox[6 * c + 2] = Infinity;
    bbox[6 * c + 3] = bbox[6 * c + 4] = bbox[6 * c + 5] = -Infinity;
  }
  for (let t = 0; t < nt; t++) {
    const c = bodyOfTri[t];
    const o = 9 * t;
    const ax = soup[o], ay = soup[o + 1], az = soup[o + 2];
    const bx = soup[o + 3], by = soup[o + 4], bz = soup[o + 5];
    const cx = soup[o + 6], cy = soup[o + 7], cz = soup[o + 8];
    vol[c] +=
      (ax * (by * cz - bz * cy) + ay * (bz * cx - bx * cz) + az * (bx * cy - by * cx)) / 6;
    for (let v = 0; v < 3; v++) {
      const x = soup[o + 3 * v], y = soup[o + 3 * v + 1], z = soup[o + 3 * v + 2];
      if (x < bbox[6 * c]) bbox[6 * c] = x;
      if (y < bbox[6 * c + 1]) bbox[6 * c + 1] = y;
      if (z < bbox[6 * c + 2]) bbox[6 * c + 2] = z;
      if (x > bbox[6 * c + 3]) bbox[6 * c + 3] = x;
      if (y > bbox[6 * c + 4]) bbox[6 * c + 4] = y;
      if (z > bbox[6 * c + 5]) bbox[6 * c + 5] = z;
    }
  }
  const mergeInto = new Int32Array(count).fill(-1);
  for (let c = 0; c < count; c++) {
    if (vol[c] >= 0) continue; // outward-wound shell: a real body
    // Smallest enclosing positive-volume component wins (nested cavities).
    let best = -1;
    let bestVol = Infinity;
    for (let d = 0; d < count; d++) {
      if (d === c || vol[d] <= 0) continue;
      const inside =
        bbox[6 * c] >= bbox[6 * d] &&
        bbox[6 * c + 1] >= bbox[6 * d + 1] &&
        bbox[6 * c + 2] >= bbox[6 * d + 2] &&
        bbox[6 * c + 3] <= bbox[6 * d + 3] &&
        bbox[6 * c + 4] <= bbox[6 * d + 4] &&
        bbox[6 * c + 5] <= bbox[6 * d + 5];
      if (inside && vol[d] < bestVol) {
        best = d;
        bestVol = vol[d];
      }
    }
    if (best >= 0) mergeInto[c] = best;
  }
  if (mergeInto.some((m) => m >= 0)) {
    // Relabel: surviving components keep first-occurrence order.
    const newLabel = new Int32Array(count).fill(-1);
    let next = 0;
    const resolve = (c: number): number => (mergeInto[c] >= 0 ? resolve(mergeInto[c]) : c);
    for (let t = 0; t < nt; t++) {
      const c = resolve(bodyOfTri[t]);
      if (newLabel[c] < 0) newLabel[c] = next++;
      bodyOfTri[t] = newLabel[c];
    }
    count = next;
  }
  return { bodyOfTri, count };
}

/** Serialize a triangle-soup subset as a binary STL (mm, computed normals). */
export function buildBinaryStl(soup: Float32Array, tris: ArrayLike<number>): ArrayBuffer {
  const n = tris.length;
  const buf = new ArrayBuffer(84 + 50 * n);
  const dv = new DataView(buf);
  dv.setUint32(80, n, true);
  let off = 84;
  for (let i = 0; i < n; i++) {
    const o = 9 * tris[i];
    const ax = soup[o], ay = soup[o + 1], az = soup[o + 2];
    const bx = soup[o + 3], by = soup[o + 4], bz = soup[o + 5];
    const cx = soup[o + 6], cy = soup[o + 7], cz = soup[o + 8];
    const ux = bx - ax, uy = by - ay, uz = bz - az;
    const vx = cx - ax, vy = cy - ay, vz = cz - az;
    let nx = uy * vz - uz * vy;
    let ny = uz * vx - ux * vz;
    let nz = ux * vy - uy * vx;
    const l = Math.hypot(nx, ny, nz) || 1;
    nx /= l; ny /= l; nz /= l;
    dv.setFloat32(off, nx, true);
    dv.setFloat32(off + 4, ny, true);
    dv.setFloat32(off + 8, nz, true);
    const verts = [ax, ay, az, bx, by, bz, cx, cy, cz];
    for (let v = 0; v < 9; v++) dv.setFloat32(off + 12 + 4 * v, verts[v], true);
    dv.setUint16(off + 48, 0, true);
    off += 50;
  }
  return buf;
}

/** True when a 12-value rigid transform is (numerically) the identity. */
export function isIdentityTransform(m: number[]): boolean {
  const I = [1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0];
  for (let i = 0; i < 12; i++) if (Math.abs(m[i] - I[i]) > 1e-12) return false;
  return true;
}
