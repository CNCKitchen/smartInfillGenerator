# Smart Infill Generator

Web tool: load a 3D-printable model, define loads and constraints by picking
surfaces, run a structural analysis **entirely in the browser**, and export
slicer-ready modifier geometry with optimized per-region infill densities.

- [DESIGN.md](DESIGN.md) — full design decisions and build plan
- [PHASE1_RESULTS.md](PHASE1_RESULTS.md) — solver engine benchmarks/validation

## Repo layout

```
crates/sig-core   Rust engine: STL, winding-number voxelization, segmentation,
                  matrix-free multigrid FEA (mixed precision), BC attachment,
                  rigid-body-mode checks
crates/sig-wasm   wasm-bindgen API consumed by the web worker
web/              Vite + React + three.js app
```

## Development

Prereqs: Rust (GNU host on Windows works), Node 18+, wasm-pack (`npm i -g wasm-pack`).

```sh
# engine tests
cargo test -p sig-core

# native benchmark
cargo run --release --bin bench

# rebuild wasm after engine changes
wasm-pack build crates/sig-wasm --target web --out-dir ../../web/src/wasm

# wasm API smoke test (Node)
node smoke-wasm.mjs

# run the app
cd web && npm install && npm run dev
```

## Current state

- Phase 1 (engine) and Phase 2 (setup UI: import, surface picking, loads/BCs,
  constraint check with animated rigid-body modes, solve + deformed view) done.
- Next: Phase 3 — SIMP density optimization, bins, comparison card.
