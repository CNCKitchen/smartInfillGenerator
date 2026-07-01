// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

// Pre-push gate — the one command to run before a major push. Bundles the
// engine's correctness + speed/drift checks and the WASM smoke test into a
// single green/red verdict (see DESIGN.md "pre-push gate"). Exits non-zero if
// any HARD gate fails, so it can also back a CI job later.
//
//   node scripts/preflight.mjs            # full suite
//   node scripts/preflight.mjs --no-wasm  # skip the WASM smoke test
//   node scripts/preflight.mjs --tol 0.01 # loosen the regbench drift tolerance
//
// Steps, in order (HARD gates fail the run; ADVISORY ones only report):
//   1. cargo fmt --check        (advisory) — the repo ships no rustfmt.toml, so
//                                default rustfmt disagrees with the house style;
//                                reported as a file count, never blocks.
//   2. cargo clippy             (advisory) — lints reported, never blocks.
//   3. cargo test (release)     (HARD) — all correctness anchors (validation.rs etc.)
//   4. regbench --check         (HARD) — physics drift + solve-speed report
//   5. smoke-wasm.mjs           (HARD) — the Model API the web worker uses
//
// The regbench step prints per-fixture QUALITY drift (fails past --tol) and
// INFO timings / iteration counts (never fail — read them before pushing).
//
// Promote fmt/clippy to hard gates once the tree is fmt-clean (add a
// rustfmt.toml capturing the house style) and clippy-clean.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const noWasm = args.includes("--no-wasm");
const tolIx = args.indexOf("--tol");
const tol = tolIx >= 0 ? args[tolIx + 1] : null;

const BASELINE = "crates/filasim-core/tests/regbench-baseline.tsv";

/**
 * Each step: [label, argv, opts].
 *   hard    — a non-zero exit fails the whole run (default true).
 *   quiet   — capture output instead of streaming it; on non-zero, `summary(out)`
 *             produces the one-line note to show (used to collapse the fmt dump).
 */
const steps = [
  [
    "rustfmt",
    ["cargo", "fmt", "--all", "--", "--check"],
    {
      hard: false,
      quiet: true,
      summary: (out) => {
        const n = (out.match(/^Diff in /gm) || []).length;
        return `${n} file(s) would be reformatted (house style has no committed rustfmt.toml)`;
      },
    },
  ],
  ["clippy", ["cargo", "clippy", "--workspace", "--all-targets"], { hard: false }],
  ["cargo test", ["cargo", "test", "--release", "-p", "filasim-core"], {}],
  [
    "regbench --check",
    ["cargo", "run", "--release", "-p", "filasim-core", "--bin", "regbench", "--",
     "--check", BASELINE, ...(tol ? ["--tol", tol] : [])],
    {},
  ],
];

// The WASM smoke test needs a built pkg (web/src/wasm). Gate on it rather than
// triggering the finicky wasm-mt build from here.
if (!noWasm) {
  const wasmBuilt = existsSync(join(root, "web/src/wasm/filasim_wasm.js"));
  if (wasmBuilt) {
    steps.push(["wasm smoke", ["node", "smoke-wasm.mjs"], {}]);
  } else {
    steps.push([
      "wasm smoke",
      null,
      { skip: "web/src/wasm not built — run `cd web && npm run wasm` first" },
    ]);
  }
}

const results = [];
const t0all = Date.now();

for (const [label, argv, opts = {}] of steps) {
  if (opts.skip) {
    console.log(`\n○ ${label}: SKIPPED — ${opts.skip}`);
    results.push({ label, status: "skip" });
    continue;
  }
  console.log(`\n▶ ${label}: ${argv.join(" ")}`);
  const t0 = Date.now();
  const r = spawnSync(argv[0], argv.slice(1), {
    cwd: opts.cwd ? join(root, opts.cwd) : root,
    stdio: opts.quiet ? ["ignore", "pipe", "pipe"] : "inherit",
    encoding: "utf8",
    shell: process.platform === "win32", // resolve cargo/node .cmd shims on Windows
  });
  const dt = ((Date.now() - t0) / 1000).toFixed(1);
  const hard = opts.hard !== false;
  const ok = r.status === 0;
  const status = ok ? "pass" : hard ? "FAIL" : "warn";
  if (opts.quiet && !ok && opts.summary) {
    console.log(`  ${opts.summary((r.stdout || "") + (r.stderr || ""))}`);
  }
  console.log(`  ${ok ? "✓" : "✗"} ${label} (${dt}s) — ${status}`);
  results.push({ label, status, hard });
}

// ---- summary ----
const dtAll = ((Date.now() - t0all) / 1000).toFixed(1);
console.log("\n" + "=".repeat(60));
console.log(`PRE-PUSH SUMMARY  (${dtAll}s total)`);
console.log("=".repeat(60));
for (const { label, status } of results) {
  const mark =
    status === "pass" ? "✓ pass" :
    status === "skip" ? "○ skip" :
    status === "warn" ? "! warn" : "✗ FAIL";
  console.log(`  ${mark.padEnd(8)} ${label}`);
}
const failed = results.filter((r) => r.status === "FAIL");
if (failed.length) {
  console.log(`\n✗ ${failed.length} hard gate(s) failed — do NOT push.`);
  process.exit(1);
}
console.log("\n✓ All hard gates passed. Review the regbench INFO deltas above before pushing.");
