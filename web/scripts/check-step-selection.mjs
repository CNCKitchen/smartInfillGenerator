// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Unit test for stepSelection.ts (DESIGN §18 M2 rebind identity).
// Run: npm run check:step  (Node ≥23.6 — imports the .ts via type stripping)
const { selectionFaceIds, trisForFaceIds } = await import(
  new URL("../src/engine/stepSelection.ts", import.meta.url).href
);

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

console.log("\nSTEP SELECTION HELPERS PASS");
