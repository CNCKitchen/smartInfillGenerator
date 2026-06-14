// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Builds BOTH wasm modules:
//   src/wasm     — single-threaded + simd128 (stable toolchain). Fallback for
//                  hosts without cross-origin isolation.
//   public/wasm-mt — threaded (rayon via wasm-bindgen-rayon) + simd128.
//                  Needs nightly + build-std (std must be rebuilt with
//                  atomics) and a cross-origin-isolated page at runtime.
//                  Served as a STATIC asset (never bundled): the pool
//                  workers re-import the glue by relative URL (no-bundler).
// The worker picks at runtime via `self.crossOriginIsolated`.
//
// Usage: node scripts/build-wasm.mjs [st|mt]   (default: both)

import { execSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
const crate = path.resolve(here, "../../crates/sig-wasm");

function run(cmd, env = {}) {
  console.log(`\n> ${cmd}`);
  execSync(cmd, {
    cwd: crate,
    stdio: "inherit",
    env: { ...process.env, ...env },
  });
}

const which = process.argv[2] ?? "both";

if (which === "st" || which === "both") {
  // simd128 comes from .cargo/config.toml; keep RUSTFLAGS unset so the
  // config applies.
  const env = { ...process.env };
  delete env.RUSTFLAGS;
  console.log("\n=== single-threaded build -> web/src/wasm ===");
  execSync("wasm-pack build --target web --out-dir ../../web/src/wasm -- --features step", {
    cwd: crate,
    stdio: "inherit",
    env,
  });
}

if (which === "mt" || which === "both") {
  console.log("\n=== threaded build -> web/public/wasm-mt ===");
  run(
    "wasm-pack build --target web --out-dir ../../web/public/wasm-mt -- --features parallel,step -Z build-std=panic_abort,std",
    {
      RUSTUP_TOOLCHAIN: "nightly",
      // RUSTFLAGS overrides .cargo/config.toml, so simd128 must be repeated.
      RUSTFLAGS:
        "-C target-feature=+simd128,+atomics,+bulk-memory,+mutable-globals " +
        "-C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=4294967296 " +
        "-C link-arg=--export=__heap_base -C link-arg=--export=__wasm_init_tls " +
        "-C link-arg=--export=__tls_size -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base",
    }
  );
}
