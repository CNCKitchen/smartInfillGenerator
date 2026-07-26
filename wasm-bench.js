// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>
//
// Phase-1 WASM benchmark harness (Node >= 18).
// Times the same scenarios as the native bench in single-threaded WASM.
// Usage: node wasm-bench.js [label]
const fs = require("fs");
const path = require("path");

const wasmPath = path.join(
  __dirname,
  "target",
  "wasm32-unknown-unknown",
  "release",
  "filasim_wasm.wasm"
);
const bytes = fs.readFileSync(wasmPath);
const mod = new WebAssembly.Module(bytes);

// The module carries wasm-bindgen imports for the Model API; the C-ABI bench
// exports never call back into JS, so throwing stubs satisfy instantiation.
const imports = {};
for (const im of WebAssembly.Module.imports(mod)) {
  imports[im.module] = imports[im.module] || {};
  imports[im.module][im.name] =
    im.kind === "function"
      ? () => {
          throw new Error(`unexpected JS call: ${im.module}.${im.name}`);
        }
      : undefined;
}
const inst = new WebAssembly.Instance(mod, imports);
const e = inst.exports;

function time(label, fn) {
  const t0 = performance.now();
  const out = fn();
  const dt = (performance.now() - t0) / 1000;
  console.log(`${label}: ${dt.toFixed(2)} s  (result ${out})`);
  return dt;
}

const label = process.argv[2] || "unlabeled build";
console.log(`wasm module: ${(bytes.length / 1024).toFixed(0)} KiB, single thread, ${label}`);
time("voxelize sphere h=0.5 (1.05M cells)", () => e.bench_voxelize(0.5));
time("voxelize sphere h=0.3 (4.66M cells)", () => e.bench_voxelize(0.3));
time("solve 128x32x32 (0.13M cells) ratio", () => e.bench_solve(128, 32, 32, 0.5).toFixed(4));
time("solve 256x64x64 (1.05M cells) ratio", () => e.bench_solve(256, 64, 64, 0.25).toFixed(4));
// Sparse thin-shell cases: a mostly-dead node grid with walls too thin to
// coarsen through — what a real part looks like, and what a solid box hides.
time("shell 128x64x64 w2 (0.52M cells) max_u", () =>
  e.bench_solve_shell(128, 64, 64, 2, 0.25).toFixed(5));
time("shell 96x96x24 w2 (0.22M cells) max_u", () =>
  e.bench_solve_shell(96, 96, 24, 2, 0.25).toFixed(5));
