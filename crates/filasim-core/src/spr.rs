// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Superconvergent patch recovery (SPR) of the stress TENSOR, plus a
//! traction projection that restores free-surface equilibrium exactly.
//!
//! # Why not the plain nodal average
//!
//! [`crate::stress::recover_nodal`] gives a node the MEAN of its 8 adjacent
//! cell-center values. The 8 centers form a cube centred on the node, so for a
//! LINEAR stress field the mean is exactly the nodal value — the averaging is
//! not the problem on a smooth field, and `surfstress.rs` case A confirms it
//! (≈2% error on a round cantilever, converging).
//!
//! It breaks at a stress CONCENTRATION. There the field is strongly curved, and
//! the mean of a convex field always sits BELOW its value at the patch centre —
//! so the recovered peak is flattened. Measured on the Kirsch plate
//! (`surfstress.rs` case B): the displayed hole-edge Kt reads **−24% at h=1 and
//! −14% at h=0.5**, i.e. the app under-reports the very stress the user is
//! looking for, which over-reports the safety factor.
//!
//! A linear patch fit does not help — it reproduces exactly the same value at
//! the patch centre. Capturing a peak needs CURVATURE, so this module fits a
//! **quadratic** polynomial (10 terms) by least squares over a 4×4×4 cell patch
//! and evaluates it at the node. Because the basis is centred on the node, the
//! recovered value is simply the constant coefficient.
//!
//! # The traction projection (the "C" of SPR-C, cheaply)
//!
//! Textbook SPR-C (Ródenas et al. 2007) adds the boundary-equilibrium
//! constraint via Lagrange multipliers, which couples all 6 stress components
//! into one 60-unknown system per patch. That is ~200× the arithmetic of six
//! independent 10×10 fits and is not affordable per node in a browser.
//!
//! Instead the fit stays decoupled and the constraint is applied at the
//! evaluation point in closed form. Given a residual traction `r = σ·n − t̄`,
//! the minimum-Frobenius-norm SYMMETRIC correction satisfying `Δ·n = r` is
//!
//! ```text
//! Δ = r nᵀ + n rᵀ − (r·n) n nᵀ
//! ```
//!
//! Subtracting it makes `σ·n = t̄` hold EXACTLY at that point for ~20 flops.
//! This is weaker than full SPR-C — it enforces the constraint pointwise rather
//! than over the patch, and it does not impose internal equilibrium — but it
//! captures the part that matters for a surface readout.
//!
//! # MEASURED OUTCOME — read this before wiring SPR into the display
//!
//! Benchmarked against `tests/surfstress.rs` (mean vs SPR vs SPR+projection):
//!
//! | case | defect | mean (ships today) | SPR + projection |
//! |---|---|---|---|
//! | A round cantilever (smooth free surface) | small, O(h) | 3.8% / 1.7% RMS | 4.7% / 2.8% — **worse** |
//! | B plate with hole (REAL peak) | −24% / −14% | baseline | −22% axis-aligned, **−11% at 45°** |
//! | C shoulder fillet (SPURIOUS staircase peak) | +9% / +14%, diverging | baseline | +14% / +20% — **worse** |
//! | traction residual, all cases | 0.2–15% | baseline | **exactly 0** |
//!
//! The pattern is coherent: SPR is a SHARPENING filter. It faithfully
//! reconstructs whatever is in the cell-centre field — including the spurious
//! peak the voxel staircase manufactures at a re-entrant fillet, and including
//! boundary noise where the patch is truncated. The plain mean was acting as an
//! accidental low-pass filter that partly masked those errors.
//!
//! So SPR is **not a net win as a drop-in replacement**, and the recovery is NOT
//! the bottleneck: no recovery can repair a cell field that already contains a
//! spurious peak. The remaining error is in the cell-centre stress itself.
//!
//! Of the two follow-ons this paragraph used to propose, one failed and one was
//! wrongly written off. Proper cut-cell quadrature: implemented in
//! [`crate::cutcell`], correct, no user-visible gain — that verdict stands.
//!
//! Resolving the feature locally (submodel) was declared dead by
//! `surf_kirsch_h_refinement`, on the grounds that the MEDIAN surface point
//! converges while the MAX — the only statistic the app shows — does not,
//! because the staircase puts `O(1/h)` corners on the rim, so a submodel would
//! inherit the defect it was meant to cure. **That holds only for the raw
//! read-back.** [`crate::stress::recover_surface`] stops reading the boundary
//! cells at all, the MAX converges too (observed order ≈1), and
//! `surf_kirsch_submodel` measured a real submodel reaching −3.7% in 21 s
//! against 548 s for the equivalent global refinement. Submodeling is live
//! again; SPR is not.
//!
//! What IS unambiguously worth shipping from this module is
//! [`project_traction`]: it zeroes the free-surface traction by construction for
//! ~20 flops, it is independent of which recovery feeds it, and it improves
//! every case where the peak is real. Keep the SPR fit for the constrained
//! (fully coupled) variant and for error estimation; do not swap the display
//! over to it on these numbers.

