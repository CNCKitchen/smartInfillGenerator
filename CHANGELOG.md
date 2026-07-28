# Changelog

All notable changes to filaSim are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0.0, minor releases (0.x → 0.y) may change saved-project
compatibility or simulation results; patch releases are fixes only.

## [Unreleased]

### Added
- **Material Manager**: materials get the same library surface as infill
  properties — a modal (⚙ Settings → Materials, or the Properties step's
  "edit" link) with a material list, a grouped detail panel and a chart band.
  FDM and isotropic materials live in one list; a per-material process
  selector switches the kind (FDM → isotropic seeds yield from σₜ; isotropic
  → FDM reseeds printed-part defaults instead of leaking derived internals).
  The old two 14/10-column Settings tables (which forced horizontal
  scrolling) are replaced by grouped label-over-input fields. Charts, plain
  SVG in the PropertyCharts idiom: a **stress–strain curve** (the pure
  bilinear idealization — elastic slope E to σy, then slope Eₜ straight to
  the rupture strain εᵣ, which ends the x-axis; yield dot, rupture ×, dashed
  layer-adhesion level σₜᶻ for FDM, crosshair tooltip, up to 4 comparison
  overlays; σᵤ is informational, readout only), per-row-normalized property
  bars (E, σ, ρ, E/ρ, σ/ρ), FDM layer anisotropy bars (σₜ / σₜᶻ / τᶻ) and a
  numeric readout table. The stress–strain fields σᵤ/Eₜ/εᵣ now apply to FDM
  materials too (chart-only), and the FDM built-ins ship with typical
  printed-part hardening/rupture values. New store actions `duplicateMaterial` and
  `openMaterialsManager`.
- **Isotropic materials** (machined metal, cast parts, resin prints): materials
  now carry a `process` — FDM or isotropic. Built-ins added: Steel (mild,
  S235), Aluminum 6061-T6 and Resin (SLA, standard). An isotropic material has
  no build direction, so the entire print stack disappears rather than being
  special-cased: no infill/pattern/walls/shells controls, no printed-vs-solid
  toggle (always solved fully dense at E₀), no build-sim workspace, no
  orientation sweep, no print-settings optimizer, and no slicer 3MF export.
  The safety factor runs against **yield** (von Mises); the layer-adhesion
  criterion structurally cannot govern (σz = σy, τz = σy/√3) and its SF views
  are hidden. Optimization becomes classic part topology optimization (the
  existing Part Topo mode — linear E=ρ evaluation, SIMP p=3, load/support
  cells frozen), with the optimized-shape STL as the export; min member size
  defaults to 2 mm instead of 4× line width. Isotropic materials also store
  ultimate strength, tangent modulus and strain at rupture — dormant fields
  reserved for a future plasticity model. The material editor in ⚙ Settings
  gains a separate isotropic table; the Properties dropdown groups the two
  families. Existing saves are upgraded in place (legacy materials default to
  FDM; the isotropic built-ins are seeded once).
- **Reaction forces result view**: a new "Reaction forces" entry in the results
  field picker shows the resultant force each support exerts on the part —
  magnitude + X/Y/Z components plus the reaction moment about the support
  centroid in a legend table, and one arrow per support on the model (kind
  color, length ∝ magnitude with a visibility floor, name + |F| callout,
  support faces tinted). Fixed supports recover `R = K·u − f` exactly (same
  matrix-free machinery as the build-sim bed reaction, TI-blend aware); penalty
  supports (frictionless / displacement / cylindrical / elastic) report their
  exact spring force, so Σ R balances the applied loads (unit-tested). Entering
  the view sets the deformation exaggeration to 0 (undeformed, still editable)
  and leaving restores the previous value.
