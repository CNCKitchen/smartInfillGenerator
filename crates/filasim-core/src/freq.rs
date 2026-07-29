// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Fundamental-frequency objective (DESIGN.md §26): the KS-aggregated
//! eigenvalue and its density sensitivity, for "maximize f1 at a mass budget".
//!
//! ## Why aggregate at all
//!
//! Maximizing `λ₁` is a **max-min** problem. Pushing the lowest eigenvalue up
//! drives it into the second, and at a crossing `λ₁` is not differentiable —
//! a plain gradient step then ping-pongs between the two branches forever
//! instead of raising both together. Real parts sit exactly there: the
//! `cantilever_matches_euler_bernoulli` golden asserts modes 1 and 2 of a
//! square-section beam agree within 10 %.
//!
//! The fix here is a **Kreisselmeier–Steinhauser softmin** over the lowest `m`
//! eigenvalues — a smooth lower bound on `λ₁` whose gradient is a convex blend
//! of the individual mode sensitivities. At a crossing both modes get weight,
//! so the update raises the *pair*, which is the physically right move. As the
//! spectrum separates the weights collapse onto mode 1 and the objective
//! becomes `λ₁` again.
//!
//! ## Why λ and not Hz
//!
//! `λ = ω²` is the quantity linear in `K` (the Rayleigh quotient is
//! `λ = φᵀKφ / φᵀMφ`), so the sensitivity is clean in λ. `f = √λ/2π` is a
//! monotone map, so maximizing λ maximizes f — the conversion happens only at
//! the reporting boundary.

use crate::fem::NODE_OFFSETS;
use crate::mg::Level;

/// KS aggregation sharpness (dimensionless — applied to `λᵢ/λ₁`).
///
/// Larger = closer to a true min, but a sharper kink at crossings. At κ = 30 a
/// mode 5 % above the fundamental still carries ~18 % weight (enough to be
/// pulled along), while one 30 % above carries ~0.01 % (effectively ignored).
/// The KS value understates λ₁ by at most `ln(m)/κ` — 0.5 % for m = 4 — which
/// only matters as a reporting offset, never for the search direction.
pub const KS_KAPPA: f64 = 30.0;

/// How many of the lowest modes enter the aggregate. Enough to hold a
/// degenerate pair plus its neighbours; more costs block width in the
/// eigensolver for no extra smoothing.
pub const KS_MODES: usize = 4;

/// A KS softmin over the lowest eigenvalues.
pub struct KsAggregate {
    /// `λ_KS ≤ λ₁` — the smooth surrogate actually maximized.
    pub value: f64,
    /// Per-mode blend weights, summing to 1. `dλ_KS/dx = Σᵢ wᵢ · dλᵢ/dx`.
    pub weights: Vec<f64>,
}

/// KS softmin of `lambdas` (ascending, `λ₁ > 0`).
///
/// `λ_KS = λ₁ − (λ_ref/κ)·ln Σᵢ exp(−κ·(λᵢ−λ₁)/λ_ref)`
///
/// Shifted by `λ₁` so the exponent is bounded above by 0 (no overflow) and
/// scaled by `λ_ref` so it is dimensionless — λ is ~1e7 for a part with a
/// few-hundred-Hz fundamental, far outside the range where an unshifted,
/// unscaled `exp(−κλ)` retains any precision.
///
/// # `lambda_ref` must be FROZEN
///
/// `lambda_ref` is a normalizing constant that the caller holds fixed while
/// differentiating. That is what makes `dλ_KS/dλⱼ = weights[j]` exact.
///
/// Normalizing by the *live* `λ₁` instead is the obvious-looking simplification
/// and it is wrong: λ₁ would then enter through the shift and the scale factor
/// as well, adding `dλ₁/dx` terms that the weights do not account for. At
/// κ = 2 that error is order-unity (it put the gradient 31 % off in
/// `ks_sensitivity_matches_finite_difference`, which is why that test exists).
/// It nearly vanishes at large κ — where the sum collapses to 1 and `ln S → 0`
/// — so it would have shipped looking fine and quietly degraded the search
/// direction on exactly the clustered spectra the aggregation is there for.
///
/// The optimizer re-freezes `λ_ref` at each iteration's `λ₁`, so the objective
/// is rescaled by a constant per iteration — invisible to the OC update, which
/// normalizes through its own volume multiplier anyway.
pub fn ks_aggregate(lambdas: &[f64], kappa: f64, lambda_ref: f64) -> KsAggregate {
    debug_assert!(!lambdas.is_empty());
    let l1 = lambdas[0];
    if !(l1 > 0.0) || !lambda_ref.is_finite() || !(lambda_ref > 0.0) {
        // Degenerate spectrum (a rigid-body / unconstrained mode reached the
        // objective). Fall back to "all weight on the fundamental" — the caller
        // still gets a usable direction, and the under-constraint gate upstream
        // is what should have caught this.
        let mut weights = vec![0.0; lambdas.len()];
        weights[0] = 1.0;
        return KsAggregate { value: l1.max(0.0), weights };
    }
    let e: Vec<f64> = lambdas.iter().map(|&li| (-kappa * (li - l1) / lambda_ref).exp()).collect();
    let s: f64 = e.iter().sum();
    KsAggregate {
        value: l1 - (lambda_ref / kappa) * s.ln(),
        weights: e.iter().map(|ei| ei / s).collect(),
    }
}

