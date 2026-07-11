// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Regression benchmark — guards that refactors keep the engine as fast AND as
//! accurate as before. Two metric classes:
//!   * QUALITY (Q): physics outputs that MUST stay put under a behaviour-
//!     preserving change — compliance, deflection, optimized stiffness gain,
//!     mean infill, sphere volume. `--check` FAILS if any drifts past `--tol`
//!     (default 0.5 %).
//!   * INFO (I): timings, MGCG iteration counts, residuals. Reported with the
//!     delta vs the baseline so a slowdown is visible, but never fails the run
//!     (machine noise / thread-count differences would make that flaky).
//!
//! Fixtures: sphere voxelize + Timoshenko cantilever + SIMP optimize, plus the
//! BEAM SUITE (`bench_beam_suite`) — a 64×8×4 mm cantilever in tension, bending,
//! and 6-mode modal, run SOLID and at uniform 30 % INFILL at two mesh sizes, and
//! the 3DBenchy voxelization at two resolutions. The beam suite's headline Q is
//! the mesh-independent solid↔infill ratio, which must hit the exact E(ρ)/mass
//! closed form (see DESIGN.md §14).
//!
//! Usage (run from the repo root; release build for realistic timing):
//!   cargo run --release -p filasim-core --bin regbench -- --save baseline.tsv
//!   cargo run --release -p filasim-core --bin regbench -- --check baseline.tsv
//!   ... add --big for the ~1M-cell solve (the perf worst case), --tol 0.01 to loosen.
//!
//! Usually invoked via `node scripts/preflight.mjs` (the pre-push gate).

use filasim_core::attach::{assemble, point_mass_lumping, BcKind, BcSpec, BodyLoad};
use filasim_core::bins::{assign_bins_mass, cleanup_small_regions, cluster_levels};
use filasim_core::mesh::primitives;
use filasim_core::modal::{analyze, ModalConfig};
use filasim_core::simp::{
    evaluate, evaluate_cached_stats, optimize, optimize_cached, BodyAccel, LoadSet, OptimizeParams,
};
use filasim_core::solve::{active_nodes, solve_cached, SolverCache};
use filasim_core::{
    pad_for_levels, solve_static, BoxRegion, NodeProblem, SolveSettings, StaticProblem, TriMesh,
    VoxelGrid,
};
use std::time::Instant;

// ---------- beam suite constants ----------

/// Uniform-infill validation density and the Gibson–Ashby stiffness law
/// `E/E0 = coeff · x^exponent` (simp.rs defaults). A uniform beam at this
/// density has EXACT analytic scaling vs the solid beam on the SAME mesh:
/// deflection × 1/x^exp, frequency × (x^exp / x)^0.5 = x^((exp-1)/2). The
/// mesh cancels in the ratio, so it isolates the E(ρ)+mass wiring.
const INFILL_X: f64 = 0.3;
const INFILL_EXP: f64 = 1.5;
const INFILL_COEFF: f64 = 1.0;
/// Beam material (PLA-ish, consistent units: MPa, mm, tonne/mm³).
const BEAM_E0: f64 = 2400.0;
const BEAM_NU: f64 = 0.35;
const BEAM_RHO: f64 = 1.24e-9;
/// Tip loads (small-strain linear regime).
const TENSION_N: f64 = 100.0;
const BENDING_N: f64 = -1.0;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Quality,
    Info,
}

struct Metrics {
    rows: Vec<(String, f64, Kind)>,
}

impl Metrics {
    fn new() -> Self {
        Metrics { rows: Vec::new() }
    }
    /// A physics output that must not move under a behaviour-preserving change.
    fn q(&mut self, name: &str, v: f64) {
        self.rows.push((name.to_string(), v, Kind::Quality));
    }
    /// A timing / iteration-count: reported, never fails the check.
    fn i(&mut self, name: &str, v: f64) {
        self.rows.push((name.to_string(), v, Kind::Info));
    }
}

// ---------- fixtures ----------

fn bench_voxelize(m: &mut Metrics) {
    let r = 10.0f32;
    let sph = primitives::sphere([0.0; 3], r, 64, 32);
    let t0 = Instant::now();
    let grid = VoxelGrid::voxelize(&sph, 0.5);
    let dt = t0.elapsed().as_secs_f64();
    let exact = 4.0 / 3.0 * std::f64::consts::PI * (r as f64).powi(3);
    let err = (grid.solid_volume() - exact) / exact;
    m.q("vox_sphere_volerr_pct", err * 100.0);
    m.i("vox_time_ms", dt * 1000.0);
    m.i("vox_mcells_per_s", grid.cell_count() as f64 / 1e6 / dt);
}

