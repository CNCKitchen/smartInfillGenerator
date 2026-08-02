<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com> -->

# Feature resolution & submodeling — implementation plan

**Status (2026-08-02):** investigation finished, nothing user-facing built.
Branch `feat/surface-stress-accuracy`, unpushed, merged with the read-back
fixes from `stress-readback-fixes`. All evidence below is reproducible from
`crates/filasim-core/tests/surfstress.rs` (Tier 7) and `bin/kirschbench`.

This document exists so implementation can start in a fresh session without
re-deriving any of it. For the measured results in their permanent home see
[Verification Manual §6b and §10](verification-manual.md).

---

## 1. The problem, in one table

In production the cell size is set by the **print** — extrusion width and layer
height — not chosen for stress accuracy. On a 1 mm fillet (Ansys-referenced
stepped round bar, case 7.12) that means:

| cell size `h` | cells per fillet radius | max σ₁ under-read | SF overstated by |
|---|---|---|---|
| 0.9 mm | 1.1 | −38.6 … −46.5% | 1.63 … 1.87× |
| 0.45 mm | 2.2 | −21.3 … −28.0% | 1.27 … 1.39× |

At `h`=0.9 the implied bending Kt is **0.85** — below one, so a stress *raiser*
is rendered as a stress *reducer*. **Nothing on screen says so.** The silence is
the defect; the error is just its size.

Generalised across the six mesh levels in case 7.12:

| cells per feature radius | typical σ₁ under-read |
|---|---|
| 1–2 | −25 … −47% |
| 4 | −11 … −26% |
| 8 | −3 … −15% |
| 16 | −7 … −10% |

**Working rule: feature radius ≥ 8 × cell size for ~10% accuracy.** At a 0.45 mm
line width, fillets ≥ 3.6 mm are fine; below ~2 mm the peak is materially
under-reported. (Unverified against a second geometry — see §8.)

---

## 2. What is already fixed, and what those fixes do NOT fix

Two read-back defects were found and fixed (merged, tested, shipping-on):

**F1 — directional occupancy decoupling** (`stress::cut_normals` +
`decouple_traction`). A cell cut *perpendicular* to the stress is a soft link in
series; its ersatz stress is already the material stress, so the scalar
`material_factor` divides by occupancy a second time. Case 7.11 measures the
error tracking `1/occ − 1` exactly:

| column occupancy | production | with F1 |
|---|---|---|
| 0.83 | +19 … +21% | −0.7 … +1.0% |
| 0.67 | +52 … +59% | +1.3 … +6.2% |
| 0.50 | **+108%** | +4.2% |

6 of 11 mesh sizes are affected — the voxelizer centres the grid in x/y, so a
flat face lands mid-cell most of the time. **This is the more important of the
two fixes for ordinary parts:** a doubled stress in a *uniform far field* has no
notch present to warn anyone it is wrong. F1 is a no-op on every stress
concentration in the tier and identically zero on interior cells.

**F3 — surface recovery** (`stress::recover_surface`). Fits clean interior cells
and evaluates at the boundary cell centre, so the staircase-corrupted cells are
never read. It makes the displayed peak *converge* under refinement where the
raw one does not (Richardson limit 3.588→3.758 raw vs 3.354→3.225 recovered;
observed order 0.42→0.33 vs 0.76→0.98; traction residual MAX flat at ~11% vs
falling 17.1→6.5%).

> **The sign rule — the one portable statement about F3.** It only ever biases
> the reading **downward**. So it helps exactly where the baseline over-reads and
> hurts where the baseline under-reads. The cell count at which a feature's
> baseline changes sign is a property of *that feature*: ~10–20 cells/radius on
> the Kirsch hole, and **not reached even at 16** on the round-bar fillet, where
> F3 loses at every mesh tested. Do not hard-code a threshold. `surface_band` +
> `MeshQuality` measure this per solve, which is why they exist.

