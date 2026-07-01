// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Build-sim H-fixture demo. Runs the (single-shot) inherent-strain warp on an
//! H-shape and writes viewable STLs of the exaggerated deformation.
//!
//! NOTE: single-shot scales the H ~uniformly (bonded adds a bed-pinned tilt);
//! the *distinct cross-bar shrink line* is a sequential-activation effect and
//! will appear once the layer loop lands. These files are the baseline to
//! contrast against. Run: `cargo run -p filasim-core --bin buildsim_h [out_dir]`.

use filasim_core::buildsim::{
    deformed_hull_stl, h_grid, solve_bonded, solve_sequential_bonded, solve_warp,
};
use filasim_core::solve::SolveSettings;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let beta: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(-0.003);
    let exag: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(6.0);
    std::fs::create_dir_all(&out).expect("create out dir");

    let (nx, ny, nz, h) = (40usize, 8usize, 60usize, 1.0f64);
    let (cross_z0, cross_h) = (26usize, 8usize);
    let grid = h_grid(nx, ny, nz, h, 8, cross_z0, cross_h);
    println!("beta {beta} (shrink), exag {exag}x");
    let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };

    let free = solve_warp(&grid, [beta, beta, beta], &s).expect("warp");
    let bonded = solve_bonded(&grid, [beta, beta, beta], &s).expect("bonded");
    let (seq, iters) = solve_sequential_bonded(&grid, [beta, beta, beta], &s).expect("sequential");

    let write = |name: &str, bytes: Vec<u8>| {
        let p = out.join(name);
        std::fs::write(&p, bytes).expect("write stl");
        println!("wrote {}", p.display());
    };
    write("h_undeformed.stl", deformed_hull_stl(&grid, &free, 0.0));
    write("h_free.stl", deformed_hull_stl(&grid, &free, exag));
    write("h_bonded_singleshot.stl", deformed_hull_stl(&grid, &bonded, exag));
    write("h_bonded_sequential.stl", deformed_hull_stl(&grid, &seq, exag));

    // MGCG conditioning watch (quiet elements).
    let (imin, imax) = (iters.iter().min().unwrap(), iters.iter().max().unwrap());
    let imean = iters.iter().sum::<usize>() as f64 / iters.len() as f64;
    println!("\nMGCG iterations/layer over {} layers: min {imin}, mean {imean:.0}, max {imax}", iters.len());

    // Left-leg lateral shrink (ux) vs height — the cross-bar (z in 26..34) should
    // show a kink in SEQUENTIAL that single-shot lacks.
    println!("\n z(mm) | singleshot ux | sequential ux   (left leg, +x = inward)");
    let xline = 2.0;
    let ymid = ny as f64 * 0.5;
    for zi in (2..nz).step_by(4) {
        let z = zi as f64 * h;
        let us = bonded.sample_displacement([xline, ymid, z])[0];
        let uq = seq.sample_displacement([xline, ymid, z])[0];
        let mark = if zi >= cross_z0 && zi < cross_z0 + cross_h { " <- cross-bar" } else { "" };
        println!(" {z:5.0} | {us:+12.4}  | {uq:+12.4}{mark}");
    }
}
