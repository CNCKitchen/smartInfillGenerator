// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Strength criterion for the SF-target optimization goal (DESIGN §17).
//!
//! The goal "minimize material s.t. SF ≥ target" needs one scalar per candidate
//! design: **SF_crit**, a mesh-stable stand-in for "the worst safety factor in
//! the part". A raw minimum cannot serve — elastic stress at re-entrant voxel
//! corners is mesh-divergent, so the raw min chases staircase artifacts and
//! recedes under refinement. The §17 dec. 4 remedy, in evaluation order:
//!
//! 1. **Per-cell SF** ([`sf_cells`]): the same math as the display's
//!    `sfm`/`sfz`/`sf` fields — von Mises vs in-plane strength, and the §15
//!    layer-adhesion interaction (⟨σzz⟩₊/Sᵗᶻ)² + (τ/Sˢᶻ)² = 1/SF² — so
//!    "optimizer says SF 2.0" is the number the SF plot shows (dec. 2).
//!    Allowables scale by the cell's stiffness factor (Gibson–Ashby, first
//!    order), exactly like the display.
//! 2. **Smoothing** ([`smooth_masked`]): volume-averaged nodal recovery of the
//!    SF field and re-interpolation to cell centers — single-cell staircase
//!    spikes melt into their neighborhood, the same remedy the display's
//!    smooth-stress toggle applies. Masked-out cells (ersatz void in solid
//!    mode) are excluded from the recovery so their meaningless near-zero
//!    stress (SF = cap) cannot inflate their neighbors.
//! 3. **Minimum over the scored set** ([`sf_min`]): SF_crit is simply the
//!    lowest smoothed value among the cells the criterion scores. **The number
//!    the app reports IS the minimum of the field the app plots** — no
//!    reduction the user cannot see.
//!
//! Until 2026-07-25 stage 3 was a volume-weighted TRIMMED percentile: the worst
//! [0.2 %] of the part's volume was discarded before taking the minimum, on the
//! critical-distance argument that peak stress at a notch tip is mesh-dependent
//! and a fixed volume fraction corresponds to a fixed distance from the tip.
//! That was mathematically defensible and practically unusable: the margin it
//! bought depended on the SHAPE of the hot spot (measured: +2 % on a beam whose
//! weakest material is a long uniform fiber, +23 % on a hook whose weakest
//! material is one fillet) and on the PART SIZE (0.2 % of a 400 g bracket is a
//! far bigger blob than 0.2 % of a 12 g hook). A margin nobody can predict from
//! the setup is a margin nobody can trust — and on screen it read as a bug: the
//! panel quoted 2.02 while the plot's min marker, on the very same field, read
//! 1.64. Retired deliberately (Stefan, 2026-07-25) with its consequence
//! accepted: **SF_crit now falls as the mesh is refined** near a stress riser,
//! and at a perfectly sharp re-entrant corner it does not converge at all. The
//! app says so where the number is reported rather than smoothing it over.
//!
//! DESIGN §20 adds one more stage AHEAD of the percentile: **BC singularity
//! exclusion** ([`bc_exclusion`], folded into [`criterion_mask`]). A perfectly
//! rigid support is a modelling fiction whose corner stress does not converge
//! under refinement, so a search that hammers on SF_crit (the §20 settings
//! optimizer) would spend its entire budget feeding an artifact. Cells within a
//! PHYSICAL, patch-scaled radius of a rigid constraint leave the scalar; the
//! displayed SF field is untouched (the clamp hotspot stays visible — the
//! criterion just stops pretending it is the part's weakest point).

use crate::fem::{NODE_OFFSETS, NODE_SIGNS};
use crate::voxel::VoxelGrid;

/// Safety-factor display/criterion cap (matches the wasm display's SF_CAP).
pub const SF_CAP: f64 = 10.0;

/// Cell-size multiple over which a stress riser's peak is considered resolved.
/// Not a filter — a REPORTING threshold: when the binding cell's neighborhood
/// climbs faster than this, the number is riding a notch tip and will keep
/// falling as the mesh is refined, which the app must say out loud (§17 dec. 4,
/// 2026-07-25). See [`riser_ratio`].
pub const RISER_NEIGHBORHOOD: usize = 2;

/// Solid-topology mode: design cells whose binned density is below this are
/// ersatz void (the optimizer's lower bound is 1e-3) — masked out of the
/// criterion so void stress cannot pollute it (§17 dec. 7).
pub const SOLID_VOID_MASK: f64 = 2e-3;

