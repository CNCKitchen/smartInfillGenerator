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
    assign_bins_mass, cleanup_small_regions, cluster_levels, extract_region, taubin_smooth,
    RegionMesh,
};
use crate::simp::{
    build_eps, classify_cells, evaluate_cached, evaluate_cached_stats, optimize_cached,
    OptimizeError, OptimizeParams, OptimizeProgress,
};
use crate::solve::{NodeProblem, SolveSettings, SolverCache};
use crate::voxel::VoxelGrid;

/// How many Taubin passes one region-smoothing slider unit buys. The slider
/// tops out at 40, so the max is 40·`SMOOTH_PASS_MULT` passes — enough, with
/// the λ/μ in `taubin_smooth`, to melt the voxel staircase at the top of range.
pub const SMOOTH_PASS_MULT: usize = 4;

/// Stiffness-match secant: at most this many warm-started passes.
const MAX_MATCH_PASSES: usize = 5;
/// Match tolerance: stop when the binned compliance is within this of target.
const MATCH_TOL: f64 = 0.02;

/// Calibrated stiffness law E/E0 = `coeff`·ρ^`exp` used for EVALUATION and
/// binning (distinct from the optimizer's possibly-penalized law in
/// `OptimizeParams`). Linear (1,1) in solid-topology mode.
#[derive(Clone, Copy, Debug)]
pub struct EvalLaw {
    pub exp: f64,
    pub coeff: f64,
}

/// Knobs that shape the pipeline beyond the per-iteration `OptimizeParams`.
pub struct PipelineCfg<'a> {
    pub eval: EvalLaw,
    /// "match uniform stiffness" goal: walk the budget so the binned design is
    /// as stiff as a uniform `ref_frac` print, at minimum mass.
    pub goal_match: bool,
    /// Reference uniform infill fraction — the match target AND the mass
    /// baseline ("vs X% uniform").
    pub ref_frac: f64,
    /// Number of density bins (auto level placement) when `levels_pct` is None.
    pub n_bins: usize,
    /// Manual level override (already clamped/sorted/deduped), e.g. binary
    /// {floor, 1} or user-calibrated densities; None ⇒ auto cluster.
    pub levels_pct: Option<&'a [f64]>,
    /// Region Taubin-smoothing passes (× `SMOOTH_PASS_MULT`).
    pub smooth_iters: usize,
}

/// Per-iteration update handed to the caller's progress callback. Carries the
/// solver's [`OptimizeProgress`] plus the secant-pass context the live UI shows.
pub struct IterUpdate<'a> {
    pub progress: &'a OptimizeProgress,
    pub pass: usize,
    pub passes: usize,
    pub budget: f64,
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
    pub c_solid: f64,
    /// Goal-match target compliance (0.0 when not matching).
    pub c_target: f64,
    pub u_binned: Vec<f64>,
    pub max_disp: f64,
    /// eps the verification solve used (for stress recovery).
    pub solution_eps: Vec<f32>,
    pub mean_binned: f64,
    pub regions: Vec<RegionMesh>,
    pub regions_raw: Vec<RegionMesh>,
    /// (budget, binned compliance) of each secant pass.
    pub pass_trace: Vec<(f64, f64)>,
    // ---- volume components (occupancy-weighted); mass = (skin+wall+infill)·h³·ρ ----
    pub vol_skin: f64,
    /// Wall-band volume inside design cells (composite skin's solid share).
    pub sum_f: f64,
    /// Infill-capable design volume (occupancy × (1 − wall fraction)).
    pub w_sum: f64,
    /// Infill volume actually placed by the binned design.
    pub infill_vol_binned: f64,
}

