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
//! Usage (run from the repo root; release build for realistic timing):
//!   cargo run --release -p filasim-core --bin regbench -- --save baseline.tsv
//!   cargo run --release -p filasim-core --bin regbench -- --check baseline.tsv
//!   ... add --big for the ~1M-cell solve (the perf worst case), --tol 0.01 to loosen.

use filasim_core::attach::{assemble, BcKind, BcSpec};
use filasim_core::bins::{assign_bins_mass, cleanup_small_regions, cluster_levels};
use filasim_core::mesh::primitives;
use filasim_core::simp::{evaluate, optimize, OptimizeParams};
use filasim_core::{pad_for_levels, solve_static, BoxRegion, SolveSettings, StaticProblem, VoxelGrid};
use std::time::Instant;

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
