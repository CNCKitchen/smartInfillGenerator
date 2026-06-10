//! Correctness anchors for the Phase-1 engine:
//! 1. Element matrix sanity (symmetry, rigid-body null space).
//! 2. Matrix-free apply == dense assembly on a small mixed void/solid grid.
//! 3. MGCG solution == dense direct solution.
//! 4. Uniaxial patch test == exact analytic field.
//! 5. Cantilever tip deflection vs Timoshenko beam theory + mesh convergence.
//! 6. Winding-number robustness: holes, flipped normals, degenerate triangles.
//! 7. Voxelizer volume accuracy on a sphere; STL parser roundtrips.

use sig_core::fem::{ke_hex, NODE_OFFSETS};
use sig_core::mesh::{primitives, TriMesh};
use sig_core::mg::{Level, MgSolver};
use sig_core::{solve_static, BoxRegion, SolveSettings, StaticProblem, VoxelGrid};

// ---------- helpers ----------

fn node_index(mx: usize, my: usize, x: usize, y: usize, z: usize) -> usize {
    (z * my + y) * mx + x
}

/// Dense assembly mirroring Level's element scatter (gold standard for small grids).
fn assemble_dense(
    nx: usize,
    ny: usize,
    nz: usize,
    eps: &[f32],
    ke64: &[[f64; 24]; 24],
    constrained: &[bool],
    identity_rows: bool,
) -> Vec<Vec<f64>> {
    let (mx, my) = (nx + 1, ny + 1);
    let ndof = 3 * mx * my * (nz + 1);
    let mut k = vec![vec![0f64; ndof]; ndof];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let e = eps[(cz * ny + cy) * nx + cx] as f64;
                if e <= 0.0 {
                    continue;
                }
                let mut nodes = [0usize; 8];
                for l in 0..8 {
                    let [ox, oy, oz] = NODE_OFFSETS[l];
                    nodes[l] = node_index(mx, my, cx + ox, cy + oy, cz + oz);
                }
                for li in 0..8 {
                    for lj in 0..8 {
                        for r in 0..3 {
                            for c in 0..3 {
                                k[3 * nodes[li] + r][3 * nodes[lj] + c] +=
                                    e * ke64[3 * li + r][3 * lj + c];
                            }
                        }
                    }
                }
            }
        }
    }
    for d in 0..ndof {
        if constrained[d] {
            for j in 0..ndof {
                k[d][j] = 0.0;
                k[j][d] = 0.0;
            }
            if identity_rows {
                k[d][d] = 1.0;
            }
        }
    }
    k
}

fn dense_solve(mut k: Vec<Vec<f64>>, mut b: Vec<f64>) -> Vec<f64> {
    let n = b.len();
    for col in 0..n {
        let piv = (col..n)
            .max_by(|&a, &bb| k[a][col].abs().partial_cmp(&k[bb][col].abs()).unwrap())
            .unwrap();
        k.swap(col, piv);
        b.swap(col, piv);
        let p = k[col][col];
        assert!(p.abs() > 1e-12, "singular dense system");
        for row in col + 1..n {
            let f = k[row][col] / p;
            if f == 0.0 {
                continue;
            }
            for j in col..n {
                k[row][j] -= f * k[col][j];
            }
            b[row] -= f * b[col];
        }
    }
    let mut x = vec![0f64; n];
    for row in (0..n).rev() {
        let mut s = b[row];
        for j in row + 1..n {
            s -= k[row][j] * x[j];
        }
        x[row] = s / k[row][row];
    }
    x
}

/// Deterministic pseudo-random in [-1,1] (no rand dependency).
fn lcg(seed: &mut u64) -> f32 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*seed >> 33) as f64 / (1u64 << 31) as f64 - 1.0) as f32
}

// ---------- 1. element matrix ----------

