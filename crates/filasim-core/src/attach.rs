// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Attach user boundary conditions (defined as triangle selections on the
//! input mesh) to voxel-grid nodes, assemble the node-level problem, and run
//! the pre-solve constraint check (islands + rigid-body modes).

use crate::bvh::WindingBvh;
use crate::check::{islands, rbm_check, ConstraintDir, RbmMode};
use crate::mesh::TriMesh;
use crate::rigid::RigidGroup;
use crate::solve::{boundary_nodes, NodeProblem, SolveSettings};
use crate::voxel::VoxelGrid;

/// Penalty stiffness multiplier for frictionless supports, relative to E0*h.
const SPRING_FACTOR: f64 = 300.0;
/// Penalty stiffness multiplier for a RIGID remote mass's patch coupling
/// (DESIGN §16 milestone 4), relative to E0·h. This is the "penalty scaling vs
/// the Chebyshev smoother" the milestone tunes: a rigid-mass sweep (validation
/// `rigid_penalty_convergence_sweep`) shows the patch's achieved rigidity
/// SATURATES by ~factor 5–10 (a stiffer penalty buys no more), while MGCG
/// iterations grow ~√factor and with resolution because the coupling is a
/// fine-only pass-through the coarse grid never sees. 20 sits at the knee:
/// ~2–3× the baseline iteration count (bounded, resolution-stable to 1M cells)
/// for a mount that is already effectively rigid.
const RIGID_FACTOR: f64 = 20.0;
/// Max distance (in cell sizes) from a boundary node to the SELECTION for the
/// node to count as attached. Must sit between the stair-step deviation of a
/// voxel surface (~0.87h worst case) and the next node ring (1.0h).
const ATTACH_DIST_CELLS: f64 = 0.9;

#[derive(Clone, Debug)]
pub enum BcKind {
    Fixed,
    Frictionless,
    /// Displacement support: prescribe any subset of the GLOBAL axes (x/y/z) to
    /// a value (mm) with stiff axis penalty springs — a roller/slider that locks
    /// only the chosen world directions. First array = which axes are enforced;
    /// second = the prescribed displacement per axis (0 = the classic pin-to-
    /// zero; a non-zero value is an enforced motion). `([true;3],[0;3])` behaves
    /// like Fixed (penalty form).
    Displacement([bool; 3], [f64; 3]),
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
    /// Bearing load (N): a pin pushing the wall of a CYLINDRICAL bore. The
    /// selection is fitted to a cylinder; the radial part of the force vector is
    /// spread over the loaded half with a projected-area cosine law (peak where
    /// the surface normal opposes the push, zero at ±90°). The axial component
    /// is rejected upstream (Ansys-Mechanical behaviour), so only the radial
    /// projection is applied. Resultant equals the (radial) input force.
    Bearing([f64; 3]),
    /// Moment (N·mm) on a surface. The voxel hex elements have no rotational
    /// DOFs and there are no remote points/MPC, so the moment is realised as a
    /// DEFORMABLE distributed force couple over the attached nodes, equivalent
    /// to the moment vector about the selection's area-weighted centroid:
    /// `fᵢ = wᵢ (G⁻¹ M) × dᵢ` with `G = Σ wᵢ(|dᵢ|²I − dᵢdᵢᵀ)`. This makes
    /// `Σ dᵢ×fᵢ = M` exactly with zero net force, mesh-independently.
    Moment([f64; 3]),
    /// Remote point mass (DESIGN §16): a component of mass `mass` (tonne) bolted
    /// to the selected patch, its centre of gravity at the remote `point` (mm).
    ///
    /// **Deformable** (`rigid = false`, the default): load-only. Under the active
    /// body acceleration `a` (see [`BodyLoad`]) it loads the patch with the
    /// statically-equivalent force `F = m·a` PLUS the transported couple
    /// `M = (p − c) × F` about the patch area-weighted centroid `c` (force by area
    /// weight, couple by the `moment_forces` machinery); it adds NO stiffness.
    ///
    /// **Rigid** (`rigid = true`, milestone 4): the mount also STIFFENS the patch,
    /// tying it to a 6-DOF virtual master at `point` (see [`crate::rigid`]). The
    /// coupling enters the solver as a penalty `RigidGroup` on the finest level;
    /// the load then distributes by the rigidity kinematics `Bᵢ G⁻¹ [m·a; 0]`
    /// instead of area weights. The stiffness is present whenever `rigid` — even
    /// with no acceleration — because a rigid boss stiffens its mount regardless
    /// of load. A degenerate patch (<3 non-collinear nodes) falls back to
    /// deformable. Zero active acceleration ⇒ zero LOAD (the stiffness stays).
    Mass { point: [f64; 3], mass: f64, rigid: bool },
}