use crate::voxel::VoxelGrid;

/// Quadratic basis in 3D: 1, x, y, z, x², y², z², xy, yz, zx.
const NB: usize = 10;
/// Linear basis is the leading 4 terms of the same array.
const NB_LIN: usize = 4;

/// Minimum solid cells in a patch before the quadratic fit is trusted. The
/// quadratic has 10 unknowns; demanding a healthy margin keeps the normal
/// equations away from the rank cliff on ragged boundary patches.
const MIN_CELLS_QUAD: usize = 18;
/// Below that, fall back to a linear fit (4 unknowns).
const MIN_CELLS_LIN: usize = 8;

/// Tikhonov ridge as a fraction of the mean diagonal — insurance against the
/// degenerate patch, not a smoothing knob. A one-cell-thick shell (routine in
/// FDM parts) puts every sample in a plane, which makes the quadratic singular
/// along the normal; the ridge plus the Cholesky failure path catch it.
const RIDGE: f64 = 1e-6;

/// Gaussian sample weight `exp(−(d/SIGMA)²)`, `d` = node-to-cell-centre distance
/// in cell units. A quadratic needs 10 samples, which forces a patch spanning
/// ±2h — but a stress concentration decays over a comparable length, so an
/// UNWEIGHTED fit averages straight across the peak it is meant to recover.
/// Weighting keeps the patch wide enough to stay determined while letting the
/// near ring dominate. 1.5 was picked as the point where the Kirsch peak stops
/// improving and boundary-patch noise starts growing.
const SIGMA: f64 = 1.5;

#[inline]
fn basis(x: f64, y: f64, z: f64) -> [f64; NB] {
    [1.0, x, y, z, x * x, y * y, z * z, x * y, y * z, z * x]
}

/// In-place Cholesky of the leading `n`×`n` block of a row-major `NB`×`NB`
/// symmetric matrix. Returns false when a pivot is non-positive — which is the
/// rank-deficiency signal we want, not an error to paper over.
fn cholesky(a: &mut [[f64; NB]; NB], n: usize) -> bool {
    for i in 0..n {
        for j in 0..=i {
            let mut s = a[i][j];
            for k in 0..j {
                s -= a[i][k] * a[j][k];
            }
            if i == j {
                if s <= 1e-300 {
                    return false;
                }
                a[i][i] = s.sqrt();
            } else {
                a[i][j] = s / a[j][j];
            }
        }
    }
    true
}