/// Tip-loaded cantilever vs Timoshenko beam theory (the solver's accuracy anchor).
fn bench_cantilever(m: &mut Metrics, prefix: &str, nx: usize, ny: usize, nz: usize, h: f64) {
    let (e0, nu, f) = (2000.0f64, 0.3f64, -10.0f64);
    let (l, bdim, hdim) = (nx as f64 * h, ny as f64 * h, nz as f64 * h);
    let grid = VoxelGrid::solid_box(nx, ny, nz, h);
    let problem = StaticProblem {
        grid,
        fixed: vec![BoxRegion::new([-0.1, -1.0, -1.0], [0.1, bdim + 1.0, hdim + 1.0])],
        loads: vec![(
            BoxRegion::new([l - 0.1 * h, -1.0, -1.0], [l + h, bdim + 1.0, hdim + 1.0]),
            [0.0, 0.0, f],
        )],
        settings: SolveSettings { e0, nu, tol: 1e-5, max_iter: 400, ..Default::default() },
    };
    let region = BoxRegion::new([l - 0.1 * h, -1.0, -1.0], [l + h, bdim + 1.0, hdim + 1.0]);
    let t0 = Instant::now();
    let sol = solve_static(&problem).expect("cantilever solve");
    let dt = t0.elapsed().as_secs_f64();
    let tip = sol.mean_displacement(&region).unwrap();
    let inertia = bdim * hdim.powi(3) / 12.0;
    let area = bdim * hdim;
    let g = e0 / (2.0 * (1.0 + nu));
    let kappa = 10.0 * (1.0 + nu) / (12.0 + 11.0 * nu);
    let exact = f * l.powi(3) / (3.0 * e0 * inertia) + f * l / (kappa * g * area);
    m.q(&format!("{prefix}_tip_uz"), tip[2]);
    m.q(&format!("{prefix}_timo_ratio"), tip[2] / exact);
    m.i(&format!("{prefix}_iters"), sol.iterations as f64);
    m.i(&format!("{prefix}_residual"), sol.rel_residual);
    m.i(&format!("{prefix}_time_ms"), dt * 1000.0);
}

/// SIMP infill optimization on a cantilever beam, binned with the app's
/// pipeline. Quality anchors: continuous & binned compliance, the stiffness
/// gain over uniform infill at equal mass, the achieved mean infill.
fn bench_optimize(m: &mut Metrics) {
    let beam = primitives::boxx([0.0; 3], [60.0, 10.0, 10.0]);
    let grid0 = VoxelGrid::voxelize(&beam, 1.0);
    let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
    let bcs = vec![
        BcSpec { kind: BcKind::Fixed, tris: vec![0, 1] },
        BcSpec { kind: BcKind::Force([0.0, 0.0, -30.0]), tris: vec![2, 3] },
    ];
    let asm = assemble(&beam, &grid, &bcs, None, &settings).expect("assemble");
    let params = OptimizeParams {
        budget: 0.35,
        exponent: 1.5,
        wall_mm: 1.0,
        max_iter: 30,
        ..Default::default()
    };
    let (exp, coeff) = (params.exponent, params.coeff);

    let t0 = Instant::now();
    let res = optimize(&grid, levels, &asm.problem, &settings, &params, None, None, |_, _, _| {})
        .expect("optimize");
    let dt = t0.elapsed().as_secs_f64();

    let target = res.x.iter().sum::<f64>() / res.x.len() as f64;

    // App binning pipeline (3 bins): floor-pinned energy-weighted levels +
    // mass-constrained assignment + small-region cleanup.
    let centers = cluster_levels(&res.x, &res.se, 3, exp, coeff, params.floor, params.cap);
    let mut bins = assign_bins_mass(&res.x, &res.se, &centers, exp, coeff, target);
    cleanup_small_regions(&grid, &res.design_cells, &mut bins, centers.len(), 30);
    let x_binned: Vec<f64> = bins.iter().map(|&b| centers[b as usize]).collect();
    let mean_binned = x_binned.iter().sum::<f64>() / x_binned.len() as f64;
    let x_uniform = vec![mean_binned; x_binned.len()];

    let (c_binned, maxd, _) = evaluate(
        &grid, levels, &asm.problem, &settings, &res.skin_cells, &res.design_cells,
        &res.skin_frac, &x_binned, exp, coeff, Some(&res.u),
    )
    .expect("binned eval");
    let (c_uniform, _, _) = evaluate(
        &grid, levels, &asm.problem, &settings, &res.skin_cells, &res.design_cells,
        &res.skin_frac, &x_uniform, exp, coeff, Some(&res.u),
    )
    .expect("uniform eval");

    m.q("opt_c_continuous", res.compliance);
    m.q("opt_c_binned", c_binned);
    m.q("opt_c_uniform", c_uniform);
    m.q("opt_gain_vs_uniform", c_uniform / c_binned);
    m.q("opt_mean_infill", target);
    m.q("opt_maxd", maxd);
    m.i("opt_iters", res.iterations as f64);
    m.i("opt_time_ms", dt * 1000.0);
}

