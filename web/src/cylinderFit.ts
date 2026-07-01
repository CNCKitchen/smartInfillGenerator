// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Synchronous cylinder fit for bearing-load validation + glyphs, mirroring the
// Rust `sig_core::cylinder` fit (axis from the surface-normal covariance, then
// an algebraic circle fit in the plane ⟂ axis). Kept in TypeScript so the
// editor can validate a selection and draw the load distribution INSTANTLY —
// no worker round-trip that could stall — while the solver re-fits in Rust for
// the actual force assembly. Both use the same algorithm, so they agree.

import type { CylFit } from "./types";

/** Acceptance threshold on the cylindricity residual (RMS radial deviation /
 *  radius) — must match sig_core::cylinder::DEFAULT_TOL. */
export const CYL_TOL = 0.07;

type V3 = [number, number, number];

const sub = (a: V3, b: V3): V3 => [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
const dot = (a: V3, b: V3): number => a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
const cross = (a: V3, b: V3): V3 => [
  a[1] * b[2] - a[2] * b[1],
  a[2] * b[0] - a[0] * b[2],
  a[0] * b[1] - a[1] * b[0],
];
const len = (a: V3): number => Math.hypot(a[0], a[1], a[2]);

/** Fit a cylinder to a triangle selection. Returns `ok=false` when the input is
 *  degenerate or the surface isn't cylindrical (residual above tolerance). */
export function fitCylinderFromSelection(positions: Float32Array, tris: Uint32Array): CylFit {
  const fail = (): CylFit => ({ ok: false, axis: [0, 0, 1], point: [0, 0, 0], radius: 0, residual: Infinity });
  const n = tris.length;
  if (!positions || n < 6) return fail();

  // One sample per triangle: centroid, unit normal, area.
  const pts: V3[] = [];
  const nrm: V3[] = [];
  const ws: number[] = [];
  for (const t of tris) {
    const o = 9 * t;
    const a: V3 = [positions[o], positions[o + 1], positions[o + 2]];
    const b: V3 = [positions[o + 3], positions[o + 4], positions[o + 5]];
    const c: V3 = [positions[o + 6], positions[o + 7], positions[o + 8]];
    const av = cross(sub(b, a), sub(c, a)); // 2·area·normal
    const area = len(av) * 0.5;
    if (area <= 0) continue;
    pts.push([(a[0] + b[0] + c[0]) / 3, (a[1] + b[1] + c[1]) / 3, (a[2] + b[2] + c[2]) / 3]);
    nrm.push([av[0] / (2 * area), av[1] / (2 * area), av[2] / (2 * area)]);
    ws.push(area);
  }
  const m = pts.length;
  if (m < 6) return fail();

  // 1. Axis = smallest-eigenvalue eigenvector of the (area-weighted) normal
  //    covariance Σ w nnᵀ.
  const cov = [
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
  ];
  let wsum = 0;
  for (let i = 0; i < m; i++) {
    const w = ws[i];
    const u = nrm[i];
    wsum += w;
    for (let r = 0; r < 3; r++) for (let cc = 0; cc < 3; cc++) cov[r][cc] += w * u[r] * u[cc];
  }
  if (wsum <= 0) return fail();
  const { values, vectors } = jacobiEigen(cov);
  let imin = 0;
  for (let i = 1; i < 3; i++) if (values[i] < values[imin]) imin = i;
  const axis = unit([vectors[0][imin], vectors[1][imin], vectors[2][imin]]);
  if (!axis) return fail();

  // 2. Circle fit in the plane ⟂ axis.
  const [u, v] = orthoBasis(axis);
  const c0: V3 = [0, 0, 0];
  for (let i = 0; i < m; i++) for (let d = 0; d < 3; d++) c0[d] += ws[i] * pts[i][d];
  for (let d = 0; d < 3; d++) c0[d] /= wsum;
  // Kåsa: minimise Σ w (x²+y² − D·x − E·y − F)² → normal equations for [D,E,F].
  const A = [
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
  ];
  const rhs: V3 = [0, 0, 0];
  for (let i = 0; i < m; i++) {
    const w = ws[i];
    const d = sub(pts[i], c0);
    const x = dot(d, u);
    const y = dot(d, v);
    const z = x * x + y * y;
    const row: V3 = [x, y, 1];
    for (let r = 0; r < 3; r++) {
      for (let cc = 0; cc < 3; cc++) A[r][cc] += w * row[r] * row[cc];
      rhs[r] += w * row[r] * z;
    }
  }
  const sol = solve3(A, rhs);
  if (!sol) return fail();
  const cx = sol[0] * 0.5;
  const cy = sol[1] * 0.5;
  const r2 = sol[2] + cx * cx + cy * cy;
  if (!(r2 > 0)) return fail();
  const radius = Math.sqrt(r2);
  if (radius < 1e-9) return fail();
  const point: V3 = [
    c0[0] + cx * u[0] + cy * v[0],
    c0[1] + cx * u[1] + cy * v[1],
    c0[2] + cx * u[2] + cy * v[2],
  ];

  // 3. Residual = RMS(radial distance − radius) / radius.
  let acc = 0;
  for (let i = 0; i < m; i++) {
    const d = sub(pts[i], point);
    const axial = dot(d, axis);
    const radial: V3 = [d[0] - axial * axis[0], d[1] - axial * axis[1], d[2] - axial * axis[2]];
    const dev = len(radial) - radius;
    acc += ws[i] * dev * dev;
  }
  const residual = Math.sqrt(acc / wsum) / radius;

  return { ok: isFinite(residual) && residual <= CYL_TOL, axis, point, radius, residual };
}

function unit(a: V3): V3 | null {
  const l = len(a);
  return l < 1e-12 ? null : [a[0] / l, a[1] / l, a[2] / l];
}

/** An orthonormal pair spanning the plane ⟂ unit vector `n`. */
function orthoBasis(n: V3): [V3, V3] {
  const ax = Math.abs(n[0]);
  const ay = Math.abs(n[1]);
  const az = Math.abs(n[2]);
  const seed: V3 = ax <= ay && ax <= az ? [1, 0, 0] : ay <= az ? [0, 1, 0] : [0, 0, 1];
  const u = unit(cross(n, seed)) ?? [1, 0, 0];
  return [u, cross(n, u)];
}

/** Solve a 3×3 system A x = b via Cramer's rule; null if singular. */
function solve3(a: number[][], b: V3): V3 | null {
  const det = det3(a);
  if (Math.abs(det) < 1e-18) return null;
  const x: V3 = [0, 0, 0];
  for (let col = 0; col < 3; col++) {
    const mt = a.map((row) => row.slice());
    for (let r = 0; r < 3; r++) mt[r][col] = b[r];
    x[col] = det3(mt) / det;
  }
  return x;
}

function det3(m: number[][]): number {
  return (
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) -
    m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0]) +
    m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
  );
}