/// BC singularity exclusion (DESIGN §20 dec. 5): the exclusion radius around a
/// constraint patch is this fraction of the patch's characteristic diameter.
/// Saint-Venant — the pollution a perfectly rigid interface injects decays over
/// the patch's own length scale, so the radius must scale with the PATCH, not
/// with the mesh. Lives in code, deliberately not in the UI; tune against test
/// parts.
pub const BC_EXCL_PATCH_FRAC: f64 = 0.15;

/// Floor on the exclusion radius, in cell sizes — a small patch on a coarse
/// mesh still has one ring of cells whose stress is pure penalty artifact.
pub const BC_EXCL_MIN_CELLS: f64 = 2.0;

/// Which safety factor the goal enforces (§17 dec. 2, per-project toggle).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SfMeasure {
    /// In-plane material failure: von Mises vs tensile strength.
    Material,
    /// Layer adhesion: the §15 tension+shear interaction across the layers.
    Layer,
    /// Worst of both per cell (the display's `sf` field) — the default.
    Both,
}

impl SfMeasure {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "material" => Self::Material,
            "layer" => Self::Layer,
            "both" => Self::Both,
            _ => return None,
        })
    }
}

/// Solid-material allowables + the measure to enforce. Strengths in MPa;
/// `shear_z` is the EFFECTIVE interlayer shear strength (the caller resolves
/// the measured-or-0.6·Sᵗᶻ default).
#[derive(Clone, Copy, Debug)]
pub struct StrengthSpec {
    pub measure: SfMeasure,
    /// In-plane tensile strength Sᵗ (von Mises allowable).
    pub strength: f64,
    /// Layer-adhesion (cross-layer tension) strength Sᵗᶻ.
    pub strength_z: f64,
    /// Interlayer shear strength Sˢᶻ (effective).
    pub shear_z: f64,
}

/// Per-cell safety factor for the given measure, capped at [`SF_CAP`]; 0.0
/// for cells with `eps <= 0` or masked out. Strains at cell centers (the same
/// stencil as `stress.rs`), stress from the CELL's effective modulus
/// `E = e0·eps`, allowables scaled by the SAME `eps` factor — the factor
/// cancels in allowable/stress, so this equals the display's material-stress
/// SF regardless of the occupancy-decoupling toggle.
pub fn sf_cells(
    grid: &VoxelGrid,
    u: &[f32],
    e0: f64,
    nu: f64,
    eps: &[f32],
    mask: &[bool],
    spec: &StrengthSpec,
) -> Vec<f32> {
    sf_cells_ti(grid, u, e0, nu, eps, None, mask, spec)
}

/// [`sf_cells`] with a transverse-isotropic infill share (DESIGN §22).
///
/// Two things change together and must not be split: the STRESS comes from the
/// blended tensor (as in `stress::cell_field_ti`), and the ALLOWABLE scales by
/// the TOTAL material factor `fs + fi` — the whole material share of the cell,
/// which is what the old single `eps` meant when both shares were isotropic.
/// Scaling the allowable by `fs` alone would collapse the safety factor of
/// every pure-infill cell to zero.
///
/// Note this keeps §22.6's known limitation: allowables still scale linearly
/// with the stiffness factor, so strength exponent = stiffness exponent. TI
/// does not fix that; it only stops the STRESS side from being wrong too.
#[allow(clippy::too_many_arguments)]
pub fn sf_cells_ti(
    grid: &VoxelGrid,
    u: &[f32],
    e0: f64,
    nu: f64,
    eps: &[f32],
    ti: Option<(&[f32], &crate::ti::TiRatios)>,
    mask: &[bool],
    spec: &StrengthSpec,
) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let inv4h = 1.0 / (4.0 * grid.h);
    let mut out = vec![0f32; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                let fi = ti.map_or(0.0, |(f, _)| f[ci] as f64);
                if (eps[ci] <= 0.0 && fi <= 0.0) || !mask[ci] {
                    continue;
                }
                // Strain at the cell center.
                let (mut exx, mut eyy, mut ezz) = (0f64, 0f64, 0f64);
                let (mut gxy, mut gyz, mut gzx) = (0f64, 0f64, 0f64);
                for l in 0..8 {
                    let [ox, oy, oz] = NODE_OFFSETS[l];
                    let [sx, sy, sz] = NODE_SIGNS[l];
                    let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
                    let (ux, uy, uz) =
                        (u[3 * n] as f64, u[3 * n + 1] as f64, u[3 * n + 2] as f64);
                    exx += sx * ux;
                    eyy += sy * uy;
                    ezz += sz * uz;
                    gxy += sy * ux + sx * uy;
                    gyz += sz * uy + sy * uz;
                    gzx += sx * uz + sz * ux;
                }
                let (exx, eyy, ezz) = (exx * inv4h, eyy * inv4h, ezz * inv4h);
                let (gxy, gyz, gzx) = (gxy * inv4h, gyz * inv4h, gzx * inv4h);
                // Allowable scales with the TOTAL material share; stress comes
                // from the blended tensor.
                let factor = eps[ci] as f64 + fi;
                let (sxx, syy, szz, sxy, syz, szx) = match ti {
                    Some((_, ratios)) => {
                        let s = crate::ti::blended_stress(
                            e0,
                            nu,
                            eps[ci] as f64,
                            fi,
                            ratios,
                            [exx, eyy, ezz, gxy, gyz, gzx],
                        );
                        (s[0], s[1], s[2], s[3], s[4], s[5])
                    }
                    None => {
                        let e = e0 * factor;
                        let lam = e * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
                        let mu = e / (2.0 * (1.0 + nu));
                        let tr = exx + eyy + ezz;
                        (
                            lam * tr + 2.0 * mu * exx,
                            lam * tr + 2.0 * mu * eyy,
                            lam * tr + 2.0 * mu * ezz,
                            mu * gxy,
                            mu * gyz,
                            mu * gzx,
                        )
                    }
                };

                let sf_m = || -> f64 {
                    let vm = (0.5
                        * ((sxx - syy).powi(2) + (syy - szz).powi(2) + (szz - sxx).powi(2))
                        + 3.0 * (sxy * sxy + syz * syz + szx * szx))
                        .sqrt();
                    ((spec.strength * factor) / vm.max(1e-9)).min(SF_CAP)
                };
                let sf_z = || -> f64 {
                    let qt = szz.max(0.0) / (spec.strength_z * factor).max(1e-9);
                    let qs2 = (syz * syz + szx * szx)
                        / (spec.shear_z * factor).max(1e-9).powi(2);
                    let q = qt * qt + qs2;
                    if q <= 1e-18 { SF_CAP } else { (1.0 / q.sqrt()).min(SF_CAP) }
                };
                let sf = match spec.measure {
                    SfMeasure::Material => sf_m(),
                    SfMeasure::Layer => sf_z(),
                    SfMeasure::Both => sf_m().min(sf_z()),
                };
                out[ci] = sf as f32;
            }
        }
    }
    out
}

