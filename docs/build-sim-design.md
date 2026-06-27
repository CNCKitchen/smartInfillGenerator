# FDM Build Simulation — Design Note (inherent strain)

*Drafted 2026-06-26 via a "grill-me" design interview. Status: **tech demo scope**, pre-prototype.
This is a forward-looking feature note, not yet a resolved DESIGN.md decision. Calibration and
product-fit are explicitly deferred (see §6).*

## 0. One-paragraph summary

Add an FDM build simulation that reuses InFEAll's existing matrix-free geometric-multigrid
voxel FEA to apply an **inherent (eigen)strain** field and predict (a) **warping** — for
part predeformation / gcode morphing — and (b) **bed peel / plate release**. A third target,
**interlayer delamination**, was scoped out of the MVP: it depends on thermal history the
inherent-strain method discards, and can't be done honestly without gcode-derived layer-time
data (§5). Method of record: **inherent strain**, single material/process, calibrated, with
**no claimed generalization**.

## 1. Scope (MVP)

| In | Out (deferred) |
|----|----------------|
| Warping → predeform / gcode morph | Interlayer delamination (→ §5) |
| Bed peel / plate release | gcode parsing (→ §5) |
| STL input only | Sequential layer activation (assume single-shot; §3) |
| Single material + process, **labeled as such** | Generalization across materials/processes |

## 2. Discretization & strain model

- **Input:** STL only. User provides **wall count** and **bottom-layer count**.
- **Surviving anisotropy:** at FEA voxel scale a voxel spans many print layers, so the
  alternating ±45° infill raster **homogenizes away**. The anisotropy that survives is
  **in-plane vs. build-Z (transversely isotropic)**. Consequence: **the raster angle is not
  needed**, which is *why* STL-only is defensible. Do **not** build the MVP around knowing the
  infill direction.
- **Walls:** the continuous perimeter is the one structure whose direction does *not*
  homogenize → **coherent in-plane directional** eigenstrain along the perimeter tangent
  (surface normal projected into the layer plane). Wall voxels come free from the existing
  `classify_cells` skin classification.
- **Infill:** **transversely isotropic**, with eigenstrain **magnitude scaled off the existing
  density field** — not a hard-coded constant (a constant would contradict the product whose
  whole premise is spatially graded density).
- **Bottom/top solid shell:** a *third* class — solid + directional, and the bed shell is the
  peel-critical region. It is typically **sub-voxel** (e.g. 4 × 0.2 mm = 0.8 mm shell vs.
  1–2 mm voxels). Handle via **sub-voxel volume-fraction weighting** on the bed-adjacent voxel
  layer (the same skin-stiffness trick already shipped for walls) — **NOT** geometric mesh
  refinement (see §3).

## 3. Solver integration & resolution

- **Reuse** the existing matrix-free **geometric multigrid** elastic solver. Inherent strain is
  a natural fit: apply eigenstrain → one elastic solve → residual distortion + stress field.
- **No local first-layer refinement.** Refining only the bottom layers breaks the uniform grid
  that geometric multigrid depends on (→ would require AMR/octree multigrid + hanging nodes =
  solver rewrite) **and** it is resolution-dependent, which fights the voxel-independence goal.
  Global Z-refinement is also rejected (4–8× elements + aspect-ratio-driven MG convergence loss).
  → Bed shell is handled by **sub-voxel weighting (§2)**, not refinement.
- **Arbitrary voxel size** is a user knob (quick vs. detailed runs). This imposes a hard
  requirement: **the same part must produce the same verdict across resolutions.**