/// Inertial body load for [`assemble`] (DESIGN §16): a world acceleration `a`
/// applied to the part's own distributed mass (self-weight) AND to every
/// [`BcKind::Mass`]. `vfrac` is the per-cell MATERIAL volume fraction
/// (occupancy × skin/infill composite; length = `grid.cell_count()`) — the same
/// field the mass readout composites, NOT the stiffness `eps` (a 20 %-infill
/// cell weighs 0.2× solid, not its stiffness fraction). A zero `accel` produces
/// no forces at all, so `Some(BodyLoad { accel: [0;3], .. })` is inert.
///
/// An EMPTY `vfrac` selects **mass-only mode**: remote-mass forces are realised
/// and solid cells still mark the RBM load flag, but the distributed self-weight
/// FORCE is skipped — for the optimizer, which recomputes self-weight from the
/// live density every SIMP iteration (DESIGN §16 dec. 4) rather than baking it
/// in once. Analysis solves always pass a real (non-empty) vfrac.
#[derive(Clone, Copy)]
pub struct BodyLoad<'a> {
    /// World acceleration, mm/s² (gravity = 9810 toward −Z).
    pub accel: [f64; 3],
    /// Material (bulk) density, tonne/mm³.
    pub density: f64,
    /// Per-cell material volume fraction (occupancy × skin/infill composite).
    /// Empty ⇒ mass-only mode (no self-weight force; see the type docs).
    pub vfrac: &'a [f32],
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
    body: Option<BodyLoad>, // inertial self-weight + remote-mass loads (DESIGN §16)
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
        // Attached boundary nodes for this selection (shared sub-BVH + bbox
        // margin). The sub-mesh/BVH are reused below for per-node normals.
        let (nodes, sub_mesh, sub_bvh) =
            attach_selection(mesh, grid, &boundary, bi, &sel, attach_d2, &node_pos)?;

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
                // Block motion along the LOCAL surface normal at each node.
                // A single averaged normal over the whole selection is wrong
                // when one frictionless support spans faces with different
                // orientations (e.g. three sides of a cube): every node would
                // be sprung along the same averaged direction (1,1,1)/√3,
                // leaving the body free to slide and rotate in the plane
                // perpendicular to it — a spurious rigid-body mode the user
                // sees as drift. Use the nearest selected triangle's normal per
                // node, so each node is constrained along the face it sits on.
                // On a single flat face this equals the old average; on a
                // curved selection it tracks the true local normal.
                let k = SPRING_FACTOR * settings.e0 * h;
                for &n in &nodes {
                    let p = node_pos(n);
                    let ti = sub_bvh.closest_triangle(p).0 as usize;
                    let normal = unit_normal(&sub_mesh.tris[ti]);
                    if normal == [0.0; 3] {
                        continue; // degenerate triangle — no usable normal
                    }
                    problem.springs.push((n, normal, k));
                    constraints.push(ConstraintDir { pos: p, dir: normal });
                }
            }
            BcKind::Displacement(axes, values) => {
                // Stiff axis springs on the selected global directions only —
                // the unselected axes stay free (roller/slider). A non-zero
                // prescribed value is enforced by an equivalent penalty force
                // k*value along that axis, so the pinned DOF settles at `value`.
                // This rides the force RHS path, so the value never invalidates
                // the cached matrix (only the constrained-axis SET does).
                let k = SPRING_FACTOR * settings.e0 * h;
                for &n in &nodes {
                    let p = node_pos(n);
                    for d in 0..3 {
                        if axes[d] {
                            let mut dir = [0f64; 3];
                            dir[d] = 1.0;
                            problem.springs.push((n, dir, k));
                            constraints.push(ConstraintDir { pos: p, dir });
                            if values[d] != 0.0 {
                                let mut f = [0f64; 3];
                                f[d] = k * values[d];
                                problem.forces.push((n, f));
                            }
                        }
                    }
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
            BcKind::Bearing(f) => {
                let fv = bearing_forces(mesh, &sel, &nodes, grid, *f, |n| node_pos(n));
                for (i, &n) in nodes.iter().enumerate() {
                    if fv[i] != [0.0; 3] {
                        problem.forces.push((n, fv[i]));
                    }
                    load_nodes.push(n);
                }
            }
            BcKind::Moment(m) => {
                let fv = moment_forces(mesh, &sel, &nodes, grid, *m, |n| node_pos(n));
                for (i, &n) in nodes.iter().enumerate() {
                    if fv[i] != [0.0; 3] {
                        problem.forces.push((n, fv[i]));
                    }
                    load_nodes.push(n);
                }
            }
            BcKind::Mass { point, mass, rigid } => {
                let a = body.map(|b| b.accel).unwrap_or([0.0; 3]);
                let active = a != [0.0; 3] && *mass != 0.0;
                // Rigid mount: try to build the condensed 6-DOF coupling first
                // (stiffness is present whenever `rigid`, even with no accel). A
                // degenerate patch (<3 non-collinear nodes ⇒ singular master
                // Gram) falls back to the deformable path below.
                let group = if *rigid {
                    let arms: Vec<[f64; 3]> = nodes
                        .iter()
                        .map(|&n| {
                            let p = node_pos(n);
                            [p[0] - point[0], p[1] - point[1], p[2] - point[2]]
                        })
                        .collect();
                    let k = RIGID_FACTOR * settings.e0 * h;
                    RigidGroup::build(nodes.clone(), arms, k)
                } else {
                    None
                };
                if let Some(g) = group {
                    // Rigid load: distribute the master force F = m·a (at the CG,
                    // no moment) by the rigidity kinematics Bᵢ G⁻¹ [F; 0]. The
                    // stiffness rides `problem.rigid`; the load is design-
                    // independent (m fixed), so it goes in `forces` as usual.
                    if active {
                        let fm = [mass * a[0], mass * a[1], mass * a[2], 0.0, 0.0, 0.0];
                        for (i, f) in g.load(fm).into_iter().enumerate() {
                            if f != [0.0; 3] {
                                problem.forces.push((nodes[i], f));
                            }
                            load_nodes.push(nodes[i]);
                        }
                    }
                    problem.rigid.push(g);
                } else {
                    // Deformable (load-only): statically-equivalent force
                    // F = m·a spread over the patch + transported couple
                    // M = (p − c) × F. Inert when no acceleration is active.
                    let fv =
                        mass_forces(mesh, &sel, &nodes, grid, *point, *mass, a, |n| node_pos(n));
                    for (i, &n) in nodes.iter().enumerate() {
                        if fv[i] != [0.0; 3] {
                            problem.forces.push((n, fv[i]));
                        }
                        if active {
                            load_nodes.push(n);
                        }
                    }
                }
            }
        }
        bc_nodes.push(nodes);
    }

    // Self-weight: inertial body force per cell, lumped to its 8 nodes and
    // scaled by the cell's MATERIAL volume fraction (occupancy × skin/infill
    // composite) so a graded-infill cell weighs its true share, not a solid
    // skin (DESIGN §16 dec. 3). Body forces also count toward the RBM check's
    // per-component load flag (dec. 11) — an unconstrained mass-bearing island
    // under acceleration is a real failure, not a free-floating speck.
    //
    // MASS-ONLY mode (`body.vfrac` empty): the optimizer recomputes the
    // design-dependent self-weight from the LIVE density every SIMP iteration
    // (dec. 4), so it assembles the density-INDEPENDENT loads (surface tractions
    // + remote masses) with an empty vfrac — no self-weight FORCE is added here,
    // but every solid cell still marks the RBM load flag so the pre-solve check
    // stays honest about a self-weight-loaded island.
    if let Some(body) = body {
        let a = body.accel;
        if a.iter().any(|&v| v != 0.0) {
            let has_vfrac = !body.vfrac.is_empty();
            // Force on a fully-dense cell, per node (÷8 lumping).
            let full = [0, 1, 2].map(|d| body.density * a[d] * h * h * h / 8.0);
            for cz in 0..grid.nz {
                for cy in 0..grid.ny {
                    for cx in 0..grid.nx {
                        let ci = (cz * grid.ny + cy) * grid.nx + cx;
                        if has_vfrac {
                            let vf = body.vfrac[ci] as f64;
                            if vf <= 0.0 {
                                continue;
                            }
                            let cell_f = [full[0] * vf, full[1] * vf, full[2] * vf];
                            for oz in 0..2 {
                                for oy in 0..2 {
                                    for ox in 0..2 {
                                        let n = ((cz + oz) * my + cy + oy) * mx + cx + ox;
                                        problem.forces.push((n as u32, cell_f));
                                    }
                                }
                            }
                        } else if grid.scale[ci] <= 0.0 {
                            continue; // mass-only mode: skip void, load solid cells
                        }
                        // One representative node marks this cell's component as
                        // loaded (the base corner; any of the 8 resolves).
                        load_nodes.push(((cz * my + cy) * mx + cx) as u32);
                    }
                }
            }
        }
    }

    Ok(Assembled { problem, bc_nodes, constraints, load_nodes })
}