#[test]
fn ke_symmetry_and_rigid_body_null_space() {
    let h = 0.7;
    let ke = ke_hex(1500.0, 0.3, h);
    for i in 0..24 {
        for j in 0..24 {
            assert!((ke[i][j] - ke[j][i]).abs() < 1e-8, "asymmetry at {i},{j}");
        }
    }
    let scale: f64 = ke.iter().flatten().fold(0.0, |a, &v| a.max(v.abs()));
    // Node positions for rotation modes.
    let pos: Vec<[f64; 3]> =
        NODE_OFFSETS.iter().map(|o| [o[0] as f64 * h, o[1] as f64 * h, o[2] as f64 * h]).collect();
    let mut modes: Vec<[f64; 24]> = Vec::new();
    for d in 0..3 {
        let mut m = [0f64; 24];
        for l in 0..8 {
            m[3 * l + d] = 1.0;
        }
        modes.push(m);
    }
    // Infinitesimal rotations about x, y, z.
    for axis in 0..3 {
        let mut m = [0f64; 24];
        for l in 0..8 {
            let p = pos[l];
            let r = match axis {
                0 => [0.0, -p[2], p[1]],
                1 => [p[2], 0.0, -p[0]],
                _ => [-p[1], p[0], 0.0],
            };
            m[3 * l] = r[0];
            m[3 * l + 1] = r[1];
            m[3 * l + 2] = r[2];
        }
        modes.push(m);
    }
    for (mi, m) in modes.iter().enumerate() {
        for i in 0..24 {
            let mut s = 0f64;
            for j in 0..24 {
                s += ke[i][j] * m[j];
            }
            assert!(s.abs() < scale * 1e-10, "rigid mode {mi} not in null space: row {i} -> {s}");
        }
    }
}

// ---------- 2 + 3. dense reference ----------

#[test]
fn apply_and_mgcg_match_dense_reference() {
    let (nx, ny, nz) = (3, 2, 2);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let ndof = 3 * mx * my * mz;
    let ke64 = ke_hex(1000.0, 0.3, 1.0);

    // Mixed grid: one void cell, one gray cell.
    let mut eps = vec![1.0f32; nx * ny * nz];
    eps[(1 * ny + 1) * nx + 1] = 0.0;
    eps[(0 * ny + 1) * nx + 2] = 0.4;

    // Fix the x=0 node plane.
    let mut fixed = vec![false; ndof];
    for z in 0..mz {
        for y in 0..my {
            let n = node_index(mx, my, 0, y, z);
            for d in 0..3 {
                fixed[3 * n + d] = true;
            }
        }
    }
    let level = Level::new(nx, ny, nz, 1.0, eps.clone(), ke64, &fixed, Vec::new());
    let constrained = level.constrained.clone();

    // Matrix-free apply vs dense multiply (rows/cols zeroed at constraints).
    let k_zero = assemble_dense(nx, ny, nz, &eps, &ke64, &constrained, false);
    let mut seed = 42u64;
    let mut x = vec![0f32; ndof];
    for (i, v) in x.iter_mut().enumerate() {
        if !constrained[i] {
            *v = lcg(&mut seed);
        }
    }
    let mut y = vec![0f32; ndof];
    level.apply(&x, &mut y);
    let ymax = y.iter().fold(0f32, |a, v| a.max(v.abs())) as f64;
    let mut x64 = vec![0f64; ndof];
    let mut y64 = vec![0f64; ndof];
    for i in 0..ndof {
        x64[i] = x[i] as f64;
    }
    level.apply64(&x64, &mut y64);
    for i in 0..ndof {
        let mut s = 0f64;
        for j in 0..ndof {
            s += k_zero[i][j] * x[j] as f64;
        }
        assert!(
            (s - y[i] as f64).abs() < ymax * 2e-5,
            "f32 apply mismatch at dof {i}: dense {s}, mf {}",
            y[i]
        );
        assert!(
            (s - y64[i]).abs() < ymax * 1e-12,
            "f64 apply mismatch at dof {i}: dense {s}, mf {}",
            y64[i]
        );
    }

    // MGCG vs dense solve.
    let mut b = vec![0f64; ndof];
    for (i, v) in b.iter_mut().enumerate() {
        if !constrained[i] {
            *v = lcg(&mut seed) as f64;
        }
    }
    let k_id = assemble_dense(nx, ny, nz, &eps, &ke64, &constrained, true);
    let xd = dense_solve(k_id, b.clone());

    let mut solver = MgSolver::new(level, 1);
    let mut u = vec![0f64; ndof];
    let stats = solver.solve(&b, &mut u, 1e-10, 500);
    assert!(stats.converged, "MGCG did not converge: {}", stats.rel_residual);

    let xnorm = xd.iter().fold(0f64, |a, v| a.max(v.abs()));
    for i in 0..ndof {
        assert!(
            (u[i] - xd[i]).abs() < xnorm * 1e-6,
            "solution mismatch at dof {i}: dense {}, mgcg {}",
            xd[i],
            u[i]
        );
    }
}

// ---------- 4. patch test ----------

