// Phase-1 WASM benchmark harness (Node >= 18).
// Times the same scenarios as the native bench, single-threaded WASM + SIMD128.
const fs = require("fs");
const path = require("path");

const wasmPath = path.join(
  __dirname,
  "target",
  "wasm32-unknown-unknown",
  "release",
  "sig_wasm.wasm"
);
const bytes = fs.readFileSync(wasmPath);
const mod = new WebAssembly.Module(bytes);
const inst = new WebAssembly.Instance(mod, {});
const e = inst.exports;

function time(label, fn) {
  const t0 = performance.now();
  const out = fn();
  const dt = (performance.now() - t0) / 1000;
  console.log(`${label}: ${dt.toFixed(2)} s  (result ${out})`);
  return dt;
}

console.log(`wasm module: ${(bytes.length / 1024).toFixed(0)} KiB, single thread + simd128`);
time("voxelize sphere h=0.5 (1.05M cells)", () => e.bench_voxelize(0.5));
time("voxelize sphere h=0.3 (4.66M cells)", () => e.bench_voxelize(0.3));
time("solve 128x32x32 (0.13M cells) ratio", () => e.bench_solve(128, 32, 32, 0.5).toFixed(4));
time("solve 256x64x64 (1.05M cells) ratio", () => e.bench_solve(256, 64, 64, 0.25).toFixed(4));