/// Boundary nodes attached to one triangle selection: build a mini-BVH over just
/// the selection (distance to the SELECTION decides attachment, immune to
/// nearest-triangle ties at face borders), prune candidates to the selection
/// bbox + a 2·h margin, and keep the boundary nodes within `attach_d2` of it.
/// Shared by [`assemble`] and [`point_mass_lumping`] so a remote mass's MODAL
/// inertia footprint is EXACTLY the patch its static force loads. Returns the
/// attached nodes plus the sub-mesh/BVH (the caller reuses them for per-node
/// surface normals).
fn attach_selection(
    mesh: &TriMesh,
    grid: &VoxelGrid,
    boundary: &[u32],
    bi: usize,
    sel: &[u32],
    attach_d2: f64,
    node_pos: &impl Fn(u32) -> [f64; 3],
) -> Result<(Vec<u32>, TriMesh, WindingBvh), AttachError> {
    let sub_mesh =
        TriMesh::from_triangles(sel.iter().map(|&ti| mesh.tris[ti as usize]).collect());
    let sub_bvh = WindingBvh::build(&sub_mesh);
    let (lo, hi) = sub_mesh.bounds().unwrap();
    let margin = 2.0 * grid.h;
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
    Ok((nodes, sub_mesh, sub_bvh))
}