- **Cylindrical support**: pick a bore or shaft seat and hold it in its own
  frame — radial, tangential and axial each free or fixed. The selection is
  fitted to a cylinder (same check and CAD-exact axis as the bearing load) and
  every attached node gets stiff penalty springs along the locked local
  directions, so a full bore is held toward its own centre everywhere. Defaults
  to radial + axial fixed with tangential free — a journal bearing or a
  bolted-through hole; the axial spin stays a real free rigid-body mode that the
  pre-solve check reports instead of being quietly locked. All three fixed is a
  press fit and is identical to a three-axis displacement support. The viewport
  draws the fitted axis (with end stops when axial is held) through the support
  cones.
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

### Changed
- Support/load rows in the Boundary-conditions step are now collapsible: only
  the active (highlighted) condition shows its editor, the rest fold to their
  one-line header — a part with many conditions no longer scrolls the panel.
  Clicking a row expands it, same gesture that highlighted it before. Collapsed
  loads show their effective (load-step-aware) value — |F|, |M|, p, |a|, mass —
  in place of the triangle count; supports keep the selection size.

### Performance
- **Solver: 1.5–2.5× faster with bit-identical results.** Every quality metric in
  `regbench` is unchanged to ±0.000 %, and the MGCG iteration count is identical
  on every fixture — this is pure cost-per-iteration and hierarchy work, not a
  change to the physics or the convergence criterion. Four changes:
  - **Live-set skipping.** On a voxelized part most of the padded node grid has
    no incident solid cell and is identically zero for the whole solve (a
    3DBenchy is 18 % live, a flat plate ~25 %), yet the smoother, the transfers
    and the CG vector ops streamed all of it. They now skip wholly dead 16-node
    blocks. Provably value-preserving: the skipped writes were writing the zero
    that is already stored, and skipped terms contribute exactly 0 to the dot
    products. Worth ~35 % on shell-like parts.
  - **Element matvec reduction order.** KE is symmetric, so the 24×24 element
    product can accumulate into 24 registers instead of horizontally reducing 24
    dot products. On x86-64 AVX2 that is 1.53× faster; on wasm32 simd128 the old
    row form is 1.26× faster (128-bit lanes spill the wide accumulator), so the
    form is target-gated and both were measured.
  - **Semicoarsening.** Coarsening required ALL three axes to be splittable, so a
    single thin axis capped the hierarchy for the others — a 3-cell-thick plate
    collapsed to ONE level with no multigrid at all, leaving the coarse-grid PCG
    to do the entire solve. Each axis is now padded and halved to its own depth;
    coarse cells become bricks and their element matrix is re-integrated
    accordingly. A 1 M-cell plate goes from 1 level / 2.74 s to 5 levels / 1.93 s,
    and the 64×8×4 beam suite (previously 2 levels) is 2.5–6× faster.
  - **Live-set-aware f64 CG.** The outer mixed-precision CG's dot products, axpys
    and demote pass now use the same live set, cutting its share of a Benchy
    solve from 10 % to 3 %.
  - Measured (16 threads, native): 3DBenchy 13.5 s → 6.4 s at 300 k cells and
    68.6 s → 27.1 s at 1 M; MicHolder 12.2 s → 8.5 s; hook 1.35 s → 0.91 s;
    the `regbench` solver fixtures total 16.3 s → 6.7 s (2.42×). Single-threaded
    wasm gains ~1.2× on sparse parts (the matvec change does not apply there).
  - `solvebench` (new `filasim-core` bin) is the harness: real STLs over a
    mesh-refinement sweep with iteration counts, a live/dead DOF census, kernel
    micro-timings and a nested-start probe. `mg.rs` records the negative results.

### Fixed
- Reorienting the part (rotate, place-on-face, rescale) left the cached cylinder
  fit of a cylindrical support or a bearing load behind, so its axis glyph and
  bore-fan drew at the old pose while the part had moved. The cached fit now
  moves with the part. The CONSTRAINT and the load were never affected — the
  engine re-fits the cylinder from the current triangles on every assembly, which
  a rotated-pose invariance test now locks down.
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

[Unreleased]: https://github.com/CNCKitchen/filaSim/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/CNCKitchen/filaSim/releases/tag/v0.1.0
