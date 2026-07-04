# Changelog

All notable changes to filaSim are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0.0, minor releases (0.x → 0.y) may change saved-project
compatibility or simulation results; patch releases are fixes only.

## [Unreleased]

### Added
- CAD-style capped section view: the section plane closes the cut with a solid
  cap (distinct clay cut-face color) instead of exposing a hollow interior; the
  cap also covers the voxel-result surface, which previously had no cap at all.
- Result fields mapped onto the section cap: the cut face shows the volumetric
  stress/strain/SF/displacement field inside the part (new `sectionVolume`
  engine op — recovered nodal field + nodal displacements as 3D textures,
  exaggeration-aware inverse mapping, shared LUT so banding and legend ranges
  apply).
- Interior-aware result extremes: the color range now includes the solid-cell
  (interior) min/max — on skin+infill parts the true peak often sits at the
  perimeter/infill interface — and the log names the interior peak location
  when it exceeds the surface extreme.
- Keyboard camera shortcuts (slicer layout): plain 1–6 snap to top / bottom /
  front / rear / left / right, F fits the part into the current viewport
  without changing the view direction. Ctrl/⌘ + 0–6 keep working (0 = default
  isometric).
- Third "mark min / max" marker: a hollow-ring "max (interior)" mark (or
  "min (interior)" for safety factors) appears at the interior extreme
  whenever it beats the surface value by more than 2%; hidden otherwise.
  Interior extremes are taken over FULL cells only (occupancy ≈ 1) — boundary
  cut cells belong to the surface plot and their centers can lie outside the
  mesh.

### Fixed
- Section cap was invisible when the cut was viewed from the removed side
  (one-sided cap quad was back-face culled), which made a sectioned part look
  hollow.
- Section/symmetry gizmo: hovering the move arrow no longer grabs a rotation
  ring — the rings' invisible pick zones pass right through the arrow-tip
  region, so drags meant as a shift often rotated the plane instead. The
  translate arrow now has hover priority.
- Section plane now always starts through the part's bounding-box center and
  re-centers when the part is replaced or rotated (it used to spawn at the
  orbit target, which lands off-part after panning).
- Section plane opens toward the viewer: on activation the normal follows the
  dominant view axis with the near half clipped (it used to cut the far half,
  showing the intact surface), and the X/Y/Z snap buttons pick the
  camera-facing sign too.

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
