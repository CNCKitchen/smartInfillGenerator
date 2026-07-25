# filaSim — Design Document

*Resolved via design interview, 2026-06-10. Renamed to **filaSim** on 2026-07-01
(previously "InFEAll", named 2026-06-12; original working name "Smart Infill
Generator"). The GitHub repo / Cloudflare deploy path still carry the old slugs.*

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
| 5 | Loads & BCs (v1) | Fixed support, **elastic support** (Winkler foundation, bedding modulus k in N/mm³, σ = k·u — area-consistent axis springs per node; added 2026-06 because rigid Fixed patches artificially stiffen the part and produce edge stress singularities), **displacement support** (enforce any subset of the global X/Y/Z axes via stiff axis penalty springs — a roller/slider; `[true;3]` ≈ Fixed; added 2026-06. **2026-06: per-axis PRESCRIBED VALUE in mm** — 0 = the classic pin-to-zero, a non-zero value is an enforced motion (e.g. 1 mm in X), applied as an equivalent `k·value` penalty force so it rides the force RHS path and never invalidates the cached matrix; per-axis activate/deactivate + a value, since 0 ≠ off), surface force (total N over patch, defined as **X/Y/Z components OR a direction + magnitude**; the direction defaults to the selection's area-weighted average normal and is re-aimable by clicking a triangle on the model), pressure, gravity/self-weight, **frictionless support** (renamed 2026-06 from "slide"), **cylindrical support** (added 2026-07-25 — a cylindrical selection constrained in its OWN frame: radial / tangential / axial each free or fixed, penalty springs along the local directions of the fitted cylinder — the same fit the bearing load uses. Default radial+axial fixed, tangential free = a journal bearing / bolted-through hole; all three fixed = a press fit (≈ Fixed on a bore). Tangential-free deliberately leaves the axial spin as a real rigid-body mode for the §6 check to report rather than quietly locking it). *Note: frictionless on arbitrary (non-axis-aligned) patches via penalty/transformed constraints along averaged patch normal.* |
| 6 | Under-constraint check | Pre-solve: rank test of the 6 rigid-body modes against the constraint set + connected-component (floating island) check. On failure: **block the run and animate the offending rigid-body motion** so the user sees what's unconstrained. |
| 7 | Material model | **Walls + infill core.** Boundary voxels get solid-material skin stiffness (wall count × line width; defaults 2 × 0.45 mm, plus top/bottom shells). Interior voxels get per-pattern Gibson-Ashby law **E(ρ) = E₀ · c · ρⁿ**. |
| 8 | Optimization | **Continuous SIMP-style compliance minimization** under mass constraint using the *physical* E(ρ) (no artificial penalization — graded infill is the one case where intermediate density is printable). Optimality-criteria updates, ~50–100 multigrid solves. Then discretize to bins → **final verification solve** with binned densities + walls → report. |
| 9 | User control | **Infill-budget slider** ("X %" = target MEAN INTERIOR infill density, 10–70 — same scale as a slicer's uniform infill setting; the solid skin comes on top; revised 2026-06 from a total-mass % so low values make sense and the reference comparison is honest) + **comparison card**: "vs X% uniform infill at the same weight: +Y% stiffer", stiffness retained vs solid (%), mass, max displacement. **Goal "match uniform stiffness"** (2026-06, the dual problem): lightest design as stiff as a uniform X% print — one uniform solve sets the target compliance, then a guarded secant on the budget (warm-started passes, ≤5) lands the BINNED design within 2%; card leads with "same stiffness as X% uniform: −N% weight" (measured −28% on the smoke beam at 35%). v1.x: "solve for target displacement" (same mechanism, displacement target). |
| 10 | Density bins | **3 bins by default, values auto-placed** (revised 2026-06): the bottom level is PINNED at the 10 % printability floor ("just so it prints" — gyroid top surfaces sag below ~10 %), upper levels by strain-energy-weighted 1-D clustering in stiffness space E(ρ), and assignment is anchored at the optimizer's field with a bisected mass multiplier so the budget survives quantization. Rationale: E(ρ)=c·ρⁿ with n>1 is convex → stiffness per gram grows with density (the SIMP bang-bang argument), so load-bearing levels belong high, not at the histogram mean. Measured on the cantilever fixture: +15.2 % vs uniform at equal mass (was +13.9 % with plain density-space k-means). Cap 70 % (+ "consider solid here" flag for capped hot spots). Floor/cap and a manual level list are user-editable in ⚙ Settings (manual levels let calibrated densities be used verbatim; the mass-true assignment works for any level set). **Binary mode** (2026-06): interior is either the binary floor (default 5 %, printability) or 100 % solid — the optimizer runs SIMP-penalized (p = 3) so the field converges to black/white before quantization, while verification uses the calibrated pattern law (exact at both endpoints); export pins the pattern via per-modifier `sparse_infill_pattern` AND the object-level GENERAL `sparse_infill_pattern` (2026-06: the whole part prints in the chosen pattern, not just the modifiers); the UI offers only **rectilinear/concentric** (no profile-default). This is the SPARSE-infill pattern key — deliberately NOT object-level `internal_solid_infill_pattern` (newer Bambu Studio renames THAT key's `rectilinear` value to `zig-zag` and warns on every load; the `bambu` flavor maps rectilinear→zig-zag for the sparse key instead). Measured on the smoke beam at 30 %: +40 % stiffer than uniform at equal mass. Part's own infill setting = lowest bin; modifiers = higher bins. |
| 11 | Slicer output | **OrcaSlicer project 3MF + Bambu Studio** (shared dialect, pinned from sample — see §5) and **PrusaSlicer flavor** (`Slic3r_PE_model_config`). **Per-bin STL export always available** as universal fallback. Cura deferred. |
| 12 | Infill patterns | Calibrated E(ρ) for **gyroid (default), cubic, grid**. All other patterns: generic Gibson-Ashby fallback + warning. Grid's anisotropy documented as limitation. |
| 13 | Validation bar | **Solver unit tests vs analytic solutions (CI) + golden comparisons vs established FEA (CalculiX/Fusion) on ~5 representative parts.** Physical testing is post-launch content, not a release gate. |
| 14 | Source posture | **REVISED 2026-06: Open source, AGPL-3.0-only, dual-licensed.** Code: AGPL (network copyleft closes the hosted-fork hole; GPL alone would not). Copyright stays with Stefan via CLA (CONTRIBUTING.md) → commercial exceptions sellable to slicer/printer/CAD vendors (COMMERCIAL.md). Name/logo trademarked, NOT AGPL. Measured calibration data licensed separately (the verified-materials business must stay unforkable). **Standing rule: no third-party (A)GPL/LGPL/SSPL/BSL/NC code in the core, ever** — it would legally break the commercial-exception model; allowed: MIT/Apache/BSD/ISC/Zlib/CC0/MPL-2.0 (enforced via deny.toml + CONTRIBUTING.md). |
| 15 | Solid topology mode | **Material-removal topology optimization (2026-06), the third Optimize mode beside Graded / Binary infill (UI name "Part Topo").** Same SIMP engine, three swaps: (a) NO skin — `classify_cells` is bypassed, every solid cell is a design cell; (b) the lower bound is ersatz **void** (E_min ≈ 0, material actually removed) instead of a printability floor; (c) the eval law is **linear E = ρ** (SIMP-penalized p = 3 in the optimizer only, exact at the {void, solid} endpoints), and the output is **one watertight optimized shape** (marching-tets isosurface at ρ = 0.5 + the largest load-connected component), NOT density bins/modifiers. **Load/support patches are auto-frozen solid** (derived from the assembled fixed/spring/force nodes → incident cells, like a commercial optimizer's "keep regions") so material under a load/BC is never deleted. Kept material prints **solid (100 %)** — infill and topology stay separate features (combining them is a noted v1.x). **Self-supporting filter** (toggleable, variable overhang angle, build direction = global Z): a Langelaar-style layer-by-layer AM projection in the filter chain (`selfsupport.rs`) so the shape prints without supports; advisory (the staircase + smoothing can still nick the angle locally), default 45°. Budget knob = **retained volume fraction**; "match uniform stiffness" goal reuses the budget secant against the solid part as reference. Measured/expected: classic MBB/cantilever layouts. |

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
- **Capped section view (2026-07):** the section plane cuts like CAD, not like a clipped
  depth buffer. A stencil-buffer cap (three.js clipping-stencil technique: clipped back
  faces increment, front faces decrement, a plane quad fills where ≠ 0) closes the cut on
  every opaque surface — part mesh, mesh-view voxel hull, and the voxel-result surface.
  The quad is double-sided (the cut is viewed from the REMOVED side, where a one-sided
  quad culls away — the pre-2026-07 "hollow" look) and plain cuts use a clay tone
  (`CUT_FACE_COLOR`) clearly distinct from the part gray. In result views the cap is
  FIELD-MAPPED: `Model::section_volume` ships the recovered nodal field over the whole
  solution grid (void-adjacent nodes back-filled so the smooth surface can overhang the
  voxelization) plus the padded nodal displacements; both live as 3D textures and the cap
  fragment shader un-deforms its sample point (2 fixed-point steps of x = p − s·u(x),
  exaggeration-aware), trilinearly samples the field (manual texelFetch taps — no
  float-linear extension), and colors through the SHARED jet LUT — so banding, legend
  overrides, and |u|/component switches apply to the cut face automatically. The same
  payload reports the INTERIOR (solid-cell) extremes: the color range is widened to
  volume min/max (a skin+infill part often peaks at the perimeter/infill interface, not
  on the skin) and a log advisory names the interior peak and its location whenever it
  exceeds the surface max (or undercuts the surface min-SF). Envelope results and peel
  maps keep the plain cap (no single engine solution to slice).
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
  cheap). **Default `d` = 4× line width** (a robustly printable smallest rib), exposed as
  an editable mm value with an "auto" reset; the panel warns when a fine mesh hits the
  8-cell cap (enforced size then ≈ `16·h`). This is **advisory**, not a hard
  guarantee — members below the size blur below the bin threshold and drop out, the
  same honest framing as the tool's other approximations. Heaviside/robust
  (eroded–dilated) projection would give a near-hard guarantee but pushes the field
  black/white, which fights graded infill's whole point (intermediate densities are
  printable); it stays a noted future lever for **binary** mode, alongside a
  PDE/Helmholtz `O(n)` filter for cheap large length scales. Applies to both graded
  and binary passes (same filter), and is constant across the stiffness-match secant.
- **Solid topology mode (2026-06, decision #15) — engine plumbing:** the mode is
  `OptimizeParams::solid_mode`; almost everything is reuse. `optimize_cached` branches
  the cell split: `classify_cells` (skin band) → `build_solid_split`, which makes the
  auto-frozen cells the "skin" (always-solid, eps = 1, excluded from the design vector —
  reusing the existing skin path verbatim) and every other solid cell a design cell with
  `skin_frac = 0`. **Frozen cells** are derived inside the optimizer from the assembled
  `NodeProblem` (the `fixed` ∪ spring ∪ force nodes → their incident solid cells, one-cell
  dilated), so no plumbing of a separate keep-set is needed and every BC type is covered.
  **`retain_bc` toggle (2026-06, default on):** off ⇒ no frozen cells, the whole part is
  free to be carved (pure topology optimization, load/support regions included); the
  connected-keep then has no anchors and falls back to the largest component.
  The wasm layer sets `floor ≈ 1e-3` (void), `cap = 1`, optimizer law p = 3 / coeff 1
  (penalization → black/white), eval law linear (exp 1 / coeff 1, exact at 0 and 1). The
  bins/clustering/modifier-3MF tail is skipped; instead the continuous field is
  isosurfaced at ρ = 0.5 (the existing `extract_iso` + Taubin smooth), the **largest
  connected component touching a frozen region** is kept (floating islands dropped), and
  that single mesh is the result — visualized in the Regions/density views and exported as
  one **STL** (a single-object project 3MF is a small follow-up — the modifier-3MF writer
  takes a fixed base part, so solid mode needs the optimized mesh AS the base). Comparison
  card reframes to "vs solid part: −N% mass at equal stiffness"; `stiffnessVsSolid` is
  unchanged. UI name: **"Part Topo"**.
- **Export isosurface threshold (2026-06):** the level the EXPORTED geometry is cut from
  the CONTINUOUS optimized field is user-tunable AFTER the run (separate from the budget /
  density target — this is the iso level, not the mass goal), for **Part Topo and binary**.
  `Model::set_iso_threshold(t, smooth_iters)` re-extracts from `x_cont`: Part Topo re-runs
  the connected-keep (anchored to the frozen load/support cells, stored as `anchor_cells`);
  binary keeps the design cells denser than `t` (no keep — sparse infill supports interior
  islands). **Both extract via `extract_region` on the BINARY keep indicator (iso 0.4) — the
  same watertight path the run uses (2026-06 fix).** Extracting the CONTINUOUS field at an
  arbitrary level instead produced sliver/degenerate triangles wherever the level grazed the
  flat boundary nodes (≈0.5), which the smoother then tore into holes/spikes on the exported
  STL. Relatedly, the aggressive Taubin λ/μ (0.63/−0.65) was reverted to the STABLE textbook
  0.5/−0.53 — the aggressive pair folded thin features; strength now comes from the pass
  count (`SMOOTH_PASS_MULT`) on the clean base mesh, not from pushing λ/μ. Re-smoothed, regions replaced; lower
  `t` keeps more material (chunkier), higher trims it leaner. **Unified with the density
  cutaway (2026-06):** for Part Topo/binary this is ONE `densityThreshold` slider —
  previewing the cutaway AND setting the export level (for Part Topo the cutaway
  `density_isosurface` injects the frozen cells as solid so it matches the body; in infill
  modes that injection is SKIPPED — `anchor_cells` there is the whole wall skin and would hide
  the interior, so the cutaway stays the dense core only); graded keeps a display-only cutaway
  (its regions are bin sets, no single iso level). The Region-smoothing slider lives in the
  Export step. Iteration cap raised 40 → 80 (complex parts can still be moving at 40); the
  progress UI reads "iteration X of max N" since convergence usually stops it earlier.
- **Result presentation polish (2026-06):** finishing an optimization jumps straight to the
  Export step with the Regions view active. The slider is renamed **"Fine-tune surface"** and
  INVERTED (left = retain less, right = retain more) since a lower iso level keeps more material
  — `value = 100 − densityThreshold`. **Part Topo render fix:** the optimized body coincides
  with the original envelope on retained faces, so the ghosted envelope hull moiréd against it
  (z-fight between two differently-tessellated coincident surfaces — only near iso levels that
  sit on the envelope plane). Fixed by dropping the envelope hull in the density/regions views
  for a solid result (`SceneManager.setResultSolid`), rendering the body opaque, and a small
  `polygonOffset` on the cutaway; infill modes keep the ghost hull + translucent modifiers.
- **Aggressive region smoothing (2026-06):** Taubin λ/μ moved from 0.5/−0.53 (pass-band
  kPB ≈ 0.11) to 0.63/−0.65 (kPB ≈ 0.048 — a tighter band removes more), and `smooth_regions`
  runs `SMOOTH_PASS_MULT` (= 4) Taubin passes per slider unit, so the 0–40 slider tops out
  at 160 passes — enough to fully melt the voxel staircase while staying volume-preserving
  (no net shrink). *(Superseded 2026-07: λ/μ reverted to 0.5/−0.53, then Taubin replaced
  entirely for regions — see below.)*
- **Constrained region smoothing (2026-07):** the slider's Taubin filter could never melt
  the terraces a BINARY voxel extraction leaves on shallow slopes — wide treads are
  low-frequency *along the surface*, inside Taubin's pass band at any pass count (the
  aggressive-λ/μ experiment above found the ceiling). `smooth_regions` now runs
  **constrained Laplacian** smoothing (SurfaceNets-style, `bins::constrained_smooth`):
  pure λ = 0.5 diffusion — which converges to the minimal-area surface, flattening
  terraces completely — with every vertex clamped to a ball of radius
  `SMOOTH_CLAMP_H` (= 0.6)·h. The clamp replaces Taubin's μ step as the
  anti-shrink/anti-collapse guarantee: staircase amplitude is ≤ h/2 < 0.6·h so it can
  flatten fully, while thin members, holes, and sharp edges can never drift more than
  sub-voxel — the export's dimensional error stays below one cell by construction.
  `smooth_regions` runs THREE stages: (1) a short Taubin pre-pass (2 + slider/4 passes)
  that melts the needle/tent spikes marching tets grows at single-voxel corners — the
  constraint centers are captured from these DE-SPIKED positions, because clamping around
  the raw positions gives every spike a protected ball around its own tip and it survives
  the whole pipeline 0.6·h tall (first-print feedback 2026-07-09); (2) the constrained
  diffusion; (3) a 2-pass Taubin polish for the C0 kinks where the clamp bound. Slider
  mapping unchanged (0–40 units × `SMOOTH_PASS_MULT` = 4 passes; diffusion reach at max
  ≈ √(0.5·160) ≈ 9 cells). Taubin (stable λ/μ) remains for the smooth-field preview
  meshes (live skeleton, density cutaway) which have no staircase to fight.
- **Blurred-indicator extraction (2026-07, `extract_region_smooth`):** mesh smoothing
  alone still left immovable pimples (second print feedback 2026-07-09). Root cause is in
  the EXTRACTION, not the smoother: a binary indicator quantizes the lattice node values
  to eighths, so at iso 0.4 many crossing edges around one node interpolate to t ≈ 0.9 —
  clusters of near-coincident vertices ("knots") whose uniform-weight Laplacian is ≈ 0.
  NO umbrella-operator smoother (ours, Meshmixer's shape-preserving, Blender's Smooth)
  can move a vertex whose neighbors all sit on top of it. Fix at the source: all export
  regions extract via `extract_region_smooth` — ONE 7-point cell-blur pass (center 2,
  faces 1, /8) over the indicator before marching tets. Node values become continuous:
  vertices spread evenly (no knots), and single-voxel junk drops below the iso before it
  is ever meshed (isolated cell peaks at 0.25, proud bump cell at 0.375 < 0.4) while
  members ≥ 2 cells survive — the same thin-feature floor the unblurred node averaging
  already imposed (a 1-cell beam never exported: raw node values 2/8 = 0.25 < 0.4).
  Nesting of graded modifier regions is preserved (blur is monotone). Flat-face dilation
  is nearly unchanged (0.23·h vs 0.20·h outward at iso 0.4).
- **Self-supporting AM filter (2026-06, `selfsupport.rs`):** a Langelaar-style
  layer-by-layer projection inserted between the density filter and `build_eps`, so the
  printed field (hence the exported shape) overhangs at most the chosen angle from the
  build plate. Build direction is **global Z** (the part is already oriented Z-up, so the
  layer-adhesion SF and this filter share one convention — no new axis). Per layer from
  the plate up, the printed density ξ_e = `smin(ρ_e, smax_{s ∈ support(e)} ξ_s)` with
  `smin(a,b) = ½(a+b−√((a−b)²+ε))` and `smax` a P-norm over the supporting cells in the
  layer below; the lateral support reach in cells = `round(1/tan θ)` clamped to
  `[0, MAX_REACH]`. **Angle convention (2026-06): θ measured from the horizontal plate,
  0–90° — 0° allows flat overhangs (no constraint, reach = MAX), 45° = the classic
  one-cell rule, 90° = vertical growth only (reach 0).** Cells on the plate layer and
  frozen/solid cells are full supporters (ξ = 1). The forward pass and its transpose
  (chain-rule reverse sweep, recomputed from the filtered field — stateless) plug into the
  OC sensitivity flow exactly like the density filter. Toggle = skip the stage.
  **Available in EVERY mode** (2026-06): in Part Topo it shapes the removed material; in
  graded/binary infill it constrains the dense regions (unsupported cells fall to the floor
  density, not void). **Advisory** (matching the tool's other approximations): it guarantees
  the *optimizer's field* is printable, but the staircase + region smoothing on the final
  mesh can still nick the angle locally. A finite-difference gradient test guards the
  transpose. Future lever: a Heaviside/robust projection for a near-hard guarantee.
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
    the tree (`cargo test -p filasim-core --test meshbench -- --ignored --nocapture`) so any future
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
  thread-local checker (`filasim_core::cancel`); the MGCG loop polls it every CG iteration and
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
  build direction; compression cannot delaminate → SF = cap, 10), and the default "worst case"
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
- `project_settings.config` in the sample is a full 30 KB print profile. We deliberately do
  NOT embed it — that would force a print profile over the user's selected printer/filament.
- **Project recognition (2026-06, RESOLVED by diffing a Bambu re-save).** Without it the
  loader pops *"The 3mf is not from Bambu Lab, load geometry data and color data only"* and
  **drops the modifiers**. The discriminator that flips the loader's `is_bbl_3mf` flag is a
  **`schemas.bambulab.com/package/2021/cover-thumbnail-*` relationship in `_rels/.rels`**.
  Our writer now emits: that relationship (+ the generic OPC `metadata/thumbnail`) pointing
  at `plate_1.png`/`plate_1_small.png` — **a real square snapshot of the optimized part
  rendered from the live three.js view** (`SceneManager.captureThumbnail` → bytes threaded
  through `export_3mf`), falling back to a tiny embedded 1×1 placeholder if the capture
  fails — `Application = "BambuStudio-…"`
  (real OrcaSlicer exports claim BambuStudio too), and a `Metadata/slice_info.config` with
  the `X-BBL-Client` header — all WITHOUT a `project_settings.config`, so the user's profiles
  stay intact. The modifier encoding itself was already correct (a Bambu re-save preserved
  our `sparse_infill_pattern=concentric` modifier verbatim). Still a test matrix across Orca
  2.x / Bambu versions to confirm.

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
  exactly on the true surface). Dev scaffolding kept: `filasim-wasm` debug exports
  `step_face_report` / `step_face_stl`, harness `stepnode_test.mjs`.
  **SUPERSEDED 2026-07-24 by §18**: STEP tessellation moves to meshStep (JS-side);
  the truck path gets deleted once §18 M1 is proven.
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

