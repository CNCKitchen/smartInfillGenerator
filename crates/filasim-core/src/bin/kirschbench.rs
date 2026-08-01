// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Kirsch benchmark: mesh-convergence study of the notch stress at a circular
//! hole against the closed-form / handbook solution.
//!
//! Part: quarter of a 50×50×4 mm plate with a central ⌀5 mm hole (the STEP
//! `KischPlateWHole_quater.step`), x ∈ [−25,0], y ∈ [0,25], z ∈ [0,4], hole
//! centred on the origin. BCs: frictionless on the two symmetry planes x=0 and
//! y=0, frictionless on one z face (the only thing stopping the z rigid-body
//! mode), −1 MPa pressure on the x = −25 end ⇒ 1 MPa gross tension along x.
//!
//! Reference: for uniaxial tension σ along x, the hole-edge tangential stress
//! is σ_θθ = σ(1 − 2cos2θ) — peak 3σ at θ = 90° (the point (0, r), where the
//! tangential direction is x, so the peak IS σxx) and −σ at θ = 0° (the point
//! (−r, 0), where it is σyy). Finite width (d/W = 0.1) raises the peak via
//! Heywood: Kt_net = 2 + (1−d/W)³ on the net section ⇒ Kt_gross = 3.032.
//! Along the ligament x = 0 the infinite-plate profile is
//! σxx(y) = σ/2 · (2 + (r/y)² + 3(r/y)⁴).
//!
//! The point of the harness is that the SAME solution is read back the four
//! ways the app offers — per-cell on the voxel hull, nodal-recovered on the
//! voxel hull ("smoothed"), nearest-cell on the CAD surface, and trilinear-
//! nodal on the CAD surface — each with `material_stress` on and off, for both
//! σ₁ and σ_eqv. Run:
//!
//! ```text
//! cargo run --release -p filasim-core --bin kirschbench -- --mesh kirsch.bin \
//!     --h 1.0,0.7,0.5,0.37,0.25,0.2,0.125
//! ```
//!
//! `kirsch.bin` is produced by the companion Node script (meshStep tessellation
//! of the STEP, identical to the app's import path): a 12-byte header
//! ("KRSH", u32 ntri, u32 nface), ntri×9 f32 triangle soup in mm, ntri u32
//! dense CAD-face ids, nface f32 face areas.

use filasim_core::attach::{assemble, check_problem, BcKind, BcSpec};
use filasim_core::eps::{grid_eps, material_factor};
use filasim_core::mesh::TriMesh;
use filasim_core::solve::{solve_nodes_cached, SolveSettings};
use filasim_core::stress::{
    cell_field, cell_field_cut, cut_normals, recover_nodal, recover_surface, surface_band,
    FieldKind,
};
use filasim_core::voxel::VoxelGrid;
use filasim_core::pad_for_levels;

const R: f64 = 2.5; // hole radius, mm
const SIGMA: f64 = 1.0; // gross applied tension, MPa
const D_OVER_W: f64 = 0.1; // 5 mm hole in a 50 mm wide plate

/// Heywood finite-width stress concentration on the GROSS section.
fn kt_gross() -> f64 {
    let kt_net = 2.0 + (1.0 - D_OVER_W).powi(3);
    kt_net / (1.0 - D_OVER_W)
}

/// Infinite-plate Kirsch σxx along the ligament x = 0 (y ≥ r).
fn kirsch_ligament(y: f64) -> f64 {
    let t = R / y;
    0.5 * SIGMA * (2.0 + t * t + 3.0 * t.powi(4))
}

struct Mesh {
    tris: Vec<[f32; 9]>,
    face_of_tri: Vec<u32>,
}

fn load_mesh(path: &str) -> Mesh {
    let b = std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    assert_eq!(&b[0..4], b"KRSH", "bad magic in {path}");
    let ntri = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    let nface = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
    let mut o = 12;
    let mut tris = Vec::with_capacity(ntri);
    for _ in 0..ntri {
        let mut t = [0f32; 9];
        for v in t.iter_mut() {
            *v = f32::from_le_bytes(b[o..o + 4].try_into().unwrap());
            o += 4;
        }
        tris.push(t);
    }
    let mut face_of_tri = Vec::with_capacity(ntri);
    for _ in 0..ntri {
        face_of_tri.push(u32::from_le_bytes(b[o..o + 4].try_into().unwrap()));
        o += 4;
    }
    o += 4 * nface; // areas — informational, recomputed below
    let _ = o;
    Mesh { tris, face_of_tri }
}