/// Density below which the mass interpolation is bent away from linear.
///
/// Set to the printable infill floor, so for every INFILL mode (band
/// `[floor, cap]` with `floor ≥ 0.10`) `mass_interp` is the exact identity and
/// this whole mechanism is inert. It exists for SOLID topology mode, whose
/// lower bound is ersatz void (`1e-3`).
pub const MASS_RHO0: f64 = 0.10;

/// Decay exponent applied below [`MASS_RHO0`]. Must exceed the stiffness
/// exponent (3 in solid mode) for the guard to work at all; 6 is the
/// literature-standard choice and leaves plenty of margin.
pub const MASS_Q: f64 = 6.0;

/// Mass interpolation `M(ρ)` for the eigen-objective — identity above
/// [`MASS_RHO0`], steeply decaying below it.
///
/// # Why not just use ρ
///
/// Stiffness follows `ρ^p` (p = 3 in solid topology mode) while mass follows
/// `ρ¹`, so the local stiffness-to-mass ratio `ρ^(p−1)` collapses toward zero as
/// a cell empties. Near-void regions then behave like heavy, floppy membranes
/// and the eigensolver dutifully reports their **spurious localized modes** as
/// the fundamental — the optimizer is handed a gradient describing a numerical
/// artifact rather than the part.
///
/// Bending the mass down as `ρ^6` below the threshold makes `K/M ~ ρ^(3−6)`
/// GROW as a cell empties, so an emptying region can no longer host a low
/// frequency and the fundamental stays a global mode.
///
/// The join at `ρ₀` is continuous but not smooth (slope 1 above, `q` below).
/// That kink is the standard form and is harmless here: it sits at a density
/// the converged design is pushed away from, and the density filter smooths any
/// cell that lingers near it.
#[inline]
pub fn mass_interp(rho: f64) -> f64 {
    if rho >= MASS_RHO0 {
        rho
    } else {
        MASS_RHO0 * (rho / MASS_RHO0).max(0.0).powf(MASS_Q)
    }
}

/// `dM/dρ` for [`mass_interp`].
#[inline]
pub fn d_mass_interp(rho: f64) -> f64 {
    if rho >= MASS_RHO0 {
        1.0
    } else {
        MASS_Q * (rho / MASS_RHO0).max(0.0).powf(MASS_Q - 1.0)
    }
}

/// [`crate::eps::build_vfrac`] with [`mass_interp`] applied to the infill share
/// — the mass field the eigen-objective optimizes against.
///
/// Used ONLY inside the optimizer loop, where intermediate densities exist.
/// Verification (`pipeline::modal_f1`) deliberately uses the TRUE mass, because
/// the delivered number must be the part's real frequency, not the guarded
/// surrogate's. The two agree at any deliverable design anyway: binned infill
/// sits at or above the floor where `mass_interp` is the identity, and binned
/// solid mode is `{void, solid}` where it is likewise exact.
pub fn build_modal_vfrac(
    grid: &crate::voxel::VoxelGrid,
    design_cells: &[u32],
    skin_frac: &[f32],
    x: &[f64],
) -> Vec<f32> {
    let mut vf = grid.scale.clone();
    for (k, &c) in design_cells.iter().enumerate() {
        let occ = grid.scale[c as usize] as f64;
        let f = skin_frac[k] as f64;
        // The wall-band share `f` is genuinely solid material — only the infill
        // share carries the design density and gets the guard.
        vf[c as usize] = (occ * (f + (1.0 - f) * mass_interp(x[k]))) as f32;
    }
    vf
}