## 11. Result organization & staleness (interview 2026-06-16)

**Problem.** The viewport had ONE "Results" tab fed by ONE live solution: whatever
ran last (as-printed solve, solid solve, or the optimized design's verification
solve), with nothing labeling which. Editing a load / mesh / material *destroyed*
the result (snapped back to Setup). Users couldn't tell what they were looking at
or compare the optimized design against the homogeneous print.

| # | Topic | Decision |
|---|-------|----------|
| 1 | Availability | **Retain finished results in memory, switch instantly** (not re-solve-on-switch, not label-only). The comparison experience is the point. |
| 2 | Result set | **Manual population** — results exist only after you run them. But ONE Optimize yields several at once because the engine already solves them: **Optimized**, **Uniform · equal mass** (the "evenly distributed" baseline at the optimized mean density), and **Solid** (CAD-ideal reference). **As printed** comes from Solve once (As printed). No arbitrary verification runs — "as printed" in the Verify tab *is* the verification run. |
| 3 | Equal-mass is free | The optimize pipeline ALREADY solves the equal-mass uniform field (`x_uniform = mean_binned`) and the solid field for the comparison card (`pipeline.rs`, `c_uniform`/`c_solid`), then **discarded their displacement vectors** (`let (c_uniform, _, _) = …`). We now KEEP `u_uniform`/`u_solid` and surface them as selectable results — near-zero extra compute. |
| 4 | Selector placement | A result dropdown **inside the Results view only**, in the floating field-chip beside the STL/Voxel + field-type controls. Density/Regions tabs are untouched (they belong to the optimized run; a uniform field has no density map) and don't show the selector. |
| 5 | Provenance window | A small **top-left overlay, legend-style** (`.provenance`), showing the SELECTED result's **inputs only** (kind · mode/goal · budget or infill % · pattern · levels · material · mesh h/cells · solver run/convergence), plus a stale notice. Outputs (mass, max deflection, min SF, comparisons) appear **once**, in the results dock — which follows the selected result: as-printed entries get the printed dock, optimized/uniform/solid the optimizer dock, and its deflection readout tracks the selected step/envelope/baseline. The optimizer dock leads with max deflection and a **comparison instrument**: deflection bars for equal-mass uniform vs optimized vs 100 % solid, with the two takeaways "+X % stiffer than N % uniform at the same weight" and "Y % of solid stiffness at Z % of the weight" — plus an **efficiency instrument** (stiffness ÷ weight, solid = 100 %, derived from the summary's compliance ratios) and a **safety-factor instrument** (min SF, worst load step for the optimized design, per-design stashed eps so allowables track each design's densities; infill modes only, persisted additively as `optMinSf`) ranking uniform / optimized / solid (2026-07 consolidation). |
| 6 | Staleness — non-destructive | Changing an invalidating input no longer drops results; it **marks them stale** (badge in the dropdown, caution in the provenance window, export guard) and the result stays viewable. Re-run **from the origin step** (Solve once / Optimize) — no in-place re-run button. |
| 7 | Staleness — per-result signature | Each result records the inputs it was built from; a change stales **only** dependents. Shared (geometry/pose, mesh resolution/snap/composite, loads & supports, material) → all stale. Optimized + Equal-mass → also budget/goal/mode/levels/min-member/symmetry/self-support/skin (Equal-mass is stale exactly when its parent Optimized is). As printed → also print infill/pattern/perimeters/line-width/shells/layer-height. Solid → shared only. |

**Enabling backend change (extends the open K2 `EngineSession` item).** Displacement
coloring (`|u|`, `ux/uy/uz`) is computed client-side from each result's cached buffer,
so switching those is instant with no engine round-trip. Stress/strain/SF fields are
computed in wasm from the *active* solution (`Model.solution` + `solution_eps`), so
instant switching of THOSE requires the engine to hold several solutions and activate
one for field queries: `Model.stash_result(id)` snapshots the live `solution`/`eps`
under a key; `Model.activate_result(id)` swaps a stashed pair back in; `clear_results()`
drops them. All results of one part share the same grid, so a stash is just the `u`
vector + its `eps` (no grid copy). The optimize pipeline additionally builds `Solution`s
for the kept `u_uniform`/`u_solid` and stashes them next to the optimized one. Retention
is capped by **replacing a same-kind result on re-run** (the list can't grow unbounded);
a resolution/geometry change clears the stash (every result is stale anyway). Verify
solve/optimize touches with regbench + the wasm smoke test (the pipeline change only
stops discarding two vectors — the numerics are byte-identical).

## 12. Project save / load — `.filasim` files (interview 2026-06-16)

**Problem.** Work was ephemeral — reload the page and the model, loads, settings,
and results were gone. Users need to save and reopen a project.

| # | Topic | Decision |
|---|-------|----------|
| 1 | Results in the file | **Embed the FEA results (instant reopen), toggled.** Re-deriving from the saved density field is cheap, but the user chose to store the displacement buffers so reopen needs zero compute. A "results" checkbox in the Save control includes/excludes them. |
| 2 | Model storage | **Embed the original imported file** (self-contained, shareable). On open it re-imports + replays the orientation, reproducing the exact working mesh so the triangle-index load/support selections restore correctly — embedding the *processed* mesh would re-tessellate differently and scramble them. |
| 3 | Results-excluded reopen | **Keep the optimized design.** The compact density field is always saved, so a small file still restores the optimized design (Density/Regions + 3MF re-export); deflection recomputes when you re-run Optimize. |
| 4 | Heavy data | Just the **displacement vector per result** (+ its `eps`). Stress/strain/SF derive from displacement on the fly, so they're never stored. |

**File format** — one **stored-zip** (`filasim_core::zip`, the same writer behind the 3MF
export), extension `.filasim`:
- `project.json` — schema + app version, all session settings (material/curve *values*
  used — self-contained), the loads/supports (triangle indices), the cumulative
  orientation matrix, the `optSummary`/region densities, and the result roster (metadata
  only) when results are included.
- `model.<stl|3mf>` — the original imported bytes, verbatim.
- `design.bin` — compact optimized design: the per-design-cell binned density + `x_cont`
  + bins + centers + the skin/eval scalars needed to **re-derive** the region meshes and
  stress `eps` on load (no region geometry is stored — it rebuilds via `classify_cells`
  + `extract_region` + `smooth_regions`, deterministic for the restored grid).
- `results/<id>.f32` — `[displacement][eps]` per retained result (only when included).

**Orientation.** `Model` tracks a cumulative transform (`transform_accum`, composed in
`transform()`); `transform_matrix()` returns it for the manifest. On open: re-import the
original file (canonical pose) → push settings → `transform(savedMatrix)` once → push
loads → restore. Because re-import reproduces the canonical subdivision and the transform
doesn't reindex, the saved BC triangle indices stay valid.

**Engine API.** `export_project(modelBytes, entry, manifest, includeResults)` assembles the
zip; free fns `project_manifest`/`project_model` pull the manifest + model out; `Model`
methods `restore_optimization(designBlob)` (rebuild `self.opt` + the optimized `eps`) and
`restore_result(id, blob)` (inject a displacement into the stash) are driven by
`restore_project(zipBytes)`. The worker keeps the original bytes (`lastModel`) for save and
stages the zip between the two-phase open (`openProjectModel` → push settings → `openProjectRestore`).

**Verified** with a smoke round-trip: orient → optimize → save → re-import the original
file + replay the transform + restore reproduces the regions, vertex density, displacement
(< 1e-4), and stress field, and re-exports the 3MF; the design-only save restores the
design with no results. regbench stays byte-identical (the optimize tail only snapshots
already-computed fields).

**Deferred.** The "few-second re-verify" on a results-excluded reopen (re-solve the
restored design's verification + baselines instead of a full re-optimize) — the fallback
today is re-running Optimize. The on-disk format is uncompressed (stored zip); add DEFLATE
if file size becomes a concern.

## 13. Multiple load steps (interview 2026-06-17)

**Problem.** A part lives as ONE flat `bcs: Bc[]` (store.ts) — every BC applied in a single
simultaneous solve. Real parts see several distinct load situations (a bracket pushed, then
pulled, then twisted), and an infill design should survive ALL of them, not just one. There
was no way to define several load cases, run them in one pass, step through their results, or
optimize one design against the set. (The "Step" in the wizard rail is a workflow STATION, and
`step.rs` is the STEP CAD format — neither is an FEA load step; this section adds the real
thing.)

| # | Topic | Decision |
|---|-------|----------|
| 1 | Data model | **One shared `bcs: Bc[]` (geometry/selection/baseline defined ONCE) + a thin per-step OVERRIDE layer — never duplicate BCs.** Add `loadSteps: LoadStep[]` + `activeLoadStepId` to the store; `LoadStep = {id, name, overrides: Record<bcId, {active, force?, pressure?}>, includeInOptimize, weight}`. An absent override field inherits the base BC; `active` defaults true. The effective BC for a step = base `Bc` merged with its override. |
| 2 | Single-case stays trivial | A new/imported model has exactly ONE step with empty overrides. While `loadSteps.length === 1` the Loads panel renders **exactly as today** and NO table appears — editing a force writes the base `Bc` directly. The table only materializes when the user adds step 2. Zero friction for the common case. |
| 3 | Per-step constraints | **Supports toggle active/inactive per step, everywhere — including optimization** (cost accepted). Their selection and parameters (pinned axes, elastic stiffness) stay SHARED; only on/off varies per step. (Per-step support *parameters* would be a wider override — deferred, see below.) |
| 4 | Per-step forces | **Full components per step** — the override stores a complete `[Fx,Fy,Fz]` (and a per-step pressure value), not a scalar multiplier. No scalar-vs-vector ambiguity; a step's load is edited outright. |
| 5 | Solve cost / grouping | Group steps by **constraint signature** (the active support set + params). The solver cache keys on `fixed`/`springs` but deliberately **NOT** `forces` (solve.rs:433-434), so steps that share fixtures share ONE stiffness-hierarchy build and differ only by a cheap RHS swap + warm-started MGCG (solve.rs:492-503). Keep a **per-step** `last_u` (not the single shared one) so a tension step doesn't warm-start from a torsion step and inflate iterations. Worst case = N rebuilds when every step pins a different DOF set. |
| 6 | Optimizer objective | **Weighted-sum compliance with editable per-step weights** + a per-step **"include in optimization"** toggle (both in the weight panel). Aggregate `se = Σ(wᵢ/Σw)·seᵢ` at the strain-energy stage (simp.rs:853-867) BEFORE the density filter / symmetry / self-support so those run once; the OC bisection + stiffness-match secant (pipeline.rs:137-277) and `update_eps` (mg.rs:921) are untouched. NOT min-max (non-smooth, deferred). |
| 7 | Per-step scope | **Only loads/BCs vary per step.** Material, mesh resolution, infill budget and optimization mode stay GLOBAL — one part, one material, one mesh, loads vary. |
| 8 | Results & step-through | One `ResultEntry` per **(kind, step)**, tagged with an optional `loadStepId` (existing entries → null/global). A **load-step selector** in the Results view, orthogonal to the kind picker; reuses `activate_result`. Per-step buffers preloaded when steps ≤ 5, lazy beyond. |
| 9 | Envelope result | A client-side reduction over the per-step buffers → per-vertex **max von Mises / min safety factor** ("does the part survive ANY load?"), shown as a pseudo-step in the selector. The view the user usually acts on. |
| 10 | Legend | **Shared fixed color range across all steps** so magnitudes compare honestly (today the legend auto-ranges per result, Viewer.tsx:362-365) + a small **"fit" button** by the legend that rescales to the current step on demand. Deform exaggeration stays global (`referenceMaxDisp`, store.ts:1039) so deflections are comparable. |
| 11 | Persistence | **STAY at `PROJECT_SCHEMA = 1`** (the loader hard-rejects schemaVersion > 1, store.ts:2610); add an **optional** `loadSteps` field to the manifest. The loader synthesizes a single step from `mf.bcs` when it's absent → existing `.filasim` files AND single-step new files interoperate across versions; only multi-step files need new code. BCs are re-id'd on load (store.ts:2623), so serialize override keys by BC order and remap after re-id; re-id step ids too. |
| 12 | Naming | Internally rename the wizard "Step" concept to **"Station"** (`activeStation`, etc.) to kill the clash with the FEA "Load step"; user-facing FEA term stays **"Load step"**. (Falls back to "Load case" for the FEA term if a code-wide rename is too invasive in the moment.) |

**Build order** (each milestone shippable, each gated by regbench + the wasm smoke test per
the §11 / architecture-hardening convention): **(1) data model + persistence** ✅ DONE — store fields,
additive save/load, single-step default; no behavior change. **(2) standard multi-step solve** ✅ DONE
(2026-06-18) — the load-step table UI, per-step results, the viewer selector, shared legend + fit
button. Done as: `runSolve` branches to `solveAllSteps` only when `loadSteps.length > 1` (single-step
path is byte-identical, not just regbench-identical — it's the same code); result identity became
**(kind, step)** with stash key `resultStashId` = bare kind for one step else `kind::stepId`; the
viewer got a kind dropdown + a `.stepsel` step dropdown; the legend PINS its range across a same-kind
step switch (auto-fit on kind switch) with a "⤢ fit to step" button. Step ids are now persisted so
`kind::stepId` result keys survive reload. **(3) envelope result** ✅ DONE (2026-06-18) — an "Envelope ·
worst case" pseudo-step (sentinel `loadStepId`) is appended to any kind with ≥2 steps. It has NO stashed
solution: selecting it renders the part UNDEFORMED, colored by a client-side per-vertex reduction over
the steps (MIN for safety factors — "does it survive any load" — MAX for everything else incl. |u|,
σᵥᴹ). Computed lazily by activating each step's stash + reducing, cached and invalidated on re-solve;
banding/fit/legend all work since it's a scalar-field path. Stripped on save, re-derived on load.
**(4) multi-load optimizer** ← NEXT — weighted-sum, include toggles + weights, `se` aggregation;
optimize is still single-load (active step) until then. Milestone 4 touches the optimizer and must stay
regbench-byte-identical for the single-step path. Also bonus (2026-06-18): **discrete contour banding** —
click the result legend bar to quantize the color scale into hard steps (classic FEA bands), scroll the
bar to change the count (2–20). Shared LUT rewrite; session-only, not persisted.

**Deferred.** Per-step support *parameters* (different pinned axes or elastic stiffness per
step) — only on/off varies in v1; a fuller override is a later decision. Min-max (worst-case)
optimization objective. A hard cap / eviction policy for very large step counts on fine meshes.

## 14. Pre-push gate & the beam regression suite (2026-07-01)

One command — `node scripts/preflight.mjs` — is the gate before a major push,
formalizing the "gated by regbench + the wasm smoke test" convention. **Hard**
gates (fail the run): `cargo test`, `regbench --check`, the wasm smoke test.
**Advisory** (reported, never block): `cargo fmt --check`, `cargo clippy` — the
tree ships no `rustfmt.toml`, so default rustfmt disagrees with the house style;
promote both to hard once a `rustfmt.toml` captures the style and clippy is clean.

**Beam suite** (added to `regbench`): one canonical 64×8×4 mm rectangular
cantilever, run **solid** and at **uniform 30 % infill**, at two mesh sizes
(h = 1.0 → ~2k cells; h = 1/3 → ~55k cells), in three load cases:
- **tension** (axial) — anchor: exact uniaxial `ux = FL/AE` (ratio ≈ 1.0);
- **bending** (transverse tip) — anchor: Timoshenko tip deflection;
- **modal** (root-clamped, 6 modes) — anchor: Euler–Bernoulli mode 1 (wide band;
  a thick hex beam is shear-stiff, so this only guards a gross mass/unit slip).

The rectangular (2:1) section separates all six modes so they are individually
interpretable. Modal is **regression-anchored** (all 6 freqs drift-guarded at
0.5 %) with the analytic ratio kept only as a sanity metric.

**The key check** is the mesh-independent solid↔infill ratio. Solid and infill
share a mesh, so discretization cancels and the ratio must equal the exact
`E(ρ)=coeff·x^exp` (stiffness) / mass-`x` closed form: deflection × `1/x^exp`
(=6.086 at x=0.3), frequency × `x^((exp-1)/2)` (=0.740). This isolates the
E(ρ)-vs-mass wiring independent of mesh error — the classic place a unit/scale
bug hides. Measured 6.085806 / 0.740083 at BOTH mesh sizes.

**Benchy voxelization**: 3DBenchy at h = 1.0 (coarse ~90k) and h = 0.4 (fine
~1.4M cells), regression-anchored (no analytic volume): cell/element counts,
solid volume, and bbox as QUALITY; time / Mcells·s⁻¹ as INFO.

Solve **speed** is guarded by INFO metrics only (iteration counts, modal
V-cycles — deterministic — plus wall-clock): the human reads the deltas before
pushing; no hard time budget (machine/thread noise would make it flaky).

## 15. Validate Orientation — pitch/roll layer-adhesion sweep (interview 2026-07-05)

**Problem.** Print orientation decides where the layer planes lie, and layer adhesion is the
weakest direction — but the tool only evaluates the orientation the model happens to be in
(`sfz` against hard-coded +Z, lib.rs:2816). The user wants to find the *strongest* orientation:
sweep pitch/roll, score each by the worst layer-adhesion safety factor, show the whole landscape
as a heatmap, click any point to preview that orientation live, and commit the best one.
(Inspiration: Ansys SpaceClaim's orientation map — but ours is physics-per-pixel, not heuristics.)

**Key insight (kills the obvious cost).** The structural solve is **isotropic** (fem.rs:36) and
loads/constraints rotate with the part, so the stress field in the part's frame is
orientation-independent. Only the failure criterion depends on the build direction **n**.
Therefore: **solve ONCE, keep the full per-cell stress tensor (all 6 components already exposed,
stress.rs:18), and per orientation just project the traction on the layer plane** —
σₙₙ = n·σ·n and shear τₙ = |σ·n − σₙₙ n|. The 37×37 heatmap costs one solve plus a trivially
parallel tensor projection, not 1,369 re-solves. This also makes the criterion flip-symmetric
(quadratic in n), so a hemisphere of orientations is the complete domain.

| # | Topic | Decision |
|---|-------|----------|
| 1 | Failure criterion | **Combined normal-tension + shear interaction** on the layer plane: `(⟨σₙₙ⟩₊/Sᵗᶻ)² + (τₙ/Sˢᶻ)² ≤ 1/SF²`. Tension-only normal traction (compression never counts against adhesion — matches existing `sfz` semantics). New material-card property **`shear_strength_z` (Sˢᶻ)**, **default 0.6 · strength_z** until Stefan's shear test data lands; additive in PROJECT_SCHEMA = 1. At n = ẑ with τ ignored this reduces to today's `sfz`. |
| 2 | Which design is scored | **The currently solved design** — the stress field of the currently selected result (uniform or optimized), so the heatmap always agrees with the viewport. NO per-orientation re-optimization (the self-support cone makes the design orientation-dependent — that's the post-Apply optimizer run, and the map marks itself stale after it). Hence the feature name: **"Validate Orientation"**, a decision aid on the design you have, not a global optimum claim. |
| 3 | BC masking | Peaks at rigid constraints are mesh-divergent singularities; at loads they are real-ish (distributed tractions). **Mask a fixed ring (2–3 cells, graph distance) around constraint node sets only — never around force/pressure regions.** The per-BC node lists already exist (attach.rs:88). Masked cells are **greyed/ghosted in the viewport** while the panel is open, and the readout reports BOTH masked min-SF (drives the heatmap color) and unmasked min — nothing is silently hidden. Same mask everywhere (heatmap scoring + readout). |
| 4 | Heatmap domain | **Square pitch × roll, each ±90°**, n = Rx(pitch)·Ry(roll)·ẑ — covers the hemisphere exactly once; (0°,0°) = current orientation. Yaw is irrelevant (rotation about ẑ changes neither layers nor overhangs). **Dense 5° sweep (37×37 = 1,369 samples) with a progress bar**; progressive 15°→5° refinement + principal-stress cell pruning are the fallback if too slow at ~1M cells. |
| 5 | Score per pixel | Per cell per **load step** evaluate the criterion, **min across steps** (envelope semantics — a step's delamination is fatal regardless of optimizer weight), then min across unmasked cells. **Heatmap colors by layer-adhesion SF only** — the material (von Mises) SF is orientation-independent and would only flatten the map into a plateau; the selected-pixel readout shows all three (layer / material / overall SF). |
| 6 | Preview vs Apply | **Clicking a pixel is always preview-only**: display-side rotation (build direction up, plate under z-min) + recolor with that n's per-cell layer SF; engine state untouched; closing the panel restores the view. Explicit **"Apply orientation"** commits: `transform()` (drops grid/solution/results, lib.rs:1045) → re-voxelize → auto re-solve → re-run the sweep so the map re-centers at the new (0°,0°). Readout shows **predicted vs re-solved** min layer SF so voxelization drift (stair-step realignment) is visible, not discovered. BC selections are index-based and survive the transform. Plate seating is display-only (no gravity in the solve). |
| 7 | UI placement | **Inside station 5 · Optimize** as a collapsible "Validate orientation" section (NOT a new rail step, NOT in results — needs a solve, and the check→apply→re-optimize loop lives in Optimize anyway). Heatmap gets a flyout/expanded view; crosshair at (0,0), marker on the global best; Werkbank styling. |
| 8 | Support volume (later) | Second metric in the SAME panel as a toggle/tab when it lands. It is NOT flip-symmetric, so its domain extends **roll to ±180°** (2:1 rectangle; the SF layer simply repeats in the outer half). Needs new overhang-face detection + support-volume estimation (selfsupport.rs is an optimizer density filter, not a detector). |

**V1 exclusions.** Support volume (dec. 8), yaw, gravity/self-weight, per-orientation
re-optimization, anisotropic *stiffness* (solve stays isotropic; only the failure criterion is
directional), progressive refinement (dec. 4 fallback), user-tunable mask radius.

**Build order** (each gated by regbench + wasm smoke test): **(1)** material card + criterion ✅
DONE (2026-07-05) — `shear_strength_z` (default 0.6·Sᵗᶻ derived at use time; optional τᶻ column in
the material card, blank = auto), `sfz`/`sf` cell values are the interaction criterion at n = ẑ;
wasm `set_material` gained an optional 6th arg (old callers keep working); theory manual §4.7
updated. **(2)** sweep kernel ✅ DONE (2026-07-05) — `filasim-core/src/orient.rs`: fused
6-component tensor pass, closed-form principal-stress prune (drop cells whose best-possible SF
stays ≥ cap at every n), 26-neighbor constraint-ring mask (`RING_DILATIONS = 2` + node seeding ≈
3 cells; Fixed/Frictionless/Displacement/Cylindrical only — loads and the compliant Elastic
foundation stay scored), pixel-parallel sweep returning (scored, all) per pixel; 6 unit tests (hand-calc uniaxial /
shear / compression, flip symmetry, prune bound). wasm API `orientation_sweep_begin(ids, stepDeg)`
(ids = result-stash ids to fold worst-case, [] = current solution; returns meta JSON) →
`orientation_sweep_rows(start, count)` (chunked so the worker posts progress between calls) →
`orientation_sweep_end()`. Smoke: grid dims/cap/symmetry, center pixel ≤ min sfz, cantilever
physics anchor (layers ⊥ X worse than flat), multi-step fold = elementwise min of per-step sweeps.
**(3)** panel UI ✅ DONE (2026-07-05) — collapsible "Validate orientation" section in station
5 · Optimize (`web/src/ui/ValidateOrientation.tsx`): jet-flipped canvas heatmap (matches the SF
legend exactly — same colormap function), ＋ current / ◎ best / ◉ selection markers, chunked
`orientationSweep` worker op with determinate progress bar (buildSim pattern), readout rows
(scored + all-cells layer SF at the pixel, orientation-independent material floor via
`materialSfMin` in the begin meta, best orientation, "material governs" hint). Click-to-preview:
`layerSfField(dir, ids)` recolors via the scalar-field path (smoke-anchored: at n = ẑ it equals
the `sfz` field elementwise) + `SceneManager.setOrientationPreview` rotates part/wireframe/BC
markers/result groups about the part center, display-only; preview cleared on
invalidation (`clearEnvelopeCache` + in-flight-sweep token) or Exit preview (full restore via
re-activating the result). UX revisions after Stefan's first test (2026-07-05): section renamed
**"Optimize orientation"**, always visible (not collapsed); axes are user-facing **rotation X /
rotation Y** (= pitch/roll internally); sweep completion lands directly in the preview at the
current orientation — part UNDEFORMED (zero-displacement push, envelope-style), STL surface
forced, and `resultField` switched to `sfz` so the legend says "Safety factor — layer adhesion"
instead of the previous field's label; Best / Current jump buttons; rotation X/Y NumInputs
stepping 5° (snap to grid); heatmap supports DRAG (pointer capture) with trailing-edge coalescing
of `layerSfField` fetches (one in flight, latest wins — the worker never floods); Best = the
HIGHEST worst-case layer SF (max of the per-pixel minima); the map is DELETED whenever inputs
change (`markResultsStale` — loads/material/print epochs — calls `clearOrientationSweep`, which
also orphans an in-flight sweep); **"Include interlayer shear (τᶻ)" checkbox** — one engine flag
(`set_layer_shear`, off ⇒ effective shear allowable = ∞) so the sfz/sf fields, the sweep and the
preview all reduce to pure cross-layer tension identically (smoke: shear off ⇒ sfz never more
critical); session setting, survives model reload/project restore, never invalidates the
solution. Round 4 (2026-07-05, after voxel-mode testing): preview NULLS the volumetric section
field on entry (the stale cap was probed in the old result's units under the SF formatter —
"−10.85×" was really −10.85 MPa; envelope had the same discipline) and no longer forces the STL
surface; **voxel display supported** via `layer_sf_voxel_field` (per-cell crisp, owning-cell
value per hull vertex) with **constraint-ring cells returned as NaN and painted flat grey** —
the shared LUTs are now 256×2 (row 0 colormap at uv.y 0.25, row 1 neutral grey at 0.75 for
non-finite values; section-cap shader, density uv writers and `setBanded` updated to address
row 0 explicitly; extremes tracker already skips NaN). The greyed-mask deferral from (3) is
thereby DONE for the voxel surface; the smoothed STL surface stays unmasked by design (nodal
recovery would smear NaN). Deferred to (4): GREYED mask cells in the viewport (needs a viewer feature; the panel's
scored-vs-all numbers carry the honesty meanwhile) and plate seating in the preview pose.
**(4)** Apply flow ← NEXT — transform + auto re-solve + re-sweep + predicted-vs-actual readout,
greyed mask cells, preview plate seating. **Apply must rotate ALL load directions WITH the part**
(forces, moments, acceleration vectors — §16 dec. 8), deliberately breaking the Model-step
"loads keep world directions" convention for this one flow: the sweep scored the service load
case as attached to the part, so keeping loads world-fixed on Apply would re-solve a different
problem than the pixel the user clicked. (Mass points and BC selections transform with the part
in every flow; only load *directions* differ between the two conventions.)

## 16. Acceleration loads & remote point masses (interview 2026-07-09)

**Problem.** Every load today is a surface traction; there is no inertial loading. Printed
parts routinely carry components (motors, batteries, cameras) whose *mass* — not a contact
force — is what loads the structure under gravity, shock or maneuver ("survives a 6g crash").
Smearing a component's weight over its mounting face loses the lever arm: a motor hanging
40 mm off a bracket loads it in bending, not compression. This section adds an
**acceleration** load type plus **dummy masses** — a mass value at a remote CG point,
attached to a selected surface, whose force AND transported couple load the patch
(a "remote point mass").

**Existing machinery (found 2026-07-09).** (a) `assemble()` already takes
`gravity: Option<(accel mm/s², density)>` and lumps a uniform body force per occupied cell
(attach.rs:275) — plumbed through wasm `set_gravity` / EngineClient but never called by any
UI, and its **uniform density is wrong for graded infill** (a 20 % cell weighs like solid
skin). (b) The moment BC already realises an exact-resultant deformable couple on a patch
with no MPCs/rotational DOFs (attach.rs:565) — precisely the transport mechanism a remote
mass needs. (c) Units are consistent mm–N–MPa, so mass × acceleration = N falls out clean
(density g/cm³ → ×1e-9 tonne/mm³, mass g → ×1e-6 tonne, 1 g₀ = 9810 mm/s²).

| # | Topic | Decision |
|---|-------|----------|
| 1 | Mass coupling behavior | Per-mass **`behavior: "deformable" \| "rigid"`, in the schema from day one; default deformable; deformable ships first, rigid is its own later milestone.** Deformable = load-only: statically equivalent force `F = m·a` + transported couple `M = (p − c) × F` (p = mass point, c = patch area-weighted centroid) distributed over the patch — reuses the force + `moment_forces` machinery verbatim, adds NO stiffness (patch stays as compliant as the bare part; conservative for displacements, Saint-Venant caveat within the first cells, same tier as bearing/moment). Rigid = penalty spider from patch nodes to a 6-DOF virtual master at the mass point, master statically condensed out (free 6×6 block): operator gains per-node 3×3 diagonal blocks (elastic-foundation character) + one **rank-6 coupling term** per rigid mass — the one genuinely new solver capability; must thread through SIMD/threaded matvec, MG preconditioner (pass-through), RBM check, optimizer solves; penalty scaling vs the Chebyshev smoother is the convergence risk to retire. UI copy states it honestly: "Deformable (load only) / Rigid (stiffens the mounting face)". |
| 2 | Sign convention | **Field convention ONLY: "every mass feels F = m·a along the entered vector."** Gravity = 1g pointing −Z, weight pulls down; "5g sideways shock" = enter 5g sideways. The ANSYS frame/d'Alembert convention (enter frame acceleration, force opposite) is NOT offered — anyone thinking in frames can negate a vector. Input = direction + magnitude (force load's dual-mode pattern), displayed in **g by default** (new "acceleration" quantity kind in units.ts, g / m/s² selectable), one-click **"1g ↓" preset**, convention stated in one panel line. |
| 3 | Part self-weight | **Always on whenever an acceleration is active** — no checkbox. Body force scaled per-cell by the volume-fraction field: `f_cell = ρ_mat · volfrac_e · a · h³` lumped to the 8 nodes, in all three model states (uniform: skin 1 / interior infill ratio / boundary occupancy-weighted; optimized: per-cell ρ; printed: homogenized) — the same field the mass readouts already composite. The dormant uniform-density hook is upgraded, not reused as-is. |
| 4 | Optimizer treatment | Self-weight makes acceleration steps **design-dependent loads** (lighter design ⇒ smaller load). Chosen: **track the load, skip the extra gradient term** — recompute the body force from the current density field every SIMP iteration (cheap next to a solve), keep the standard compliance sensitivity, drop `2uᵀ(∂f/∂ρ)`. Gradient slightly inconsistent, biased toward KEEPING mass in self-weight-loaded regions (the safe direction); the classic low-density parasitic pathology is blocked because E(ρ) is physical (exp 1.5–2.0) and ρ is floored at min printable infill. Verification solve is exact regardless. Full design-dependent sensitivity + sign-guarded OC = follow-up ONLY if a self-weight-dominated regbench case ever oscillates. Blocking accel steps from optimization was rejected — a drone arm optimized ignoring its motor masses under 6g is the headline use case. |
| 5 | Data model | **Acceleration is a load ENTITY in the shared `bcs` list** — kind `"accel"`, the first selection-less BC (`tris` becomes kind-dependent; every `bc.tris`-touching path gets a kind guard). Named ("Gravity", "Cornering 3g"), a row in the steps table, per-step override `{active, accel vector}` exactly like forces; **multiple accel entities sum vectorially** when active in the same step ("crash" = Gravity + "6g lateral"). Masses: kind `"mass"` with `{tris, massGrams, point, behavior}`; per-step override = `active` only (a component's mass isn't a per-step quantity; a per-step value override stays available as a future additive field). Rides ALL existing rails: naming, roster, per-step toggles, optimizer include/weight. |
| 6 | Engine API | New `BcKind::Mass { point, mass }` in filasim-core so the couple transport happens where the patch centroid is actually known; the worker resolves each step's active accelerations to ONE summed vector for the existing `assemble()` gravity/accel parameter. Envelope pseudo-step and per-step optimized results work unchanged (per-step results roster, §13). |
| 7 | Mass placement UI | **Numeric XYZ DRO fields; the point initializes at the area-weighted CG of the selected surface** (zero lever arm — neutral, predictable; users have component CGs from CAD). Viewport: filled sphere at the point + spider lines to the patch + name/mass label, so the lever arm is always visible. Panel: **live readout of resolved \|F\| = m·\|a\| and transported \|M\|** for the shown step — makes the lever arm visible pre-solve and catches unit slips instantly. 3-axis drag gizmo = fast follow (not gating); click-to-place rejected (a mid-air point has no raycast depth). |
| 8 | Frames | **Mass points transform WITH the part** on every reorientation (they are physical components bolted on — like the index-based selections). **Acceleration vectors stay world-fixed on Model-step rotations**, same documented rule as force loads ("all load directions stay world-fixed; re-check after reorienting") — one convention, no per-kind exception. The Validate-Orientation **Apply flow is the deliberate exception** and rotates all load directions with the part (§15 build item 4). |
| 9 | Rotational effects | **Linear acceleration only in v1.** Centrifugal (ω) / angular acceleration (α) loads — impellers, prop adapters — are reserved as a future selection-less load kind `"rotation"` (axis point + direction, ω, α); the position-dependent body force is a modest extension of the same per-cell loop, and every mass already has a coordinate for the `m·ω×(ω×r)` term. Nothing in this design blocks it; it brings its own questions (axis UI, RPM units, stress stiffening) to its own interview. |
| 10 | Mass metrics | **Dummy masses are EXCLUDED from every part-mass metric** — mass card, "−N % mass" claims, optimizer mass budget all mean *printed part mass*; external components aren't design mass. Setup panel shows an informational "attached masses: N g" line so nobody wonders. **Comparison card: each design carries its own true weight** (the solid baseline is heavier and carries a bigger self-weight load — the honest comparison of the two printable artifacts); fine-print note when accel steps exist. |
| 11 | Preflight & RBM | **Soft advisory warning** (never a blocker) when a mass has no step with any active acceleration — almost always "added the motor, forgot gravity". The RBM/mechanism check must **count body forces as loads per component** (the dormant gravity path never fed `load_nodes`/`hasLoads`): with self-weight on, every component with mass carries load, and an unconstrained island under acceleration is an RBM failure the check must catch. |
| 12 | Accel visualization | A selection-less load has no geometry anchor: draw **one labeled arrow at the part's bbox centroid** in the entity's roster color, visible when that entity is active in the displayed step — same visual language as surface loads, body-anchored. |
| 13 | Persistence | **STAY at PROJECT_SCHEMA = 1, strictly additive** (load-steps precedent, §13 dec. 11): new kinds `accel`/`mass` + fields (`massGrams`, `point`, `behavior`, override `accel`) are optional; old projects load unchanged, projects without the new kinds round-trip byte-identical. |

**V1 exclusions.** Rigid behavior (schema-ready, ships as its own milestone), rotational
loads (dec. 9), the drag gizmo (dec. 7), per-step mass value override (dec. 5), frame/
d'Alembert input convention (dec. 2), modeling the mounted component's stiffness (that is
"model the bracket as geometry", not a solver feature).

**Build order** (each milestone shippable, each gated by regbench + the wasm smoke test;
no-accel paths must stay **byte-identical**): **(1) engine** — `BcKind::Mass` (deformable:
force + transported couple about the patch centroid), per-cell volume-fraction self-weight
(replaces the uniform-density hook), per-step accel resolve, RBM/`hasLoads` body-force
awareness; regbench additions: *lever-arm analytic* (tip mass m at offset r on the §14
cantilever ≡ hand-composed tip force mg + moment mgr — must match to solver tolerance) and
*self-weight cantilever* (1g, no dummy mass, vs q·L⁴/8EI band); theory-manual section on
inertial loads. **(2) data model + UI end-to-end (deformable ships here)** — store kinds +
kind-guard sweep on `tris` paths, steps-table rows, panel (g-unit accel entity + 1g preset;
mass DRO fields + CG init + |F|/|M| readout), viewport (sphere/spider/label, centroid accel
arrow), preflight warning, additive persistence, attached-masses line. INTERIM: accel-carrying
steps are excluded from optimizer include with an advisory note (§13 precedent: "optimize is
still single-load until milestone 4"). **(3) optimizer** — dec. 4 (track load per iteration,
standard sensitivity), remove the interim exclusion, comparison-card own-weight semantics +
fine print; optimizer regbench case. **(4) rigid behavior** — penalty spider + rank-6
condensed operator term, convergence validation vs the Chebyshev smoother at 1M cells,
`behavior` toggle UI enabled.

**Deferred.** Per-step mass value override (additive when needed — "full vs empty tank").
Rotational load kind (dec. 9). Drag gizmo for the mass point. Full design-dependent
sensitivity + sign-guarded OC (dec. 4 contingency).

## 17. Strength-driven optimization — SF-target goal (interview 2026-07-24)

**Problem.** Both existing goals are stiffness-driven: "budget" (min compliance at a fixed
mean density) and "match" (least material to match uniform stiffness). Users who ask "will
it break?" want a strength goal — "use as little material as possible such that the safety
factor is at least N". Two hard sub-problems drove this interview: (a) elastic stress at
re-entrant corners ("notches") is mesh-divergent, so a raw max-stress criterion chases
artifacts and recedes under refinement; (b) the target can be unreachable even at full fill
(the optimizer's "full" is the cap, 0.70 by default, and skin cells are not designable), so
infeasibility is routine product behavior, not an error path.

**Existing machinery (found 2026-07-24).** Per-cell SF fields already exist for display:
`sfm` (von Mises vs in-plane strength), `sfz` (layer-adhesion tension+shear interaction,
§15), `sf` = min of both, SF_CAP 10 (filasim-wasm/src/lib.rs:3014-3060); graded allowables
scale by the same Gibson-Ashby factor as stiffness so the display factor cancels. Stress is
per-cell at hex centers (stress.rs) with optional ZZ-style nodal recovery for smoothing.
The "match" goal already implements a guarded secant on budget with the criterion evaluated
on the verified binned design (pipeline.rs:196-335). Design variables are interior densities
in [floor 0.10, cap 0.70]; skin is always-solid and not designable. The literature's
stress-constrained hazards (singularity of vanishing elements, thousands of local
constraints needing adjoint + p-norm aggregation, Le et al. 2010) mostly do NOT apply:
graded mode is a bounded sizing problem, not 0/1 topology.

| # | Topic | Decision |
|---|-------|----------|
| 1 | Goal shape | **Third goal alongside budget/match: minimize material s.t. SF_crit ≥ target** (default target 2.0, user-editable). Not a constraint bolted onto the existing goals (muddier UX, harder problem) and not max-SF-at-budget (users have a load and want a margin). |
| 2 | SF measure | **Per-project toggle: material / layer adhesion / both; default = both** (`sf` field) — "optimizer says SF 2.0" must match what the SF display shows, and layer adhesion is the failure mode FDM parts actually die from. |
| 3 | Algorithm | **Staged. M1 = outer guarded secant on budget reusing the match machinery**: inner layout stays stiffness-driven OC (unchanged), SF_crit is evaluated on the **verified binned design** after each pass, budget walks to the smallest value with SF_crit ≥ target. Tolerance band sits ABOVE target only (never accept below — unlike match's symmetric ±2%); bisection fallback guards non-monotonic blips. **M2 (only if M1 measurably over-spends)**: local redistribution — inflate per-cell floors where SF is violated, `floor ← floor·(target/SF)^(1/n)` (Gibson-Ashby strength exponent), re-run layout. Textbook stress-constrained SIMP (p-norm aggregate, adjoint solves, MMA) rejected as machinery the bounded-density problem doesn't need. |
| 4 | Criterion (the notch answer) | **SF_crit = the MINIMUM of the smoothed criterion field over the scored cells** — literally the minimum of what `sfx`/`sfmx`/`sfzx` plot, so the number always has a cell you can point at. Smoothing (nodal recovery + re-interpolation) still kills single-cell staircase spikes; the only material left OUT of the number is left out *visibly*, via the §20 mask (void, ersatz void, BC singularity zones — greyed in the plot). **Revised 2026-07-25**: the original criterion trimmed the worst **0.2 % of solid volume** first, on the critical-distance argument that notch-tip stress is mesh-dependent while a fixed volume fraction ≈ a fixed distance from the tip. Correct in theory, unusable in practice — the margin it bought depended on the SHAPE of the hot spot (measured: **+2 % on a beam** whose weakest material is a long uniform fiber, **+23 % on a hook** whose weakest material is one fillet) and on PART SIZE (0.2 % of a 400 g bracket is a far bigger blob than of a 12 g hook). Unpredictable margin + a panel reading 2.02 over a plot marked 1.64 = users correctly reading it as a bug. **Accepted consequence:** SF_crit now falls as the mesh is refined near a riser, and never converges at a perfectly sharp re-entrant corner. Handled by SAYING SO, not by hiding it: `riser_ratio` (mean SF in a ±2-cell box ÷ the minimum) is reported with the number, and above ~1.6 the panel and the dock warn that a finer mesh will report less. |
| 5 | Load steps | **SF enforced on the envelope over included steps** — every included step must meet the target (matches the §13 dec. 9 envelope pseudo-step). Per-step weight sliders keep their existing meaning (inner stiffness layout only); safety is worst-case, never weighted. |
| 6 | Infeasibility (the 100%-fill answer) | **Pre-flight solve with all designable cells at cap, before any optimization.** If SF_crit(cap) < target: skip the loop, deliver the all-at-cap design, banner "Target SF N not reachable — best achievable is X", plus a diagnosis of the binding region — skin-limited ("infill can't fix this: reorient / thicker walls / stronger material") vs interior-at-cap ("raise the cap") — with a one-click view of the binding cells. When feasible, the same solve is the secant's upper bracket, so it is never wasted. |
| 7 | Mode scope | **All three modes (graded/binary/solid) at launch.** Graded and binary need nothing special (SF is evaluated on the binned design either way). Solid mode masks ersatz-void cells (density < ~2× the 1e-3 void) out of the percentile so meaningless void stress cannot pollute the criterion. |
| 8 | Honesty framing | Same advisory stance as §3's SF display: strengths come from preset/measured values and Gibson-Ashby scaling — the goal is a design aid, not a certified safety factor. Copy must say so where the target is entered. |

**Build order** (each milestone shippable, gated by regbench `--check` + the wasm smoke
test; budget/match paths must stay **byte-identical**): **(1) criterion + pre-flight** —
SF_crit reduction (minimum of the smoothed recovered field, per-step +
envelope, solid-mode void mask), all-at-cap pre-flight solve + binding-region
classification; regbench case: SF_crit on the §14 cantilever, drift-guarded. **(2) M1 outer
loop** — goal plumbing (store goal kind, SF measure toggle, target input), secant-on-budget
against SF_crit, infeasible-path UX (banner + binding-cell view + delivered cap design).
**(3) M2 local redistribution** — only if a benchmark part shows M1 leaving clear material
on the table where layer-adhesion hotspots miss compliance hotspots.

**Status (2026-07-24): milestones 1 + 2 SHIPPED; M2 redistribution deferred as planned.**
Implementation notes for the record:
- Criterion lives in `filasim-core/src/strength.rs` (`sf_cells` → `smooth_masked` →
  `sf_min`; it was `sf_percentile` with a 0.2 %-volume trim until 2026-07-25, see
  dec. 4). Per-cell SF is the DISPLAY math
  (allowable and stress both scale by the cell's eps, so the factor cancels), and the
  SMOOTHED field is the SF field itself (nodal recovery + cell re-average) — the display's
  smooth-stress toggle recovers `sf` to nodes too, so dec. 2's "the number matches the plot"
  holds under smoothing as well. Masked-out ersatz-void cells neither contribute nor receive.
- The pipeline arm (`pipeline.rs`: `StrengthGoal` in `PipelineCfg`, `Preflight`/`SfEval`
  phases) reuses the match secant with two §17 twists: the accept band sits ABOVE target
  only (`STRENGTH_BAND` 5%), and the delivery is the lightest FEASIBLE snapshot seen — the
  all-at-cap pre-flight seeds that snapshot, so a below-target design can never ship. One
  extra guard vs match: when the secant asks for a budget at/below an UNTESTED floor it
  tries the floor itself (the floor is not a formal bracket here, it may be feasible).
- Envelope = per-step SF_crit, min over included steps (extra steps re-solve cold per pass
  via `simp::step_displacement`, with their own self-weight when §16 accel is active).
- Infeasible UX: `warnbanner` in the Results inspector + skin-share diagnosis
  (`bindingSkinShare` > 0.5 ⇒ skin-limited copy) + "show the critical region" button = the
  SF field with the legend banded in 2 at the target (red = below target).
- regbench anchors: `strength_sfcrit_solid` / `strength_rawmin_solid` /
  `strength_sfcrit_infill` (the infill one must equal eps × solid — the Gibson–Ashby
  allowable wiring); smoke: infeasible path (all-at-cap, 0 iterations) + feasible path.
  Budget/match stayed byte-identical (regbench +0.000% across the board).

**Deferred.** Per-step SF targets (survival vs service margins — additive later if asked).
M2 until M1 proves wasteful. Exposing the trim fraction. Textbook adjoint/p-norm machinery.

## 18. STEP import via meshStep (plan 2026-07-24)

**Problem.** STEP import has been DEACTIVATED since 2026-06 (§9): truck parses B-rep
exactly but tessellates it badly (developable-face slivers, twisted trimmed periodic
faces). Meanwhile meshStep (own tool, `Desktop/Coding/meshStep`, published as
`meshstep` on npm) became a production-quality pure-TS STEP→mesh importer: watertight
per body by construction, seam-unwrapped periodic faces (exactly truck's failure mode),
97.2% watertight on 10k wild ABC files, zero runtime deps, AGPL-3.0-only, same owner.
It also preserves B-rep provenance far beyond what truck's path carried: per-triangle
CAD face + solid + instance ids, per-face analytic surface info (type/axis/radius/
area/normal), STYLED_ITEM colors, assembly structure with part names, and opt-in
analytic measure geometry (B-rep edges incl. adjacency).

**Existing machinery (found 2026-07-24).** filasim already has a latent STEP pipeline,
gated only by the `step` cargo feature + the `.stl,.3mf` accept list: `import_any`
dispatches on `ISO-10303-21` (filasim-wasm lib.rs:213-228), `Model` carries
`cad_face_of_orig`, `cad_segmentation` seeds one patch per CAD face, `use_cad_faces` /
`has_cad_faces` exist, and the web side models `LoadedModel.hasCadFaces` +
`segSource: "angle" | "cad"`. Everything downstream of "(mesh, face-of-tri) exists"
already works — the only broken piece is tessellation, which is exactly what meshStep
replaces.

| # | Topic | Decision |
|---|-------|----------|
| 1 | Tessellator | **meshStep in JS replaces the truck path entirely.** truck's `step` feature stays off and is DELETED once M1 is proven (with its debug exports + `stepnode_test.mjs`). No Rust-side STEP parsing remains. |
| 2 | Where it runs | **Dedicated import worker** (`import.worker.ts`), not the engine worker. `importStep` is synchronous/CPU-bound (worker is meshStep's documented pattern): `onProgress` → progress UI, `signal: AbortSignal` → cancel at work-unit boundary, `worker.terminate()` → hard-stop for pathological faces. Buffers transfer zero-copy to the engine worker. |
| 3 | wasm boundary | **New entry `Model::new_from_mesh(positions: f32, indices: u32, face_of_tri: u32, solid_of_tri: u32)`** landing exactly where `import_any` returns today, so refinement → segmentation → voxelize → attach → solve are untouched. Float64→Float32 downcast in JS before transfer. STL/3MF paths must stay **byte-identical** (regbench `--check` + wasm smoke). |
| 4 | Tessellation params | Defaults from meshStep's `estimateStepSize` + `autoTessellation(diag)` (≈0.01 mm deviation / 1 mm maxEdge at 100 mm parts), with `maxEdge` additionally clamped relative to expected voxel pitch h (attach radius is 0.9·h; selection + result display live on the triangles). Keep `capped_edges` as a safety cap in M1, measure, then drop for the meshStep path. `remesh` stays off (meshStep docs: it degrades the raw pipeline). |
| 5 | Identity contract (meshstep@0.1.1, README "Identity & versioning") | Mesh layout (vertex positions/counts/triangle order) is **NOT stable across meshStep releases** — bit-identical only within a version. But `faceOfTri`/`solidOfTri`/`MeasureEdge.edgeId` are the **STEP file's own entity record numbers** (#123 of ADVANCED_FACE / EDGE_CURVE) — stable across versions for byte-identical input by construction. Rules: (a) persist entity ids + `SolidInstance.instance`, never triangle/vertex indices; (b) record the runtime `VERSION` export (trust it over the lockfile — stays correct under npm link) with any cached mesh, invalidate on mismatch; (c) hash the STEP bytes — ids are stable per FILE, not per design; a CAD re-export renumbers ⇒ "re-bind selections"; (d) tolerate the meshed-face set growing between versions (coverage improves, ids never renumber). |
| 6 | Persistence | `.filasim` embeds the original `.step` bytes verbatim (existing §12 pattern) + manifest additions: meshStep `VERSION`, tessellation opts, STEP byte hash. **BC selections on STEP models are stored as CAD face-id sets when the selection is exactly a union of patches** (the common case under CAD segmentation); brush/sub-face selections fall back to triangle indices (valid because open re-tessellates with the pinned version + saved opts). Additive in PROJECT_SCHEMA=1. |
| 7 | Diagnostics | meshStep `diagnostics`/`stats` map onto the existing import `notice` mechanism (dropped/unsupported faces, heuristic repairs). **Open shells (`openSolids` / `openEdges`) are a hard warning**: the winding-number inside-test (voxel.rs) is unreliable on open geometry. `diagnostics.ok` = quiet import. |
| 8 | Dependency & license | **Exact pin `meshstep@0.1.1`** (no caret — see dec. 5's version sensitivity); `npm link` for dev. Both projects are AGPL-3.0-only with the same owner, so the copyleft is compatible; NOTE for the §2.14 license model: any filasim **commercial exception must bundle a meshstep exception** — decided here deliberately, not discovered later. Zero runtime deps (the one LGPL package in meshStep is a dev-only test harness, never shipped). |
| 9 | Metadata exploitation (M3+) | `FaceInfo.surface` gives exact cylinder/cone axis+radius ⇒ bearing BC skips the least-squares fit on STEP input, and the axis feeds the planned rotational loads (§16 dec. 9). `meanNormal` ⇒ exact place-on-face / pickdir. `area` ⇒ true pressure↔force readouts. `colors` ⇒ CAD-colored viewer (later: paint-faces-in-CAD as BC markup — separate decision). `structure` ⇒ named body picker for multi-body. `measure.edges` (opt-in) revives the deferred edge/vertex-BC idea with EXACT B-rep edges: all edges included, B-splines as kind "other" with exact polylines + adjacent faceIds; respect `MeasureGeometry.truncated` (>250k edge instances ⇒ freeform polylines stride-decimated — selection identity still valid, but never render/snap to the decimated polyline). |

**Build order** (each milestone shippable; gates: regbench `--check` byte-identical for
STL/3MF, wasm smoke test, plus frozen STEP fixture hashes once M1 lands):
**(1) Ship STEP import** — import worker with progress/cancel, `new_from_mesh`,
`.step/.stp` accept lists (App.tsx/StepPanel.tsx), CAD segmentation active by default,
size-adaptive tolerances, diagnostics→notices + open-shell hard warning; then delete
the truck path. **(2) Persistence** — manifest fields (dec. 6), face-id BC storage +
re-bind-on-hash-mismatch UX. **(3) Analytic BCs** — exact bearing axis/radius, exact
place/pickdir normals, area readouts, cylindrical-patch → bearing suggestion.
**(4) Presentation** — CAD colors, named body picker, `vertexNormals` shading.
**(5, unscheduled)** — edge BCs via `measure.edges`, color-as-markup convention,
planar-face seeding for §15's orientation Apply flow.

**Deferred.** Assembly handling beyond "analyze one body/occurrence" (instances are
baked; a picker is enough for now). Per-face tessellation density (meshStep has no
override — REQUEST it from the maintainer if a real part needs it rather than
designing around a guess). AP242 PMI (not in meshStep, none planned).

**Status (2026-07-24): M1 SHIPPED.** Implementation notes:
- `meshstep@0.1.1` exact-pinned in web/. `import.worker.ts` runs `importStep` with
  file-derived opts (`estimateStepSize` → `autoTessellation`, normalDeviation 15°) —
  deterministic per file so project reopen reproduces the mesh bit-identically (verified
  on 3 real models incl. GoProHandlePod). It densifies the sparse entity ids and returns
  the dense→entity tables (`faceEntityIds`/`solidEntityIds`) + diagnostics + VERSION in
  `StepMeshPayload` (`StepImporter.ts`); the dec. 4 pitch-coupled maxEdge clamp was NOT
  implemented — it would couple tessellation to session state and break reopen
  determinism, and the auto maxEdge (~diag/100) already tracks the default pitch.
- wasm: `Model::new` refactored into a shared `from_import` tail + new
  `Model::from_mesh(positions, indices, face_of_tri, solid_of_tri, name)` (DENSE ids);
  regbench +0.000% across the board (STL/3MF byte-identical).
- Wire: `loadMesh` op (original .step bytes ride along for save); `openProjectModel`
  returns `stepModel` bytes for STEP projects (header sniff) and the store re-tessellates
  → `loadMesh` → `openProjectRestore` unchanged. `model.step` entry in `.filasim`.
- UI: `.step/.stp` accepted everywhere (App/StepPanel/TopBar/drop), busy chip narrates
  parse/tessellate%/finalize, Stop terminates the import worker (`importingStep` state);
  cancelled imports are not errors. Diagnostics → notices (missing faces / open bodies
  hard warning / heuristic repairs), meshStep version + face count in the nerd log.
- Verified: regbench PASS, tsc + vite build clean (import worker = own 210 kB chunk),
  smoke test extended (`from_mesh` validation, CAD-patch segmentation/solve, `.step`
  project round-trip incl. design+result restore onto a `from_mesh` model), Node e2e
  seam test on cylinderWithHole/chamferFillet/GoProHandlePod (bit-identical re-imports).
- NOT yet done: truck-path deletion (waiting for real-world proving per dec. 1), M3+
  metadata features. Note: GoProHandlePod tessellates to ~665k triangles at auto opts —
  heavy but works; a coarser display budget knob may be worth it now that opts persist.

**Status (2026-07-24): M2 SHIPPED.** Persistence per dec. 5/6, all additive in
PROJECT_SCHEMA=1 (absent fields = M1/STL behavior):
- Manifest gains `step: { meshstepVersion, opts, sha256 }` (`StepManifestInfo` in
  `stepSelection.ts`); the import worker now hashes the original bytes (SHA-256) and
  accepts an opts OVERRIDE — reopen replays the SAVED opts, so a re-anchored
  `autoTessellation` in a future meshStep can't shift the mesh.
- Per-BC `faceIds` (STEP entity record numbers) written when the selection is exactly a
  union of CAD faces (`selectionFaceIds`); brush/partial selections stay tris-only. The
  whole-face test runs against the LOAD-TIME CAD segmentation (`stepInfo.cadPatchIds` +
  `faceEntityIds` kept in the store), so it's immune to the live segSource.
- Open: same version + same opts ⇒ tris used verbatim (bit-identical mesh). Version or
  hash mismatch ⇒ `trisForFaceIds` re-derives triangles from entity ids on the fresh
  tessellation (unknown ids skipped — dec. 5d coverage growth); tris-only selections
  empty out with a "re-paint N selections" notice; design/results restore is SKIPPED
  (they were computed on a mesh that no longer exists — honest path), noticed + logged.
  A re-save then writes the current version/opts/hash, healing the project.
- Verified: `npm run check:step` (new harness `web/scripts/check-step-selection.mjs` —
  whole-face detection, brush fallback, never-fabricate on stale indices, cross-version
  entity-id rebind incl. changed dense order + grown faces), tsc + vite build clean.
  No Rust/wasm changes (regbench/smoke unaffected).

**Status (2026-07-24): M3 SHIPPED** (analytic BCs from CAD metadata; JS-only again):
- Import worker now emits `faces: StepFaceInfo[]` (dense-indexed: surface type, analytic
  origin/axis/radius/semiAngle, area mm², mean normal — all IMPORT-frame), gated on every
  assembly instance being meshed in place (`frame === null`); placed assemblies get
  `faces: null` rather than ambiguous part-local geometry.
- `stepInfo` gains `faces` + `toWorld`, the cumulative rigid transform since import, kept
  in lockstep at every `engine.transform` site (loadFile auto-center, `transformModel`
  rotate/place incl. plate seating, project-open replay) via `composeTransform` — the
  same composition `Model::transform` accumulates. Analytic values compose through it at
  USE time, so orientation changes never stale them.
- **Exact bearing**: `updateBcTris` prefers `exactCylinderForSelection` over the
  least-squares fit — non-null iff the selection is exactly a union of cylindrical faces
  sharing one axis + radius (split-bore halves combine; parallel-offset axes, radius
  mismatch, cones, partial faces all fall back to the fit). `CylFit.exact` marks
  provenance; the bearing readout shows "· CAD-exact". Wasm `add_bearing` still fits
  internally — the exact path upgrades validation + display; solver unchanged.
- **Area readout**: pressure BCs show exact CAD face area + resultant p·A when the
  selection is a whole-face union (`selectionCadArea`).
- **Bearing suggestion**: a plain force whose selection is a cylindrical bore/boss gets a
  "consider a Bearing load" hint (`CylindricalBearingHint`).
- DELIBERATE deviation from dec. 9: place/pickdir exact-normal snapping NOT built — a
  planar face's tessellated normal already equals the analytic normal to float precision,
  so there is nothing to gain; snapping pickdir-on-cylinder to the AXIS would change
  semantics (radial → axial) and is deferred as a UX question. Cone support for bearing
  likewise deferred until a real part needs it.
- Verified: check:step extended (exact-cylinder accept/reject matrix, split bore, area
  sums, transform composition vs sequential application), tsc + vite build clean, e2e
  seam test recovers the real bore's r=5.000 exactly + CAD area on cylinderWithHole.step.
  `faces` is NOT persisted in the manifest — it's rebuilt from the re-import on open.

**Status (2026-07-24): truck path DELETED + M4 SHIPPED + viewport parity.**
- **Truck deletion** (dec. 1 gate passed — real-part proving): `step.rs`, the `step`
  cargo feature (both crates), all truck deps, the wasm debug exports
  (`step_import_stl/info`, `step_face_report/stl`), `stepbench`, `stepnode_test.mjs` and
  the deny.toml/build-wasm notes are gone. `import_any` handles 3MF/STL only; a STEP
  byte-stream reaching it errors clearly. Regbench +0.000%, full smoke suite green after
  the wasm rebuild.
- **M4 CAD colors**: worker resolves STYLED_ITEM colors to a palette + per-DENSE-face
  index (meshStep composes body→face already); `cadTriangleColors` bakes per-working-
  triangle LINEAR RGB (unstyled faces = the base grey) into `stepInfo.cadTriColors`;
  ColorManager takes them as the repaint/hover base under BC tints; "Colors" chip
  (shown only when the file has colors, default on).
- **Viewport (meshstep-viewer parity, works for STL and STEP alike)**:
  camera-following key light (offset up-right in view space per frame, static cool fill
  + hemi keep a world anchor); optional smooth shading; feature-edge overlay. "Edges"
  (default on) + "Smooth" (default off) chips beside Wireframe; edges show on the opaque
  setup surface, follow section clipping, and join the orientation-preview rotation.
  **Edge derivation (fixed same day — first cut hallucinated edges):** the working mesh
  is deliberately NON-CONFORMING (T-junction refinement), so any edge detection on it
  paints "open edge" noise across flat faces. STEP models therefore get their border
  segments computed in the IMPORT WORKER from meshStep's conforming welded mesh (exact
  CAD face borders, 0 false positives — verified 0 open edges on real models), kept in
  `stepInfo.featureEdges` (import frame) and re-pushed through `toWorld` on every pose
  change; STL/3MF derive edges on the ORIGINAL soup (`Model::original_positions`, a new
  wasm export — the as-imported conforming mesh, pose-followed), fetched by the store on
  load and after every transform: properly-shared (n=2) pairs with a dihedral angle
  above the `edgeAngle` setting (default 20°, chip-inline input, shown for STL only).
  First STL cut ran on the working mesh and MISSED creases on T-junction borders
  (subdivision differs across them → no exact partner) — same root cause as the STEP
  noise, fixed the same way: never detect edges on the refined mesh. **Smooth-shading creases match the edges** (same-day follow-up): STEP
  corners average only within their CAD face (tangent neighbors converge to identical
  border normals, so fillets shade seamlessly while true edges stay hard — the same
  behavior as meshStep's analytic vertexNormals); STL creases use the same `edgeAngle`.
- **M4 named-body picker: DEFERRED** — it is an analysis feature (engine-side body
  filtering to solve one body of a multi-body file), not presentation; revisit when a
  real multi-body workflow demands it. meshStep's `vertexNormals`/`structure` payloads
  stay unused for now (computed crease normals cover the shading need without the
  subdivision remapping).
- Verified: check:step extended with color-baking cases (sRGB→linear, grey fallback,
  null on colorless), tsc + vite build clean, regbench + smoke green post-deletion.

## 19. Part Topo — validation results on the optimized shape (2026-07-24)

Request (Stefan): after a Part Topo (solid topology) run, the validation solve's
fields (displacement / stress / SF) should display on the OPTIMIZED shape, not on
the original solid model. Previously the result views painted the field on the
original STL soup or the full-envelope voxel hull — smearing meaningless
near-void (SIMP-floor) stresses over carved-away material.

Decisions:
1. **Masked voxel hull, not the smooth isosurface.** The result surface for a
   Part Topo result is the analysis voxel hull restricted to the RETAINED cells —
   exact nodal displacements, honest per-cell fields. Sampling onto the smoothed
   marching-cubes body (interpolated through half-cut boundary cells) is a
   possible later polish, deliberately not built now.
2. **Mask = export membership.** `Model::solid_topo_keep()` reproduces
   `set_iso_threshold`'s cell set exactly: frozen load/support `anchor_cells` +
   `solid_keep_bins` connected-keep of design cells above the stored
   `opt.iso_threshold`. The displayed hull therefore matches the exported body
   cell for cell, and **follows the isosurface slider** (the store invalidates +
   refetches the hull on threshold changes while the body is the active result).
3. **Boundary values don't bleed.** With smoothed stress on, nodal recovery runs
   masked (`recover_nodal_where`, filasim-core) so retained-surface nodes never
   average in the carved cells' near-zero SIMP-floor stresses.
4. **Voxels-only surface.** The carved body has no STL skin, so activating a
   Part Topo `optimized` result forces `resultSurface: "voxel"` (post-optimize
   and on every result switch); the STL chip is disabled with an explanatory
   tooltip and `setResultSurface("stl")` is a no-op while the body is active.
   Baselines (uniform/solid/as-printed) and infill-mode results are untouched —
   they still render on the full part, either surface.
5. **Plumbing.** `voxel_results(solid_body)` / `voxel_result_field(kind,
   solid_body)` take the flag; the web derives it in ONE place
   (`resultIsSolidBody` in FieldServer: active result kind `optimized` +
   `optSummary.solid`) and threads it via the EngineSession mask provider and
   `FieldDisplayState.resultIsSolid`. Cache safety: every flag flip goes through
   a result switch or threshold change, both of which invalidate the voxel hull
   + field caches.

Follow-up (2026-07-24, Stefan): the DENSITY view now keeps the translucent
original-envelope ghost around the Part Topo cutaway body (it shows what was
carved away). Since d7399fd the hull was hidden in BOTH density and Regions
views to avoid moiré against the coincident retained faces; the density body's
`polygonOffset` already wins that depth test, so only the REGIONS view still
drops the hull. Verified visually in the live app (ghost stable across re-runs,
no moiré).

Known limitations (accepted for v1): the multi-step worst-case ENVELOPE still
reduces + renders on the STL surface (envelope reduction is forced to "stl");
the volumetric section-cap payload and interior-extreme markers are unmasked
(carved cells report ~0 stress / SF at cap, which never wins an extreme, so the
displayed ranges stay honest); the §15 orientation-sweep voxel layer view is
unmasked. Verified: regbench PASS (+0.000% everywhere), filasim-core tests,
tsc clean, smoke test extended (masked hull vs full, nodal max |u| matches the
verification solve, per-cell field alignment, hull follows `set_iso_threshold`).

## 20. Settings Optimizer — min-weight print settings for a target SF (interview 2026-07-25)

**Problem.** The graded/binary/solid optimizers answer "where should material go?",
but many users just print with uniform slicer settings and want the simpler
question answered: "what infill % and how many walls do I need so the part holds
with safety factor ≥ N — at the lowest weight?" That is a search over PRINT
SETTINGS (the §"As printed" model), not over a density field. Second driver:
fixed supports produce mesh-divergent corner stress that §17's smoothing only
partially suppresses — a settings search hammering on SF_crit needs
constrained-face singularities excluded properly and VISIBLY. (At the time §17
also trimmed the worst 0.2 % of volume, which suppressed some of it by luck;
that trim was retired 2026-07-25, making the explicit exclusion load-bearing.)

**Existing machinery (found 2026-07-25).** `engine.solvePrinted()` already solves
exactly one candidate: uniform `printInfill`% + perimeters × lineWidth walls +
topBottomLayers × layerHeight shells, returning mass (store.ts ~4564). The §17
criterion chain (`sf_cells → smooth_masked → sf_min`,
envelope-over-steps, SfMeasure toggle, infeasibility banner) is the evaluation
scalar. Composite skin (2026-06) represents sub-voxel wall bands on a fixed grid.
Store clamps: perimeters 1–8. `criterion_mask` currently lets clamp-face cells
compete in the percentile — no BC-specific exclusion anywhere.

| # | Topic | Decision |
|---|-------|----------|
| 1 | Placement | **Standalone panel** (Validate-Orientation-style) in **station 5 · Optimize**, not a fourth optimizer goal — the deliverable is SETTINGS + a sweep landscape, not a density field. It is an optimization ("what should I print?"), so it sits with the other optimizers, above the orientation sweep it sends the user to when layer adhesion binds. The winner is **retained as a selectable result** (kind `settings`) so the delivery can be plotted/sectioned like any other; **Apply** writes the settings into the print-settings fields and re-verifies. |
| 2 | Target | **Minimum safety factor** (app-wide SF convention, default 2.0) — NOT "MoS". One convention everywhere; the panel's number is the number the SF plot and §17 goal show. |
| 3 | Density axis | Search **10–70%**, deliver in **5% steps rounded UP** (conservative), SF re-verified at the snapped value. No >70% recommendations — above that the answer is "more walls" (Gibson–Ashby validity + skin-dominates regime). |
| 4 | Wall axis | Perimeters **2–8** (the store allows 1, the search does not — a single perimeter is not a print anyone ships, and the composite-skin model is least trustworthy when the wall band is thinner than a cell; the optimizer must not "save weight" by recommending it). lineWidth / layerHeight / pattern held at current settings (printer choices, not strength knobs). **topBottomLayers = ceil(perimeters·lineWidth / layerHeight)** — ceiling, so the adhesion-critical top/bottom shell is never thinner than the walls just justified. Apply overwrites the manual top/bottom value (that coupling IS the feature). |
| 5 | BC singularity exclusion (the singularity answer) | Exclude cells within a **physical, patch-scaled radius** of constrained nodes: `d = max(2 voxels, ~0.15 × patch characteristic diameter)`, constant in code beside `SF_TRIM_FRAC` (tunable, NOT in UI). Physical distance is mesh-stable (Saint-Venant: BC pollution decays over the patch scale; stress at fixed physical r converges under refinement) — a cell-count ring is not (it walks into the singularity as h shrinks, SF_crit recedes = the §17 dec. 4 disease). |
| 6 | Exclusion scope | **Fixed/displacement constraints AND §16 rigid mounts** (both artificial infinite-stiffness interfaces). **Force/load pads stay IN the criterion** — under-sized load introduction is a real failure mode the optimizer must not paper over; smoothing eats single-cell edge spikes there. (With the trim retired, a pad's own peak now reaches the number directly. Revisit if pads turn out to bind in practice; the honest fix would be excluding them VISIBLY like supports, not a statistical filter.) |
| 7 | §17 retrofit | Exclusion lives in **`criterion_mask` itself → shared by the §17 SF-target goal, this panel, and `binding_cells`** — one criterion, one number (§17 dec. 2 consistency, now internal too). Regbench strength anchors re-baselined DELIBERATELY (documented behavior change: SF-target designs near clamps come out slightly lighter). Excluded zone drawn greyed in the binding-cells view (never look like hiding stress); the raw SF display field stays untouched — the plot keeps showing the clamp hotspot, only the scalar ignores it. |
| 8 | Search | Opens with the **ceiling probe** (added 2026-07-25): the strongest print the band can make — most walls at the densest infill — is solved FIRST. If it misses the target, nothing lighter can hold it, so the search stops after **one solve** instead of walking every wall count to the same answer (`SearchOutcome.ceiling_stop`, narrated in the log and the infeasibility banner). It also puts the best-possible safety factor on screen within one solve, so the user can judge whether to keep waiting. Costs one extra solve when the target IS reachable (the top row is usually weight-pruned before it would be evaluated) — the deliberate price of the early exit. Then: per wall count, **bisect density on the 13-step grid** (≤4 solves each; SF monotone-increasing in both axes, physically expected — bisection is robust to small non-monotonic blips) with **weight pruning**: weight needs no solve (geometry × density), so wall counts whose minimum-density weight already exceeds the best feasible weight are skipped. Typically **15–30 as-printed solves** (× included load steps). Tie-break within ~1% weight → higher measured SF. |
| 9 | Sweep mesh | **One frozen grid for all candidates** (user's current resolution + snap state); each candidate's walls/shells enter via composite-skin fractions in `classify_cells` — apples-to-apples SF, neighbor warm starts. **Apply runs the standard As-Printed solve as final verification** under normal snap behavior; if snap shifts SF below target the panel says so rather than silently passing. |
| 10 | Landscape | Walls × density grid rendering **solved cells** (SF-colored, feasible/infeasible banding), unsolved dimmed, winner marked; per-wall-count "lightest feasible" cell outlined, with weight + SF in its tooltip. Drawn from the FIRST progress push — axes first, one cell per solve — so the run is legible while it happens, with the part **recoloring live** by each candidate's safety factor. **"Solve full map"** backfills the rest on demand. |
| 11 | Inherited from §17 | Load steps: SF on the **envelope**, every included step must meet target, weights never dilute safety (§17 dec. 5). SF measure: material / layer / both toggle, default both (dec. 2). Infeasibility: if 8 walls + 70% misses, deliver that best candidate + banner "best achievable is SF X" + binding-region diagnosis, escalation copy → reorient / stronger material / graded optimizer (dec. 6 pattern). Honesty copy at the target input (dec. 8). |

**Build order** (each milestone shippable; gated by regbench `--check` + wasm smoke
test; budget/match paths byte-identical, §17 strength anchors re-baselined once in M1):
**(M1) BC exclusion in `criterion_mask`** — constrained-node + rigid-mount seed set,
patch characteristic diameter, physical-radius dilation; greyed exclusion zone in the
binding view; regbench: new anchor for SF_crit-with-exclusion on a clamped cantilever
(drift-guarded), §17 anchors re-baselined with a NOTE in the baseline commit.
**(M2) sweep kernel + panel** — wasm sweep entry (frozen grid, per-candidate
classify + solvePrinted + SF_crit, warm starts), bisection/pruning driver, panel UI
(target input, run, winner card, Apply → store writeback + As-Printed verify).
**(M3) landscape + full-map backfill** — solved-cell grid heatmap, lightest-feasible
curve, "solve full map".

**Status (2026-07-25): M1 + M2 + M3 SHIPPED.** Implementation notes for the record:
- **Exclusion** lives in `filasim-core/src/strength.rs`: `bc_exclusion(grid, patches)`
  with `BC_EXCL_PATCH_FRAC = 0.15` / `BC_EXCL_MIN_CELLS = 2.0` beside `SF_TRIM_FRAC`.
  Radius per patch = `max(2h, 0.15·d_c)`, `d_c = 2√(A/π)` with `A = node_count·h²` —
  the node COUNT is mesh-dependent but the AREA is not, so the radius converges
  (O(1/n) from above). Distance is the exact Euclidean transform from the constraint
  NODES evaluated at CELL CENTERS (separable Felzenszwalb lower envelope with a
  half-cell output shift, O(cells) per patch), so no staircase bias is introduced.
  `criterion_mask` gained a `bc_excl` argument; **empty ⇒ the pre-§20 criterion**,
  which is why budget/match stayed byte-identical and the §17 regbench anchors did
  NOT need re-baselining (`bench_strength` builds its mask inline). New drift-guarded
  anchors instead: `strength_sfcrit_excl` / `strength_excl_cells` /
  `strength_excl_radius_mm`.
- **Scope (dec. 6), one deliberate extension:** Frictionless supports are excluded
  too. They are penalty-enforced kinematic constraints with the SAME `SPRING_FACTOR`
  as Displacement — a Displacement BC along a rotated axis — so excluding one and not
  the other would be inconsistent. The cylindrical support (2026-07-25) joins them for
  the same reason: its locks are the identical springs on the fitted cylinder's local
  axes, and an all-free one (which constrains nothing) excludes nothing. Elastic (Winkler) supports are NOT excluded (their
  whole point is a compliant mount that spreads the interface stress physically), and
  neither is any load pad.
- **Empirical note:** how much the exclusion moves SF_crit is strongly setup-dependent.
  A fully-supported flat end face is barely singular (the exclusion shifts SF_crit by
  well under 1 % on the §14 beam, in either direction); a small pad on a large body is
  where it earns its keep — and it became load-bearing once the trim was retired.
  This is expected: the radius scales with the PATCH, so a benign clamp gets a benign
  exclusion. The core tests assert what is universally true (mesh stability of the
  excluded volume, exact distances, mass untouched, scored set shrinks).
- **Sweep kernel**: `filasim-core/src/settings.rs` — `WallGeometry` (one
  `classify_cells` per wall count + its volume components, so every density on that
  row costs a multiplication, not a solve), `Sweep` (frozen grid, criterion mask built
  once, one multigrid cache reused across candidates on the primary step, extras via
  `step_displacement` with their own self-weight), and `search` (the dec. 8 ceiling
  probe first; then per wall count: probe the top density, then bisect; prune and BREAK
  once a row's 10 %-infill weight exceeds the best feasible + the 1 % tie band, since
  weight grows with wall count). Delivery is the lightest feasible candidate,
  tie-broken on measured SF; when nothing is feasible the strongest candidate ships
  with the honest ceiling.
- **Ceiling probe (dec. 8, added 2026-07-25 on use).** `search` opens by solving the
  top wall count at the top density and returns immediately with `ceiling_stop: true`
  when it misses the target. Considered and rejected: probing only AFTER the first row
  fails, which would save the extra solve in the feasible case — but the probe's
  second job is putting the best-achievable number on screen within one solve, and a
  user staring at a search that cannot succeed is the case worth optimizing. The core
  test pins the budget (one solve, one landscape cell, `pruned_walls` empty); the
  wasm smoke pins the plumbing on a restricted band.
- **wasm**: `Model::settings_sweep(opts, progress)` — `mode: "search" | "full"`, with
  `walls`/`densities` overrides so a ONE-candidate call re-verifies the applied
  settings on the (possibly re-snapped) grid. Optimize passes the union of every
  included step's exclusion into `PipelineCfg::bc_excl` and reports `bcExcludedCells`.
- **The criterion fields (dec. 7)**: `sfx` / `sfmx` / `sfzx` plot the CRITERION — the
  §17 chain itself: masked nodal smoothing with the BC singularity zone dropped (NaN →
  the renderer's neutral mask grey), independent of the display smoothing toggle. The
  plain `sf`/`sfm`/`sfz` stay raw and unmasked and keep showing the clamp hotspot.
  **This is what reconciles the number with the picture** (see below); the §17 "show the
  critical region" button and the §20 auto-view both switch to a `…x` kind, and the
  field chip always offers the criterion group.
- **"The reported SF is higher than the plotted one" (found in use, 2026-07-25).** Fixed
  in two rounds, and the second one is the interesting one.
  **Round 1** made the `…x` kinds plot exactly the field the number is reduced from
  (before, they were the plain field with grey bits) and reported the untrimmed minimum
  as a second number, "Lowest scored cell". That closed the *plot vs plot* gap but left
  the real one: on Stefan's hook the panel said **2.02** while the plot's marker said
  **1.64**, and "we trim the worst 0.2 % of volume" is not an answer a user can act on
  when they are about to print the thing.
  **Round 2 retired the trim** (§17 dec. 4). SF_crit is now `sf_min` — the minimum of
  the criterion field — so reported == plotted by construction and `rawMin` is a
  synonym kept only for API compatibility. Measured cost of the trim before removal:
  §14 beam `strength_sfcrit_solid` 4.5232 → 4.5159 (**−0.16 %**), smoke part 8.18 →
  7.92 (**−3 %**), the hook (**−19 %**) — the spread across those three IS the argument.
  The new mesh-dependence is surfaced, not hidden: `strength::riser_ratio` reports how
  fast the field climbs away from the binding cell, and above ~1.6 the panel and the
  dock both say a finer mesh will report less.
- **The dock had a FIFTH number** (same session): "Min safety factor" was the raw
  minimum of the display field on the surface soup — the readout a user checks after
  the optimizer promises SF ≥ 2, showing ~30 % less. `computeMinSf` now calls
  `criterion_sf` for both measures and reports the criterion, so the station that
  validates a goal uses the goal's own reduction. **Rule: one quantity, one reduction,
  everywhere it appears** — a "safety factor" that means something different in each
  panel is worse than no safety factor.
- **Panel**: `web/src/ui/SettingsOptimizer.tsx` in station 5 · Optimize — target +
  measure, winner card, honesty rows (solves, pruned wall counts, excluded cells), the
  walls × infill landscape with the lightest-feasible outline and the winner ring
  (weight and safety factor live in the cell tooltip — a weight column overflowed the
  panel and said less than the hover), "Solve full map" backfill (merged into the existing landscape,
  winner re-decided), and **Apply settings & verify**. `settingsSfTarget` persists
  additively in PROJECT_SCHEMA=1; the landscape itself is never saved and is dropped
  by every invalidation that stales results.
- **Live run (2026-07-25 revision)**: the sweep's first progress push carries the
  landscape AXES, so the grid draws before the first solve and fills in one cell per
  candidate; every later push carries that candidate's own per-soup-vertex SF field,
  and the part recolors with it (scale pinned to [0, 2×target] so candidates are
  comparable frame to frame). `Sweep::evaluate_keep` returns the primary displacement
  + eps for exactly this, which also lets the winner be promoted without holding every
  candidate's ndof vector. `enterSettingsPreview` also switches the field KIND to the
  criterion SF (and bands it at the target) before the first push — as §15 does with
  `sfz`. Found in use: it did not, so the legend kept the previous field's name and the
  live safety factors were labelled "Displacement |u|, mm". **A legend that names the
  wrong quantity is worse than no legend**; any preview that pushes values must claim
  them. Set directly, not through `setResultField` — that re-fetches from the live
  solution, which the sweep is busy replacing.
- **The winner is a RESULT**: after the search the engine re-solves the winner (one
  extra solve, cheaper than retaining 30 displacement fields) and leaves it as the live
  solution; the store stashes it under a new `settings` result kind and selects it, then
  switches to the criterion SF field banded at the target with the min/max markers on —
  so the panel's number and the red spot in the viewport are the same fact. That result
  stales only on loads/material (it carries its own walls/infill in its provenance) and
  survives an optimize run. The banded criterion plot is a VIEW of that delivery, not a
  new global default: `criterionViewPrev` snapshots the field/banding/legend/extremes it
  replaced and `selectResult` restores them the moment another result is picked.
- **Verification bug found in review**: Apply originally re-ran a one-candidate
  `settings_sweep` to re-measure SF_crit, which calls `setBcs` → `clear_bcs` → **drops
  the engine's solution**, leaving the As-Printed run the user was looking at with no
  stress/SF fields at all (displacement only, from the client-side buffer). Replaced by
  `Model::criterion_sf(measure)`: the §17 chain over the §20 mask on whatever result is
  LIVE — no solve, no assembly, nothing invalidated — which also returns the worst
  scored cell's world position for the pin. Smoke-tested as read-only and repeatable.
- Verified: `cargo test -p filasim-core` (73 lib + all integration), regbench `--check`
  PASS at +0.000 % on every pre-existing anchor, `tsc` clean, wasm smoke test extended
  (criterion fields grey only the support zone and match the base field elsewhere; the
  axes arrive before any solve; every candidate pushes a live SF field; the search stays
  a strict subset of the 7 × 13 map; shells ceil the walls; no lighter feasible candidate
  passed over; `criterion_sf` reproduces the winner's SF, reports where it binds, and is
  read-only).

**Deferred.** Per-step SF targets (as in §17). Exposing the exclusion radius in the UI. lineWidth/layerHeight as search axes. Non-uniform per-region settings
(that's what the graded optimizer is for). Warm-starting the sweep's extra load steps
(only the primary step reuses its multigrid hierarchy today).

## 21. Panel copy & fold conventions (2026-07-25)

**Problem.** Station 5 had grown into a wall of text: three optimizers stacked
open (infill, print settings, orientation), each control carrying a paragraph of
`.dim small` under it. ~2000 px of scrolling before the first click, and the
explanation crowded out the instrument. The other stations were drifting the
same way.

**The rule (applies to every step panel).**
- A control shows its **name, its live value, and at most ONE short line** — and
  that line only if it changes with the state (a computed wall thickness, a
  capped filter, a warning that actually fired). Static explanation is not panel
  furniture.
- Everything that **explains, qualifies, or caveats in the abstract** moves into
  a hover card: `<InfoTip help={…}/>` at the end of the group label, or
  `<HelpTip>` wrapped around the control it describes. Copy lives in
  `ui/helptext.ts` (per station), next to the existing `BC_HELP` in `bcmeta.ts`.
  Cards are for reading; panels are for operating.
- The ⓘ is a real button: hover or keyboard-focus shows the card after the usual
  delay, click toggles it (touch + deliberate taps), blur dismisses.
- **Sub-sections fold** (`ui/Section.tsx`). A folded section still reports itself
  through a live badge — `30 %`, `SF ≥ 2`, `best 1.84×`, the active constraints —
  so nothing is hidden, only collapsed. Station 5 opens on the infill optimizer;
  Constraints (self-supporting / symmetry / min member), Optimize print settings
  and Optimize orientation start folded. Fold state is remembered per title for
  the tab's lifetime, so stepping 5 → 4 → 5 does not re-fold what you were using.

**Result.** Station 5 is ~520 px with everything reachable in one screen (~870 px
with every section expanded), and the same scheme now runs across stations 1–6
plus the build-sim panel. No behaviour or engine change — copy, layout and two
small components only.
