// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! The full optimization PIPELINE: the goal-match budget secant, density
//! binning, the binned verification solve, the uniform/solid reference solves,
//! and watertight region extraction — everything between "optimize this design"
//! and "here are the numbers + meshes the UI shows".
//!
//! This used to live inside the `#[wasm_bindgen] Model::optimize` method, where
//! it was a ~400-line algorithm trapped behind a JS boundary: untestable in
//! native Rust and impossible to reuse from a CLI / batch tool. It now lives
//! here as [`run_optimization`], a plain function over a generic progress
//! callback; the wasm layer is a thin adapter that resolves params, marshals
//! the per-iteration callback to `js_sys`, and serializes [`OptOutcome`].
//!
//! The matrix-free/SIMD/multigrid fast path is untouched — this orchestrates
//! the same `simp`/`bins`/solver calls the wasm method made, in the same order.

use crate::bins::{
    assign_bins_mass, cleanup_small_regions, cluster_levels, constrained_smooth,
    extract_region_smooth, smooth_design_field, smooth_passes_for_radius, taubin_smooth,
    RegionMesh,
};
use crate::eps::{build_eps, build_vfrac};
use crate::simp::{
    classify_cells, design_split, evaluate_cached, evaluate_cached_stats, optimize_cached,
    step_displacement, FreqSpec, LoadSet, OptimizeError, OptimizeParams, OptimizeProgress,
    OptimizeResult,
};
use crate::solve::{NodeProblem, SolveSettings, SolverCache};
use crate::strength::{self, StrengthSpec};
use crate::voxel::VoxelGrid;

/// How many constrained-Laplacian passes one region-smoothing slider unit
/// buys. The slider tops out at 40, so the max is 40·`SMOOTH_PASS_MULT`
/// diffusion sweeps — reach ≈ √(0.5·160) ≈ 9 cells, enough to melt even wide
/// shallow-slope terraces inside the `SMOOTH_CLAMP_H` constraint band.
pub const SMOOTH_PASS_MULT: usize = 4;

/// Displacement clamp of the region smoother, in units of the voxel pitch h.
/// The binary-extraction staircase deviates from the ideal smooth surface by
/// at most ~h/2, so 0.6·h lets terraces flatten completely while bounding any
/// real-geometry drift (shrinkage, thin-member collapse, edge rounding) to
/// sub-voxel — the dimensional error of the export stays below one cell.
pub const SMOOTH_CLAMP_H: f64 = 0.6;

/// Stiffness-match secant: at most this many warm-started passes.
const MAX_MATCH_PASSES: usize = 5;
/// Match tolerance: stop when the binned compliance is within this of target.
const MATCH_TOL: f64 = 0.02;

/// Strength goal (DESIGN §17): at most this many warm-started secant passes
/// on the budget.
const MAX_STRENGTH_PASSES: usize = 5;
/// Strength accept band, ABOVE the target only (§17 dec. 3): a pass is done
/// when target ≤ SF_crit ≤ target·(1 + band). Below target is never accepted —
/// the delivered design is the lightest FEASIBLE pass seen (the all-at-cap
/// pre-flight guarantees one exists whenever the loop runs at all).
const STRENGTH_BAND: f64 = 0.05;

/// Calibrated stiffness law E/E0 = `coeff`·ρ^`exp` used for EVALUATION and
/// binning (distinct from the optimizer's possibly-penalized law in
/// `OptimizeParams`). Linear (1,1) in solid-topology mode.
#[derive(Clone, Copy, Debug)]
pub struct EvalLaw {
    pub exp: f64,
    pub coeff: f64,
}

/// Strength goal (DESIGN §17): minimize material such that the safety-factor
/// criterion SF_crit — the lowest smoothed scored cell — stays at or above
/// `target` on every included load step. Mutually exclusive with `goal_match`.
#[derive(Clone, Copy, Debug)]
pub struct StrengthGoal {
    /// Required minimum SF_crit (§17 dec. 1; UI default 2.0).
    pub target: f64,
    /// Which SF measure + solid allowables (§17 dec. 2).
    pub spec: StrengthSpec,
}

/// Fundamental frequency (Hz) of an explicit density field — the §26 goal's
/// verification measure, and the counterpart of `evaluate_cached` for the
/// frequency objective.
///
/// Cold-started deliberately: this runs once per reported design, on a field
/// (the BINNED one) that the optimizer's warm block was never converged
/// against, so reusing that block would risk reporting a subspace that never
/// re-converged. Correctness over the ~3x here; it is not in the loop.
#[allow(clippy::too_many_arguments)]
fn modal_f1(
    slot: &mut Option<SolverCache>,
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    skin: &[u32],
    design: &[u32],
    skin_frac: &[f32],
    x: &[f64],
    exp: f64,
    coeff: f64,
    fs: &FreqSpec,
) -> Result<f64, OptimizeError> {
    let eps = crate::simp::build_eps(grid, skin, design, skin_frac, x, exp, coeff);
    let vfrac = crate::simp::build_vfrac(grid, design, skin_frac, x);
    let cache = SolverCache::prepare(slot, grid, levels, problem, settings, eps.clone());
    cache.solver.update_eps(eps);
    let cfg = crate::modal::ModalConfig::new(fs.resolved_modes());
    let res = crate::modal::analyze(
        &mut cache.solver,
        &vfrac,
        fs.density,
        &fs.extra_mass,
        &cfg,
        |_, _, _| {},
    )?;
    Ok(res.freqs_hz[0])
}

/// Knobs that shape the pipeline beyond the per-iteration `OptimizeParams`.
pub struct PipelineCfg<'a> {
    pub eval: EvalLaw,
    /// "match uniform stiffness" goal: walk the budget so the binned design is
    /// as stiff as a uniform `ref_frac` print, at minimum mass.
    pub goal_match: bool,
    /// "reach safety factor" goal (§17): `Some` walks the budget so the binned
    /// design meets the SF target at minimum mass. None = budget/match.
    pub strength: Option<StrengthGoal>,
    /// "maximize fundamental frequency" goal (§26): `Some` runs the frequency
    /// objective at the user's FIXED budget. Unlike match and strength this
    /// walks no budget — the mass constraint is the user's number and the
    /// objective is what changes — so it needs exactly one pass.
    ///
    /// Mutually exclusive with `goal_match` and `strength`: all three would
    /// otherwise try to own the budget.
    pub freq: Option<&'a FreqSpec>,
    /// Reference uniform infill fraction — the match target AND the mass
    /// baseline ("vs X% uniform").
    pub ref_frac: f64,
    /// Number of density bins (auto level placement) when `levels_pct` is None.
    pub n_bins: usize,
    /// Manual level override (already clamped/sorted/deduped), e.g. binary
    /// {floor, 1} or user-calibrated densities; None ⇒ auto cluster.
    pub levels_pct: Option<&'a [f64]>,
    /// Region-smoothing slider units (× `SMOOTH_PASS_MULT` constrained passes).
    pub smooth_iters: usize,
    /// BC singularity exclusion (DESIGN §20 dec. 5/7), per padded grid cell —
    /// `strength::bc_exclusion` over the rigid constraint patches of EVERY
    /// included load step. Folded into the criterion mask, so the SF-target
    /// goal, its binding-region view and the §20 settings sweep all score the
    /// same cells. Empty ⇒ no exclusion (the pre-§20 criterion).
    pub bc_excl: &'a [bool],
}

