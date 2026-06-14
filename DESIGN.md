# InFEAll — Design Document

*Resolved via design interview, 2026-06-10. Named **InFEAll** on 2026-06-12 (working name
was "Smart Infill Generator"); the GitHub repo / deploy path still carry the old slug.*

## 1. Product definition

A **web-based tool** that takes a 3D-printable model, lets the user define loads and
constraints by clicking surfaces, runs a structural analysis + density optimization
**entirely in the browser**, and exports slicer-ready files where different regions of
the part get different sparse-infill densities — same stiffness target, less plastic
and print time.

Reference points:
- [Strecs3D](https://github.com/tomohiron907/Strecs3D) (BSD-3) — does region
  segmentation + 3MF export but requires *external* FEA results (VTU). We own the whole
  pipeline including analysis and the density–stiffness iteration.
- CNC Kitchen video ["Load dependent Infill placement: Smart Infill for FDM 3D prints!"](https://www.youtube.com/watch?v=q0YsC53mFvY)
  — the manual version of this workflow.

## 2. Resolved decisions (interview outcomes)

| # | Topic | Decision |
|---|-------|----------|
| 1 | Architecture | **All compute in-browser (WASM).** Zero hosting cost, models never leave the machine, works offline. No server compute path in v1. |
| 2 | Discretization | **Voxel grid** from **fast winding-number voxelization** (robust to triangle soup: holes, self-intersections, non-manifold). Matrix-free FEA with geometric multigrid. No tet meshing. |
| 3 | Input formats | **STL + 3MF in v1. STEP added 2026-06 via the `truck` CAD kernel** (Apache-2.0, pure-Rust → wasm-clean; OpenCASCADE was rejected — LGPL breaks the commercial-exception model, see #14). truck parses the BREP exactly and tessellates it; BREP faces are preserved as one-click selectable surfaces (the segmentation "CAD faces" source). **Known truck limitation:** its tessellation can TWIST trimmed periodic faces (cylinders) and emits developable-surface slivers — display artifacts only (the mesh is voxelized, so analysis is unaffected). We mitigate the slivers with longest-edge/aspect refinement, but the twist made it unshippable, so **STEP import is DEACTIVATED in the build as of 2026-06** — all code stays behind the `step` cargo feature; re-enable via `web/scripts/build-wasm.mjs` (add `step` to `--features`) + restore the `.step/.stp` accept lists in the UI. See §9. |
| 4 | Surface selection | **Auto-segmentation** (region-growing across edges with dihedral angle < ~30°, slider-adjustable) makes CAD-derived patches one-click selectable. **Brush/lasso + click-to-grow fallback** for organic meshes. |
| 5 | Loads & BCs (v1) | Fixed support, **elastic support** (Winkler foundation, bedding modulus k in N/mm³, σ = k·u — area-consistent axis springs per node; added 2026-06 because rigid Fixed patches artificially stiffen the part and produce edge stress singularities), **displacement support** (pin any subset of the global X/Y/Z axes to zero via stiff axis penalty springs — a roller/slider; `[true;3]` ≈ Fixed; added 2026-06), surface force (total N over patch, defined as **X/Y/Z components OR a direction + magnitude**; the direction defaults to the selection's area-weighted average normal and is re-aimable by clicking a triangle on the model), pressure, gravity/self-weight, **frictionless support** (renamed 2026-06 from "slide"). *Note: frictionless on arbitrary (non-axis-aligned) patches via penalty/transformed constraints along averaged patch normal.* |
| 6 | Under-constraint check | Pre-solve: rank test of the 6 rigid-body modes against the constraint set + connected-component (floating island) check. On failure: **block the run and animate the offending rigid-body motion** so the user sees what's unconstrained. |
| 7 | Material model | **Walls + infill core.** Boundary voxels get solid-material skin stiffness (wall count × line width; defaults 2 × 0.45 mm, plus top/bottom shells). Interior voxels get per-pattern Gibson-Ashby law **E(ρ) = E₀ · c · ρⁿ**. |
| 8 | Optimization | **Continuous SIMP-style compliance minimization** under mass constraint using the *physical* E(ρ) (no artificial penalization — graded infill is the one case where intermediate density is printable). Optimality-criteria updates, ~50–100 multigrid solves. Then discretize to bins → **final verification solve** with binned densities + walls → report. |
| 9 | User control | **Infill-budget slider** ("X %" = target MEAN INTERIOR infill density, 10–70 — same scale as a slicer's uniform infill setting; the solid skin comes on top; revised 2026-06 from a total-mass % so low values make sense and the reference comparison is honest) + **comparison card**: "vs X% uniform infill at the same weight: +Y% stiffer", stiffness retained vs solid (%), mass, max displacement. **Goal "match uniform stiffness"** (2026-06, the dual problem): lightest design as stiff as a uniform X% print — one uniform solve sets the target compliance, then a guarded secant on the budget (warm-started passes, ≤5) lands the BINNED design within 2%; card leads with "same stiffness as X% uniform: −N% weight" (measured −28% on the smoke beam at 35%). v1.x: "solve for target displacement" (same mechanism, displacement target). |
| 10 | Density bins | **3 bins by default, values auto-placed** (revised 2026-06): the bottom level is PINNED at the 10 % printability floor ("just so it prints" — gyroid top surfaces sag below ~10 %), upper levels by strain-energy-weighted 1-D clustering in stiffness space E(ρ), and assignment is anchored at the optimizer's field with a bisected mass multiplier so the budget survives quantization. Rationale: E(ρ)=c·ρⁿ with n>1 is convex → stiffness per gram grows with density (the SIMP bang-bang argument), so load-bearing levels belong high, not at the histogram mean. Measured on the cantilever fixture: +15.2 % vs uniform at equal mass (was +13.9 % with plain density-space k-means). Cap 70 % (+ "consider solid here" flag for capped hot spots). Floor/cap and a manual level list are user-editable in ⚙ Settings (manual levels let calibrated densities be used verbatim; the mass-true assignment works for any level set). **Binary mode** (2026-06): interior is either the binary floor (default 5 %, printability) or 100 % solid — the optimizer runs SIMP-penalized (p = 3) so the field converges to black/white before quantization, while verification uses the calibrated pattern law (exact at both endpoints); export can pin the dense regions via per-modifier `sparse_infill_pattern` (rectilinear/concentric; deliberately NOT object-level `internal_solid_infill_pattern` — newer Bambu Studio renames its `rectilinear` value to `zig-zag` and warns on every load). Measured on the smoke beam at 30 %: +40 % stiffer than uniform at equal mass. Part's own infill setting = lowest bin; modifiers = higher bins. |
| 11 | Slicer output | **OrcaSlicer project 3MF + Bambu Studio** (shared dialect, pinned from sample — see §5) and **PrusaSlicer flavor** (`Slic3r_PE_model_config`). **Per-bin STL export always available** as universal fallback. Cura deferred. |
| 12 | Infill patterns | Calibrated E(ρ) for **gyroid (default), cubic, grid**. All other patterns: generic Gibson-Ashby fallback + warning. Grid's anisotropy documented as limitation. |
| 13 | Validation bar | **Solver unit tests vs analytic solutions (CI) + golden comparisons vs established FEA (CalculiX/Fusion) on ~5 representative parts.** Physical testing is post-launch content, not a release gate. |
| 14 | Source posture | **REVISED 2026-06: Open source, AGPL-3.0-only, dual-licensed.** Code: AGPL (network copyleft closes the hosted-fork hole; GPL alone would not). Copyright stays with Stefan via CLA (CONTRIBUTING.md) → commercial exceptions sellable to slicer/printer/CAD vendors (COMMERCIAL.md). Name/logo trademarked, NOT AGPL. Measured calibration data licensed separately (the verified-materials business must stay unforkable). **Standing rule: no third-party (A)GPL/LGPL/SSPL/BSL/NC code in the core, ever** — it would legally break the commercial-exception model; allowed: MIT/Apache/BSD/ISC/Zlib/CC0/MPL-2.0 (enforced via deny.toml + CONTRIBUTING.md). |

## 3. Engineering decisions (made during design, not interview-blocking)

- **Units:** internal system mm–N–MPa (consistent; stresses fall out in MPa, mass via
  tonne/mm³ → displayed in g). STL is unitless → assume mm with import-dialog override (inch/cm).
- **Stack:** TypeScript + React + three.js (react-three-fiber) UI. **Rust → WASM core**
  (one crate: STL/3MF parse, winding-number voxelization, segmentation, FEA, SIMP,
  marching cubes, 3MF writers). WASM threads (SharedArrayBuffer → site needs COOP/COEP
  headers) + SIMD. Zip/unzip via `fflate` (MIT) or Rust `zip` crate. No GPL anywhere.
- **Visual design (2026-06, "Werkbank"):** software styled as a measuring instrument —
  light warm-gray chassis, recessed input wells, CNC-Kitchen-orange accent, DRO-style
  result readouts, machine status strip. Layout: top bar (part + Export), caliper-scale
  step rail (1 Model … 6 Export, orange carriage on the active station), one panel
  showing only the active step, dominant viewport (view modes top-center, section plane
  bottom-left), results dock right, telemetry strip bottom. Result review happens ON the
  viewport (2026-06): the deformed view is labeled "Results"; the field picker floats
  under the view tabs, deflection playback bottom-center, and the legend hosts the
  click-to-edit color scale, the min/max-marker toggle, and a click-to-edit
  exaggeration factor. Type: Barlow / Barlow Semi
  Condensed / B612 Mono — all SIL OFL 1.1, self-hosted under `web/public/fonts/` with
  their licenses. Rejected drafts (drawing-office light, operator dark) kept in
  `design-drafts/` for reference.
- **Solver:** 8-node hex elements, matrix-free CG preconditioned by geometric multigrid;
  identical-element stiffness scaled per-voxel by E(ρ) — the standard topology-optimization
  formulation (cf. the 88-line/PolyTop lineage, all permissively published math).
- **Modifier mesh generation:** per-bin indicator field → marching cubes → Taubin
  smoothing (no shrinkage) → **min-region cleanup** (absorb slivers below ~ a few hundred
  voxels into the neighboring bin) → dilate by ~half a voxel so regions overlap slightly
  (no coplanar z-fighting, no uncovered slivers). Modifiers exported **nested/overlapping,
  ordered low→high density** — later modifiers win in Orca/Prusa, so denser regions
  override sparser ones; gaps are impossible by construction.
- **Performance budget:** default grid auto-sized to ~1–2 M active cells (device-memory
  aware); resolution presets Preview / Normal / Fine. Target: full optimize < ~60 s on a
  mid desktop at Normal. Warn when thin features span < 3 cells at chosen resolution.
- **As-printed analysis (2026-06):** Verify can solve the part AS PRINTED — skin
  (perimeters × line width) at 100%, interior at a uniform infill ratio through the
  calibrated pattern law: the same `evaluate` path the optimizer's baselines use, exposed
  as `solve_printed` with stress/SF on the homogenized eps (min SF, mass at the print
  settings and deflection feed the results dock). This makes the tool a general FDM-FEA
  whose accuracy IS the accuracy of the measured E(ρ) calibration. Voxel size optionally
  snaps to wall/k (`pick_voxel_size`) so the skin is an exact integer number of cell
  layers; hard 4M-cell cap, snap abandoned when even k = 1 would exceed it.
- **Composite skin (2026-06, toggleable):** `classify_cells` measures the wall band in
  FRACTIONAL cell layers — a cell the band only partially covers stays a design cell and
  records its covered fraction f; `build_eps` blends its stiffness Voigt-style
  (f + (1−f)·E_infill(ρ)), the same homogenization move as the infill law, applied at the
  surface. Surface cells exposed on several sides count the overlapping slabs (a convex
  corner at wall/h = 0.5 is 7/8 skin). Consequences: walls THINNER than a voxel stay
  representable (large parts no longer need h ≤ wall — the snap becomes an accuracy
  nicety), mass and "mean infill" weight composite cells by their infill share (1−f), the
  optimizer's sensitivities scale by (1−f), and modifier regions may reach the surface
  cells behind a sub-voxel wall (correct: the slicer prints perimeters there regardless and
  the modifier only sets the infill behind them). Validated against the composite-beam
  closed form at h = 1 mm / wall = 0.45 mm (legacy is ~30+% too stiff there, composite
  within ~10%). The checkbox lives in step 3 Properties (default ON); OFF restores the
  legacy whole-layer skin (round(wall/h), min 1 layer) — kept for comparison "for now".
  The dock's "skin resolution" shows fractional layers with a "composite" tag. Trade-off
  stated: surface stress/SF readouts smear over the cell — deflection/stiffness is the
  trustworthy output (unchanged advisory framing).
- **Smoothed stress display (2026-06, toggleable):** result fields are recovered to the
  grid NODES (volume-averaged over adjacent solid cells — ZZ-style; cell centers are the
  superconvergent points, the staircase checkerboard lives between them) and evaluated AT
  the display surface: trilinear interpolation at STL vertices (weights renormalized over
  valid nodes, nearest-cell fallback), exact nodal values on the voxel hull (its vertices
  ARE nodes → smooth shading). Applies to every cell field including SF, so the dock's
  min-SF follows the active mode. Pure post-processing — the solution is untouched and
  toggling re-fetches fields only. Checkbox "smoothed (nodal average)" lives in the
  results legend (default ON); OFF restores flat per-cell painting. NOT fixed by this:
  re-entrant-corner singularities (peaks there never converge — advisory framing stands).
  Considered and deferred: snapping boundary nodes onto the STL (body-fitted voxels) —
  legitimate isoparametric FEM but it breaks the one-KE-scaled matrix-free/SIMD fast path
  for boundary cells and needs heavy element-quality guards; revisit only if surface SF
  noise still bites after recovery.
- **Element density plot (2026-06):** the Mesh view's "color skin cells" tint was replaced
  by an "element density" plot — each cell colored 0–1 through the density ramp (skin = 1,
  interior = the print-settings infill ratio, or the OPTIMIZED per-cell density once an
  optimization result exists; composite surface cells blend by wall fraction). Works with
  the voxel-true section; legend shows the ramp and what the interior value means.
- **Hover value probe (2026-06):** whenever a contour legend is on screen (result fields,
  density/regions views, mesh-view element density) the cursor carries a DRO-style readout
  of the value under it — barycentric interpolation on the displayed (possibly deformed)
  triangle of the active surface (STL or voxel hull), formatted per field (mm/µm, MPa,
  SF ×, %). Implemented as a raycast in the viewer; off whenever no legend is shown.
- **Print orientation (2026-06, Model step):** "Place on face" (click the surface the part
  prints on — its normal rotates to −Z) and ⟳X/⟳Y/⟳Z 90° buttons. Implemented as a rigid
  transform in the engine (`Model::transform`, 3×3 + translation) applied to BOTH the
  working and original meshes — exports carry the orientation; segmentation patches and BC
  triangle selections are index-based and survive; grid/results drop. The part re-seats on
  the plate (z-min → 0) after every transform. Z is the print direction: the layer-adhesion
  SF reads σzz, so orientation is a physics input, not cosmetics. Loads keep their world
  directions (documented in the panel).
- **Symmetry constraint (2026-06, Optimize step):** optional planar symmetry for the
  optimization. The plane uses the SAME combined gizmo as the section plane (translate
  along the normal + two rotation rings — arbitrary plane orientation), plus ⊥X/⊥Y/⊥Z
  align buttons and "Center" on the bbox; state is n·p = c, any normal. The plane is an
  EDITING AID: visible only while the Optimize step is active, nothing is running, and a
  setup-ish view is shown (hidden in deformed/density/regions views and during the run).
  Engine: design cells are mirror-paired by reflecting cell centers (`build_mirror_pairs`;
  nearest-cell, exact involution when the plane sits on a grid plane); each OC iteration
  averages paired sensitivities and densities, and the FINAL field is projected after
  filtering, so the output (and the bins/regions/exports built from it) is exactly
  mirror-symmetric. Cells whose mirror lands outside the part/in skin stay free.
  Asymmetric loads with symmetry ON are allowed — the result is then
  symmetric-but-suboptimal by design (the usual reason: two mirrored load cases, one
  printed part).
- **Minimum member size (2026-06, Optimize step):** a printability length-scale
  control, like the "minimum member size" knob in commercial topology optimizers.
  The optimizer's density filter (the anti-checkerboard/anti-sliver smoother) is the
  mechanism: a linear conic filter of radius `r` suppresses members narrower than
  ~its diameter `2r`, so the user's minimum member size `d` (mm) maps to
  `radius_cells = clamp(d / (2·h), 1.6, 8)` (`filter_radius_cells` in simp.rs). The
  key fix is that the radius was previously a fixed **1.6 cells** — a mesh-relative
  size, so refining the mesh shrank the protected feature and fine meshes went thin;
  expressing it in mm makes it **mesh-independent**. The 1.6-cell floor preserves the
  prior numerical behavior (`d = 0`/off ≡ before, and coarse meshes where `2r < 1.6`
  cells are unchanged); the 8-cell cap bounds the explicit filter's `(2r+1)³` stencil
  cost (the filter build uses a dense cell→slot array so even the capped radius stays
  cheap). **Default `d` = 2× line width** (a true smallest printable rib), exposed as
  an editable mm value with an "auto" reset; the panel warns when a fine mesh hits the
  8-cell cap (enforced size then ≈ `16·h`). This is **advisory**, not a hard
  guarantee — members below the size blur below the bin threshold and drop out, the
  same honest framing as the tool's other approximations. Heaviside/robust
  (eroded–dilated) projection would give a near-hard guarantee but pushes the field
  black/white, which fights graded infill's whole point (intermediate densities are
  printable); it stays a noted future lever for **binary** mode, alongside a
  PDE/Helmholtz `O(n)` filter for cheap large length scales. Applies to both graded
  and binary passes (same filter), and is constant across the stiffness-match secant.
- **Directional skin: top/bottom shells (2026-06):** `classify_cells` models the printed
  shell structure the way a slicer builds it. WALLS (perimeters × line width) are an
  IN-PLANE band from each layer's outline (per-slice 2D BFS — no leaking through
  top/bottom faces); TOP/BOTTOM SHELLS (layers × layer height, "Top/bottom layers" +
  "Layer height" in Properties, defaults 5 × 0.2 mm) are a VERTICAL band from up/down-
  facing surfaces via per-column contiguous solid runs (internal cavities get shells
  above/below, like sliced parts). 0 layers = open-top showpieces: infill runs to the
  surface, and the exports say so. Bands combine exactly (opposite slabs add and clamp;
  orthogonal bands overlap independently) and reuse the composite-skin fraction machinery
  unchanged. Exports carry the assumed counts: Orca/Bambu `top_shell_layers` /
  `bottom_shell_layers`, Prusa `top_solid_layers`/`bottom_solid_layers` (object level,
  next to wall_loops/perimeters). Layer height itself is NOT exported — it's a global
  process choice; like line width, the user matches it to their profile.
- **Cut-cell convention — Finite-Cell occupancy (2026-06):** every boundary cell carries a
  3×3×3-supersampled OCCUPANCY fraction in `grid.scale` — the share of the cell actually
  inside the STL. The occupancy also decides the cell SET (Finite-Cell / ersatz-material):
  a cell joins the solid when occupancy ≥ `BOUNDARY_FLOOR` (0.15), which **includes cells
  whose center is outside but the surface cuts** (so the part never protrudes past its mesh
  — the original complaint) and **drops sub-floor slivers** (the small-cut-cell conditioning
  / false-alarm guard). Occupancy scales stiffness (`build_eps` and the plain solve multiply
  by it), mass (all dock masses occupancy-weighted), the optimizer's infill weights, the
  element-density plot, and the hull display (which now encloses the part). Interior cells
  stay exactly 1, so exact-fitting test grids are unchanged.
  - **Why this convention (decided by benchmark, not guess — `tests/meshbench.rs`):** an
    earlier interim version (2026-06, "center-occ") kept the center-inside test for the SET
    and only derated center-inside boundary cells — which biases volume/mass LOW (one-sided
    derating). An 8-case harness compared five conventions (center-full, center-occ, inflate-
    derate, inflate+floor, majority-50%) against analytic/textbook truth: sphere & rotated-box
    volume, grid-phase robustness, rotated-square cantilever stiffness, Kirsch plate-with-hole
    stress, solid round cantilever, thin-walled tube, and a shoulder-fillet Kt (Betancur et al.,
    Tecciencia 12(23) 2017, D/d=1.5). Findings: **inflate-derate + 0.15 floor wins on every
    axis** — volume bias ≈0 (vs center-occ −5 to −13% on thin walls), lowest phase wobble
    (CoV 0.009% vs ~0.3%), accurate stiffness, AND most-accurate peak stress on curved features
    (Kt within ~1–3% on the fillet where binary conventions over-read 12–28%, because occupancy
    derating tempers the staircase stress spikes). The floor removes the lone pure-inflate
    coarse-mesh sliver false-alarm (min-SF dip) while costing ~1% volume. The harness stays in
    the tree (`cargo test -p sig-core --test meshbench -- --ignored --nocapture`) so any future
    change is one command from re-validation.
- **Custom analysis resolution (2026-06):** the Preview/Normal/Fine presets (~100k/300k/1M
  cells) gained a "Custom…" option where the user sets the CELL SIZE h in mm (seeded from
  the current grid); the panel shows the implied cell count and warns when it's past the
  4M cap (engine coarsens to fit), absurdly coarse, or coarser than the wall.
- **Mesh view (2026-06):** the STL stays visible as a transparent overlay on the voxel
  hull, so the discretization quality is visible at a glance. Force arrows are solid
  shaded glyphs with a value label ("12 N") — an ArrowHelper's line shaft vanished when
  viewed end-on, leaving an unexplainable floating dot above the part.
- **Post-optimize wait (2026-06):** after convergence the pipeline runs the binned
  verification solve plus uniform and solid REFERENCE solves (comparison card) and region
  extraction. The two reference solves now run at a relaxed tolerance (max(tol, 5e-4) —
  compliance converges much faster than the residual; the solver cache doesn't key on tol,
  so warm starts survive), which removes most of the silent wait between "converged" and
  results.
- **Stop/cancel (2026-06):** running solves and optimizations are cancellable. The worker
  is blocked inside wasm, so a postMessage can never arrive mid-call — instead the UI
  thread sets a SharedArrayBuffer flag (available because the site already ships COOP/COEP
  for threaded wasm; without isolation the Stop button hides). The wasm side installs a
  thread-local checker (`sig_core::cancel`); the MGCG loop polls it every CG iteration and
  the SIMP loop every outer iteration, surfacing a `Cancelled` error ("■ Stop" button in
  the busy chip → "Solve/Optimization stopped." notice, no error toast). The partial CG
  iterate is kept as a warm start for the next run; the flag re-arms at the start of every
  solve/optimize op. The Mesh view exposes the model: a skin-cell tint
  (legend checkbox) and a VOXEL-TRUE section — cells on the far side of the plane drop
  out entirely (`surface_mesh_where`, plane in three.js normal·p + c ≥ 0 convention,
  recut debounced while the gizmo drags) so the interior cells and the modeled wall
  thickness are inspectable instead of a hollow planar slice. Stated approximations:
  nominal skin thickness exact only on flat faces (voxel staircase on curves), ONE
  isotropic skin thickness (real top/bottom shells are layers × layer height — not
  modeled separately yet), homogenized infill, stiffness isotropic (strength anisotropy
  IS modeled via the SF variants). Print properties (perimeters, line width, pattern,
  infill %) live in step 3 "Properties", shared by verify, optimizer and export — no
  duplicates.
- **Materials:** presets PLA, PETG, ABS, ASA (E₀, ν, density, tensile strength σₜ, layer
  adhesion σₜᶻ), user-editable. Safety factors (2026-06): three fields — "material"
  (σₜ·rel(ρ)/σᵥM), "layer adhesion" (σₜᶻ·rel(ρ) vs TENSION σzz across the layers, Z-up
  build direction; compression cannot delaminate → SF 99), and the default "worst case"
  = per-cell min of both (the results dock states which limit governs). Graded infill's
  allowables scale with the same Gibson-Ashby factor as its stiffness; inverted colormap,
  red = critical low. ADVISORY readouts: STRENGTH anisotropy is modeled this way, but
  stiffness anisotropy and shear-mode delamination are not, and none of it is a certified
  safety factor.
- **Project persistence:** single JSON project file (embedded mesh + setup) download/load;
  auto-save to IndexedDB.
- **Out of scope v1:** assemblies/multi-body, thermal/dynamic loads, mobile browsers.
  Print-orientation stiffness anisotropy was out of scope here until 2026-06; now a
  planned v1.x item (transverse isotropy, see §9) — toolpath-direction in-plane
  orthotropy remains out of scope permanently (pre-slice tool, no toolpaths).

## 4. Pipeline

```
Import (STL/3MF) ─► Segmentation (dihedral region-growing)
      │                     │
      ▼                     ▼
 Winding-number        Surface picking UI ─► Loads/BCs (N, MPa, g)
 voxelization                │
      │                      ▼
      ▼              Constraint sanity: RBM rank check + islands ─► block+animate if bad
 Voxel model (skin/core tagged)
      │
      ▼
 SIMP loop: [multigrid solve → OC density update] × ~50–100   (infill budget from slider)
      │
      ▼
 Volume-weighted 1-D clustering → N bins (floor/cap)
      │
      ▼
 Verification solve (binned ρ + walls) ─► comparison card
      │
      ▼
 Marching cubes per bin → smooth → cleanup → dilate
      │
      ├─► Orca/Bambu project 3MF        (part + modifier_parts, §5)
      ├─► PrusaSlicer 3MF               (Slic3r_PE_model_config)
      └─► per-bin STLs (universal)
```

## 5. Orca/Bambu 3MF output spec (pinned from `Cube.3mf` sample)

Container (OPC zip): `[Content_Types].xml`, `_rels/.rels`,
`3D/3dmodel.model` (+ `3D/_rels/3dmodel.model.rels`), `3D/Objects/<name>_1.model`,
`Metadata/model_settings.config`, `Metadata/project_settings.config`, plate
thumbnails/json. Generated by BambuStudio 02.06 / OrcaSlicer 2.4.0-alpha.

Key facts to reproduce:
- **Production extension required** (`requiredextensions="p"`): root `3dmodel.model`
  holds one `object type="model"` composed of `<component p:path="/3D/Objects/x.model" objectid="…">`
  entries with UUIDs; actual meshes live in the Objects file. Part mesh = objectid 1,
  each modifier mesh = its own objectid.
- **`Metadata/model_settings.config`** is where modifier semantics live:
  - part: `<part id="1" subtype="normal_part">`
  - modifier: `<part id="N" subtype="modifier_part">` with metadata keys:
    `name`, `matrix` (row-major 4×4), `extruder` = `0`,
    **`sparse_infill_density` value="50%"** — and nothing else.
    **Field finding (2026-06, two rounds of real-Orca testing):** the
    sample's `wall_loops="0"` strips perimeters where a modifier touches the
    surface. Final: modifiers override **only** `sparse_infill_density`;
    walls/shells inherit from the part. The OBJECT level carries
    `sparse_infill_density` (base bin) and `wall_loops` = the in-app
    perimeter count, so the print matches the FEA skin assumption
    (perimeters × line width); line width itself stays profile-controlled.
  - `<plate>` block with `model_instance` (object_id / instance_id / identify_id) and
    `<assemble>` transform for plate placement.
- We emit modifier meshes in part-local coordinates with identity matrices (sample's
  non-identity matrices come from its reused cylinder primitive — not needed for us).
- `project_settings.config` in the sample is a full 30 KB print profile. **Open question
  for testing:** find the minimal subset Orca accepts without complaining (or template a
  lean default profile). Test matrix across Orca 2.x and Bambu Studio versions.

Extracted sample lives in `_sample_extracted/` for reference during development.

## 6. E(ρ) calibration

Law: `E(ρ) = E₀ · c · ρⁿ` per pattern (Gibson-Ashby). Initial constants from literature
(bending-dominated patterns n ≈ 2; gyroid closer to n ≈ 1.3–1.5) refined with CNC Kitchen
measured stiffness-vs-density data for gyroid/cubic/grid. Constants exposed in an advanced
panel. Fallback pattern law: conservative generic n = 2.

## 7. Top risks

1. **WASM solve performance** at useful resolutions — *mitigate first* (Phase 1 spike is
   exactly this; threads + SIMD + multigrid are the known-good recipe).
2. **Voxel resolution vs thin walls/ribs** — feature-size warning, local refinement later.
3. **Orca/Bambu 3MF compatibility drift** across versions — golden-file tests, minimal
   `project_settings` strategy, version test matrix.
4. **Segmentation quality on ugly meshes** — brush fallback guarantees a path; corpus of
   "Thingiverse horrors" as test fixtures.
5. **SharedArrayBuffer hosting constraints** (COOP/COEP headers) — host on a static CDN
   that supports custom headers; single-thread fallback mode.

## 8. Build plan

| Phase | Scope | Exit criterion |
|-------|-------|----------------|
| 1. Core spike (risk-first) ✅ **done, see PHASE1_RESULTS.md** | Rust→WASM: STL parse → voxelize → multigrid elasticity solve → displacement field | Cantilever matches analytic within tolerance; ~1 M cells solved in seconds on desktop |
| 2. Setup UI ✅ **done** | three.js viewer, drag-drop import, segmentation + brush picking, loads/BCs, RBM check + animation | A novice can set up a bracket case unaided |
| 3. Optimization ✅ **done** | SIMP loop, bins + clustering, verification solve, comparison card, density/displacement views | Mass slider → stable binned result with reported stiffness retention |
| 4. Export ✅ **done** (Prusa writer + golden FEA comparisons still open) | Marching-tetrahedra regions, Orca/Bambu writer, per-bin STLs, 3MF import | Sample-equivalent 3MF opens clean in Orca & Bambu with densities applied — **manual Orca open-tests passed** (final: modifiers override only sparse_infill_density; the part carries the user's wall_loops) |
| 5. Beta hardening | Dirty-mesh corpus, perf tuning, materials panel, docs/limitations page, project save | Public free beta |

## 9. Open items

- [ ] **Calibration data**: locate/compile CNC Kitchen stiffness-vs-density measurements for gyroid/cubic/grid (else schedule a short test series). The ⚙ Settings page already exposes c and n per pattern.
- [ ] **Transversely isotropic stiffness (v1.x)** — from FDM-FEA literature review 2026-06:
  printed solid material has E_z typically 10–25% below E_xy; pattern anisotropy (grid) on
  top. Print direction is globally Z in the grid, so a transversely isotropic material
  (E_xy, E_z, G_xz, ν's per material card) still yields ONE reference KE scaled per voxel —
  the matrix-free/SIMD fast path survives. Toolpath-direction in-plane orthotropy stays out
  of scope (we are pre-slice by design). Supersedes the §3 out-of-scope entry.
- [ ] **Offline RVE homogenization tool** — small offline Rust tool: periodic-BC FE
  homogenization of gyroid/cubic/grid unit cells per density → E(ρ) curves incl.
  anisotropy tensors. Fills calibration gaps where measurements are missing, provides
  grid's anisotropy for the item above, cross-validates measured data. Output cards are
  part of the proprietary calibration asset (license model §2.14).
- [ ] **Shear-mode layer adhesion SF** — current SF checks tension σzz only; add in-plane
  shear τ across the layer plane against a calibrated shear allowable (Mode-II analogue).
  Small change in stress.rs; removes the documented "shear-mode delamination not modeled"
  caveat.
- [ ] **Calibration validity window** — c, n per pattern are only valid near the layer
  height / line width they were measured at; state that window in the materials panel and
  the limitations docs (no extra calibration dimension yet).
- [ ] **Name/branding** for the tool.
- [ ] Minimal `project_settings.config` experiment (what Orca tolerates) — Phase 4.
- [ ] Orca/Bambu/Prusa version test matrix definition.
- [ ] **STEP tessellation quality (truck limitation, 2026-06).** truck parses
  geometry exactly but its tessellation is weak: (a) developable faces
  (cylinders/extrusions) come out as full-length slivers — mitigated by the
  aspect-aware longest-edge refinement in `mesh.rs::capped_edges`; (b) trimmed
  periodic faces can come out TWISTED (spiral strips connecting top/bottom rings
  with an angular offset; an isolated cylinder-with-cutout: 13% of edges spiral
  up to 177°, vs 0% in the CAD's own STL). Both are DISPLAY-only — the mesh is
  voxelized so analysis is correct — but the twist looks broken. truck's
  non-robust `triangulation` avoids the twist but fails outright on those faces;
  we're on the latest truck (0.4/0.6/0.3). **DEACTIVATED in the build (2026-06)**
  because the twist is too visible to ship — all code stays behind the `step`
  cargo feature (not enabled by `build-wasm.mjs`); a STEP file now hits the
  "STEP import unavailable in this build" guard in `mesh.rs::from_stl`. Re-enable
  = add `step` to the `--features` in `build-wasm.mjs` (both ST + MT builds) and
  restore the `.step/.stp` accept lists + labels in `App.tsx`/`StepPanel.tsx`.
  Real fix before re-enabling = re-tessellate analytic
  faces (cylinder/cone/plane) ourselves in their natural 2D parameter space from
  truck's correct BREP + boundary polylines (the surface vertices truck emits are
  exactly on the true surface). Dev scaffolding kept: `sig-wasm` debug exports
  `step_face_report` / `step_face_stl`, harness `stepnode_test.mjs`.
- Self-weight: engine supports it, UI hides it (negligible for desktop plastic prints; revisit for large/heavy parts).

## 10. Future simulation types (requested 2026-06, not scheduled)

- **Inertia relief + point masses** (quadcopter frames and other free-flying
  parts): no supports — applied loads (motor thrust at arms) are balanced by
  d'Alembert inertial body forces from the rigid-body acceleration computed
  off the total load and the mass distribution. Needs: lumped **mass points**
  attachable to surfaces (motors, battery, ESCs) entering both the mass
  matrix and the balancing acceleration; solver-side it is the same K but
  with a self-equilibrated RHS and the 6 rigid-body modes projected out of
  the Krylov space (our RBM machinery from the constraint check provides the
  modes). Fits the existing MGCG solver well.
- **Modal analysis + frequency optimization**: lowest eigenpairs of
  K φ = λ M φ (lumped mass incl. mass points) via matrix-free LOBPCG/Lanczos
  preconditioned by the existing multigrid; display mode shapes with the RBM
  animation path. Optimization objective "maximize first resonance
  frequency" (Rayleigh-quotient sensitivities — known to need mode-switching
  care). Useful for drone frames (prop-wash excitation) and machine parts.
