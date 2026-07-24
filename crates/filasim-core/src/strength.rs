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
//! 3. **Volume-weighted trimmed percentile** ([`sf_percentile`]): SF_crit is
//!    the SF value such that cells totaling ≤ [`SF_TRIM_FRAC`] of the solid
//!    volume lie below it. Volume weighting (not cell count) keeps the number
//!    stable across mesh resolutions; the trim drops what smoothing couldn't.
//!
//! Known residual risk (accepted in the §17 interview): a REAL hotspot smaller
//! than the trim volume can hide in the trimmed tail — the binding-cells view
//! ([`binding_cells`]) is the safety net.

use crate::fem::{NODE_OFFSETS, NODE_SIGNS};
use crate::voxel::VoxelGrid;

/// Safety-factor display/criterion cap (matches the wasm display's SF_CAP).
pub const SF_CAP: f64 = 10.0;

/// Volume fraction of the solid part that may lie below SF_crit — the notch
/// trim (§17 dec. 4). Lives in code, deliberately NOT exposed in the UI;
/// tune against test parts if the default proves too blunt/too sharp.
pub const SF_TRIM_FRAC: f64 = 0.002;

/// Solid-topology mode: design cells whose binned density is below this are
/// ersatz void (the optimizer's lower bound is 1e-3) — masked out of the
/// criterion so void stress cannot pollute it (§17 dec. 7).
pub const SOLID_VOID_MASK: f64 = 2e-3;

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
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let inv4h = 1.0 / (4.0 * grid.h);
    let mut out = vec![0f32; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if eps[ci] <= 0.0 || !mask[ci] {
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
                let factor = eps[ci] as f64;
                let e = e0 * factor;
                let lam = e * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
                let mu = e / (2.0 * (1.0 + nu));
                let tr = exx + eyy + ezz;
                let (sxx, syy, szz) =
                    (lam * tr + 2.0 * mu * exx, lam * tr + 2.0 * mu * eyy, lam * tr + 2.0 * mu * ezz);
                let (sxy, syz, szx) = (mu * gxy, mu * gyz, mu * gzx);

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

/// Criterion mask: solid cells minus (solid-mode) ersatz-void design cells.
/// `x` is the design density per `design_cells` slot (the binned field).
pub fn criterion_mask(
    grid: &VoxelGrid,
    design_cells: &[u32],
    x: &[f64],
    solid_mode: bool,
) -> Vec<bool> {
    let mut mask: Vec<bool> = grid.scale.iter().map(|&sc| sc > 0.0).collect();
    if solid_mode {
        for (k, &c) in design_cells.iter().enumerate() {
            if x[k] < SOLID_VOID_MASK {
                mask[c as usize] = false;
            }
        }
    }
    mask
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

/// **SF_crit**: the volume-weighted trimmed minimum (§17 dec. 4) — the SF
/// value such that masked cells totaling ≤ `trim_frac` of the total masked
/// volume lie strictly below it. Weights are the cells' geometric occupancy
/// (`grid.scale`), so the number is a property of the PART, not the mesh.
/// Empty mask ⇒ [`SF_CAP`] (nothing to check).
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

/// Cells (padded-grid ids) whose smoothed SF lies below `threshold` — the
/// binding region behind SF_crit / the infeasibility diagnosis (§17 dec. 6),
/// and the safety net for hotspots smaller than the trim volume.
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

    /// The trimmed percentile ignores outliers up to the trim volume and no
    /// further; volume weighting (occupancy) decides, not cell count.
    #[test]
    fn percentile_trims_by_volume() {
        let mut grid = VoxelGrid::solid_box(1000, 1, 1, 1.0);
        let mut sf = vec![5.0f32; 1000];
        sf[0] = 0.5; // one full-volume outlier = 0.1% of volume < 0.2% trim
        let mask = vec![true; 1000];
        let crit = sf_percentile(&grid, &sf, &mask, SF_TRIM_FRAC);
        assert!((crit - 5.0).abs() < 1e-6, "outlier trimmed: {crit}");

        // Three outliers = 0.3% > 0.2%: the third must bind.
        sf[1] = 0.6;
        sf[2] = 0.7;
        let crit = sf_percentile(&grid, &sf, &mask, SF_TRIM_FRAC);
        assert!((crit - 0.7).abs() < 1e-6, "third outlier binds: {crit}");

        // Volume weighting: shrink the outlier cells' occupancy so all three
        // together stay under the trim volume — the criterion returns to 5.
        for c in 0..3 {
            grid.scale[c] = 0.5;
        }
        let crit = sf_percentile(&grid, &sf, &mask, SF_TRIM_FRAC);
        assert!((crit - 5.0).abs() < 1e-6, "volume-weighted trim: {crit}");
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
