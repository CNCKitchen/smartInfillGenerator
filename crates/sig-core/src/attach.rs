// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Attach user boundary conditions (defined as triangle selections on the
//! input mesh) to voxel-grid nodes, assemble the node-level problem, and run
//! the pre-solve constraint check (islands + rigid-body modes).

use crate::bvh::WindingBvh;
use crate::check::{islands, rbm_check, ConstraintDir, RbmMode};
use crate::mesh::TriMesh;
use crate::segment::average_normal;
use crate::solve::{boundary_nodes, NodeProblem, SolveSettings};
use crate::voxel::VoxelGrid;

/// Penalty stiffness multiplier for frictionless supports, relative to E0*h.
const SPRING_FACTOR: f64 = 300.0;
/// Max distance (in cell sizes) from a boundary node to the SELECTION for the
/// node to count as attached. Must sit between the stair-step deviation of a
/// voxel surface (~0.87h worst case) and the next node ring (1.0h).
const ATTACH_DIST_CELLS: f64 = 0.9;

#[derive(Clone, Debug)]
pub enum BcKind {
    Fixed,
    Frictionless,
    /// Elastic ("soft") support: Winkler foundation with bedding modulus k in
    /// N/mm³ (surface pressure per unit displacement, σ = k·u). Each attached
    /// node gets three axis springs of k × its tributary selection area —
    /// a compliant mount instead of a rigid wall, so the part is not
    /// artificially stiffened and the support-edge stress singularity of a
    /// Fixed patch is spread out physically.
    Elastic(f64),
    /// Total force vector (N), split equally over attached nodes.
    Force([f64; 3]),
    /// Pressure (MPa), applied as total force -p * (sum of selected area vectors).
    Pressure(f64),
}

#[derive(Clone, Debug)]
pub struct BcSpec {
    pub kind: BcKind,
    /// Selected triangle indices into the mesh.
    pub tris: Vec<u32>,
}

#[derive(Debug)]
pub enum AttachError {
    EmptySelection(usize),
    NoNodesAttached(usize),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::EmptySelection(i) => write!(f, "boundary condition {i} has no triangles"),
            AttachError::NoNodesAttached(i) => {
                write!(f, "boundary condition {i} maps to no grid nodes (selection too small for this resolution?)")
            }
        }
    }
}

impl std::error::Error for AttachError {}

pub struct Assembled {
    pub problem: NodeProblem,
    /// Nodes attached to each BC, in input order (for visualization/debug).
    pub bc_nodes: Vec<Vec<u32>>,
    /// Per-BC constraint directions contributed to the rigid-body check.
    constraints: Vec<ConstraintDir>,
    load_nodes: Vec<u32>,
}

