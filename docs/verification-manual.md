<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com> -->

# InFEAll — Verification & Validation Manual

*Engine version: `sig-core` (master). Document revised 2026-06-14.*

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
7. [Tier 5 — Cross-code golden comparison (planned)](#7-tier-5--cross-code-golden-comparison-planned)
8. [Tier 6 — Physical testing (planned)](#8-tier-6--physical-testing-planned)
9. [The Standard Validation Battery](#9-the-standard-validation-battery)
10. [Measured accuracy envelope](#10-measured-accuracy-envelope)

---

## 1. V&V philosophy and tiers

The validation bar (`DESIGN.md §13`): **solver unit tests vs analytic solutions
in CI**, **golden comparisons vs established FEA on ~5 representative parts**, and
physical testing as post-launch content (not a release gate). The suite is
organized into six tiers by what they prove:

| Tier | Proves | Runs | Status |
|---|---|---|---|
| 1 | The FE math is correct (closed-form agreement) | CI, every commit | ✅ implemented |
| 2 | The voxel/cut-cell discretization is accurate & robust | manual benchmark | ✅ implemented (printing harness) |
| 3 | The printed-material (skin + homogenized infill) model is correct | CI | ✅ implemented |
| 4 | Exports/imports are byte-correct for slicers | CI | ✅ implemented |
| 5 | Whole-part results match an established FEA code | manual, periodic | ⏳ planned |
| 6 | The model matches physical reality | offline, post-launch | ⏳ planned |

All Tier-1/3/4 tests are first-party (no GPL test deps) and assert against
formulas or exact structure, so a regression fails the build.

---

## 2. How to run the suite

```bash
# Tier 1, 3, 4 — the CI suite (fast, assertion-backed). Run on every change.
cargo test -p sig-core

# Tier 2 — the meshing-convention benchmark harness (prints tables, mostly
# #[ignore]d so it does not run in normal CI). One command re-validates the
# cut-cell convention against analytic/textbook truth.
cargo test -p sig-core --test meshbench -- --ignored --nocapture

# Phase-1 exit-criterion benchmark (throughput + analytic cantilever check).
cargo run -p sig-core --release --bin bench            # add --small to skip the 1M-cell run
# Optional: drop a 3dbenchy.stl in the working dir for the thin-shell case.

# STEP tessellation regularity harness (needs the `step` feature).
cargo run -p sig-core --features step --bin stepbench -- "model.step" "model.stl"
```

Tests that need an external fixture (`Cube.3mf`, `3dbenchy.stl`) **self-skip**
when the file is absent, so the core suite is hermetic.

---

## 3. Tier 1 — Analytic verification (CI)

These run on every commit (`crates/sig-core/tests/validation.rs`,
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

`crates/sig-core/tests/meshbench.rs` is the harness that **chose the cut-cell
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

`crates/sig-core/tests/printed.rs` validates the **skin + homogenized infill**
model (the thing that makes InFEAll an FDM tool, not a generic solid solver)
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

`crates/sig-core/tests/phase3.rs` checks that exports/imports are **byte-correct**
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

**How to run it (when set up):** export each fixture as STL, solve in InFEAll
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

**Automated core (`cargo test -p sig-core`, < ~1 min):**

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
| **B12 Mesh-convention benchmark** | `cargo test -p sig-core --test meshbench -- --ignored --nocapture` | cut-cell convention still wins on volume/phase/Kirsch/fillet |
| **B13 Performance budget** | `cargo run -p sig-core --release --bin bench` | 1 M cells solved in seconds; cantilever ratio in band |
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
| Stress-concentration Kt (hole, fillet) | within ~1–3% on curved features (cut-cell convention) | 2.4, 2.6 |
| Printed composite-beam stiffness | within ~10% of the sandwich closed form | 3.1, 3.2 |
| MGCG iteration count | mesh-independent (8–9 iters, 130k→1M solid cells) | `PHASE1_RESULTS.md` |
| Graded-infill stiffness gain vs uniform (equal mass) | +15% (cantilever), up to +40% (binary smoke beam) | 1.19, 1.20, `DESIGN.md` |

> These envelopes apply to **global stiffness/deflection** and **nominal stress**.
> Surface stress at staircased curves, re-entrant corners, and point loads is
> advisory and does not carry these tolerances — see Theory Manual §12.

---

*For the engineering theory behind these cases, see the
[Theory Manual](theory-manual.md). For product rationale, see
[`DESIGN.md`](../DESIGN.md).*
