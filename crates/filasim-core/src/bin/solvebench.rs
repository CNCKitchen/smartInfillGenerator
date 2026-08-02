// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Solver research bench: MGCG iteration counts / wall time on REAL parts
//! across a mesh-refinement sweep, so the h-dependence of convergence is
//! visible. Grids are cached on disk (voxelizing the Benchy at 3M cells costs
//! more than the solve).
//!
//!   cargo run --release -p filasim-core --bin solvebench -- [cases...] [flags]
//!
//! Cases: benchy hook michol surface beam plate<N>   (default: benchy hook michol)
//!   `plate<N>` is a solid plate N cells thick — the semicoarsening fixture.
//! Flags: --cells a,b,c   target solid-cell counts (default 100k,300k,1M)
//!        --levels N      cap the hierarchy depth (default: SolveSettings)
//!        --tol T         MGCG relative-residual tolerance (default 1e-5)
//!        --profile       time one apply64 vs one V-cycle (the cost split)
//!        --kernel        isolated finest-level f32 matvec throughput
//!        --occ           live/dead DOF census at several block granularities
//!        --hist          per-cell stiffness histogram (contrast diagnostic)
//!        --trace         residual history + asymptotic reduction factor
//!        --nested        solve at 2h first and use it as the initial guess
//!                        (measures the ceiling of any full-multigrid start)
//!        --epsfloor F    floor the EXACT operator's eps (contrast diagnostic)

use filasim_core::mesh::TriMesh;
use filasim_core::solve::{active_nodes, boundary_nodes, solve_cached, SolverCache};
use filasim_core::{pad_for_levels, NodeProblem, SolveSettings, VoxelGrid};
use std::time::Instant;

fn cache_dir() -> std::path::PathBuf {
    let d = std::path::PathBuf::from(
        std::env::var("SOLVEBENCH_CACHE").unwrap_or_else(|_| "target/solvebench-cache".into()),
    );
    let _ = std::fs::create_dir_all(&d);
    d
}