#[test]
fn uniaxial_patch_test_is_exact() {
    let (nx, ny, nz) = (4, 2, 2);
    let (e0, nu, h) = (1000.0f64, 0.3f64, 1.0f64);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let ndof = 3 * mx * my * mz;
    let ke64 = ke_hex(e0, nu, h);
    let eps = vec![1.0f32; nx * ny * nz];

    // Roller planes: ux=0 @ x=0, uy=0 @ y=0, uz=0 @ z=0 (exact field compatible).
    let mut fixed = vec![false; ndof];
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
    let level = Level::new(nx, ny, nz, h, eps, ke64, &fixed, Vec::new());

    // Uniform traction sigma on the x = L face: sigma*h^2/4 per face-cell corner.
    let sigma = 10.0f64;
    let mut b = vec![0f64; ndof];
    for cz in 0..nz {
        for cy in 0..ny {
            for (oy, oz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let n = node_index(mx, my, nx, cy + oy, cz + oz);
                b[3 * n] += sigma * h * h / 4.0;
            }
        }
    }

    let mut solver = MgSolver::new(level, 1);
    let mut u = vec![0f64; ndof];
    let stats = solver.solve(&b, &mut u, 1e-12, 1000);
    assert!(stats.converged);

    // Exact: ux = sigma x / E, uy = -nu sigma y / E, uz = -nu sigma z / E.
    let uref = sigma * nx as f64 * h / e0;
    for z in 0..mz {
        for y in 0..my {
            for x in 0..mx {
                let n = node_index(mx, my, x, y, z);
                let ex = sigma * (x as f64 * h) / e0;
                let ey = -nu * sigma * (y as f64 * h) / e0;
                let ez = -nu * sigma * (z as f64 * h) / e0;
                for (d, expect) in [(0, ex), (1, ey), (2, ez)] {
                    let got = u[3 * n + d];
                    assert!(
                        (got - expect).abs() < uref * 1e-8,
                        "patch test off at node ({x},{y},{z}) dof {d}: {got} vs {expect}"
                    );
                }
            }
        }
    }
}

// ---------- 5. cantilever ----------

fn cantilever_deflection(nx: usize, ny: usize, nz: usize, h: f64) -> (f64, f64) {
    let (e0, nu) = (2000.0f64, 0.3f64);
    let f = -10.0f64; // N, -z at the tip
    let l = nx as f64 * h;
    let bdim = ny as f64 * h;
    let hdim = nz as f64 * h;

    let grid = VoxelGrid::solid_box(nx, ny, nz, h);
    let problem = StaticProblem {
        grid,
        fixed: vec![BoxRegion::new([-0.1, -1.0, -1.0], [0.1, bdim + 1.0, hdim + 1.0])],
        loads: vec![(
            BoxRegion::new([l - 0.1, -1.0, -1.0], [l + 0.1, bdim + 1.0, hdim + 1.0]),
            [0.0, 0.0, f],
        )],
        settings: SolveSettings { e0, nu, tol: 1e-6, max_iter: 400, ..Default::default() },
    };
    let sol = solve_static(&problem).expect("cantilever solve failed");
    let tip = sol
        .mean_displacement(&BoxRegion::new([l - 0.1, -1.0, -1.0], [l + 0.1, bdim + 1.0, hdim + 1.0]))
        .unwrap();

    // Timoshenko reference.
    let inertia = bdim * hdim.powi(3) / 12.0;
    let area = bdim * hdim;
    let g = e0 / (2.0 * (1.0 + nu));
    let kappa = 10.0 * (1.0 + nu) / (12.0 + 11.0 * nu);
    let delta = f * l.powi(3) / (3.0 * e0 * inertia) + f * l / (kappa * g * area);
    (tip[2], delta)
}

#[test]
fn cantilever_matches_timoshenko_and_converges() {
    let (fe1, exact) = cantilever_deflection(80, 8, 8, 1.0);
    let r1 = fe1 / exact;
    assert!(
        (0.90..=1.02).contains(&r1),
        "cantilever 80x8x8: FE/analytic = {r1:.4} (FE {fe1:.4} vs {exact:.4})"
    );
    // Same physical beam, refined: error must shrink.
    let (fe2, _) = cantilever_deflection(160, 16, 16, 0.5);
    let r2 = fe2 / exact;
    assert!(
        (1.0 - r2).abs() < (1.0 - r1).abs() + 1e-3,
        "refinement did not converge: coarse ratio {r1:.4}, fine ratio {r2:.4}"
    );
    assert!((0.95..=1.02).contains(&r2), "cantilever 160x16x16: FE/analytic = {r2:.4}");
}

// ---------- 6. winding robustness ----------

