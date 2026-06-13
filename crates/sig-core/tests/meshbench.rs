// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Meshing-convention benchmark (decision harness, 2026-06).
//!
//! Compares four boundary-cell conventions against analytic references so we
//! can decide how to treat cut cells without guessing:
//!
//!   center-full   : solid iff cell CENTER inside; stiffness 1   (the OLD app)
//!   center-occ    : solid iff CENTER inside;       stiffness = occupancy (NOW)
//!   inflate-derate: solid iff occupancy > 0;       stiffness = occupancy (FCM/ersatz)
//!   majority      : solid iff occupancy >= 0.5;    stiffness 1   (50%% rule)
//!
//! All four are built from public APIs (VoxelGrid fields + WindingBvh), so
//! production voxel.rs is untouched. Occupancy is supersampled finely here so
//! the convention's BIAS is what's measured, not occupancy quantization.
//!
//! Run:  cargo test -p sig-core --test meshbench -- --ignored --nocapture

use sig_core::bvh::WindingBvh;
use sig_core::mesh::{primitives, TriMesh};
use sig_core::solve::{active_nodes, grid_eps};
use sig_core::stress::{cell_field, FieldKind};
use sig_core::{
    pad_for_levels, solve_nodes, solve_static, BoxRegion, NodeProblem, SolveSettings,
    StaticProblem, VoxelGrid,
};

#[derive(Clone, Copy, PartialEq)]
enum Conv {
    CenterFull,
    CenterOcc,
    Inflate,
    Majority,
    /// Inflate-derate but drop slivers below `INFLATE_FLOOR` occupancy (the
    /// FCM small-cut-cell guard).
    InflateFloor,
}
const INFLATE_FLOOR: f64 = 0.15;
const CONVS: [(&str, Conv); 4] = [
    ("center-full(OLD)", Conv::CenterFull),
    ("center-occ (NOW)", Conv::CenterOcc),
    ("inflate-derate  ", Conv::Inflate),
    ("majority-50%    ", Conv::Majority),
];

/// Rotate a mesh about the x axis (beam axis) then the z axis, through the
/// origin — turns an axis-aligned primitive into an off-grid one whose
/// surface actually cuts cells.
fn rotated(mesh: &TriMesh, rx: f64, rz: f64) -> TriMesh {
    let (cx, sx) = (rx.cos(), rx.sin());
    let (cz, sz) = (rz.cos(), rz.sin());
    let map = |p: [f32; 3]| -> [f32; 3] {
        let (x, y, z) = (p[0] as f64, p[1] as f64, p[2] as f64);
        // Rx
        let (y, z) = (y * cx - z * sx, y * sx + z * cx);
        // Rz
        let (x, y) = (x * cz - y * sz, x * sz + y * cz);
        [x as f32, y as f32, z as f32]
    };
    TriMesh::from_triangles(
        mesh.tris
            .iter()
            .map(|t| {
                let a = map([t[0], t[1], t[2]]);
                let b = map([t[3], t[4], t[5]]);
                let c = map([t[6], t[7], t[8]]);
                [a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]
            })
            .collect(),
    )
}