/// Solve `L Lᵀ x = b` in place given the Cholesky factor from [`cholesky`].
fn chol_solve(l: &[[f64; NB]; NB], n: usize, b: &mut [f64; NB]) {
    for i in 0..n {
        let mut s = b[i];
        for k in 0..i {
            s -= l[i][k] * b[k];
        }
        b[i] = s / l[i][i];
    }
    for i in (0..n).rev() {
        let mut s = b[i];
        for k in (i + 1)..n {
            s -= l[k][i] * b[k];
        }
        b[i] = s / l[i][i];
    }
}

/// Recover a per-cell stress tensor field to the nodes by quadratic SPR.
///
/// `cells` are the six per-cell component arrays in Voigt order
/// `[xx, yy, zz, xy, yz, zx]`, each `nx*ny*nz` long (as produced by
/// [`crate::stress::cell_field`]). Returns one interleaved array of
/// `(nx+1)(ny+1)(nz+1)` nodes × 6 components; nodes whose patch holds no solid
/// cell are `NaN`, matching [`crate::stress::recover_nodal`] so existing
/// samplers can renormalize around them unchanged.
pub fn recover_nodal_spr(grid: &VoxelGrid, cells: &[&[f32]; 6]) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let mut out = vec![f32::NAN; mx * my * mz * 6];

    for k in 0..mz {
        for j in 0..my {
            for i in 0..mx {
                let node = (k * my + j) * mx + i;
                // Patch: the 4×4×4 cell block centred on the node. Cell (cx,cy,cz)
                // has its centre at (cx+0.5) in cell units, so offsets -2..=1
                // put the samples symmetrically at ±0.5h and ±1.5h.
                let mut ata = [[0f64; NB]; NB];
                let mut atb = [[0f64; NB]; 6];
                let mut count = 0usize;
                let mut mean = [0f64; 6];

                let lo = |c: usize| (c as i64) - 2;
                for cz in lo(k)..(k as i64 + 2) {
                    if cz < 0 || cz >= nz as i64 {
                        continue;
                    }
                    for cy in lo(j)..(j as i64 + 2) {
                        if cy < 0 || cy >= ny as i64 {
                            continue;
                        }
                        for cx in lo(i)..(i as i64 + 2) {
                            if cx < 0 || cx >= nx as i64 {
                                continue;
                            }
                            let ci = ((cz as usize) * ny + cy as usize) * nx + cx as usize;
                            if grid.scale[ci] <= 0.0 {
                                continue;
                            }
                            // Node-centred, h-normalized coordinates.
                            let (dx, dy, dz) = (
                                cx as f64 + 0.5 - i as f64,
                                cy as f64 + 0.5 - j as f64,
                                cz as f64 + 0.5 - k as f64,
                            );
                            let p = basis(dx, dy, dz);
                            let w = (-(dx * dx + dy * dy + dz * dz) / (SIGMA * SIGMA)).exp();
                            for a in 0..NB {
                                for b in 0..=a {
                                    ata[a][b] += w * p[a] * p[b];
                                }
                            }
                            for c in 0..6 {
                                let v = cells[c][ci] as f64;
                                mean[c] += v;
                                for a in 0..NB {
                                    atb[c][a] += w * p[a] * v;
                                }
                            }
                            count += 1;
                        }
                    }
                }

                if count == 0 {
                    continue; // stays NaN
                }
                for m in mean.iter_mut() {
                    *m /= count as f64;
                }

                let n_basis = if count >= MIN_CELLS_QUAD {
                    NB
                } else if count >= MIN_CELLS_LIN {
                    NB_LIN
                } else {
                    0
                };

                let mut done = false;
                if n_basis > 0 {
                    // Mirror the lower triangle and add the ridge.
                    let diag: f64 =
                        (0..n_basis).map(|a| ata[a][a]).sum::<f64>() / n_basis as f64;
                    let mut m = [[0f64; NB]; NB];
                    for a in 0..n_basis {
                        for b in 0..=a {
                            m[a][b] = ata[a][b];
                        }
                        m[a][a] += RIDGE * diag;
                    }
                    if cholesky(&mut m, n_basis) {
                        for c in 0..6 {
                            let mut rhs = [0f64; NB];
                            rhs[..n_basis].copy_from_slice(&atb[c][..n_basis]);
                            chol_solve(&m, n_basis, &mut rhs);
                            // Basis is node-centred, so evaluating the fitted
                            // polynomial AT the node is just its constant term.
                            out[node * 6 + c] = rhs[0] as f32;
                        }
                        done = true;
                    }
                }
                if !done {
                    // Degenerate patch (too few cells, or all coplanar — a
                    // one-cell shell). The plain mean is the honest answer.
                    for c in 0..6 {
                        out[node * 6 + c] = mean[c] as f32;
                    }
                }
            }
        }
    }
    out
}