/// Per-iteration update handed to the caller's progress callback. Carries the
/// solver's [`OptimizeProgress`] plus the secant-pass context the live UI shows.
pub struct IterUpdate<'a> {
    pub progress: &'a OptimizeProgress,
    pub pass: usize,
    pub passes: usize,
    pub budget: f64,
}

/// Coarse pipeline stage, reported through the `status` callback so a UI can
/// say what the engine is doing during the long stretches that emit no
/// per-iteration progress (the post-optimization verification, baseline solves
/// and region extraction otherwise look like a hang).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelinePhase {
    /// Tight uniform solve that sets the goal-match stiffness target.
    ReferenceSolve,
    /// Strength goal: the all-designable-at-cap pre-flight solve that decides
    /// feasibility up front (§17 dec. 6).
    Preflight,
    /// Strength goal: per-step SF_crit evaluation of a binned design (extra
    /// load steps re-solve here, so it can take a solve's worth of time).
    SfEval,
    /// A SIMP optimization pass is about to start (its first iteration carries
    /// a full cold/warm solve, so this fires well before the first progress).
    OptimizePass { pass: usize, passes: usize },
    /// Clustering the continuous densities into printable levels.
    Binning,
    /// Verification solve of the binned design.
    VerifySolve,
    /// Equal-mass uniform baseline solve (comparison card / Results roster).
    UniformSolve,
    /// Fully-solid baseline solve.
    SolidSolve,
    /// Stress-recovery fields for the roster results.
    StressRecovery,
    /// Watertight region extraction (marching tets per level).
    Regions,
    /// Region surface smoothing.
    Smoothing,
}

/// Everything the front end needs after a run: the binned design, the
/// verification + reference compliances, the deformed field, watertight
/// regions, and the volume components for mass (the caller multiplies by
/// cell volume × material density).
pub struct OptOutcome {
    pub design_cells: Vec<u32>,
    /// Always-solid cells (skin in infill modes; frozen load/support anchors in
    /// solid mode) — the export's `anchor_cells`.
    pub skin_cells: Vec<u32>,
    /// Continuous (filtered) optimized density per design cell.
    pub x_cont: Vec<f64>,
    pub centers: Vec<f64>,
    pub bins: Vec<u8>,
    pub x_binned: Vec<f64>,
    /// Summed optimizer iterations across all secant passes (summary count).
    pub total_iters: usize,
    /// Optimizer iterations of the FINAL pass (the deformed-view Solution count).
    pub design_iters: usize,
    pub design_converged: bool,
    pub effective_budget: f64,
    /// Real convergence of the binned verification solve (not assumed).
    pub verify_converged: bool,
    pub verify_residual: f64,
    pub c_binned: f64,
    pub c_uniform: f64,
    /// §26 frequency goal (all 0.0 when the goal is off): fundamental frequency
    /// in Hz of, respectively, the optimizer's final CONTINUOUS design, the
    /// delivered BINNED design, and an equal-mass UNIFORM print.
    ///
    /// `f1_binned` is the number to show — `f1_design` is unprintable and
    /// `f1_binned / f1_uniform` is what optimizing actually bought.
    pub f1_design: f64,
    pub f1_binned: f64,
    pub f1_uniform: f64,
    pub c_solid: f64,
    /// Goal-match target compliance (0.0 when not matching).
    pub c_target: f64,
    pub u_binned: Vec<f64>,
    pub max_disp: f64,
    /// Equal-mass uniform baseline solve (displacement + stress eps + max |u|),
    /// kept so the Results view can show "the same material spread evenly".
    pub u_uniform: Vec<f64>,
    pub eps_uniform: Vec<f32>,
    pub max_disp_uniform: f64,
    /// Fully-solid (CAD-ideal) baseline solve — the same trio.
    pub u_solid: Vec<f64>,
    pub eps_solid: Vec<f32>,
    pub max_disp_solid: f64,
    /// eps the verification solve used (for stress recovery).
    pub solution_eps: Vec<f32>,
    pub mean_binned: f64,
    pub regions: Vec<RegionMesh>,
    pub regions_raw: Vec<RegionMesh>,
    /// (budget, binned compliance) of each secant pass.
    pub pass_trace: Vec<(f64, f64)>,
    // ---- strength goal outputs (DESIGN §17; zero/empty for budget/match) ----
    /// Required SF_crit (0.0 when the strength goal is off).
    pub sf_target: f64,
    /// SF_crit of the DELIVERED binned design (min over included load steps).
    pub sf_crit: f64,
    /// SF_crit of the all-at-cap pre-flight — the best any layout can do.
    pub sf_crit_cap: f64,
    /// Pre-flight verdict: false = target unreachable even at the cap; the
    /// delivered design IS the all-at-cap design (§17 dec. 6).
    pub sf_feasible: bool,
    /// Per-step SF_crit of the delivered design (primary first, then the
    /// extra cases in `LoadSet` order) — the envelope's members.
    pub sf_per_step: Vec<f64>,
    /// (budget, SF_crit) of every strength evaluation, pre-flight first.
    pub sf_trace: Vec<(f64, f64)>,
    /// Cells (padded grid) whose smoothed envelope SF lies below the target on
    /// the delivered design — the binding region (one-click view / diagnosis).
    pub binding_cells: Vec<u32>,
    /// Volume share of the binding region inside skin cells: ≳0.5 reads
    /// "skin-limited" (infill can't fix it), else "interior-at-cap".
    pub binding_skin_share: f64,
    // ---- volume components (occupancy-weighted); mass = (skin+wall+infill)·h³·ρ ----
    pub vol_skin: f64,
    /// Wall-band volume inside design cells (composite skin's solid share).
    pub sum_f: f64,
    /// Infill-capable design volume (occupancy × (1 − wall fraction)).
    pub w_sum: f64,
    /// Infill volume actually placed by the binned design.
    pub infill_vol_binned: f64,
}

/// One deliverable design: everything a secant pass (or the strength
/// pre-flight) produces that the post-loop pipeline consumes. The strength
/// goal keeps the lightest FEASIBLE one as its delivery fallback; budget and
/// match construct it once at their break site (sf fields empty).
struct Snapshot {
    budget: f64,
    result: OptimizeResult,
    centers: Vec<f64>,
    bins: Vec<u8>,
    x_binned: Vec<f64>,
    c_binned: f64,
    u_binned: Vec<f64>,
    stats: crate::mg::SolveStats,
    /// Strength goal only: SF_crit (min over steps), per-step values, the
    /// elementwise-min folded smoothed SF field, and the criterion mask.
    sf_crit: f64,
    sf_steps: Vec<f64>,
    sf_folded: Vec<f32>,
    sf_mask: Vec<bool>,
}