**Neither fix addresses feature resolution.** Both are read-back corrections on
a given mesh. If the mesh cannot see the fillet, no read-back recovers it — which
is what this plan is about.

---

## 3. Why global refinement is not the answer

The cell size is coupled to the print model (walls, infill, layer adhesion), so
it cannot be lowered for stress accuracy alone. And even ignoring that, on the
Ansys bar reaching 16 cells per fillet radius globally needs `h`≈0.06 → **~35M
cells**. Measured: `h`=0.125 gives 4.4M cells and still −10 … −15%.

Submodeling reaches the same resolution for a fraction of that, and it was
measured working (cases 7.9, 7.10, 7.13):

| | cells | max σ₁ vs reference |
|---|---|---|
| global h=0.25 (4 cells/rf) | 1.05M | −20.6 … −10.9% |
| **submodel h=0.0625 (16 cells/rf)** | 1.05M + 6.2M | **−9.5 … +1.2%** |
| global equivalent | ~35M | — |

Validation that it is the *same answer*, not a coincidence: `sub h=0.125 from
h=0.5` reproduces the global `h`=0.125 to 0.2 / 1.5 / **0.0** points across
axial / bending / torsion. On the Kirsch plate the free-surface traction
residual matches its global counterpart to the decimal in every pairing.

**Box size barely matters given an adequate driver** (case 7.10): from a 5
cells/radius global, ±1.25a is as good as ±4a. The submodel re-solves the
perturbation itself; it needs only correct boundary *data*. Box size matters only
when the driver is pathological (2.5 cells/radius), where bigger is monotonically
better. **Rule: ±1.5× feature size from a driver with ≥5 cells per feature
radius, ±3× below that.**

