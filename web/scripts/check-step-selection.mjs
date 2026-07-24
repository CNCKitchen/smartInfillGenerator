// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Unit test for stepSelection.ts (DESIGN §18 M2 rebind identity + M3 analytics).
// Run: npm run check:step  (Node ≥23.6 — imports the .ts via type stripping)
const {
  selectionFaceIds,
  trisForFaceIds,
  exactCylinderForSelection,
  selectionCadArea,
  composeTransform,
  transformPoint,
  transformDir,
  IDENTITY_TRANSFORM,
} = await import(new URL("../src/engine/stepSelection.ts", import.meta.url).href);

const assert = (c, m) => { if (!c) { console.error(`FAIL: ${m}`); process.exit(1); } console.log(`ok: ${m}`); };
const eq = (a, b) => JSON.stringify([...a]) === JSON.stringify([...b]);

// "Old" tessellation: 6 working tris, faces dense 0/1/2 → entities 101/205/999.
const cad1 = Uint32Array.from([0, 0, 1, 1, 2, 2]);
const ent1 = Uint32Array.from([101, 205, 999]);

assert(eq(selectionFaceIds(Uint32Array.from([0, 1]), cad1, ent1), [101]), "whole face → its entity id");
assert(selectionFaceIds(Uint32Array.from([0]), cad1, ent1) === null, "partial face → null (brush fallback)");
assert(eq(selectionFaceIds(Uint32Array.from([2, 0, 3, 1]), cad1, ent1), [101, 205]), "two whole faces, sorted ids");
assert(selectionFaceIds(Uint32Array.from([]), cad1, ent1) === null, "empty selection → null");
assert(selectionFaceIds(Uint32Array.from([99]), cad1, ent1) === null, "stale index → null, never fabricates");

assert(eq(trisForFaceIds([101, 999], cad1, ent1), [0, 1, 4, 5]), "entity ids → their triangles");
assert(eq(trisForFaceIds([555], cad1, ent1), []), "unknown entity id skipped (coverage growth tolerated)");

// Round-trip on the same tessellation.
const sel = Uint32Array.from([2, 3, 4, 5]);
const ids = selectionFaceIds(sel, cad1, ent1);
assert(eq(trisForFaceIds(ids, cad1, ent1), [2, 3, 4, 5]), "same-version round-trip is identity");

// "New" meshStep version: same entities, DIFFERENT triangle layout AND
// different densification order (dense ids are per-import, entity ids are
// the join key). Face 205 now meshes to 3 triangles.
const cad2 = Uint32Array.from([1, 1, 1, 0, 0, 2, 2]); // dense: 0→205? no — see ent2
const ent2 = Uint32Array.from([999, 205, 101]); // dense0=999, dense1=205, dense2=101
assert(eq(trisForFaceIds([205], cad2, ent2), [0, 1, 2]), "rebind across versions via entity id (grown face)");
assert(eq(trisForFaceIds([101, 999], cad2, ent2), [3, 4, 5, 6]), "multi-face rebind lands on the new layout");

// ---- M3: analytic surface helpers ----
// Faces for cad1 (dense 0/1/2): a plane, and a bore split into two halves —
// same axis (z through origin), same radius, origins offset ALONG the axis.
const faces = [
  { entityId: 101, type: "plane", area: 100, meanNormal: [0, 0, 1] },
  { entityId: 205, type: "cylinder", area: 31.4, meanNormal: [0, 0, 0], origin: [0, 0, 0], axis: [0, 0, 1], radius: 5 },
  { entityId: 999, type: "cylinder", area: 31.4, meanNormal: [0, 0, 0], origin: [0, 0, 10], axis: [0, 0, -1], radius: 5 },
];

let cyl = exactCylinderForSelection(Uint32Array.from([2, 3]), cad1, faces);
assert(cyl && cyl.radius === 5 && Math.abs(cyl.axis[2]) === 1, "single cylindrical face → exact cylinder");
cyl = exactCylinderForSelection(Uint32Array.from([2, 3, 4, 5]), cad1, faces);
assert(cyl && cyl.radius === 5, "split bore (two halves, flipped axis, colinear origins) combines");
assert(exactCylinderForSelection(Uint32Array.from([0, 1]), cad1, faces) === null, "plane → no cylinder");
assert(exactCylinderForSelection(Uint32Array.from([2]), cad1, faces) === null, "partial face → no cylinder");
assert(
  exactCylinderForSelection(Uint32Array.from([0, 1, 2, 3]), cad1, faces) === null,
  "plane + cylinder union → no cylinder"
);
{
  const off = faces.map((f) => ({ ...f }));
  off[2].origin = [1, 0, 10]; // parallel but NOT colinear → two different bores
  assert(
    exactCylinderForSelection(Uint32Array.from([2, 3, 4, 5]), cad1, off) === null,
    "parallel offset axes rejected"
  );
  const r2 = faces.map((f) => ({ ...f }));
  r2[2].radius = 6;
  assert(
    exactCylinderForSelection(Uint32Array.from([2, 3, 4, 5]), cad1, r2) === null,
    "radius mismatch rejected"
  );
}
assert(exactCylinderForSelection(Uint32Array.from([2, 3]), cad1, null) === null, "faces=null (instanced) → null");

assert(selectionCadArea(Uint32Array.from([0, 1]), cad1, faces) === 100, "whole-face CAD area");
assert(Math.abs(selectionCadArea(Uint32Array.from([0, 1, 2, 3]), cad1, faces) - 131.4) < 1e-9, "multi-face area sums");
assert(selectionCadArea(Uint32Array.from([0]), cad1, faces) === null, "partial selection → no CAD area");

// ---- M3: rigid-transform bookkeeping ----
// 90° about Z then translate: matches applying the parts in sequence.
const rot = [0, -1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0];
const trans = [1, 0, 0, 0, 1, 0, 0, 0, 1, 5, 0, 0];
const both = composeTransform(trans, rot); // trans AFTER rot
const p = [2, 0, 0];
const seq = transformPoint(trans, transformPoint(rot, p));
const one = transformPoint(both, p);
assert(seq.every((v, i) => Math.abs(v - one[i]) < 1e-12), "composeTransform == sequential application");
assert(eq(transformPoint(both, [2, 0, 0]).map((v) => Math.round(v * 1e9) / 1e9), [5, 2, 0]), "rotate 90°Z then +5x");
const d = transformDir(both, [1, 0, 0]);
assert(eq(d.map((v) => Math.round(v * 1e9) / 1e9), [0, 1, 0]), "directions rotate, ignore translation");
assert(eq(transformPoint(IDENTITY_TRANSFORM, [3, 4, 5]), [3, 4, 5]), "identity transform is identity");

console.log("\nSTEP SELECTION HELPERS PASS");