/// Run the optimization + binning + verification + reference solves + region
/// extraction. `progress` is called once per inner SIMP iteration with the
/// solver progress and the current secant pass; the caller marshals it (e.g.
/// to a JS callback / live preview).
#[allow(clippy::too_many_arguments)]
pub fn run_optimization(
    slot: &mut Option<SolverCache>,
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    params: &OptimizeParams,
    cfg: &PipelineCfg,
    mut progress: impl FnMut(&IterUpdate, &[f64], &[u32]),
) -> Result<OptOutcome, OptimizeError> {
    let solid = params.solid_mode;
    let (eval_exp, eval_coeff) = (cfg.eval.exp, cfg.eval.coeff);

    // ---- goal handling ----
    // "match": one tight uniform solve at ref_frac sets the target compliance;
    // a guarded secant then walks the budget until the BINNED design lands
    // within tolerance, each pass warm-started from the previous design.
    let max_passes = if cfg.goal_match { MAX_MATCH_PASSES } else { 1 };
    let mut c_target = 0.0f64;
    if cfg.goal_match {
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
            &x_ref, eval_exp, eval_coeff,
        )?;
        c_target = c_ref;
    }

    let mut pass_no = 1usize;
    let mut budget_k = if cfg.goal_match {
        // Optimized designs match uniform stiffness at ~70–85% of the mass —
        // start the search there.
        (cfg.ref_frac * 0.8).max(params.floor)
    } else {
        params.budget
    };
    let (mut lo_b, mut hi_b) = (params.floor, cfg.ref_frac);
    let mut warm_x: Option<Vec<f64>> = None;
    let mut warm_u: Option<Vec<f64>> = None;
    let mut pass_trace: Vec<(f64, f64)> = Vec::new();
    let mut total_iters = 0usize;

    let (result, centers, bins, x_binned, c_binned, u_binned, verify_stats) = loop {
        let params_k = OptimizeParams { budget: budget_k, ..*params };
        let pass = pass_no;
        let budget = budget_k;
        let result = optimize_cached(
            slot,
            grid,
            levels,
            problem,
            settings,
            &params_k,
            warm_x.as_deref(),
            warm_u.as_deref(),
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
        let (centers, bins): (Vec<f64>, Vec<u8>) = if solid {
            (
                vec![0.0, 1.0],
                solid_keep_bins(grid, &result.skin_cells, &result.design_cells, &result.x, 0.5),
            )
        } else {
            let centers: Vec<f64> = match cfg.levels_pct {
                Some(user) if !user.is_empty() => user.to_vec(),
                _ => cluster_levels(
                    &result.x, &result.se, cfg.n_bins, eval_exp, eval_coeff, params.floor,
                    params.cap,
                ),
            };
            let target_mean = result.x.iter().sum::<f64>() / result.x.len().max(1) as f64;
            let mut bins =
                assign_bins_mass(&result.x, &result.se, &centers, eval_exp, eval_coeff, target_mean);
            let min_cells = (result.design_cells.len() / 500).max(30);
            cleanup_small_regions(grid, &result.design_cells, &mut bins, centers.len(), min_cells);
            (centers, bins)
        };
        let x_binned: Vec<f64> = bins.iter().map(|&b| centers[b as usize]).collect();

        // Verification solve of the binned design (calibrated law), warm-started
        // from the optimizer's displacement; real convergence stats kept.
        let (c_b, _maxd, u_b, stats_b) = evaluate_cached_stats(
            slot, grid, levels, problem, settings, &result.skin_cells, &result.design_cells,
            &result.skin_frac, &x_binned, eval_exp, eval_coeff,
        )?;
        pass_trace.push((budget_k, c_b));

        if !cfg.goal_match {
            break (result, centers, bins, x_binned, c_b, u_b, stats_b);
        }
        // dev > 0: too compliant (needs more material); < 0: too stiff.
        let dev = c_b / c_target - 1.0;
        if dev.abs() <= MATCH_TOL || pass_no >= max_passes || hi_b - lo_b < 0.005 {
            break (result, centers, bins, x_binned, c_b, u_b, stats_b);
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
    let x_uniform = vec![mean_binned; x_binned.len()];
    let (c_uniform, _, _) = evaluate_cached(
        slot, grid, levels, problem, &ref_settings, &result.skin_cells, &result.design_cells,
        &result.skin_frac, &x_uniform, eval_exp, eval_coeff,
    )?;
    let x_solid = vec![1.0; x_binned.len()];
    let (c_solid, _, _) = evaluate_cached(
        slot, grid, levels, problem, &ref_settings, &result.skin_cells, &result.design_cells,
        &result.skin_frac, &x_solid, eval_exp, eval_coeff,
    )?;

    // ---- deformed field + stress eps ----
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
        let mut r = extract_region(grid, &inside, 0.4);
        if r.indices.is_empty() {
            continue;
        }
        r.density = centers[level];
        regions_raw.push(r);
    }
    let regions = smooth_regions(&regions_raw, cfg.smooth_iters);

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
        c_solid,
        c_target,
        u_binned,
        max_disp,
        solution_eps,
        mean_binned,
        regions,
        regions_raw,
        pass_trace,
        vol_skin,
        sum_f,
        w_sum,
    })
}

/// Taubin-smooth copies of the raw regions (0 passes = verbatim copy). Exposed
/// so the post-run smoothing slider can re-smooth without re-extracting.
pub fn smooth_regions(raw: &[RegionMesh], iters: usize) -> Vec<RegionMesh> {
    raw.iter()
        .map(|r| {
            let mut rr = RegionMesh {
                density: r.density,
                positions: r.positions.clone(),
                indices: r.indices.clone(),
            };
            if iters > 0 {
                taubin_smooth(&mut rr.positions, &rr.indices, iters * SMOOTH_PASS_MULT);
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
            ref_frac: 0.35,
            n_bins: 3,
            levels_pct: None,
            smooth_iters: 4,
        };
        let oc = run_optimization(
            &mut None, &grid, levels, &asm.problem, &settings, &params, &cfg, |_, _, _| {},
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
}
