// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Phase-1 exit-criterion benchmark:
//!   1. Winding-number voxelization of a ~16k-triangle sphere at fine resolution.
//!   2. Cantilever MGCG solves at 131k and ~1.05M cells, with the
//!      Timoshenko-ratio printed as a built-in correctness check.

use sig_core::mesh::primitives;
use sig_core::{solve_static, BoxRegion, SolveSettings, StaticProblem, VoxelGrid};
use std::time::Instant;

fn bench_voxelize() {
    let sph = primitives::sphere([0.0; 3], 25.0, 128, 64);
    let t0 = Instant::now();
    let grid = VoxelGrid::voxelize(&sph, 0.3);
    let dt = t0.elapsed();
    println!(
        "voxelize: {} tris -> {}x{}x{} = {:.2}M cells ({:.2}M solid) in {:.2} s  ({:.1} Mcells/s)",
        sph.len(),
        grid.nx,
        grid.ny,
        grid.nz,
        grid.cell_count() as f64 / 1e6,
        grid.solid_count() as f64 / 1e6,
        dt.as_secs_f64(),
        grid.cell_count() as f64 / 1e6 / dt.as_secs_f64()
    );
    let exact = 4.0 / 3.0 * std::f64::consts::PI * 25.0f64.powi(3);
    println!(
        "          volume check: {:.0} mm3 vs analytic {:.0} mm3 ({:+.2}%)",
        grid.solid_volume(),
        exact,
        100.0 * (grid.solid_volume() - exact) / exact
    );
}

fn bench_cantilever(nx: usize, ny: usize, nz: usize, h: f64) {
    let (e0, nu, f) = (2000.0f64, 0.3f64, -10.0f64);
    let (l, bdim, hdim) = (nx as f64 * h, ny as f64 * h, nz as f64 * h);

    let grid = VoxelGrid::solid_box(nx, ny, nz, h);
    let cells = grid.cell_count();
    let problem = StaticProblem {
        grid,
        fixed: vec![BoxRegion::new([-0.1, -1.0, -1.0], [0.1, bdim + 1.0, hdim + 1.0])],
        loads: vec![(
            BoxRegion::new([l - 0.1 * h, -1.0, -1.0], [l + h, bdim + 1.0, hdim + 1.0]),
            [0.0, 0.0, f],
        )],
        settings: SolveSettings { e0, nu, tol: 1e-5, max_iter: 300, ..Default::default() },
    };

    let t0 = Instant::now();
    let sol = solve_static(&problem).expect("solve failed");
    let dt = t0.elapsed();

    let tip = sol
        .mean_displacement(&BoxRegion::new(
            [l - 0.1 * h, -1.0, -1.0],
            [l + h, bdim + 1.0, hdim + 1.0],
        ))
        .unwrap();
    let inertia = bdim * hdim.powi(3) / 12.0;
    let area = bdim * hdim;
    let g = e0 / (2.0 * (1.0 + nu));
    let kappa = 10.0 * (1.0 + nu) / (12.0 + 11.0 * nu);
    let exact = f * l.powi(3) / (3.0 * e0 * inertia) + f * l / (kappa * g * area);

    println!(
        "solve:    {}x{}x{} = {:.2}M cells ({:.2}M dof): {:.2} s, {} MGCG iters ({:.0} ms/iter), res {:.1e}",
        nx,
        ny,
        nz,
        cells as f64 / 1e6,
        3.0 * (nx + 1) as f64 * (ny + 1) as f64 * (nz + 1) as f64 / 1e6,
        dt.as_secs_f64(),
        sol.iterations,
        dt.as_secs_f64() * 1000.0 / sol.iterations as f64,
        sol.rel_residual
    );
    println!(
        "          tip uz {:.4} mm vs Timoshenko {:.4} mm (ratio {:.3})",
        tip[2],
        exact,
        tip[2] / exact
    );
}

/// Real-part case: 3DBenchy (thin shells, jagged boundaries — the MGCG
/// iteration-count worst case). Fix the hull bottom, press the top down.
fn bench_benchy() {
    let bytes = match std::fs::read("3dbenchy.stl") {
        Ok(b) => b,
        Err(_) => {
            println!("benchy:   3dbenchy.stl not found (run from repo root) — skipped");
            return;
        }
    };
    let mesh = sig_core::mesh::TriMesh::from_stl(&bytes).expect("benchy parse");
    let (lo, hi) = mesh.bounds().unwrap();
    let vol = (hi[0] - lo[0]) * (hi[1] - lo[1]) * (hi[2] - lo[2]);
    let h = sig_core::voxel::pick_voxel_size(vol, 300_000.0, 0.0);
    let t0 = Instant::now();
    let grid = VoxelGrid::voxelize(&mesh, h);
    let t_vox = t0.elapsed();
    let problem = StaticProblem {
        grid,
        fixed: vec![BoxRegion::new(
            [lo[0] - 1.0, lo[1] - 1.0, lo[2] - 1.0],
            [hi[0] + 1.0, hi[1] + 1.0, lo[2] + 1.0],
        )],
        loads: vec![(
            BoxRegion::new(
                [lo[0] - 1.0, lo[1] - 1.0, hi[2] - 3.0],
                [hi[0] + 1.0, hi[1] + 1.0, hi[2] + 1.0],
            ),
            [0.0, 0.0, -50.0],
        )],
        settings: SolveSettings {
            // Hierarchy-depth diagnostic: SIG_LEVELS=N caps the level count.
            max_levels: std::env::var("SIG_LEVELS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(SolveSettings::default().max_levels),
            ..SolveSettings::default()
        },
    };
    // Opt-in (SIG_PROGRESS=1): install a residual-progress sink that mimics
    // the web worker's copy into a shared buffer (a reused Vec, no allocation
    // in the hot path) so the live-preview publish overhead is measurable
    // natively. Without it the stride'd `publish` is just a thread-local
    // None-check — what every normal solve pays.
    if std::env::var("SIG_PROGRESS").is_ok() {
        let buf = std::cell::RefCell::new(Vec::<f32>::with_capacity(1024));
        sig_core::progress::set_sink(Some(Box::new(move |trace: &[f32]| {
            let mut b = buf.borrow_mut();
            b.clear();
            b.extend_from_slice(trace);
        })));
        println!("benchy:   live-progress sink installed (measuring publish overhead)");
    }
    let t0 = Instant::now();
    let sol = solve_static(&problem).expect("benchy solve");
    let dt = t0.elapsed();
    println!(
        "benchy:   {:.2}M-cell grid: voxelize {:.2} s, solve {:.2} s, {} MGCG iters ({:.0} ms/iter), max u {:.3} mm, res {:.1e}",
        problem.grid.cell_count() as f64 / 1e6,
        t_vox.as_secs_f64(),
        dt.as_secs_f64(),
        sol.iterations,
        dt.as_secs_f64() * 1000.0 / sol.iterations.max(1) as f64,
        sol.max_displacement(),
        sol.rel_residual,
    );
}

fn main() {
    #[cfg(feature = "parallel")]
    println!("threads: {}", rayon::current_num_threads());
    #[cfg(not(feature = "parallel"))]
    println!("threads: 1 (sequential build)");

    let big = !std::env::args().any(|a| a == "--small");
    let benchy_only = std::env::args().any(|a| a == "--benchy");

    if benchy_only {
        bench_benchy();
        return;
    }
    bench_voxelize();
    bench_cantilever(128, 32, 32, 0.5);
    if big {
        bench_cantilever(256, 64, 64, 0.25);
    }
    bench_benchy();
}
