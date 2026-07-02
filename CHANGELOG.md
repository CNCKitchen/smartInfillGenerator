# Changelog

All notable changes to filaSim are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0.0, minor releases (0.x → 0.y) may change saved-project
compatibility or simulation results; patch releases are fixes only.

## [Unreleased]

## [0.1.0] - 2026-07-02

First versioned release. Everything below is the state of the project at this point.

### Core engine (Rust → WASM)
- Voxel FEA engine: winding-number voxelization, mixed-precision MGCG solver.
- SIMP infill topology optimization with per-step optimized evaluation for
  multi-load designs.
- FDM build simulation: inherent-strain method with sequential layer activation,
  residual print-stress fields, transversely-isotropic build shrink (in-plane vs
  through-layer), bed-peel risk as mesh-independent traction (MPa), on-bed and
  released result states, plasticity wiring.
- Modal analysis solver.
- Threaded WASM build (rayon + SharedArrayBuffer) with a single-threaded
  fallback for non-cross-origin-isolated hosts.

### Web app
- Interactive setup UI: boundary conditions, load cases and load steps,
  material properties, units input on import.
- Results viewer: true colormaps, signed von Mises, fixed value callouts,
  ortho view, isometric framed 3MF thumbnails, inherent-strain layer view,
  bed-peel heatmap on the plate.
- Build Sim workspace with live progress and warp preview on the voxel hull.
- Project save/load (`.infeall` format).

### Import / export
- STEP import via the truck CAD kernel (BREP read + tessellation).
- 3MF import and export, including colored 3MF with discrete filament bands
  for Orca/Bambu slicers.

[Unreleased]: https://github.com/CNCKitchen/smartInfillGenerator/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/CNCKitchen/smartInfillGenerator/releases/tag/v0.1.0
