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
2. **Boundary conditions** — add Fixed / Slide / Force / Pressure
   conditions, then arm **Pick surface** or **Brush** (below the condition
   list) to assign surfaces to the highlighted one. Orbiting is always
   active; Esc or clicking another step disarms the tool. (Self-weight
   exists in the engine but is hidden in the UI — negligible for desktop
   plastic prints.)
3. **Material & analysis** — material presets (editable in ⚙ Settings,
   persisted per browser), resolution (Preview recommended for the first
   pass). The `Mesh` view shows the actual voxel mesh the solver runs on.
4. **Verify** — `Check setup` animates any remaining rigid-body freedom;
   `Solve once` shows the deformed shape (jet colormap + value legend,
   exaggeration slider + readout, optional 0→max deflection animation).
   A **Result field** selector switches between displacement, stress
   (von Mises, σxx/σyy/σzz, τxy/τyz/τzx in MPa) and strain (equivalent +
   components) — cell-center values mapped to the surface. A **section
   plane** (gizmo to move/rotate, Flip, X/Y/Z presets) cuts through any
   view with stencil-filled caps, so the part and the analysis mesh read
   as solid at the cut.
5. **Optimize infill** — pick an infill budget (the target MEAN interior
   density, 10–70% — same scale as your slicer's uniform infill setting;
   walls/shells come on top), pattern (gyroid/cubic/grid — E(ρ) curves
   editable in ⚙ Settings), perimeters × line width (the solid skin the
   analysis assumes — the perimeter count is also written into the
   exported 3MF so the print matches; line width stays profile-controlled),
   region smoothing, and number of density levels. The evolving dense-core
   shape is shown live each iteration; the loop stops on a design-stationarity
   criterion (iteration cap is only a safety net). The card reports the
   headline comparison — **"vs X% uniform infill at the same weight: +Y%
   stiffer"** — plus stiffness vs solid, mass, and max deflection.
   The run lands in the Density view with a 25% cutaway by default; the
   slider sweeps the threshold, and the Regions view has a per-region
   visibility list.
6. **Export** — `.3mf` project (part + nested modifier volumes with
   `sparse_infill_density` set; the object carries the base density and the
   `wall_loops` count the analysis assumed — modifiers override ONLY density,
   so walls inherit cleanly) for OrcaSlicer/Bambu Studio, or a `.zip` of
   modifier STLs for any slicer.

A **"Log for nerds"** drawer (bottom-left of the viewer) streams the raw
telemetry: voxel grid stats, RBM check results, MGCG iterations/residuals,
and one line per optimizer iteration (compliance, mean infill, design change,
inner CG effort) — with live convergence charts for compliance, design
change vs the 0.005 stationarity threshold, inner CG iterations, and the
solver residual curve.

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
  perimeters where a modifier touches the surface, so no wall keys on
  modifiers. The PART (object level) carries `wall_loops` = the in-app
  perimeter count, keeping the print consistent with the analysis skin.
- Single-threaded WASM: optimization at Preview resolution takes ~½–2 min
  depending on part size (live density view while it runs). wasm threads are
  the planned next multiplier (~5–8×).
- Thin-shell jagged parts (Benchy-class) need 170–290 MGCG iterations at the
  presets — the cap is 600 (2× margin), and hitting it is a notice with the
  best-available approximation shown, never a hard error. Measured 3DBenchy
  solve times (native, 16 threads): 1.2 s / 3.1 s / 12 s at
  preview/normal/fine; single-thread browser is ~10× that.
- PrusaSlicer-flavor 3MF writer, golden FEA comparisons, anisotropy, and STEP
  import remain per DESIGN.md.
- Multi-mesh 3MF imports analyze the largest body only (warned in-app).

## License

**AGPL-3.0-only** — see [LICENSE](LICENSE). Free to use, modify, self-host,
and redistribute; if you distribute it or offer it over a network, your
version's complete source must be available under the same terms.

Want it inside closed-source software? **Commercial exceptions are
available** — see [COMMERCIAL.md](COMMERCIAL.md). Contributions require the
CLA in [CONTRIBUTING.md](CONTRIBUTING.md), which also documents the strict
dependency license policy (no third-party copyleft in the core — it would
break the dual-licensing model; enforced via [deny.toml](deny.toml)).

The project name/logo are trademarks and not covered by the code license.