/// Per-cell **kinetic energy** `Σ_{8 nodes} |φ_node|²` of a mode shape — the
/// mass-side counterpart of [`crate::simp::cell_strain_energy`].
///
/// The lumped mass puts `ρ·h³·vfrac/8` on each of a cell's 8 nodes in all three
/// translational DOFs, so `φᵀ(∂M_e/∂vfrac)φ = ρ·h³/8 · Σ|φ_node|²` — this sum
/// is the geometry-only factor. Constrained DOFs carry no mass, and `φ` is
/// identically zero there, so they drop out on their own.
pub fn cell_kinetic_energy(level: &Level, phi: &[f64], cells: &[u32], out: &mut [f64]) {
    let (nx, ny) = (level.nx, level.ny);
    let (mx, my) = (level.mx, level.my);
    for (k, &ci) in cells.iter().enumerate() {
        let ci = ci as usize;
        let cx = ci % nx;
        let cy = (ci / nx) % ny;
        let cz = ci / (nx * ny);
        let mut ke = 0f64;
        for [ox, oy, oz] in NODE_OFFSETS {
            let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
            let (a, b, c) = (phi[3 * n], phi[3 * n + 1], phi[3 * n + 2]);
            ke += a * a + b * b + c * c;
        }
        out[k] = ke;
    }
}

/// Inputs to [`ks_sensitivity`] that describe the design parameterization —
/// grouped because they travel together from the optimizer and are all constant
/// across the mode loop.
pub struct DesignLaw<'a> {
    /// Infill volume share per design cell, `occupancy × (1 − wall fraction)`.
    /// This is BOTH `∂vfrac/∂x` (mass) and the occupancy factor on `∂eps/∂x`
    /// (stiffness) — the same `w` the compliance path already builds.
    pub w: &'a [f64],
    /// Current printed density per design cell.
    pub x_phys: &'a [f64],
    /// Infill law `E/E₀ = coeff · x^exponent`.
    pub exponent: f64,
    pub coeff: f64,
    /// Mass density in consistent units (tonne/mm³).
    pub density: f64,
    /// Voxel volume `h³` (mm³).
    pub cell_vol: f64,
}