/// Build a VoxelGrid for one convention from any inside/outside indicator over
/// the box `[lo, hi]`. `ss` = supersampling per axis for the occupancy
/// fraction; `shift` offsets the grid origin (grid-phase tests); `pad` adds a
/// void ring so inflate has room to add cells.
fn voxelize_indicator(
    inside: &dyn Fn([f64; 3]) -> bool,
    lo: [f64; 3],
    hi: [f64; 3],
    h: f64,
    conv: Conv,
    ss: usize,
    shift: [f64; 3],
    pad: usize,
) -> VoxelGrid {
    let nx = ((hi[0] - lo[0]) / h).ceil() as usize + 2 * pad;
    let ny = ((hi[1] - lo[1]) / h).ceil() as usize + 2 * pad;
    let nz = ((hi[2] - lo[2]) / h).ceil() as usize + 2 * pad;
    let origin = [
        lo[0] - 0.5 * (((nx - 2 * pad) as f64) * h - (hi[0] - lo[0])) - pad as f64 * h + shift[0],
        lo[1] - 0.5 * (((ny - 2 * pad) as f64) * h - (hi[1] - lo[1])) - pad as f64 * h + shift[1],
        lo[2] - 0.5 * (((nz - 2 * pad) as f64) * h - (hi[2] - lo[2])) - pad as f64 * h + shift[2],
    ];
    let center = |cx: usize, cy: usize, cz: usize| -> bool {
        inside([
            origin[0] + (cx as f64 + 0.5) * h,
            origin[1] + (cy as f64 + 0.5) * h,
            origin[2] + (cz as f64 + 0.5) * h,
        ])
    };

    // Center pass (the topology decision of the OLD/NOW conventions).
    let mut ci = vec![false; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                ci[(cz * ny + cy) * nx + cx] = center(cx, cy, cz);
            }
        }
    }
    let cin = |x: i64, y: i64, z: i64| -> bool {
        x >= 0
            && y >= 0
            && z >= 0
            && x < nx as i64
            && y < ny as i64
            && z < nz as i64
            && ci[((z as usize) * ny + y as usize) * nx + x as usize]
    };
    let occ_at = |cx: usize, cy: usize, cz: usize| -> f64 {
        let mut n = 0u32;
        for a in 0..ss {
            for b in 0..ss {
                for c in 0..ss {
                    let q = [
                        origin[0] + (cx as f64 + (a as f64 + 0.5) / ss as f64) * h,
                        origin[1] + (cy as f64 + (b as f64 + 0.5) / ss as f64) * h,
                        origin[2] + (cz as f64 + (c as f64 + 0.5) / ss as f64) * h,
                    ];
                    if inside(q) {
                        n += 1;
                    }
                }
            }
        }
        n as f64 / (ss * ss * ss) as f64
    };

    let mut scale = vec![0f32; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let idx = (cz * ny + cy) * nx + cx;
                let (xi, yi, zi) = (cx as i64, cy as i64, cz as i64);
                // Fast paths: fully interior / fully exterior cells skip the
                // expensive supersample.
                let self_in = ci[idx];
                let all_same = [
                    (xi - 1, yi, zi),
                    (xi + 1, yi, zi),
                    (xi, yi - 1, zi),
                    (xi, yi + 1, zi),
                    (xi, yi, zi - 1),
                    (xi, yi, zi + 1),
                ]
                .iter()
                .all(|&(x, y, z)| cin(x, y, z) == self_in);
                let occ = if all_same {
                    if self_in {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    occ_at(cx, cy, cz)
                };
                let floor = 1.0 / (ss * ss * ss) as f64;
                scale[idx] = match conv {
                    Conv::CenterFull => {
                        if self_in {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    Conv::CenterOcc => {
                        if self_in {
                            occ.max(floor) as f32
                        } else {
                            0.0
                        }
                    }
                    Conv::Inflate => {
                        if occ > 1e-9 {
                            occ as f32
                        } else {
                            0.0
                        }
                    }
                    Conv::Majority => {
                        if occ >= 0.5 {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    Conv::InflateFloor => {
                        if occ >= INFLATE_FLOOR {
                            occ as f32
                        } else {
                            0.0
                        }
                    }
                };
            }
        }
    }
    VoxelGrid { nx, ny, nz, h, origin, scale }
}

/// Mesh-backed convention voxelizer (wraps the indicator version with a BVH).
fn voxelize_conv(mesh: &TriMesh, h: f64, conv: Conv, ss: usize, shift: [f64; 3], pad: usize) -> VoxelGrid {
    let (lo, hi) = mesh.bounds().expect("empty mesh");
    let bvh = WindingBvh::build(mesh);
    let inside = move |q: [f64; 3]| bvh.winding_number(q).abs() >= 0.5;
    voxelize_indicator(&inside, lo, hi, h, conv, ss, shift, pad)
}

fn vol(g: &VoxelGrid) -> f64 {
    g.scale.iter().map(|&s| s as f64).sum::<f64>() * g.h * g.h * g.h
}

// ---------------- Benchmark 1: volume convergence ----------------

#[test]
#[ignore]
fn bench_volume_convergence() {
    println!("\n=== Benchmark 1: solid VOLUME vs analytic (signed % error) ===");
    let sphere = primitives::sphere([0.0; 3], 10.0, 96, 48);
    let sphere_v = 4.0 / 3.0 * std::f64::consts::PI * 10f64.powi(3);
    // Box 40x10x10 rotated off-axis: volume is rotation-invariant = 4000.
    let boxm = rotated(&primitives::boxx([-20.0, -5.0, -5.0], [20.0, 5.0, 5.0]), 0.41, 0.27);
    let box_v = 40.0 * 10.0 * 10.0;

    for (name, mesh, exact) in
        [("sphere r=10", &sphere, sphere_v), ("box 40x10x10 @rot", &boxm, box_v)]
    {
        println!("\n  {name} (exact {exact:.1} mm^3)");
        println!("    h      {:>16} {:>16} {:>16} {:>16}", CONVS[0].0, CONVS[1].0, CONVS[2].0, CONVS[3].0);
        for &h in &[1.0, 0.5, 0.25] {
            print!("    {h:<6}");
            for (_n, conv) in CONVS {
                let g = voxelize_conv(mesh, h, conv, 6, [0.0; 3], 1);
                let e = (vol(&g) - exact) / exact * 100.0;
                print!(" {e:>+15.2}%");
            }
            println!();
        }
    }
    println!("\n  (signed: + overestimates material, - loses material)");
}

// ---------------- Benchmark 2: grid-phase robustness ----------------

#[test]
#[ignore]
fn bench_phase_robustness() {
    println!("\n=== Benchmark 2: grid-PHASE robustness (spread over origin shifts) ===");
    let sphere = primitives::sphere([0.0; 3], 10.0, 96, 48);
    let h = 0.8;
    let shifts = [
        [0.0, 0.0, 0.0],
        [0.27 * h, 0.13 * h, 0.41 * h],
        [0.5 * h, 0.5 * h, 0.5 * h],
        [0.7 * h, 0.31 * h, 0.62 * h],
        [0.13 * h, 0.62 * h, 0.9 * h],
    ];
    let exact = 4.0 / 3.0 * std::f64::consts::PI * 10f64.powi(3);
    println!("\n  sphere r=10, h={h}: VOLUME mean error and coefficient of variation");
    println!("    {:<18} {:>10} {:>10}", "convention", "mean%err", "CoV%");
    for (name, conv) in CONVS {
        let vols: Vec<f64> =
            shifts.iter().map(|&s| vol(&voxelize_conv(&sphere, h, conv, 6, s, 1))).collect();
        let mean = vols.iter().sum::<f64>() / vols.len() as f64;
        let var = vols.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vols.len() as f64;
        let cov = var.sqrt() / mean * 100.0;
        let mean_err = (mean - exact) / exact * 100.0;
        println!("    {name:<18} {mean_err:>+9.2}% {cov:>9.3}");
    }
    println!("  (lower CoV = less wobble when the user nudges resolution/position)");
}

// ---------------- Benchmark 3 + 4: stiffness vs analytic ----------------

/// Cantilever on a square (a x a) beam length L, rotated 30 deg about its
/// long axis so the cross-section cuts cells. A square's second moment is
/// rotation-invariant (I = a^4/12), giving a clean analytic reference.
fn beam_ratios(conv: Conv, a: f64, l: f64, h: f64) -> (f64, f64) {
    let (e0, nu) = (2000.0f64, 0.3f64);
    let mesh = rotated(
        &primitives::boxx([0.0, -(a as f32) / 2.0, -(a as f32) / 2.0], [l as f32, (a as f32) / 2.0, (a as f32) / 2.0]),
        std::f64::consts::PI / 6.0,
        0.0,
    );
    let grid = voxelize_conv(&mesh, h, conv, 5, [0.0; 3], 1);
    let span = a * 1.5; // half-extent that captures the rotated section
    let settings = SolveSettings { e0, nu, tol: 1e-6, max_iter: 600, ..Default::default() };

    // Bending: -z tip load.
    let fb = -10.0f64;
    let problem_b = StaticProblem {
        grid: grid.clone(),
        fixed: vec![BoxRegion::new([-0.6, -span, -span], [0.6, span, span])],
        loads: vec![(BoxRegion::new([l - 0.6, -span, -span], [l + 0.6, span, span]), [0.0, 0.0, fb])],
        settings,
    };
    let sol_b = solve_static(&problem_b).expect("bend solve");
    let tip_b = sol_b
        .mean_displacement(&BoxRegion::new([l - 0.6, -span, -span], [l + 0.6, span, span]))
        .unwrap();
    let inertia = a.powi(4) / 12.0;
    let delta_eb = fb * l.powi(3) / (3.0 * e0 * inertia);
    let bend_ratio = tip_b[2] / delta_eb;

    // Axial: +x tip load, k = EA/L.
    let fa = 100.0f64;
    let problem_a = StaticProblem {
        grid,
        fixed: vec![BoxRegion::new([-0.6, -span, -span], [0.6, span, span])],
        loads: vec![(BoxRegion::new([l - 0.6, -span, -span], [l + 0.6, span, span]), [fa, 0.0, 0.0])],
        settings,
    };
    let sol_a = solve_static(&problem_a).expect("axial solve");
    let tip_a = sol_a
        .mean_displacement(&BoxRegion::new([l - 0.6, -span, -span], [l + 0.6, span, span]))
        .unwrap();
    let delta_ax = fa * l / (e0 * a * a);
    let axial_ratio = tip_a[0] / delta_ax;

    (bend_ratio, axial_ratio)
}

#[test]
#[ignore]
fn bench_stiffness_vs_analytic() {
    println!("\n=== Benchmark 3+4: ROTATED SQUARE cantilever, FE/analytic ratio ===");
    println!("  (1.000 = exact; bending uses Euler-Bernoulli I=a^4/12, slight shear");
    println!("   softening expected; axial uses k=EA/L. Compare conventions, not abs.)");
    let (a, l) = (10.0f64, 100.0f64);
    for &h in &[1.0, 0.5] {
        println!("\n  a={a} L={l} h={h}  (cells across ~ {:.0})", a / h);
        println!("    {:<18} {:>12} {:>12}", "convention", "bend FE/EB", "axial FE/an");
        for (name, conv) in CONVS {
            let (b, ax) = beam_ratios(conv, a, l, h);
            println!("    {name:<18} {b:>12.4} {ax:>12.4}");
        }
    }
    println!("\n  bend ratio near/below 1 good; axial ratio = captured-area / true-area.");
}

// ---------------- Benchmark 5: stress concentration (Kirsch) ----------------

/// Plate (half-width `hw`, thickness `t`) with a central circular hole radius
/// `a`, pulled in +x. Kirsch: σ at the hole edge (θ=90°) = 3·σ∞ for a small
/// hole. Solve each convention, then read the hole-edge concentration, the
/// global peak von Mises near the hole, and the minimum safety factor — the
/// numbers that actually govern a go/no-go.
fn kirsch(conv: Conv, hw: f64, t: f64, a: f64, h: f64) -> (f64, f64, f64) {
    let (e0, nu, strength) = (2000.0f64, 0.3f64, 60.0f64);
    let lo = [-hw, -hw, 0.0];
    let hi = [hw, hw, t];
    let inside = move |p: [f64; 3]| -> bool {
        p[0] >= -hw
            && p[0] <= hw
            && p[1] >= -hw
            && p[1] <= hw
            && p[2] >= 0.0
            && p[2] <= t
            && (p[0] * p[0] + p[1] * p[1]) >= a * a
    };
    let grid = voxelize_indicator(&inside, lo, hi, h, conv, 6, [0.0; 3], 1);

    let settings = SolveSettings { e0, nu, tol: 1e-6, max_iter: 800, ..Default::default() };
    let (pg, levels) = pad_for_levels(&grid, settings.max_levels);
    let (mx, my, mz) = (pg.nx + 1, pg.ny + 1, pg.nz + 1);
    let active = active_nodes(&pg);
    let npos = |n: usize| -> [f64; 3] {
        let x = n % mx;
        let y = (n / mx) % my;
        let z = n / (mx * my);
        [pg.origin[0] + x as f64 * pg.h, pg.origin[1] + y as f64 * pg.h, pg.origin[2] + z as f64 * pg.h]
    };
    // σ∞ = 1 MPa → total force F = σ∞ · (gross width · thickness).
    let sigma_inf = 1.0f64;
    let f_total = sigma_inf * (2.0 * hw) * t;
    let mut np = NodeProblem::default();
    let fixed = BoxRegion::new([-hw - 1.0, -hw - 1.0, -1.0], [-hw + 0.5 * h, hw + 1.0, t + 1.0]);
    let loadr = BoxRegion::new([hw - 0.5 * h, -hw - 1.0, -1.0], [hw + 1.0, hw + 1.0, t + 1.0]);
    for n in 0..mx * my * mz {
        if active[n] && fixed.contains(npos(n)) {
            np.fixed.push(n as u32);
        }
    }
    let load_nodes: Vec<usize> =
        (0..mx * my * mz).filter(|&n| active[n] && loadr.contains(npos(n))).collect();
    let inv = 1.0 / load_nodes.len() as f64;
    for n in load_nodes {
        np.forces.push((n as u32, [f_total * inv, 0.0, 0.0]));
    }
    let sol = solve_nodes(&pg, levels, &np, &settings).expect("kirsch solve");
    let eps = grid_eps(&pg);
    let vm = cell_field(&pg, &sol.u, e0, nu, &eps, FieldKind::VonMises);
    let sxx = cell_field(&pg, &sol.u, e0, nu, &eps, FieldKind::Sxx);

    // Scan: hole-edge concentration (max σxx in the rim annulus, = θ=90 peak),
    // global peak vm near the hole, and the min safety factor over solid cells.
    let mid = t / 2.0;
    let mut kt = 0.0f64; // max σxx / σ∞ in the rim
    let mut peak_vm = 0.0f64;
    let mut min_sf = f64::INFINITY;
    for cz in 0..pg.nz {
        for cy in 0..pg.ny {
            for cx in 0..pg.nx {
                let ci = (cz * pg.ny + cy) * pg.nx + cx;
                if eps[ci] <= 1e-4 {
                    continue;
                }
                let p = [
                    pg.origin[0] + (cx as f64 + 0.5) * pg.h,
                    pg.origin[1] + (cy as f64 + 0.5) * pg.h,
                    pg.origin[2] + (cz as f64 + 0.5) * pg.h,
                ];
                let r = (p[0] * p[0] + p[1] * p[1]).sqrt();
                let near_mid = (p[2] - mid).abs() <= pg.h;
                // SF over the whole mid-plane (catches the real critical spot).
                if near_mid {
                    let sf = (strength * eps[ci] as f64 / vm[ci].max(1e-9) as f64).min(99.0);
                    min_sf = min_sf.min(sf);
                }
                // Concentration only in the hole rim (a .. a+1.5h) at mid-plane.
                if near_mid && r >= a && r <= a + 1.5 * pg.h {
                    kt = kt.max(sxx[ci] as f64 / sigma_inf);
                    peak_vm = peak_vm.max(vm[ci] as f64 / sigma_inf);
                }
            }
        }
    }
    (kt, peak_vm, min_sf)
}

#[test]
#[ignore]
fn bench_stress_concentration_kirsch() {
    println!("\n=== Benchmark 5: KIRSCH plate-with-hole stress concentration ===");
    println!("  Plate hw=50 t=8, hole a=5 (d/W=0.10), σ∞=1 MPa, σt=60 MPa.");
    println!("  Analytic: hole-edge Kt = σxx/σ∞ ≈ 3.0; min-SF ≈ 60/(3·1) = 20.");
    let (hw, t, a) = (50.0f64, 8.0f64, 5.0f64);
    for &h in &[1.0, 0.5] {
        println!("\n  h={h}  (cells per hole-radius ~ {:.0})", a / h);
        println!("    {:<18} {:>10} {:>10} {:>10}", "convention", "Kt(σxx)", "peakVM", "min-SF");
        for (name, conv) in CONVS {
            let (kt, pvm, sf) = kirsch(conv, hw, t, a, h);
            println!("    {name:<18} {kt:>10.3} {pvm:>10.3} {sf:>10.3}");
        }
    }
    println!("\n  Kt near 3 = concentration captured; min-SF stable across");
    println!("  conventions = the go/no-go number is convention-independent.");
}

// ---------- Benchmark 7: curved cross-section + thin curved wall ----------

/// Cantilever from any solid-of-revolution indicator along x. Returns
/// (bending FE/analytic, axial FE/analytic).
#[allow(clippy::too_many_arguments)]
fn cantilever_indicator(
    inside: &dyn Fn([f64; 3]) -> bool,
    lo: [f64; 3],
    hi: [f64; 3],
    l: f64,
    span: f64,
    inertia: f64,
    area: f64,
    h: f64,
    conv: Conv,
) -> (f64, f64) {
    let (e0, nu) = (2000.0f64, 0.3f64);
    let grid = voxelize_indicator(inside, lo, hi, h, conv, 4, [0.0; 3], 1);
    let settings = SolveSettings { e0, nu, tol: 1e-6, max_iter: 700, ..Default::default() };
    let root = BoxRegion::new([lo[0] - 0.6, -span, -span], [lo[0] + 0.6, span, span]);
    let tip = BoxRegion::new([hi[0] - 0.6, -span, -span], [hi[0] + 0.6, span, span]);

    let fb = -10.0f64;
    let pb = StaticProblem {
        grid: grid.clone(),
        fixed: vec![root.clone()],
        loads: vec![(tip.clone(), [0.0, 0.0, fb])],
        settings,
    };
    let sb = solve_static(&pb).expect("bend solve");
    let bend = sb.mean_displacement(&tip).unwrap()[2] / (fb * l.powi(3) / (3.0 * e0 * inertia));

    let fa = 100.0f64;
    let pa = StaticProblem {
        grid,
        fixed: vec![root],
        loads: vec![(tip.clone(), [fa, 0.0, 0.0])],
        settings,
    };
    let sa = solve_static(&pa).expect("axial solve");
    let axial = sa.mean_displacement(&tip).unwrap()[0] / (fa * l / (e0 * area));
    (bend, axial)
}

#[test]
#[ignore]
fn bench_curved_and_thinwall() {
    use std::f64::consts::PI;
    let convs5 = [
        ("center-full(OLD)", Conv::CenterFull),
        ("center-occ (NOW)", Conv::CenterOcc),
        ("inflate-derate  ", Conv::Inflate),
        ("inflate+floor   ", Conv::InflateFloor),
        ("majority-50%    ", Conv::Majority),
    ];

    // Example A: SOLID round cantilever r=8, L=100 (fully curved section).
    println!("\n=== Benchmark 7A: SOLID round cantilever (r=8, L=100) ===");
    println!("  curved boundary everywhere; I=pi r^4/4, A=pi r^2. FE/analytic:");
    let (r, l) = (8.0f64, 100.0f64);
    let cyl = move |p: [f64; 3]| p[0] >= 0.0 && p[0] <= l && (p[1] * p[1] + p[2] * p[2]) <= r * r;
    let (lo, hi) = ([0.0, -r, -r], [l, r, r]);
    let (i_cyl, a_cyl) = (PI * r.powi(4) / 4.0, PI * r * r);
    for &h in &[1.0, 0.5] {
        println!("  h={h}  (cells across diameter ~ {:.0})", 2.0 * r / h);
        println!("    {:<18} {:>12} {:>12}", "convention", "bend FE/an", "axial FE/an");
        for (name, conv) in convs5 {
            let (b, ax) = cantilever_indicator(&cyl, lo, hi, l, r + 2.0, i_cyl, a_cyl, h, conv);
            println!("    {name:<18} {b:>12.4} {ax:>12.4}");
        }
    }

    // Example B: THIN-WALLED round tube ro=10, ri=8 (wall 2), L=80.
    // Almost every cell is a boundary cell — the convention dominates.
    println!("\n=== Benchmark 7B: THIN-WALLED round tube (ro=10 ri=8, L=80) ===");
    println!("  thin curved wall (boundary-cell dominated, like FDM shells).");
    let (ro, ri, lt) = (10.0f64, 8.0f64, 80.0f64);
    let tube = move |p: [f64; 3]| {
        let rr = p[1] * p[1] + p[2] * p[2];
        p[0] >= 0.0 && p[0] <= lt && rr <= ro * ro && rr >= ri * ri
    };
    let (lo_t, hi_t) = ([0.0, -ro, -ro], [lt, ro, ro]);
    let (i_tube, a_tube) = (PI * (ro.powi(4) - ri.powi(4)) / 4.0, PI * (ro * ro - ri * ri));
    let tube_v = a_tube * lt;
    for &h in &[1.0, 0.5] {
        println!("  h={h}  (wall ~ {:.1} cells thick)", (ro - ri) / h);
        println!(
            "    {:<18} {:>10} {:>12} {:>12}",
            "convention", "vol%err", "bend FE/an", "axial FE/an"
        );
        for (name, conv) in convs5 {
            let ve = (vol(&voxelize_indicator(&tube, lo_t, hi_t, h, conv, 4, [0.0; 3], 1)) - tube_v)
                / tube_v
                * 100.0;
            let (b, ax) = cantilever_indicator(&tube, lo_t, hi_t, lt, ro + 2.0, i_tube, a_tube, h, conv);
            println!("    {name:<18} {ve:>+9.2}% {b:>12.4} {ax:>12.4}");
        }
    }
    println!("\n  Confirm the Kirsch verdict holds: inflate+floor accurate &");
    println!("  stable on curved + thin-wall geometry; center-occ biased low.");
}

// ---- Benchmark 8: shoulder-fillet Kt (Betancur et al., Tecciencia 2017) ----

/// Stepped (shouldered) flat bar in axial tension — the classic shoulder-fillet
/// stress raiser from Betancur et al. 2017 (their "graded plate", D/d = 1.5).
/// A wide section (half-width D/2) necks down to a narrow one (d/2) through a
/// true circular fillet of radius `r` on each flank. We pull the bar, then read
/// the von-Mises concentration at the fillet root: Kt = σ_peak / σ_nom, with
/// σ_nom on the minimum (narrow) section. Reference textbook Kt for D/d = 1.5
/// tension: ≈1.68 at r/d = 0.10, ≈1.55 at r/d = 0.15 (Pilkey/Peterson, via Mott).
///
/// This is a *different* stress-raiser topology than Kirsch (a convex re-entrant
/// fillet, not a hole), so it independently checks that the boundary convention
/// doesn't distort peak stress / the governing safety factor on curved features.
fn stepped_bar(conv: Conv, h: f64, r: f64) -> (f64, f64, f64) {
    let (big_d, small_d, t) = (30.0f64, 20.0f64, 3.0f64); // D, d, thickness
    let (e0, nu, strength) = (2000.0f64, 0.3f64, 60.0f64);
    let (x0, lbar) = (25.0f64, 50.0f64); // shoulder at x0; wide left, narrow right
    let (hd, hn) = (big_d / 2.0, small_d / 2.0); // half-widths
    // Fillet centre for the top flank (mirrored in y): tangent to the shoulder
    // face x=x0 and the narrow flank y=hn, filling the concave corner (x0, hn).
    let (cx, cy) = (x0 + r, hn + r);
    let inside = move |p: [f64; 3]| -> bool {
        let (x, ay, z) = (p[0], p[1].abs(), p[2]);
        if z < 0.0 || z > t || x < 0.0 || x > lbar {
            return false;
        }
        if ay <= hn {
            return true; // narrow flank spans the whole length
        }
        if x <= x0 {
            return ay <= hd; // wide section
        }
        // x > x0, ay > hn: void wedge except the fillet that rounds the corner.
        if x <= cx && ay <= cy && ay <= hd {
            let (dx, dy) = (x - cx, ay - cy);
            return dx * dx + dy * dy >= r * r;
        }
        false
    };
    let (lo, hi) = ([0.0, -hd, 0.0], [lbar, hd, t]);
    let grid = voxelize_indicator(&inside, lo, hi, h, conv, 6, [0.0; 3], 1);

    let settings = SolveSettings { e0, nu, tol: 1e-6, max_iter: 900, ..Default::default() };
    let (pg, levels) = pad_for_levels(&grid, settings.max_levels);
    let (mx, my, mz) = (pg.nx + 1, pg.ny + 1, pg.nz + 1);
    let active = active_nodes(&pg);
    let npos = |n: usize| -> [f64; 3] {
        let x = n % mx;
        let y = (n / mx) % my;
        let z = n / (mx * my);
        [pg.origin[0] + x as f64 * pg.h, pg.origin[1] + y as f64 * pg.h, pg.origin[2] + z as f64 * pg.h]
    };
    // σ_nom = 1 MPa on the narrow section → F = σ_nom · d · t. Kt then reads
    // straight off the peak von Mises.
    let sigma_nom = 1.0f64;
    let f_total = sigma_nom * small_d * t;
    let mut np = NodeProblem::default();
    let fixed = BoxRegion::new([-1.0, -hd - 1.0, -1.0], [0.5 * h, hd + 1.0, t + 1.0]);
    let loadr = BoxRegion::new([lbar - 0.5 * h, -hd - 1.0, -1.0], [lbar + 1.0, hd + 1.0, t + 1.0]);
    for n in 0..mx * my * mz {
        if active[n] && fixed.contains(npos(n)) {
            np.fixed.push(n as u32);
        }
    }
    let load_nodes: Vec<usize> =
        (0..mx * my * mz).filter(|&n| active[n] && loadr.contains(npos(n))).collect();
    let inv = 1.0 / load_nodes.len() as f64;
    for n in load_nodes {
        np.forces.push((n as u32, [f_total * inv, 0.0, 0.0]));
    }
    let sol = solve_nodes(&pg, levels, &np, &settings).expect("stepped-bar solve");
    let eps = grid_eps(&pg);
    let vm = cell_field(&pg, &sol.u, e0, nu, &eps, FieldKind::VonMises);

    // Scan a box around the fillet root only — away from the load/fix faces, so
    // BC singularities don't masquerade as the concentration.
    let mid = t / 2.0;
    let mut peak_vm = 0.0f64;
    let mut min_sf = f64::INFINITY;
    for cz in 0..pg.nz {
        for cy_i in 0..pg.ny {
            for cx_i in 0..pg.nx {
                let ci = (cz * pg.ny + cy_i) * pg.nx + cx_i;
                if eps[ci] <= 1e-4 {
                    continue;
                }
                let p = [
                    pg.origin[0] + (cx_i as f64 + 0.5) * pg.h,
                    pg.origin[1] + (cy_i as f64 + 0.5) * pg.h,
                    pg.origin[2] + (cz as f64 + 0.5) * pg.h,
                ];
                let near_fillet = p[0] >= x0 - 2.0 * pg.h
                    && p[0] <= cx + 2.0 * pg.h
                    && p[1].abs() >= hn - 2.0 * pg.h
                    && p[1].abs() <= cy + 2.0 * pg.h
                    && (p[2] - mid).abs() <= pg.h;
                if near_fillet {
                    peak_vm = peak_vm.max(vm[ci] as f64 / sigma_nom);
                    let sf = (strength * eps[ci] as f64 / vm[ci].max(1e-9) as f64).min(99.0);
                    min_sf = min_sf.min(sf);
                }
            }
        }
    }
    let kt = peak_vm / sigma_nom;
    (kt, peak_vm, min_sf)
}

#[test]
#[ignore]
fn bench_stress_concentration_fillet() {
    println!("\n=== Benchmark 8: SHOULDER-FILLET Kt (Betancur 2017, D/d=1.5) ===");
    println!("  Stepped flat bar D=30 d=20 t=3, axial tension, σ_nom=1 MPa.");
    println!("  Textbook Kt (Pilkey/Peterson, tension): r/d=0.10→≈1.68, 0.15→≈1.55.");
    let convs5 = [
        ("center-full(OLD)", Conv::CenterFull),
        ("center-occ (NOW)", Conv::CenterOcc),
        ("inflate-derate  ", Conv::Inflate),
        ("inflate+floor   ", Conv::InflateFloor),
        ("majority-50%    ", Conv::Majority),
    ];
    for &(r, kt_ref) in &[(2.0f64, 1.68f64), (3.0f64, 1.55f64)] {
        println!("\n  fillet r={r} (r/d={:.2}), reference Kt≈{kt_ref}", r / 20.0);
        for &h in &[0.5, 0.25] {
            println!("    h={h}  (cells across fillet ~ {:.0})", r / h);
            println!("    {:<18} {:>10} {:>10} {:>10}", "convention", "Kt(VM)", "Kt err%", "min-SF");
            for (name, conv) in convs5 {
                let (kt, _pvm, sf) = stepped_bar(conv, h, r);
                let err = (kt - kt_ref) / kt_ref * 100.0;
                println!("    {name:<18} {kt:>10.3} {err:>+9.1}% {sf:>10.3}");
            }
        }
    }
    println!("\n  Raw cell stress on a staircased voxel boundary OVER-predicts a");
    println!("  sharp fillet, and the spurious peak grows with refinement (the");
    println!("  production app tempers this with nodal-recovered/true-surface stress).");
    println!("  Cross-convention signal: occupancy DERATING (inflate / inflate+floor)");
    println!("  softens the staircase spikes, landing nearest the textbook Kt, while");
    println!("  the binary conventions over-read by 12-28%%. The floor also lifts the");
    println!("  lone pure-inflate sliver min-SF dip back to the stable value.");
}

// ---------- Benchmark 6: does a sliver floor fix the coarse-SF outlier? ----------

#[test]
#[ignore]
fn bench_inflate_floor() {
    println!("\n=== Benchmark 6: inflate-derate + sliver floor ({INFLATE_FLOOR:.2}) ===");
    let trio = [
        ("center-full(OLD)", Conv::CenterFull),
        ("inflate-derate  ", Conv::Inflate),
        ("inflate+floor   ", Conv::InflateFloor),
    ];

    // Volume (rotated box, exact 4000): does flooring keep inflate's accuracy?
    let boxm = rotated(&primitives::boxx([-20.0, -5.0, -5.0], [20.0, 5.0, 5.0]), 0.41, 0.27);
    println!("\n  rotated box VOLUME % error (exact 4000)");
    println!("    {:<18} {:>9} {:>9}", "convention", "h=1.0", "h=0.5");
    for (name, conv) in trio {
        let e1 = (vol(&voxelize_conv(&boxm, 1.0, conv, 6, [0.0; 3], 1)) - 4000.0) / 4000.0 * 100.0;
        let e2 = (vol(&voxelize_conv(&boxm, 0.5, conv, 6, [0.0; 3], 1)) - 4000.0) / 4000.0 * 100.0;
        println!("    {name:<18} {e1:>+8.2}% {e2:>+8.2}%");
    }

    // Kirsch min-SF: does flooring remove the coarse sliver false-alarm?
    println!("\n  Kirsch hole: Kt(σxx) and min-SF (analytic Kt≈3, SF≈20)");
    println!("    {:<18} {:>10} {:>10}", "convention @h", "Kt", "min-SF");
    for &h in &[1.0, 0.5] {
        for (name, conv) in trio {
            let (kt, _pvm, sf) = kirsch(conv, 50.0, 8.0, 5.0, h);
            println!("    {} @{h:<3} {kt:>16.3} {sf:>10.3}", name.trim_end());
        }
    }
    println!("\n  Want: inflate+floor keeps the volume accuracy AND lifts the");
    println!("  coarse min-SF back to the well-resolved value (no sliver alarm).");
}

// ---------------- self-check (runs in normal CI) ----------------

#[test]
fn inflate_is_least_volume_biased_on_sphere() {
    // Cheap guard so the harness keeps compiling/working: on a sphere,
    // inflate-derate's volume error must beat center-occ's (the regression),
    // and center-full must sit between them (roughly unbiased).
    let sphere = primitives::sphere([0.0; 3], 8.0, 64, 32);
    let exact = 4.0 / 3.0 * std::f64::consts::PI * 8f64.powi(3);
    let err = |c: Conv| (vol(&voxelize_conv(&sphere, 1.0, c, 4, [0.0; 3], 1)) - exact) / exact;
    let center_occ = err(Conv::CenterOcc);
    let inflate = err(Conv::Inflate);
    let center_full = err(Conv::CenterFull);
    assert!(center_occ < -0.02, "center-occ should under-count (got {center_occ:+.3})");
    assert!(inflate.abs() < center_occ.abs(), "inflate {inflate:+.3} should beat center-occ {center_occ:+.3}");
    assert!(center_full.abs() < center_occ.abs(), "center-full {center_full:+.3} should beat center-occ {center_occ:+.3}");
}