/// Criterion mask: solid cells minus (solid-mode) ersatz-void design cells,
/// minus the BC singularity zone. `x` is the design density per `design_cells`
/// slot (the binned field); `bc_excl` is [`bc_exclusion`]'s per-cell verdict
/// (empty ⇒ no exclusion, the pre-§20 behavior).
pub fn criterion_mask(
    grid: &VoxelGrid,
    design_cells: &[u32],
    x: &[f64],
    solid_mode: bool,
    bc_excl: &[bool],
) -> Vec<bool> {
    let mut mask: Vec<bool> = grid.scale.iter().map(|&sc| sc > 0.0).collect();
    if solid_mode {
        for (k, &c) in design_cells.iter().enumerate() {
            if x[k] < SOLID_VOID_MASK {
                mask[c as usize] = false;
            }
        }
    }
    if !bc_excl.is_empty() {
        debug_assert_eq!(bc_excl.len(), mask.len());
        for (m, &e) in mask.iter_mut().zip(bc_excl) {
            if e {
                *m = false;
            }
        }
    }
    mask
}

/// Exclusion radius (mm) of one constraint patch (DESIGN §20 dec. 5):
/// `max(BC_EXCL_MIN_CELLS · h, BC_EXCL_PATCH_FRAC · d_c)` where `d_c` is the
/// patch's characteristic diameter — the diameter of the disc with the same
/// area, taken as `node_count · h²` (attached nodes tile the selection at the
/// node pitch, so the AREA is mesh-stable even though the count is not).
pub fn bc_exclusion_radius(grid: &VoxelGrid, patch_nodes: usize) -> f64 {
    let area = patch_nodes as f64 * grid.h * grid.h;
    let d_c = 2.0 * (area / std::f64::consts::PI).sqrt();
    (BC_EXCL_PATCH_FRAC * d_c).max(BC_EXCL_MIN_CELLS * grid.h)
}

