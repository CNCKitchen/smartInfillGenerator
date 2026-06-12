// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! "As printed" verify path: voxel-size snapping to the wall thickness,
//! exact skin layer counts, and the composite-sandwich stiffness check.

use sig_core::attach::{assemble, BcKind, BcSpec};
use sig_core::mesh::primitives;
use sig_core::simp::{build_eps, classify_cells, evaluate, solve_with_eps};
use sig_core::solve::SolveSettings;
use sig_core::voxel::pick_voxel_size;
use sig_core::{pad_for_levels, VoxelGrid};

fn face_tris(face: usize) -> Vec<u32> {
    vec![2 * face as u32, 2 * face as u32 + 1]
}

#[test]
fn voxel_snap_picks_integer_wall_fractions() {
    let vol = 60.0 * 10.0 * 10.0;
    let target = vol / 0.6f64.powi(3); // nominal h0 = 0.6

    // 0.9 mm wall: k = round(0.9/0.6) = 2 -> h = 0.45, and 2·h is the wall.
    let h = pick_voxel_size(vol, target, 0.9);
    assert!((h - 0.45).abs() < 1e-9, "snapped h: {h}");
    assert!((2.0 * h - 0.9).abs() < 1e-9);

    // Wall coarser than nominal still snaps (k = 1 -> h = wall).
    let h1 = pick_voxel_size(vol, target, 0.5);
    assert!((h1 - 0.5).abs() < 1e-9, "k=1 snap: {h1}");

    // Snap off: the nominal size.
    let h0 = pick_voxel_size(vol, target, 0.0);
    assert!((h0 - 0.6).abs() < 1e-9, "nominal h: {h0}");

    // A wall far finer than the budget allows abandons the snap instead of
    // exploding the grid: 0.2 mm wall over a 1 m³-class volume.
    let hbig = pick_voxel_size(1.0e6, 1.0e6, 0.2);
    assert!((hbig - 1.0).abs() < 1e-9, "cap fallback: {hbig}");
}

#[test]
fn snapped_grid_resolves_skin_as_exact_layers() {
    let beam = primitives::boxx([0.0; 3], [20.0, 6.0, 6.0]);
    let vol = 20.0 * 6.0 * 6.0;
    let wall = 0.9; // 2 perimeters x 0.45
    let h = pick_voxel_size(vol, vol / 0.5f64.powi(3), wall);
    assert!((h - 0.45).abs() < 1e-9, "0.5 nominal snaps to 0.45: {h}");

    let grid0 = VoxelGrid::voxelize(&beam, h);
    let settings = SolveSettings::default();
    let (grid, _levels) = pad_for_levels(&grid0, settings.max_levels);
    let split = classify_cells(&grid, wall, false);
    let (skin, design) = (split.skin, split.design);
    assert!(!design.is_empty());

    // Walk the central column bottom -> top: exactly two skin cells on each
    // free face before the interior starts.
    let skin_set: std::collections::HashSet<u32> = skin.iter().copied().collect();
    let design_set: std::collections::HashSet<u32> = design.iter().copied().collect();
    let (cx, cy) = (grid.nx / 2, grid.ny / 2);
    let mut seq = String::new();
    for cz in 0..grid.nz {
        let ci = ((cz * grid.ny + cy) * grid.nx + cx) as u32;
        if grid.scale[ci as usize] > 0.0 {
            seq.push(if skin_set.contains(&ci) {
                's'
            } else if design_set.contains(&ci) {
                'd'
            } else {
                '?'
            });
        }
    }
    assert!(seq.starts_with("ssd"), "two skin layers at the bottom: {seq}");
    assert!(seq.ends_with("dss"), "two skin layers at the top: {seq}");
    assert!(!seq.contains('?'), "every solid cell is classified: {seq}");
}

