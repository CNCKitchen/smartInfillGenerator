// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! DESIGN §22 — the transverse-isotropic infill kernel, checked end-to-end
//! through a real solve rather than at the tensor level (`ti.rs` covers that).
//!
//! Two things must hold and neither is visible in a unit test of the tensor:
//! the two-tensor kernel must be a strict GENERALIZATION (isotropic data
//! reproduces the old kernel), and the anisotropy must survive assembly,
//! multigrid and the outer CG with the right axes attached.

use filasim_core::mg::{Level, MgSolver};
use filasim_core::ti;

fn node_index(mx: usize, my: usize, x: usize, y: usize, z: usize) -> usize {
    (z * my + y) * mx + x
}

/// Roller planes at x=0, y=0, z=0 — compatible with a uniform uniaxial stress
/// state, so the FE answer is EXACT for any homogeneous material (the strain
/// field is constant, hence exactly representable by trilinear elements).
fn roller_fixed(mx: usize, my: usize, mz: usize) -> Vec<bool> {
    let mut fixed = vec![false; 3 * mx * my * mz];
    for z in 0..mz {
        for y in 0..my {
            for x in 0..mx {
                let n = node_index(mx, my, x, y, z);
                if x == 0 {
                    fixed[3 * n] = true;
                }
                if y == 0 {
                    fixed[3 * n + 1] = true;
                }
                if z == 0 {
                    fixed[3 * n + 2] = true;
                }
            }
        }
    }
    fixed
}

/// Uniform traction `sigma` on the far face along `axis`, lumped to the four
/// corners of each face cell.
fn face_traction(n: [usize; 3], m: [usize; 3], axis: usize, sigma: f64, h: f64) -> Vec<f64> {
    let (mx, my) = (m[0], m[1]);
    let mut b = vec![0f64; 3 * m[0] * m[1] * m[2]];
    let (a, c) = ((axis + 1) % 3, (axis + 2) % 3);
    for ia in 0..n[a] {
        for ic in 0..n[c] {
            for (oa, oc) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let mut p = [0usize; 3];
                p[axis] = n[axis];
                p[a] = ia + oa;
                p[c] = ic + oc;
                let nd = node_index(mx, my, p[0], p[1], p[2]);
                b[3 * nd + axis] += sigma * h * h / 4.0;
            }
        }
    }
    b
}

/// Far-face displacement along `axis` under uniform traction — i.e. the
/// effective modulus in that direction.
fn uniaxial_modulus(eps: &[f32], eps_i: &[f32], c_unit: [[f64; 6]; 6], axis: usize) -> f64 {
    let (nx, ny, nz) = (4usize, 4, 4);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let (h, e0, nu, sigma) = (1.0f64, 1000.0f64, 0.35f64, 10.0f64);
    let fixed = roller_fixed(mx, my, mz);
    let level = Level::new_ti(
        nx,
        ny,
        nz,
        h,
        eps.to_vec(),
        eps_i.to_vec(),
        c_unit,
        e0,
        nu,
        &fixed,
        Vec::new(),
        Vec::new(),
    );
    let b = face_traction([nx, ny, nz], [mx, my, mz], axis, sigma, h);
    let mut solver = MgSolver::new(level, 1);
    let mut u = vec![0f64; 3 * mx * my * mz];
    let stats = solver.solve(&b, &mut u, 1e-12, 2000);
    assert!(stats.converged, "TI solve did not converge on axis {axis}");

    // u = sigma * L / E  =>  E = sigma * L / u, at the far-face node.
    let mut p = [0usize; 3];
    p[axis] = [nx, ny, nz][axis];
    let n = node_index(mx, my, p[0], p[1], p[2]);
    let l = [nx, ny, nz][axis] as f64 * h;
    sigma * l / u[3 * n + axis]
}

