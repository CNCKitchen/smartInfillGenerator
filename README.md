# Smart Infill Generator

Web tool: load a 3D-printable model, define loads and constraints by picking
surfaces, run a structural analysis and density optimization **entirely in the
browser**, and export an OrcaSlicer/Bambu Studio project 3MF where each region
of the part gets the infill density the loads actually demand.

- [DESIGN.md](DESIGN.md) — full design decisions and build plan
- [PHASE1_RESULTS.md](PHASE1_RESULTS.md) — solver engine benchmarks/validation

## Run it

```sh
cd web
npm install     # first time only
npm run dev     # opens on http://localhost:5173
```

Workflow in the app:

1. **Model** — drop an STL or 3MF (units mm). Adjust the surface-detection
   angle if patches come out too coarse/fine.
2. **Supports & loads** — add Fixed / Slide / Force / Pressure conditions and
   click surfaces (or brush) to assign them. Optional self-weight.
3. **Material & analysis** — PLA/PETG/ABS/ASA presets, resolution
   (Preview recommended for the first pass).
4. **Verify** — `Check setup` animates any remaining rigid-body freedom;
   `Solve once` shows the deformed shape.
5. **Optimize infill** — pick a mass budget, pattern (gyroid/cubic/grid),
   wall thickness, and number of density levels. Watch the density field
   evolve live; the card reports mass, stiffness vs solid, and the gain over
   uniform infill at equal mass.
6. **Export** — `.3mf` project (part + nested modifier volumes with
   `sparse_infill_density` set, base density on the object) for
   OrcaSlicer/Bambu Studio, or a `.zip` of modifier STLs for any slicer.

## Repo layout

```
crates/sig-core   Rust engine: STL/3MF I/O, winding-number voxelization,
                  segmentation, matrix-free multigrid FEA (mixed precision),
                  BC attachment, RBM checks, SIMP optimizer, bins, marching
                  tetrahedra, zip + Orca/Bambu 3MF writer
crates/sig-wasm   wasm-bindgen API consumed by the web worker
web/              Vite + React + three.js app
```

## Development

Prereqs: Rust (GNU host works on Windows), Node 18+, wasm-pack.

```sh
cargo test -p sig-core                                            # 23 tests
cargo run --release --bin bench                                   # native numbers
wasm-pack build crates/sig-wasm --target web --out-dir ../../web/src/wasm
node smoke-wasm.mjs                                               # full-pipeline smoke
cd web && npm run build                                           # production build
```

## State / known limitations

- Phases 1–4 of DESIGN.md are implemented and tested (engine: 23 tests; full
  pipeline smoke incl. optimization quality: binned layout measured ~14–16 %
  stiffer than uniform infill at equal mass on the cantilever fixture).
- **The exported 3MF has not yet been opened in a real OrcaSlicer install** —
  the structure mirrors the reference Cube.3mf byte-for-byte in spirit
  (production extension, model_settings.config with modifier_part +
  sparse_infill_density + wall_loops=0), but the first manual open-test is the
  next validation step.
- Single-threaded WASM: optimization at Preview resolution takes ~½–2 min
  depending on part size (live density view while it runs). wasm threads are
  the planned next multiplier (~5–8×).
- PrusaSlicer-flavor 3MF writer, golden FEA comparisons, anisotropy, and STEP
  import remain per DESIGN.md.
- Multi-mesh 3MF imports analyze the largest body only (warned in-app).