/// SIMP optimization DRIVEN BY SELF-WEIGHT (DESIGN §16 dec. 4): the same
/// cantilever as `bench_optimize`, but the only load is the part's own weight
/// under 1 g — a DESIGN-DEPENDENT load the optimizer must recompute from the
/// live density every iteration. Anchors the self-weight optimizer path
/// (continuous/binned compliance, gain vs uniform) AND dec. 10's own-weight
/// comparison: the fully-solid baseline carries FULL self-weight, so it must
/// deflect MORE than the equal-mass uniform (which is lighter). A heavy density
/// makes the self-weight bending measurable at this size.
fn bench_optimize_selfweight(m: &mut Metrics) {
    let beam = primitives::boxx([0.0; 3], [60.0, 10.0, 10.0]);
    let grid0 = VoxelGrid::voxelize(&beam, 1.0);
    let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
    let density = 5.0e-8; // tonne/mm³ — heavy, so self-weight loads the beam
    let accel = [0.0, 0.0, -9810.0]; // 1 g down
    // Fixed root only — the load is the part's own weight. Mass-only assemble
    // (empty vfrac): the self-weight FORCE is added per iteration; here it only
    // sets the RBM load flag on the solid cells.
    let bcs = vec![BcSpec { kind: BcKind::Fixed, tris: vec![0, 1] }];
    let body = BodyLoad { accel, density, vfrac: &[] };
    let asm = assemble(&beam, &grid, &bcs, Some(body), &settings).expect("assemble self-weight");
    let params = OptimizeParams {
        budget: 0.35,
        exponent: 1.5,
        wall_mm: 1.0,
        max_iter: 30,
        ..Default::default()
    };
    let (exp, coeff) = (params.exponent, params.coeff);
    let loads = LoadSet {
        primary_body: Some(BodyAccel { accel, density }),
        ..Default::default()
    };

    let t0 = Instant::now();
    let res = optimize_cached(
        &mut None, &grid, levels, &asm.problem, &settings, &params, None, None, &loads,
        |_, _, _| {},
    )
    .expect("optimize self-weight");
    let dt = t0.elapsed().as_secs_f64();

    let target = res.x.iter().sum::<f64>() / res.x.len() as f64;
    let centers = cluster_levels(&res.x, &res.se, 3, exp, coeff, params.floor, params.cap);
    let mut bins = assign_bins_mass(&res.x, &res.se, &centers, exp, coeff, target);
    cleanup_small_regions(&grid, &res.design_cells, &mut bins, centers.len(), 30);
    let x_binned: Vec<f64> = bins.iter().map(|&b| centers[b as usize]).collect();
    let mean_binned = x_binned.iter().sum::<f64>() / x_binned.len() as f64;
    let x_uniform = vec![mean_binned; x_binned.len()];
    let x_solid = vec![1.0; x_binned.len()];

    // Each design carries its OWN self-weight (via `loads`): the eval recomputes
    // the body force from the density it is handed (dec. 10).
    let ev = |x: &[f64]| {
        evaluate_cached_stats(
            &mut None, &grid, levels, &asm.problem, &settings, &res.skin_cells, &res.design_cells,
            &res.skin_frac, x, exp, coeff, &loads,
        )
        .expect("self-weight eval")
    };
    let (c_binned, maxd, _, _) = ev(&x_binned);
    let (c_uniform, _, _, _) = ev(&x_uniform);
    let (c_solid, maxd_solid, _, _) = ev(&x_solid);

    m.q("opt_sw_c_continuous", res.compliance);
    m.q("opt_sw_c_binned", c_binned);
    m.q("opt_sw_c_uniform", c_uniform);
    m.q("opt_sw_c_solid", c_solid);
    m.q("opt_sw_gain_vs_uniform", c_uniform / c_binned);
    m.q("opt_sw_mean_infill", target);
    m.q("opt_sw_maxd", maxd);
    m.i("opt_sw_iters", res.iterations as f64);
    m.i("opt_sw_time_ms", dt * 1000.0);

    // Sanity (caught at bench-run time, before --check): the self-weight
    // optimization converged to a sane design, and the fully-solid baseline —
    // heavier, carrying full self-weight — deflects under its own weight.
    assert!(
        res.iterations > 0 && c_binned > 0.0 && maxd > 0.0 && maxd_solid > 0.0,
        "self-weight optimize produced a sane result (c_binned {c_binned}, maxd {maxd})"
    );
}

// ---------- beam suite (solid + uniform infill; tension, bending, modal) ----------

struct BeamOut {
    ux: f64,      // tension: mean axial tip displacement (mm)
    uz: f64,      // bending: mean transverse tip displacement (mm)
    f: Vec<f64>,  // modal natural frequencies (Hz, ascending)
}

/// Mean of displacement component `comp` (0=x,1=y,2=z) over `nodes`.
fn mean_disp(u: &[f64], nodes: &[u32], comp: usize) -> f64 {
    if nodes.is_empty() {
        return 0.0;
    }
    let s: f64 = nodes.iter().map(|&n| u[3 * n as usize + comp]).sum();
    s / nodes.len() as f64
}

