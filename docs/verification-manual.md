<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com> -->

# filaSim — Verification & Validation Manual

*Engine version: `filasim-core` (master). Document revised 2026-06-14.*

This is the companion to the [Theory Manual](theory-manual.md). It documents how
the engine is checked against **known answers** — closed-form solutions, textbook
stress-concentration factors, established-FEA comparisons, and format
conformance — and defines the **regression battery** that should run regularly so
both the team and users can trust the tool.

It mirrors what ANSYS/Abaqus ship as a *Verification Manual*: each case states the
geometry, material, loads/BCs, the analytic/reference answer with its formula, and
the acceptance tolerance.

**Terminology.** *Verification* = "are we solving the equations right?"
(analytic/textbook checks). *Validation* = "are we solving the right equations?"
(comparison to other established FEA and, eventually, physical tests).

---

## Table of contents

1. [V&V philosophy and tiers](#1-vv-philosophy-and-tiers)
2. [How to run the suite](#2-how-to-run-the-suite)
3. [Tier 1 — Analytic verification (CI)](#3-tier-1--analytic-verification-ci)
4. [Tier 2 — Meshing / discretization benchmarks](#4-tier-2--meshing--discretization-benchmarks)
5. [Tier 3 — Printed-material (composite) verification](#5-tier-3--printed-material-composite-verification)
6. [Tier 4 — Interoperability / format golden files](#6-tier-4--interoperability--format-golden-files)
6b. [Tier 7 — Displayed surface-stress accuracy](#6b-tier-7--displayed-surface-stress-accuracy)
7. [Tier 5 — Cross-code golden comparison (planned)](#7-tier-5--cross-code-golden-comparison-planned)
8. [Tier 6 — Physical testing (planned)](#8-tier-6--physical-testing-planned)
9. [The Standard Validation Battery](#9-the-standard-validation-battery)
10. [Measured accuracy envelope](#10-measured-accuracy-envelope)

---

## 1. V&V philosophy and tiers

The validation bar (`DESIGN.md §13`): **solver unit tests vs analytic solutions
in CI**, **golden comparisons vs established FEA on ~5 representative parts**, and
physical testing as post-launch content (not a release gate). The suite is
organized into seven tiers by what they prove:

| Tier | Proves | Runs | Status |
|---|---|---|---|
| 1 | The FE math is correct (closed-form agreement) | CI, every commit | ✅ implemented |
| 2 | The voxel/cut-cell discretization is accurate & robust | manual benchmark | ✅ implemented (printing harness) |
| 3 | The printed-material (skin + homogenized infill) model is correct | CI | ✅ implemented |
| 4 | Exports/imports are byte-correct for slicers | CI | ✅ implemented |
| 5 | Whole-part results match an established FEA code | manual, periodic | ⏳ planned |
| 6 | The model matches physical reality | offline, post-launch | ⏳ planned |
| 7 | The DISPLAYED surface stress is accurate (not just the cell field) | manual benchmark | ✅ implemented |

All Tier-1/3/4 tests are first-party (no GPL test deps) and assert against
formulas or exact structure, so a regression fails the build.

---

## 2. How to run the suite

```bash
# Tier 1, 3, 4 — the CI suite (fast, assertion-backed). Run on every change.
cargo test -p filasim-core

# Tier 2 — the meshing-convention benchmark harness (prints tables, mostly
# #[ignore]d so it does not run in normal CI). One command re-validates the
# cut-cell convention against analytic/textbook truth.
cargo test -p filasim-core --test meshbench -- --ignored --nocapture

# Phase-1 exit-criterion benchmark (throughput + analytic cantilever check).
cargo run -p filasim-core --release --bin bench            # add --small to skip the 1M-cell run
# Optional: drop a 3dbenchy.stl in the working dir for the thin-shell case.

# Tier 7 — displayed surface-stress accuracy (what the app paints on the part),
# plus the exact-cut-cell cost benchmark. Also #[ignore]d.
cargo test -p filasim-core --test surfstress -- --ignored --nocapture

# STEP tessellation regularity harness (needs the `step` feature).
cargo run -p filasim-core --features step --bin stepbench -- "model.step" "model.stl"
```

Tests that need an external fixture (`Cube.3mf`, `3dbenchy.stl`) **self-skip**
when the file is absent, so the core suite is hermetic.

---

## 3. Tier 1 — Analytic verification (CI)

These run on every commit (`crates/filasim-core/tests/validation.rs`,
`phase2.rs`, `phase3.rs`). Each compares the FE result to a **closed-form**
solution. Tolerances are the actual acceptance criteria in the code.

### 3.1 Element & operator correctness

| # | Case | Reference | Tolerance |
|---|---|---|---|
| 1.1 | **Element matrix properties** — `KE(E=1500, ν=0.3, h=0.7)` | symmetric; all **6 rigid-body modes** (3 transl. + 3 rot.) lie in the null space `KE·m = 0` | symmetry `< 1e-8`; null space `< max\|KE\|·1e-10` |
| 1.2 | **Matrix-free apply == dense assembly** — 3×2×2 grid, mixed void/gray/solid, fixed plane | dense Gaussian-elimination assembly is the gold reference (f32 & f64 paths) | f32 `< 2e-5·\|y\|`; f64 `< 1e-12·\|y\|` |
| 1.3 | **MGCG == dense direct solve** — same grid, random RHS | dense partial-pivot solve | `< 1e-6·‖x‖` (MGCG to tol 1e-10) |

### 3.2 Patch tests (exactness)

| # | Case | Reference (closed form) | Tolerance |
|---|---|---|---|
| 1.4 | **Uniaxial patch test** — 4×2×2, roller BCs, uniform traction σ=10 on the x=L face as consistent nodal loads | exact linear field `ux=σx/E`, `uy=−νσy/E`, `uz=−νσz/E` | **every DOF `< 1e-8`** (machine-exact) |
| 1.5 | **Frictionless rollers reproduce the roller patch test** — 4×2×2 bar, three frictionless faces + axial force | `ux = σL/E = 10·4/1000` | `< 3%` |
| 1.6 | **Displacement axis-locks reproduce the roller patch test** — same bar, single-axis locks on three faces; plus a negative case (lone X-lock must NOT fully constrain) | `ux = σL/E` | `< 3%`; negative case must report under-constrained |

### 3.3 Beams and bars

| # | Case | Reference (closed form) | Tolerance |
|---|---|---|---|
| 1.7 | **Cantilever vs Timoshenko + mesh convergence** — solid box L=80, b=h=8, tip load F=−10 N, at 8 and 16 cells through thickness | `δ = FL³/(3EI) + FL/(κGA)`, `κ=10(1+ν)/(12+11ν)` | coarse ratio ∈ [0.90, 1.02]; refined ∈ [0.95, 1.02]; **must converge** (refined closer than coarse) |
| 1.8 | **Attach→assemble→check→solve end-to-end cantilever** — 40×6×6, fixed face, tip load (loose Timoshenko) | `δ = FL³/(3EI) + FL/(κGb·h)` | ratio ∈ [0.85, 1.10]; under-constrained variant must fail the check |
| 1.9 | **Self-weight cantilever** — 40×6×6, gravity g=9810, ρ=PLA | uniformly-loaded cantilever `δ = qL⁴/(8EI)`, `q=ρgA` | ratio ∈ [0.85, 1.10] |
| 1.10 | **Uniaxial column stress** — 8×8×16, clamped base, 64 N top (−1 MPa) | `σzz=−F/A=−1`, `σ_vM=1`, `σxx≈0`, `εzz=σ/E=−5e-4` | σzz/vM `< 0.06`; σxx `< 0.08`; εzz `< 3e-5` |

### 3.4 Supports

| # | Case | Reference (closed form) | Tolerance |
|---|---|---|---|
| 1.11 | **Winkler elastic foundation settlement** — 10×10×20 column, ν=0, k=10 N/mm³ on the base, σ=0.5 MPa on top (springs only, no Dirichlet) | base settles `σ/k = 0.05 mm`; top = settle + `σL/E = 0.005 mm` | `< 8%` at base and top |
| 1.12 | **RBM rank test** — full fixed plane / single point / two collinear points / three roller planes | analytic free-mode count & axis (e.g. two x-axis points ⇒ free rotation about x) | free-mode detection; rotation axis `> 0.99` aligned |

### 3.5 Geometry / robustness

| # | Case | Reference | Tolerance |
|---|---|---|---|
| 1.13 | **Voxelizer sphere volume** — r=10 sphere at h=0.5 | `V = 4/3·πr³` | `< 4%` |
| 1.14 | **Winding-number robustness** — closed / open (hole punched) / inverted-normal box | interior w≈1, exterior w≈0; open mesh classifies interior; flipped mesh ⇒ same solid count | analytic w ranges; flipped == normal solid count |
| 1.15 | **STL roundtrip & dirty input** — binary/ASCII, "solid"-prefixed binary, degenerate/NaN triangles | exact triangle counts and bounds | exact equality |
| 1.16 | **Island detection** — two separated boxes vs one | `islands == 2` / `1` | exact |
| 1.17 | **Surface segmentation** — box / sphere at 30° | box → 6 patches; smooth sphere → 1 | exact |
| 1.18 | **BVH closest-triangle == brute force** — 200 random queries | brute-force distance | `≤ 1e-9·(1+d)` |

### 3.6 Optimization & post-processing invariants

| # | Case | Reference / invariant | Tolerance |
|---|---|---|---|
| 1.19 | **Optimized bins beat uniform at equal mass** — 60×10×10 cantilever, budget 35%, 3 bins | floor pinned at 0.10; top level > 0.45; binned mean ≈ target; compliance gain | gain `> 1.03`; mean `< 0.03`; floor exact |
| 1.20 | **Binary mode beats uniform** — same fixture, {floor, solid}, SIMP p=3 | values ∈ {0.05, 1.0}; gain at equal mass | gain `> 1.10`; mean `< 0.05` |
| 1.21 | **k-means recovers separated levels** — synthetic 0.12/0.38/0.66 | input centers | each `< 0.02` |
| 1.22 | **Region mesh watertight & oriented** — extract + Taubin | every edge shared by exactly 2 tris; signed volume > 0; volume preserved | edges == 2; vol ratio ∈ (0.7, 1.3) |
| 1.23 | **Symmetry constraint yields mirror density** — plane y=5 | optimized field mirror-symmetric | > 90% cells paired; paired Δ `< 1e-9` |
| 1.24 | **Nodal recovery averages adjacent cells** — values 1,3 across a shared face | shared node = 2; void-only node = NaN | exact |
| 1.25 | **Cut-cell occupancy fractions** — 9.5 mm box on 1 mm grid | face 18/27, edge 12/27, corner 8/27, interior 1; occupancy-weighted volume vs 9.5³ | per-cell `< 1e-6`; volume `< 7%` |
| 1.26 | **`classify_cells` skin/shell counts** — 10³ box, walls & directional shells, composite fractions | analytic layer counts (e.g. 1-layer skin = 10³−8³ = 488); composite face f=0.5, corner 0.875 | exact counts; fractions `< 1e-6` |
| 1.27 | **Active-aware displacement sampling** — single solid cell in void | sampling stays exact at the cell value (plain trilinear would dilute) | `< 1e-9` |

---

## 4. Tier 2 — Meshing / discretization benchmarks

`crates/filasim-core/tests/meshbench.rs` is the harness that **chose the cut-cell
convention** (Finite-Cell occupancy + 0.15 floor) by comparing five boundary
conventions against analytic/textbook truth across eight cases. Most functions
print comparison tables and are `#[ignore]`d (run them when touching meshing);
one encodes the winning convention as a CI assertion.

| # | Case | Reference (closed form / textbook) | What it measures |
|---|---|---|---|
| 2.1 | **Volume convergence** — sphere r=10, rotated box | `4/3·πr³`; box = 4000 (rotation-invariant) | signed volume bias per convention, h ∈ {1, 0.5, 0.25} |
| 2.2 | **Grid-phase robustness** — sphere, 5 origin shifts | sphere volume | mean error & coefficient of variation (phase wobble) |
| 2.3 | **Stiffness (rotated square cantilever)** — a=10, L=100, 30° | bending `FL³/(3EI)`, `I=a⁴/12`; axial `FL/(EA)` | FE/analytic ratio off-axis |
| 2.4 | **Kirsch plate-with-hole** — plate hw=50, t=8, hole a=5, σ∞=1 | hole-edge `Kt = σxx/σ∞ ≈ 3.0`; min-SF ≈ 20 | peak stress concentration accuracy |
| 2.5 | **Solid round cantilever & thin-walled tube** — r=8 / ro=10,ri=8 | `I=πr⁴/4`; tube `I=π(ro⁴−ri⁴)/4`, volume | stiffness + tube volume on curved/thin features |
| 2.6 | **Shoulder-fillet Kt** — stepped bar D=30, d=20, tension, D/d=1.5, r/d∈{0.10,0.15} | Pilkey/Peterson via Betancur 2017: **Kt ≈ 1.68 / 1.55** | peak von-Mises Kt at a fillet (the hardest stress case) |
| 2.7 | **Sliver-floor preservation** — rotated box + Kirsch with/without the 0.15 floor | volume 4000; Kt≈3, SF≈20 | confirms the floor removes false slivers at ~1% volume cost |
| 2.8 | **Convention regression (CI assertion)** — sphere r=8 | `4/3·πr³` | asserts inflate-derate beats center-occ on volume bias |

**Headline finding (documented in `DESIGN.md §3`):** inflate-derate + 0.15 floor
wins on every axis — volume bias ≈ 0 (vs −5 to −13% for center-occ on thin
walls), lowest phase wobble (CoV 0.009% vs ~0.3%), and most-accurate peak stress
on curved features (fillet Kt within ~1–3% where binary conventions over-read
12–28%, because occupancy derating tempers the staircase stress spikes).

---

## 5. Tier 3 — Printed-material (composite) verification

`crates/filasim-core/tests/printed.rs` validates the **skin + homogenized infill**
model (the thing that makes filaSim an FDM tool, not a generic solid solver)
against the **composite/sandwich-beam closed form**. CI.

| # | Case | Reference (closed form) | Tolerance |
|---|---|---|---|
| 3.1 | **Printed beam vs composite sandwich** — 80×8×8, ν=0, skin = 2 cell layers, core ρ=0.25 at `E/E₀=ρ^1.5` | beam stiffness ratio `I_o / ((I_o−I_i) + e_core·I_i)`, `e_core=ρ^1.5` | `< 10%` on `δ_printed/δ_solid` vs analytic |
| 3.2 | **Composite skin tracks a sub-voxel wall** — 80×8×8, wall=0.45 mm < ½ cell, ρ=0.25 | sandwich ratio for the *real* 0.45 mm wall; legacy whole-layer model must be badly off | composite `< 12%`; **legacy err > 20%** (proves composite is needed) |
| 3.3 | **Voxel-size snap picks integer wall fractions** — `pick_voxel_size` over several walls | `h = wall/k`, k=round(wall/h₀); cap fallback | `< 1e-9` |
| 3.4 | **Snapped grid resolves skin as exact layers** — 20×6×6, wall=0.9→h=0.45 | exactly 2 skin cells per free face (column reads `ss…ss`) | exact layer labeling |

This tier is why the as-printed analysis can be trusted for **stiffness/
deflection** within ~10% of the closed form (given the infill calibration); see
the limitation on surface stress in the Theory Manual §12.

---

## 6. Tier 4 — Interoperability / format golden files

`crates/filasim-core/tests/phase3.rs` checks that exports/imports are **byte-correct**
against the real slicer dialects (pinned from the `Cube.3mf` sample). CI.

| # | Case | What it asserts |
|---|---|---|
| 4.1 | **Orca/Bambu 3MF roundtrip** — part + 2 modifier regions (25%, 50%), base 12% | required OPC zip entries; one `normal_part` + two `modifier_part`; `sparse_infill_density` 25/50%, object `wall_loops=3`; binary mode writes `sparse_infill_pattern="concentric"` and never the deprecated `internal_solid_infill_pattern` |
| 4.2 | **PrusaSlicer 3MF flavor** | `Slic3r_PE_model_config`, one object, 3 volumes with correct triangle ranges, `fill_density`/`perimeters`/`fill_pattern` per modifier |
| 4.3 | **Per-bin STL fallback** | universal STL-zip export with named entries |
| 4.4 | **Import real `Cube.3mf` sample** | 25 mm cube, part + modifier meshes within 1 mm of 25 (self-skips if the sample file is absent) |

> **Open item (`DESIGN.md §5/§9`):** the minimal `project_settings.config` Orca
> tolerates, and a version test matrix across Orca 2.x / Bambu Studio / Prusa, are
> still to be pinned down with golden files. Manual open-tests in real Orca have
> passed.

---

## 6b. Tier 7 — Displayed surface-stress accuracy

`crates/filasim-core/tests/surfstress.rs`. Tiers 1–2 measure the raw cell field;
this tier measures **what the app paints on the part** — the nodal-recovered
stress tensor evaluated on the true surface — and reports it as a distribution
binned by how far the surface normal sits from a grid axis. Geometry is an exact
indicator, not a tessellation, so STL faceting cannot contaminate the result.

Its distinguishing metric is the **traction residual** `‖σ·n‖ / σ_ref` on a
traction-free surface: σ·n must vanish there, so all of it is error, and it needs
**no analytic solution** — which makes it the only stress diagnostic here that
can be pointed at a real part.

| # | Case | Reference | What it measures |
|---|---|---|---|
| 7.1 | **Round cantilever** — r=8, L=100, tip load | `σxx = M·z/I`; lateral surface traction-free | the cylinder sweeps every normal orientation in one solve |
| 7.2 | **Plate with a hole**, rotated 0/15/30/45° | Kirsch `Kt = 3.0` on σ₁ | peak accuracy when the concentration is moved off-axis |
| 7.3 | **Stepped shaft fillet**, rotated 0/45° | Pilkey/Peterson Kt (see the caution in §10) | a second stress-raiser topology |
| 7.4 | **Thin-walled tube** — ro=10, ri=8, wall 2–4 cells | `σxx = M·z/I`; both surfaces traction-free | the FDM-relevant, boundary-cell-dominated regime |
| 7.5 | **`bench_cut_cell_cost`** | — | runtime, MGCG iteration count and memory, exact cut cells vs ersatz |
| 7.6 | **`surf_kirsch_h_refinement`** — the hole at h = 1 → 0.125 | Kirsch `σ_θθ(r,θ)` as an *h-independent normalizer* | does the displayed peak converge under refinement? **~107 min, ~13 GB** |
| 7.7 | **`surf_kirsch_probe_standoff`** | — | control: is 7.6's trend an artifact of where the probe samples? |
| 7.8 | **`bin/kirschbench`** — quarter plate, 7+ meshes | Kirsch with the Heywood finite-width correction, `Kt_gross = 3.032` | independent cross-check: the app's four CAD/voxel samplers verbatim, plus the `surface_band` / `MeshQuality` calibration |
| 7.9 | **`surf_kirsch_submodel`** — ±3a box, Dirichlet-driven from a coarse global | the same global solved fine everywhere | does voxel submodeling recover the fine answer, and at what cost? **~11 min** (dominated by the fine global it is checked against) |
| 7.10 | **`surf_kirsch_submodel_box_sweep`** — 5 box sizes × 3 driver meshes | global h=0.25 | how small can the box be, and how coarse the driver, before submodeling breaks? ~4 min |
| 7.11 | **`far_field_cut_perpendicular_to_stress`** — bar, frictionless ends, 11 meshes | `σxx = σ∞` exactly, everywhere | the FAR-FIELD read-back on a cell cut perpendicular to the stress — the case F1 was built for. ~10 s |

Run: `cargo test -p filasim-core --test surfstress -- --ignored --nocapture`
(7.6 is excluded from any routine sweep — run it by name, deliberately: it took
111 min and ~13 GB for the four-mesh sweep across three read-back modes.)

7.8 needs its geometry tessellated through the app's own STEP import path first:

```bash
node web/scripts/tessellate-step.mjs KischPlateWHole_quater.step kirsch-q.bin
cargo run --release -p filasim-core --bin kirschbench -- --mesh kirsch-q.bin \
    --h 1.0,0.7,0.5,0.37,0.25,0.2,0.125
```

The quarter part is required — `classify` locates the `x=0` and `y=0` symmetry
planes geometrically, and handing it the full plate fails with "boundary
condition 0 has no triangles".

Every case sweeps the read-back modes side by side on ONE solve, so a difference
between rows is the read-back and nothing else:

| mode | what it is |
|---|---|
| `mean (today)` | volume-averaged nodal recovery — the long-standing production path |
| `SPR` | superconvergent patch recovery (`spr.rs`) |
| `SPR+proj` | SPR plus the closed-form traction projection at the sample point |
| `cut-decoupled` | F1 — directional occupancy decoupling on cut cells (`stress::cut_normals` + `decouple_traction`) |
| `cut+surf-rec` | F1 + F3 — plus `stress::recover_surface` before nodal recovery. **This is the shipping wasm path** (`cell_field_cut` → `recover_surface` → `recover_nodal`) |

**Headline findings.** Orientation is *not* the driver — error is flat across
normal-angle bins in every case. The displayed field is accurate on smooth
surfaces (0.6–3.8% RMS) and materially less so at a stress concentration.
Neither superconvergent patch recovery (`spr.rs`) nor exactly-integrated
boundary elements (`cutcell.rs`) improve the displayed number enough to justify
shipping; both modules carry their measured verdicts in their own docs so the
results are not re-derived.

**Surface recovery (F3) is the exception, and it is resolution-dependent.** It
biases the reading *downward* by removing the boundary cells from it, so its
sign of benefit follows the sign of the baseline error, and that flips at
roughly **10–15 cells per feature radius**:

- *Below* the threshold the baseline under-reads and F3 doubles the error
  (hole at 5 cells/radius: −16.6% → −31.3%).
- *Above* it the baseline diverges upward on the staircase and F3 converges
  (hole at 40 cells/radius: +4.6% → +2.1%; fillet at 8 cells across the
  fillet: +17.6% → +3.7%).

The same crossover appears independently in `bin/kirschbench` on a different
geometry (quarter plate, symmetry BCs, its own samplers). Normalised to cells
per hole radius the two harnesses agree to within a few points — 5 cells:
raw −12.9% / recovered −29.1% there vs −16.6% / −31.3% here; 10 cells: −0.7% /
−10.2% vs −7.6% / −13.6%. Two independently written samplers, same curve.

F1 alone is a **no-op on the peak** (identical to three decimals at every mesh
in 7.6) but consistently improves the free-surface traction residual MAX
(11.5% → 7.3% at 20 cells/radius). Its value is equilibrium and far field, not
the concentration; the far-field claim it was built for is **not** measured by
this tier.

**The refinement result (7.6) — the RAW peak does not converge; the
surface-recovered one does.** The first half of that sentence was the finding
that closed the surface-stress investigation. The second half was measured
afterwards, on the same case with the same solves, and reopens it.

| h | cells/radius | mean (today) | cut-decoupled (F1) | **cut+surf-rec (F1+F3)** |
|---|---|---|---|---|
| 1.0   | 5  | 2.501 (−16.6%) | 2.501 (−16.6%) | 2.062 (−31.3%) |
| 0.5   | 10 | 2.773 (−7.6%)  | 2.773 (−7.6%)  | 2.592 (−13.6%) |
| 0.25  | 20 | 2.976 (−0.8%)  | 2.976 (−0.8%)  | 2.904 (**−3.2%**) |
| 0.125 | 40 | 3.138 (**+4.6%**) | 3.138 (+4.6%) | 3.063 (**+2.1%**) |

Under the production read-back the distribution splits. The **median** surface
point converges — `σ₁/Kirsch` reaches 1.00 by 20 cells per radius and stays —
while the **upper tail does not**: p90 climbs 1.001 → 1.026 → 1.051 and the max
1.022 → 1.033 → 1.075, both rising monotonically *after* the median has settled.
The traction residual says the same thing with no analytic reference at all,
since its exact answer is zero by construction: RMS falls 3× while its MAX stays
flat at ~11% through an 8× refinement.

The mechanism is the staircase. The number of boundary corners on the rim grows
like `1/h`, so a maximum taken over them grows even as each corner's typical
error shrinks — and a maximum is exactly what the app paints.

**`recover_surface` removes the exposure to that mechanism** by fitting the
clean interior and never reading the corrupted boundary cells. Every
reference-free diagnostic that indicted the production path reverses under it:

| diagnostic (no analytic reference needed) | mean (today) | cut+surf-rec |
|---|---|---|
| Richardson limit, successive `h` triples | 3.588 → **3.758** (runs away) | 3.354 → **3.225** (settles) |
| observed order `p` | 0.42 → **0.33** (degrading) | 0.76 → **0.98** (≈ first order) |
| traction resid MAX, h = 1 → 0.125 | 12.1 → 9.2 → 11.5 → **11.1%** (flat) | 17.1 → 13.5 → 6.8 → **6.5%** (falling) |
| `σ₁/Kirsch` max (upper tail) | 1.119 → 1.022 → 1.033 → **1.075** (rising) | 1.079 → 1.037 → 1.025 → **1.047** (flat) |

Consequences, restated against the new evidence:

- **The −0.8% at 20 cells per radius is still cancellation, not accuracy** —
  peak-flattening pulling down, boundary roughness pushing up. That reading is
  unchanged; it is why the production number looks best at exactly the mesh
  where two errors happen to cross.
- **Refining is counterproductive on the raw read-back and productive on the
  recovered one.** This is the practical inversion: the reflex to add cells was
  wrong before and is right now, *provided* surface recovery is on and the mesh
  is past ~15 cells per feature radius.
- **Submodeling is un-rejected, and it was then measured working (7.9).** The
  rejection rested on "smaller `h` at the hot spot is all submodeling buys, and
  smaller `h` makes it worse." Smaller `h` now makes it monotonically better, so
  the argument no longer holds — and case 7.9 built the thing and measured it: a
  ±3a box re-voxelized to 20 cells per radius, driven by displacements
  interpolated from the **5 cells/radius** global, reads **−3.7% in 21 s total
  against −3.2% in 548 s** for the full global refinement. Driven from the 10
  cells/radius global it reproduces the fine answer exactly (2.904 vs 2.904).
  The free-surface traction residual matches its global counterpart to the
  decimal in every pairing, which is the reference-free confirmation that the
  submodel reproduces the field and not merely a similar peak.

  The failure mode this was expected to hit — the coarse global's *local
  compliance* being wrong, and that error living in the very displacements being
  interpolated — did not appear, and case 7.10 then swept both knobs to find
  where it would:

  | box half-width | from h=0.5 (10 cells/a) | from h=1 (5 cells/a) | from h=2 (2.5 cells/a) |
  |---|---|---|---|
  | ±1.25a | −2.8% | −3.1% | −10.0% |
  | ±1.5a  | −3.0% | −3.4% | −9.5% |
  | ±2a    | −3.1% | −3.7% | −7.9% |
  | ±3a    | −3.2% | −3.7% | −6.4% |
  | ±4a    | −3.3% | −3.7% | −5.8% |

  **Box size barely matters, provided the driver is adequate.** From a 5
  cells/radius global, ±1.25a is as good as ±4a — all five rows sit within half
  a point of the −3.2% target, at 196k cells against the global's 8.31M (42×
  fewer). The `(a/r)²`-decay reasoning that motivated a large box turns out not
  to govern: the submodel *re-solves* the perturbation itself, so it needs only
  the boundary data to be right, and a 5 cells/radius global is already accurate
  a short distance out.

  **Box size matters a great deal once the driver is pathological**, and the
  trend reverses: from a 2.5 cells/radius global — which reads −56.1% on its own
  — a bigger box is monotonically better (−10.0% → −5.8%), because it pushes the
  artificial cut out of the neighbourhood the coarse solve got wrong. Even at
  its worst that is a 46-point improvement on the driver.

  Practical rule: **±1.5a from a driver with ≥5 cells per feature radius, ±3a or
  more below that.** The cell counts above are the padded multigrid grids, so
  ±1.25a and ±1.5a round to the same solve size.

  **The framing that matters is 5 → 20 cells per radius, not 20 → 40.** The
  latter is one point of Kt for 12× the runtime and is not worth chasing. The
  former is 28 points, and it is where real parts sit: a 3 mm fillet meshed at
  `h`=0.5 has six cells across it, and globally refining a whole bracket to fix
  that is usually infeasible. Below the crossover the recovered read-back also
  *under*-reads — the unconservative direction — so the under-resolved regime is
  not merely imprecise, it is imprecise the dangerous way. Submodeling is the
  cheap way out of it.

**Caveat from the control (7.7).** Sweeping the probe standoff independently of
`h` shows the rising max holds at 0.15 and 0.30 cells from the rim, is flat at
0.60, and reverses at 1.00. So "refinement inflates the displayed peak" is a
statement about sampling within ~⅓ cell of the boundary — which is what the app
does — and not about the interior field. What is unconditional is that
`σ₁/Kirsch` p50 converges at every standoff, that the spread across standoffs
narrows with refinement (11.6 → 7.0 points), and that it converges to ≈1.00
rather than ≈1.04 — which is what rules out the alternative reading that the
h=0.125 over-read is really convergence to a higher 3-D truth. That reading was
the only one under which submodeling would have paid.

**Where this line of work stands.** Two of the original candidate fixes remain
closed: superconvergent recovery (`spr.rs` — sharpens the spurious peak along
with the real one) and exact cut-cell elements (`cutcell.rs` — correct, no
user-visible gain, even on thin walls; F1 shrinks the ersatz↔exact gap further
still, from 2.9 points to 0.3 on the fillet, so fixing the read-back makes the
operator matter *less*, not more).

The earlier version of this section concluded that what remained was a
*representational* change — "a conforming surface, or displaying a spatially
filtered peak instead of a raw cell max" — and treated that as too large to
attempt, capping the displayed peak at ±5%. **`stress::recover_surface` is that
change**, and it was built: a degree-1 fit over clean interior cells evaluated
at the boundary cell centre is exactly a spatially filtered peak. It reaches
+2.1% at 40 cells per hole radius and −3.2% at 20, and +3.7% on the fillet at 8
cells across the fillet where the production path reads +17.6%. **The ±5% cap no
longer describes the recovered read-back**, though it still describes the raw
one.

What that does *not* change: the recovered value is a better estimator of the
peak, not a more equilibrium-consistent field. It raises the traction residual
in the under-resolved regime (hole at 10 cells/radius: 5.3% → 9.5% RMS) even
where it improves Kt, and it under-reads — the unconservative direction — below
the crossover. Both facts are why the shipping path reports a **band** rather
than a single number: `stress::surface_band` returns the recovered peak, the
un-recovered `bound`, and a `MeshQuality` bucket, and the calibrated rule is to
take the verdict from `bound` whenever quality is not `unresolved`. That rule
was independently re-checked against `bin/kirschbench` on the quarter plate: on
every mesh the band calls trustworthy, the worst unconservative bound error is
−0.7%, and `unresolved` fires on precisely the three meshes where the recovered
peak under-reads by 25–47%.

**F1's own claim, finally measured (7.11) — and it is the more important of the
two fixes.** Every other case in this tier reads a stress *concentration*, where
7.6 shows F1 changing the peak by nothing at all. It was built for something
else: a cell cut PERPENDICULAR to the stress is a soft link in series, its ersatz
stress is already the material stress, and the scalar `material_factor` divides
by the occupancy a second time. Case 7.11 probes a bar in pure tension with
frictionless ends, where the exact answer is `σxx = σ∞` *everywhere* — boundary
column included, so the reference is a constant and any deviation is read-back
error:

| column occupancy | production | with F1 | `1/occ − 1` |
|---|---|---|---|
| 1.00 (5 of 11 meshes) | −1.7 … 0.0% | identical | 0% |
| 0.83 | +19.1, +21.2% | −0.7, +1.0% | +20% |
| 0.67 | +51.9, +52.9, +59.4% | +1.3, +2.0, +6.2% | +49% |
| 0.50 | **+108.3%** | +4.2% | +100% |

The error tracks `1/occ` to within a few points at every occupancy — the double
division, confirmed as a formula and not merely as a magnitude. Worst case is a
**doubling** of the reported stress, at 1 of 11 mesh sizes; 6 of 11 are affected
at all, which is the "5 times in 6" the production voxelizer's grid centring
produces. Mid-bar columns are bit-identical between modes at every `h`,
confirming F1 is a no-op away from cut cells.

This makes F1 the fix that matters most for ordinary parts. A doubled stress in a
**uniform far field** on a plain bar is a wrong number in the safe-looking part
of a model, with no notch present to warn anyone it is there.

---

## 7. Tier 5 — Cross-code golden comparison (planned)

The release bar calls for **golden comparisons vs an established FEA code**
(CalculiX or Fusion 360 Simulation) on ~5 representative parts. This is **not yet
automated** (Phase 4 leaves it open). Recommended set, chosen to span the
behaviors the engine claims:

| # | Part | Why it's representative | Reference | Accept (target) |
|---|---|---|---|---|
| 5.1 | **L-bracket** (mounting flange + load arm) | bending + a re-entrant corner (singularity handling) | CalculiX C3D8 on a comparable mesh | max deflection within ~5–10%; stress *away from* the corner within ~10% |
| 5.2 | **Pillow/bearing block** with a bolt-hole | stress concentration on a real part | CalculiX / Fusion | Kt-region peak within ~10–15% |
| 5.3 | **Thin-walled enclosure** | thin features, contrast, solver iteration count | CalculiX shell or fine solid | global stiffness within ~10% |
| 5.4 | **Hook / cantilever fixture** (the in-repo smoke beam) | the primary optimization fixture; checks as-printed vs solid | CalculiX with manual skin/infill ersatz | deflection within ~10% |
| 5.5 | **Lattice/infill coupon** | validates the homogenized E(ρ) against a *resolved* infill model | CalculiX on a meshed gyroid unit cell, or measured data | effective stiffness within calibration error |

**How to run it (when set up):** export each fixture as STL, solve in filaSim
(Fine preset) and in the reference code with matched material/BC/load, and record
max displacement, compliance, and peak nominal stress. Track the ratios over time
as a golden file. Discrepancies at singular features are expected and should be
read at nominal locations.

---

## 8. Tier 6 — Physical testing (planned)

Post-launch content, **not a release gate** (`DESIGN.md §13`). The natural
program: print the Tier-5 fixtures (and graded-infill variants), measure
stiffness on a universal tester, and compare to the as-printed and optimized
predictions. This is also how the E(ρ) calibration curves (gyroid/cubic/grid) get
their measured `c, n` per material — the offline RVE homogenization tool
(`DESIGN.md §9`) cross-validates where measurements are missing.

---

## 9. The Standard Validation Battery

A curated subset to run **regularly** (before releases, after solver/meshing
changes, and as a periodic confidence check). It is deliberately small,
fast-running, physically meaningful, and spans every subsystem — so a green
battery is a strong statement that "the tool works", suitable to show users.

**Automated core (`cargo test -p filasim-core`, < ~1 min):**

| Battery item | Backing test(s) | Proves | Pass criterion |
|---|---|---|---|
| **B1 Patch test** | 1.4 | exact constant-stress recovery | DOF error `< 1e-8` |
| **B2 Cantilever + convergence** | 1.7 | bending accuracy & mesh convergence | ratio ∈ [0.95, 1.02] refined; monotone |
| **B3 Axial bar / rollers** | 1.5, 1.6 | penalty supports & consistent loads | `< 3%` |
| **B4 Column stress** | 1.10 | stress recovery vs σ=F/A | `< 6%` |
| **B5 Self-weight beam** | 1.9 | body-force (gravity) loads | ratio ∈ [0.85, 1.10] |
| **B6 Elastic foundation** | 1.11 | Winkler support | `< 8%` |
| **B7 Voxel volume + dirty mesh** | 1.13, 1.14, 1.15 | discretization & robustness | `< 4%` volume; robustness ranges |
| **B8 Under-constraint detection** | 1.12, 1.8 | the RBM safety net | free modes detected |
| **B9 Composite printed beam** | 3.1, 3.2 | skin + homogenized infill | `< 10–12%` vs sandwich |
| **B10 Optimizer beats uniform** | 1.19, 1.20 | the value proposition holds | gain `> 1.03` (graded), `> 1.10` (binary) |
| **B11 Export/import golden** | 4.1, 4.2 | slicer files stay correct | exact structure |

**Periodic / on-change (manual):**

| Battery item | Command | Proves |
|---|---|---|
| **B12 Mesh-convention benchmark** | `cargo test -p filasim-core --test meshbench -- --ignored --nocapture` | cut-cell convention still wins on volume/phase/Kirsch/fillet |
| **B12b Displayed surface stress** | `cargo test -p filasim-core --test surfstress -- --ignored --nocapture --skip surf_kirsch_h_refinement` | the field the USER reads stays inside its (looser) envelope; free-surface traction residual does not regress |
| **B13 Performance budget** | `cargo run -p filasim-core --release --bin bench` | 1 M cells solved in seconds; cantilever ratio in band |
| **B14 Cross-code goldens (Tier 5)** | manual, per §7 | whole-part agreement with CalculiX/Fusion |

**Recommended cadence:** B1–B11 on every commit (CI); B12–B13 before each release
and after any change to `fem.rs`/`mg.rs`/`voxel.rs`/`simp.rs`; B14 per release
milestone and whenever the material/infill model changes.

---

## 10. Measured accuracy envelope

A one-glance summary of what the engine actually achieves on the verification
cases — useful for setting user expectations.

| Quantity | Demonstrated accuracy | Source |
|---|---|---|
| Constant-stress field (patch test) | machine-exact (`< 1e-8`) | 1.4 |
| Beam bending deflection (≥ 8 cells thick) | within ~1.6% → 0.7% (8 → 32 cells), converging | 1.7, `PHASE1_RESULTS.md` |
| Axial / roller-supported elongation | `< 3%` | 1.5, 1.6 |
| Uniaxial stress (St-Venant zone) | `< 6%` | 1.10 |
| Elastic-foundation settlement | `< 8%` | 1.11 |
| Voxelized volume (curved bodies) | `< 4%` (sphere); bias ≈ 0% with cut-cell occupancy | 1.13, 2.1 |
| Stress-concentration Kt — **raw cell stress** (hole, fillet) | within ~1–3% on curved features (cut-cell convention) | 2.4, 2.6 |
| Stress-concentration Kt — **raw (un-recovered) displayed field** (hole) | −16.6% at 5 cells per hole radius, −7.6% at 10, −0.8% at 20, **+4.6% at 40**. **Does not converge** — refining past ~20 cells/radius makes it worse. This is the `bound` half of the reported band | 7.2, 7.6 |
| Stress-concentration Kt — **surface-recovered field, what the app displays** (hole) | −31.3% at 5 cells/radius, −13.6% at 10, −3.2% at 20, **+2.1% at 40**. **Converges**, observed order ≈1. Under-reads below ~10–15 cells/radius — the unconservative side, which `MeshQuality` flags | 7.2, 7.6 |
| Stress-concentration Kt — surface-recovered, fillet | +3.7% at 8 cells across the fillet, where the raw path reads +17.6% | 7.3 |
| Stress-concentration — *typical* (median) surface point, vs Kirsch | converges to within 1% by 20 cells per hole radius | 7.6 |
| Surface stress on a smooth free surface (round cantilever, thin tube) | 0.6–1.3% (thin wall) to 1.7–3.8% (solid), RMS | 7.1, 7.4 |
| Uniform far-field stress in a **cut boundary column** (cut ⊥ to the stress) | with the directional decoupling: −1.7 … +6.2%. **Without it: up to +108%**, tracking `1/occupancy`, on 6 of 11 mesh sizes | 7.11 |
| Free-surface traction residual ‖σ·n‖/σ_ref (should be 0) | 0.5–2% on smooth surfaces, 5–16% at a stress raiser. Raw read-back: **RMS converges (~O(√h)), MAX does not** — flat at ~11% over an 8× refinement. Surface-recovered: MAX falls 17.1% → 6.5% over the same refinement | 7.1–7.4, 7.6 |
| Printed composite-beam stiffness | within ~10% of the sandwich closed form | 3.1, 3.2 |
| MGCG iteration count | mesh-independent (8–9 iters, 130k→1M solid cells) | `PHASE1_RESULTS.md` |
| Graded-infill stiffness gain vs uniform (equal mass) | +15% (cantilever), up to +40% (binary smoke beam) | 1.19, 1.20, `DESIGN.md` |

> These envelopes apply to **global stiffness/deflection** and **nominal stress**.
> Surface stress at staircased curves, re-entrant corners, and point loads is
> advisory and does not carry these tolerances — see Theory Manual §12.

> **Read the two Kt rows carefully — they are not the same number.** Tier 2
> measures the **raw cell-centre** stress at the single worst cell. The app
> displays a **nodal-recovered field sampled on the true surface**, which is a
> different quantity and is materially less accurate at a stress concentration,
> because volume-averaging a curved field flattens its peak. Quoting the ~1–3%
> figure for what a user reads on screen overstates the accuracy by roughly 3×.
> The displayed value **under**-reads a concentration, which is unconservative:
> low stress means a high reported safety factor.
>
> Two further cautions from the Tier-7 work:
>
> - **Kt is defined on the peak principal stress σ₁, not von Mises.** They agree
>   only under plane stress; at the mid-thickness of a 3D section the state tends
>   toward plane strain and von Mises reads lower. Reading the wrong one on the
>   Kirsch plate costs ~8 percentage points. Both matter — σ₁ against a textbook
>   Kt, von Mises for the material safety factor — but they are not
>   interchangeable.
> - **The shoulder-fillet case (2.6) does not converge to its 2-D textbook Kt**
>   for the section thickness used here: the error grows from +4.9% to +14.6% as
>   `h` halves, with σ₁ and with an exactly-integrated boundary element alike.
>   The 3-D-constraint explanation previously offered here is **no longer the
>   leading one**: case 7.6 refined the *hole* to 40 cells per radius and found
>   the same signature — median converging, peak diverging — on a topology where
>   3-D constraint is measurably small (median settles at 1.00× Kirsch, not
>   1.04×). Staircase roughness in a max statistic explains both cases with one
>   mechanism. **Do not use case 2.6 as an accuracy gate until the Tier-5
>   CalculiX cross-check gives it a 3-D reference.** It can measure *changes*,
>   not *correctness*.
>
>   **The staircase reading is now confirmed rather than inferred.** Under
>   surface recovery the same fillet converges: −8.5% at `h`=0.5 and +6.2% at
>   `h`=0.25, against +4.9% → +14.6% raw, and the same reversal holds across
>   both radii, both rotations and both stiffness operators. A read-back change
>   cannot remove a genuine 3-D-constraint effect, so the fact that it removes
>   this divergence rules the 3-D explanation out and leaves the staircase.
>
> - **Refining now helps — but only with surface recovery on.** This caution
>   previously read "do not refine a mesh to make a displayed peak more
>   accurate," and that is still correct for the raw read-back: more cells per
>   feature radius improve the field everywhere except the max statistic, which
>   boundary roughness drives upward. With `recover_surface` the max converges
>   too (observed order ≈1), so refining is productive above roughly **15 cells
>   per feature radius**. Below that threshold recovery under-reads and the raw
>   value is closer — which is the regime `MeshQuality` reports as `unresolved`.
>   Read the band, not one number.

---

*For the engineering theory behind these cases, see the
[Theory Manual](theory-manual.md). For product rationale, see
[`DESIGN.md`](../DESIGN.md).*
