// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Phase-2 engine tests: segmentation, closest-point queries, island and
//! rigid-body-mode checks, triangle-selection BC attachment, frictionless
//! penalty springs, gravity.

use sig_core::attach::{assemble, check_problem, BcKind, BcSpec};
use sig_core::bvh::WindingBvh;
use sig_core::check::{islands, rbm_check, ConstraintDir};
use sig_core::mesh::{primitives, TriMesh};
use sig_core::segment::segment;
use sig_core::{pad_for_levels, solve_nodes, BoxRegion, SolveSettings, VoxelGrid};

#[test]
fn segmentation_box_has_six_patches() {
    let cube = primitives::boxx([0.0; 3], [10.0, 20.0, 5.0]);
    let seg = segment(&cube, 30.0);
    assert_eq!(seg.patch_count, 6, "box should segment into 6 faces");
    let mut sizes = vec![0usize; 6];
    for &p in &seg.patch_of_tri {
        sizes[p as usize] += 1;
    }
    assert!(sizes.iter().all(|&s| s == 2), "each face patch should hold 2 triangles");

    // A sphere at fine angle threshold stays one patch (smooth surface).
    let sph = primitives::sphere([0.0; 3], 10.0, 48, 24);
    let seg2 = segment(&sph, 30.0);
    assert_eq!(seg2.patch_count, 1, "smooth sphere should be a single patch");
}

#[test]
fn closest_triangle_matches_brute_force() {
    let sph = primitives::sphere([2.0, -1.0, 0.5], 8.0, 24, 12);
    let bvh = WindingBvh::build(&sph);
    let mut seed = 7u64;
    let mut rnd = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 33) as f64 / (1u64 << 31) as f64 - 1.0) * 15.0
    };
    for _ in 0..200 {
        let q = [rnd(), rnd(), rnd()];
        let (_, d2) = bvh.closest_triangle(q);
        // Brute force reference.
        let mut best = f64::INFINITY;
        for t in &sph.tris {
            let mut t64 = [0f64; 9];
            for k in 0..9 {
                t64[k] = t[k] as f64;
            }
            // distance via sampling barycentric grid would be slow; reuse the
            // engine routine indirectly by building a 1-triangle BVH.
            let one = TriMesh::from_triangles(vec![*t]);
            let b1 = WindingBvh::build(&one);
            let (_, d) = b1.closest_triangle(q);
            best = best.min(d);
        }
        assert!(
            (d2 - best).abs() <= 1e-9 * (1.0 + best),
            "closest mismatch: bvh {d2} vs brute {best} at {q:?}"
        );
    }
}

#[test]
fn islands_detects_disconnected_solids() {
    // Two separated boxes in one mesh.
    let mut m = primitives::boxx([0.0; 3], [10.0, 5.0, 5.0]);
    let b2 = primitives::boxx([20.0, 0.0, 0.0], [30.0, 5.0, 5.0]);
    m.tris.extend_from_slice(&b2.tris);
    let grid = VoxelGrid::voxelize(&m, 1.0);
    let isl = islands(&grid);
    assert_eq!(isl.count, 2, "expected two solid components");

    let single = VoxelGrid::voxelize(&primitives::boxx([0.0; 3], [10.0, 5.0, 5.0]), 1.0);
    assert_eq!(islands(&single).count, 1);
}