/// Make `sigma · n == t_bar` hold exactly, changing `sigma` as little as
/// possible (minimum Frobenius norm, symmetry preserved).
///
/// `sigma` is Voigt `[xx, yy, zz, xy, yz, zx]`; `n` must be a unit vector.
/// On a traction-free surface pass `t_bar = [0.0; 3]`.
pub fn project_traction(sigma: &mut [f64; 6], n: [f64; 3], t_bar: [f64; 3]) {
    let t = [
        sigma[0] * n[0] + sigma[3] * n[1] + sigma[5] * n[2],
        sigma[3] * n[0] + sigma[1] * n[1] + sigma[4] * n[2],
        sigma[5] * n[0] + sigma[4] * n[1] + sigma[2] * n[2],
    ];
    let r = [t[0] - t_bar[0], t[1] - t_bar[1], t[2] - t_bar[2]];
    let c = r[0] * n[0] + r[1] * n[1] + r[2] * n[2];
    // Δ = r nᵀ + n rᵀ − (r·n) n nᵀ  ⇒  Δ·n = r, Δ symmetric, ‖Δ‖_F minimal.
    sigma[0] -= 2.0 * r[0] * n[0] - c * n[0] * n[0];
    sigma[1] -= 2.0 * r[1] * n[1] - c * n[1] * n[1];
    sigma[2] -= 2.0 * r[2] * n[2] - c * n[2] * n[2];
    sigma[3] -= r[0] * n[1] + n[0] * r[1] - c * n[0] * n[1];
    sigma[4] -= r[1] * n[2] + n[1] * r[2] - c * n[1] * n[2];
    sigma[5] -= r[2] * n[0] + n[2] * r[0] - c * n[2] * n[0];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lin_grid() -> VoxelGrid {
        VoxelGrid::solid_box(8, 8, 8, 1.0)
    }

    /// SPR must reproduce a QUADRATIC field exactly — that is the whole point
    /// over the plain mean, which can only reproduce a linear one.
    #[test]
    fn spr_reproduces_quadratic_field_exactly() {
        let g = lin_grid();
        let n = g.cell_count();
        // σxx = 2 + 3x − x², others distinct so a component mix-up is caught.
        let mut comp: Vec<Vec<f32>> = (0..6).map(|_| vec![0f32; n]).collect();
        let val = |x: f64, c: usize| -> f64 {
            match c {
                0 => 2.0 + 3.0 * x - x * x,
                1 => 1.0 - 0.5 * x * x,
                2 => 0.25 * x,
                3 => 7.0,
                4 => -2.0 + x,
                _ => x * x,
            }
        };
        for cz in 0..g.nz {
            for cy in 0..g.ny {
                for cx in 0..g.nx {
                    let ci = (cz * g.ny + cy) * g.nx + cx;
                    let x = cx as f64 + 0.5;
                    for (c, comp_c) in comp.iter_mut().enumerate() {
                        comp_c[ci] = val(x, c) as f32;
                    }
                }
            }
        }
        let refs: [&[f32]; 6] = [&comp[0], &comp[1], &comp[2], &comp[3], &comp[4], &comp[5]];
        let nodal = recover_nodal_spr(&g, &refs);

        // Check an interior node with a full 4×4×4 patch.
        let (mx, my) = (g.nx + 1, g.ny + 1);
        let (i, j, k) = (4usize, 4usize, 4usize);
        let node = (k * my + j) * mx + i;
        for c in 0..6 {
            let got = nodal[node * 6 + c] as f64;
            let want = val(i as f64, c);
            assert!(
                (got - want).abs() < 1e-3,
                "component {c}: SPR {got:.6} vs exact {want:.6}"
            );
        }
    }

    /// The failure mode this module exists to fix: on a curved (peaked) field
    /// the plain mean under-reads while SPR does not.
    #[test]
    fn spr_beats_mean_on_a_peak() {
        let g = lin_grid();
        let n = g.cell_count();
        // A parabolic ridge peaking at x = 4 — a stand-in for a concentration.
        let peak = |x: f64| 10.0 - (x - 4.0) * (x - 4.0);
        let mut s = vec![0f32; n];
        for cz in 0..g.nz {
            for cy in 0..g.ny {
                for cx in 0..g.nx {
                    s[(cz * g.ny + cy) * g.nx + cx] = peak(cx as f64 + 0.5) as f32;
                }
            }
        }
        let zero = vec![0f32; n];
        let refs: [&[f32]; 6] = [&s, &zero, &zero, &zero, &zero, &zero];
        let spr = recover_nodal_spr(&g, &refs);
        let mean = crate::stress::recover_nodal(&g, &s);

        let (mx, my) = (g.nx + 1, g.ny + 1);
        let node = (4 * my + 4) * mx + 4; // sits on the ridge crest
        let exact = peak(4.0);
        let e_spr = (spr[node * 6] as f64 - exact).abs();
        let e_mean = (mean[node] as f64 - exact).abs();
        assert!(e_spr < 1e-3, "SPR should be exact on a quadratic, off by {e_spr:.4}");
        assert!(
            e_mean > 0.1,
            "the plain mean is expected to flatten the peak (off by {e_mean:.4})"
        );
    }

    /// The traction projection must zero the free-surface traction exactly, for
    /// an arbitrary (non-axis-aligned) normal, and keep σ symmetric.
    #[test]
    fn traction_projection_zeroes_free_surface_traction() {
        let mut s = [12.0, -3.0, 5.0, 2.5, -1.5, 0.75];
        let raw = s;
        let n = {
            let v = [0.37, -0.62, 0.69f64];
            let m = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            [v[0] / m, v[1] / m, v[2] / m]
        };
        project_traction(&mut s, n, [0.0; 3]);
        let t = [
            s[0] * n[0] + s[3] * n[1] + s[5] * n[2],
            s[3] * n[0] + s[1] * n[1] + s[4] * n[2],
            s[5] * n[0] + s[4] * n[1] + s[2] * n[2],
        ];
        let mag = (t[0] * t[0] + t[1] * t[1] + t[2] * t[2]).sqrt();
        assert!(mag < 1e-9, "traction must vanish after projection, got {mag:.3e}");
        // And it must be a genuine correction, not a no-op.
        let changed: f64 = (0..6).map(|c| (s[c] - raw[c]).abs()).sum();
        assert!(changed > 1e-6, "projection did nothing on a violating state");
    }

    /// A state that already satisfies the constraint must be left alone.
    #[test]
    fn traction_projection_is_idempotent_on_a_valid_state() {
        let n = [0.0, 0.0, 1.0];
        // Plane stress in the xy plane: σ·ẑ is already zero.
        let mut s = [9.0, -4.0, 0.0, 3.0, 0.0, 0.0];
        let before = s;
        project_traction(&mut s, n, [0.0; 3]);
        for c in 0..6 {
            assert!(
                (s[c] - before[c]).abs() < 1e-12,
                "component {c} moved on an already-valid state"
            );
        }
    }
}
