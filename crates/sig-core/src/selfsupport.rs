// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Self-supporting (additive-manufacturing) density filter for the SOLID
//! topology-optimization mode (DESIGN.md decision #15).
//!
//! A Langelaar-style layer-by-layer projection: the *printed* density of a
//! cell can be no larger than what the cells beneath it (within the overhang
//! cone) can support, so the optimized shape overhangs at most the chosen
//! angle from the build plate and prints WITHOUT supports.
//!
//! Build direction is global **+Z** — the part is already oriented Z-up (the
//! layer-adhesion safety factor reads σzz against the same axis), so this
//! filter introduces no new convention.
//!
//! Printed density (processing layers from the plate up):
//!   ξ_e = smin( ρ_e , smax_{s ∈ support(e)} ξ_s )
//! with
//!   smin(a,b) = ½(a + b − √((a−b)² + ε))            (smooth minimum)
//!   smax(s)   = (Σ_s ξ_s^P)^{1/P}                    (P-norm soft maximum)
//! Cells on the plate layer and cells with a fully-solid (frozen) supporter
//! beneath them are unconditionally supported (S ≡ 1 ⇒ ξ = ρ). A cell with no
//! supporter in the cone and not on the plate gets S = 0 ⇒ ξ ≈ 0 (removed).
//!
//! The forward projection and its transpose (chain-rule reverse layer sweep,
//! recomputed from the filtered field — stateless) plug into the optimizer's
//! OC sensitivity flow exactly like the linear density filter. The filter is
//! **advisory**: it makes the optimizer's field printable, but the voxel
//! staircase + region smoothing on the exported mesh can still nick the angle.

use crate::voxel::VoxelGrid;

/// Smoothing of the smooth-minimum (mm² in density units). Small ⇒ closer to a
/// hard min; large ⇒ smoother gradient. Langelaar's "Q" analogue.
const EPS_SMIN: f64 = 1e-4;
/// Sharpness of the P-norm soft maximum. Higher ⇒ closer to the true max (and
/// less of the P-norm's upward bias), at the cost of a stiffer gradient.
const P_SMAX: f64 = 40.0;
/// Cap on the lateral support reach (cells) so the per-cell support stencil
/// `(2·reach+1)²` stays cheap on very shallow angles.
const MAX_REACH: i64 = 6;

/// Lateral support reach in cells for the self-supporting angle `deg`, measured
/// from the HORIZONTAL build plate and interpreted as the minimum overhang the
/// print can hold: **0° = flat overhangs allowed (effectively no constraint)**,
/// 45° = the classic one-cell rule, **90° = vertical growth only (reach 0)**.
/// One layer up, a printable overhang at angle θ advances at most `1/tan(θ)`
/// cells laterally, so reach = round(1/tan(θ)) — larger θ ⇒ smaller reach ⇒
/// stricter. Capped at `MAX_REACH` for the shallow end.
fn reach_cells(deg: f64) -> i64 {
    let deg = deg.clamp(0.0, 90.0);
    if deg < 1.0 {
        return MAX_REACH; // ~horizontal: unconstrained
    }
    let r = (1.0 / deg.to_radians().tan()).round() as i64;
    r.clamp(0, MAX_REACH)
}

pub struct SelfSupportFilter {
    /// Design slots ordered by ascending cell-z (forward processing order; the
    /// transpose walks it in reverse). Same length as the design vector.
    order: Vec<u32>,
    /// Per design slot: the design slots in the layer below within the cone.
    supporters: Vec<Vec<u32>>,
    /// Per design slot: unconditionally supported (plate layer, or a fully
    /// solid/frozen cell sits in the cone below) ⇒ printed = blueprint.
    full_support: Vec<bool>,
    n: usize,
}

