# Smart Infill Generator — Design Document

*Resolved via design interview, 2026-06-10. Working name: Smart Infill Generator (naming open).*

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
| 3 | Input formats | **STL + 3MF in v1.** STEP in v1.x via lazy-loaded OpenCASCADE WASM module (BREP faces become selectable surfaces). |
| 4 | Surface selection | **Auto-segmentation** (region-growing across edges with dihedral angle < ~30°, slider-adjustable) makes CAD-derived patches one-click selectable. **Brush/lasso + click-to-grow fallback** for organic meshes. |
| 5 | Loads & BCs (v1) | Fixed support, **elastic support** (Winkler foundation, bedding modulus k in N/mm³, σ = k·u — area-consistent axis springs per node; added 2026-06 because rigid Fixed patches artificially stiffen the part and produce edge stress singularities), surface force (total N over patch, presets: normal / global −Z), pressure, gravity/self-weight, frictionless support. *Note: frictionless on arbitrary (non-axis-aligned) patches via penalty/transformed constraints along averaged patch normal.* |
| 6 | Under-constraint check | Pre-solve: rank test of the 6 rigid-body modes against the constraint set + connected-component (floating island) check. On failure: **block the run and animate the offending rigid-body motion** so the user sees what's unconstrained. |
| 7 | Material model | **Walls + infill core.** Boundary voxels get solid-material skin stiffness (wall count × line width; defaults 2 × 0.45 mm, plus top/bottom shells). Interior voxels get per-pattern Gibson-Ashby law **E(ρ) = E₀ · c · ρⁿ**. |
| 8 | Optimization | **Continuous SIMP-style compliance minimization** under mass constraint using the *physical* E(ρ) (no artificial penalization — graded infill is the one case where intermediate density is printable). Optimality-criteria updates, ~50–100 multigrid solves. Then discretize to bins → **final verification solve** with binned densities + walls → report. |
| 9 | User control | **Infill-budget slider** ("X %" = target MEAN INTERIOR infill density, 10–70 — same scale as a slicer's uniform infill setting; the solid skin comes on top; revised 2026-06 from a total-mass % so low values make sense and the reference comparison is honest) + **comparison card**: "vs X% uniform infill at the same weight: +Y% stiffer", stiffness retained vs solid (%), mass, max displacement. **Goal "match uniform stiffness"** (2026-06, the dual problem): lightest design as stiff as a uniform X% print — one uniform solve sets the target compliance, then a guarded secant on the budget (warm-started passes, ≤5) lands the BINNED design within 2%; card leads with "same stiffness as X% uniform: −N% weight" (measured −28% on the smoke beam at 35%). v1.x: "solve for target displacement" (same mechanism, displacement target). |
| 10 | Density bins | **3 bins by default, values auto-placed** (revised 2026-06): the bottom level is PINNED at the 10 % printability floor ("just so it prints" — gyroid top surfaces sag below ~10 %), upper levels by strain-energy-weighted 1-D clustering in stiffness space E(ρ), and assignment is anchored at the optimizer's field with a bisected mass multiplier so the budget survives quantization. Rationale: E(ρ)=c·ρⁿ with n>1 is convex → stiffness per gram grows with density (the SIMP bang-bang argument), so load-bearing levels belong high, not at the histogram mean. Measured on the cantilever fixture: +15.2 % vs uniform at equal mass (was +13.9 % with plain density-space k-means). Cap 70 % (+ "consider solid here" flag for capped hot spots). Floor/cap and a manual level list are user-editable in ⚙ Settings (manual levels let calibrated densities be used verbatim; the mass-true assignment works for any level set). **Binary mode** (2026-06): interior is either the binary floor (default 5 %, printability) or 100 % solid — the optimizer runs SIMP-penalized (p = 3) so the field converges to black/white before quantization, while verification uses the calibrated pattern law (exact at both endpoints); export can pin object-level `internal_solid_infill_pattern` (rectilinear/concentric). Measured on the smoke beam at 30 %: +40 % stiffer than uniform at equal mass. Part's own infill setting = lowest bin; modifiers = higher bins. |
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
  layers (`classify_cells` uses layers = round(wall/h)); hard 4M-cell cap, snap abandoned
  when even k = 1 would exceed it. Stated approximations: nominal skin thickness exact
  only on flat faces (voxel staircase on curves), ONE isotropic skin thickness (real
  top/bottom shells are layers × layer height — not modeled separately yet), homogenized
  infill, no FDM anisotropy. Print properties (perimeters, line width, pattern, infill %)
  live in step 3 "Properties", shared by verify, optimizer and export — no duplicates.
- **Materials:** presets PLA, PETG, ABS, ASA (E₀, ν, density, tensile strength σₜ),
  user-editable. The safety-factor plot (2026-06: σₜ·rel(ρ)/σᵥM per cell, graded infill's
  allowable scaled with the same Gibson-Ashby factor as its stiffness, inverted colormap
  so red = critical low) is an ADVISORY readout, never a certified safety factor — FDM
  anisotropy and layer adhesion are not modeled.
- **Project persistence:** single JSON project file (embedded mesh + setup) download/load;
  auto-save to IndexedDB.
- **Out of scope v1:** assemblies/multi-body, print-orientation anisotropy in the solver,
  thermal/dynamic loads, mobile browsers.

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
- [ ] **Name/branding** for the tool.
- [ ] Minimal `project_settings.config` experiment (what Orca tolerates) — Phase 4.
- [ ] Orca/Bambu/Prusa version test matrix definition.
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