/// One canonical rectangular cantilever (L×B×H = nx·h × ny·h × nz·h), root
/// plane x=0 fully fixed, run in three ways: axial tension, transverse bending,
/// and 6-mode modal. `infill=None` → solid (eps=1, mass frac=1); `infill=Some(x)`
/// → uniform density x with stiffness eps=coeff·x^exp (Gibson–Ashby) and mass
/// fraction x — DECOUPLED, so the solid↔infill ratio is the exact closed form.
fn bench_beam(
    m: &mut Metrics,
    prefix: &str,
    nx: usize,
    ny: usize,
    nz: usize,
    h: f64,
    infill: Option<f64>,
) -> BeamOut {
    let s = SolveSettings { e0: BEAM_E0, nu: BEAM_NU, tol: 1e-6, max_iter: 400, ..Default::default() };

    // Solid box; for infill, drop every cell's material fraction to x (mass law).
    let mut raw = VoxelGrid::solid_box(nx, ny, nz, h);
    if let Some(x) = infill {
        for v in raw.scale.iter_mut() {
            *v = x as f32;
        }
    }
    let (grid, levels) = pad_for_levels(&raw, s.max_levels);
    let active = active_nodes(&grid);
    let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);

    // Root plane (x=0) fixed; tip plane (x=nx, the free end of the solid) loaded.
    // The chosen sizes need no padding on x, so the solid tip sits at node x=nx.
    let mut fixed = Vec::new();
    let mut tip = Vec::new();
    for z in 0..mz {
        for y in 0..my {
            let root = (z * my + y) * mx;
            if active[root] {
                fixed.push(root as u32);
            }
            let t = (z * my + y) * mx + nx;
            if active[t] {
                tip.push(t as u32);
            }
        }
    }

    // Stiffness eps: flat solid (1.0) or Gibson–Ashby infill (coeff·x^exp), on
    // solid cells only. Uniform ⇒ scales K by a constant vs the solid beam.
    let eps_val = match infill {
        Some(x) => (INFILL_COEFF * x.powf(INFILL_EXP)) as f32,
        None => 1.0,
    };
    let eps: Vec<f32> = grid.scale.iter().map(|&sc| if sc > 0.0 { eps_val } else { 0.0 }).collect();

    // Geometry for the analytic anchors.
    let (l, bdim, hdim) = (nx as f64 * h, ny as f64 * h, nz as f64 * h);
    let area = bdim * hdim;

    // --- tension: total +Fx spread over the tip face ---
    let forces: Vec<(u32, [f64; 3])> =
        tip.iter().map(|&n| (n, [TENSION_N / tip.len() as f64, 0.0, 0.0])).collect();
    let prob = NodeProblem { fixed: fixed.clone(), springs: Vec::new(), forces, rigid: Vec::new() };
    let t0 = Instant::now();
    let r = solve_cached(&mut None, &grid, levels, &prob, &s, eps.clone(), s.tol, s.max_iter)
        .expect("tension solve");
    let dt = t0.elapsed().as_secs_f64();
    let ux = mean_disp(&r.u, &tip, 0);
    m.q(&format!("{prefix}_tension_ux"), ux);
    m.i(&format!("{prefix}_tension_iters"), r.stats.iterations as f64);
    m.i(&format!("{prefix}_tension_residual"), r.stats.rel_residual);
    m.i(&format!("{prefix}_tension_time_ms"), dt * 1000.0);
    if infill.is_none() {
        // Exact uniaxial: ux = F·L / (A·E).
        let exact = TENSION_N * l / (area * BEAM_E0);
        m.q(&format!("{prefix}_tension_axial_ratio"), ux / exact);
    }

    // --- bending: total -Fz spread over the tip face ---
    let forces: Vec<(u32, [f64; 3])> =
        tip.iter().map(|&n| (n, [0.0, 0.0, BENDING_N / tip.len() as f64])).collect();
    let prob = NodeProblem { fixed: fixed.clone(), springs: Vec::new(), forces, rigid: Vec::new() };
    let t0 = Instant::now();
    let r = solve_cached(&mut None, &grid, levels, &prob, &s, eps.clone(), s.tol, s.max_iter)
        .expect("bending solve");
    let dt = t0.elapsed().as_secs_f64();
    let uz = mean_disp(&r.u, &tip, 2);
    m.q(&format!("{prefix}_bend_uz"), uz);
    m.i(&format!("{prefix}_bend_iters"), r.stats.iterations as f64);
    m.i(&format!("{prefix}_bend_residual"), r.stats.rel_residual);
    m.i(&format!("{prefix}_bend_time_ms"), dt * 1000.0);
    if infill.is_none() {
        // Timoshenko tip deflection (bending in z; weak-axis inertia I = B·H³/12).
        let inertia = bdim * hdim.powi(3) / 12.0;
        let g = BEAM_E0 / (2.0 * (1.0 + BEAM_NU));
        let kappa = 10.0 * (1.0 + BEAM_NU) / (12.0 + 11.0 * BEAM_NU);
        let exact = BENDING_N * l.powi(3) / (3.0 * BEAM_E0 * inertia)
            + BENDING_N * l / (kappa * g * area);
        m.q(&format!("{prefix}_bend_timo_ratio"), uz / exact);
    }

    // --- modal: root-clamped free vibration, lowest 6 modes ---
    let mprob = NodeProblem { fixed: fixed.clone(), springs: Vec::new(), forces: Vec::new(), rigid: Vec::new() };
    let mut cache = SolverCache::build(&grid, levels, &mprob, &s, eps.clone());
    let cfg = ModalConfig::new(6);
    let t0 = Instant::now();
    let res = analyze(&mut cache.solver, &grid.scale, BEAM_RHO, &[], &cfg, |_, _, _| {})
        .expect("modal solve");
    let dt = t0.elapsed().as_secs_f64();
    for (k, &fk) in res.freqs_hz.iter().take(6).enumerate() {
        m.q(&format!("{prefix}_modal_f{}", k + 1), fk);
    }
    m.i(&format!("{prefix}_modal_outer_iters"), res.outer_iters as f64);
    m.i(&format!("{prefix}_modal_vcycles"), res.total_inner_iters as f64);
    m.i(&format!("{prefix}_modal_time_ms"), dt * 1000.0);
    if infill.is_none() {
        // Euler–Bernoulli 1st-bending (weak axis): wide band (a thick hex beam
        // is shear-stiff) — this only catches a gross mass/unit slip.
        let inertia = bdim * hdim.powi(3) / 12.0;
        let bl = 1.875104f64;
        let eb = bl * bl / std::f64::consts::TAU
            * (BEAM_E0 * inertia / (BEAM_RHO * area * l.powi(4))).sqrt();
        m.q(&format!("{prefix}_modal_f1_eb_ratio"), res.freqs_hz.first().copied().unwrap_or(0.0) / eb);
    }

    BeamOut { ux, uz, f: res.freqs_hz }
}