impl Snapshot {
    /// A budget/match delivery (no strength fields).
    #[allow(clippy::too_many_arguments)]
    fn plain(
        budget: f64,
        result: OptimizeResult,
        centers: Vec<f64>,
        bins: Vec<u8>,
        x_binned: Vec<f64>,
        c_binned: f64,
        u_binned: Vec<f64>,
        stats: crate::mg::SolveStats,
    ) -> Self {
        Snapshot {
            budget,
            result,
            centers,
            bins,
            x_binned,
            c_binned,
            u_binned,
            stats,
            sf_crit: 0.0,
            sf_steps: Vec::new(),
            sf_folded: Vec::new(),
            sf_mask: Vec::new(),
        }
    }
}

/// SF_crit of ONE design over every included load step (DESIGN §17 dec. 4/5):
/// per-cell SF (display math) → masked nodal smoothing → minimum over the
/// scored cells, per step; the scalar is the min over steps (every step
/// must meet the target). The primary step reuses `u_primary` (already solved
/// for exactly this `x`); extra steps re-solve cold, each with its own
/// self-weight when the case carries one. Returns
/// (min SF_crit, per-step SF_crit, folded elementwise-min smoothed SF, mask).
#[allow(clippy::too_many_arguments)]
fn strength_eval(
    grid: &VoxelGrid,
    levels: usize,
    settings: &SolveSettings,
    loads: &LoadSet,
    skin: &[u32],
    design_cells: &[u32],
    skin_frac: &[f32],
    x: &[f64],
    eval_exp: f64,
    eval_coeff: f64,
    solid_mode: bool,
    spec: &StrengthSpec,
    bc_excl: &[bool],
    u_primary: &[f64],
) -> Result<(f64, Vec<f64>, Vec<f32>, Vec<bool>), OptimizeError> {
    let eps = build_eps(grid, skin, design_cells, skin_frac, x, eval_exp, eval_coeff);
    let mask = strength::criterion_mask(grid, design_cells, x, solid_mode, bc_excl);
    let vfrac = if loads.has_self_weight() {
        build_vfrac(grid, design_cells, skin_frac, x)
    } else {
        Vec::new()
    };
    let mut folded: Vec<f32> = Vec::new();
    let fold_step = |u64: &[f64], folded: &mut Vec<f32>| -> f64 {
        let u: Vec<f32> = u64.iter().map(|&v| v as f32).collect();
        let cells = strength::sf_cells(grid, &u, settings.e0, settings.nu, &eps, &mask, spec);
        let sm = strength::smooth_masked(grid, &cells, &mask);
        let crit = strength::sf_min(grid, &sm, &mask);
        if folded.is_empty() {
            *folded = sm;
        } else {
            for (f, v) in folded.iter_mut().zip(&sm) {
                *f = f.min(*v);
            }
        }
        crit
    };
    let mut per_step = vec![fold_step(u_primary, &mut folded)];
    for (j, (p, _w)) in loads.extra.iter().enumerate() {
        let u_j = step_displacement(
            grid, levels, p, settings, eps.clone(), loads.extra_body(j), &vfrac,
        )?;
        per_step.push(fold_step(&u_j, &mut folded));
    }
    let crit_min = per_step.iter().copied().fold(f64::INFINITY, f64::min);
    Ok((crit_min, per_step, folded, mask))
}