/// Classify the CAD faces geometrically so the harness does not hard-code
/// dense ids: returns (x=−25 end, y=0 plane, x=0 plane, z=0 face, z=4 face).
fn classify(mesh: &TriMesh, face_of: &[u32]) -> [Vec<u32>; 5] {
    let nface = face_of.iter().copied().max().unwrap() as usize + 1;
    let mut cent = vec![[0f64; 3]; nface];
    let mut nrm = vec![[0f64; 3]; nface];
    let mut area = vec![0f64; nface];
    for (ti, t) in mesh.tris.iter().enumerate() {
        let f = face_of[ti] as usize;
        let u = [t[3] - t[0], t[4] - t[1], t[5] - t[2]];
        let v = [t[6] - t[0], t[7] - t[1], t[8] - t[2]];
        let av = [
            0.5 * (u[1] * v[2] - u[2] * v[1]),
            0.5 * (u[2] * v[0] - u[0] * v[2]),
            0.5 * (u[0] * v[1] - u[1] * v[0]),
        ];
        let a = ((av[0] as f64).powi(2) + (av[1] as f64).powi(2) + (av[2] as f64).powi(2)).sqrt();
        area[f] += a;
        for d in 0..3 {
            cent[f][d] += a * (t[d] + t[3 + d] + t[6 + d]) as f64 / 3.0;
            nrm[f][d] += av[d] as f64;
        }
    }
    let mut out: [Vec<u32>; 5] = Default::default();
    for f in 0..nface {
        if area[f] <= 0.0 {
            continue;
        }
        let c = [cent[f][0] / area[f], cent[f][1] / area[f], cent[f][2] / area[f]];
        let nl = (nrm[f][0].powi(2) + nrm[f][1].powi(2) + nrm[f][2].powi(2)).sqrt();
        let n = [nrm[f][0] / nl, nrm[f][1] / nl, nrm[f][2] / nl];
        let slot = if n[0].abs() > 0.99 && c[0] < -20.0 {
            0 // x = −25 loaded end
        } else if n[1].abs() > 0.99 && c[1].abs() < 1e-3 {
            1 // y = 0 symmetry plane
        } else if n[0].abs() > 0.99 && c[0].abs() < 1e-3 {
            2 // x = 0 symmetry plane
        } else if n[2].abs() > 0.99 && c[2].abs() < 1e-3 {
            3 // z = 0
        } else if n[2].abs() > 0.99 && (c[2] - 4.0).abs() < 1e-3 {
            4 // z = 4
        } else {
            continue;
        };
        for (ti, &ff) in face_of.iter().enumerate() {
            if ff as usize == f {
                out[slot].push(ti as u32);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The app's two CAD-surface samplers, copied verbatim from filasim-wasm so the
// harness reads the field EXACTLY as the viewport does.
// ---------------------------------------------------------------------------

fn sample_cell_values(tris: &[[f32; 9]], grid: &VoxelGrid, values: &[f32]) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let h = grid.h;
    let cell_at = |p: [f64; 3]| -> Option<usize> {
        let cx = ((p[0] - grid.origin[0]) / h).floor() as i64;
        let cy = ((p[1] - grid.origin[1]) / h).floor() as i64;
        let cz = ((p[2] - grid.origin[2]) / h).floor() as i64;
        if cx < 0 || cy < 0 || cz < 0 || cx >= nx as i64 || cy >= ny as i64 || cz >= nz as i64 {
            return None;
        }
        Some(((cz as usize) * ny + cy as usize) * nx + cx as usize)
    };
    let mut out = Vec::with_capacity(tris.len() * 3);
    for t in tris {
        for v in 0..3 {
            let p = [t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64];
            let mut val = 0.0f32;
            'search: for r in 0..3i64 {
                for dz in -r..=r {
                    for dy in -r..=r {
                        for dx in -r..=r {
                            let q =
                                [p[0] + dx as f64 * h, p[1] + dy as f64 * h, p[2] + dz as f64 * h];
                            if let Some(ci) = cell_at(q) {
                                if grid.scale[ci] > 0.0 {
                                    val = values[ci];
                                    break 'search;
                                }
                            }
                        }
                    }
                }
            }
            out.push(val);
        }
    }
    out
}

fn sample_nodal_values(
    tris: &[[f32; 9]],
    grid: &VoxelGrid,
    nodal: &[f32],
    cells: &[f32],
) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let (h, o) = (grid.h, grid.origin);
    let mut out = Vec::with_capacity(tris.len() * 3);
    let mut fallback: Vec<usize> = Vec::new();
    for (ti, t) in tris.iter().enumerate() {
        for v in 0..3 {
            let p = [t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64];
            let f = |d: usize| ((p[d] - o[d]) / h).clamp(0.0, [nx, ny, nz][d] as f64 - 1e-9);
            let (fx, fy, fz) = (f(0), f(1), f(2));
            let (cx, cy, cz) = (fx.floor() as usize, fy.floor() as usize, fz.floor() as usize);
            let (tx, ty, tz) = (fx - cx as f64, fy - cy as f64, fz - cz as f64);
            let (mut val, mut wsum) = (0f64, 0f64);
            for oz in 0..2 {
                for oy in 0..2 {
                    for ox in 0..2 {
                        let nv = nodal[((cz + oz) * my + (cy + oy)) * mx + (cx + ox)];
                        if nv.is_nan() {
                            continue;
                        }
                        let w = (if ox == 1 { tx } else { 1.0 - tx })
                            * (if oy == 1 { ty } else { 1.0 - ty })
                            * (if oz == 1 { tz } else { 1.0 - tz });
                        val += w * nv as f64;
                        wsum += w;
                    }
                }
            }
            if wsum > 1e-9 {
                out.push((val / wsum) as f32);
            } else {
                out.push(0.0);
                fallback.push(ti * 3 + v);
            }
        }
    }
    if !fallback.is_empty() {
        let near = sample_cell_values(tris, grid, cells);
        for i in fallback {
            out[i] = near[i];
        }
    }
    out
}

/// Max value + its position over a per-vertex field on a triangle soup.
fn max_at(tris: &[[f32; 9]], vals: &[f32]) -> (f64, [f64; 3]) {
    let mut best = (f64::NEG_INFINITY, [0f64; 3]);
    for (i, &v) in vals.iter().enumerate() {
        if v as f64 > best.0 {
            let (t, k) = (i / 3, i % 3);
            best = (
                v as f64,
                [
                    tris[t][3 * k] as f64,
                    tris[t][3 * k + 1] as f64,
                    tris[t][3 * k + 2] as f64,
                ],
            );
        }
    }
    best
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str, def: &str| -> String {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| def.to_string())
    };
    let mesh_path = arg("--mesh", "kirsch.bin");
    let hs: Vec<f64> =
        arg("--h", "1.0,0.7,0.5,0.37,0.25,0.2,0.125").split(',').map(|s| s.trim().parse().unwrap()).collect();
    let zface = arg("--zface", "bottom"); // which z face carries the frictionless
    let e0: f64 = arg("--e0", "2400").parse().unwrap();
    let nu: f64 = arg("--nu", "0.35").parse().unwrap();
    let tol: f64 = arg("--tol", "1e-9").parse().unwrap();
    let ligament = args.iter().any(|a| a == "--ligament");
    let thickness = args.iter().any(|a| a == "--thickness");
    let probe = args.iter().any(|a| a == "--probe");
    let surface = !args.iter().any(|a| a == "--no-surface");
    let excl_check = args.iter().any(|a| a == "--excl");

    let raw = load_mesh(&mesh_path);
    let mesh_orig = TriMesh::from_triangles(raw.tris);
    // The app's display refinement (Model::from_import): longest-edge cap at
    // diag/60 with CAD faces remapped through the parent map.
    let (lo, hi) = mesh_orig.bounds().unwrap();
    let diag = ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2) + (hi[2] - lo[2]).powi(2)).sqrt();
    let (mesh, parents) = mesh_orig.capped_edges(diag / 60.0, 160_000);
    let face_of: Vec<u32> = parents.iter().map(|&p| raw.face_of_tri[p as usize]).collect();
    let faces = classify(&mesh, &face_of);
    println!(
        "mesh: {} tris (orig {}), bounds [{:.3},{:.3},{:.3}]..[{:.3},{:.3},{:.3}]",
        mesh.tris.len(),
        mesh_orig.tris.len(),
        lo[0], lo[1], lo[2], hi[0], hi[1], hi[2]
    );
    println!(
        "faces: end(x=-25)={} sym(y=0)={} sym(x=0)={} z0={} z4={}   z-support on {zface}",
        faces[0].len(), faces[1].len(), faces[2].len(), faces[3].len(), faces[4].len()
    );

    let zsel = if zface == "top" { faces[4].clone() } else { faces[3].clone() };
    let bcs = vec![
        BcSpec { kind: BcKind::Frictionless, tris: faces[1].clone() }, // y = 0
        BcSpec { kind: BcKind::Frictionless, tris: faces[2].clone() }, // x = 0
        BcSpec { kind: BcKind::Frictionless, tris: zsel },
        BcSpec { kind: BcKind::Pressure(-SIGMA), tris: faces[0].clone() },
    ];

    let kt = kt_gross();
    println!(
        "\nreference: infinite-plate Kt = 3.000 (σ_max = {:.3} MPa)   \
         finite-width (d/W = {D_OVER_W}) Kt_gross = {:.3} (σ_max = {:.3} MPa)",
        3.0 * SIGMA, kt, kt * SIGMA
    );
    println!("           hole-edge compression at θ=0: σyy = {:.3} MPa\n", -SIGMA);

    let settings = SolveSettings { e0, nu, tol, max_iter: 4000, ..Default::default() };

    println!("{}", "=".repeat(150));
    println!(
        "{:>6} {:>14} {:>9} {:>5} {:>7} {:>8} | {:>44} | {:>44}",
        "h", "grid", "cells", "it", "resid", "load N", "σ₁ peak  [MPa] (err % vs 3.032)", "σ_eqv peak [MPa] (err % vs 3.032)"
    );
    println!(
        "{:>6} {:>14} {:>9} {:>5} {:>7} {:>8} | {:>10} {:>10} {:>10} {:>10} | {:>10} {:>10} {:>10} {:>10}",
        "", "", "", "", "", "", "vox raw", "vox smth", "stl raw", "stl smth", "vox raw", "vox smth", "stl raw", "stl smth"
    );
    println!("{}", "=".repeat(150));

    for &h in &hs {
        let grid0 = VoxelGrid::voxelize(&mesh, h);
        let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
        let asm = match assemble(&mesh, &grid, &bcs, None, &settings) {
            Ok(a) => a,
            Err(e) => {
                println!("h={h:.4}: attach failed: {e}");
                continue;
            }
        };
        let chk = check_problem(&grid, &asm);
        let load: [f64; 3] = asm.problem.forces[asm.bc_forces[3].clone()]
            .iter()
            .fold([0f64; 3], |a, (_, f)| [a[0] + f[0], a[1] + f[1], a[2] + f[2]]);
        let load_mag = (load[0] * load[0] + load[1] * load[1] + load[2] * load[2]).sqrt();

        let t0 = std::time::Instant::now();
        let sol = match solve_nodes_cached(&mut None, &grid, levels, &asm.problem, &settings) {
            Ok(s) => s,
            Err(e) => {
                println!("h={h:.4}: solve failed: {e}");
                continue;
            }
        };
        let secs = t0.elapsed().as_secs_f64();

        let eps = grid_eps(&grid);
        let matf = material_factor(&grid, &eps);
        // F1: directional occupancy decoupling accompanies the material factor.
        let cutn = cut_normals(&grid);
        let mat = |k| {
            let c = cell_field_cut(&grid, &sol.u, e0, nu, &matf, None, [0.0; 3], k, Some(&cutn));
            // F3: boundary cells carry the staircase, so read them from the
            // interior instead of trusting them.
            if surface { recover_surface(&grid, &c) } else { c }
        };

        // Four read-back flavors × two field kinds, with material_stress ON
        // (the app default) and OFF.
        let mut row: Vec<[f64; 4]> = Vec::new();
        let mut pos_note = String::new();
        let mut extra = String::new();
        for (fi, kind) in [FieldKind::S1, FieldKind::VonMises].into_iter().enumerate() {
            for (mi, factor) in [&matf, &eps].into_iter().enumerate() {
                let cells = if mi == 0 {
                    mat(kind)
                } else {
                    let c = cell_field(&grid, &sol.u, e0, nu, factor, kind);
                    if surface { recover_surface(&grid, &c) } else { c }
                };
                let nodal = recover_nodal(&grid, &cells);
                // voxel hull, per-cell (crisp) and nodal (smoothed)
                let (tri_hull, _e, cell_of_tri) = grid.surface_mesh_where(&|_| true);
                let mut vox_raw = f64::NEG_INFINITY;
                let mut vox_raw_p = [0f64; 3];
                for (t, &ci) in cell_of_tri.iter().enumerate() {
                    let v = cells[ci as usize] as f64;
                    if v > vox_raw {
                        vox_raw = v;
                        vox_raw_p = [
                            tri_hull[9 * t] as f64,
                            tri_hull[9 * t + 1] as f64,
                            tri_hull[9 * t + 2] as f64,
                        ];
                    }
                }
                let (mx, my) = (grid.nx + 1, grid.ny + 1);
                let mut vox_smt = f64::NEG_INFINITY;
                for p in tri_hull.chunks_exact(3) {
                    let x = (((p[0] as f64 - grid.origin[0]) / h).round() as usize).min(mx - 1);
                    let y = (((p[1] as f64 - grid.origin[1]) / h).round() as usize).min(my - 1);
                    let z = (((p[2] as f64 - grid.origin[2]) / h).round() as usize)
                        .min(grid.nz);
                    let v = nodal[(z * my + y) * mx + x];
                    if !v.is_nan() && v as f64 > vox_smt {
                        vox_smt = v as f64;
                    }
                }
                if fi == 0 && mi == 0 {
                    // Occupancy of the peak cell, and the peak restricted to
                    // FULLY INTERIOR cells (occupancy 1 with all six face
                    // neighbours solid) — the staircase-free subset.
                    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
                    let mut occ_peak = 0f32;
                    let mut interior = f64::NEG_INFINITY;
                    let mut interior_p = [0f64; 3];
                    for cz in 0..nz {
                        for cy in 0..ny {
                            for cx in 0..nx {
                                let ci = (cz * ny + cy) * nx + cx;
                                if grid.scale[ci] <= 0.0 {
                                    continue;
                                }
                                if (cells[ci] as f64 - vox_raw).abs() < 1e-6 {
                                    occ_peak = grid.scale[ci];
                                }
                                if grid.scale[ci] < 1.0 {
                                    continue;
                                }
                                let nb = |x: i64, y: i64, z: i64| {
                                    x >= 0 && y >= 0 && z >= 0
                                        && x < nx as i64 && y < ny as i64 && z < nz as i64
                                        && grid.scale
                                            [(z as usize * ny + y as usize) * nx + x as usize]
                                            >= 1.0
                                };
                                let (x, y, z) = (cx as i64, cy as i64, cz as i64);
                                if !(nb(x - 1, y, z) && nb(x + 1, y, z) && nb(x, y - 1, z)
                                    && nb(x, y + 1, z) && nb(x, y, z - 1) && nb(x, y, z + 1))
                                {
                                    continue;
                                }
                                if (cells[ci] as f64) > interior {
                                    interior = cells[ci] as f64;
                                    interior_p = [
                                        grid.origin[0] + (cx as f64 + 0.5) * h,
                                        grid.origin[1] + (cy as f64 + 0.5) * h,
                                        grid.origin[2] + (cz as f64 + 0.5) * h,
                                    ];
                                }
                            }
                        }
                    }
                    extra = format!(
                        "peak-cell occ {:.2}   interior-only σ₁ peak {:.3} ({:+.1}%) at [{:.2},{:.2},{:.2}]",
                        occ_peak, interior,
                        100.0 * (interior - kt * SIGMA) / (kt * SIGMA),
                        interior_p[0], interior_p[1], interior_p[2]
                    );
                }
                let (stl_raw, stl_raw_p) =
                    max_at(&mesh.tris, &sample_cell_values(&mesh.tris, &grid, &cells));
                let (stl_smt, _) = max_at(
                    &mesh.tris,
                    &sample_nodal_values(&mesh.tris, &grid, &nodal, &cells),
                );
                row.push([vox_raw, vox_smt, stl_raw, stl_smt]);
                if fi == 0 && mi == 0 {
                    pos_note = format!(
                        "peak σ₁ at voxel [{:.2},{:.2},{:.2}] / stl [{:.2},{:.2},{:.2}]  (exact: [0.00,2.50,·])",
                        vox_raw_p[0], vox_raw_p[1], vox_raw_p[2],
                        stl_raw_p[0], stl_raw_p[1], stl_raw_p[2]
                    );
                }
            }
        }
        // row order: [s1 mat, s1 raw, vm mat, vm raw]
        let pct = |v: f64| 100.0 * (v - kt * SIGMA) / (kt * SIGMA);
        let f4 = |a: &[f64; 4]| {
            format!(
                "{:>10} {:>10} {:>10} {:>10}",
                format!("{:.3}", a[0]),
                format!("{:.3}", a[1]),
                format!("{:.3}", a[2]),
                format!("{:.3}", a[3])
            )
        };
        println!(
            "{:>6.4} {:>14} {:>9} {:>5} {:>7.0e} {:>8.2} | {} | {}",
            h,
            format!("{}x{}x{}", grid0.nx, grid0.ny, grid0.nz),
            grid0.solid_count(),
            sol.iterations,
            sol.rel_residual,
            load_mag,
            f4(&row[0]),
            f4(&row[2]),
        );
        println!(
            "{:>6} {:>14} {:>9} {:>5} {:>7} {:>8} | {} | {}",
            "", "(mat off)", "", "", "", "",
            f4(&row[1]),
            f4(&row[3]),
        );
        println!(
            "       err%  σ₁ mat: {:+6.1} {:+6.1} {:+6.1} {:+6.1}   σeqv mat: {:+6.1} {:+6.1} {:+6.1} {:+6.1}   [{:.1}s, {} lvl, chk {}]",
            pct(row[0][0]), pct(row[0][1]), pct(row[0][2]), pct(row[0][3]),
            pct(row[2][0]), pct(row[2][1]), pct(row[2][2]), pct(row[2][3]),
            secs, levels, if chk.ok { "ok" } else { "RBM!" }
        );
        println!("       {pos_note}");
        println!("       {extra}");
        {
            // The shipped uncertainty measure, checked against the truth this
            // benchmark happens to know.
            let rawf = cell_field_cut(&grid, &sol.u, e0, nu, &matf, None, [0.0; 3], FieldKind::S1, Some(&cutn));
            let recf = recover_surface(&grid, &rawf);
            if let Some(b) = surface_band(&grid, &rawf, &recf) {
                println!(
                    "       band: peak {:.3} bound {:.3} → {:5.1}%  [{}]   true err of peak {:+6.1}%, of bound {:+6.1}%",
                    b.peak, b.bound, 100.0 * b.band, b.quality.as_str(),
                    100.0 * (b.peak - kt * SIGMA) / (kt * SIGMA),
                    100.0 * (b.bound - kt * SIGMA) / (kt * SIGMA)
                );
            }
        }

        {
            // Frictionless supports are PENALTY springs (k = 300·E0·h).
            // Check how well they actually enforce u·n = 0 on each symmetry
            // plane, and that the spring reactions balance the applied load.
            let umax = sol.max_displacement();
            for (bi, name) in [(0usize, "y=0"), (1, "x=0"), (2, "z-face")] {
                let mut leak = 0f64;
                let mut react = [0f64; 3];
                for &(n, dir, k) in &asm.problem.springs[asm.bc_springs[bi].clone()] {
                    let n = n as usize;
                    let un = (0..3).map(|d| sol.u[3 * n + d] as f64 * dir[d]).sum::<f64>();
                    leak = leak.max(un.abs());
                    for d in 0..3 {
                        react[d] -= k * un * dir[d];
                    }
                }
                println!(
                    "       support {name}: max |u·n| = {:.3e} mm ({:.4}% of |u|max {:.4e})                        reaction [{:+.2},{:+.2},{:+.2}] N",
                    leak, 100.0 * leak / umax, umax, react[0], react[1], react[2]
                );
            }
        }

        if excl_check {
            // DESIGN §20 dec. 5: the safety-factor CRITERION blanks cells near
            // a rigid constraint patch. On a symmetry model every symmetry
            // plane is a support, so this is worth measuring — how much of the
            // part survives, does the notch, and what SF_crit comes out.
            use filasim_core::strength as sg;
            let spec = sg::StrengthSpec {
                measure: sg::SfMeasure::Material,
                // Deliberately low: at PLA's 50 MPa a 1 MPa load puts every
                // cell above SF_CAP, so the criterion would read 10.0 whether
                // or not it is measuring anything. 10 MPa puts the notch near
                // SF 3 and the number becomes a signal.
                strength: 10.0,
                strength_z: 7.0,
                shear_z: 4.2,
            };
            let solid = grid.scale.iter().filter(|&&s| s > 0.0).count();
            // The notch cell (nearest the exact peak) at mid-thickness.
            let czn = grid.nz / 2;
            let mut notch = (usize::MAX, f64::INFINITY);
            for cy in 0..grid.ny {
                for cx in 0..grid.nx {
                    let ci = (czn * grid.ny + cy) * grid.nx + cx;
                    if grid.scale[ci] <= 0.0 {
                        continue;
                    }
                    let px = grid.origin[0] + (cx as f64 + 0.5) * h;
                    let py = grid.origin[1] + (cy as f64 + 0.5) * h;
                    let d = px * px + (py - R) * (py - R);
                    if d < notch.1 {
                        notch = (ci, d);
                    }
                }
            }
            let crit = |excl: &[bool]| -> (f64, usize, bool, bool) {
                let (mask, dropped) = sg::criterion_mask_checked(&grid, &[], &[], false, excl);
                let cells = sg::sf_cells(&grid, &sol.u, e0, nu, &eps, &mask, &spec);
                let sm = sg::smooth_masked(&grid, &cells, &mask);
                let scored = mask.iter().filter(|&&m| m).count();
                (sg::sf_min(&grid, &sm, &mask), scored, mask[notch.0], dropped)
            };
            // (a) the pre-F2 classification — all three frictionless supports
            //     treated as rigid interfaces — measured with the NEW radius cap.
            let patches: Vec<&[u32]> = asm.bc_nodes[..3].iter().map(|v| &v[..]).collect();
            for (i, p) in patches.iter().enumerate() {
                let area = p.len() as f64 * h * h;
                let d_c = 2.0 * (area / std::f64::consts::PI).sqrt();
                let uncapped = (sg::BC_EXCL_PATCH_FRAC * d_c).max(sg::BC_EXCL_MIN_CELLS * h);
                println!(
                    "       BC{i} patch {:>6} nodes → radius {:.2} mm (uncapped {:.2}, cap {:.2})",
                    p.len(),
                    sg::bc_exclusion_radius(&grid, p.len()),
                    uncapped,
                    sg::BC_EXCL_MAX_THICKNESS_FRAC * sg::min_solid_extent(&grid)
                );
            }
            let excl_a = sg::bc_exclusion(&grid, &patches);
            let (ca, sa, na, da) = crit(&excl_a);
            println!(
                "       frictionless AS rigid : {:>7}/{} cells scored ({:.1}%) · notch {} · SF_crit {:.3}{}",
                sa, solid, 100.0 * sa as f64 / solid as f64,
                if na { "kept" } else { "BLANKED" }, ca,
                if da { "  [exclusion dropped — guard fired]" } else { "" }
            );
            // (b) post-F2: a frictionless support is a roller / symmetry plane,
            //     not an infinite-stiffness bond, so it excludes nothing.
            let (cb, sb, nb, db) = crit(&[]);
            println!(
                "       frictionless AS roller: {:>7}/{} cells scored ({:.1}%) · notch {} · SF_crit {:.3}{}",
                sb, solid, 100.0 * sb as f64 / solid as f64,
                if nb { "kept" } else { "BLANKED" }, cb,
                if db { "  [dropped]" } else { "" }
            );
        }

        if probe {
            // What the boundary max cannot give: the field at FIXED physical
            // points on the ligament, trilinearly sampled from the recovered
            // nodal field. These points sit in the interior, away from the
            // staircase, so they isolate the discretization error of the
            // solution itself from the peak-extraction error.
            let cells = mat(FieldKind::Sxx);
            let nodal = recover_nodal(&grid, &cells);
            let (mx, my) = (grid.nx + 1, grid.ny + 1);
            let at = |p: [f64; 3]| -> f64 {
                let f = |d: usize| {
                    ((p[d] - grid.origin[d]) / h)
                        .clamp(0.0, [grid.nx, grid.ny, grid.nz][d] as f64 - 1e-9)
                };
                let (fx, fy, fz) = (f(0), f(1), f(2));
                let (cx, cy, cz) = (fx.floor() as usize, fy.floor() as usize, fz.floor() as usize);
                let (tx, ty, tz) = (fx - cx as f64, fy - cy as f64, fz - cz as f64);
                let (mut v, mut w) = (0f64, 0f64);
                for oz in 0..2 {
                    for oy in 0..2 {
                        for ox in 0..2 {
                            let nv = nodal[((cz + oz) * my + (cy + oy)) * mx + (cx + ox)];
                            if nv.is_nan() {
                                continue;
                            }
                            let ww = (if ox == 1 { tx } else { 1.0 - tx })
                                * (if oy == 1 { ty } else { 1.0 - ty })
                                * (if oz == 1 { tz } else { 1.0 - tz });
                            v += ww * nv as f64;
                            w += ww;
                        }
                    }
                }
                if w > 1e-9 { v / w } else { f64::NAN }
            };
            let zmid = 2.0;
            let ys = [2.75f64, 3.0, 3.5, 4.0, 5.0, 10.0];
            print!("       ligament σxx MAT  @ x=0,z=2 :");
            for y in ys {
                print!("  y{y:.2}={:.3}({:+.1}%)", at([0.0, y, zmid]),
                    100.0 * (at([0.0, y, zmid]) - kirsch_ligament(y)) / kirsch_ligament(y));
            }
            println!();
            let rcells = cell_field(&grid, &sol.u, e0, nu, &eps, FieldKind::Sxx);
            let rnodal = recover_nodal(&grid, &rcells);
            let at_raw = |p: [f64; 3]| -> f64 {
                let f = |d: usize| {
                    ((p[d] - grid.origin[d]) / h)
                        .clamp(0.0, [grid.nx, grid.ny, grid.nz][d] as f64 - 1e-9)
                };
                let (fx, fy, fz) = (f(0), f(1), f(2));
                let (cx, cy, cz) = (fx.floor() as usize, fy.floor() as usize, fz.floor() as usize);
                let (tx, ty, tz) = (fx - cx as f64, fy - cy as f64, fz - cz as f64);
                let (mut v, mut w) = (0f64, 0f64);
                for oz in 0..2 { for oy in 0..2 { for ox in 0..2 {
                    let nv = rnodal[((cz + oz) * my + (cy + oy)) * mx + (cx + ox)];
                    if nv.is_nan() { continue; }
                    let ww = (if ox == 1 { tx } else { 1.0 - tx })
                        * (if oy == 1 { ty } else { 1.0 - ty })
                        * (if oz == 1 { tz } else { 1.0 - tz });
                    v += ww * nv as f64; w += ww;
                } } }
                if w > 1e-9 { v / w } else { f64::NAN }
            };
            print!("       ligament σxx RAW  @ x=0,z=2 :");
            for y in ys {
                print!("  y{y:.2}={:.3}({:+.1}%)", at_raw([0.0, y, zmid]),
                    100.0 * (at_raw([0.0, y, zmid]) - kirsch_ligament(y)) / kirsch_ligament(y));
            }
            println!();
            // Where does the error live? σxx across the width at y = 10 (four
            // hole radii out, essentially uniform tension) with the cell
            // occupancy alongside — a cut column at the x=0 symmetry plane
            // shows up here immediately.
            let cz = (((2.0 - grid.origin[2]) / h) as usize).min(grid.nz - 1);
            let cy = (((10.0 - grid.origin[1]) / h) as usize).min(grid.ny - 1);
            let raw_cells = cell_field(&grid, &sol.u, e0, nu, &eps, FieldKind::Sxx);
            print!("       σxx across x @ y=10,z=2 :");
            let cxs: Vec<usize> = (0..grid.nx)
                .filter(|&cx| grid.scale[(cz * grid.ny + cy) * grid.nx + cx] > 0.0)
                .collect();
            for &cx in cxs.iter().rev().take(6) {
                let ci = (cz * grid.ny + cy) * grid.nx + cx;
                print!(
                    "  x{:+.3}: mat {:.3} / raw {:.3} (occ {:.2})",
                    grid.origin[0] + (cx as f64 + 0.5) * h,
                    cells[ci],
                    raw_cells[ci],
                    grid.scale[ci]
                );
            }
            println!();
            println!(
                "       grid origin [{:.4},{:.4},{:.4}]  x=0 plane at cell-frac {:.3}  \
                 z=4 face at cell-frac {:.3}  springs on x=0 BC: {}",
                grid.origin[0], grid.origin[1], grid.origin[2],
                (0.0 - grid.origin[0]) / h % 1.0,
                (4.0 - grid.origin[2]) / h % 1.0,
                asm.bc_springs[1].len()
            );
        }

        if thickness {
            // Through-thickness state at the notch cell column (the solid cell
            // nearest the exact peak (0, r, ·)). A traction-free z face must
            // have σzz = 0 ⇒ σ_eqv = σ₁; a frictionless z face is a MIRROR
            // plane, i.e. the mid-plane of a plate of twice the thickness, so
            // σzz builds up there and σ_eqv drops below σ₁.
            let f = |k| mat(k);
            let (s1, vm, szz, sxx) =
                (f(FieldKind::S1), f(FieldKind::VonMises), f(FieldKind::Szz), f(FieldKind::Sxx));
            let cz0 = grid.nz / 2;
            let (mut bx, mut by, mut bd) = (0usize, 0usize, f64::INFINITY);
            for cy in 0..grid.ny {
                for cx in 0..grid.nx {
                    if grid.scale[(cz0 * grid.ny + cy) * grid.nx + cx] <= 0.0 {
                        continue;
                    }
                    let p = [
                        grid.origin[0] + (cx as f64 + 0.5) * h,
                        grid.origin[1] + (cy as f64 + 0.5) * h,
                    ];
                    let d = p[0] * p[0] + (p[1] - R) * (p[1] - R);
                    if d < bd {
                        bd = d;
                        bx = cx;
                        by = cy;
                    }
                }
            }
            println!(
                "       through-thickness at notch cell [{:.3},{:.3}]:",
                grid.origin[0] + (bx as f64 + 0.5) * h,
                grid.origin[1] + (by as f64 + 0.5) * h
            );
            println!(
                "            {:>7} {:>9} {:>9} {:>9} {:>9} {:>9}",
                "z", "σ₁", "σeqv", "σzz", "σxx", "eqv/σ₁"
            );
            for cz in 0..grid.nz {
                let ci = (cz * grid.ny + by) * grid.nx + bx;
                if grid.scale[ci] <= 0.0 {
                    continue;
                }
                println!(
                    "            {:>7.3} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>9.3}",
                    grid.origin[2] + (cz as f64 + 0.5) * h,
                    s1[ci], vm[ci], szz[ci], sxx[ci],
                    vm[ci] / s1[ci].max(1e-9)
                );
            }
        }

        if ligament {
            // σxx along the ligament: the cell row adjacent to the x=0 plane,
            // at mid-thickness, compared with the infinite-plate Kirsch profile.
            let cells = mat(FieldKind::Sxx);
            let cz = grid.nz / 2;
            let cx = (0..grid.nx)
                .rev()
                .find(|&cx| {
                    (0..grid.ny).any(|cy| grid.scale[(cz * grid.ny + cy) * grid.nx + cx] > 0.0)
                })
                .unwrap();
            println!("       ligament σxx at x={:.3}, z={:.3}  (cell centres)",
                grid.origin[0] + (cx as f64 + 0.5) * h,
                grid.origin[2] + (cz as f64 + 0.5) * h);
            println!("            {:>8} {:>10} {:>10} {:>8}", "y", "fea", "kirsch∞", "err%");
            for cy in 0..grid.ny {
                let ci = (cz * grid.ny + cy) * grid.nx + cx;
                if grid.scale[ci] <= 0.0 {
                    continue;
                }
                let y = grid.origin[1] + (cy as f64 + 0.5) * h;
                if y > 4.0 * R {
                    break;
                }
                let k = kirsch_ligament(y.max(R));
                println!(
                    "            {:>8.3} {:>10.3} {:>10.3} {:>8.1}",
                    y, cells[ci], k, 100.0 * (cells[ci] as f64 - k) / k
                );
            }
        }
    }
}