#[test]
fn winding_number_closed_open_flipped() {
    use sig_core::bvh::WindingBvh;
    let cube = primitives::boxx([0.0; 3], [10.0; 3]);
    let bvh = WindingBvh::build(&cube);
    assert!((bvh.winding_number([5.0, 5.0, 5.0]) - 1.0).abs() < 1e-3);
    assert!((bvh.winding_number([5.0, 5.0, 0.5]) - 1.0).abs() < 1e-3);
    assert!(bvh.winding_number([20.0, 5.0, 5.0]).abs() < 1e-3);

    // Punch a hole: drop the +z face (last two triangles).
    let mut open = cube.clone();
    open.tris.truncate(10);
    let bvh_open = WindingBvh::build(&open);
    let w_in = bvh_open.winding_number([5.0, 5.0, 5.0]);
    assert!((0.7..0.95).contains(&w_in), "open-mesh interior winding {w_in}");
    assert!(bvh_open.winding_number([5.0, 5.0, 15.0]).abs() < 0.3);

    // Flipped (inside-out) mesh: |w| classification must still work.
    let flipped = TriMesh::from_triangles(
        cube.tris
            .iter()
            .map(|t| [t[0], t[1], t[2], t[6], t[7], t[8], t[3], t[4], t[5]])
            .collect(),
    );
    let solid_n = VoxelGrid::voxelize(&cube, 1.0).solid_count();
    let solid_f = VoxelGrid::voxelize(&flipped, 1.0).solid_count();
    assert_eq!(solid_n, 1000);
    assert_eq!(solid_f, 1000);
}

// ---------- 7. voxelizer + STL ----------

#[test]
fn voxelizer_sphere_volume() {
    let r = 10.0f32;
    let sph = primitives::sphere([0.0; 3], r, 64, 32);
    let grid = VoxelGrid::voxelize(&sph, 0.5);
    let exact = 4.0 / 3.0 * std::f64::consts::PI * (r as f64).powi(3);
    let got = grid.solid_volume();
    let err = (got - exact).abs() / exact;
    assert!(err < 0.04, "sphere volume off by {:.2}%: {got} vs {exact}", err * 100.0);
}

#[test]
fn stl_roundtrip_and_dirty_input() {
    let beam = primitives::boxx([0.0; 3], [80.0, 8.0, 8.0]);
    let bytes = beam.to_stl_binary();
    let parsed = TriMesh::from_stl(&bytes).unwrap();
    assert_eq!(parsed.len(), 12);

    // Binary STL whose header starts with "solid" (classic exporter quirk).
    let mut sneaky = bytes.clone();
    sneaky[..5].copy_from_slice(b"solid");
    assert_eq!(TriMesh::from_stl(&sneaky).unwrap().len(), 12);

    // Degenerate + NaN triangles get dropped, not fatal.
    let mut dirty = beam.clone();
    dirty.tris.push([1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]); // zero area
    dirty.tris.push([f32::NAN, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    let cleaned = TriMesh::from_stl(&dirty.to_stl_binary()).unwrap();
    assert_eq!(cleaned.len(), 12);

    // ASCII parse.
    let ascii = "solid t\n facet normal 0 0 1\n  outer loop\n   vertex 0 0 0\n   vertex 1 0 0\n   vertex 0 1 0\n  endloop\n endfacet\nendsolid t\n";
    assert_eq!(TriMesh::from_stl(ascii.as_bytes()).unwrap().len(), 1);

    // Bounds.
    let (lo, hi) = beam.bounds().unwrap();
    assert_eq!(lo, [0.0; 3]);
    assert_eq!(hi, [80.0, 8.0, 8.0]);
}

#[test]
fn end_to_end_stl_cantilever() {
    let beam = primitives::boxx([0.0; 3], [40.0, 6.0, 6.0]);
    let grid = VoxelGrid::voxelize(&beam, 1.0);
    assert_eq!(grid.solid_count(), 40 * 6 * 6);
    let problem = StaticProblem {
        grid,
        fixed: vec![BoxRegion::new([-0.6, -1.0, -1.0], [0.6, 7.0, 7.0])],
        loads: vec![(BoxRegion::new([39.4, -1.0, -1.0], [40.6, 7.0, 7.0]), [0.0, 0.0, -5.0])],
        settings: SolveSettings { e0: 2000.0, nu: 0.3, tol: 1e-6, ..Default::default() },
    };
    let sol = solve_static(&problem).expect("e2e solve");
    // Just sanity here (beam test covers accuracy): deflects downward, sane magnitude.
    let tip = sol
        .mean_displacement(&BoxRegion::new([39.4, -1.0, -1.0], [40.6, 7.0, 7.0]))
        .unwrap();
    assert!(tip[2] < -0.05 && tip[2] > -5.0, "tip uz {tip:?}");
    assert!(sol.iterations > 0);
}