#[test]
fn rbm_rank_test_finds_free_modes() {
    // Full plane of fixed nodes: all 6 modes killed.
    let mut cons = Vec::new();
    for y in 0..5 {
        for z in 0..5 {
            let p = [0.0, y as f64, z as f64];
            for d in 0..3 {
                let mut dir = [0f64; 3];
                dir[d] = 1.0;
                cons.push(ConstraintDir { pos: p, dir });
            }
        }
    }
    let r = rbm_check(&cons, [5.0, 2.0, 2.0], 5.0);
    assert!(r.ok, "fixed plane should fully constrain (ratio {})", r.lambda_ratio);

    // A single fixed point: rotations free.
    let mut cons1 = Vec::new();
    for d in 0..3 {
        let mut dir = [0f64; 3];
        dir[d] = 1.0;
        cons1.push(ConstraintDir { pos: [1.0, 2.0, 3.0], dir });
    }
    let r1 = rbm_check(&cons1, [1.0, 2.0, 3.0], 5.0);
    assert!(!r1.ok);
    let mode = r1.mode.expect("mode expected");
    let rot = (mode.r[0].powi(2) + mode.r[1].powi(2) + mode.r[2].powi(2)).sqrt();
    assert!(rot > 1e-6, "free mode must be rotational, got {mode:?}");

    // Two fixed points on the x-axis: rotation about x stays free.
    let mut cons2 = Vec::new();
    for px in [0.0, 10.0] {
        for d in 0..3 {
            let mut dir = [0f64; 3];
            dir[d] = 1.0;
            cons2.push(ConstraintDir { pos: [px, 0.0, 0.0], dir });
        }
    }
    let r2 = rbm_check(&cons2, [5.0, 0.0, 0.0], 5.0);
    assert!(!r2.ok);
    let m2 = r2.mode.unwrap();
    let rlen = (m2.r[0].powi(2) + m2.r[1].powi(2) + m2.r[2].powi(2)).sqrt();
    assert!(
        (m2.r[0].abs() / rlen) > 0.99,
        "free rotation should be about x, got r = {:?}",
        m2.r
    );

    // Three orthogonal frictionless planes constrain all 6 modes.
    let mut cons3 = Vec::new();
    for a in 0..5 {
        for b in 0..5 {
            cons3.push(ConstraintDir { pos: [0.0, a as f64, b as f64], dir: [1.0, 0.0, 0.0] });
            cons3.push(ConstraintDir { pos: [a as f64, 0.0, b as f64], dir: [0.0, 1.0, 0.0] });
            cons3.push(ConstraintDir { pos: [a as f64, b as f64, 0.0], dir: [0.0, 0.0, 1.0] });
        }
    }
    let r3 = rbm_check(&cons3, [2.5; 3], 4.0);
    assert!(r3.ok, "3 orthogonal roller planes should constrain (ratio {})", r3.lambda_ratio);
}

/// Box-face triangle ids from primitives::boxx: faces emitted in order
/// -x,+x,-y,+y,-z,+z, two triangles each.
fn face_tris(face: usize) -> Vec<u32> {
    vec![2 * face as u32, 2 * face as u32 + 1]
}

#[test]
fn attach_assemble_check_solve_end_to_end() {
    let beam = primitives::boxx([0.0; 3], [40.0, 6.0, 6.0]);
    let grid0 = VoxelGrid::voxelize(&beam, 1.0);
    let settings = SolveSettings { e0: 2000.0, nu: 0.3, tol: 1e-6, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);

    // Under-constrained first: force only, no support.
    let bcs_bad = vec![BcSpec { kind: BcKind::Force([0.0, 0.0, -5.0]), tris: face_tris(1) }];
    let asm_bad = assemble(&beam, &grid, &bcs_bad, None, &settings).unwrap();
    let report_bad = check_problem(&grid, &asm_bad);
    assert!(!report_bad.ok, "no supports must fail the check");
    assert!(report_bad.components[0].mode.is_some());

    // Proper cantilever: fixed -x face, tip load on +x face.
    let bcs = vec![
        BcSpec { kind: BcKind::Fixed, tris: face_tris(0) },
        BcSpec { kind: BcKind::Force([0.0, 0.0, -5.0]), tris: face_tris(1) },
    ];
    let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
    let report = check_problem(&grid, &asm);
    assert!(report.ok, "cantilever should pass the check: {report:?}");
    assert_eq!(report.island_count, 1);

    let sol = solve_nodes(&grid, levels, &asm.problem, &settings).expect("solve");
    let tip = sol
        .mean_displacement(&BoxRegion::new([39.5, -1.0, -1.0], [40.5, 7.0, 7.0]))
        .unwrap();

    // Compare against beam theory (loose: load attaches over a node band).
    let (l, bdim, hdim, e0, f) = (40.0f64, 6.0f64, 6.0f64, 2000.0f64, -5.0f64);
    let inertia = bdim * hdim.powi(3) / 12.0;
    let g = e0 / 2.6;
    let kappa = 13.0 / 15.3;
    let exact = f * l.powi(3) / (3.0 * e0 * inertia) + f * l / (kappa * g * bdim * hdim);
    let ratio = tip[2] / exact;
    assert!(
        (0.85..=1.1).contains(&ratio),
        "attached cantilever ratio {ratio:.3} (tip {tip:?} vs {exact:.4})"
    );
}

