// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Build-sim on a real 3DBenchy — the project's north-star fixture. Voxelizes
//! the STL, runs single-shot vs sequential inherent-strain warp (bonded to the
//! bed), and writes exaggerated deformed hulls so the "hull line" warp signature
//! can be inspected.
//!
//! Run: `cargo run -p filasim-core --bin buildsim_benchy -- <benchy.stl> [out_dir] [h_mm]`

use filasim_core::buildsim::{deformed_hull_stl, solve_bonded, solve_build};
use filasim_core::mesh::TriMesh;
use filasim_core::solve::SolveSettings;
use filasim_core::voxel::VoxelGrid;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let stl = args.next().unwrap_or_else(|| "3dbenchy.stl".into());
    let out = PathBuf::from(args.next().unwrap_or_else(|| "buildsim_out".into()));
    let h: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    // beta = isotropic shrink fraction (0.003 ~ PLA, uncalibrated); exag = STL
    // display magnification. Both tunable: `... <stl> <out> <h> <beta> <exag>`.
    let beta: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(-0.003);
    let exag: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(4.0);
    std::fs::create_dir_all(&out).expect("create out dir");

    let data = std::fs::read(&stl).expect("read stl");
    let mesh = TriMesh::from_stl(&data).expect("parse stl");
    let (lo, hi) = mesh.bounds().expect("bounds");
    println!(
        "{}: {} tris, bbox {:.1} x {:.1} x {:.1} mm",
        stl,
        mesh.len(),
        hi[0] - lo[0],
        hi[1] - lo[1],
        hi[2] - lo[2]
    );

    let t0 = Instant::now();
    let grid = VoxelGrid::voxelize(&mesh, h);
    println!(
        "voxelize @ {h} mm -> {}x{}x{} = {} solid cells ({:.1}s)",
        grid.nx,
        grid.ny,
        grid.nz,
        grid.solid_count(),
        t0.elapsed().as_secs_f64()
    );

    let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };
    println!("beta {beta} (shrink), exag {exag}x");

    let t1 = Instant::now();
    let bonded_ss = solve_bonded(&grid, [beta, beta, beta], &s).expect("single-shot");
    let t_ss = t1.elapsed().as_secs_f64();

    let t2 = Instant::now();
    let r = solve_build(&grid, [beta, beta, beta], &s, None).expect("build"); // State 1+2 + peel
    let t_sq = t2.elapsed().as_secs_f64();

    let write = |name: &str, bytes: Vec<u8>| {
        let p = out.join(name);
        std::fs::write(&p, bytes).expect("write stl");
        println!("wrote {}", p.display());
    };
    write("benchy_undeformed.stl", deformed_hull_stl(&grid, &r.bonded, 0.0));
    write("benchy_singleshot.stl", deformed_hull_stl(&grid, &bonded_ss, exag));
    write("benchy_bonded.stl", deformed_hull_stl(&grid, &r.bonded, exag));
    write("benchy_released.stl", deformed_hull_stl(&grid, &r.released, exag));

    let (imin, imax) = (r.iters.iter().min().unwrap(), r.iters.iter().max().unwrap());
    let imean = r.iters.iter().sum::<usize>() as f64 / r.iters.len() as f64;
    println!("\nsingle-shot   : {:.1}s, max disp {:.3} mm", t_ss, bonded_ss.max_displacement());
    println!(
        "seq build     : {:.1}s over {} layers (MGCG/layer min {imin} mean {imean:.0} max {imax})",
        t_sq,
        r.iters.len()
    );
    println!("  bonded  max disp {:.3} mm", r.bonded.max_displacement());
    println!("  released max disp {:.3} mm (off-bed sprung shape)", r.released.max_displacement());

    // Peel summary: peak upward (+Z) bed reaction and peak shear.
    let mut peak_lift = 0.0f64;
    let mut peak_shear = 0.0f64;
    for (_, rv) in &r.bed_reaction {
        peak_lift = peak_lift.max(rv[2]);
        peak_shear = peak_shear.max((rv[0] * rv[0] + rv[1] * rv[1]).sqrt());
    }
    println!("  peel: peak bed lift (+Z) {peak_lift:.3} N, peak shear {peak_shear:.3} N (uncalibrated)");
    println!("(deformed STLs exaggerated {exag}x)");
}