/// **BC singularity exclusion** (DESIGN §20 dec. 5/6): cells whose center lies
/// within [`bc_exclusion_radius`] of any node of a rigid constraint patch —
/// fixed/displacement supports and §16 rigid mounts, the artificial
/// infinite-stiffness interfaces. Force pads are NOT passed here: an
/// under-sized load introduction is a real failure mode the criterion must keep
/// seeing (dec. 6).
///
/// Each `patches` entry is one BC's attached node list (padded NODE grid ids,
/// `n = (z·my + y)·mx + x`); each gets its OWN radius, so a small tab and a
/// large clamped face are treated at their own scales.
///
/// The distance is PHYSICAL (mm), which is what makes the criterion
/// mesh-stable: refining the mesh moves the same physical shell of cells out of
/// the percentile, whereas a fixed ring of CELLS walks into the singularity as
/// h shrinks and drags SF_crit down with it (the §17 dec. 4 disease). Computed
/// as an exact Euclidean distance transform from the constraint nodes,
/// evaluated at cell centers — separable, O(cells) per patch.
pub fn bc_exclusion(grid: &VoxelGrid, patches: &[&[u32]]) -> Vec<bool> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let mut out = vec![false; nx * ny * nz];
    for patch in patches {
        if patch.is_empty() {
            continue;
        }
        let r_cells = bc_exclusion_radius(grid, patch.len()) / grid.h;
        let r2 = r_cells * r_cells;
        // Seeds on the NODE lattice; the transform's half-cell shift moves the
        // sample points to cell centers, so the result is the true
        // center-to-node distance (no staircase bias from seeding cells).
        let mut f = vec![DT_INF; mx * my * mz];
        for &n in *patch {
            let n = n as usize;
            if n < f.len() {
                f[n] = 0.0;
            }
        }
        // x: (mx,my,mz) → (nx,my,mz); then y → (nx,ny,mz); then z → (nx,ny,nz).
        let fx = dt_axis(&f, [mx, my, mz], 0, nx);
        let fy = dt_axis(&fx, [nx, my, mz], 1, ny);
        let fz = dt_axis(&fy, [nx, ny, mz], 2, nz);
        for (o, &d2) in out.iter_mut().zip(&fz) {
            if d2 <= r2 {
                *o = true;
            }
        }
    }
    out
}

/// Sentinel "no seed" cost. Finite (not `f64::INFINITY`) on purpose: the
/// envelope's intersection formula subtracts two of these, and `INF − INF`
/// must be 0, not NaN.
const DT_INF: f64 = 1e20;

/// One axis of the separable squared-distance transform, in lattice units.
/// `dims` is the input extent; the output replaces axis `axis` with `n_out`
/// samples taken at `p + 0.5` (input lattice = grid NODES, output lattice =
/// cell CENTERS). `d[q] = min_p (f[p] + (q + 0.5 − p)²)`.
fn dt_axis(f: &[f64], dims: [usize; 3], axis: usize, n_out: usize) -> Vec<f64> {
    let n_in = dims[axis];
    let mut odims = dims;
    odims[axis] = n_out;
    let mut out = vec![0f64; odims[0] * odims[1] * odims[2]];
    let stride = match axis {
        0 => 1,
        1 => dims[0],
        _ => dims[0] * dims[1],
    };
    let ostride = match axis {
        0 => 1,
        1 => odims[0],
        _ => odims[0] * odims[1],
    };
    // Lower-envelope scratch (Felzenszwalb & Huttenlocher).
    let mut v = vec![0usize; n_in];
    let mut z = vec![0f64; n_in + 1];
    let mut line = vec![0f64; n_in];
    // Iterate every line along `axis`.
    let (a1, a2) = match axis {
        0 => (1usize, 2usize),
        1 => (0, 2),
        _ => (0, 1),
    };
    for j in 0..dims[a2] {
        for i in 0..dims[a1] {
            let base = match axis {
                0 => (j * dims[1] + i) * dims[0],
                1 => j * dims[0] * dims[1] + i,
                _ => i + j * dims[0],
            };
            let obase = match axis {
                0 => (j * odims[1] + i) * odims[0],
                1 => j * odims[0] * odims[1] + i,
                _ => i + j * odims[0],
            };
            for p in 0..n_in {
                line[p] = f[base + p * stride];
            }
            // Build the lower envelope of the parabolas f[p] + (x − p)².
            let mut k = 0usize;
            v[0] = 0;
            z[0] = -DT_INF;
            z[1] = DT_INF;
            for q in 1..n_in {
                let mut s;
                loop {
                    let vk = v[k] as f64;
                    let qf = q as f64;
                    s = ((line[q] + qf * qf) - (line[v[k]] + vk * vk)) / (2.0 * qf - 2.0 * vk);
                    if s > z[k] {
                        break;
                    }
                    k -= 1; // z[0] = −DT_INF terminates this
                }
                k += 1;
                v[k] = q;
                z[k] = s;
                z[k + 1] = DT_INF;
            }
            // Evaluate at the shifted (cell-center) sample points.
            let mut k = 0usize;
            for q in 0..n_out {
                let x = q as f64 + 0.5;
                while z[k + 1] < x {
                    k += 1;
                }
                let dx = x - v[k] as f64;
                out[obase + q * ostride] = dx * dx + line[v[k]];
            }
        }
    }
    out
}