/** Cyclic Jacobi eigensolver for a symmetric 3×3. Eigenvectors are columns of
 *  `vectors` (eigenvector k = [vectors[0][k], vectors[1][k], vectors[2][k]]). */
function jacobiEigen(input: number[][]): { values: V3; vectors: number[][] } {
  const a = input.map((r) => r.slice());
  const vec = [
    [1, 0, 0],
    [0, 1, 0],
    [0, 0, 1],
  ];
  for (let sweep = 0; sweep < 50; sweep++) {
    let p = 0;
    let q = 1;
    let max = Math.abs(a[0][1]);
    for (const [i, j] of [
      [0, 2],
      [1, 2],
    ]) {
      if (Math.abs(a[i][j]) > max) {
        max = Math.abs(a[i][j]);
        p = i;
        q = j;
      }
    }
    if (max < 1e-15) break;
    const theta = 0.5 * Math.atan2(2 * a[p][q], a[p][p] - a[q][q]);
    const s = Math.sin(theta);
    const c = Math.cos(theta);
    const b = a.map((r) => r.slice());
    for (let k = 0; k < 3; k++) {
      b[k][p] = c * a[k][p] + s * a[k][q];
      b[k][q] = -s * a[k][p] + c * a[k][q];
    }
    const d = b.map((r) => r.slice());
    for (let k = 0; k < 3; k++) {
      d[p][k] = c * b[p][k] + s * b[q][k];
      d[q][k] = -s * b[p][k] + c * b[q][k];
    }
    for (let r = 0; r < 3; r++) for (let cc = 0; cc < 3; cc++) a[r][cc] = d[r][cc];
    const nv = vec.map((r) => r.slice());
    for (let k = 0; k < 3; k++) {
      nv[k][p] = c * vec[k][p] + s * vec[k][q];
      nv[k][q] = -s * vec[k][p] + c * vec[k][q];
    }
    for (let r = 0; r < 3; r++) for (let cc = 0; cc < 3; cc++) vec[r][cc] = nv[r][cc];
  }
  return { values: [a[0][0], a[1][1], a[2][2]], vectors: vec };
}
