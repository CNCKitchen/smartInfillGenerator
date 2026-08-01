// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Tessellate a STEP file through the SAME path the app uses (stepConvert.ts →
// meshStep) and dump a flat binary the native benchmark harnesses can read —
// so a Rust benchmark runs on bit-identical geometry to the browser, including
// the CAD face grouping that BC selections are made on.
//
//   node scripts/tessellate-step.mjs <input.step> <output.bin>
//
// Format ("KRSH"): u8[4] magic, u32 ntri, u32 nface, then ntri×9 f32 triangle
// soup (mm), ntri u32 dense CAD-face ids, nface f32 face areas. Consumed by
// `crates/filasim-core/src/bin/kirschbench.rs`.
//
// Also prints the CAD face table (id, type, triangle count, area, centroid,
// mean normal) — that is how you pick which dense face id is which BC surface.
//
// Node ≥23.6 (the .ts import relies on type stripping).

import { readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { convertStepBytes } = await import(
  pathToFileURL(resolve(here, "../src/engine/stepConvert.ts")).href
);

const [SRC, OUT] = process.argv.slice(2);
if (!SRC || !OUT) {
  console.error("usage: node scripts/tessellate-step.mjs <input.step> <output.bin>");
  process.exit(1);
}

const nodeBytes = readFileSync(SRC);
// Copy into a fresh ArrayBuffer — Node Buffers are views on a shared pool.
const bytes = nodeBytes.buffer.slice(
  nodeBytes.byteOffset,
  nodeBytes.byteOffset + nodeBytes.byteLength
);

const p = await convertStepBytes(bytes);
const ntri = p.indices.length / 3;
const nface = p.faceEntityIds.length;
console.log(
  `meshStep ${p.meshstepVersion}: ${ntri} tris, ${nface} CAD faces, ` +
    `units ${JSON.stringify(p.units)}, opts ${JSON.stringify(p.opts)}`
);

// Per-face area, area-weighted centroid and mean normal, from the tessellation.
const acc = Array.from({ length: nface }, () => ({
  a: 0, t: 0, x: 0, y: 0, z: 0, nx: 0, ny: 0, nz: 0,
}));
const pos = p.positions;
const idx = p.indices;
for (let t = 0; t < ntri; t++) {
  const f = acc[p.faceOfTri[t]];
  const [i, j, k] = [idx[3 * t], idx[3 * t + 1], idx[3 * t + 2]];
  const ax = pos[3 * i], ay = pos[3 * i + 1], az = pos[3 * i + 2];
  const bx = pos[3 * j], by = pos[3 * j + 1], bz = pos[3 * j + 2];
  const cx = pos[3 * k], cy = pos[3 * k + 1], cz = pos[3 * k + 2];
  const ux = bx - ax, uy = by - ay, uz = bz - az;
  const vx = cx - ax, vy = cy - ay, vz = cz - az;
  const nx = uy * vz - uz * vy, ny = uz * vx - ux * vz, nz = ux * vy - uy * vx;
  const a = Math.hypot(nx, ny, nz) / 2;
  f.a += a;
  f.t++;
  f.x += (a * (ax + bx + cx)) / 3;
  f.y += (a * (ay + by + cy)) / 3;
  f.z += (a * (az + bz + cz)) / 3;
  f.nx += nx / 2;
  f.ny += ny / 2;
  f.nz += nz / 2;
}
const fx = (v) => v.toFixed(3).padStart(9);
console.log("\ndense entity  type       tris     area      centroid                    normal");
for (let d = 0; d < nface; d++) {
  const f = acc[d];
  const info = p.faces?.[d];
  const na = Math.hypot(f.nx, f.ny, f.nz) || 1;
  console.log(
    `${String(d).padStart(4)} ${String(p.faceEntityIds[d]).padStart(6)}  ` +
      `${(info?.type ?? "?").padEnd(9)} ${String(f.t).padStart(5)} ${fx(f.a)}  ` +
      `[${fx(f.x / f.a)},${fx(f.y / f.a)},${fx(f.z / f.a)}]  ` +
      `[${fx(f.nx / na)},${fx(f.ny / na)},${fx(f.nz / na)}]` +
      (info?.radius ? `  r=${info.radius.toFixed(3)}` : "")
  );
}

const head = new DataView(new ArrayBuffer(12));
"KRSH".split("").forEach((c, i) => head.setUint8(i, c.charCodeAt(0)));
head.setUint32(4, ntri, true);
head.setUint32(8, nface, true);
const soup = new Float32Array(ntri * 9);
for (let t = 0; t < ntri; t++) {
  for (let v = 0; v < 3; v++) {
    const i = idx[3 * t + v];
    soup[9 * t + 3 * v] = pos[3 * i];
    soup[9 * t + 3 * v + 1] = pos[3 * i + 1];
    soup[9 * t + 3 * v + 2] = pos[3 * i + 2];
  }
}
const areas = Float32Array.from(acc, (f) => f.a);
const out = Buffer.concat([
  Buffer.from(head.buffer),
  Buffer.from(soup.buffer),
  Buffer.from(Uint32Array.from(p.faceOfTri).buffer),
  Buffer.from(areas.buffer),
]);
writeFileSync(OUT, out);
console.log(`\nwrote ${OUT} (${out.byteLength} B)`);