/// Solid + uniform-infill beam at one mesh, plus the mesh-independent solid↔infill
/// ratios (must equal the closed-form E(ρ)/mass multiplier).
fn bench_beam_pair(m: &mut Metrics, sz: &str, nx: usize, ny: usize, nz: usize, h: f64) {
    let solid = bench_beam(m, &format!("beam_solid_{sz}"), nx, ny, nz, h, None);
    let infill = bench_beam(m, &format!("beam_infill_{sz}"), nx, ny, nz, h, Some(INFILL_X));
    // Deflection ratio = 1/x^exp; frequency ratio = x^((exp-1)/2).
    m.q(&format!("beam_ratio_tension_{sz}"), infill.ux / solid.ux);
    m.q(&format!("beam_ratio_bend_{sz}"), infill.uz / solid.uz);
    if !solid.f.is_empty() && !infill.f.is_empty() {
        m.q(&format!("beam_ratio_modal_{sz}"), infill.f[0] / solid.f[0]);
    }
}

/// Acceleration loads + remote point masses (DESIGN §16), on the §14 solid
/// 64×8×4 cantilever with the root plane fully fixed. Two anchors:
///  (a) LEVER ARM — a tip point mass `m` whose CG is offset `r` along the beam
///      axis, under 1 g, must reproduce the hand-composed tip force `m·g` +
///      moment `m·g·r` bit-for-bit (the remote-mass couple transport IS the
///      Force+Moment machinery), so the deflection ratio is exactly 1.
///  (b) SELF-WEIGHT — 1 g with no dummy mass vs the Euler cantilever band
///      `q·L⁴/8EI`, `q = ρ·A·g`. Regression-anchored (thick-hex shear makes the
///      exact ratio a fixed value slightly off 1; drift is what matters).
fn bench_accel(m: &mut Metrics) {
    let (nx, ny, nz, h) = (64usize, 8usize, 4usize, 1.0f64);
    let beam = primitives::boxx(
        [0.0, 0.0, 0.0],
        [nx as f32 * h as f32, ny as f32 * h as f32, nz as f32 * h as f32],
    );
    let s = SolveSettings { e0: BEAM_E0, nu: BEAM_NU, tol: 1e-7, max_iter: 500, ..Default::default() };
    let (grid, levels) = pad_for_levels(&VoxelGrid::voxelize(&beam, h), s.max_levels);
    // Solid stiffness: eps = 1 on solid cells (occupancy is 1 on this box).
    let eps: Vec<f32> = grid.scale.iter().map(|&sc| if sc > 0.0 { 1.0 } else { 0.0 }).collect();

    // Tip-plane nodes (x = nx, the solid free end) for the mean deflection.
    let active = active_nodes(&grid);
    let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
    let mut tip = Vec::new();
    for z in 0..mz {
        for y in 0..my {
            let t = (z * my + y) * mx + nx;
            if t < active.len() && active[t] {
                tip.push(t as u32);
            }
        }
    }
    let cross = |a: [f64; 3], b: [f64; 3]| {
        [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
    };
    let g = 9810.0f64;
    let root = vec![0u32, 1]; // -x face

    // --- (a) remote-mass lever arm ---
    let tip_face = vec![2u32, 3]; // +x face
    let mass = 1.0e-4; // tonne (100 g)
    let r = 40.0; // CG offset along +x, beyond the tip
    // The tip face is symmetric, so its area-weighted centroid is the geometric
    // face centre — the point the couple transports about. Offset the CG +r·x̂.
    let c = [nx as f64 * h, ny as f64 * h * 0.5, nz as f64 * h * 0.5];
    let point = [c[0] + r, c[1], c[2]];
    let accel = [0.0, 0.0, -g];
    let force = [mass * accel[0], mass * accel[1], mass * accel[2]];
    let moment = cross([r, 0.0, 0.0], force); // = (p − c) × F

    // Solve A: the mass entity (density 0 ⇒ mass load only, no self-weight).
    let bcs_a = vec![
        BcSpec { kind: BcKind::Fixed, tris: root.clone() },
        BcSpec { kind: BcKind::Mass { point, mass, rigid: false }, tris: tip_face.clone() },
    ];
    let body_a = BodyLoad { accel, density: 0.0, vfrac: &grid.scale };
    let asm_a = assemble(&beam, &grid, &bcs_a, Some(body_a), &s).expect("assemble mass");
    let ra = solve_cached(&mut None, &grid, levels, &asm_a.problem, &s, eps.clone(), s.tol, s.max_iter)
        .expect("mass solve");
    let uz_a = mean_disp(&ra.u, &tip, 2);

    // Solve B: the same load hand-composed as tip Force + Moment.
    let bcs_b = vec![
        BcSpec { kind: BcKind::Fixed, tris: root.clone() },
        BcSpec { kind: BcKind::Force(force), tris: tip_face.clone() },
        BcSpec { kind: BcKind::Moment(moment), tris: tip_face.clone() },
    ];
    let asm_b = assemble(&beam, &grid, &bcs_b, None, &s).expect("assemble force+moment");
    let rb = solve_cached(&mut None, &grid, levels, &asm_b.problem, &s, eps.clone(), s.tol, s.max_iter)
        .expect("force+moment solve");
    let uz_b = mean_disp(&rb.u, &tip, 2);

    m.q("accel_mass_leverarm_uz", uz_a);
    // The couple transport IS Force+Moment, so this is 1 to solver tolerance.
    m.q("accel_mass_vs_forcemoment_ratio", if uz_b != 0.0 { uz_a / uz_b } else { 0.0 });

    // --- (b) part self-weight ---
    let bcs_g = vec![BcSpec { kind: BcKind::Fixed, tris: root.clone() }];
    let body_g = BodyLoad { accel: [0.0, 0.0, -g], density: BEAM_RHO, vfrac: &grid.scale };
    let asm_g = assemble(&beam, &grid, &bcs_g, Some(body_g), &s).expect("assemble self-weight");
    let rg = solve_cached(&mut None, &grid, levels, &asm_g.problem, &s, eps.clone(), s.tol, s.max_iter)
        .expect("self-weight solve");
    let uz_g = mean_disp(&rg.u, &tip, 2);
    let (l, bdim, hdim) = (nx as f64 * h, ny as f64 * h, nz as f64 * h);
    let area = bdim * hdim;
    let inertia = bdim * hdim.powi(3) / 12.0;
    let q = BEAM_RHO * g * area; // distributed weight, N/mm
    let exact = -q * l.powi(4) / (8.0 * BEAM_E0 * inertia);
    m.q("accel_selfweight_tip_uz", uz_g);
    m.q("accel_selfweight_eb_ratio", uz_g / exact);
    m.i("accel_selfweight_iters", rg.stats.iterations as f64);

    // --- (c) modal inertia of a remote point mass (DESIGN §16) ---
    // The static path realises a mass only as F = m·a; MODAL must additionally
    // add its inertia to the mass matrix M, or a payload would leave the natural
    // frequencies untouched. Guard: a tip mass equal to the beam's own mass must
    // drop f1 toward the Rayleigh estimate f_M/f_bare = √(0.2357·m_b/(M+0.2357·m_b))
    // ≈ 0.437 (33/140 is the cantilever's tip-effective self-mass).
    let m_beam = BEAM_RHO * (l * bdim * hdim); // total beam mass (tonne)
    let bcs_m = vec![BcSpec { kind: BcKind::Fixed, tris: root.clone() }];
    let asm_m = assemble(&beam, &grid, &bcs_m, None, &s).expect("assemble modal support");
    let mut mcache = SolverCache::build(&grid, levels, &asm_m.problem, &s, eps.clone());
    let mcfg = ModalConfig::new(1);
    let f_bare = analyze(&mut mcache.solver, &grid.scale, BEAM_RHO, &[], &mcfg, |_, _, _| {})
        .expect("modal bare")
        .freqs_hz[0];
    // Tip mass = beam mass, bolted to the +x face. The CG offset is irrelevant to
    // the modal mass (a translational-DOF mesh cannot hold the offset's rotatory
    // inertia), so `point = c` (the face centre) reproduces the same M.
    let bcs_mp = vec![
        BcSpec { kind: BcKind::Fixed, tris: root.clone() },
        BcSpec { kind: BcKind::Mass { point: c, mass: m_beam, rigid: false }, tris: tip_face.clone() },
    ];
    let extra = point_mass_lumping(&beam, &grid, &bcs_mp);
    let f_load = analyze(&mut mcache.solver, &grid.scale, BEAM_RHO, &extra, &mcfg, |_, _, _| {})
        .expect("modal loaded")
        .freqs_hz[0];
    let ratio = f_load / f_bare;
    let pred = (0.2357 * m_beam / (m_beam + 0.2357 * m_beam)).sqrt();
    assert!(
        f_load < f_bare && ratio < 0.9,
        "tip point mass must lower f1: bare {f_bare:.1} Hz, loaded {f_load:.1} Hz"
    );
    m.q("accel_modal_pointmass_f1_bare", f_bare);
    m.q("accel_modal_pointmass_f1_loaded", f_load);
    m.q("accel_modal_pointmass_ratio", ratio);
    m.i("accel_modal_pointmass_rayleigh", pred);
}

/// RIGID remote mass (DESIGN §16 milestone 4) on the §14 solid cantilever: a
/// 100 g mass bolted to the tip face with its CG offset 40 mm, under 1 g, with
/// the mount RIGID vs DEFORMABLE. Two things are guarded:
///  (a) the rigid PATCH moves as a rigid body — its `nonrigidity` fraction is
///      near zero, and far below the deformable patch's (proof the penalty
///      stiffens the mount), and
///  (b) the MGCG solve still CONVERGES with the penalty rank-6 term (the flagged
///      convergence risk — iterations/residual are Info-tracked, convergence
///      asserted). Tip deflection + the rigid/deformable ratio are anchored.
fn bench_rigid_mass(m: &mut Metrics) {
    let (nx, ny, nz, h) = (64usize, 8usize, 4usize, 1.0f64);
    let beam = primitives::boxx(
        [0.0, 0.0, 0.0],
        [nx as f32 * h as f32, ny as f32 * h as f32, nz as f32 * h as f32],
    );
    let s = SolveSettings { e0: BEAM_E0, nu: BEAM_NU, tol: 1e-7, max_iter: 500, ..Default::default() };
    let (grid, levels) = pad_for_levels(&VoxelGrid::voxelize(&beam, h), s.max_levels);
    let eps: Vec<f32> = grid.scale.iter().map(|&sc| if sc > 0.0 { 1.0 } else { 0.0 }).collect();
    let active = active_nodes(&grid);
    let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
    let mut tip = Vec::new();
    for z in 0..mz {
        for y in 0..my {
            let t = (z * my + y) * mx + nx;
            if t < active.len() && active[t] {
                tip.push(t as u32);
            }
        }
    }
    let root = vec![0u32, 1]; // -x face
    let tip_face = vec![2u32, 3]; // +x face
    let mass = 1.0e-4; // 100 g (tonne)
    let r = 40.0;
    let c = [nx as f64 * h, ny as f64 * h * 0.5, nz as f64 * h * 0.5];
    let point = [c[0] + r, c[1], c[2]];
    let body = BodyLoad { accel: [0.0, 0.0, -9810.0], density: 0.0, vfrac: &grid.scale };

    let solve = |rigid: bool| {
        let bcs = vec![
            BcSpec { kind: BcKind::Fixed, tris: root.clone() },
            BcSpec { kind: BcKind::Mass { point, mass, rigid }, tris: tip_face.clone() },
        ];
        let asm = assemble(&beam, &grid, &bcs, Some(body), &s).expect("assemble mass");
        let sol = solve_cached(&mut None, &grid, levels, &asm.problem, &s, eps.clone(), s.tol, s.max_iter)
            .expect("mass solve");
        (asm, sol)
    };
    let (asm_r, rr) = solve(true);
    let (_asm_d, rd) = solve(false);
    assert!(
        asm_r.problem.rigid.len() == 1,
        "the rigid mount built a coupling (not a degenerate fallback)"
    );
    // Convergence with the penalty term is the milestone's flagged risk.
    assert!(
        rr.stats.converged && rr.stats.rel_residual < 1e-6,
        "rigid solve converged (res {:.2e}, {} iters)",
        rr.stats.rel_residual,
        rr.stats.iterations
    );
    // The rigid group evaluated on BOTH displacement fields: the rigid mount's
    // patch is near-rigid, the deformable one is not.
    let rg = &asm_r.problem.rigid[0];
    let nonrigid_r = rg.nonrigidity(&rr.u);
    let nonrigid_d = rg.nonrigidity(&rd.u);
    assert!(
        nonrigid_r < 0.05 && nonrigid_r < 0.5 * nonrigid_d,
        "rigid patch must move (near-)rigidly: rigid {nonrigid_r:.4} vs deformable {nonrigid_d:.4}"
    );

    let uz_d = mean_disp(&rd.u, &tip, 2);
    m.q("rigid_mass_tip_uz", mean_disp(&rr.u, &tip, 2));
    m.q("rigid_vs_deformable_tip", if uz_d != 0.0 { mean_disp(&rr.u, &tip, 2) / uz_d } else { 0.0 });
    // Near-zero diagnostics carry solve noise (threaded f32 reduction order), so
    // they are Info, not drift-guarded Quality — the asserts above pin behaviour.
    m.i("rigid_mass_nonrigidity", nonrigid_r);
    m.i("rigid_deformable_nonrigidity", nonrigid_d);
    m.i("rigid_mass_iters", rr.stats.iterations as f64);
    m.i("rigid_mass_residual", rr.stats.rel_residual);
    m.i("rigid_deformable_iters", rd.stats.iterations as f64);
}

/// Voxelize the 3DBenchy at one resolution — regression-anchored (no analytic
/// volume): cell/element counts and solid volume must not drift; time is Info.
fn bench_voxelize_benchy(m: &mut Metrics, sz: &str, mesh: &TriMesh, h: f64) {
    let t0 = Instant::now();
    let grid = VoxelGrid::voxelize(mesh, h);
    let dt = t0.elapsed().as_secs_f64();
    m.q(&format!("benchy_{sz}_cells"), grid.cell_count() as f64);
    m.q(&format!("benchy_{sz}_solid"), grid.solid_count() as f64);
    m.q(&format!("benchy_{sz}_volume_mm3"), grid.solid_volume());
    m.i(&format!("benchy_{sz}_time_ms"), dt * 1000.0);
    m.i(&format!("benchy_{sz}_mcells_s"), grid.cell_count() as f64 / 1e6 / dt);
}

/// The full beam suite (both mesh sizes) + the Benchy voxelization checks.
/// Physical beam is fixed at 64×8×4 mm; the fine mesh keeps it (h = 1/3 mm).
fn bench_beam_suite(m: &mut Metrics) {
    bench_beam_pair(m, "small", 64, 8, 4, 1.0);
    bench_beam_pair(m, "fine", 192, 24, 12, 1.0 / 3.0);

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../3dbenchy.stl");
    match std::fs::read(path) {
        Ok(bytes) => {
            let mesh = TriMesh::from_stl(&bytes).expect("parse 3dbenchy.stl");
            let (lo, hi) = mesh.bounds().expect("benchy bounds");
            m.q("benchy_bbox_x", hi[0] - lo[0]);
            m.q("benchy_bbox_y", hi[1] - lo[1]);
            m.q("benchy_bbox_z", hi[2] - lo[2]);
            bench_voxelize_benchy(m, "coarse", &mesh, 1.0);
            bench_voxelize_benchy(m, "fine", &mesh, 0.4);
        }
        Err(e) => eprintln!("skip benchy voxelization ({path}: {e})"),
    }
}

// ---------- baseline I/O ----------

fn save(m: &Metrics, path: &str) {
    let mut out = String::from("# regbench baseline — name\\tvalue\\tQ|I\n");
    for (name, v, kind) in &m.rows {
        let k = if *kind == Kind::Quality { "Q" } else { "I" };
        out.push_str(&format!("{name}\t{v}\t{k}\n"));
    }
    std::fs::write(path, out).expect("write baseline");
    eprintln!("saved {} metrics to {path}", m.rows.len());
}

fn load(path: &str) -> std::collections::HashMap<String, f64> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read baseline {path}: {e}"));
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 2 {
            if let Ok(v) = parts[1].parse::<f64>() {
                map.insert(parts[0].to_string(), v);
            }
        }
    }
    map
}