/// Map each BC's triangle selection to boundary nodes of the (padded) grid.
/// A node attaches to a BC when it lies within ATTACH_DIST_CELLS*h of the
/// selected triangles themselves (per-BC sub-BVH) — nodes on shared edges and
/// corners attach to every adjacent BC, which is what supports need.
pub fn assemble(
    mesh: &TriMesh,
    grid: &VoxelGrid,
    bcs: &[BcSpec],
    gravity: Option<([f64; 3], f64)>, // (acceleration mm/s², density tonne/mm³)
    settings: &SolveSettings,
) -> Result<Assembled, AttachError> {
    let h = grid.h;
    let (mx, my) = (grid.nx + 1, grid.ny + 1);
    let node_pos = |n: u32| -> [f64; 3] {
        let n = n as usize;
        let x = n % mx;
        let y = (n / mx) % my;
        let z = n / (mx * my);
        [
            grid.origin[0] + x as f64 * h,
            grid.origin[1] + y as f64 * h,
            grid.origin[2] + z as f64 * h,
        ]
    };
    let boundary = boundary_nodes(grid);
    let attach_d2 = (ATTACH_DIST_CELLS * h) * (ATTACH_DIST_CELLS * h);

    let mut problem = NodeProblem::default();
    let mut bc_nodes: Vec<Vec<u32>> = Vec::with_capacity(bcs.len());
    let mut constraints: Vec<ConstraintDir> = Vec::new();
    let mut load_nodes: Vec<u32> = Vec::new();

    for (bi, bc) in bcs.iter().enumerate() {
        if bc.tris.is_empty() {
            return Err(AttachError::EmptySelection(bi));
        }
        let sel = bc.tris.clone();
        // Mini-BVH over just this selection: distance to the SELECTION decides
        // attachment (immune to nearest-triangle ties at face borders).
        let sub_mesh = TriMesh::from_triangles(
            sel.iter().map(|&ti| mesh.tris[ti as usize]).collect(),
        );
        let sub_bvh = WindingBvh::build(&sub_mesh);
        // Selection bounding box + margin restricts the candidate nodes.
        let (lo, hi) = sub_mesh.bounds().unwrap();
        let margin = 2.0 * h;
        let nodes: Vec<u32> = boundary
            .iter()
            .copied()
            .filter(|&n| {
                let p = node_pos(n);
                (0..3).all(|d| p[d] >= lo[d] - margin && p[d] <= hi[d] + margin)
            })
            .filter(|&n| sub_bvh.closest_triangle(node_pos(n)).1 <= attach_d2)
            .collect();
        if nodes.is_empty() {
            return Err(AttachError::NoNodesAttached(bi));
        }

        match &bc.kind {
            BcKind::Fixed => {
                for &n in &nodes {
                    problem.fixed.push(n);
                    let p = node_pos(n);
                    for d in 0..3 {
                        let mut dir = [0f64; 3];
                        dir[d] = 1.0;
                        constraints.push(ConstraintDir { pos: p, dir });
                    }
                }
            }
            BcKind::Frictionless => {
                let normal = average_normal(mesh, &sel);
                let k = SPRING_FACTOR * settings.e0 * h;
                for &n in &nodes {
                    problem.springs.push((n, normal, k));
                    constraints.push(ConstraintDir { pos: node_pos(n), dir: normal });
                }
            }
            BcKind::Elastic(k_found) => {
                // Consistent Winkler foundation: node spring = modulus times
                // the node's tributary area of the selection, so the total
                // foundation stiffness is k * A regardless of resolution.
                let k_found = k_found.max(0.0);
                let w = area_weights(mesh, &sel, &nodes, grid);
                for (i, &n) in nodes.iter().enumerate() {
                    let k = k_found * w[i];
                    if k <= 0.0 {
                        continue;
                    }
                    let p = node_pos(n);
                    for d in 0..3 {
                        let mut dir = [0f64; 3];
                        dir[d] = 1.0;
                        problem.springs.push((n, dir, k));
                        constraints.push(ConstraintDir { pos: p, dir });
                    }
                }
            }
            BcKind::Force(f) => {
                // Area-weighted distribution => consistent nodal loads on flat
                // faces (corner/edge/interior get their Voronoi share).
                let w = area_weights(mesh, &sel, &nodes, grid);
                let total: f64 = w.iter().sum();
                for (i, &n) in nodes.iter().enumerate() {
                    let s = w[i] / total;
                    problem.forces.push((n, [f[0] * s, f[1] * s, f[2] * s]));
                    load_nodes.push(n);
                }
            }
            BcKind::Pressure(p) => {
                // Per-sample normals: correct on curved selections.
                let fv = pressure_forces(mesh, &sel, &nodes, grid, *p);
                for (i, &n) in nodes.iter().enumerate() {
                    problem.forces.push((n, fv[i]));
                    load_nodes.push(n);
                }
            }
        }
        bc_nodes.push(nodes);
    }

    // Gravity: body force per solid cell, lumped to its 8 nodes.
    if let Some((g, density)) = gravity {
        let cell_f = [0, 1, 2].map(|d| density * g[d] * h * h * h / 8.0);
        if cell_f.iter().any(|&v| v != 0.0) {
            for cz in 0..grid.nz {
                for cy in 0..grid.ny {
                    for cx in 0..grid.nx {
                        if grid.scale[(cz * grid.ny + cy) * grid.nx + cx] <= 0.0 {
                            continue;
                        }
                        for oz in 0..2 {
                            for oy in 0..2 {
                                for ox in 0..2 {
                                    let n = ((cz + oz) * my + cy + oy) * mx + cx + ox;
                                    problem.forces.push((n as u32, cell_f));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(Assembled { problem, bc_nodes, constraints, load_nodes })
}

/// Sample selected triangles on a sub-cell lattice; route each sample's area
/// share to the nearest attached node. `f(node_slot, tri_index, sample_area)`.
fn sample_selection<F: FnMut(usize, u32, f64)>(
    mesh: &TriMesh,
    sel: &[u32],
    nodes: &[u32],
    grid: &VoxelGrid,
    mut f: F,
) {
    let h = grid.h;
    let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
    // Slot lookup for attached nodes.
    let mut slot_of: std::collections::HashMap<u32, usize> = Default::default();
    for (i, &n) in nodes.iter().enumerate() {
        slot_of.insert(n, i);
    }
    let node_of = |x: i64, y: i64, z: i64| -> Option<u32> {
        if x < 0 || y < 0 || z < 0 || x >= mx as i64 || y >= my as i64 || z >= mz as i64 {
            return None;
        }
        Some(((z as usize * my + y as usize) * mx + x as usize) as u32)
    };
    let mut leftovers: f64 = 0.0;
    let mut leftover_tris: Vec<(u32, f64)> = Vec::new();
    for &ti in sel {
        let t = &mesh.tris[ti as usize];
        let a = [t[0] as f64, t[1] as f64, t[2] as f64];
        let b = [t[3] as f64, t[4] as f64, t[5] as f64];
        let c = [t[6] as f64, t[7] as f64, t[8] as f64];
        let av = crate::mesh::triangle_area_vector(t);
        let area = ((av[0] as f64).powi(2) + (av[1] as f64).powi(2) + (av[2] as f64).powi(2))
            .sqrt();
        if area <= 0.0 {
            continue;
        }
        let edge = |p: [f64; 3], q: [f64; 3]| {
            ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt()
        };
        let longest = edge(a, b).max(edge(b, c)).max(edge(c, a));
        let m = ((longest / (0.5 * h)).ceil() as usize).clamp(1, 48);
        let sample_area = area / (m * m) as f64;
        for i in 0..m {
            for j in 0..m {
                let (mut u, mut v) = ((i as f64 + 0.5) / m as f64, (j as f64 + 0.5) / m as f64);
                if u + v > 1.0 {
                    u = 1.0 - u;
                    v = 1.0 - v;
                }
                let p = [
                    a[0] + u * (b[0] - a[0]) + v * (c[0] - a[0]),
                    a[1] + u * (b[1] - a[1]) + v * (c[1] - a[1]),
                    a[2] + u * (b[2] - a[2]) + v * (c[2] - a[2]),
                ];
                let gx = ((p[0] - grid.origin[0]) / h).round() as i64;
                let gy = ((p[1] - grid.origin[1]) / h).round() as i64;
                let gz = ((p[2] - grid.origin[2]) / h).round() as i64;
                // Nearest attached node within an expanding neighborhood.
                let mut best: Option<(usize, f64)> = None;
                for radius in [1i64, 2] {
                    for dz in -radius..=radius {
                        for dy in -radius..=radius {
                            for dx in -radius..=radius {
                                if let Some(n) = node_of(gx + dx, gy + dy, gz + dz) {
                                    if let Some(&slot) = slot_of.get(&n) {
                                        let np = [
                                            grid.origin[0] + (gx + dx) as f64 * h,
                                            grid.origin[1] + (gy + dy) as f64 * h,
                                            grid.origin[2] + (gz + dz) as f64 * h,
                                        ];
                                        let d2 = (np[0] - p[0]).powi(2)
                                            + (np[1] - p[1]).powi(2)
                                            + (np[2] - p[2]).powi(2);
                                        if best.map_or(true, |(_, bd)| d2 < bd) {
                                            best = Some((slot, d2));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if best.is_some() {
                        break;
                    }
                }
                match best {
                    Some((slot, _)) => f(slot, ti, sample_area),
                    None => {
                        leftovers += sample_area;
                        leftover_tris.push((ti, sample_area));
                    }
                }
            }
        }
    }
    // Orphan samples (selection thinner than the grid can see): spread evenly.
    if leftovers > 0.0 && !nodes.is_empty() {
        for (ti, sa) in leftover_tris {
            for slot in 0..nodes.len() {
                f(slot, ti, sa / nodes.len() as f64);
            }
        }
    }
}

/// Per-attached-node area share of the selection.
fn area_weights(mesh: &TriMesh, sel: &[u32], nodes: &[u32], grid: &VoxelGrid) -> Vec<f64> {
    let mut w = vec![0f64; nodes.len()];
    sample_selection(mesh, sel, nodes, grid, |slot, _ti, a| w[slot] += a);
    // Guard: never let all-zero weights through.
    if w.iter().sum::<f64>() <= 0.0 {
        w.fill(1.0);
    }
    w
}

/// Per-attached-node force vectors for pressure p, using per-triangle normals.
fn pressure_forces(
    mesh: &TriMesh,
    sel: &[u32],
    nodes: &[u32],
    grid: &VoxelGrid,
    p: f64,
) -> Vec<[f64; 3]> {
    // Unit normals per selected triangle.
    let mut normal_of: std::collections::HashMap<u32, [f64; 3]> = Default::default();
    for &ti in sel {
        let av = crate::mesh::triangle_area_vector(&mesh.tris[ti as usize]);
        let len = ((av[0] as f64).powi(2) + (av[1] as f64).powi(2) + (av[2] as f64).powi(2))
            .sqrt();
        let n = if len > 0.0 {
            [av[0] as f64 / len, av[1] as f64 / len, av[2] as f64 / len]
        } else {
            [0.0; 3]
        };
        normal_of.insert(ti, n);
    }
    let mut fv = vec![[0f64; 3]; nodes.len()];
    sample_selection(mesh, sel, nodes, grid, |slot, ti, a| {
        let n = normal_of[&ti];
        for d in 0..3 {
            fv[slot][d] += -p * a * n[d];
        }
    });
    fv
}

#[derive(Clone, Debug)]
pub struct ComponentReport {
    pub cells: usize,
    pub constrained: bool,
    pub lambda_ratio: f64,
    pub has_loads: bool,
    /// Free rigid-body motion when under-constrained.
    pub mode: Option<RbmMode>,
}

#[derive(Clone, Debug)]
pub struct CheckReport {
    pub ok: bool,
    pub island_count: usize,
    pub components: Vec<ComponentReport>,
}

/// Island + rigid-body-mode check for an assembled problem.
pub fn check_problem(grid: &VoxelGrid, assembled: &Assembled) -> CheckReport {
    let isl = islands(grid);
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let h = grid.h;

    // Component id of a node = component of any adjacent solid cell.
    let node_component = |n: u32| -> Option<u32> {
        let n = n as usize;
        let x = n % mx;
        let y = (n / mx) % my;
        let z = n / (mx * my);
        for dz in 0..2usize {
            for dy in 0..2usize {
                for dx in 0..2usize {
                    if dx > x || dy > y || dz > z {
                        continue;
                    }
                    let (cx, cy, cz) = (x - dx, y - dy, z - dz);
                    if cx < nx && cy < ny && cz < nz {
                        let c = isl.cell_component[(cz * ny + cy) * nx + cx];
                        if c != u32::MAX {
                            return Some(c);
                        }
                    }
                }
            }
        }
        None
    };

    // Per-component geometry (centroid, bbox) for conditioning.
    let mut cells = vec![0usize; isl.count];
    let mut centroid = vec![[0f64; 3]; isl.count];
    let mut lo = vec![[f64::INFINITY; 3]; isl.count];
    let mut hi = vec![[f64::NEG_INFINITY; 3]; isl.count];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let c = isl.cell_component[(cz * ny + cy) * nx + cx];
                if c == u32::MAX {
                    continue;
                }
                let c = c as usize;
                let p = [
                    grid.origin[0] + (cx as f64 + 0.5) * h,
                    grid.origin[1] + (cy as f64 + 0.5) * h,
                    grid.origin[2] + (cz as f64 + 0.5) * h,
                ];
                cells[c] += 1;
                for d in 0..3 {
                    centroid[c][d] += p[d];
                    lo[c][d] = lo[c][d].min(p[d]);
                    hi[c][d] = hi[c][d].max(p[d]);
                }
            }
        }
    }
    for c in 0..isl.count {
        for d in 0..3 {
            centroid[c][d] /= cells[c].max(1) as f64;
        }
    }

    // Group constraints and loads by component. A constraint's position is a
    // node position; find its component via adjacent solid cells.
    let mut comp_constraints: Vec<Vec<ConstraintDir>> = vec![Vec::new(); isl.count];
    for cd in &assembled.constraints {
        // Recover the node from the position (constraints were built from nodes).
        let x = ((cd.pos[0] - grid.origin[0]) / h).round() as usize;
        let y = ((cd.pos[1] - grid.origin[1]) / h).round() as usize;
        let z = ((cd.pos[2] - grid.origin[2]) / h).round() as usize;
        let n = ((z * my + y) * mx + x) as u32;
        if let Some(c) = node_component(n) {
            comp_constraints[c as usize].push(*cd);
        }
    }
    let mut comp_loaded = vec![false; isl.count];
    for &n in &assembled.load_nodes {
        if let Some(c) = node_component(n) {
            comp_loaded[c as usize] = true;
        }
    }

    let mut components = Vec::with_capacity(isl.count);
    let mut all_ok = true;
    for c in 0..isl.count {
        let half_diag = (0..3)
            .map(|d| (hi[c][d] - lo[c][d]) * 0.5)
            .fold(0f64, |a, v| a + v * v)
            .sqrt()
            .max(h);
        let r = rbm_check(&comp_constraints[c], centroid[c], half_diag);
        all_ok &= r.ok;
        components.push(ComponentReport {
            cells: cells[c],
            constrained: r.ok,
            lambda_ratio: r.lambda_ratio,
            has_loads: comp_loaded[c],
            mode: r.mode,
        });
    }

    CheckReport { ok: all_ok, island_count: isl.count, components }
}
