# Phase 1 Results — Core Engine Spike

*2026-06-10. Machine: AMD Ryzen 7 9800X3D (8C/16T), 62 GB RAM, Windows 11.
Toolchain: Rust 1.96 (GNU host), Node 24, wasm32-unknown-unknown + simd128.*

## Exit criteria vs. measured

| Criterion (DESIGN.md §8) | Result |
|---|---|
| Cantilever matches analytic within tolerance | ✅ FE/Timoshenko ratio **0.984 → 0.991 → 0.993** at 8/16/32 elements through thickness — monotone convergence, remaining ~1 % is physical (clamped-root stiffening) |
| ~1 M cells solved in seconds on desktop | ✅ **0.88 s** for 1.05 M cells / 3.26 M DOF (8 MGCG iterations), 16 threads native |
| WASM viability | ✅ **4.95 s** for the same 1.05 M-cell solve, **single-threaded** WASM+SIMD128, 92 KiB module; identical physics (ratio 0.9843). Browser threads (Phase 2, wasm-bindgen-rayon) bring an expected 5–8× on top |

## Measured numbers

Native (16 threads, release):

```
voxelize: 16 128 tris -> 167³ = 4.66 M cells (2.42 M solid) in 0.76 s (6.1 Mcells/s)
          volume check: -0.09 % vs analytic sphere
solve:    128×32×32 = 0.13 M cells (0.42 M DOF): 0.17 s,  9 MGCG iters
solve:    256×64×64 = 1.05 M cells (3.26 M DOF): 0.88 s,  8 MGCG iters
```

WASM (Node 24, 1 thread, simd128):

```
voxelize 1.05 M cells: 1.08 s     voxelize 4.66 M cells: 5.06 s
solve    0.13 M cells: 0.71 s     solve    1.05 M cells: 4.95 s
```

MGCG iteration counts are resolution-independent (9 → 8 from 131 k to 1 M cells) —
the multigrid is doing its job; cost scales linearly with cell count.

Projection for the optimization loop (Phase 3): SIMP needs ~50–100 solves, but
warm-started re-solves after small density updates converge in a few iterations;
a 1 M-cell optimization should land around ~10–20 s threaded in-browser. Within
the < 60 s budget from DESIGN.md with margin.

## What was validated (tests, all green)

1. **Element matrix**: symmetry + all 6 rigid-body modes in the null space.
2. **Matrix-free apply == dense assembly** (f32 and f64 paths) on a mixed
   solid/void/gray grid with constraints — catches any indexing/scatter bug.
3. **MGCG == dense direct solve** to 1e-6.
4. **Uniaxial patch test exact to 1e-8** (roller BCs, consistent tractions).
5. **Cantilever vs Timoshenko** + mesh-convergence monotonicity.
6. **Winding-number robustness**: closed mesh, mesh with a hole punched in it
   (still classifies interior), fully inverted normals (|w| classification),
   degenerate/NaN triangles dropped at parse.
7. **Voxelizer volume** −0.09 % on a 16 k-tri sphere; **STL parser** roundtrip,
   binary-with-"solid"-header quirk, ASCII, dirty input.
8. **End-to-end**: STL bytes → voxelize → solve → sane deflection.

## The one real bug found (and its fix)

First implementation stored everything in f32. Refining the cantilever made it
*stiffer* (ratio 0.962 → 0.915), violating FE convergence theory. Root cause:
near equilibrium, K·u sums element forces of magnitude ~E·u that cancel to the
~10⁴× smaller applied load — f32 cancellation noise capped attainable solution
accuracy, and the cap worsens with condition number κ ∝ (L/h)². Fix: **mixed
precision** — outer CG loop and operator in f64, V-cycle preconditioner (the
bulk of the flops) stays f32. Cost ≈ +20 % time; restored textbook convergence.
This is permanently guarded by the convergence assertion in
`cantilever_matches_timoshenko_and_converges`.

## Architecture as built

```
crates/sig-core        engine library (no_std-free, zero GPL deps; rayon optional)
  mesh.rs              TriMesh + robust STL reader (binary/ASCII/dirty)
  bvh.rs               triangle BVH + fast winding numbers (Barill-style dipoles)
  voxel.rs             |w|≥0.5 voxelization
  fem.rs               hex KE (2×2×2 Gauss), diag blocks, 3×3 inverse
  mg.rs                8-color matrix-free apply (f32+f64), block-Jacobi smoother,
                       rediscretized geometric multigrid, mixed-precision MGCG
  solve.rs             padding, BC regions, force assembly, Solution queries
  bin/bench.rs         the numbers above
crates/sig-wasm        C-ABI cdylib benchmark surface (92 KiB)
wasm-bench.js          Node harness
```

Licenses in the build: rayon (MIT/Apache-2.0), crossbeam (MIT/Apache-2.0),
either (MIT/Apache-2.0). Everything else is first-party. ✅ commercial-clean.

## Carry-forward notes for Phase 2

- Browser threads: wasm-bindgen + wasm-bindgen-rayon; host needs COOP/COEP headers.
- Apply is memory-bound: per-level node-reordering or tiling is the next perf
  lever if needed; not needed for Phase-2 targets.
- `Solution.u` is exposed f32 (display); keep solver-internal f64.
- Frictionless support (penalty/transformed DOFs) intentionally not in the
  Phase-1 solver core yet — design note in DESIGN.md §2 row 5.
- Voxelizer currently classifies cell centers only; the walls+core material
  model (Phase 3) wants boundary-cell occupancy fractions — straightforward
  extension (supersample boundary cells 2×2×2).