/// The generalization identity of DESIGN §22.4: splitting one isotropic cell
/// into a solid share and an "infill" share whose tensor is ALSO isotropic
/// must reproduce the single-tensor kernel. If this drifts, the two-tensor
/// kernel is not a superset of the old one and every regbench baseline is
/// suspect.
#[test]
fn isotropic_split_reproduces_the_single_tensor_solve() {
    let (nx, ny, nz) = (4usize, 4, 4);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let (h, e0, nu, sigma) = (1.0f64, 1000.0f64, 0.35f64, 10.0f64);
    let ncell = nx * ny * nz;

    // A deliberately UNEVEN split so the test cannot pass by both shares
    // being equal, and so some cells are pure-infill (solid share exactly 0 —
    // the case that dies if a liveness test still reads `eps` directly).
    let mut es = vec![0f32; ncell];
    let mut ei = vec![0f32; ncell];
    for c in 0..ncell {
        let f = match c % 4 {
            0 => 0.0,
            1 => 0.25,
            2 => 0.6,
            _ => 1.0,
        };
        es[c] = f;
        ei[c] = 1.0 - f;
    }
    let total: Vec<f32> = es.iter().zip(&ei).map(|(a, b)| a + b).collect();
    assert!(es.iter().any(|&v| v == 0.0), "test must exercise pure-infill cells");

    let fixed = roller_fixed(mx, my, mz);
    let b = face_traction([nx, ny, nz], [mx, my, mz], 0, sigma, h);
    let ndof = 3 * mx * my * mz;

    let mut u_ref = vec![0f64; ndof];
    let lvl = Level::new(nx, ny, nz, h, total, e0, nu, &fixed, Vec::new(), Vec::new());
    assert!(MgSolver::new(lvl, 1).solve(&b, &mut u_ref, 1e-12, 2000).converged);

    let mut u_ti = vec![0f64; ndof];
    let lvl = Level::new_ti(
        nx,
        ny,
        nz,
        h,
        es,
        ei,
        ti::isotropic_ratios(nu).stiffness(),
        e0,
        nu,
        &fixed,
        Vec::new(),
        Vec::new(),
    );
    assert!(MgSolver::new(lvl, 1).solve(&b, &mut u_ti, 1e-12, 2000).converged);

    let scale = u_ref.iter().fold(0f64, |m, v| m.max(v.abs()));
    assert!(scale > 0.0);
    for (i, (a, r)) in u_ti.iter().zip(&u_ref).enumerate() {
        assert!(
            (a - r).abs() < 1e-9 * scale,
            "DOF {i}: TI-split {a:e} vs single-tensor {r:e} (scale {scale:e})"
        );
    }
}

/// The frozen cubic tensor must deliver its MEASURED anisotropy through a
/// real solve. A tensor transposed into the wrong axis, a strain-order
/// mismatch in `ke_hex_c`, or an overlay that never reaches the operator all
/// leave the SPD/symmetry unit tests green and fail right here.
#[test]
fn cubic_bar_reproduces_the_measured_ez_over_ep() {
    let ncell = 4 * 4 * 4;
    let solid = vec![0f32; ncell]; // pure infill: no skin anywhere
    let infill = vec![1f32; ncell]; // rel(rho) = 1, so Ep_eff == e0
    let c = ti::CUBIC.stiffness();

    let ex = uniaxial_modulus(&solid, &infill, c, 0);
    let ey = uniaxial_modulus(&solid, &infill, c, 1);
    let ez = uniaxial_modulus(&solid, &infill, c, 2);

    // In-plane isotropy (the measurement says Ex/Ey = 1.0008 +- 0.2 %).
    assert!((ex - ey).abs() < 1e-6 * ex, "in-plane anisotropy leaked in: Ex {ex} vs Ey {ey}");
    // Ep is normalized to 1 in the tensor, so the bar must read back e0.
    assert!((ex - 1000.0).abs() < 1e-6 * 1000.0, "Ep should be e0, got {ex}");
    // And the ratio is the frozen constant, exactly.
    let ratio = ez / ex;
    assert!(
        (ratio - ti::CUBIC.ez_ep).abs() < 1e-6,
        "Ez/Ep from the solve is {ratio}, frozen constant is {}",
        ti::CUBIC.ez_ep
    );
}