impl SelfSupportFilter {
    /// Build the support graph. `design_cells` are the optimizer's design
    /// cells (padded-grid cell ids); `frozen` are the always-solid cells
    /// (skin / keep regions) — full supporters when they sit in the cone.
    pub fn build(
        grid: &VoxelGrid,
        design_cells: &[u32],
        frozen: &[u32],
        overhang_deg: f64,
    ) -> Self {
        let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
        let reach = reach_cells(overhang_deg);
        let n = design_cells.len();

        let mut slot_of = vec![u32::MAX; grid.cell_count()];
        for (i, &c) in design_cells.iter().enumerate() {
            slot_of[c as usize] = i as u32;
        }
        let mut is_frozen = vec![false; grid.cell_count()];
        for &c in frozen {
            if (c as usize) < is_frozen.len() {
                is_frozen[c as usize] = true;
            }
        }

        // Lowest occupied (solid) layer = the build plate contact.
        let mut zmin = nz;
        for &c in design_cells.iter().chain(frozen) {
            let cz = (c as usize) / (nx * ny);
            if cz < zmin {
                zmin = cz;
            }
        }

        let mut supporters = vec![Vec::new(); n];
        let mut full_support = vec![false; n];
        for (i, &c) in design_cells.iter().enumerate() {
            let c = c as usize;
            let (cx, cy, cz) = (c % nx, (c / nx) % ny, c / (nx * ny));
            if cz <= zmin {
                full_support[i] = true; // resting on the plate
                continue;
            }
            let zb = cz - 1;
            let mut full = false;
            for dy in -reach..=reach {
                for dx in -reach..=reach {
                    let x = cx as i64 + dx;
                    let y = cy as i64 + dy;
                    if x < 0 || y < 0 || x >= nx as i64 || y >= ny as i64 {
                        continue;
                    }
                    let cb = (zb * ny + y as usize) * nx + x as usize;
                    if grid.scale[cb] <= 0.0 {
                        continue; // void below: not a supporter
                    }
                    if is_frozen[cb] {
                        full = true; // solid below fully supports
                    } else {
                        let s = slot_of[cb];
                        if s != u32::MAX {
                            supporters[i].push(s);
                        }
                    }
                }
            }
            full_support[i] = full;
        }

        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_by_key(|&i| {
            let c = design_cells[i as usize] as usize;
            c / (nx * ny) // cell-z
        });

        Self { order, supporters, full_support, n }
    }

    /// Support value S_e for cell `i` given the printed field `xi`, plus its
    /// kind: full-support ⇒ S = 1; empty cone ⇒ S = 0; else the P-norm.
    #[inline]
    fn support_value(&self, i: usize, xi: &[f64]) -> f64 {
        if self.full_support[i] {
            return 1.0;
        }
        let sup = &self.supporters[i];
        if sup.is_empty() {
            return 0.0;
        }
        let mut acc = 0f64;
        for &s in sup {
            acc += xi[s as usize].max(0.0).powf(P_SMAX);
        }
        acc.powf(1.0 / P_SMAX)
    }

    /// Forward projection: blueprint densities `x_in` (already in [floor, cap])
    /// → printed densities `x_out`. Processes layers bottom-up so a cell's
    /// supporters are finalized before it.
    pub fn apply(&self, x_in: &[f64], x_out: &mut [f64]) {
        for k in 0..self.n {
            x_out[k] = x_in[k]; // default (overwritten in order below)
        }
        for &iu in &self.order {
            let i = iu as usize;
            let s = self.support_value(i, x_out);
            x_out[i] = smin(x_in[i], s);
        }
    }

    /// Transpose / adjoint: given the upstream gradient `g` = dC/d(printed),
    /// accumulate `out` = dC/d(blueprint). Stateless — re-runs the forward pass
    /// from `x_in` to recover the printed field and the local partials.
    pub fn apply_t(&self, x_in: &[f64], g: &[f64], out: &mut [f64]) {
        // Forward pass to recover the printed field.
        let mut xi = vec![0f64; self.n];
        self.apply(x_in, &mut xi);

        // Adjoint accumulator, seeded with the direct upstream gradient.
        let mut lambda = g.to_vec();
        out.iter_mut().for_each(|v| *v = 0.0);

        // Reverse layer sweep: by the time cell e is visited, every cell it
        // supports (higher layers) has already pushed its contribution into
        // lambda[e].
        for &iu in self.order.iter().rev() {
            let i = iu as usize;
            let s = self.support_value(i, &xi);
            let (dxi_drho, dxi_ds) = smin_grad(x_in[i], s);
            // dC/d blueprint at this cell.
            out[i] += lambda[i] * dxi_drho;
            // Propagate through the support max into the supporters' adjoints.
            if self.full_support[i] {
                continue; // S ≡ 1, no dependence on supporters
            }
            let sup = &self.supporters[i];
            if sup.is_empty() || s <= 0.0 {
                continue;
            }
            let up = lambda[i] * dxi_ds;
            // d S / d ξ_s = (ξ_s / S)^{P-1}
            let inv_s = 1.0 / s;
            for &su in sup {
                let xs = xi[su as usize].max(0.0);
                let dsdxs = (xs * inv_s).powf(P_SMAX - 1.0);
                lambda[su as usize] += up * dsdxs;
            }
        }
    }
}