/// Volume-averaged nodal recovery + re-interpolation to cell centers of a
/// per-cell field, honoring `mask`: masked-out cells neither contribute to
/// nor receive values (their slot stays 0). This is one diffusion step at the
/// staircase length scale — single-cell spikes melt into their 26-cell
/// neighborhood while real gradients pass through.
pub fn smooth_masked(grid: &VoxelGrid, cells: &[f32], mask: &[bool]) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let mut sum = vec![0f32; mx * my * mz];
    let mut count = vec![0u16; mx * my * mz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if grid.scale[ci] <= 0.0 || !mask[ci] {
                    continue;
                }
                let v = cells[ci];
                for oz in 0..2 {
                    for oy in 0..2 {
                        for ox in 0..2 {
                            let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
                            sum[n] += v;
                            count[n] += 1;
                        }
                    }
                }
            }
        }
    }
    let mut out = vec![0f32; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if grid.scale[ci] <= 0.0 || !mask[ci] {
                    continue;
                }
                let (mut s, mut c) = (0f64, 0u32);
                for oz in 0..2 {
                    for oy in 0..2 {
                        for ox in 0..2 {
                            let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
                            if count[n] > 0 {
                                s += (sum[n] / count[n] as f32) as f64;
                                c += 1;
                            }
                        }
                    }
                }
                // A masked-in cell always has ≥1 contributing node (itself).
                out[ci] = (s / c.max(1) as f64) as f32;
            }
        }
    }
    out
}

/// **SF_crit** (§17 dec. 4 as of 2026-07-25): the lowest smoothed safety factor
/// among the cells the criterion scores — the minimum of exactly the field the
/// viewport plots as `sfx`/`sfmx`/`sfzx`. Empty mask ⇒ [`SF_CAP`].
///
/// No trimming, no percentile: whatever the number is, the user can point at
/// the cell it came from. What the criterion still leaves out it leaves out
/// VISIBLY, through [`criterion_mask`] (void, ersatz-void, BC singularity
/// zones) — the plot greys those cells.
pub fn sf_min(grid: &VoxelGrid, sf: &[f32], mask: &[bool]) -> f64 {
    sf_percentile(grid, sf, mask, 0.0)
}

/// The volume-weighted trimmed percentile: the SF value such that masked cells
/// totaling ≤ `trim_frac` of the total masked volume lie strictly below it.
/// Weights are geometric occupancy (`grid.scale`). `trim_frac = 0` is the plain
/// minimum, which is what [`sf_min`] — and therefore SF_crit — uses.
///
/// Retained for diagnostics and drift anchors; **not** the criterion. See the
/// module docs for why the trim was retired.
pub fn sf_percentile(grid: &VoxelGrid, sf: &[f32], mask: &[bool], trim_frac: f64) -> f64 {
    let mut cells: Vec<(f32, f32)> = Vec::new();
    let mut total = 0f64;
    for (ci, &m) in mask.iter().enumerate() {
        if m && grid.scale[ci] > 0.0 {
            cells.push((sf[ci], grid.scale[ci]));
            total += grid.scale[ci] as f64;
        }
    }
    if cells.is_empty() {
        return SF_CAP;
    }
    cells.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let budget = trim_frac * total;
    let mut cum = 0f64;
    for &(v, w) in &cells {
        cum += w as f64;
        if cum > budget {
            return v as f64;
        }
    }
    cells.last().unwrap().0 as f64
}

/// How sharply the field climbs away from the binding cell: the occupancy-
/// weighted mean smoothed SF in a ±[`RISER_NEIGHBORHOOD`] cell box around the
/// minimum, divided by the minimum itself. Returns `(binding cell, ratio)`.
///
/// This is the honest replacement for the retired trim. The trim silently
/// discarded sharp peaks; this measures one and says so. ~1 means the weakest
/// material is a broad region and SF_crit is a converged property of the part;
/// well above 1 means the number is sitting on a notch tip, where the peak
/// stress — and therefore SF_crit — keeps rising with mesh refinement. The
/// caller reports it; nothing is filtered on it.
pub fn riser_ratio(grid: &VoxelGrid, sf: &[f32], mask: &[bool]) -> Option<(usize, f64)> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let mut binding: Option<(usize, f32)> = None;
    for ci in 0..nx * ny * nz {
        if !mask[ci] || grid.scale[ci] <= 0.0 {
            continue;
        }
        if binding.is_none_or(|(_, v)| sf[ci] < v) {
            binding = Some((ci, sf[ci]));
        }
    }
    let (ci, v0) = binding?;
    if !(v0 > 1e-6) {
        return None;
    }
    let (cz, rem) = (ci / (nx * ny), ci % (nx * ny));
    let (cy, cx) = (rem / nx, rem % nx);
    let r = RISER_NEIGHBORHOOD as isize;
    let (mut sum, mut wsum) = (0f64, 0f64);
    for dz in -r..=r {
        for dy in -r..=r {
            for dx in -r..=r {
                let (x, y, z) = (cx as isize + dx, cy as isize + dy, cz as isize + dz);
                if x < 0 || y < 0 || z < 0 {
                    continue;
                }
                let (x, y, z) = (x as usize, y as usize, z as usize);
                if x >= nx || y >= ny || z >= nz {
                    continue;
                }
                let n = (z * ny + y) * nx + x;
                if !mask[n] || grid.scale[n] <= 0.0 {
                    continue;
                }
                sum += sf[n] as f64 * grid.scale[n] as f64;
                wsum += grid.scale[n] as f64;
            }
        }
    }
    if wsum <= 0.0 {
        return None;
    }
    Some((ci, (sum / wsum) / v0 as f64))
}

