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

1. **Model** — drop an STL or 3MF (units mm). Coarse tessellations are
   subdivided internally (~1/60 of the diagonal, 160k-triangle budget) so
   deformed shapes can actually show curvature; exports always carry the
   original mesh. Adjust the surface-detection angle if patches come out too
   coarse/fine. An axis gizmo (bottom-right) shows the orientation; supports
   are marked with classic FEA triangles. Parallel projection throughout.
2. **Supports & loads** — add Fixed / Slide / Force / Pressure conditions and
   click surfaces (or brush) to assign them. Clicking anywhere outside this
   section snaps the tool back to Orbit. (Self-weight exists in the engine
   but is hidden in the UI — negligible for desktop plastic prints.)
3. **Material & analysis** — material presets (editable in ⚙ Settings,
   persisted per browser), resolution (Preview recommended for the first
   pass). The `Mesh` view shows the actual voxel mesh the solver runs on.
4. **Verify** — `Check setup` animates any remaining rigid-body freedom;
   `Solve once` shows the deformed shape (jet colormap + value legend,
   exaggeration slider + readout, optional 0→max deflection animation).
5. **Optimize infill** — pick a mass budget, pattern (gyroid/cubic/grid —
   E(ρ) curves editable in ⚙ Settings), perimeters × line width (the solid
   skin the analysis assumes — analysis only, never written to the slicer),
   region smoothing, and number of density levels. The evolving dense-core
   shape is shown live each iteration; the loop stops on a design-stationarity
   criterion (iteration cap is only a safety net). The card reports mass,
   stiffness vs solid, and the gain over uniform infill at equal mass.
   Afterwards the Density view has a cutaway slider (show only material
   denser than a threshold) and the Regions view a per-region visibility
   list.
6. **Export** — `.3mf` project (part + nested modifier volumes with
   `sparse_infill_density` set, base density on the object — densities are
   the ONLY override; walls/shells inherit from your profile) for
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
cargo test -p sig-core                                            # 24 tests
cargo run --release --bin bench                                   # native numbers
wasm-pack build crates/sig-wasm --target web --out-dir ../../web/src/wasm
node smoke-wasm.mjs                                               # full-pipeline smoke
cd web && npm run build                                           # production build
```

## State / known limitations

- Phases 1–4 of DESIGN.md are implemented and tested (engine: 24 tests; full
  pipeline smoke incl. optimization quality: binned layout measured ~14–16 %
  stiffer than uniform infill at equal mass on the cantilever fixture).
- The exported 3MF **opens in a real OrcaSlicer install** (manual tests
  2026-06). Final format decision from those tests: modifiers override
  ONLY `sparse_infill_density` — the sample's `wall_loops=0` strips
  perimeters where a modifier touches the surface, and pinning a count
  would override the user's process profile, so no wall keys are written
  at all.
- Single-threaded WASM: optimization at Preview resolution takes ~½–2 min
  depending on part size (live density view while it runs). wasm threads are
  the planned next multiplier (~5–8×).
- PrusaSlicer-flavor 3MF writer, golden FEA comparisons, anisotropy, and STEP
  import remain per DESIGN.md.
- Multi-mesh 3MF imports analyze the largest body only (warned in-app).