- **Sequential layer-stack activation (DECIDED 2026-06-26 — required).** Single-shot (all cells
  active, one solve) was the initial MVP assumption but is **rejected**: stress-accumulation
  history matters, especially for **progressive bed peel** (the peel front grows with height, which
  an end-state-only solve cannot see). Activation increment = **one voxel Z-layer** (M = nz; the
  several physical print layers inside a voxel layer homogenize, §2).
  - **Activation method — quiet vs. inactive is an OPEN benchmark (do not assume quiet).**
    Established metal-AM practice (Michaleris quiet/inactive comparison; Ansys element birth/death;
    the modified-inherent-strain codes) prefers the **inactive** method, or Michaleris's **hybrid**
    (quiet for the just-activated layer, inactive above), *because pure-quiet's near-zero elements
    pollute the solve* — the exact conditioning artifact we worried about. The catch: their solvers
    are direct/sparse, where "inactive" is cheap DOF bookkeeping; *our* geometric multigrid makes
    inactive (active-set change) the expensive one (hierarchy rebuild), so the cost tradeoff is
    inverted for us. Therefore **prototype BOTH on a cantilever and measure**:
    - **Quiet:** full grid fixed, dormant cells at the `EMIN_REL` floor, activate by ramping `eps`
      via the cheap `update_eps` path → hierarchy intact, warm-started incremental solves. Risk:
      ~1e6 active/dormant contrast → MGCG convergence loss. **Measure iterations/step.**
    - **Inactive (trim-to-height):** rebuild the hierarchy each step on a grid only as tall as
      printed so far (early steps cheap, growing). Correct, no contrast; cost = M rebuilds (prolong
      previous `u` as warm start onto the taller grid).
  - **Activation rule (standard; verified vs PMC variant + Michaleris 2014):** each new layer's
    cells are added **strain-free at their NOMINAL mesh coordinates** — NOT on the deformed
    substrate (the printer lays material at design position) — and only *that layer's* eigenstrain
    is applied; the whole active structure re-equilibrates. Path dependence comes from each cell
    being strain-free *in the config it was born into* (reference locked at activation), carried as
    a **locked-reference force** `f_lock = Σ_born Kₑ·u_birth`; each step solves the total
    `K u = f_eigen + f_lock`. Because new top nodes start at nominal (0), the **last layer adds only
    its own one-layer strain and barely moves** — it must not be the most-distorted region (an
    earlier "inherit the deformed substrate" version violated this and spiked the top ~5×; fixed).
    Consequence: for a pure-elastic *uniform* eigenstrain the build-order effect is **mild**, showing
    only where geometry couples (cross-bar, overhangs); the large sequential effects in real AM come
    from plasticity + bed release, not yet modelled.
  - **Layer-lump floor (from the literature):** computational layers can't be arbitrarily thin —
    one PMC sensitivity study saw convergence issues below ~3 physical layers/lump (they used 6)
    and needed ≥2 computational layers across a feature. → build-sim voxels have a **sweet-spot
    height**, not "finer is better"; this also bounds step-count cost.
  - **Prior art (don't reinvent):** sequential activation + layer lumping + one quasi-static solve
    per computational layer is the standard metal-AM inherent-strain approach since ~2014. Key
    refs: Michaleris 2014 (quiet vs inactive vs hybrid activation); Keller & Ploshikhin 2014 /
    Liang, Cheng, Chen et al. (modified inherent strain, anisotropic strain extracted + applied
    layer-by-layer in quasi-static equilibrium, cantilever-calibrated); Ansys Additive (super
    layers / element birth-death). The FDM novelty is only the *physics of the eigenstrain itself*
    (polymer thermal contraction, transverse isotropy), not the activation machinery.
  - **Voxel-size independence preserved:** each layer applies the *full* eigenstrain to its cells
    on activation, so the total is M-independent — finer voxels give finer *history resolution*,
    not a different answer. (Cost: step-count scales with Z-resolution.)

## 3a. Two-state solve (the deformation spine)

Warp and peel fall out of one sequential build followed by one release solve:

1. **State 1 — bonded build (sequential loop, §3).** Bed plane fixed throughout. At step k,
   activate voxel-layer k (apply its eigenstrain to the freshly-active cells), warm-solve,
   accumulate `u`. Across the loop, record the **bed-interface stress envelope** — the max peel
   stress each bed location experiences over the whole build. That envelope is the peel indicator
   (§4.2) and needs **no contact release** (consistent with "indicate risk, don't model contact").
2. **State 2 — released (single final solve).** After the full bonded build, remove the bed BC and
   relax to free equilibrium once. The **displacement field is the warp / predeform target** (§4.1)
   — the cantilever spring-back the §6 calibration bars measure.

**Gotcha:** State 2 is *unconstrained* — the part floats (6 rigid-body modes) and the existing
under-constraint guard (DESIGN item 6 rigid-body rank test) will flag it. Do **not** add real
supports (injects fake stress). Use **inertia relief** or a minimal **3-2-1 pinning** that removes
rigid-body motion without restraining deformation. This is a dedicated path through the constraint
checker, distinct from a normal supported solve.

## 4. Outputs & voxel independence

**General principle:** *resultant / length-averaged* quantities converge under refinement;
*pointwise peak* stress/strain at geometric stress concentrations does **not** (it diverges, or
on a voxel grid is set by voxel size). This is a pre-existing issue in the structural solver too;
a future **elastic-perfectly-plastic** model would cap stress, but **max plastic strain still
localizes** and stays mesh-dependent — so plasticity is not a free fix for pointwise maps.

### 4.1 Warping
Residual distortion field — inherent strain is *made* for this. Convergent. Used to predeform
the mesh (apply negative distortion) and/or morph gcode.

### 4.2 Bed peel / plate release — **per-voxel localized field (option B)**
Chosen **deliberately over a global resultant**: peel is **initiation-controlled** — a fracture
front starts where local interface stress beats local adhesion at one corner. A resultant would
miss exactly that failure mode.

**Non-negotiable architectural constraint (honor even in the throwaway demo — it is the only
expensive-to-retrofit decision):**
> Store the peel field on **physical coordinates** and design the smoothing as a
> **physical-length (mm) kernel**, never a voxel-count kernel.

Rationale: a fixed-**mm** blur is **both** localized (shows *which* edge) **and**
voxel-convergent (coarse and fine agree). A fixed-**voxel** blur smooths more on coarse than
fine → mesh-dependent → "quick vs. detailed" runs disagree → trust bug. The blur length `X` is a
**physical calibration parameter** (process-zone size), not a display slider — and it secretly
carries the fracture-mechanics physics we're choosing not to model (see §6).

### 4.3 Cross-resolution agreement test
Add a CI test: voxelize a fixed warpy reference part at e.g. 2.0 / 1.0 / 0.5 mm and **assert the
warp magnitude and peel verdict agree within band.** Turns "voxel independence" from a wish into
a release gate, in the same spirit as the existing analytic + CalculiX golden tests.

## 5. Delamination — why it's deferred

Inherent strain throws away thermal history. Delamination is governed by **interface temperature
when the next layer lands** (a function of layer time + local cross-section), i.e. exactly what
the method deletes. Symptoms that killed it for the MVP:
- Two identical geometries printed fast-cold vs. slow-hot get an **identical** elastic field but
  different real outcomes → a stress map can't tell them apart.
- The interlayer bond plane is **sub-voxel**; σ_zz is a homogenized continuum, not a weld traction.
- Pointwise σ_zz at stress concentrations is **voxel-dependent** (§4).

**Unlock:** parse gcode for **layer time + per-layer cross-sectional area** → a cooling proxy →
a spatially varying bond-strength denominator. gcode is the common unlock for *three* future
gains at once: bead anisotropy beyond walls, as-printed density mapping onto the stress model,
and the delamination thermal proxy.

## 6. Calibration & validation (DEFERRED — path sketch only)

Single material + process. **No real generalization** — and the demo must say so.

- **Inherent strain:** **cantilever-release curl** bars (bonded to base, cut free, measure
  spring-back) — *not* uniform free-shrink bars (that measures the wrong quantity). Bars in
  **multiple print orientations** to capture the in-plane-vs-Z components. Print at different
  speeds to map process dependence. **Validate** on an independent **3D-scanned larger part**
  (clean calibrate/validate split).
- **Bed peel:** a load-cell pull gives uniform Z/shear release stress — but that is
  **strength-of-materials**, while real peel is **fracture mechanics**; pull-off strength
  *overestimates* peel resistance. So **do not** fit the threshold from a pull test and pick `X`
  by eye. **Jointly fit (threshold, blur-length X) against actual corner-lift events** so the
  *pair* reproduces real lift. `X` absorbs the fracture physics we don't model — fit it together
  with the threshold, not separately.
- **Validation bar:** build sim has **no analytic and no CalculiX ground truth** — it needs a
  **physical** measurement campaign. That campaign (not the solver prototype) is the real gate on
  whether this becomes a product vs. a demo.

## 7. Open / strategic

- **Product fit:** does build-sim belong in InFEAll, or is it a separate product sharing only the
  voxel grid (different user, different physics, different validation culture)? **To be answered
  via literature + a few tests, not from the armchair.**
- The gcode pipeline (§5) and its three payoffs.
- Quiet-element conditioning under sequential activation (§3) — validate MGCG iterations/step.
- **Map the displacement field onto the ORIGINAL mesh, not the voxel hull.** Today
  `deformed_hull_stl` warps the blocky voxel surface. For a real predeform export (and clean
  visuals) the warp must be sampled onto the original STL/CAD triangle vertices — morph the input
  mesh by the released-state displacement field (and invert it for predeformation). Needed before
  the predeform/gcode-morph output is usable.

## 8. Placement — separate top-level mode (decided 2026-06-26)

Two top-level modes sharing the front-end plumbing, viewer, and sig-core solver:
- **"Simulate & Optimize"** — the existing tool (structural FEA + infill/topology optimization).
- **"Build Sim"** — the new inherent-strain build simulation.

Chosen = **option C** (separate mode), over a shared rail step (B) or an experimental sub-branch
(A). Rationale: different physics, different user intent, different validation culture — and clean
separation means Build Sim can be **deleted as a unit** if §7's strategic question resolves against
it, rather than untangled from the structural flow. Marginal cost is low: both modes share Model
import, voxelization, the viewer/colormap/legend, and the sig-core solver; they diverge only at the
rail, the analysis, and export.

### Mode switch
- Top-bar **segmented control**: `Simulate & Optimize | Build Sim`.
- `store.ts`: add `appMode: "optimize" | "buildsim"`; rail steps, active step, and result kinds
  become mode-scoped.
- `StepRail.tsx`: `STEPS` parametrized by mode.

### Build Sim step rail (trim of the 6-step structural rail)
| Structural step | Build Sim |
|---|---|
| 1 Model | **1 Model** — import + set **print orientation / seat on plate** (orientation is a build-sim-only input; the plate is the Z=0 boundary) |
| 2 Loads (structural BCs) | **dropped** — no structural BCs; the only "load" is the eigenstrain + the plate |
| 3 Properties | **2 Print settings** — material (→ eigenstrain calibration), wall count, bottom-layer count, analysis grid (quick/detailed voxel size) |
| 4 Verify | **inlined** into Simulate's pre-solve gate. The structural under-constraint check is *inverted* here — State 2 is deliberately free (inertia relief, §3a), so that guard must **not** fire |
| 5 Optimize infill | **3 Simulate** — the two-state solve (§3a) → warp field + peel field |
| 6 Export | **4 Export** — predeformed mesh (STL/3MF) and/or morphed gcode |

→ Lean 4-step rail: **Model · Print settings · Simulate · Export.**

### Code reuse
- **Solver:** eigenstrain enters as an **RHS force** on the existing `StaticProblem`/`NodeProblem`
  (same path as prescribed displacement — never invalidates the cached matrix). Reuse `solve.rs`,
  `fem.rs`, `mg.rs`, `stress.rs` wholesale. New **`crates/sig-core/src/buildsim.rs`**, a *sibling*
  to `pipeline.rs` (not part of the SIMP loop); new `sig-wasm` entry point.
- **Shared touch point:** State 2's free-body solve needs inertia relief / 3-2-1 pinning, which
  intersects `check.rs` — the under-constraint guard must not fire on the deliberately free part.
- **Viewer:** warp rides the existing `ViewMode "deformed"` path; peel is a new bottom-surface
  field on the same colormap / legend / exaggeration infra.

---

*Build the single-shot inherent-strain solver first; treat every number as uncalibrated until §6
exists. Honor the §4.2 physical-coordinate / physical-length-blur constraint from line one.*