/// Cells (padded-grid ids) whose smoothed SF lies below `threshold` — the
/// binding region behind SF_crit / the infeasibility diagnosis (§17 dec. 6).
pub fn binding_cells(sf: &[f32], mask: &[bool], threshold: f64) -> Vec<u32> {
    mask.iter()
        .enumerate()
        .filter(|&(ci, &m)| m && (sf[ci] as f64) < threshold)
        .map(|(ci, _)| ci as u32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linear displacement field u_x = a·X on a solid box grid (uniform
    /// ε_xx = a everywhere) — the σ_xx state is exact and uniform.
    fn uniaxial_u(grid: &VoxelGrid, a: f32) -> Vec<f32> {
        let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
        let mut u = vec![0f32; 3 * mx * my * mz];
        for nz in 0..mz {
            for ny in 0..my {
                for nx in 0..mx {
                    let n = (nz * my + ny) * mx + nx;
                    u[3 * n] = a * (nx as f32 * grid.h as f32);
                }
            }
        }
        u
    }

    fn all_mask(grid: &VoxelGrid) -> Vec<bool> {
        grid.scale.iter().map(|&s| s > 0.0).collect()
    }

    /// Uniaxial tension, ν = 0: σ_xx = E·a is the only stress. Material SF is
    /// the hand calc S/σ; the layer measure sees no σzz/interlayer shear ⇒ cap.
    #[test]
    fn material_sf_matches_hand_calc_and_layer_caps() {
        let grid = VoxelGrid::solid_box(4, 2, 2, 1.0);
        let eps = grid.scale.clone();
        let u = uniaxial_u(&grid, 0.01);
        let (e0, nu) = (1000.0, 0.0); // σ_xx = 10 MPa
        let mask = all_mask(&grid);
        let spec = StrengthSpec {
            measure: SfMeasure::Both,
            strength: 50.0,
            strength_z: 35.0,
            shear_z: 21.0,
        };
        let both = sf_cells(&grid, &u, e0, nu, &eps, &mask, &spec);
        // von Mises of uniaxial 10 MPa is 10 ⇒ SF = 5. Layer sees nothing.
        assert!((both[0] - 5.0).abs() < 1e-4, "sf {}", both[0]);
        let layer = sf_cells(
            &grid, &u, e0, nu, &eps, &mask,
            &StrengthSpec { measure: SfMeasure::Layer, ..spec },
        );
        assert!((layer[0] - SF_CAP as f32).abs() < 1e-6, "layer sf {}", layer[0]);
    }

    /// Graded allowable: the SAME strain state at half stiffness must yield
    /// the SAME safety factor (stress and allowable both scale by eps — the
    /// Gibson–Ashby factor cancels, exactly like the display).
    #[test]
    fn gibson_ashby_factor_cancels() {
        let grid = VoxelGrid::solid_box(2, 2, 2, 1.0);
        let u = uniaxial_u(&grid, 0.01);
        let mask = all_mask(&grid);
        let spec = StrengthSpec {
            measure: SfMeasure::Material,
            strength: 50.0,
            strength_z: 35.0,
            shear_z: 21.0,
        };
        let full: Vec<f32> = grid.scale.clone();
        let half: Vec<f32> = grid.scale.iter().map(|&s| s * 0.5).collect();
        let a = sf_cells(&grid, &u, 1000.0, 0.3, &full, &mask, &spec);
        let b = sf_cells(&grid, &u, 1000.0, 0.3, &half, &mask, &spec);
        assert!((a[0] - b[0]).abs() < 1e-4, "factor must cancel: {} vs {}", a[0], b[0]);
    }

    /// Smoothing melts a single-cell spike into its neighborhood; the far
    /// field stays put. Masked-out cells must not contribute (a cap-valued
    /// void neighbor would otherwise inflate the smoothed result).
    #[test]
    fn smoothing_melts_spikes_and_respects_mask() {
        let grid = VoxelGrid::solid_box(5, 1, 1, 1.0);
        let mut sf = vec![5.0f32; 5];
        sf[2] = 0.5; // spike
        let mask = vec![true; 5];
        let sm = smooth_masked(&grid, &sf, &mask);
        // Spike cell: nodes average {5, 0.5} pairs → well above the raw 0.5.
        assert!(sm[2] > 2.0 && sm[2] < 5.0, "spike melted: {}", sm[2]);
        // End cell: untouched by the spike (no shared node) → exactly 5.
        assert!((sm[0] - 5.0).abs() < 1e-5, "far field moved: {}", sm[0]);

        // Mask out a cap-valued cell next to a low one: the low cell must not
        // be pulled UP by the masked neighbor.
        let sf2 = vec![10.0f32, 1.0, 10.0, 10.0, 10.0];
        let mut mask2 = vec![true; 5];
        mask2[0] = false;
        let sm2 = smooth_masked(&grid, &sf2, &mask2);
        let with_mask = sm2[1];
        let sm3 = smooth_masked(&grid, &sf2, &vec![true; 5]);
        assert!(
            with_mask < sm3[1],
            "masked cap neighbor must not inflate: {} vs {}",
            with_mask,
            sm3[1]
        );
    }

    /// SF_crit hides nothing: one bad cell IS the answer, however small its
    /// volume, and however many good cells surround it. (Until 2026-07-25 a
    /// 0.2 %-volume trim swallowed exactly this case — the reported number sat
    /// above the plotted minimum and users could not reconcile the two.)
    #[test]
    fn sf_min_reports_the_single_worst_cell() {
        let mut grid = VoxelGrid::solid_box(1000, 1, 1, 1.0);
        let mut sf = vec![5.0f32; 1000];
        sf[0] = 0.5; // 0.1 % of the volume — the OLD trim ignored it
        let mask = vec![true; 1000];
        assert!((sf_min(&grid, &sf, &mask) - 0.5).abs() < 1e-6);
        // Nor does a sliver hide: a 1 %-occupancy cell binds like any other.
        sf[7] = 0.25;
        grid.scale[7] = 0.01;
        assert!((sf_min(&grid, &sf, &mask) - 0.25).abs() < 1e-6);
        // Masked cells still do not count — that exclusion is VISIBLE (greyed).
        let mut mask2 = mask.clone();
        mask2[7] = false;
        assert!((sf_min(&grid, &sf, &mask2) - 0.5).abs() < 1e-6);
        // The retired percentile is still available for diagnostics.
        assert!(sf_percentile(&grid, &sf, &mask, 0.002) > 0.5);
    }

    /// The riser diagnostic separates "one sharp notch tip" from "a broad
    /// weak region" — the honest replacement for the trim: instead of hiding
    /// the peak it measures how peaked the field is and lets the caller say so.
    #[test]
    fn riser_ratio_flags_a_lone_spike_but_not_a_plateau() {
        let grid = VoxelGrid::solid_box(9, 1, 1, 1.0);
        let mask = vec![true; 9];
        // Spike: one low cell in an otherwise uniform field.
        let mut spike = vec![5.0f32; 9];
        spike[4] = 1.0;
        let (ci, r) = riser_ratio(&grid, &spike, &mask).expect("spike");
        assert_eq!(ci, 4, "the binding cell is the spike");
        assert!(r > 2.0, "a lone spike reads as a riser: {r}");
        // Plateau: the whole neighborhood is equally weak.
        let plateau = vec![1.0f32; 9];
        let (_, r2) = riser_ratio(&grid, &plateau, &mask).expect("plateau");
        assert!((r2 - 1.0).abs() < 1e-6, "a broad weak region is not a riser: {r2}");
    }

    /// Every node of the x = 0 face of a solid box (the classic clamped end).
    fn face_nodes(grid: &VoxelGrid) -> Vec<u32> {
        let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
        let mut v = Vec::new();
        for z in 0..mz {
            for y in 0..my {
                v.push(((z * my + y) * mx) as u32);
            }
        }
        v
    }

    /// The transform must return the TRUE distance from each cell center to the
    /// nearest seed node — checked against brute force on a small grid.
    #[test]
    fn exclusion_distance_matches_brute_force() {
        let grid = VoxelGrid::solid_box(7, 5, 3, 0.5);
        let (mx, my) = (grid.nx + 1, grid.ny + 1);
        let seeds: Vec<u32> = vec![0, ((2 * my + 3) * mx + 5) as u32];
        // Radius big enough to sweep a range of true distances.
        let r = bc_exclusion_radius(&grid, seeds.len());
        let got = bc_exclusion(&grid, &[&seeds]);
        for cz in 0..grid.nz {
            for cy in 0..grid.ny {
                for cx in 0..grid.nx {
                    let c = [
                        (cx as f64 + 0.5) * grid.h,
                        (cy as f64 + 0.5) * grid.h,
                        (cz as f64 + 0.5) * grid.h,
                    ];
                    let mut best = f64::INFINITY;
                    for &n in &seeds {
                        let n = n as usize;
                        let p = [
                            (n % mx) as f64 * grid.h,
                            ((n / mx) % my) as f64 * grid.h,
                            (n / (mx * my)) as f64 * grid.h,
                        ];
                        let d = ((c[0] - p[0]).powi(2) + (c[1] - p[1]).powi(2) + (c[2] - p[2]).powi(2)).sqrt();
                        best = best.min(d);
                    }
                    let ci = (cz * grid.ny + cy) * grid.nx + cx;
                    // Away from the radius the verdict must be unambiguous.
                    if (best - r).abs() > 1e-9 {
                        assert_eq!(got[ci], best <= r, "cell {cx},{cy},{cz}: d {best}, r {r}");
                    }
                }
            }
        }
    }

    /// The §20 dec. 5 property: the excluded PHYSICAL volume converges under
    /// refinement. The same 8 × 8 mm clamped face at h and h/2 must exclude the
    /// same share of the part — a fixed ring of CELLS would halve it.
    #[test]
    fn exclusion_is_mesh_stable() {
        let share = |n: usize, h: f64| -> f64 {
            let grid = VoxelGrid::solid_box(4 * n, n, n, h);
            let excl = bc_exclusion(&grid, &[&face_nodes(&grid)]);
            excl.iter().filter(|&&e| e).count() as f64 / excl.len() as f64
        };
        // 32 × 8 × 8 mm at h = 0.5 and h = 0.25 — both well clear of the
        // 2-cell floor, so this measures the PHYSICAL radius alone.
        let coarse = share(16, 0.5);
        let fine = share(32, 0.25);
        assert!(
            (coarse - fine).abs() / coarse < 0.08,
            "excluded volume share must be mesh-stable: {coarse} vs {fine}"
        );
        // …and the radius itself is a physical constant of the patch.
        let g_c = VoxelGrid::solid_box(64, 16, 16, 0.5);
        let g_f = VoxelGrid::solid_box(128, 32, 32, 0.25);
        let r_c = bc_exclusion_radius(&g_c, face_nodes(&g_c).len());
        let r_f = bc_exclusion_radius(&g_f, face_nodes(&g_f).len());
        assert!((r_c - r_f).abs() / r_c < 0.08, "radius drifted: {r_c} vs {r_f}");
    }

    /// A tiny patch on a coarse mesh still clears its own penalty ring.
    #[test]
    fn exclusion_has_a_two_cell_floor() {
        let grid = VoxelGrid::solid_box(9, 9, 9, 1.0);
        let (mx, my) = (grid.nx + 1, grid.ny + 1);
        let center = ((4 * my + 4) * mx + 4) as u32;
        assert!(
            (bc_exclusion_radius(&grid, 1) - BC_EXCL_MIN_CELLS * grid.h).abs() < 1e-12,
            "single-node patch takes the floor radius"
        );
        let excl = bc_exclusion(&grid, &[&[center]]);
        // Sphere of radius 2h around one node: ≥ the 8 cells sharing it.
        assert!(excl.iter().filter(|&&e| e).count() >= 8, "floor radius covers the node's cells");
    }

    /// The mask drops excluded cells, and an empty exclusion is a no-op (the
    /// pre-§20 path stays byte-identical).
    #[test]
    fn criterion_mask_applies_exclusion() {
        let grid = VoxelGrid::solid_box(4, 1, 1, 1.0);
        let plain = criterion_mask(&grid, &[], &[], false, &[]);
        assert_eq!(plain, vec![true; 4]);
        let excl = vec![false, true, false, false];
        let masked = criterion_mask(&grid, &[], &[], false, &excl);
        assert_eq!(masked, vec![true, false, true, true]);
        // …and it composes with the solid-mode ersatz-void mask.
        let both = criterion_mask(&grid, &[2u32], &[1e-3], true, &excl);
        assert_eq!(both, vec![true, false, false, true]);
    }

    #[test]
    fn binding_cells_lists_below_threshold() {
        let sf = vec![5.0f32, 1.2, 0.8, 3.0];
        let mut mask = vec![true; 4];
        mask[3] = false;
        let b = binding_cells(&sf, &mask, 1.5);
        assert_eq!(b, vec![1, 2]);
    }
}