/// Run the optimization + binning + verification + reference solves + region
/// extraction. `progress` is called once per inner SIMP iteration with the
/// solver progress and the current secant pass; the caller marshals it (e.g.
/// to a JS callback / live preview). `status` is called once at every stage
/// boundary so the UI can narrate the otherwise-silent stretches.
#[allow(clippy::too_many_arguments)]
pub fn run_optimization(
    slot: &mut Option<SolverCache>,
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    params: &OptimizeParams,
    cfg: &PipelineCfg,
    loads: &LoadSet,
    mut progress: impl FnMut(&IterUpdate, &[f64], &[u32]),
    mut status: impl FnMut(PipelinePhase),
) -> Result<OptOutcome, OptimizeError> {
    let solid = params.solid_mode;
    let (eval_exp, eval_coeff) = (cfg.eval.exp, cfg.eval.coeff);

    // ---- goal handling ----
    // "match": one tight uniform solve at ref_frac sets the target compliance;
    // a guarded secant then walks the budget until the BINNED design lands
    // within tolerance, each pass warm-started from the previous design.
    // "strength" (§17): an all-at-cap pre-flight decides feasibility, then the
    // same secant machinery walks the budget against SF_crit instead.
    // "frequency" (§26): no budget walk at all — one pass at the user's budget
    // with the eigenvalue objective in place of compliance.
    debug_assert!(
        (cfg.goal_match as u8) + (cfg.strength.is_some() as u8) + (cfg.freq.is_some() as u8) <= 1,
        "match, strength and frequency goals are mutually exclusive (each owns the budget)"
    );
    debug_assert_eq!(
        cfg.freq.is_some(),
        params.objective == crate::simp::Objective::MaxFundamental,
        "PipelineCfg::freq and OptimizeParams::objective must agree"
    );
    let strength_goal = cfg.strength.as_ref();
    let max_passes = if cfg.goal_match {
        MAX_MATCH_PASSES
    } else if strength_goal.is_some() {
        MAX_STRENGTH_PASSES
    } else {
        1
    };
    let mut c_target = 0.0f64;
    if cfg.goal_match {
        status(PipelinePhase::ReferenceSolve);
        let split = classify_cells(
            grid,
            params.wall_mm,
            params.top_mm,
            params.bottom_mm,
            params.composite_skin,
        );
        if split.design.is_empty() {
            return Err(OptimizeError::NoInterior);
        }
        let x_ref = vec![cfg.ref_frac; split.design.len()];
        let (c_ref, _, _) = evaluate_cached(
            slot, grid, levels, problem, settings, &split.skin, &split.design, &split.skin_frac,
            &x_ref, eval_exp, eval_coeff, loads,
        )?;
        c_target = c_ref;
    }

    // ---- strength pre-flight (§17 dec. 6) ----
    // One tight solve with EVERY designable cell at the cap — the best any
    // layout can reach. Below target ⇒ infeasible: skip the loop entirely and
    // deliver this cap design (with diagnosis). Feasible ⇒ it seeds the secant
    // trace as the guaranteed-feasible upper bracket AND the delivery
    // fallback, so the solve is never wasted.
    let mut sf_trace: Vec<(f64, f64)> = Vec::new();
    let mut sf_cap_crit = 0.0f64;
    let mut sf_feasible = true;
    let mut best_feasible: Option<Snapshot> = None;
    if let Some(sg) = strength_goal {
        status(PipelinePhase::Preflight);
        let split = design_split(grid, params, problem, loads);
        if split.design.is_empty() {
            return Err(OptimizeError::NoInterior);
        }
        let x_cap = vec![params.cap; split.design.len()];
        let (c_cap, _maxd, u_cap, stats_cap) = evaluate_cached_stats(
            slot, grid, levels, problem, settings, &split.skin, &split.design, &split.skin_frac,
            &x_cap, eval_exp, eval_coeff, loads,
        )?;
        status(PipelinePhase::SfEval);
        let (crit, steps, folded, mask) = strength_eval(
            grid, levels, settings, loads, &split.skin, &split.design, &split.skin_frac, &x_cap,
            eval_exp, eval_coeff, solid, &sg.spec, cfg.bc_excl, &u_cap,
        )?;
        sf_cap_crit = crit;
        sf_feasible = crit >= sg.target;
        sf_trace.push((params.cap, crit));
        // Cap delivery: one printable level at the cap (solid mode: the full
        // part) — bins/regions/baselines flow through the normal tail.
        let centers = if solid { vec![0.0, 1.0] } else { vec![params.floor, params.cap] };
        let bins = vec![1u8; split.design.len()];
        let result = OptimizeResult {
            x: x_cap.clone(),
            design_cells: split.design,
            skin_cells: split.skin,
            skin_frac: split.skin_frac,
            effective_budget: params.cap,
            iterations: 0,
            converged: true,
            compliance: c_cap,
            f1_hz: 0.0,
            u: u_cap.clone(),
            mode1: Vec::new(),
            se: vec![0.0; bins.len()],
        };
        best_feasible = Some(Snapshot {
            budget: params.cap,
            result,
            centers,
            bins,
            x_binned: x_cap,
            c_binned: c_cap,
            u_binned: u_cap,
            stats: stats_cap,
            sf_crit: crit,
            sf_steps: steps,
            sf_folded: folded,
            sf_mask: mask,
        });
    }

    let mut pass_no = 1usize;
    let mut budget_k = if cfg.goal_match {
        // Optimized designs match uniform stiffness at ~70–85% of the mass —
        // start the search there.
        (cfg.ref_frac * 0.8).max(params.floor)
    } else if let Some(sg) = strength_goal {
        // First guess from the first-order power law SF(b) ≈ SF_cap·(b/cap)^n
        // (Gibson–Ashby: strength tracks stiffness), aimed at the middle of
        // the accept band. Degenerate when the cap design sits AT the SF cap
        // (flat spot) — the guarded secant/bisection recovers from that.
        let aim = sg.target * (1.0 + 0.5 * STRENGTH_BAND);
        (params.cap * (aim / sf_cap_crit.max(1e-9)).powf(1.0 / eval_exp.max(0.5)))
            .clamp(params.floor, params.cap)
    } else {
        params.budget
    };
    let (mut lo_b, mut hi_b) = if strength_goal.is_some() {
        (params.floor, params.cap)
    } else {
        (params.floor, cfg.ref_frac)
    };
    let mut warm_x: Option<Vec<f64>> = None;
    let mut warm_u: Option<Vec<f64>> = None;
    let mut pass_trace: Vec<(f64, f64)> = Vec::new();
    let mut total_iters = 0usize;

    let delivered: Snapshot = 'deliver: {
        // Infeasible strength target: no layout can reach it — deliver the
        // all-at-cap design straight away (§17 dec. 6), no optimization loop.
        if strength_goal.is_some() && !sf_feasible {
            break 'deliver best_feasible.take().unwrap();
        }
        loop {
        let params_k = OptimizeParams { budget: budget_k, ..*params };
        let pass = pass_no;
        let budget = budget_k;
        status(PipelinePhase::OptimizePass { pass, passes: max_passes });
        let result = optimize_cached(
            slot,
            grid,
            levels,
            problem,
            settings,
            &params_k,
            warm_x.as_deref(),
            warm_u.as_deref(),
            loads,
            cfg.freq,
            |p, x_phys, design_cells| {
                let upd = IterUpdate { progress: p, pass, passes: max_passes, budget };
                progress(&upd, x_phys, design_cells);
            },
        )?;
        total_iters += result.iterations;

        // ---- bins ----
        // SOLID topology: two levels {void, solid}, thresholded at ρ = 0.5, then
        // floating islands (not connected to a frozen load/support cell) dropped.
        // Infill modes: floor-pinned, strain-energy-weighted level placement
        // (or the manual override) + a mass-true bin assignment.
        status(PipelinePhase::Binning);
        let (centers, bins): (Vec<f64>, Vec<u8>) = if solid {
            (
                vec![0.0, 1.0],
                solid_keep_bins(grid, &result.skin_cells, &result.design_cells, &result.x, 0.5),
            )
        } else {
            // Strain energy smoothed at the density-filter radius: level
            // placement and the idle-cell gate then carry the same length
            // scale as the minimum member size, free of per-cell speckle.
            let r_cells = crate::simp::filter_radius_cells(params.min_member_mm, grid.h);
            let se_s = smooth_design_field(
                grid,
                &result.design_cells,
                &result.se,
                smooth_passes_for_radius(r_cells),
            );
            let centers: Vec<f64> = match cfg.levels_pct {
                Some(user) if !user.is_empty() => user.to_vec(),
                _ => cluster_levels(
                    &result.x, &se_s, cfg.n_bins, eval_exp, eval_coeff, params.floor, params.cap,
                ),
            };
            let target_mean = result.x.iter().sum::<f64>() / result.x.len().max(1) as f64;
            let mut bins =
                assign_bins_mass(&result.x, &se_s, &centers, eval_exp, eval_coeff, target_mean);
            // Cleanup floor: whichever is larger of the relative-size floor and
            // the volume of the smallest legitimate member (sphere of diameter
            // 2r, ≈ 4.2·r³ cells) — regions under the member size can't print.
            let member_cells = (4.2 * r_cells.powi(3)).ceil() as usize;
            let min_cells = (result.design_cells.len() / 500).max(30).max(member_cells);
            cleanup_small_regions(grid, &result.design_cells, &mut bins, centers.len(), min_cells);
            (centers, bins)
        };
        let x_binned: Vec<f64> = bins.iter().map(|&b| centers[b as usize]).collect();

        // Verification solve of the binned design (calibrated law), warm-started
        // from the optimizer's displacement; real convergence stats kept.
        status(PipelinePhase::VerifySolve);
        let (c_b, _maxd, u_b, stats_b) = evaluate_cached_stats(
            slot, grid, levels, problem, settings, &result.skin_cells, &result.design_cells,
            &result.skin_frac, &x_binned, eval_exp, eval_coeff, loads,
        )?;
        pass_trace.push((budget_k, c_b));

        // ---- strength goal arm (§17 dec. 3): budget secant against SF_crit ----
        if let Some(sg) = strength_goal {
            status(PipelinePhase::SfEval);
            let (crit, steps, folded, mask) = strength_eval(
                grid, levels, settings, loads, &result.skin_cells, &result.design_cells,
                &result.skin_frac, &x_binned, eval_exp, eval_coeff, solid, &sg.spec, cfg.bc_excl,
                &u_b,
            )?;
            sf_trace.push((budget_k, crit));
            let feasible_now = crit >= sg.target;
            if feasible_now {
                hi_b = hi_b.min(budget_k);
            } else {
                lo_b = lo_b.max(budget_k);
            }
            // Keep the lightest feasible design seen — the delivery. Below
            // target is never delivered (the band sits ABOVE target only),
            // and the cap pre-flight guarantees a feasible fallback exists.
            let lighter = best_feasible.as_ref().map_or(true, |b| budget_k <= b.budget);
            if feasible_now && lighter {
                best_feasible = Some(Snapshot {
                    budget: budget_k,
                    result: result.clone(),
                    centers: centers.clone(),
                    bins: bins.clone(),
                    x_binned: x_binned.clone(),
                    c_binned: c_b,
                    u_binned: u_b.clone(),
                    stats: stats_b.clone(),
                    sf_crit: crit,
                    sf_steps: steps,
                    sf_folded: folded,
                    sf_mask: mask,
                });
            }
            let in_band = feasible_now && crit <= sg.target * (1.0 + STRENGTH_BAND);
            let at_floor = feasible_now && budget_k <= params.floor + 1e-9;
            if in_band || at_floor || pass_no >= max_passes || hi_b - lo_b < 0.005 {
                break 'deliver best_feasible.take().unwrap();
            }
            // Guarded secant on the last two evaluations, aimed at the band
            // midpoint; bisection fallback inside the feasibility bracket.
            let n = sf_trace.len();
            let aim = sg.target * (1.0 + 0.5 * STRENGTH_BAND);
            let (b1, c1) = sf_trace[n - 2];
            let (b2, c2) = sf_trace[n - 1];
            let mut next = if (c1 - c2).abs() > 1e-12 {
                b2 + (aim - c2) * (b1 - b2) / (c1 - c2)
            } else {
                0.5 * (lo_b + hi_b)
            };
            // UNLIKE match, the lower bracket starts AT the floor without a
            // measurement there — the floor itself may well be feasible. When
            // the secant wants to go at/below an untested floor, TRY the floor
            // (the lightest legal design; the `at_floor` break then ends the
            // search) instead of bisecting above it for the remaining passes.
            let floor_untested = lo_b <= params.floor + 1e-12
                && sf_trace.iter().all(|&(b, _)| b > params.floor + 1e-9);
            if next <= params.floor + 0.002 && floor_untested {
                next = params.floor;
            } else if !(next > lo_b + 0.002 && next < hi_b - 0.002) {
                next = 0.5 * (lo_b + hi_b);
            }
            budget_k = next.clamp(params.floor, params.cap);
            warm_x = Some(result.x.clone());
            warm_u = Some(result.u.clone());
            pass_no += 1;
            continue;
        }
        if !cfg.goal_match {
            break 'deliver Snapshot::plain(
                budget_k, result, centers, bins, x_binned, c_b, u_b, stats_b,
            );
        }
        // dev > 0: too compliant (needs more material); < 0: too stiff.
        let dev = c_b / c_target - 1.0;
        if dev.abs() <= MATCH_TOL || pass_no >= max_passes || hi_b - lo_b < 0.005 {
            break 'deliver Snapshot::plain(
                budget_k, result, centers, bins, x_binned, c_b, u_b, stats_b,
            );
        }
        if dev > 0.0 {
            lo_b = lo_b.max(budget_k);
        } else {
            hi_b = hi_b.min(budget_k);
        }
        // Guarded secant on the last two passes; bisection fallback.
        let n = pass_trace.len();
        let mut next = if n >= 2 {
            let (b1, c1) = pass_trace[n - 2];
            let (b2, c2) = pass_trace[n - 1];
            if (c1 - c2).abs() > 1e-12 {
                b2 + (c_target - c2) * (b1 - b2) / (c1 - c2)
            } else {
                0.5 * (lo_b + hi_b)
            }
        } else {
            0.5 * (lo_b + hi_b)
        };
        if !(next > lo_b + 0.002 && next < hi_b - 0.002) {
            next = 0.5 * (lo_b + hi_b);
        }
        budget_k = next.clamp(params.floor, params.cap);
        warm_x = Some(result.x.clone());
        warm_u = Some(result.u.clone());
        pass_no += 1;
        }
    };

    let Snapshot {
        result,
        centers,
        bins,
        x_binned,
        c_binned,
        u_binned,
        stats: verify_stats,
        sf_crit: sf_crit_out,
        sf_steps: sf_steps_out,
        sf_folded,
        sf_mask,
        budget: _,
    } = delivered;

    // ---- binding region on the DELIVERED design (§17 dec. 6) ----
    // Cells below the SF target on the folded smoothed field. On a feasible
    // delivery this is EMPTY (SF_crit is the minimum, so nothing scored sits
    // below it); on an infeasible one it is the diagnosis: mostly-skin ⇒
    // infill can't fix it, else raise the cap.
    let (binding_cells, binding_skin_share) = if let Some(sg) = strength_goal {
        let b = strength::binding_cells(&sf_folded, &sf_mask, sg.target);
        let mut is_skin = vec![false; grid.cell_count()];
        for &c in &result.skin_cells {
            is_skin[c as usize] = true;
        }
        let v_all: f64 = b.iter().map(|&c| grid.scale[c as usize] as f64).sum();
        let v_skin: f64 = b
            .iter()
            .filter(|&&c| is_skin[c as usize])
            .map(|&c| grid.scale[c as usize] as f64)
            .sum();
        (b, if v_all > 0.0 { v_skin / v_all } else { 0.0 })
    } else {
        (Vec::new(), 0.0)
    };

    // ---- volume bookkeeping (occupancy × (1 − wall fraction)) ----
    let w_inf: Vec<f64> = result
        .design_cells
        .iter()
        .zip(&result.skin_frac)
        .map(|(&c, &f)| grid.scale[c as usize] as f64 * (1.0 - f as f64))
        .collect();
    let w_sum: f64 = w_inf.iter().sum();
    let sum_f: f64 = result
        .design_cells
        .iter()
        .zip(&result.skin_frac)
        .map(|(&c, &f)| grid.scale[c as usize] as f64 * f as f64)
        .sum();
    let sum_wx = |x: &[f64]| w_inf.iter().zip(x).map(|(&w, &v)| w * v).sum::<f64>();
    let mean_binned = sum_wx(&x_binned) / w_sum.max(1e-12);
    let vol_skin: f64 = result.skin_cells.iter().map(|&c| grid.scale[c as usize] as f64).sum();

    // Uniform + solid reference solves at a relaxed tolerance — they only feed
    // the comparison card and compliance converges faster than the residual.
    // (The cache doesn't key on tol, so warm starts survive.)
    let ref_settings = SolveSettings { tol: settings.tol.max(5e-4), ..*settings };
    status(PipelinePhase::UniformSolve);
    let x_uniform = vec![mean_binned; x_binned.len()];
    // KEEP the reference solves' displacement fields (they used to be dropped):
    // the equal-mass uniform and fully-solid baselines are surfaced as
    // selectable results in the Results view, and the optimizer already pays
    // for the solves. Their eps (for stress recovery) is rebuilt from x.
    let (c_uniform, max_disp_uniform, u_uniform) = evaluate_cached(
        slot, grid, levels, problem, &ref_settings, &result.skin_cells, &result.design_cells,
        &result.skin_frac, &x_uniform, eval_exp, eval_coeff, loads,
    )?;
    let eps_uniform = build_eps(
        grid, &result.skin_cells, &result.design_cells, &result.skin_frac, &x_uniform, eval_exp,
        eval_coeff,
    );
    // ---- §26 frequency goal: the two numbers that make the result judgeable --
    // `f1_binned` is what the user actually gets (the quantized, printable
    // design — the optimizer's continuous field is not deliverable), and
    // `f1_uniform` is the same MASS spent as uniform infill. The second is the
    // honest comparison: a high f1 means nothing without knowing what the plain
    // print at that mass already gave, and binning can give back some of the
    // gain, so reporting only the optimizer's own λ would overstate the result.
    let (f1_binned, f1_uniform) = match cfg.freq {
        Some(fs) => {
            status(PipelinePhase::VerifySolve);
            let fb = modal_f1(
                slot, grid, levels, problem, settings, &result.skin_cells, &result.design_cells,
                &result.skin_frac, &x_binned, eval_exp, eval_coeff, fs,
            )?;
            status(PipelinePhase::UniformSolve);
            let fu = modal_f1(
                slot, grid, levels, problem, settings, &result.skin_cells, &result.design_cells,
                &result.skin_frac, &x_uniform, eval_exp, eval_coeff, fs,
            )?;
            (fb, fu)
        }
        None => (0.0, 0.0),
    };

    status(PipelinePhase::SolidSolve);
    let x_solid = vec![1.0; x_binned.len()];
    let (c_solid, max_disp_solid, u_solid) = evaluate_cached(
        slot, grid, levels, problem, &ref_settings, &result.skin_cells, &result.design_cells,
        &result.skin_frac, &x_solid, eval_exp, eval_coeff, loads,
    )?;
    let eps_solid = build_eps(
        grid, &result.skin_cells, &result.design_cells, &result.skin_frac, &x_solid, eval_exp,
        eval_coeff,
    );

    // ---- deformed field + stress eps ----
    status(PipelinePhase::StressRecovery);
    let max_disp = (0..u_binned.len() / 3)
        .map(|n| {
            u_binned[3 * n] * u_binned[3 * n]
                + u_binned[3 * n + 1] * u_binned[3 * n + 1]
                + u_binned[3 * n + 2] * u_binned[3 * n + 2]
        })
        .fold(0f64, f64::max)
        .sqrt();
    let solution_eps = build_eps(
        grid, &result.skin_cells, &result.design_cells, &result.skin_frac, &x_binned, eval_exp,
        eval_coeff,
    );

    // ---- regions (bins above base) ----
    status(PipelinePhase::Regions);
    let mut bin_of_cell: std::collections::HashMap<u32, u8> = Default::default();
    for (i, &c) in result.design_cells.iter().enumerate() {
        bin_of_cell.insert(c, bins[i]);
    }
    // SOLID mode: the frozen load/support cells are part of the optimized body.
    if solid {
        for &c in &result.skin_cells {
            bin_of_cell.insert(c, 1);
        }
    }
    let mut regions_raw = Vec::new();
    for level in 1..centers.len() {
        let inside =
            |ci: usize| -> bool { bin_of_cell.get(&(ci as u32)).is_some_and(|&b| b as usize >= level) };
        let mut r = extract_region_smooth(grid, &inside, 0.4);
        if r.indices.is_empty() {
            continue;
        }
        r.density = centers[level];
        regions_raw.push(r);
    }
    status(PipelinePhase::Smoothing);
    let regions = smooth_regions(&regions_raw, cfg.smooth_iters, grid.h);

    Ok(OptOutcome {
        design_cells: result.design_cells,
        skin_cells: result.skin_cells,
        x_cont: result.x,
        centers,
        bins,
        infill_vol_binned: sum_wx(&x_binned),
        x_binned,
        total_iters,
        design_iters: result.iterations,
        design_converged: result.converged,
        effective_budget: result.effective_budget,
        verify_converged: verify_stats.converged,
        verify_residual: verify_stats.rel_residual,
        c_binned,
        c_uniform,
        f1_design: result.f1_hz,
        f1_binned,
        f1_uniform,
        c_solid,
        c_target,
        u_binned,
        max_disp,
        u_uniform,
        eps_uniform,
        max_disp_uniform,
        u_solid,
        eps_solid,
        max_disp_solid,
        solution_eps,
        mean_binned,
        regions,
        regions_raw,
        pass_trace,
        sf_target: strength_goal.map_or(0.0, |g| g.target),
        sf_crit: sf_crit_out,
        sf_crit_cap: sf_cap_crit,
        sf_feasible,
        sf_per_step: sf_steps_out,
        sf_trace,
        binding_cells,
        binding_skin_share,
        vol_skin,
        sum_f,
        w_sum,
    })
}