> **This is VOXEL submodeling.** The box is re-voxelized with the same
> production convention and the same staircase — it *out-runs* the staircase, it
> does not remove it. Conforming/body-fitted submodeling is untested and unbuilt;
> it breaks the one-KE-scaled matrix-free/SIMD fast path ([DESIGN.md:110](../DESIGN.md#L110))
> and the prize shrank now that voxel submodeling reaches within 7–10%.

---

## 4. Phase 1 — warn (cheap, no solve, do this first)

The dangerous property is silence, not error size. A **geometric** resolution
check needs no solve and can run at import.

**Method.** `stress::cut_normals(grid)` already recovers a per-cut-cell outward
normal from the occupancy gradient (built for F1, already computed on the
display path). The *change* in that normal between adjacent boundary cells is a
discrete curvature:

```
local feature radius  r ≈ h / |n̂_i − n̂_j|      for adjacent boundary cells i, j
cells per radius      = r / h  =  1 / |n̂_i − n̂_j|
```

Flag any connected boundary region whose median falls below ~8. Smooth over a
small neighbourhood — single-cell normal differences are noisy on a staircase.

**Report.** Paint the flagged regions in the Mesh/Results view and put one line
in the dock: *"fillet at ⟨location⟩: ~2 cells across the radius — peak stress
under-reported, SF likely optimistic by ~1.5×."* Derive the factor from the
table in §1.

**Why first:** converts a silent 1.9× SF overstatement into a known limitation,
needs no new solver path, and is roughly a day of work. It is also the input
Phase 2 needs to choose a box.

---

## 5. Phase 2 — auto-submodel the hot spot

### Pipeline

1. Solve globally at print resolution (unchanged).
2. Locate the peak — the min-SF cell the dock already finds.
3. Build a box around it: half-width per the rule in §3, minimum a few cells.
   Cut only where the box crosses material; surfaces of the part inside the box
   stay free.
4. Re-voxelize the box at `h/4` (or `h/8` if affordable).
5. Interpolate global displacements onto the box's **artificial cut faces only**
   and impose them as penalty springs + equivalent forces.
6. Solve. Read the peak through the same read-back path as the global.
7. Report the submodel peak as the headline with the global as context, and state
   the resolution reached.

### Code sketch (validated — this is what the harness does)

```rust
// 5. Dirichlet on the cut faces. k mirrors attach.rs SPRING_FACTOR = 300.0.
let k = 300.0 * settings.e0 * pg_sub.h;
let mut np = NodeProblem::default();
for n in cut_face_nodes {                     // ONLY the artificial faces
    let Some(ud) = sample_u_trilinear(&pg_global, &u_global, &active_global, pos(n))
        else { continue };
    for d in 0..3 {
        let mut dir = [0.0; 3];
        dir[d] = 1.0;
        np.springs.push((n as u32, dir, k));
        if ud[d] != 0.0 {
            let mut f = [0.0; 3];
            f[d] = k * ud[d];                 // penalty force ⇒ DOF settles at ud[d]
            np.forces.push((n as u32, f));
        }
    }
}
let u_sub = solve_nodes(&pg_sub, levels, &np, &settings)?.u;
```

`sample_u_trilinear` is standard trilinear interpolation over the padded node
grid that **skips inactive nodes and renormalises by the accumulated weight** —
copy the weighting from `SurfaceStress::sigma` in `tests/surfstress.rs`.

### The one piece of new core API you need

`VoxelGrid::voxelize(mesh, h)` takes its bounds from the whole mesh
([voxel.rs:58](../crates/filasim-core/src/voxel.rs#L58)) — there is **no
sub-box variant**. Add one:

```rust
pub fn voxelize_in_box(mesh: &TriMesh, h: f64, lo: [f64; 3], hi: [f64; 3]) -> Self
```

Same `WindingBvh` inside-test, same occupancy supersampling, same
`BOUNDARY_FLOOR` solid rule — only the extent differs. Keep the convention
identical or the submodel stops being comparable to the global.

---

## 6. Verified API inventory

| symbol | file | note |
|---|---|---|
| `stress::cut_normals(grid) -> Vec<[f32;3]>` | stress.rs | F1 normals; also the curvature input for Phase 1 |
| `stress::decouple_traction(&mut [f64;6], n, occ)` | stress.rs | identity at occupancy 1 |
| `stress::recover_surface(grid, cells) -> Vec<f32>` | stress.rs | F3; `SURFACE_PATCH_CELLS = 2` |
| `stress::surface_band(grid, raw, recovered) -> Option<SurfaceBand>` | stress.rs | `{peak, bound, band, quality, ..}` |
| `stress::MeshQuality` | stress.rs | `BAND_RESOLVED = 0.08`, `BAND_MARGINAL = 0.20` |
| `stress::cell_field_cut(.., kind, cut: Option<&[[f32;3]]>)` | stress.rs | `cut = None` is byte-identical to the old path |
| `eps::material_factor(grid, eps)` | eps.rs | re-exported as `stress::material_factor` |
| `solve::NodeProblem { fixed, springs, forces, rigid }` | solve.rs | node-level BCs |
| `solve::active_nodes(grid) -> Vec<bool>` | solve.rs | |
| `solve_nodes(grid, levels, np, settings)` | lib.rs | |
| `pad_for_levels(grid, max_levels) -> (VoxelGrid, usize)` | lib.rs | |
| `attach::BcKind::Displacement([bool;3], [f64;3])` | attach.rs | prescribed displacement, rides the force RHS |
| `Model::set_surface_recovery(bool)` / `field_uncertainty(kind)` | filasim-wasm/lib.rs:1319, :3466 | existing wasm hooks |
| `VoxelGrid { nx, ny, nz, h, origin, scale }` | voxel.rs:13 | `scale` = occupancy, index `(cz*ny + cy)*nx + cx` |

Read-back order on the shipping path is `cell_field_cut(Some(cut))` →
`recover_surface` → `recover_nodal` ([filasim-wasm/lib.rs:3425](../crates/filasim-wasm/src/lib.rs#L3425)).

---

## 7. Pitfalls that already bit

- **Only the artificial faces get Dirichlet data.** In the round-bar submodel the
  box is cut in `x` only; the cylindrical surface inside it is the part's own
  free surface. Constraining it would have silently changed the geometry. Same
  trap on the Kirsch plate, where the box's `z` faces are the plate's real free
  surfaces.
- **`MeshQuality` flags the regime where F3 under-reads**, and F3 under-reading
  is the *unconservative* direction. Take the verdict from `bound` whenever
  quality is not `resolved`. Calibration re-checked against `kirschbench`: worst
  unconservative bound is −0.7% on meshes the band calls trustworthy.
- **Nonlinear fields are node-averaged as scalars**, not as tensors — von Mises,
  SF and σ₁/σ₂/σ₃ are each evaluated per cell and *then* averaged. The displayed
  σ₁ is therefore not the same quantity Tier 7 reports as `Kt(σ₁)` (which
  recovers the six components first). See Theory Manual §11.3.
- **A 2-D Peterson chart is not a valid reference for a thick section.** Case 2.6
  looks like an over-read against it; the round 3-D version against Ansys
  under-reads. Do not tune anything against the flat-bar fillet.
- **Don't re-run Tier-7 benchmarks expecting the read-back fixes to show up
  unless the harness opts in** — `cell_field` defaults `cut = None` and nothing
  calls `recover_surface` implicitly.

---

## 8. Open / unverified — do not treat as settled

- **The 8×-cell-size rule rests on one geometry** (1 mm fillet, three load cases)
  against **one Ansys run whose own mesh convergence is not independently
  established**. If that run is under-converged at the fillet, every error in §1
  is understated by the same amount. *Ask Stefan for a refinement check before
  this rule goes into user-facing text.*
- **Submodel drivers so far are always a uniform far field.** A box cut through a
  region with its own gradients is untested and is the most likely failure mode.
- **Curvature-from-normals (Phase 1) is a sketch, not a measurement.** Nobody has
  checked it recovers the right radius on a staircase.
- **F1's original defect was at a *constrained* symmetry plane;** case 7.11 uses
  a free end face to keep the reference exact. The constrained variant is
  untested.
- **`cutcell.rs` (exact cut-cell stiffness) is finished, tested and off.** Every
  prior evaluation ran at resolutions where read-back error swamped it. The cheap
  open experiment: solve a 7.13 submodel box with the exact operator and see
  whether it recovers the residual few percent now the mesh is fine enough.

---

## 9. How to reproduce anything above

```bash
# Tier 7 — everything except the 111-minute refinement case
cargo test -p filasim-core --test surfstress -- --ignored --nocapture \
    --skip surf_kirsch_h_refinement

# Named cases
cargo test -p filasim-core --test surfstress -- --ignored --nocapture \
    stepped_round_bar_vs_ansys              # 7.12, the Ansys reference
cargo test -p filasim-core --test surfstress -- --ignored --nocapture \
    stepped_round_bar_submodel_vs_ansys     # 7.13, submodel incl. torsion
cargo test -p filasim-core --test surfstress -- --ignored --nocapture \
    far_field_cut_perpendicular_to_stress   # 7.11, the F1 claim, ~10 s
cargo test -p filasim-core --test surfstress -- --ignored --nocapture \
    surf_kirsch_submodel_box_sweep          # 7.10, box size vs driver

# Independent cross-check (needs the QUARTER part — the full plate fails)
node web/scripts/tessellate-step.mjs KischPlateWHole_quater.step kirsch-q.bin
cargo run --release -p filasim-core --bin kirschbench -- --mesh kirsch-q.bin \
    --h 1.0,0.7,0.5,0.37,0.25,0.2,0.125
```

Test models live in the repo root: `KischPlateWHole_quater.step`,
`roundbarwithstep.step`.