/// Smooth minimum: ½(a + b − √((a−b)² + ε)).
#[inline]
fn smin(a: f64, b: f64) -> f64 {
    let d = a - b;
    0.5 * (a + b - (d * d + EPS_SMIN).sqrt())
}

/// Partials (∂/∂a, ∂/∂b) of `smin`.
#[inline]
fn smin_grad(a: f64, b: f64) -> (f64, f64) {
    let d = a - b;
    let r = d / (d * d + EPS_SMIN).sqrt();
    (0.5 * (1.0 - r), 0.5 * (1.0 + r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reach_tracks_angle() {
        assert_eq!(reach_cells(45.0), 1); // classic one-cell rule
        assert_eq!(reach_cells(30.0), 2); // 1/tan30 ≈ 1.73 → 2 (shallower ⇒ wider)
        assert_eq!(reach_cells(90.0), 0); // vertical only
        assert_eq!(reach_cells(5.0), MAX_REACH); // ~horizontal ⇒ unconstrained
    }

    /// An unsupported overhang (a high-density island floating above void with
    /// nothing in its cone) must be pushed toward zero by the projection,
    /// while a column resting on the plate is preserved.
    #[test]
    fn overhang_is_removed_column_is_kept() {
        // 5×1×5 grid: a vertical column at x=0 (supported up from the plate)
        // and a single high cell at (x=4, z=4) floating with void below.
        let (nx, ny, nz, h) = (5usize, 1usize, 5usize, 1.0);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let design: Vec<u32> = (0..grid.cell_count() as u32).collect();
        let ss = SelfSupportFilter::build(&grid, &design, &[], 45.0);
        let cell = |x: usize, z: usize| grid.cell_index(x, 0, z) as usize;

        let mut x = vec![0.02f64; design.len()];
        for z in 0..nz {
            x[cell(0, z)] = 1.0; // a full column on the plate
        }
        x[cell(4, 4)] = 1.0; // a floating high cell (void directly below it)

        let mut xi = vec![0f64; design.len()];
        ss.apply(&x, &mut xi);

        assert!(xi[cell(0, 0)] > 0.95, "plate cell preserved");
        assert!(xi[cell(0, 4)] > 0.9, "supported column top preserved");
        assert!(
            xi[cell(4, 4)] < 0.1,
            "floating overhang must be projected toward void, got {}",
            xi[cell(4, 4)]
        );
    }

    /// Finite-difference check of the transpose (the OC update trusts it).
    #[test]
    fn transpose_matches_finite_difference() {
        let (nx, ny, nz, h) = (4usize, 3usize, 4usize, 1.0);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let design: Vec<u32> = (0..grid.cell_count() as u32).collect();
        let ss = SelfSupportFilter::build(&grid, &design, &[], 40.0);
        let n = design.len();

        // A varied blueprint field in (0,1).
        let mut x = vec![0f64; n];
        for (i, v) in x.iter_mut().enumerate() {
            *v = 0.15 + 0.7 * ((i * 7 % 11) as f64 / 11.0);
        }
        // An arbitrary upstream gradient g; scalar objective J = Σ g·ξ(x).
        let g: Vec<f64> = (0..n).map(|i| 0.3 + (i % 5) as f64 * 0.1).collect();

        let mut analytic = vec![0f64; n];
        ss.apply_t(&x, &g, &mut analytic);

        let j = |xx: &[f64]| -> f64 {
            let mut xi = vec![0f64; n];
            ss.apply(xx, &mut xi);
            xi.iter().zip(&g).map(|(&a, &b)| a * b).sum()
        };
        let eps = 1e-6;
        let mut max_err = 0f64;
        for k in 0..n {
            let mut xp = x.clone();
            let mut xm = x.clone();
            xp[k] += eps;
            xm[k] -= eps;
            let fd = (j(&xp) - j(&xm)) / (2.0 * eps);
            max_err = max_err.max((fd - analytic[k]).abs());
        }
        assert!(max_err < 1e-3, "transpose vs FD mismatch: {max_err}");
    }
}
