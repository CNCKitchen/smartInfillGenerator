<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com> -->

# InFEAll — Theory & Engineering Reference

*Engine version: `sig-core` (master). Document revised 2026-06-14.*

This document describes the structural-analysis engine inside InFEAll the way a
commercial code's theory reference does: the governing equations, material
models, element formulation, meshing, solver, boundary conditions, the analysis
types, and — most importantly for an engineer deciding whether to trust a
result — the **assumptions and limitations**.

It is a companion to:
- [**Verification Manual**](verification-manual.md) — the analytic and benchmark
  cases the engine is tested against, the recommended regression battery, and
  the measured accuracy envelope.
- [`DESIGN.md`](../DESIGN.md) — the product design record (the *why* behind the
  decisions summarized here).

> **Status disclaimer.** InFEAll is a fast, voxel-based linear-elastic FEA aimed
> at *FDM design decisions* (where to put dense infill, how stiff a part is, will
> it delaminate). It is **not** a certified analysis tool. Safety factors are
> **advisory**. Read [§12 Assumptions & Limitations](#12-assumptions--limitations)
> before using a number in a load-bearing decision.

---

## Table of contents

1. [Scope and intended use](#1-scope-and-intended-use)
2. [Units, notation, and conventions](#2-units-notation-and-conventions)
3. [Governing equations](#3-governing-equations)
4. [Material models](#4-material-models)
5. [Element library](#5-element-library)
6. [Discretization and meshing](#6-discretization-and-meshing)
7. [Boundary conditions and loads](#7-boundary-conditions-and-loads)
8. [Solution procedure](#8-solution-procedure)
9. [Pre-solve diagnostics](#9-pre-solve-diagnostics)
10. [Analysis types](#10-analysis-types)
11. [Results and post-processing](#11-results-and-post-processing)
12. [Assumptions & limitations](#12-assumptions--limitations)
13. [References](#13-references)

---

## 1. Scope and intended use

InFEAll solves **3-D linear elastostatics** on a regular voxel discretization of
a printable part, with a material model that homogenizes FDM sparse infill and
the solid perimeter/shell structure into per-cell effective stiffnesses. Its two
jobs are:

1. **Analysis** — given loads and supports, compute the displacement field, the
   stress/strain fields, and FDM-specific safety factors of a part either as a
   solid or *as it will be printed* (walls + uniform infill).
2. **Optimization** — grade the interior infill density so the part hits a
   target stiffness with the least plastic, and emit slicer-ready regions.

Everything runs **in the browser** (Rust compiled to WebAssembly). There is no
server compute path.

**In scope:** static, small-displacement, linear-elastic response of single
solid bodies; isotropic solid material; homogenized infill; strength screening
for both the bulk material and the inter-layer (delamination) direction.

**Out of scope (v1):** nonlinearity of any kind (large displacement, contact,
plasticity, hyperelasticity), buckling/stability, dynamics and modal analysis,
thermal/transient loads, fatigue, fracture, assemblies / multi-body / bonded
contact. See [§10.5](#105-not-implemented) and [§12](#12-assumptions--limitations).

---

## 2. Units, notation, and conventions

The engine works in a single consistent unit system; the UI converts to
friendlier units for display.

| Quantity | Internal unit | Notes |
|---|---|---|
| Length | mm | STL is unitless → assumed mm (import dialog can override inch/cm) |
| Force | N | |
| Stress / modulus | MPa (= N/mm²) | falls out automatically |
| Density | tonne/mm³ | e.g. PLA = `1.24e-9`; entered as g/cm³ in the UI |
| Mass | tonne | displayed in **g** |
| Displacement | mm | displayed in mm or µm |
| Acceleration | mm/s² | gravity = `9810 mm/s²` |

**Coordinate system.** Global Cartesian X/Y/Z. **Z is the print/build
direction** (layers stack along +Z). This is physically meaningful, not
cosmetic: the inter-layer (delamination) safety factor reads σ_zz, so the part's
print orientation is an analysis input. "Place on face" and 90° rotate operate
on the actual mesh before meshing.

**Strain/stress ordering (Voigt).** Engineering order `[xx, yy, zz, xy, yz, zx]`,
with **engineering shear strains** γ_ij = 2ε_ij in the strain vector.

**Sign conventions.** Tension positive. Displacements follow the global axes.
A surface force is the **total** force (N) over the selected patch (not a
traction); pressure is in MPa and acts along the inward surface normal.

---

## 3. Governing equations

The engine solves the equilibrium of a linear-elastic continuum under static
loads. In strong form, on the part domain Ω:

```
∇ · σ + b = 0           in Ω        (equilibrium)
σ = C : ε               in Ω        (constitutive, §4)
ε = ½(∇u + ∇uᵀ)         in Ω        (small-strain kinematics)
u = ū                   on Γ_u      (Dirichlet / supports)
σ · n = t̄              on Γ_t      (Neumann / tractions, loads)
```

where `u` is displacement, `σ` stress, `ε` (infinitesimal) strain, `b` body
force (gravity), `C` the elasticity tensor. **Geometric and material linearity
are assumed**: `C` is constant, kinematics are linearized, and the equations are
solved once (no load incrementation).

The standard Galerkin finite-element discretization with the trilinear hex shape
functions ([§5](#5-element-library)) reduces this to the linear system

```
K u = f
```

with `K` the global stiffness matrix (symmetric positive-definite once rigid-body
modes are removed by supports), `u` the nodal displacement vector, and `f` the
consistent nodal load vector. `K` is never assembled explicitly — see
[§8](#8-solution-procedure).

---

## 4. Material models

### 4.1 Base solid material — isotropic linear elasticity

Each solid (perimeter/shell) cell uses St-Venant–Kirchhoff isotropic elasticity
parameterized by Young's modulus `E` and Poisson's ratio `ν`:

```
σ = λ·tr(ε)·I + 2μ·ε
λ = E·ν / ((1+ν)(1−2ν))          (Lamé first parameter)
μ = E / (2(1+ν))                  (shear modulus)
```

The 6×6 Voigt elasticity matrix `C` (engineering shear) has `C[i][i] = λ+2μ` and
off-diagonal `λ` on the normal block, and `μ` on the three shear diagonals. This
is implemented exactly once in `ke_hex` (`crates/sig-core/src/fem.rs`) and reused
for stress recovery (`crates/sig-core/src/stress.rs`).

### 4.2 Material library

Built-in presets (all user-editable in ⚙ Settings). Strengths are conservative
printed-part datasheet values, not virgin-resin values.

| Material | E₀ (MPa) | ν | Density (g/cm³) | σₜ in-plane (MPa) | σₜᶻ layer adhesion (MPa) |
|---|---|---|---|---|---|
| **PLA** (default) | 3500 | 0.35 | 1.24 | 50 | 35 |
| **PETG** | 2100 | 0.37 | 1.27 | 45 | 34 |
| **ABS** | 2250 | 0.37 | 1.05 | 38 | 25 |
| **ASA** | 2400 | 0.37 | 1.07 | 43 | 29 |

- `E₀` and `ν` define the solid stiffness (§4.1).
- `σₜ` is the in-plane tensile allowable (drives the "material" SF, §4.7).
- `σₜᶻ` is the **inter-layer** tensile allowable (drives the "layer adhesion" SF).
- Density drives mass reporting and self-weight (gravity) loads.

> The library values are starting points. The engine's accuracy is ultimately
> the accuracy of these inputs and the infill calibration (§4.3). Calibrate to
> your own filament/printer for quantitative work.

### 4.3 Infill homogenization — Gibson–Ashby law

Sparse infill is **not** resolved geometrically. Each interior cell carries a
relative density `ρ ∈ [floor, cap]` and is homogenized to an *effective isotropic*
stiffness through a Gibson–Ashby power law:

```
E_eff(ρ) / E₀ = c · ρⁿ          (capped at 1.0 = solid)
```

`c` (coefficient) and `n` (exponent) are **per infill pattern**, exposed in the
advanced panel. Defaults:

| Pattern | c | n | Notes |
|---|---|---|---|
| **gyroid** (default) | 1.0 | 1.5 | near stretch-dominated; the recommended pattern |
| **cubic** | 1.0 | 1.8 | |
| **grid** | 1.0 | 2.0 | bending-dominated; **in-plane anisotropy not modeled** (documented limitation) |
| *(other patterns)* | 1.0 | 2.0 | conservative generic fallback + warning |

The law is convex for `n > 1` (stiffness-per-gram rises with density). This is
the physical basis for graded infill and for how the optimizer places density
bins (§10.3, [`bins.rs`](../crates/sig-core/src/bins.rs)).

> **Calibration validity window.** `c, n` are only valid near the layer height /
> line width at which they were measured. Using them far outside that window
> (e.g. a 0.1 mm vs 0.3 mm layer) degrades accuracy. State your calibration
> window when editing the curve. (Open item in `DESIGN.md §9`.)

### 4.4 Skin / wall model and composite blend

A printed part is a solid shell around sparse infill. `classify_cells`
(`crates/sig-core/src/simp.rs`) reproduces the way a slicer builds that shell,
directionally:

- **Walls** (perimeters × line width = `wall_mm`): an **in-plane** band measured
  per Z-slice by 2-D BFS from each layer's outline. Vertical and sloped surfaces
  get walls; the band does not leak through horizontal faces.
- **Top/bottom shells** (`top_mm`/`bottom_mm` = layers × layer height): a
  **vertical** band measured per column from up/down-facing surfaces (and around
  internal cavities), exactly like sliced solid top/bottom layers. `0` layers =
  open-top showpiece (infill runs to the surface).

Cells fully inside the band are **skin** (relative stiffness 1.0, the solid
material). Cells outside it are **design/interior** cells carrying ρ.

**Composite skin (toggle, default on for printed analysis).** A cell the band
only *partially* covers stays an interior cell with a blended stiffness rather
than rounding the band to whole cell layers. With covered fraction `f`:

```
E_cell / E₀ = f + (1 − f) · E_eff(ρ)/E₀      (Voigt volume blend)
```

Overlapping bands combine as `f = 1 − (1 − f_wall)(1 − f_shell)` (orthogonal
slabs independent; opposite slabs add and clamp). This is the same
homogenization move as the infill law, applied at the surface. It lets walls
**thinner than one voxel** stay representable, so coarse grids no longer have to
resolve the wall. Validated against the composite-beam closed form (see the
Verification Manual). Off (legacy) rounds the wall to `round(wall/h)` ≥ 1 layer.

> **Trade-off.** With composite cells, surface stress/SF readouts smear over the
> cell; **deflection/stiffness is the trustworthy output** there.

### 4.5 Cut-cell occupancy (Finite-Cell / ersatz material)

Boundary cells are partially inside the part. Each boundary cell stores a 3×3×3
**supersampled occupancy** fraction (`grid.scale ∈ [0,1]` = the share inside the
STL). Occupancy:

- decides the **solid set**: a cell joins the solid when occupancy ≥
  `BOUNDARY_FLOOR = 0.15` — this *includes* cells whose center is outside but the
  surface still cuts (so the part never protrudes past its mesh) and *drops*
  sub-floor slivers (small-cut-cell conditioning guard);
- **scales stiffness, mass, and infill weights** linearly: a half-occupied cell
  contributes half the stiffness and half the mass.

This convention was selected by an 8-case analytic benchmark over five
alternatives (`tests/meshbench.rs`) — it minimizes volume bias and stress
over-read on curved features. See the Verification Manual, Tier 2.

### 4.6 Void floor and SIMP penalization

- A small relative-stiffness floor `EMIN_REL = 1e-6` is applied to non-void cells
  (a SIMP soft floor keeps `K` well-conditioned; void cells with `eps = 0` are
  skipped entirely).
- For **binary** optimization (§10.3) the optimizer runs the SIMP density law
  with a penalization exponent `p = 3` so the continuous field converges to
  black/white before quantization. The *verification* solve still uses the
  calibrated physical law (exact at both 0%/100% endpoints). Graded optimization
  uses **no** artificial penalization — intermediate infill is physically
  printable.

### 4.7 Strength model and safety factors

Stress is screened against two **anisotropic** allowables, reflecting the two
ways an FDM part fails. Allowables of graded infill scale by the **same**
Gibson–Ashby relative factor as the stiffness (strength tracks stiffness to first
order; the skin carries full strength). Implemented in
`crates/sig-wasm/src/lib.rs`.

| SF field | Definition | Failure mode |
|---|---|---|
| **Material** (`sfm`) | `σₜ · rel(ρ) / σ_vM` | bulk yield/fracture against von-Mises stress |
| **Layer adhesion** (`sfz`) | `σₜᶻ · rel(ρ) / σ_zz` for **σ_zz > 0 only** | delamination — tension across the print layers; compression cannot delaminate → SF = 99 |
| **Worst case** (`sf`, default) | per-cell `min(sfm, sfz)` | the governing limit; the dock states which one governs |

where `rel(ρ)` is the cell's relative stiffness factor (`= eps`, i.e. occupancy ×
infill/skin blend). All SFs are **capped at 99**.

> **Advisory only.** Strength anisotropy is modeled this way, but stiffness
> anisotropy and **shear-mode** delamination (Mode-II, in-plane shear across
> layers) are **not** modeled. None of these is a certified safety factor. Treat
> them as *screening* readouts — they tell you where to look, not whether to fly.

### 4.8 Stiffness anisotropy (status)

The solid material is modeled as **isotropic**. Real FDM material is
transversely isotropic (E_z typically 10–25% below E_xy). A transversely
isotropic material model (E_xy, E_z, G_xz, two ν's; print direction = global Z)
is a **planned v1.x** item — it still yields one reference element matrix scaled
per cell, so the matrix-free fast path survives. Toolpath-direction in-plane
orthotropy is permanently out of scope (the tool is pre-slice). See `DESIGN.md §9`.

---

## 5. Element library

### 5.1 HEX8 — 8-node trilinear hexahedron

The single element type is the **8-node isoparametric hexahedron** ("brick",
equivalent to ANSYS `SOLID185` / Abaqus `C3D8`), with **3 translational DOF per
node** (u_x, u_y, u_z) → **24 DOF per element**. Because the mesh is a regular
voxel grid, every element is a cube of edge `h`; there is no element distortion.

Node ordering (local ξη ζ signs) and the strain-displacement `B` matrix
(engineering order) are defined in `crates/sig-core/src/fem.rs`. The trilinear
shape functions are `N_l = ⅛(1+ξξ_l)(1+ηη_l)(1+ζζ_l)`.

### 5.2 Integration scheme

**Full integration**: 2×2×2 Gauss quadrature (8 points, `ξ = ±1/√3`). The
element stiffness is

```
KE = ∫_V Bᵀ C B dV  ≈  Σ_gp  Bᵀ(gp) C B(gp) · |J| w_gp
```

with the Jacobian `J = (h/2)·I` on a cube cell, so `|J|·w = (h/2)³`. There is **no
reduced integration and no hourglass control** — full integration on the hex has
no zero-energy (hourglass) modes, so none is needed.

A single 24×24 reference `KE` is computed per grid level (it depends only on `E₀`,
`ν`, `h`) and **scaled per cell** by the cell's relative stiffness factor `eps`
(occupancy × infill/skin blend, §4) — the standard matrix-free
topology-optimization formulation (the "88-line"/PolyTop lineage). `KE` scales
linearly with `h` on cube cells, which the multigrid coarsening exploits.

### 5.3 Numerical characteristics an analyst should know

- **Shear locking.** A fully-integrated trilinear hex is **over-stiff in bending**
  on coarse meshes (parasitic shear). The engine does not add incompatible/
  enhanced-assumed-strain modes; instead it relies on **mesh refinement**. In the
  cantilever verification the FE/Timoshenko ratio is 0.984 → 0.991 → 0.993 at
  8/16/32 elements through the thickness — it converges *from below* (slightly
  flexible at the coarsest, the remaining ~1% is physical root stiffening). Rule
  of thumb: **≥ 4–6 cells across a bending member**, and the engine warns when a
  feature spans < 3 cells.
- **Staircase boundary.** Curved/inclined surfaces are approximated by the voxel
  staircase; this is what the cut-cell occupancy (§4.5) and nodal stress recovery
  (§11) temper. Surface stress at a sub-cell feature is smeared.
- **Stress singularities.** Re-entrant corners, point supports, and point loads
  produce stresses that **do not converge** under refinement (true of all
  displacement FEM). Such peaks are advisory; read nominal stress away from them.
- **No mid-side nodes.** First-order elements only; expect first-order spatial
  accuracy of stress.

---

## 6. Discretization and meshing

### 6.1 Voxel grid

The domain is a regular axis-aligned grid of cubic cells of edge `h` (mm).
Storage is one `f32` relative-density per cell (`0` = void). No tetrahedral
meshing, no mesh-quality metrics, no manual meshing step. This is what makes the
matrix-free multigrid solver fast and robust to dirty input.

### 6.2 Winding-number voxelization

Cell centers are classified inside/outside by the **generalized winding number**
(Barill-style dipole BVH, `crates/sig-core/src/bvh.rs`): `|w| ≥ 0.5` ⇒ inside.
This is robust to "triangle soup" — holes, self-intersections, non-manifold
edges, and even fully inverted (inside-out) normals all voxelize correctly. No
mesh repair is required from the user.

### 6.3 Cut-cell occupancy pass

After the center test, boundary cells (where a cell and its 6 neighbors disagree)
are **3×3×3 supersampled** (27 winding-number samples) to get the occupancy
fraction; the solid set and per-cell stiffness then follow §4.5. Fully interior /
exterior cells skip the supersample. The result is stored in `grid.scale`.

### 6.4 Resolution and voxel sizing

| Preset | Target active cells | Typical use |
|---|---|---|
| **Preview** | ~100 k | fast setup iteration |
| **Normal** (default) | ~300 k | day-to-day analysis |
| **Fine** | ~1 M | final verification |
| **Custom** | user sets `h` (mm) | UI shows implied cell count and warnings |

`pick_voxel_size` (`crates/sig-core/src/voxel.rs`) derives `h` from the bbox
volume and target count: `h₀ = (V / target)^{1/3}`. It can **snap** `h` to an
integer fraction of the wall thickness (`h = wall/k`) so the solid skin is an
exact number of cell layers (an accuracy nicety once composite skin is on). A
**hard cap of 4 M cells** bounds memory/time; snapping is abandoned rather than
exceeding the cap. The UI warns when a chosen `h` is coarser than the wall, past
the cap, or absurdly coarse, and when any thin feature spans **< 3 cells**.

### 6.5 Active nodes and multigrid padding

- A node is **active** if it touches at least one solid cell. Inactive DOFs are
  masked out of the solve (kept at zero), so empty bounding-box space costs
  nothing in the solution.
- Grid dimensions are **padded** so the multigrid hierarchy halves evenly
  (`pad_for_levels`); padding is void and free.

### 6.6 Mesh import and refinement

STL (binary/ASCII, robust to the "solid"-prefixed binary quirk and dropping
degenerate/NaN triangles), 3MF, and STEP (tessellated via the `truck` CAD kernel,
keeping BREP face topology for one-click face selection) are accepted. The
display/analysis triangle mesh is refined so edges are ≤ ~1/60 of the bbox
diagonal (deflection curves stay smooth on coarse STLs), capped at a 160k-triangle
budget. Surface segmentation for click-selection uses dihedral region-growing
(10° default) or exact CAD faces for STEP.

---

## 7. Boundary conditions and loads

BCs are defined by selecting triangles on the input mesh; `attach.rs` maps each
selection to the nearest grid **boundary nodes** (within `0.9·h`) and assembles
the node-level problem. Supports are imposed by **penalty springs** or **DOF
elimination**; loads are converted to **consistent nodal forces**.

| BC / load | Symbol | Implementation | Notes |
|---|---|---|---|
| **Fixed support** | u = 0 | all 3 DOF of attached nodes eliminated (Dirichlet) | rigid clamp; can over-stiffen & create edge singularities — prefer Elastic for realism |
| **Displacement support** | pin chosen X/Y/Z | stiff axis penalty springs on the selected global axes only | roller/slider; `[true;3]` ≈ Fixed (penalty form) |
| **Frictionless support** | u·n = 0 | penalty spring along the patch's area-weighted average normal | works on arbitrary (non-axis-aligned) patches |
| **Elastic support** | σ = k·u | Winkler foundation; each node gets 3 axis springs of `k × tributary area` | bedding modulus `k` (N/mm³); area-consistent so total stiffness = k·A independent of mesh |
| **Surface force** | total **N** | area-weighted (Voronoi) split into consistent nodal forces | defined as X/Y/Z components OR direction + magnitude; direction defaults to the patch's average normal |
| **Pressure** | MPa | `f = −p·(Σ area-vectors)`, per-sample normals | correct on curved patches; acts along inward normal |
| **Gravity / self-weight** | g, ρ | body force `ρ·g·h³` per solid cell, lumped ⅛ to its 8 nodes | engine supports it; UI hides by default (negligible for desktop prints) |

**Penalty stiffness.** Frictionless/Displacement springs use
`k = 300 · E₀ · h` (`SPRING_FACTOR = 300`) — stiff enough to enforce the
constraint to engineering tolerance, soft enough to keep `K` well-conditioned.
Penalty constraints allow a small (bounded) constraint violation by construction;
the axial-bar and roller patch tests confirm < 3% error (Verification Manual).

**Consistent loads.** Surface forces and pressures are distributed by the actual
tributary area of each node (corner/edge/interior nodes get their proper share),
sampled on a sub-cell lattice — so a uniform traction reproduces the exact patch
test. Orphan samples on selections thinner than the grid are spread evenly with a
warning path.

---

## 8. Solution procedure

### 8.1 The linear system

`K u = f` is solved with a **matrix-free, geometric-multigrid-preconditioned
conjugate gradient (MGCG)** method (`crates/sig-core/src/mg.rs`,
`crates/sig-core/src/solve.rs`). `K` is never assembled: the operator `y = Ku` is
evaluated cell-by-cell from the single reference `KE` scaled by per-cell `eps`,
with an 8-color parity partition so the scatter-adds parallelize without races.

### 8.2 Why MGCG

For 3-D elasticity at 10⁵–10⁶ cells, direct factorization is infeasible in a
browser. Geometric multigrid gives a **mesh-independent** iteration count
(measured 8–9 MGCG iterations from 130k to 1M cells on solid parts); CG provides
robustness and a clean residual-based stopping test. Thin-shell parts with
high-contrast infill need more iterations (Benchy worst case ~170–290) because
1–2-cell features are *resolution-limited*, not solver-limited (documented
negative results in `mg.rs`).

### 8.3 Multigrid components

- **Smoother:** Chebyshev polynomial (degree `NU1 = NU2 = 3` pre/post) over a
  **block-Jacobi** preconditioner (3×3 per-node blocks). A fixed polynomial in
  `D⁻¹K` is a constant SPD operator, so the V-cycle remains a valid CG
  preconditioner while damping the upper spectrum far better than weighted
  Jacobi — this is what cuts iterations on high-contrast (thin shell + soft
  infill) grids. The Chebyshev interval is `[λ_max/8, λ_max]`
  (`CHEB_EIG_RATIO = 8`), with `λ_max` from power iteration × `1.1` safety.
- **Coarsening:** rediscretization (not Galerkin) with child-averaged cell
  stiffness; `KE` scales ×2 per level (linear in `h`). Hierarchy halves while
  dimensions stay even and ≥ 2 cells/axis, up to `max_levels = 5`.
- **Transfer:** trilinear prolongation `P`; restriction `R = Pᵀ`. Dirichlet/
  inactive DOFs masked throughout (vectors stay zero there).
- **Coarsest level:** block-Jacobi-preconditioned CG (≤ 800 iters, tol 1e-8).
- **Preconditioner contrast clamp:** inside the V-cycle only, non-void cells are
  floored to `PC_EPS_FLOOR = 0.20` relative stiffness. The *outer* CG uses the
  exact (unclamped) `eps`, so the answer is exact; clamping the up-to-10⁶:1
  boundary-sliver contrast just keeps the preconditioner effective.

### 8.4 Mixed precision

The outer CG loop and the operator `apply` run in **f64**; the V-cycle
preconditioner (the bulk of the flops) runs in **f32**. This is mandatory:
near equilibrium `K·u` sums element forces ~E·u that cancel to the ~10⁴× smaller
applied load, and pure-f32 cancellation noise *caps* attainable accuracy (and the
cap worsens as κ ∝ (L/h)²). Mixed precision costs ≈ +20% time and restores
textbook convergence — permanently guarded by a convergence assertion in the test
suite (this was the one real bug found in the Phase-1 spike; see
[`PHASE1_RESULTS.md`](../PHASE1_RESULTS.md)).

### 8.5 Convergence controls and defaults

| Setting | Default | Meaning |
|---|---|---|
| `tol` | `1e-5` | relative residual ‖r‖/‖f‖ stopping target for the interactive solve |
| `max_iter` | `600` | CG iteration cap (~2× the worst measured Benchy count) |
| `max_levels` | `5` | multigrid depth |

Hitting the cap is **reported, not fatal**: the returned field is the best
available approximation and the reached residual is reported (and plotted live in
the "nerd log"). The optimizer's *reference/baseline* solves run at a relaxed
`max(tol, 5e-4)` (compliance converges much faster than the residual) to cut the
post-optimize wait. The full per-iteration residual trace is returned for the
convergence plot.

### 8.6 Warm starts and caching

A `SolverCache` keeps the assembled hierarchy (colors, constraint masks,
block-Jacobi inverses, coarsened levels, the smoother's eigen-estimate) and the
last displacement field. The expensive setup depends only on the grid, the `KE`
parameters, the BC set, and the **void pattern** of `eps` — all of which usually
survive interactive re-solves, as-printed checks, and the optimizer's
verification passes. A change in `eps` *values* (same topology) rides the cheap
`update_eps` path; the previous `u` warm-starts the next solve so a load tweak or
a density update converges in a few iterations. The cache self-validates and
falls back to a cold rebuild when the grid/material/BCs/topology change.

### 8.7 Performance (measured)

From the 2026-06 perf round (AMD Ryzen 7 9800X3D, browser wasm):

- **1 M-cell solid solve:** 7.1 s single-thread → **1.26 s** threaded (×16).
- **3DBenchy (thin shell, ~300k cells):** 12.9 → **6.3 s**.
- Native (16 threads): 1 M cells in **0.88 s** (8 MGCG iters).

Threaded wasm requires the host to send **COOP/COEP** headers (SharedArrayBuffer);
without them the app silently falls back to the single-thread module. simd128 is
always on. Remaining levers (compact active-DOF indexing for sparse parts,
WebGPU) are noted in the engine notes.

---

## 9. Pre-solve diagnostics

Before solving, the engine runs the checks from `DESIGN.md §6`
(`crates/sig-core/src/check.rs`):

1. **Disconnected islands.** A 6-connected flood fill over solid cells finds
   separate bodies; **each** island must be independently supported and is
   checked separately.
2. **Rigid-body-mode (RBM) rank test.** Per island, the 6 rigid-body modes (3
   translations + 3 rotations) are tested against the constraint set: each scalar
   constraint contributes a row `g_k = d·u_k(p)` and the 6×6 Gram matrix
   `Σ gᵀg` is eigen-decomposed (cyclic Jacobi). The set is sufficient when
   `λ_min/λ_max > 1e-7`. Rotation rows are conditioned by the component centroid
   and half-diagonal.
3. **On failure**, the offending free rigid-body motion (translation + rotation
   vector about the centroid) is returned so the UI can **animate** exactly what
   is unconstrained, instead of a cryptic "singular matrix".

This catches the classic under-constraint and floating-island mistakes *before*
a meaningless solve. (Tested in `phase2.rs`.)

---

## 10. Analysis types

### 10.1 Linear static (solid)

The plain solve: the part as a solid of the chosen material, with the defined
loads/BCs. Per-cell `eps` = occupancy only. Outputs the displacement, stress,
strain, and SF fields. Entry point `Model::solve` / `solve_static` /
`solve_nodes`.

### 10.2 As-printed verification

`Model::solve_printed` solves the part **as it will actually be printed**: skin
(perimeters × line width, plus top/bottom shells) at 100%, interior at a single
uniform infill ratio through the calibrated pattern law (§4.3–4.4). It is the
same machinery as the optimizer's verification solve. This turns InFEAll into a
general FDM-FEA: stiffness, deflection, mass, and per-cell SF of the printed
part — its accuracy *is* the accuracy of the E(ρ) calibration. Reports min-SF and
which limit governs, mass at the print settings, and skin resolution.

### 10.3 Infill optimization

Continuous, SIMP-style **compliance minimization under a mass budget** using the
*physical* E(ρ) (`crates/sig-core/src/simp.rs`):

- **Objective/update:** minimize compliance `fᵀu` at a target mean interior infill
  (the slider %), via classic **optimality-criteria (OC)** updates with move
  limits, a bisection on the volume (mass) multiplier, a linear **conic density
  filter** (anti-checkerboard / minimum-member-size, §10.4), optional **planar
  symmetry**, and oscillation/2-cycle damping. ~30–100 warm-started multigrid
  solves; inexact inner solves while the layout forms, tightening as it settles.
- **Goal "match uniform stiffness":** one uniform solve sets the target
  compliance, then a guarded secant on the budget (≤ 5 warm passes) lands the
  *binned* design within ~2% — reports "same stiffness as X% uniform: −N% weight".
- **Binning:** the continuous field is quantized to **3 levels by default** —
  bottom level pinned at the printability **floor** (0.10 graded / 0.05 binary),
  upper levels by **strain-energy-weighted k-means in stiffness space**, with a
  mass-constrained assignment bisected so the binned design still meets the
  budget. Convex E(ρ) ⇒ load-bearing cells belong at high density, not the
  histogram mean (measured +15.2% vs uniform at equal mass on the cantilever).
  Cap 0.70 (graded). **Binary mode:** interior is either the floor or 100% solid
  (SIMP `p = 3`), exact at both endpoints in verification.
- **Verification solve** at the binned densities + walls produces the comparison
  card (stiffness retained vs solid, vs uniform, mass, max displacement).
- **Region export:** per-bin indicator → marching-tetrahedra isosurface → Taubin
  smoothing → small-region cleanup → slight dilation, emitted as nested
  overlapping modifier meshes (low→high density) so the slicer resolves them with
  no gaps (`crates/sig-core/src/bins.rs`, `threemf.rs`).

### 10.4 Minimum member size (printability length scale)

The density filter radius is driven by a physical minimum member size `d` (mm):
`radius_cells = clamp(d/(2h), 1.6, 8)`. Expressing it in mm (not cells) makes the
protected feature **mesh-independent** (refining the mesh no longer shrinks it).
Default `d` = 2 × line width. Advisory, not a hard guarantee — members below the
size blur below the bin threshold and drop out.

### 10.5 Not implemented

Modal/eigenvalue analysis, inertia relief + point masses, buckling, transient/
dynamic, thermal, contact, and nonlinear analyses are **not** in v1. Modal and
inertia relief are requested future work (`DESIGN.md §10`) that fit the existing
MGCG machinery (LOBPCG/Lanczos; self-equilibrated RHS with RBMs projected out)
but are **not present today** — do not assume them.

---

## 11. Results and post-processing

### 11.1 Field recovery

Strains are evaluated at **cell centers**, the superconvergent point of the
trilinear hex, where `dN_l/dx_i = s_li/(4h)` (`crates/sig-core/src/stress.rs`).
Strain is pure kinematics — it carries no `eps` factor and is therefore correct
even on partially-filled boundary cells. Stresses use the isotropic law (§4.1)
with a per-cell **effective modulus**.

The solve's stiffness factor `eps` factors as `eps = occupancy × material
density`, where `occupancy` is the finite-cell cut fraction (`grid.scale`, §4.5;
`= 1` for interior cells) and the density factor is the Gibson–Ashby `rel(ρ)`
(`= 1` for solid/skin). Two recovery modes follow:

- **Material stress (default, occupancy-decoupled).** Stress uses
  `E = E₀ · (eps ÷ occupancy)`, i.e. the material density factor alone. A cut
  boundary cell is *fully dense material partially covering its cube*, so this
  reports the **true material stress** there — for a solid part that is `E₀·ε`
  everywhere, including the skin. This removes the staircase **stress stripes**
  that otherwise appear wherever the visible surface is all cut cells (e.g. a
  curved skin: every column has a different occupancy, so an occupancy-scaled
  stress paints a different band per column even under a uniform strain).
- **Legacy (occupancy-scaled).** Stress uses `E = E₀·eps`, i.e. the exact
  modulus the solve used. Boundary cells then under-read by their occupancy.

`material_factor` (`crates/sig-core/src/stress.rs`) builds the decoupled factor;
the toggle is display-side only (`set_material_stress`). The **safety factor is
identical in both modes** — the allowable scales by the *same* factor as the
stress, so it cancels (§4.7). For a binned-infill cell either mode reports the
**homogenized (macro) stress** of the graded cell, differing only by the cut-cell
occupancy at the part boundary. Regression: `material_factor_removes_occupancy_stripe`.

### 11.2 Available fields

- **Stress (MPa):** von Mises, σxx, σyy, σzz, σxy, σyz, σzx
- **Strain:** equivalent (von-Mises) strain, εxx, εyy, εzz, γxy, γyz, γzx
- **Safety factors:** material, layer adhesion, worst-case (§4.7)
- **Displacement:** magnitude and components (mm/µm)

### 11.3 Smoothed (nodal) display

By default, result fields are recovered to the grid **nodes** by volume-averaging
the adjacent solid cells (a ZZ-style recovery; cell centers are accurate, the
staircase checkerboard lives between them) and evaluated **at the display
surface** (trilinear interpolation at STL vertices, exact nodal values on the
voxel hull). This is pure post-processing — the solution is untouched, and
toggling re-fetches fields only. It does **not** fix re-entrant-corner
singularities (those peaks never converge — advisory framing stands).

### 11.4 Visualization

Deformed shape with an editable exaggeration factor and playback; click-to-edit
color scale; min/max markers; a DRO-style **hover value probe** (barycentric
interpolation on the displayed triangle, formatted per field). The voxel-true
section view drops cells on one side of a plane to expose the interior cells and
the modeled wall thickness. An "element density" plot tints each cell by its
density/occupancy.

---

## 12. Assumptions & limitations

Consolidated, because this is the section an engineer must read before trusting a
number.

**Physics**
- **Linear elastic, small-strain, static only.** No large displacement/rotation,
  no plasticity/yielding, no creep, no hyperelasticity. Results scale linearly
  with load — beyond yield or large deflection they are non-physical.
- **No stability analysis.** Buckling/post-buckling is not detected; a slender
  part in compression may report fine while being unstable.
- **No dynamics, modal, thermal, fatigue, fracture, or contact.**
- **Single bonded body.** No assemblies, joints, or frictional contact. Separate
  islands are each checked but solved in one system.

**Material**
- **Isotropic stiffness.** FDM transverse isotropy (E_z < E_xy) and pattern
  in-plane anisotropy (e.g. grid) are **not** in the stiffness model (planned
  v1.x for transverse isotropy). Stiffness predictions for strongly anisotropic
  layups are optimistic in the weak direction.
- **Homogenized infill.** Infill is a smeared effective continuum, not resolved
  geometry. Local lattice-strut stresses are not produced; the macro stress is.
- **Calibration-bound accuracy.** Quantitative accuracy = accuracy of the E(ρ)
  curve and material allowables, valid only near the calibrated layer height/line
  width.

**Strength / safety factors**
- **Advisory, not certified.** Material and layer-adhesion SFs screen for bulk
  yield and tensile delamination respectively. **Shear-mode (Mode-II)
  delamination is not modeled.** SFs are capped at 99.

**Discretization**
- **Voxel staircase.** Curved/inclined surfaces are stair-stepped; cut-cell
  occupancy and nodal recovery temper but do not remove this. **Surface stress
  is the least trustworthy output**, especially with composite skin (it smears
  over the cell). **Deflection and global stiffness are the trustworthy
  outputs.** Material-stress recovery (§11.1) removes the *occupancy-scaling*
  stripe artifact, but the residual staircase strain error at the boundary
  remains — and decoupling occupancy amplifies any such boundary noise by up to
  `1 ÷ BOUNDARY_FLOOR` on the smallest cut cells, so pair it with smoothing.
- **Shear locking** of the first-order hex ⇒ use ≥ 4–6 cells across bending
  members; heed the < 3-cell feature warning.
- **Stress singularities** at re-entrant corners and point loads/supports do not
  converge — ignore those peaks; use nominal stress away from them.
- **Rigid Fixed supports** over-stiffen and create edge stress singularities;
  prefer Elastic (Winkler) support for realistic mounts.
- **Penalty constraints** (frictionless/displacement/elastic) admit a small,
  bounded constraint violation (validated < 3% on patch tests).
- **Resolution cap** of 4 M cells; very large or very thin-walled parts may be
  under-resolved at the chosen preset.

**Numerical**
- Iterative solver: a result may hit the **iteration cap** before `tol`; check
  the reported residual / convergence plot. Optimizer reference solves use a
  relaxed tolerance by design.

When in doubt, **refine the mesh and re-run** — if deflection/stiffness is stable
under refinement, trust it; if a stress peak grows without bound, it is a
singularity, not a result.

---

## 13. References

**Method lineage** (all permissively published math):
- O. Sigmund, "A 99 line topology optimization code written in Matlab," *Struct.
  Multidisc. Optim.* 21 (2001) — the matrix-free element-scaling formulation.
- Talischi et al., "PolyTop" — OC/density-filter topology optimization.
- A. Barill et al., "Fast Winding Numbers for Soups and Clouds," *ACM TOG* 2018 —
  robust inside/outside classification.
- J. Parvizian, A. Düster, E. Rank, "Finite cell method," *Comput. Mech.* 2007 —
  the cut-cell / ersatz-material occupancy convention.
- M. Adams et al., on parallel multigrid with Chebyshev/polynomial smoothers.
- L. J. Gibson, M. F. Ashby, *Cellular Solids: Structure and Properties* — the
  `E ∝ ρⁿ` infill homogenization law.

**Verification references** (formulas used in the test suite — see the
[Verification Manual](verification-manual.md)):
- S. Timoshenko, *Strength of Materials* / *Theory of Elasticity* — beam
  deflection with shear, Kirsch plate-with-hole stress concentration.
- W. D. Pilkey, *Peterson's Stress Concentration Factors* — shoulder-fillet Kt.
- Betancur et al., *Tecciencia* 12(23) 2017 — fillet Kt benchmark values (D/d=1.5).
- R. J. Roark / W. C. Young, *Roark's Formulas for Stress and Strain*.

**Project documents:** [`DESIGN.md`](../DESIGN.md),
[`PHASE1_RESULTS.md`](../PHASE1_RESULTS.md),
[Verification Manual](verification-manual.md).