/// Returns process exit code (0 ok, 1 on a quality regression).
fn check(m: &Metrics, path: &str, tol: f64) -> i32 {
    let base = load(path);
    let mut failed = 0;
    println!(
        "{:<26} {:>14} {:>14} {:>10}  {}",
        "metric", "baseline", "current", "rel.diff", "status"
    );
    println!("{}", "-".repeat(82));
    for (name, cur, kind) in &m.rows {
        let Some(&old) = base.get(name) else {
            println!("{name:<26} {:>14} {cur:>14.6} {:>10}  NEW", "—", "—");
            continue;
        };
        let denom = if old.abs() > 1e-30 { old.abs() } else { 1.0 };
        let rel = (cur - old) / denom;
        let status = match kind {
            Kind::Quality => {
                if rel.abs() > tol {
                    failed += 1;
                    "FAIL"
                } else {
                    "ok"
                }
            }
            Kind::Info => "info",
        };
        println!(
            "{name:<26} {old:>14.6} {cur:>14.6} {:>+9.3}% {:>5}",
            rel * 100.0,
            status
        );
    }
    println!("{}", "-".repeat(82));
    if failed == 0 {
        println!("PASS — all quality metrics within {:.3}%", tol * 100.0);
        0
    } else {
        println!("FAIL — {failed} quality metric(s) drifted past {:.3}%", tol * 100.0);
        1
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    #[cfg(feature = "parallel")]
    eprintln!("threads: {}", rayon::current_num_threads());
    #[cfg(not(feature = "parallel"))]
    eprintln!("threads: 1 (sequential build)");

    let mut m = Metrics::new();
    bench_voxelize(&mut m);
    bench_cantilever(&mut m, "solve_small", 80, 8, 8, 1.0);
    bench_cantilever(&mut m, "solve_mid", 160, 16, 16, 0.5);
    bench_optimize(&mut m);
    bench_optimize_selfweight(&mut m);
    bench_beam_suite(&mut m);
    bench_accel(&mut m);
    bench_rigid_mass(&mut m);
    if args.iter().any(|a| a == "--big") {
        bench_cantilever(&mut m, "solve_big", 256, 64, 64, 0.25);
    }

    let tol = arg_value(&args, "--tol").and_then(|v| v.parse().ok()).unwrap_or(0.005);

    if let Some(path) = arg_value(&args, "--save") {
        save(&m, &path);
    } else if let Some(path) = arg_value(&args, "--check") {
        std::process::exit(check(&m, &path, tol));
    } else {
        for (name, v, kind) in &m.rows {
            let k = if *kind == Kind::Quality { "Q" } else { "I" };
            println!("[{k}] {name:<26} {v:>16.6}");
        }
    }
}