fn save_grid(path: &std::path::Path, g: &VoxelGrid) {
    let mut buf = Vec::with_capacity(g.scale.len() * 4 + 64);
    for v in [g.nx as u64, g.ny as u64, g.nz as u64] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf.extend_from_slice(&g.h.to_le_bytes());
    for v in g.origin {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for v in &g.scale {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    let _ = std::fs::write(path, buf);
}

fn load_grid(path: &std::path::Path) -> Option<VoxelGrid> {
    let b = std::fs::read(path).ok()?;
    if b.len() < 64 {
        return None;
    }
    let rd_u64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) as usize;
    let rd_f64 = |o: usize| f64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let (nx, ny, nz) = (rd_u64(0), rd_u64(8), rd_u64(16));
    let h = rd_f64(24);
    let origin = [rd_f64(32), rd_f64(40), rd_f64(48)];
    let n = nx * ny * nz;
    if b.len() != 56 + 4 * n {
        return None;
    }
    let mut scale = vec![0f32; n];
    for (i, s) in scale.iter_mut().enumerate() {
        *s = f32::from_le_bytes(b[56 + 4 * i..60 + 4 * i].try_into().unwrap());
    }
    Some(VoxelGrid { nx, ny, nz, h, origin, scale })
}

/// Voxelize `stl` so the SOLID cell count lands near `target`, with disk cache.
fn grid_for(stl: &str, target: f64) -> VoxelGrid {
    let key = format!(
        "{}-{}.grid",
        stl.replace(['/', '\\', ' ', '.'], "_"),
        (target as u64)
    );
    let path = cache_dir().join(key);
    if let Some(g) = load_grid(&path) {
        return g;
    }
    let bytes = std::fs::read(stl).unwrap_or_else(|e| panic!("read {stl}: {e}"));
    let mesh = TriMesh::from_stl(&bytes).expect("parse stl");
    let (lo, hi) = mesh.bounds().expect("bounds");
    let bbox = (hi[0] - lo[0]) as f64 * (hi[1] - lo[1]) as f64 * (hi[2] - lo[2]) as f64;
    // Two-pass: guess from the bbox, measure the solid fill, rescale.
    let mut h = (bbox / target).cbrt();
    let probe = VoxelGrid::voxelize(&mesh, h * 2.0);
    let fill = probe.solid_count() as f64 * (h * 2.0).powi(3);
    h = (fill / target).cbrt().max(1e-3);
    let g = VoxelGrid::voxelize(&mesh, h);
    save_grid(&path, &g);
    g
}

fn box_grid(target: f64) -> VoxelGrid {
    // 8:2:1 solid block at the requested cell count.
    let n = (target / 16.0).cbrt().max(2.0);
    let (nx, ny, nz) = ((8.0 * n) as usize, (2.0 * n) as usize, n as usize);
    VoxelGrid::solid_box(nx.max(8), ny.max(2), nz.max(1), 1.0)
}

/// Thin plate: 1-2 cells through thickness at every resolution — the
/// "thin-feature mode" the negative-results note in mg.rs blames for the
/// Benchy's iteration count. Scales in-plane only.
fn plate_grid(target: f64, thick: usize) -> VoxelGrid {
    let n = (target / thick as f64).sqrt().max(4.0) as usize;
    VoxelGrid::solid_box(n, n, thick, 1.0)
}

struct Case {
    name: &'static str,
    grid: VoxelGrid,
}

/// Canonical well-posed load case on any grid: fix the boundary nodes in the
/// lowest 6% of z, push the top 6% down (total 50 N).
fn problem_for(grid: &VoxelGrid) -> NodeProblem {
    let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
    let active = active_nodes(grid);
    let bnd = boundary_nodes(grid);
    // z extent of ACTIVE nodes (the padding is void).
    let mut zlo = usize::MAX;
    let mut zhi = 0usize;
    for n in 0..mx * my * mz {
        if active[n] {
            let z = n / (mx * my);
            zlo = zlo.min(z);
            zhi = zhi.max(z);
        }
    }
    let span = (zhi - zlo).max(1);
    let band = ((span as f64 * 0.06).ceil() as usize).max(1);
    let mut fixed = Vec::new();
    let mut load = Vec::new();
    for &n in &bnd {
        let z = n as usize / (mx * my);
        if z <= zlo + band {
            fixed.push(n);
        } else if z + band >= zhi {
            load.push(n);
        }
    }
    assert!(!fixed.is_empty() && !load.is_empty(), "degenerate BC bands");
    let f = -50.0 / load.len() as f64;
    NodeProblem {
        fixed,
        springs: Vec::new(),
        forces: load.into_iter().map(|n| (n, [0.0, 0.0, f])).collect(),
        rigid: Vec::new(),
        prescribed: Vec::new(),
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    #[cfg(feature = "parallel")]
    println!("threads: {}", rayon::current_num_threads());

    let cells: Vec<f64> = arg_value(&args, "--cells")
        .map(|s| s.split(',').filter_map(|v| v.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![100_000.0, 300_000.0, 1_000_000.0]);
    let tol: f64 = arg_value(&args, "--tol").and_then(|v| v.parse().ok()).unwrap_or(1e-5);
    let max_levels = arg_value(&args, "--levels")
        .and_then(|v| v.parse().ok())
        .unwrap_or(SolveSettings::default().max_levels);
    let profile = args.iter().any(|a| a == "--profile");
    let hist = args.iter().any(|a| a == "--hist");
    let kernel = args.iter().any(|a| a == "--kernel");
    let modal = args.iter().any(|a| a == "--modal");
    let occupancy = args.iter().any(|a| a == "--occ");
    let trace = args.iter().any(|a| a == "--trace");
    let nested = args.iter().any(|a| a == "--nested");
    let epsfloor: Option<f32> = arg_value(&args, "--epsfloor").and_then(|v| v.parse().ok());

    let wanted: Vec<String> = {
        // Positional case names: skip flags AND the value that follows a
        // value-taking flag.
        let mut list: Vec<String> = Vec::new();
        let mut skip = false;
        for a in &args[1..] {
            if skip {
                skip = false;
                continue;
            }
            if a.starts_with("--") {
                skip = matches!(a.as_str(), "--cells" | "--tol" | "--levels" | "--epsfloor");
                continue;
            }
            list.push(a.clone());
        }
        if list.is_empty() {
            vec!["benchy".into(), "hook".into(), "michol".into()]
        } else {
            list
        }
    };

    println!(
        "{:<10} {:>9} {:>9} {:>8} {:>7} {:>6} {:>16} {:>8} {:>9} {:>11}",
        "case", "cells", "solid", "dof/M", "levels", "iters", "coarsest", "time_s", "ms/iter", "max_u"
    );
    println!("{}", "-".repeat(112));

    for name in &wanted {
        for &target in &cells {
            let raw = match name.as_str() {
                "benchy" => grid_for("3dbenchy.stl", target),
                "hook" => grid_for("hook5 v3.stl", target),
                "michol" => grid_for("MicHolder_Inserts.stl", target),
                "surface" => grid_for("surface.stl", target),
                "beam" => box_grid(target),
                name if name.starts_with("plate") => {
                    let t: usize = name[5..].parse().unwrap_or(3);
                    plate_grid(target, t)
                }
                other => {
                    eprintln!("unknown case {other}");
                    continue;
                }
            };
            let case = Case { name: Box::leak(name.clone().into_boxed_str()), grid: raw };
            let s = SolveSettings { tol, max_levels, max_iter: 3000, ..Default::default() };
            let (grid, levels) = pad_for_levels(&case.grid, s.max_levels);
            let problem = problem_for(&grid);
            let mut eps = filasim_core::solve::grid_eps(&grid);
            // Diagnostic: floor the EXACT operator's per-cell stiffness (not just
            // the preconditioner's) — isolates how much of the iteration count is
            // near-zero-stiffness boundary slivers polluting K's spectrum.
            if let Some(f) = epsfloor {
                for e in eps.iter_mut() {
                    if *e > 0.0 {
                        *e = e.max(f);
                    }
                }
            }
            if occupancy {
                // How much of the padded node grid is DEAD (no incident solid
                // cell)? Every vector op, smoother pass and reduction currently
                // streams those DOFs even though they are provably zero.
                let act = active_nodes(&grid);
                let n = act.len();
                let live = act.iter().filter(|&&a| a).count();
                let mut report = String::new();
                for &c in &[64usize, 512, 4096] {
                    let chunks = n.div_ceil(c);
                    let dead = (0..chunks)
                        .filter(|i| act[i * c..((i + 1) * c).min(n)].iter().all(|&a| !a))
                        .count();
                    report.push_str(&format!(
                        "  chunk{c}: {:.0}% fully dead",
                        100.0 * dead as f64 / chunks as f64
                    ));
                }
                println!(
                    "           nodes {n} | live {live} ({:.0}%) | dead {:.0}%{report}",
                    100.0 * live as f64 / n as f64,
                    100.0 * (n - live) as f64 / n as f64,
                );
            }
            if hist {
                let mut b = [0usize; 7];
                for &e in eps.iter().filter(|&&e| e > 0.0) {
                    let k = match e {
                        x if x < 1e-4 => 0,
                        x if x < 1e-3 => 1,
                        x if x < 1e-2 => 2,
                        x if x < 0.05 => 3,
                        x if x < 0.2 => 4,
                        x if x < 0.99 => 5,
                        _ => 6,
                    };
                    b[k] += 1;
                }
                println!(
                    "           eps histogram (solid cells): <1e-4 {} | <1e-3 {} | <1e-2 {} | <0.05 {} | <0.2 {} | <0.99 {} | =1 {}",
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6]
                );
            }

            // NESTED-START PROBE (does an FMG-style coarse-first solve pay?):
            // solve the SAME part on a 2x coarser grid, trilinearly sample that
            // field onto the fine node grid, and hand it to the fine solve as the
            // initial guess. Measures the ceiling of any full-multigrid start
            // without committing to one.
            let warm: Option<Vec<f64>> = if nested {
                let mut coarse_raw = case.grid.clone();
                // Rebuild at 2h by 2x2x2 occupancy averaging of the fine grid.
                let (cnx, cny, cnz) = (coarse_raw.nx / 2, coarse_raw.ny / 2, coarse_raw.nz / 2);
                let mut cscale = vec![0f32; cnx * cny * cnz];
                for cz in 0..cnz {
                    for cy in 0..cny {
                        for cx in 0..cnx {
                            let mut acc = 0f32;
                            for dz in 0..2 {
                                for dy in 0..2 {
                                    for dx in 0..2 {
                                        acc += coarse_raw.scale[((2 * cz + dz) * coarse_raw.ny
                                            + 2 * cy + dy)
                                            * coarse_raw.nx
                                            + 2 * cx + dx];
                                    }
                                }
                            }
                            cscale[(cz * cny + cy) * cnx + cx] = acc / 8.0;
                        }
                    }
                }
                coarse_raw = VoxelGrid {
                    nx: cnx, ny: cny, nz: cnz,
                    h: case.grid.h * 2.0,
                    origin: case.grid.origin,
                    scale: cscale,
                };
                let (cg, clv) = pad_for_levels(&coarse_raw, s.max_levels);
                let cprob = problem_for(&cg);
                let ceps = filasim_core::solve::grid_eps(&cg);
                let t0 = Instant::now();
                match solve_cached(&mut None, &cg, clv, &cprob, &s, ceps, tol, s.max_iter) {
                    Ok(cr) => {
                        let csol = cr.into_solution(&cg);
                        let dt_c = t0.elapsed().as_secs_f64();
                        let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
                        let mut u0 = vec![0f64; 3 * mx * my * mz];
                        for n in 0..mx * my * mz {
                            let x = n % mx;
                            let y = (n / mx) % my;
                            let z = n / (mx * my);
                            let p = [
                                grid.origin[0] + x as f64 * grid.h,
                                grid.origin[1] + y as f64 * grid.h,
                                grid.origin[2] + z as f64 * grid.h,
                            ];
                            let d = csol.sample_displacement(p);
                            u0[3 * n] = d[0];
                            u0[3 * n + 1] = d[1];
                            u0[3 * n + 2] = d[2];
                        }
                        println!(
                            "           nested: coarse grid {}x{}x{} solved in {} iters / {:.2}s (= {:.2} fine-iter equivalents)",
                            cg.nx, cg.ny, cg.nz, csol.iterations, dt_c, dt_c
                        );
                        Some(u0)
                    }
                    Err(e) => {
                        println!("           nested: coarse solve failed: {e}");
                        None
                    }
                }
            } else {
                None
            };
            let mut slot = warm.map(|u0| {
                let mut c = SolverCache::build(&grid, levels, &problem, &s, eps.clone());
                for (dst, src) in c.last_u.iter_mut().zip(&u0) {
                    *dst = *src;
                }
                c
            });
            let t0 = Instant::now();
            let r = match solve_cached(&mut slot, &grid, levels, &problem, &s, eps.clone(), tol, s.max_iter)
            {
                Ok(r) => r,
                Err(e) => {
                    println!("{:<10} {target:>9.0}  FAILED: {e}", case.name);
                    continue;
                }
            };
            let dt = t0.elapsed().as_secs_f64();
            let mut maxu = 0f64;
            for c in r.u.chunks(3) {
                maxu = maxu.max(c[0] * c[0] + c[1] * c[1] + c[2] * c[2]);
            }
            let maxu = maxu.sqrt();
            if trace {
                let t = &r.residuals;
                let pick: Vec<String> = (0..t.len())
                    .step_by((t.len() / 24).max(1))
                    .map(|i| format!("{i}:{:.1e}", t[i]))
                    .collect();
                println!("           residual trace: {}", pick.join(" "));
                // Per-iteration reduction factor over the last half of the run.
                let h = t.len() / 2;
                if t.len() > 4 {
                    let rate = (t[t.len() - 1] as f64 / t[h] as f64)
                        .powf(1.0 / (t.len() - 1 - h) as f64);
                    println!("           asymptotic reduction factor {rate:.4} / iteration");
                }
            }

            // Coarsest-level dims for the hierarchy-depth diagnostic.
            let cache = SolverCache::build(&grid, levels, &problem, &s, eps.clone());
            let cl = cache.solver.levels.last().unwrap();
            let coarsest = format!("{}x{}x{}", cl.nx, cl.ny, cl.nz);

            println!(
                "{:<10} {:>9} {:>9} {:>8.2} {:>7} {:>6} {:>16} {:>8.2} {:>9.1} {:>11.5}",
                case.name,
                grid.cell_count(),
                grid.solid_count(),
                3.0 * (grid.nx + 1) as f64 * (grid.ny + 1) as f64 * (grid.nz + 1) as f64 / 1e6,
                levels,
                r.stats.iterations,
                coarsest,
                dt,
                dt * 1000.0 / r.stats.iterations.max(1) as f64,
                maxu,
            );

            if modal {
                // Same hierarchy, driven by the LOBPCG eigensolver instead of CG:
                // modal spends nearly all its time in V-cycles + the f64 matvec,
                // so it inherits the same wins.
                let mut cache = SolverCache::build(&grid, levels, &problem, &s, eps.clone());
                let cfg = filasim_core::modal::ModalConfig::new(6);
                let t = Instant::now();
                let res = filasim_core::modal::analyze(
                    &mut cache.solver, &grid.scale, 1.24e-9, &[], &cfg, |_, _, _| {},
                )
                .expect("modal");
                let f: Vec<String> =
                    res.freqs_hz.iter().take(3).map(|v| format!("{v:.2}")).collect();
                println!(
                    "           modal: {:.2} s | {} outer iters | {} V-cycles | f1..f3 = {} Hz",
                    t.elapsed().as_secs_f64(),
                    res.outer_iters,
                    res.total_inner_iters,
                    f.join(", "),
                );
            }

            if kernel {
                // Isolated finest-level kernel throughput: the f32 `apply`
                // (45% of a V-cycle) and one Chebyshev-style dinv pass.
                let cache = &cache;
                let lvl = &cache.solver.levels[0];
                let n = lvl.ndof();
                let x = vec![1e-3f32; n];
                let mut y = vec![0f32; n];
                let reps = 20;
                let t = Instant::now();
                for _ in 0..reps {
                    lvl.apply(&x, &mut y);
                }
                let ta = t.elapsed().as_secs_f64() / reps as f64;
                let solid = grid.solid_count() as f64;
                println!(
                    "           kernel: apply_f32 {:.2} ms  ({:.0} Mcell/s, {:.1} GFLOP/s eff)",
                    ta * 1000.0,
                    solid / 1e6 / ta,
                    solid * 1152.0 / 1e9 / ta,
                );
            }

            if profile {
                let mut cache = cache;
                let n = cache.solver.levels[0].ndof();
                let x = vec![1e-3f64; n];
                let mut y = vec![0f64; n];
                let reps = 5;
                let t = Instant::now();
                for _ in 0..reps {
                    cache.solver.apply_k(&x, &mut y);
                }
                let t_apply = t.elapsed().as_secs_f64() / reps as f64;
                let mut z = vec![0f64; n];
                let t = Instant::now();
                for _ in 0..reps {
                    cache.solver.precondition(&x, &mut z);
                }
                let t_vc = t.elapsed().as_secs_f64() / reps as f64;
                println!(
                    "           profile: apply64 {:.1} ms | V-cycle {:.1} ms | iter≈{:.1} ms",
                    t_apply * 1000.0,
                    t_vc * 1000.0,
                    (t_apply + t_vc) * 1000.0
                );
            }
        }
    }
}
