// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Pre-tessellates the bundled sample model (repo-root CNCKTestHook.step) into
// web/public/sample/ so the app's first paint skips the meshStep conversion:
//   sample/CNCKTestHook.step  — the original bytes, verbatim (engine + saves)
//   sample/CNCKTestHook.mesh  — encoded StepMeshPayload (stepMeshCache.ts)
// The import WORKER validates the cache (sha-256 + meshStep VERSION + opts)
// before trusting it, so a stale cache degrades to a normal live import, never
// to a wrong mesh. Re-run after a meshStep upgrade or a sample-file change:
//   npm run sample:mesh          (Node ≥23.6 — imports the .ts via type stripping)
//   npm run sample:mesh -- --faces   additionally dumps the CAD face table
//                                    (pick BC faces for the sample setup)

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const { convertStepBytes } = await import(
  new URL("../src/engine/stepConvert.ts", import.meta.url).href
);
const { encodeStepMesh, decodeStepMesh } = await import(
  new URL("../src/engine/stepMeshCache.ts", import.meta.url).href
);

const here = dirname(fileURLToPath(import.meta.url));
const SRC = join(here, "../..", "CNCKTestHook.step");
const OUT_DIR = join(here, "..", "public", "sample");
const dumpFaces = process.argv.includes("--faces");

const nodeBytes = readFileSync(SRC);
// Copy into a fresh ArrayBuffer — Node Buffers are views on a shared pool.
const bytes = nodeBytes.buffer.slice(
  nodeBytes.byteOffset,
  nodeBytes.byteOffset + nodeBytes.byteLength
);

const t0 = performance.now();
const payload = await convertStepBytes(bytes);
const dt = ((performance.now() - t0) / 1000).toFixed(2);
console.log(
  `converted ${SRC} in ${dt}s — meshStep ${payload.meshstepVersion}, ` +
    `${payload.indices.length / 3} tris, ${payload.faceEntityIds.length} CAD faces, ` +
    `opts ${JSON.stringify(payload.opts)}, sha256 ${payload.sha256.slice(0, 12)}…`
);

if (dumpFaces) {
  const pos = payload.positions;
  const idx = payload.indices;
  // Area-weighted centroid per dense face id, from the tessellation.
  const cent = payload.faceEntityIds.length;
  const acc = Array.from({ length: cent }, () => ({ a: 0, x: 0, y: 0, z: 0 }));
  for (let t = 0; t < payload.faceOfTri.length; t++) {
    const f = acc[payload.faceOfTri[t]];
    const [i, j, k] = [idx[3 * t], idx[3 * t + 1], idx[3 * t + 2]];
    const ax = pos[3 * i], ay = pos[3 * i + 1], az = pos[3 * i + 2];
    const bx = pos[3 * j], by = pos[3 * j + 1], bz = pos[3 * j + 2];
    const cx = pos[3 * k], cy = pos[3 * k + 1], cz = pos[3 * k + 2];
    const ux = bx - ax, uy = by - ay, uz = bz - az;
    const vx = cx - ax, vy = cy - ay, vz = cz - az;
    const nx = uy * vz - uz * vy, ny = uz * vx - ux * vz, nz = ux * vy - uy * vx;
    const a = Math.hypot(nx, ny, nz) / 2;
    f.a += a;
    f.x += (a * (ax + bx + cx)) / 3;
    f.y += (a * (ay + by + cy)) / 3;
    f.z += (a * (az + bz + cz)) / 3;
  }
  const fmt = (v) => v.map((x) => x.toFixed(1)).join(",");
  const rows = (payload.faces ?? [])
    .map((f, d) => ({ f, d }))
    .sort((p, q) => q.f.area - p.f.area);
  console.log("\n#dense entity   type      area      centroid            normal              extra");
  for (const { f, d } of rows) {
    const c = acc[d].a > 0 ? [acc[d].x / acc[d].a, acc[d].y / acc[d].a, acc[d].z / acc[d].a] : [0, 0, 0];
    const extra =
      f.type === "cylinder" || f.type === "cone"
        ? `r=${f.radius?.toFixed(2)} axis=${fmt(f.axis ?? [0, 0, 0])} o=${fmt(f.origin ?? [0, 0, 0])}`
        : "";
    console.log(
      `${String(d).padStart(4)} ${String(f.entityId).padStart(6)}  ${f.type.padEnd(9)} ` +
        `${f.area.toFixed(1).padStart(8)}  [${fmt(c).padEnd(18)}] [${fmt(f.meanNormal).padEnd(17)}] ${extra}`
    );
  }
}

const encoded = encodeStepMesh(payload);
// Round-trip self-check: the decoder must reproduce the payload exactly.
const back = decodeStepMesh(encoded);
const eqArr = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);
if (
  back.sha256 !== payload.sha256 ||
  back.meshstepVersion !== payload.meshstepVersion ||
  !eqArr(back.positions, payload.positions) ||
  !eqArr(back.indices, payload.indices) ||
  !eqArr(back.faceOfTri, payload.faceOfTri) ||
  !eqArr(back.solidOfTri, payload.solidOfTri) ||
  !eqArr(back.faceEntityIds, payload.faceEntityIds) ||
  !eqArr(back.featureEdges, payload.featureEdges) ||
  !eqArr(back.featureEdgeFaces, payload.featureEdgeFaces) ||
  (payload.faceColorIdx === null) !== (back.faceColorIdx === null) ||
  (payload.faceColorIdx && !eqArr(back.faceColorIdx, payload.faceColorIdx)) ||
  JSON.stringify(back.faces) !== JSON.stringify(payload.faces) ||
  JSON.stringify(back.structure) !== JSON.stringify(payload.structure) ||
  JSON.stringify(back.opts) !== JSON.stringify(payload.opts)
) {
  console.error("FAIL: mesh cache round-trip mismatch");
  process.exit(1);
}

mkdirSync(OUT_DIR, { recursive: true });
writeFileSync(join(OUT_DIR, "CNCKTestHook.step"), nodeBytes);
writeFileSync(join(OUT_DIR, "CNCKTestHook.mesh"), new Uint8Array(encoded));
console.log(
  `\nwrote ${join(OUT_DIR, "CNCKTestHook.step")} (${nodeBytes.byteLength} B)` +
    `\nwrote ${join(OUT_DIR, "CNCKTestHook.mesh")} (${encoded.byteLength} B) — round-trip OK`
);