#[test]
fn frictionless_springs_reproduce_roller_patch_test() {
    let bar = primitives::boxx([0.0; 3], [4.0, 2.0, 2.0]);
    let grid0 = VoxelGrid::voxelize(&bar, 1.0);
    let settings = SolveSettings { e0: 1000.0, nu: 0.3, tol: 1e-9, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, 1);

    let bcs = vec![
        BcSpec { kind: BcKind::Frictionless, tris: face_tris(0) }, // x=0
        BcSpec { kind: BcKind::Frictionless, tris: face_tris(2) }, // y=0
        BcSpec { kind: BcKind::Frictionless, tris: face_tris(4) }, // z=0
        BcSpec { kind: BcKind::Force([40.0, 0.0, 0.0]), tris: face_tris(1) }, // sigma=10 MPa * 4 mm^2
    ];
    let asm = assemble(&bar, &grid, &bcs, None, &settings).unwrap();
    let report = check_problem(&grid, &asm);
    assert!(report.ok, "three roller planes should pass: {report:?}");

    let sol = solve_nodes(&grid, levels, &asm.problem, &settings).expect("solve");
    let tip = sol.mean_displacement(&BoxRegion::new([3.9, -1.0, -1.0], [4.1, 3.0, 3.0])).unwrap();
    let exact = 10.0 * 4.0 / 1000.0; // sigma L / E
    let err = (tip[0] - exact).abs() / exact;
    assert!(
        err < 0.03,
        "frictionless patch test: ux {} vs exact {exact} ({:.2}% off)",
        tip[0],
        err * 100.0
    );
}

#[test]
fn gravity_self_weight_cantilever() {
    let beam = primitives::boxx([0.0; 3], [40.0, 6.0, 6.0]);
    let grid0 = VoxelGrid::voxelize(&beam, 1.0);
    let settings = SolveSettings { e0: 2000.0, nu: 0.3, tol: 1e-7, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);

    let density = 1.24e-9; // PLA, tonne/mm^3
    let gvec = [0.0, 0.0, -9810.0]; // mm/s^2
    let bcs = vec![BcSpec { kind: BcKind::Fixed, tris: face_tris(0) }];
    let asm = assemble(&beam, &grid, &bcs, Some((gvec, density)), &settings).unwrap();
    let sol = solve_nodes(&grid, levels, &asm.problem, &settings).expect("solve");
    let tip = sol
        .mean_displacement(&BoxRegion::new([39.5, -1.0, -1.0], [40.5, 7.0, 7.0]))
        .unwrap();

    // delta = q L^4 / (8 E I), q = rho g A
    let (l, a, inertia, e0) = (40.0f64, 36.0f64, 6.0 * 216.0 / 12.0, 2000.0f64);
    let q = density * 9810.0 * a;
    let exact = -q * l.powi(4) / (8.0 * e0 * inertia);
    let ratio = tip[2] / exact;
    assert!(
        (0.85..=1.1).contains(&ratio),
        "gravity cantilever ratio {ratio:.3} (tip {:.3e} vs {exact:.3e})",
        tip[2]
    );
}