/// Per-node lumped mass (tonne) contributed by every [`BcKind::Mass`] in `bcs`
/// for MODAL analysis (DESIGN §16). A remote point mass adds inertia to the
/// eigenproblem `K v = λ M v` that the static force path (which only realises
/// `F = m·a`) never sees — so a heavy payload correctly drags the natural
/// frequencies down. Each mass `m` is distributed over its attachment patch by
/// the SAME area weights the static force uses: node `i` gets `m · wᵢ/Σw`, on
/// all three translational DOFs. The voxel HEX8 mesh has no rotational DOFs and
/// no rigid link to the remote CG, so the offset's **rotatory** inertia is not
/// representable; only the resultant translational mass is lumped — consistent
/// with the static distributed-force resultant, and the honest limit of a
/// translational-DOF model (documented in the theory manual). Node indices are
/// finest-grid nodes (the modal solver's DOF layout); a node may repeat across
/// mass BCs, so the caller sums. Empty when there are no mass BCs.
pub fn point_mass_lumping(mesh: &TriMesh, grid: &VoxelGrid, bcs: &[BcSpec]) -> Vec<(u32, f64)> {
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
    let mut out = Vec::new();
    for (bi, bc) in bcs.iter().enumerate() {
        let mass = match &bc.kind {
            BcKind::Mass { mass, .. } => *mass,
            _ => continue,
        };
        if mass == 0.0 || bc.tris.is_empty() {
            continue;
        }
        // Same attachment footprint the static force uses; a mass whose patch
        // grabs no nodes simply contributes nothing (no hard error here).
        let Ok((nodes, _, _)) =
            attach_selection(mesh, grid, &boundary, bi, &bc.tris, attach_d2, &node_pos)
        else {
            continue;
        };
        let w = area_weights(mesh, &bc.tris, &nodes, grid);
        let wsum: f64 = w.iter().sum();
        if wsum <= 0.0 {
            continue;
        }
        for (i, &n) in nodes.iter().enumerate() {
            out.push((n, mass * w[i] / wsum));
        }
    }
    out
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

/// Unit outward normal of a triangle; zero vector when degenerate.
fn unit_normal(t: &[f32; 9]) -> [f64; 3] {
    let av = crate::mesh::triangle_area_vector(t);
    let len = ((av[0] as f64).powi(2) + (av[1] as f64).powi(2) + (av[2] as f64).powi(2)).sqrt();
    if len > 0.0 {
        [av[0] as f64 / len, av[1] as f64 / len, av[2] as f64 / len]
    } else {
        [0.0; 3]
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

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Fit a triangle selection to a cylinder (axis + radius + cylindricity
/// residual). Public so the front end can validate a bearing-load selection and
/// read back the axis/radius; assembly uses the same fit so the two agree.
pub fn fit_selection_cylinder(mesh: &TriMesh, sel: &[u32]) -> Option<crate::cylinder::Cylinder> {
    let (pts, nrm, ws) = selection_samples(mesh, sel);
    crate::cylinder::fit(&pts, &nrm, &ws)
}

/// One sample per selected triangle: (centroid, unit normal, area). Feeds the
/// cylinder fit for bearing loads.
fn selection_samples(mesh: &TriMesh, sel: &[u32]) -> (Vec<[f64; 3]>, Vec<[f64; 3]>, Vec<f64>) {
    let mut pts = Vec::with_capacity(sel.len());
    let mut nrm = Vec::with_capacity(sel.len());
    let mut ws = Vec::with_capacity(sel.len());
    for &ti in sel {
        let t = &mesh.tris[ti as usize];
        let av = crate::mesh::triangle_area_vector(t);
        let area = ((av[0] as f64).powi(2) + (av[1] as f64).powi(2) + (av[2] as f64).powi(2)).sqrt();
        if area <= 0.0 {
            continue;
        }
        let c = [
            (t[0] + t[3] + t[6]) as f64 / 3.0,
            (t[1] + t[4] + t[7]) as f64 / 3.0,
            (t[2] + t[5] + t[8]) as f64 / 3.0,
        ];
        pts.push(c);
        nrm.push([av[0] as f64 / area, av[1] as f64 / area, av[2] as f64 / area]);
        ws.push(area);
    }
    (pts, nrm, ws)
}

/// Bearing load: fit the selection to a cylinder, then spread the radial part of
/// `f` over the loaded half with a projected-area cosine law so the resultant
/// equals the radial force. `node_pos` maps a node id to world position.
fn bearing_forces<F: Fn(u32) -> [f64; 3]>(
    mesh: &TriMesh,
    sel: &[u32],
    nodes: &[u32],
    grid: &VoxelGrid,
    f: [f64; 3],
    node_pos: F,
) -> Vec<[f64; 3]> {
    let mut out = vec![[0f64; 3]; nodes.len()];
    let (pts, nrm, ws) = selection_samples(mesh, sel);
    let cyl = match crate::cylinder::fit(&pts, &nrm, &ws) {
        Some(c) => c,
        None => return out, // not cylindrical — UI blocks this; no-op as a guard
    };
    let axis = cyl.axis;
    // Radial part of the load (the axial component is rejected in the UI).
    let f_ax = dot3(f, axis);
    let f_rad = [f[0] - f_ax * axis[0], f[1] - f_ax * axis[1], f[2] - f_ax * axis[2]];
    let fr_len = dot3(f_rad, f_rad).sqrt();
    if fr_len <= 1e-12 {
        return out;
    }
    let load_dir = [f_rad[0] / fr_len, f_rad[1] / fr_len, f_rad[2] / fr_len];

    let w = area_weights(mesh, sel, nodes, grid);
    let mut rhat = vec![[0f64; 3]; nodes.len()];
    let mut gain = vec![0f64; nodes.len()];
    let mut denom = 0f64;
    for (i, &n) in nodes.iter().enumerate() {
        let p = node_pos(n);
        let d = [p[0] - cyl.point[0], p[1] - cyl.point[1], p[2] - cyl.point[2]];
        let axial = dot3(d, axis);
        let radial = [d[0] - axial * axis[0], d[1] - axial * axis[1], d[2] - axial * axis[2]];
        let rl = dot3(radial, radial).sqrt();
        if rl < 1e-9 {
            continue;
        }
        let rh = [radial[0] / rl, radial[1] / rl, radial[2] / rl];
        let cos = dot3(rh, load_dir);
        if cos <= 0.0 {
            continue; // unloaded half
        }
        rhat[i] = rh;
        gain[i] = w[i] * cos;
        denom += w[i] * cos * cos;
    }
    if denom <= 1e-20 {
        return out;
    }
    let k = fr_len / denom;
    for i in 0..nodes.len() {
        if gain[i] > 0.0 {
            let s = k * gain[i];
            out[i] = [s * rhat[i][0], s * rhat[i][1], s * rhat[i][2]];
        }
    }
    out
}

/// Moment as a deformable distributed couple about the area-weighted centroid:
/// `fᵢ = wᵢ (G⁻¹ M) × dᵢ`, `G = Σ wᵢ(|dᵢ|²I − dᵢdᵢᵀ)`. Exact resultant moment,
/// zero net force, mesh-independent.
fn moment_forces<F: Fn(u32) -> [f64; 3]>(
    mesh: &TriMesh,
    sel: &[u32],
    nodes: &[u32],
    grid: &VoxelGrid,
    m: [f64; 3],
    node_pos: F,
) -> Vec<[f64; 3]> {
    let mut out = vec![[0f64; 3]; nodes.len()];
    if m == [0.0; 3] {
        return out;
    }
    let w = area_weights(mesh, sel, nodes, grid);
    let wsum: f64 = w.iter().sum();
    if wsum <= 0.0 {
        return out;
    }
    let mut c = [0f64; 3];
    for (i, &n) in nodes.iter().enumerate() {
        let p = node_pos(n);
        for d in 0..3 {
            c[d] += w[i] * p[d];
        }
    }
    for d in 0..3 {
        c[d] /= wsum;
    }
    let mut g = [[0f64; 3]; 3];
    let mut dd = vec![[0f64; 3]; nodes.len()];
    for (i, &n) in nodes.iter().enumerate() {
        let p = node_pos(n);
        let d = [p[0] - c[0], p[1] - c[1], p[2] - c[2]];
        dd[i] = d;
        let r2 = dot3(d, d);
        for r in 0..3 {
            for cc in 0..3 {
                g[r][cc] += w[i] * ((if r == cc { r2 } else { 0.0 }) - d[r] * d[cc]);
            }
        }
    }
    // Light Tikhonov term so a degenerate (collinear/point) selection still has
    // an invertible inertia tensor instead of blowing up.
    let tr = g[0][0] + g[1][1] + g[2][2];
    let eps = 1e-9 * tr.max(1e-12);
    for k in 0..3 {
        g[k][k] += eps;
    }
    let x = match crate::cylinder::solve3(g, m) {
        Some(x) => x,
        None => return out,
    };
    for i in 0..nodes.len() {
        let cr = cross3(x, dd[i]);
        out[i] = [w[i] * cr[0], w[i] * cr[1], w[i] * cr[2]];
    }
    out
}

/// Remote point mass as a deformable patch load (DESIGN §16): the force
/// `F = m·a` distributed by area weight PLUS the couple `M = (p − c) × F`
/// transported by [`moment_forces`], both about the patch area-weighted
/// centroid `c`. The area-weighted force split contributes zero moment about
/// `c`, so the superposition is statically equivalent to `F` acting at the
/// remote point `p` — exact resultant, zero spurious moment, mesh-independent.
/// Returns the zero field when the force vanishes.
#[allow(clippy::too_many_arguments)]
fn mass_forces<F: Fn(u32) -> [f64; 3]>(
    mesh: &TriMesh,
    sel: &[u32],
    nodes: &[u32],
    grid: &VoxelGrid,
    point: [f64; 3],
    mass: f64,
    accel: [f64; 3],
    node_pos: F,
) -> Vec<[f64; 3]> {
    let mut out = vec![[0f64; 3]; nodes.len()];
    let force = [mass * accel[0], mass * accel[1], mass * accel[2]];
    if force == [0.0; 3] {
        return out;
    }
    let w = area_weights(mesh, sel, nodes, grid);
    let wsum: f64 = w.iter().sum();
    if wsum <= 0.0 {
        return out;
    }
    // Patch area-weighted centroid — the point the couple transports about, and
    // the point the distributed force produces no net moment about.
    let mut c = [0f64; 3];
    for (i, &n) in nodes.iter().enumerate() {
        let p = node_pos(n);
        for d in 0..3 {
            c[d] += w[i] * p[d];
        }
    }
    for d in 0..3 {
        c[d] /= wsum;
    }
    // Distributed statically-equivalent force (Voronoi share of F).
    for (i, o) in out.iter_mut().enumerate() {
        let s = w[i] / wsum;
        for d in 0..3 {
            o[d] += s * force[d];
        }
    }
    // Transported couple M = (p − c) × F, realised with zero net force.
    let arm = [point[0] - c[0], point[1] - c[1], point[2] - c[2]];
    let moment = cross3(arm, force);
    let cpl = moment_forces(mesh, sel, nodes, grid, moment, &node_pos);
    for (o, m) in out.iter_mut().zip(&cpl) {
        for d in 0..3 {
            o[d] += m[d];
        }
    }
    out
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