/// Smoothed copies of the raw regions (0 passes = verbatim copy). Exposed so
/// the post-run smoothing slider can re-smooth without re-extracting.
/// Three stages (`h` = voxel pitch the regions were extracted at):
///
/// 1. **Outlier kill** — a short Taubin pre-pass. Marching tets on a binary
///    indicator grows needle/tent spikes at single-voxel corners and ridges;
///    Taubin's band-pass melts those high-frequency outliers in a few passes
///    with no net shrink. Crucially this runs BEFORE the constraint centers
///    are captured — otherwise every spike gets a protected ball around its
///    own tip and survives the whole pipeline 0.6·h tall.
/// 2. **Terrace melt** — constrained Laplacian (clamped to `SMOOTH_CLAMP_H`·h
///    around the de-spiked reference): pure diffusion flattens the wide
///    shallow-slope terraces a Taubin band-pass preserves at any pass count,
///    while the clamp bounds shrinkage / thin-member drift to sub-voxel.
/// 3. **Polish** — two stable Taubin passes to round the C0 kinks left where
///    the clamp bound; drift is negligible at this pass count.
pub fn smooth_regions(raw: &[RegionMesh], iters: usize, h: f64) -> Vec<RegionMesh> {
    raw.iter()
        .map(|r| {
            let mut rr = RegionMesh {
                density: r.density,
                positions: r.positions.clone(),
                indices: r.indices.clone(),
            };
            if iters > 0 {
                taubin_smooth(&mut rr.positions, &rr.indices, 2 + iters / 4);
                constrained_smooth(
                    &mut rr.positions,
                    &rr.indices,
                    iters * SMOOTH_PASS_MULT,
                    (SMOOTH_CLAMP_H * h) as f32,
                );
                taubin_smooth(&mut rr.positions, &rr.indices, 2);
            }
            rr
        })
        .collect()
}