/// `dλ_KS/dx` per design cell.
///
/// For an M-normalized mode (`φᵀMφ = 1`) the classical first-order eigenvalue
/// derivative is
///
/// ```text
///   ∂λ/∂x_e = φᵀ(∂K/∂x_e)φ − λ · φᵀ(∂M/∂x_e)φ
/// ```
///
/// and the KS chain rule blends those across modes by `weights`. The two terms
/// pull in OPPOSITE directions — stiffness raises the frequency, the mass it
/// costs lowers it — which is exactly why this sensitivity is sign-indefinite
/// where the compliance one never is, and why the OC update needs the
/// non-negativity clamp documented at its call site.
///
/// `out` is `dλ_KS/dx` (positive = adding material here RAISES the frequency).
/// The caller negates it to feed a minimizer.
pub fn ks_sensitivity(
    level: &Level,
    ke64: &[[f64; 24]; 24],
    shapes: &[Vec<f64>],
    lambdas: &[f64],
    weights: &[f64],
    design_cells: &[u32],
    law: &DesignLaw,
    out: &mut [f64],
) {
    let n = design_cells.len();
    debug_assert_eq!(out.len(), n);
    out.iter_mut().for_each(|v| *v = 0.0);
    let mut se = vec![0f64; n];
    let mut ke = vec![0f64; n];
    // ∂(mass per unit vfrac)/∂x, minus the geometric Σ|φ|² factor.
    let mass_coef = law.density * law.cell_vol / 8.0;

    for (j, phi) in shapes.iter().enumerate() {
        let wj = weights[j];
        // Modes the softmin has effectively excluded contribute nothing but
        // two full sweeps over the grid — skip them.
        if wj < 1e-6 {
            continue;
        }
        crate::simp::cell_strain_energy(level, ke64, phi, design_cells, &mut se);
        cell_kinetic_energy(level, phi, design_cells, &mut ke);
        let lam = lambdas[j];
        for k in 0..n {
            // ∂eps/∂x = w · (1 − EMIN) · coeff · n · x^(n−1); the cap at solid
            // (`min(coeff·xⁿ, 1)` in `build_eps`) is inactive across the whole
            // printable band, so it is omitted here exactly as the compliance
            // sensitivity omits it.
            let d_eps = law.w[k]
                * (1.0 - crate::eps::EMIN_REL as f64)
                * law.coeff
                * law.exponent
                * law.x_phys[k].powf(law.exponent - 1.0);
            // ∂vfrac/∂x = w · M'(x), with M the guarded mass interpolation
            // (M' ≡ 1 across every infill band, so this is `w` there).
            let d_mass = law.w[k] * mass_coef * d_mass_interp(law.x_phys[k]);
            out[k] += wj * (d_eps * se[k] - lam * d_mass * ke[k]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ks_collapses_onto_the_fundamental_when_separated() {
        // A well-separated spectrum: the aggregate must be λ₁ to within a hair
        // and put essentially all weight on mode 1 (so it behaves as plain λ₁).
        let l = [1.0e7, 4.0e7, 9.0e7, 1.6e8];
        let a = ks_aggregate(&l, KS_KAPPA, l[0]);
        assert!(a.weights[0] > 0.999, "weights {:?}", a.weights);
        assert!(
            (a.value - l[0]).abs() / l[0] < 1e-3,
            "KS {} should track λ1 {}",
            a.value,
            l[0]
        );
    }

    #[test]
    fn ks_splits_weight_across_a_degenerate_pair() {
        // The case that breaks a naive max-λ₁ gradient: two coincident modes.
        // Both must carry equal weight so the update raises the PAIR.
        let l = [1.0e7, 1.0e7, 5.0e7, 6.0e7];
        let a = ks_aggregate(&l, KS_KAPPA, l[0]);
        assert!((a.weights[0] - 0.5).abs() < 1e-9, "weights {:?}", a.weights);
        assert!((a.weights[1] - 0.5).abs() < 1e-9, "weights {:?}", a.weights);
        // KS understates λ₁ by ln(2)/κ — a bounded, harmless reporting offset.
        let expect = l[0] * (1.0 - 2f64.ln() / KS_KAPPA);
        assert!((a.value - expect).abs() / expect < 1e-9);
    }

    #[test]
    fn ks_weights_always_sum_to_one() {
        for l in [
            vec![1.0e7, 1.02e7, 1.05e7, 3.0e7],
            vec![5.0, 5.0, 5.0, 5.0],
            vec![1.0e-3, 1.0e9],
            vec![2.5e7],
        ] {
            let a = ks_aggregate(&l, KS_KAPPA, l[0]);
            let s: f64 = a.weights.iter().sum();
            assert!((s - 1.0).abs() < 1e-12, "weights {:?} sum to {s}", a.weights);
            // A softmin is a lower bound on the minimum.
            assert!(a.value <= l[0] + 1e-9, "KS {} exceeds λ1 {}", a.value, l[0]);
        }
    }

    #[test]
    fn ks_survives_a_zero_fundamental() {
        // An unconstrained/rigid-body mode reaching the objective must not NaN.
        let a = ks_aggregate(&[0.0, 1.0e7], KS_KAPPA, 1.0e7);
        assert!(a.value.is_finite());
        assert!((a.weights.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn mass_interp_is_the_identity_across_every_infill_band() {
        // The guard must be provably inert for infill modes, or it would be
        // silently changing the shipped objective for the common case.
        for rho in [0.10, 0.15, 0.25, 0.5, 0.7, 1.0] {
            assert_eq!(mass_interp(rho), rho, "mass_interp bent at ρ={rho}");
            assert_eq!(d_mass_interp(rho), 1.0, "d_mass_interp bent at ρ={rho}");
        }
    }

    #[test]
    fn mass_interp_outruns_the_stiffness_law_below_the_threshold() {
        // The point of the guard: K/M must GROW as a solid-mode cell empties,
        // so an emptying region cannot host the fundamental. With stiffness
        // ρ³ and mass M(ρ), that means M must fall faster than ρ³.
        let stiffness = |r: f64| r.powi(3);
        let mut prev = f64::INFINITY;
        for &rho in &[0.09, 0.05, 0.02, 0.01, 1e-3] {
            let ratio = stiffness(rho) / mass_interp(rho);
            assert!(
                ratio > prev || prev.is_infinite(),
                "K/M fell from {prev:.3e} to {ratio:.3e} at ρ={rho} — the guard is not guarding"
            );
            prev = ratio;
        }
        // And it must be continuous at the join, or the optimizer sees a cliff.
        // Approaching from below the gap closes at the lower branch's slope
        // (`q`), so the bound is proportional to the offset — not a fixed
        // epsilon. A genuine discontinuity would leave a gap that does NOT
        // shrink with the offset, which is what the loop checks.
        for off in [1e-6f64, 1e-8, 1e-10] {
            let gap = (mass_interp(MASS_RHO0) - mass_interp(MASS_RHO0 - off)).abs();
            assert!(
                gap <= MASS_Q * off * 1.01,
                "mass_interp jumps at ρ0: gap {gap:.3e} at offset {off:.0e} exceeds q·offset"
            );
        }
    }

    #[test]
    fn d_mass_interp_matches_finite_difference_below_the_threshold() {
        // The guarded branch is only exercised by solid mode, which no other
        // test in this module reaches — so check its derivative directly.
        for rho in [0.02f64, 0.05, 0.09] {
            let d = 1e-6;
            let fd = (mass_interp(rho + d) - mass_interp(rho - d)) / (2.0 * d);
            let an = d_mass_interp(rho);
            let rel = (an - fd).abs() / fd.abs().max(1e-30);
            assert!(rel < 1e-4, "d_mass_interp({rho}) = {an:.6e} vs FD {fd:.6e} (rel {rel:.2e})");
        }
    }

    /// The end-to-end claim: optimizing for frequency at a FIXED mass budget
    /// must beat spending that same mass as uniform infill.
    ///
    /// This is the test that says the clamped OC update actually works. The FD
    /// test proves the gradient is right; nothing there proves the update rule
    /// can follow a sign-indefinite gradient to a better design instead of
    /// oscillating. Equal mass is the whole point — a "gain" bought by spending
    /// more material is no gain, so the budget is pinned and both designs are
    /// measured at the same achieved mean infill.
    ///
    /// Physically, a cantilever's fundamental wants stiffness at the root
    /// (high strain, low modal displacement) and mass off the tip (high modal
    /// displacement) — both terms of the sensitivity push the same way, so a
    /// working optimizer has an unmistakable signal to follow here.
    #[test]
    fn optimizing_for_frequency_beats_uniform_at_equal_mass() {
        use crate::eps::{build_eps, build_vfrac};
        use crate::modal::{analyze, ModalConfig};
        use crate::simp::{optimize_cached, FreqSpec, LoadSet, Objective, OptimizeParams};
        use crate::solve::{pad_for_levels, NodeProblem, SolveSettings, SolverCache};
        use crate::voxel::VoxelGrid;

        let settings = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };
        let density = 1.24e-9f64;
        let (nxc, nyc, nzc, h) = (40usize, 8usize, 8usize, 1.0f64);
        let raw = VoxelGrid::solid_box(nxc, nyc, nzc, h);
        let (grid, levels) = pad_for_levels(&raw, settings.max_levels);
        let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
        let active = crate::solve::active_nodes(&grid);
        let mut fixed = Vec::new();
        for z in 0..mz {
            for y in 0..my {
                let n = (z * my + y) * mx;
                if active[n] {
                    fixed.push(n as u32);
                }
            }
        }
        let problem = NodeProblem { fixed, ..Default::default() };

        let budget = 0.25;
        let params = OptimizeParams {
            objective: Objective::MaxFundamental,
            budget,
            exponent: 1.5,
            coeff: 1.0,
            floor: 0.10,
            cap: 0.70,
            max_iter: 40,
            ..Default::default()
        };
        let spec = FreqSpec { density, ..Default::default() };
        let mut slot = None;
        let res = optimize_cached(
            &mut slot,
            &grid,
            levels,
            &problem,
            &settings,
            &params,
            None,
            None,
            &LoadSet::default(),
            Some(&spec),
            |_, _, _| {},
        )
        .expect("frequency optimization");

        // Measure both fields cold, through the same path, at the same mass.
        let f1_of = |x: &[f64]| -> f64 {
            let eps = build_eps(
                &grid,
                &res.skin_cells,
                &res.design_cells,
                &res.skin_frac,
                x,
                params.exponent,
                params.coeff,
            );
            let vfrac = build_vfrac(&grid, &res.design_cells, &res.skin_frac, x);
            let mut cache = SolverCache::build(&grid, levels, &problem, &settings, eps);
            analyze(&mut cache.solver, &vfrac, density, &[], &ModalConfig::new(1), |_, _, _| {})
                .unwrap()
                .freqs_hz[0]
        };

        // Equal mass by construction: the uniform field is the optimized one's
        // OWN achieved mean, not the requested budget (the OC bisection lands
        // near but not exactly on target, and comparing against the request
        // would quietly hand one design a mass advantage).
        let w: Vec<f64> = res
            .design_cells
            .iter()
            .zip(&res.skin_frac)
            .map(|(&c, &f)| grid.scale[c as usize] as f64 * (1.0 - f as f64))
            .collect();
        let w_sum: f64 = w.iter().sum();
        let mean: f64 = w.iter().zip(&res.x).map(|(&wk, &xk)| wk * xk).sum::<f64>() / w_sum;
        let f_opt = f1_of(&res.x);
        let f_uni = f1_of(&vec![mean; res.x.len()]);

        eprintln!(
            "[freq-opt] {} design cells, {} iters (converged={}), mean infill {:.3} \
             (budget {:.3})\n  f1 optimized {:.2} Hz vs uniform {:.2} Hz — {:+.1}%",
            res.design_cells.len(),
            res.iterations,
            res.converged,
            mean,
            budget,
            f_opt,
            f_uni,
            (f_opt / f_uni - 1.0) * 100.0
        );

        assert!(
            (mean - budget).abs() < 0.02,
            "mass budget not held: mean infill {mean:.4} vs budget {budget:.4}"
        );
        assert!(
            f_opt > f_uni,
            "frequency optimization did not beat uniform at equal mass: \
             {f_opt:.2} Hz vs {f_uni:.2} Hz"
        );
        // The optimizer's own tracked f1 must agree with an independent cold
        // re-analysis of the same field — a warm block that drifted off the
        // true subspace would show up as a gap here.
        let rel = (res.f1_hz - f_opt).abs() / f_opt;
        assert!(
            rel < 0.02,
            "reported f1 {:.3} Hz disagrees with a cold re-analysis {f_opt:.3} Hz ({:.2}%)",
            res.f1_hz,
            rel * 100.0
        );
    }

    /// The M1 anchor: `dλ_KS/dx` against a central finite difference of the
    /// actual KS value from two full modal analyses.
    ///
    /// This is the test that earns the objective its trust. The sensitivity is
    /// a DIFFERENCE of two comparable terms (stiffness up, mass down), so the
    /// failure mode is not a crash — it is a gradient that points somewhere
    /// slightly wrong and silently converges to a worse design. Dropping the
    /// `−λ·φᵀM'φ` mass term entirely still produces a plausible-looking
    /// optimization; only an FD check catches it.
    ///
    /// Run at two aggregation sharpnesses: κ = 2 spreads real weight across all
    /// four modes (exercising the KS chain rule), κ = KS_KAPPA is the shipping
    /// value where mode 1 dominates.
    #[test]
    fn ks_sensitivity_matches_finite_difference() {
        use crate::eps::{build_eps, build_vfrac};
        use crate::fem::ke_hex;
        use crate::modal::{analyze, ModalConfig};
        use crate::solve::{NodeProblem, SolveSettings, SolverCache};
        use crate::voxel::VoxelGrid;

        // Non-square 3x4 section so modes 1 and 2 are cleanly separated — a
        // degenerate pair would make the INDIVIDUAL mode shapes non-unique
        // (any rotation within the eigenspace), which the aggregate handles but
        // which would make this test's per-mode intermediate values ambiguous.
        // Degeneracy is covered by `ks_splits_weight_across_a_degenerate_pair`.
        let (nx, ny, nz, h) = (8usize, 3usize, 4usize, 1.0f64);
        let (e0, nu, exp, coeff) = (2000.0f64, 0.3f64, 1.5f64, 1.0f64);
        let density = 1.24e-9f64; // PLA, tonne/mm³
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let (mx, my) = (nx + 1, ny + 1);
        // Cantilever: pin the whole root plane.
        let mut fixed = Vec::new();
        for z in 0..nz + 1 {
            for y in 0..ny + 1 {
                fixed.push(((z * my + y) * mx) as u32);
            }
        }
        let problem = NodeProblem { fixed, ..Default::default() };
        let settings = SolveSettings { e0, nu, ..Default::default() };

        let design: Vec<u32> = (0..grid.cell_count() as u32).collect();
        let skin: Vec<u32> = Vec::new();
        let skin_frac = vec![0f32; design.len()];
        // Every cell fully occupied and no wall band ⇒ w = 1 throughout.
        let w = vec![1.0f64; design.len()];
        let ke64 = ke_hex(e0, nu, h);

        // Tight modal analysis: the FD signal here is a ~1e-4 relative change in
        // λ, so the default 1e-4 eigenvalue-stabilization stop would be pure
        // noise at that scale.
        let cfg = ModalConfig {
            num_modes: KS_MODES,
            max_iters: 4000,
            tol: 1e-11,
            eig_tol: 1e-13,
        };

        // λ at a density field — one full modal analysis.
        let lambdas_at = |xv: &[f64]| -> Vec<f64> {
            let eps = build_eps(&grid, &skin, &design, &skin_frac, xv, exp, coeff);
            let vfrac = build_vfrac(&grid, &design, &skin_frac, xv);
            let mut cache = SolverCache::build(&grid, 1, &problem, &settings, eps);
            analyze(&mut cache.solver, &vfrac, density, &[], &cfg, |_, _, _| {})
                .unwrap()
                .lambdas
        };

        for kappa in [2.0f64, KS_KAPPA] {
            let x = vec![0.5f64; design.len()];
            let eps = build_eps(&grid, &skin, &design, &skin_frac, &x, exp, coeff);
            let vfrac = build_vfrac(&grid, &design, &skin_frac, &x);
            let mut cache = SolverCache::build(&grid, 1, &problem, &settings, eps);
            let res =
                analyze(&mut cache.solver, &vfrac, density, &[], &cfg, |_, _, _| {}).unwrap();
            // The normalizing constant, frozen at the base design — the SAME
            // value must be used for both perturbed evaluations, or the FD
            // measures a different function than the one differentiated.
            let lref = res.lambdas[0];
            let agg = ks_aggregate(&res.lambdas, kappa, lref);
            let ks_at = |xv: &[f64]| ks_aggregate(&lambdas_at(xv), kappa, lref).value;

            let law = DesignLaw {
                w: &w,
                x_phys: &x,
                exponent: exp,
                coeff,
                density,
                cell_vol: h * h * h,
            };
            let mut sens = vec![0f64; design.len()];
            ks_sensitivity(
                &cache.solver.levels[0],
                &ke64,
                &res.shapes,
                &res.lambdas,
                &agg.weights,
                &design,
                &law,
                &mut sens,
            );

            // Check the cell the optimizer would move hardest — where a wrong
            // gradient does the most damage — and, separately, the most
            // NEGATIVE one, which only exists because of the mass term and so
            // is the direct guard on it.
            let k_max = (0..sens.len()).max_by(|&a, &b| sens[a].total_cmp(&sens[b])).unwrap();
            let k_min = (0..sens.len()).min_by(|&a, &b| sens[a].total_cmp(&sens[b])).unwrap();
            assert!(
                sens[k_min] < 0.0,
                "κ={kappa}: no cell has a negative dλ/dx — the mass term is missing or \
                 mis-signed (min {:.4e}, max {:.4e})",
                sens[k_min],
                sens[k_max]
            );

            for k in [k_max, k_min] {
                let d = 1e-2;
                let mut xp = x.clone();
                xp[k] += d;
                let mut xm = x.clone();
                xm[k] -= d;
                let fd = (ks_at(&xp) - ks_at(&xm)) / (2.0 * d);
                let rel = (sens[k] - fd).abs() / fd.abs().max(1e-30);
                assert!(
                    rel < 0.02,
                    "κ={kappa}: dλ_KS/dx at cell {k} disagrees with FD: \
                     analytic {:.6e}, fd {fd:.6e} (rel {rel:.4})",
                    sens[k]
                );
            }
        }
    }
}