/// The as-printed solve against Euler–Bernoulli composite-beam theory:
/// a solid frame (the skin) around a core at the calibrated infill law.
/// ν = 0 and a slender beam keep the closed form honest; the deflection
/// RATIO printed/solid cancels load distribution and most shear effects.
#[test]
fn printed_solve_matches_composite_sandwich() {
    let beam = primitives::boxx([0.0; 3], [80.0, 8.0, 8.0]);
    let grid0 = VoxelGrid::voxelize(&beam, 0.5);
    let settings = SolveSettings { e0: 2000.0, nu: 0.0, tol: 1e-6, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
    let bcs = vec![
        BcSpec { kind: BcKind::Fixed, tris: face_tris(0) },
        BcSpec { kind: BcKind::Force([0.0, 0.0, -10.0]), tris: face_tris(1) },
    ];
    let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
    let wall = 1.0; // exactly 2 cell layers at h = 0.5
    // Composite mode: at an integer wall/h it degenerates to the legacy
    // whole-layer split, so this also covers the production default.
    let split = classify_cells(&grid, wall, true);
    let (skin, design, frac) = (split.skin, split.design, split.skin_frac);
    assert!(!design.is_empty());
    assert!(frac.iter().all(|&f| f == 0.0), "integer wall/h: no composite cells");

    let x_solid = vec![1.0; design.len()];
    let (_, d_solid, _) = evaluate(
        &grid, levels, &asm.problem, &settings, &skin, &design, &frac, &x_solid, 1.5, 1.0, None,
    )
    .unwrap();

    // Production path: eps from skin + uniform interior, full-stats solve.
    let rho = 0.25;
    let x = vec![rho; design.len()];
    let eps = build_eps(&grid, &skin, &design, &frac, &x, 1.5, 1.0);
    let (sol, _c) = solve_with_eps(&grid, levels, &asm.problem, &settings, eps).unwrap();
    assert!(sol.converged, "printed solve converged");
    assert!(sol.iterations > 0 && !sol.residuals.is_empty(), "stats captured");
    let d_printed = sol.max_displacement();
    assert!(d_printed > d_solid, "infill part bends more than solid");

    let (b_o, h_o) = (8.0f64, 8.0f64);
    let (b_i, h_i) = (b_o - 2.0 * wall, h_o - 2.0 * wall);
    let i_o = b_o * h_o.powi(3) / 12.0;
    let i_i = b_i * h_i.powi(3) / 12.0;
    let e_core = 1.0 * rho.powf(1.5); // E/E0 of 25% infill, law 1.0·ρ^1.5
    let ratio_analytic = i_o / ((i_o - i_i) + e_core * i_i);
    let ratio_fem = d_printed / d_solid;
    assert!(
        (ratio_fem / ratio_analytic - 1.0).abs() < 0.10,
        "deflection ratio {ratio_fem:.3} vs composite-beam {ratio_analytic:.3}"
    );
}

/// Composite-beam ratio for a solid frame of `wall` around a core at
/// relative stiffness e_core (square 8x8 section).
fn sandwich_ratio(wall: f64, e_core: f64) -> f64 {
    let (b_o, h_o) = (8.0f64, 8.0f64);
    let (b_i, h_i) = (b_o - 2.0 * wall, h_o - 2.0 * wall);
    let i_o = b_o * h_o.powi(3) / 12.0;
    let i_i = b_i * h_i.powi(3) / 12.0;
    i_o / ((i_o - i_i) + e_core * i_i)
}

/// The composite-skin headline case: a wall THINNER than the voxel
/// (0.45 mm wall, 1 mm cells). The blended surface cells must track the
/// composite-beam closed form for the REAL wall, while the legacy model —
/// which rounds the skin up to one full cell layer — is far stiffer than
/// the actual print.
#[test]
fn composite_skin_tracks_subvoxel_wall() {
    let beam = primitives::boxx([0.0; 3], [80.0, 8.0, 8.0]);
    let grid0 = VoxelGrid::voxelize(&beam, 1.0);
    let settings = SolveSettings { e0: 2000.0, nu: 0.0, tol: 1e-6, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
    let bcs = vec![
        BcSpec { kind: BcKind::Fixed, tris: face_tris(0) },
        BcSpec { kind: BcKind::Force([0.0, 0.0, -10.0]), tris: face_tris(1) },
    ];
    let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
    let wall = 0.45; // 1 perimeter x 0.45 — under half a cell
    let rho: f64 = 0.25;
    let e_core = rho.powf(1.5);

    let solve_ratio = |composite: bool| -> f64 {
        let s = classify_cells(&grid, wall, composite);
        let x_solid = vec![1.0; s.design.len()];
        let (_, d_solid, _) = evaluate(
            &grid, levels, &asm.problem, &settings, &s.skin, &s.design, &s.skin_frac, &x_solid,
            1.5, 1.0, None,
        )
        .unwrap();
        let x = vec![rho; s.design.len()];
        let eps = build_eps(&grid, &s.skin, &s.design, &s.skin_frac, &x, 1.5, 1.0);
        let (sol, _) = solve_with_eps(&grid, levels, &asm.problem, &settings, eps).unwrap();
        sol.max_displacement() / d_solid
    };

    let ratio_true = sandwich_ratio(wall, e_core);
    let ratio_composite = solve_ratio(true);
    let ratio_legacy = solve_ratio(false);

    // Composite skin lands on the closed form for the REAL 0.45 mm wall...
    assert!(
        (ratio_composite / ratio_true - 1.0).abs() < 0.12,
        "composite ratio {ratio_composite:.3} vs analytic {ratio_true:.3}"
    );
    // ...while the legacy rounded skin behaves like a 1 mm wall (far too
    // stiff: it underestimates the deflection ratio by tens of percent).
    let err_composite = (ratio_composite / ratio_true - 1.0).abs();
    let err_legacy = (ratio_legacy / ratio_true - 1.0).abs();
    assert!(
        err_legacy > 0.2 && err_composite < err_legacy,
        "legacy err {err_legacy:.3} should dwarf composite err {err_composite:.3}"
    );
    // And the legacy result is explained by the rounded-up wall.
    let ratio_rounded = sandwich_ratio(1.0, e_core);
    assert!(
        (ratio_legacy / ratio_rounded - 1.0).abs() < 0.12,
        "legacy ratio {ratio_legacy:.3} vs rounded-wall analytic {ratio_rounded:.3}"
    );
}
