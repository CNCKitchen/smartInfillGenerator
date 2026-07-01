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

use filasim_core::attach::{assemble, BcKind, BcSpec};
use filasim_core::bins::{assign_bins_mass, cleanup_small_regions, cluster_levels};
use filasim_core::mesh::primitives;
use filasim_core::modal::{analyze, ModalConfig};
use filasim_core::simp::{evaluate, optimize, OptimizeParams};
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
    let prob = NodeProblem { fixed: fixed.clone(), springs: Vec::new(), forces };
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
    let prob = NodeProblem { fixed: fixed.clone(), springs: Vec::new(), forces };
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
    let mprob = NodeProblem { fixed: fixed.clone(), springs: Vec::new(), forces: Vec::new() };
    let mut cache = SolverCache::build(&grid, levels, &mprob, &s, eps.clone());
    let cfg = ModalConfig::new(6);
    let t0 = Instant::now();
    let res = analyze(&mut cache.solver, &grid.scale, BEAM_RHO, &cfg, |_, _, _| {})
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
    bench_beam_suite(&mut m);
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