/// A TI solver must refuse the isotropic update path. `update_eps` would set
/// the solid weights and silently leave the infill weights at their initial
/// values — the optimizer would then iterate on a structure whose infill
/// never changes, converging happily to the wrong answer.
#[test]
#[should_panic(expected = "update_eps_ti")]
fn ti_solver_rejects_the_isotropic_update_path() {
    let (nx, ny, nz) = (4usize, 4, 4);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let ncell = nx * ny * nz;
    let fixed = roller_fixed(mx, my, mz);
    let level = Level::new_ti(
        nx,
        ny,
        nz,
        1.0,
        vec![0.5f32; ncell],
        vec![0.5f32; ncell],
        ti::CUBIC.stiffness(),
        1000.0,
        0.35,
        &fixed,
        Vec::new(),
        Vec::new(),
    );
    MgSolver::new(level, 1).update_eps(vec![0.4f32; ncell]);
}

/// STRESS MUST FOLLOW STIFFNESS. Splitting an isotropic cell into a solid and
/// an "isotropic TI" share must leave the evaluated stress unchanged — the
/// readout twin of `isotropic_split_reproduces_the_single_tensor_solve`.
///
/// Without this, a TI solve read back through the isotropic law reports wrong
/// stress and wrong safety factors while converging perfectly, which no
/// residual or convergence check can catch.
#[test]
fn isotropic_split_reproduces_the_single_tensor_stress() {
    use filasim_core::stress::{cell_field_eigen, cell_field_ti, FieldKind};
    use filasim_core::voxel::VoxelGrid;

    let (nx, ny, nz) = (3usize, 3, 3);
    let ncell = nx * ny * nz;
    let grid = VoxelGrid {
        nx,
        ny,
        nz,
        h: 0.5,
        origin: [0.0; 3],
        scale: vec![1.0; ncell],
    };
    // A deterministic non-trivial displacement field: every strain component
    // non-zero, so a mis-ordered tensor row cannot cancel out.
    let nn = (nx + 1) * (ny + 1) * (nz + 1);
    let u: Vec<f32> = (0..3 * nn)
        .map(|i| {
            let t = i as f32 * 0.37;
            0.001 * (t.sin() + 0.5 * (2.0 * t).cos())
        })
        .collect();

    let (e0, nu) = (2400.0f64, 0.35f64);
    let total = vec![0.8f32; ncell];
    let solid = vec![0.3f32; ncell];
    let infill = vec![0.5f32; ncell];
    let iso = filasim_core::ti::isotropic_ratios(nu);

    for kind in [
        FieldKind::VonMises,
        FieldKind::Szz,
        FieldKind::Syz,
        FieldKind::Sxx,
        FieldKind::Szx,
    ] {
        let a = cell_field_eigen(&grid, &u, e0, nu, &total, [0.0; 3], kind);
        let b = cell_field_ti(
            &grid,
            &u,
            e0,
            nu,
            &solid,
            Some((&infill, &iso)),
            [0.0; 3],
            kind,
        );
        let scale = a.iter().fold(0f32, |m, v| m.max(v.abs())).max(1e-12);
        for (i, (x, y)) in b.iter().zip(&a).enumerate() {
            assert!(
                (x - y).abs() < 1e-4 * scale,
                "{kind:?} cell {i}: TI-split {x} vs single-tensor {y}"
            );
        }
    }
}

/// The frozen cubic tensor must make the READOUT anisotropic too: identical
/// strain along z vs in-plane must give a lower z stress, in the same ratio
/// the tensor says. Catches a stress path that silently kept the isotropic law.
#[test]
fn ti_stress_is_softer_along_the_build_axis() {
    let strain_x = [1e-3, 0.0, 0.0, 0.0, 0.0, 0.0];
    let strain_z = [0.0, 0.0, 1e-3, 0.0, 0.0, 0.0];
    let (e0, nu) = (2400.0f64, 0.35f64);
    // Pure infill: no solid share at all.
    let sx = filasim_core::ti::blended_stress(e0, nu, 0.0, 1.0, &ti::CUBIC, strain_x);
    let sz = filasim_core::ti::blended_stress(e0, nu, 0.0, 1.0, &ti::CUBIC, strain_z);
    assert!(sz[2] < sx[0], "szz {} must be below sxx {}", sz[2], sx[0]);
    // C33/C11 — the confined ratio, which for this tensor sits near Ez/Ep.
    let r = sz[2] / sx[0];
    assert!(r > 0.5 && r < 1.0, "confined z/in-plane ratio {r} implausible");
}