/// SOLID topology mode: threshold the optimized field at `thresh` and keep only
/// material 6-connected to a frozen load/support cell, so floating islands are
/// dropped. Returns 1 (kept solid) / 0 (void) per design slot. With no anchors,
/// keeps the single largest component. `thresh` is the export isosurface
/// density — lower keeps more material.
pub fn solid_keep_bins(
    grid: &VoxelGrid,
    skin: &[u32],
    design: &[u32],
    x: &[f64],
    thresh: f64,
) -> Vec<u8> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let n = grid.cell_count();
    let mut member = vec![false; n];
    let mut anchor = vec![false; n];
    for &c in skin {
        member[c as usize] = true;
        anchor[c as usize] = true;
    }
    for (i, &c) in design.iter().enumerate() {
        if x[i] >= thresh {
            member[c as usize] = true;
        }
    }
    // 6-connected components over member cells.
    let mut comp = vec![u32::MAX; n];
    let mut comp_keep: Vec<bool> = Vec::new();
    let mut comp_size: Vec<usize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..n {
        if !member[start] || comp[start] != u32::MAX {
            continue;
        }
        let id = comp_keep.len() as u32;
        let mut has_anchor = false;
        let mut size = 0usize;
        comp[start] = id;
        stack.push(start);
        while let Some(c) = stack.pop() {
            size += 1;
            if anchor[c] {
                has_anchor = true;
            }
            let (cx, cy, cz) = (c % nx, (c / nx) % ny, c / (nx * ny));
            let mut visit = |x: i64, y: i64, z: i64, comp: &mut [u32], stack: &mut Vec<usize>| {
                if x < 0 || y < 0 || z < 0 || x >= nx as i64 || y >= ny as i64 || z >= nz as i64 {
                    return;
                }
                let d = (z as usize * ny + y as usize) * nx + x as usize;
                if member[d] && comp[d] == u32::MAX {
                    comp[d] = id;
                    stack.push(d);
                }
            };
            let (xi, yi, zi) = (cx as i64, cy as i64, cz as i64);
            visit(xi - 1, yi, zi, &mut comp, &mut stack);
            visit(xi + 1, yi, zi, &mut comp, &mut stack);
            visit(xi, yi - 1, zi, &mut comp, &mut stack);
            visit(xi, yi + 1, zi, &mut comp, &mut stack);
            visit(xi, yi, zi - 1, &mut comp, &mut stack);
            visit(xi, yi, zi + 1, &mut comp, &mut stack);
        }
        comp_keep.push(has_anchor);
        comp_size.push(size);
    }
    // No anchors (no BCs reached the grid): fall back to the largest component.
    if !anchor.iter().any(|&a| a) {
        if let Some((bi, _)) = comp_size.iter().enumerate().max_by_key(|(_, &s)| s) {
            comp_keep.iter_mut().for_each(|k| *k = false);
            comp_keep[bi] = true;
        }
    }
    design
        .iter()
        .map(|&c| {
            let c = c as usize;
            let id = comp[c];
            if member[c] && id != u32::MAX && comp_keep[id as usize] {
                1
            } else {
                0
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::{assemble, BcKind, BcSpec};
    use crate::mesh::primitives;
    use crate::pad_for_levels;

    /// End-to-end pipeline on a tip-loaded cantilever — the whole goal-budget
    /// path (optimize → bin → verify → reference solves → regions) that used to
    /// be untestable inside the wasm boundary. Anchors the headline outputs the
    /// comparison card shows.
    #[test]
    fn graded_pipeline_beats_uniform_and_converges() {
        let beam = primitives::boxx([0.0; 3], [60.0, 10.0, 10.0]);
        let grid0 = VoxelGrid::voxelize(&beam, 1.0);
        let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
        let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
        let bcs = vec![
            BcSpec { kind: BcKind::Fixed, tris: vec![0, 1] },
            BcSpec { kind: BcKind::Force([0.0, 0.0, -30.0]), tris: vec![2, 3] },
        ];
        let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
        let params = OptimizeParams {
            budget: 0.35,
            exponent: 1.5,
            coeff: 1.0,
            wall_mm: 1.0,
            max_iter: 30,
            ..Default::default()
        };
        let cfg = PipelineCfg {
            eval: EvalLaw { exp: 1.5, coeff: 1.0 },
            goal_match: false,
            strength: None,
            freq: None,
            ref_frac: 0.35,
            n_bins: 3,
            levels_pct: None,
            smooth_iters: 4,
            bc_excl: &[],
        };
        let oc = run_optimization(
            &mut None, &grid, levels, &asm.problem, &settings, &params, &cfg, &LoadSet::default(),
            |_, _, _| {},
            |_| {},
        )
        .expect("pipeline");

        assert!(oc.centers.len() >= 2, "expected ≥2 bins, got {:?}", oc.centers);
        assert!((oc.centers[0] - 0.10).abs() < 1e-9, "bottom level pinned to floor: {:?}", oc.centers);
        assert!(oc.c_binned.is_finite() && oc.c_binned > 0.0, "sane binned compliance");
        assert!(oc.verify_converged, "binned verification solve converged");
        assert!(
            oc.c_uniform / oc.c_binned > 1.03,
            "binned design beats uniform at equal mass: C_uni/C_bin = {:.3}",
            oc.c_uniform / oc.c_binned
        );
        assert!(
            (oc.mean_binned - 0.35).abs() < 0.08,
            "mean infill near budget: {:.3}",
            oc.mean_binned
        );
        assert!(!oc.regions.is_empty(), "at least one region extracted");
        assert!(oc.max_disp > 0.0 && oc.max_disp < 50.0, "sane deflection {}", oc.max_disp);
    }

    /// STRENGTH goal (DESIGN §17): min material s.t. SF_crit ≥ target, on the
    /// tip-loaded cantilever. Three behaviours anchored:
    ///  (a) an unreachable target (beyond SF_CAP) takes the infeasible path —
    ///      no optimization loop, the all-at-cap design delivered, best
    ///      achievable + binding region reported;
    ///  (b) a reachable target delivers a design AT or ABOVE the target
    ///      (never below — the band sits above only) with LESS material than
    ///      the cap design;
    ///  (c) budget/match outputs stay inert (sf fields zero/empty) — the
    ///      byte-identity of those paths is regbench's job, this guards the
    ///      plumbing.
    #[test]
    fn strength_goal_feasible_and_infeasible() {
        let beam = primitives::boxx([0.0; 3], [60.0, 10.0, 10.0]);
        let grid0 = VoxelGrid::voxelize(&beam, 1.0);
        let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
        let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
        let bcs = vec![
            BcSpec { kind: BcKind::Fixed, tris: vec![0, 1] },
            BcSpec { kind: BcKind::Force([0.0, 0.0, -30.0]), tris: vec![2, 3] },
        ];
        let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
        let params = OptimizeParams {
            budget: 0.35,
            exponent: 1.5,
            coeff: 1.0,
            wall_mm: 1.0,
            max_iter: 30,
            ..Default::default()
        };
        let spec = crate::strength::StrengthSpec {
            measure: crate::strength::SfMeasure::Both,
            strength: 50.0,
            strength_z: 35.0,
            shear_z: 21.0,
        };
        let run = |target: f64| {
            let cfg = PipelineCfg {
                eval: EvalLaw { exp: 1.5, coeff: 1.0 },
                goal_match: false,
                strength: Some(StrengthGoal { target, spec }),
                freq: None,
                ref_frac: 0.35,
                n_bins: 3,
                levels_pct: None,
                smooth_iters: 0,
                bc_excl: &[],
            };
            run_optimization(
                &mut None, &grid, levels, &asm.problem, &settings, &params, &cfg,
                &LoadSet::default(),
                |_, _, _| {},
                |_| {},
            )
            .expect("strength pipeline")
        };

        // (a) Unreachable: SF_CAP is 10, so 50 can never be met.
        let inf = run(50.0);
        assert!(!inf.sf_feasible, "target 50 must be infeasible");
        assert!(
            inf.sf_crit_cap > 0.0 && inf.sf_crit_cap < 50.0,
            "best achievable reported: {}",
            inf.sf_crit_cap
        );
        assert_eq!(inf.design_iters, 0, "infeasible path must skip the SIMP loop");
        assert!(
            (inf.mean_binned - params.cap).abs() < 1e-6,
            "all-at-cap delivery: mean {}",
            inf.mean_binned
        );
        assert!(!inf.binding_cells.is_empty(), "binding region identified");
        assert!(
            (0.0..=1.0).contains(&inf.binding_skin_share),
            "sane skin share {}",
            inf.binding_skin_share
        );
        assert_eq!(inf.sf_per_step.len(), 1, "single load step");

        // (b) Reachable: aim well below the cap design's SF_crit.
        let target = (inf.sf_crit_cap * 0.55).max(1.1);
        let ok = run(target);
        assert!(ok.sf_feasible, "target {target:.2} must be feasible");
        assert!(
            ok.sf_crit >= target - 1e-9,
            "delivered design meets the target: SF_crit {} < {target:.2}",
            ok.sf_crit
        );
        assert!(
            ok.mean_binned < params.cap - 0.02,
            "feasible delivery saves material vs cap: mean {}",
            ok.mean_binned
        );
        assert!(ok.sf_trace.len() >= 2, "pre-flight + ≥1 pass traced");
        assert!(ok.verify_converged, "verification solve converged");

        // (c) Plain budget run: strength outputs stay inert.
        let cfg = PipelineCfg {
            eval: EvalLaw { exp: 1.5, coeff: 1.0 },
            goal_match: false,
            strength: None,
            freq: None,
            ref_frac: 0.35,
            n_bins: 3,
            levels_pct: None,
            smooth_iters: 0,
            bc_excl: &[],
        };
        let plain = run_optimization(
            &mut None, &grid, levels, &asm.problem, &settings, &params, &cfg,
            &LoadSet::default(),
            |_, _, _| {},
            |_| {},
        )
        .expect("budget pipeline");
        assert_eq!(plain.sf_target, 0.0);
        assert!(plain.sf_trace.is_empty() && plain.binding_cells.is_empty());
    }

    /// MULTI-LOAD (DESIGN §13): a cantilever loaded +Z in one case and +Y in
    /// another. Optimizing for BOTH must (a) produce a layout different from the
    /// single-Z optimum and (b) make that layout genuinely stiffer under the Y
    /// case than the Z-only optimum is — the whole point of the weighted sum.
    #[test]
    fn multiload_design_resists_both_cases() {
        let beam = primitives::boxx([0.0; 3], [40.0, 14.0, 14.0]);
        let grid0 = VoxelGrid::voxelize(&beam, 1.0);
        let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
        let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
        let asm = |dir: [f64; 3]| {
            let bcs = vec![
                BcSpec { kind: BcKind::Fixed, tris: vec![0, 1] },
                BcSpec { kind: BcKind::Force(dir), tris: vec![2, 3] },
            ];
            assemble(&beam, &grid, &bcs, None, &settings).unwrap()
        };
        let asm_z = asm([0.0, 0.0, -30.0]);
        let asm_y = asm([0.0, -30.0, 0.0]);
        let params = OptimizeParams {
            budget: 0.35,
            exponent: 1.5,
            coeff: 1.0,
            wall_mm: 1.0,
            max_iter: 45,
            ..Default::default()
        };
        let cfg = PipelineCfg {
            eval: EvalLaw { exp: 1.5, coeff: 1.0 },
            goal_match: false,
            strength: None,
            freq: None,
            ref_frac: 0.35,
            n_bins: 3,
            levels_pct: None,
            smooth_iters: 0,
            bc_excl: &[],
        };
        let run = |loads: &LoadSet| {
            run_optimization(
                &mut None, &grid, levels, &asm_z.problem, &settings, &params, &cfg, loads,
                |_, _, _| {},
                |_| {},
            )
            .expect("pipeline")
        };
        // Single-load (Z only) vs multi-load (Z + Y, equal weight).
        let a = run(&LoadSet::default());
        let b = run(&LoadSet {
            extra: vec![(asm_y.problem.clone(), 1.0)],
            primary_weight: 1.0,
            ..Default::default()
        });

        // (a) The second load case visibly redistributes material.
        let mean_delta = a
            .x_binned
            .iter()
            .zip(&b.x_binned)
            .map(|(p, q)| (p - q).abs())
            .sum::<f64>()
            / a.x_binned.len().max(1) as f64;
        assert!(mean_delta > 0.015, "multi-load layout should differ from single-Z: mean |Δ| = {mean_delta:.4}");

        // (b) The multi-load design is stiffer under the Y case than the Z-only
        // design is (infill mode → identical skin/design cells from the same
        // geometry, so this is a fair x-for-x comparison).
        let split = classify_cells(&grid, params.wall_mm, params.top_mm, params.bottom_mm, params.composite_skin);
        let cy = |x_binned: &[f64]| {
            crate::simp::evaluate(
                &grid, levels, &asm_y.problem, &settings, &split.skin, &split.design,
                &split.skin_frac, x_binned, 1.5, 1.0, None,
            )
            .unwrap()
            .0
        };
        let (ca_y, cb_y) = (cy(&a.x_binned), cy(&b.x_binned));
        assert!(cb_y < ca_y, "multi-load must resist the Y case better: C_B(Y)={cb_y:.3} vs C_A(Y)={ca_y:.3}");
    }
}
