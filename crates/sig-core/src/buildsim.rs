// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! FDM build simulation via the **inherent-strain** method (see
//! `docs/build-sim-design.md`).
//!
//! The print's locked-in shrinkage is modelled as a per-cell **eigenstrain**
//! `ε₀`. Equilibrium is `K u = f_eigen` with `f_eigen = ∫ Bᵀ D ε₀ dV` — i.e.
//! the eigenstrain enters purely as a right-hand-side nodal force, exactly the
//! path `solve.rs` already drives for prescribed displacement. The element
//! stiffness `K`, the multigrid hierarchy and the whole solver are reused
//! unchanged; this module only assembles the eigenstrain force and the two
//! boundary states.
//!
//! Two-state spine (design §3a):
//! - **State 1 — bonded:** the bed plane (node z-layer 0) is fixed. The bed
//!   reactions are the peel driver. → [`solve_bonded`].
//! - **State 2 — released:** no bed; the part relaxes to its free warped shape.
//!   The body is unconstrained, so rigid-body motion is removed with a minimal
//!   statically-determinate 3-2-1 pin (zero added stress). → [`solve_warp`].
//!
//! MVP scope: a single, spatially uniform eigenstrain triple `[εx, εy, εz]`
//! (isotropic when all equal; transversely isotropic for the in-plane-vs-Z
//! anisotropy of §2 when `εz` differs). Per-cell orientation/density scaling,
//! the bed shell and peel-reaction extraction are follow-ups.

use crate::fem::{ke_hex, NODE_OFFSETS, NODE_SIGNS};
use crate::solve::{
    active_nodes, grid_eps, pad_for_levels, solve_cached, solve_nodes, NodeProblem, SolveSettings,
    SolveError, SolverCache, Solution,
};
use crate::voxel::VoxelGrid;

/// Penalty stiffness for a 3-2-1 pin DOF, relative to a cell's stiffness
/// (`~e0·h`). Large enough that the pinned displacement error is ~1e-6 of the
/// shrinkage, small enough not to wreck MGCG conditioning.
const PIN_REL: f64 = 1.0e6;

/// Stiffness factor for not-yet-printed "quiet" cells in sequential activation.
/// Small enough to be mechanically negligible, large enough to keep the
/// active/dormant contrast (here 1e4) from wrecking multigrid conditioning.
const QUIET_EPS: f32 = 1.0e-4;

/// Assemble the equivalent nodal force of a uniform eigenstrain `eigen`
/// (`[εx, εy, εz]`, engineering normal strains; shear-free in the MVP) over
/// every solid cell of `grid`.
///
/// For a constant eigenstrain the initial stress `σ₀ = D ε₀` is cell-constant,
/// so `f = ∫ Bᵀ σ₀ dV` integrates in closed form: the gradient integral
/// `∫ ∂N_l/∂x dV = sign_l·h²/4`, giving per local node `l`
/// `f_l = (h²/4) · [σ₀ₓ·sx, σ₀ᵧ·sy, σ₀_z·sz]` (shear-free σ₀). Each cell scales
/// by `eps[ci]` so the eigen force tracks the *same* stiffness the solver uses
/// for that cell (void cells contribute nothing).
pub fn eigen_forces(
    grid: &VoxelGrid,
    eps: &[f32],
    e0: f64,
    nu: f64,
    eigen: [f64; 3],
) -> Vec<(u32, [f64; 3])> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);

    // Cell-constant initial stress σ₀ = D·ε₀ for unit cell modulus e0.
    let lam = e0 * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
    let mu = e0 / (2.0 * (1.0 + nu));
    let tr = eigen[0] + eigen[1] + eigen[2];
    let s0 = [
        lam * tr + 2.0 * mu * eigen[0],
        lam * tr + 2.0 * mu * eigen[1],
        lam * tr + 2.0 * mu * eigen[2],
    ];
    let coeff = grid.h * grid.h / 4.0;

    let mut acc = vec![[0f64; 3]; mx * my * (nz + 1)];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                let s = eps[ci] as f64;
                if s <= 0.0 {
                    continue;
                }
                for l in 0..8 {
                    let [ox, oy, oz] = NODE_OFFSETS[l];
                    let g = ((cz + oz) * my + cy + oy) * mx + cx + ox;
                    let [sx, sy, sz] = NODE_SIGNS[l];
                    acc[g][0] += coeff * s * s0[0] * sx;
                    acc[g][1] += coeff * s * s0[1] * sy;
                    acc[g][2] += coeff * s * s0[2] * sz;
                }
            }
        }
    }

    acc.into_iter()
        .enumerate()
        .filter(|(_, f)| f[0] != 0.0 || f[1] != 0.0 || f[2] != 0.0)
        .map(|(n, f)| (n as u32, f))
        .collect()
}

#[inline]
fn node_idx(grid: &VoxelGrid, x: usize, y: usize, z: usize) -> usize {
    let (mx, my) = (grid.nx + 1, grid.ny + 1);
    (z * my + y) * mx + x
}

/// Active nodes on the bed plane (node z-layer 0) — the bonded support set
/// for State 1.
pub fn bottom_nodes(grid: &VoxelGrid) -> Vec<u32> {
    let (mx, my) = (grid.nx + 1, grid.ny + 1);
    let active = active_nodes(grid);
    let mut out = Vec::new();
    for y in 0..my {
        for x in 0..mx {
            let n = (0 * my + y) * mx + x;
            if active[n] {
                out.push(n as u32);
            }
        }
    }
    out
}

/// Add a minimal statically-determinate **3-2-1** rigid-body constraint to
/// `np` so the otherwise-free State-2 body has a unique solution with **zero
/// added stress** under a uniform eigenstrain (design §3a inertia relief).
///
/// - `p0` (a bed-corner node): all 3 DOF — removes translation.
/// - `p1` (same y,z as p0, max x): pin y,z — removes rotation about y and z.
/// - `p2` (same z as p0, max y): pin z — removes rotation about x.
///
/// The shared-coordinate choice is what makes it stress-free: under
/// `u = ε₀·(x−x₀)` the pinned DOFs are already zero, so the pins carry no
/// reaction. Implemented via stiff axis springs (the displacement-support
/// mechanism), so it never invalidates the cached hierarchy. Returns the three
/// chosen nodes for inspection/tests.
pub fn ground_rigid_body(grid: &VoxelGrid, np: &mut NodeProblem) -> Option<[u32; 3]> {
    let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
    let active = active_nodes(grid);
    let is_active = |x: usize, y: usize, z: usize| active[node_idx(grid, x, y, z)];

    // p0: first active node scanning z, then y, then x ascending (a min corner).
    let mut p0 = None;
    'find0: for z in 0..mz {
        for y in 0..my {
            for x in 0..mx {
                if is_active(x, y, z) {
                    p0 = Some((x, y, z));
                    break 'find0;
                }
            }
        }
    }
    let (x0, y0, z0) = p0?;

    // p1: same (y0,z0) row, the farthest active node in +x.
    let mut x1 = None;
    for x in (x0 + 1..mx).rev() {
        if is_active(x, y0, z0) {
            x1 = Some(x);
            break;
        }
    }
    // p2: same z0 plane, the farthest active node in +y (any x).
    let mut p2 = None;
    'find2: for y in (y0 + 1..my).rev() {
        for x in 0..mx {
            if is_active(x, y, z0) {
                p2 = Some((x, y));
                break 'find2;
            }
        }
    }
    let (x1, (x2, y2)) = (x1?, p2?);

    let kpin = grid.settings_pin_stiffness();
    let n0 = node_idx(grid, x0, y0, z0) as u32;
    let n1 = node_idx(grid, x1, y0, z0) as u32;
    let n2 = node_idx(grid, x2, y2, z0) as u32;

    np.fixed.push(n0);
    np.springs.push((n1, [0.0, 1.0, 0.0], kpin));
    np.springs.push((n1, [0.0, 0.0, 1.0], kpin));
    np.springs.push((n2, [0.0, 0.0, 1.0], kpin));
    Some([n0, n1, n2])
}

impl VoxelGrid {
    /// Pin penalty stiffness scaled to a cell's stiffness (`e0·h`), using the
    /// solver default modulus. Build-sim only.
    fn settings_pin_stiffness(&self) -> f64 {
        SolveSettings::default().e0 * self.h * PIN_REL
    }
}

/// **State 2 — released.** Apply the eigenstrain to the free body (3-2-1 pin)
/// and solve for the warped shape. The displacement field is the warp /
/// predeform target.
pub fn solve_warp(
    grid: &VoxelGrid,
    eigen: [f64; 3],
    s: &SolveSettings,
) -> Result<Solution, SolveError> {
    let (g, levels) = pad_for_levels(grid, s.max_levels);
    let eps = grid_eps(&g);
    let mut np = NodeProblem { forces: eigen_forces(&g, &eps, s.e0, s.nu, eigen), ..Default::default() };
    if ground_rigid_body(&g, &mut np).is_none() {
        return Err(SolveError::NoFixedNodes);
    }
    solve_nodes(&g, levels, &np, s)
}

/// **State 1 — bonded.** Apply the eigenstrain with the bed plane fully fixed.
/// The bonded distortion field; the bed reactions (peel) are a follow-up.
pub fn solve_bonded(
    grid: &VoxelGrid,
    eigen: [f64; 3],
    s: &SolveSettings,
) -> Result<Solution, SolveError> {
    let (g, levels) = pad_for_levels(grid, s.max_levels);
    let eps = grid_eps(&g);
    let np = NodeProblem {
        fixed: bottom_nodes(&g),
        forces: eigen_forces(&g, &eps, s.e0, s.nu, eigen),
        ..Default::default()
    };
    solve_nodes(&g, levels, &np, s)
}

/// The sequential bonded build loop (State 1). Returns the padded grid, level
/// count, the bonded displacement, the final accumulated load terms `f_eig` and
/// `f_lock` (needed for peel reactions + the release solve), and MGCG
/// iterations/layer. See [`solve_sequential_bonded`] / [`solve_build`].
fn build_bonded_inner(
    grid: &VoxelGrid,
    eigen: [f64; 3],
    s: &SolveSettings,
    on_layer: &mut dyn FnMut(usize, usize, &Solution),
) -> Result<(VoxelGrid, usize, Vec<f64>, Vec<f64>, Vec<f64>, Vec<usize>), SolveError> {
    let (g, levels) = pad_for_levels(grid, s.max_levels);
    let (nx, ny, nz) = (g.nx, g.ny, g.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let ndof = 3 * mx * my * mz;
    let eps_full = grid_eps(&g);
    let ke = ke_hex(s.e0, s.nu, g.h); // reference element stiffness (for E=e0)

    // Quiet base: every solid cell present at near-zero stiffness; void = 0. The
    // void PATTERN is then constant across layers, so the multigrid hierarchy is
    // reused (cheap `update_eps`) instead of rebuilt each step.
    let mut eps_cur: Vec<f32> =
        eps_full.iter().map(|&e| if e > 0.0 { QUIET_EPS } else { 0.0 }).collect();

    let fixed = bottom_nodes(&g);
    let mut u_total = vec![0f64; ndof];
    let mut active_node = vec![false; mx * my * mz];
    for &n in &fixed {
        active_node[n as usize] = true;
    }

    // Cumulative right-hand side terms (grow as layers are born, never reset):
    // f_eig = total applied eigenstrain force; f_lock = Σ Kₑ·u_birth.
    let mut f_eig = vec![0f64; ndof];
    let mut f_lock = vec![0f64; ndof];

    let mut slot: Option<SolverCache> = None;
    let mut iters = Vec::new();

    // Total SOLID layers, for progress reporting (empty layers are skipped).
    let total_layers = (0..nz)
        .filter(|&k| (0..ny * nx).any(|c| eps_full[k * ny * nx + c] > 0.0))
        .count();
    let mut layers_done = 0usize;

    for k in 0..nz {
        // Mark this layer's nodes active. New top nodes stay at NOMINAL (their
        // u_total is 0 — dormant nodes were zeroed below), which is the whole
        // point: the printer lays the layer down at design coordinates.
        let mut any = false;
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (k * ny + cy) * nx + cx;
                if eps_full[ci] <= 0.0 {
                    continue;
                }
                any = true;
                for l in 0..8 {
                    let [ox, oy, oz] = NODE_OFFSETS[l];
                    let n = ((k + oz) * my + cy + oy) * mx + cx + ox;
                    active_node[n] = true;
                }
            }
        }
        if !any {
            continue;
        }

        // Born strain-free in the current config: lock each new cell's reference
        // (f_lock += eps·Kₑ·u_birth, with fresh top nodes at 0) and add its own
        // eigenstrain force. Done once, at birth.
        let mut eps_layer = vec![0f32; g.cell_count()];
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (k * ny + cy) * nx + cx;
                let epsc = eps_full[ci];
                if epsc <= 0.0 {
                    continue;
                }
                eps_cur[ci] = epsc;
                eps_layer[ci] = epsc;

                let mut n8 = [0usize; 8];
                let mut u24 = [0f64; 24];
                for l in 0..8 {
                    let [ox, oy, oz] = NODE_OFFSETS[l];
                    let n = ((k + oz) * my + cy + oy) * mx + cx + ox;
                    n8[l] = n;
                    for d in 0..3 {
                        u24[3 * l + d] = u_total[3 * n + d];
                    }
                }
                for i in 0..24 {
                    let mut fi = 0.0;
                    for j in 0..24 {
                        fi += ke[i][j] * u24[j];
                    }
                    f_lock[3 * n8[i / 3] + (i % 3)] += epsc as f64 * fi;
                }
            }
        }
        for (n, fv) in eigen_forces(&g, &eps_layer, s.e0, s.nu, eigen) {
            for d in 0..3 {
                f_eig[3 * n as usize + d] += fv[d];
            }
        }

        // Total equilibrium of the active structure: K u = f_eig + f_lock.
        let forces: Vec<(u32, [f64; 3])> = (0..mx * my * mz)
            .filter_map(|n| {
                let f = [f_eig[3 * n] + f_lock[3 * n], f_eig[3 * n + 1] + f_lock[3 * n + 1], f_eig[3 * n + 2] + f_lock[3 * n + 2]];
                (f[0] != 0.0 || f[1] != 0.0 || f[2] != 0.0).then_some((n as u32, f))
            })
            .collect();
        let problem = NodeProblem { fixed: fixed.clone(), forces, ..Default::default() };
        let res =
            solve_cached(&mut slot, &g, levels, &problem, s, eps_cur.clone(), s.tol, s.max_iter)?;
        u_total = res.u;
        // Keep dormant nodes at nominal so the next layer is truly born at 0.
        for n in 0..mx * my * mz {
            if !active_node[n] {
                for d in 0..3 {
                    u_total[3 * n + d] = 0.0;
                }
            }
        }
        iters.push(res.stats.iterations);

        // Report progress + the warp accumulated so far (live preview). The
        // solution carries the BUILD activation mask (active_node), NOT the
        // geometric one, so the preview shows only already-printed cells.
        layers_done += 1;
        let max_it = iters.iter().copied().max().unwrap_or(0);
        let sol = Solution {
            u: u_total.iter().map(|&v| v as f32).collect(),
            mx,
            my,
            mz,
            h: g.h,
            origin: g.origin,
            active: active_node.clone(),
            iterations: max_it,
            rel_residual: 0.0,
            converged: true,
            residuals: Vec::new(),
        };
        on_layer(layers_done, total_layers, &sol);
    }

    Ok((g, levels, u_total, f_eig, f_lock, iters))
}

/// Wrap a padded-grid displacement vector as a [`Solution`].
fn solution_from(g: &VoxelGrid, u: Vec<f64>, iterations: usize) -> Solution {
    let (mx, my, mz) = (g.nx + 1, g.ny + 1, g.nz + 1);
    Solution {
        u: u.iter().map(|&v| v as f32).collect(),
        mx,
        my,
        mz,
        h: g.h,
        origin: g.origin,
        active: active_nodes(g),
        iterations,
        rel_residual: 0.0,
        converged: true,
        residuals: Vec::new(),
    }
}

/// Matrix-free `K·u` over all solid cells (for nodal reaction recovery).
fn apply_k(g: &VoxelGrid, eps: &[f32], ke: &[[f64; 24]; 24], u: &[f64]) -> Vec<f64> {
    let (nx, ny, nz) = (g.nx, g.ny, g.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let mut ku = vec![0f64; 3 * mx * my * mz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let e = eps[(cz * ny + cy) * nx + cx] as f64;
                if e <= 0.0 {
                    continue;
                }
                let mut n8 = [0usize; 8];
                let mut ul = [0f64; 24];
                for l in 0..8 {
                    let [ox, oy, oz] = NODE_OFFSETS[l];
                    let n = ((cz + oz) * my + cy + oy) * mx + cx + ox;
                    n8[l] = n;
                    for d in 0..3 {
                        ul[3 * l + d] = u[3 * n + d];
                    }
                }
                for i in 0..24 {
                    let mut sgi = 0.0;
                    for j in 0..24 {
                        sgi += ke[i][j] * ul[j];
                    }
                    ku[3 * n8[i / 3] + (i % 3)] += e * sgi;
                }
            }
        }
    }
    ku
}

/// **State 1 — sequential bonded build** (layer-by-layer inherent strain, §3a).
///
/// Activates voxel-Z-layers bottom-up using the standard AM element-activation
/// rule (Michaleris 2014; PMC inherent-strain variant): each new layer's cells
/// are added **strain-free at their nominal mesh coordinates** — *not* on the
/// deformed substrate — and only **that layer's** eigenstrain is applied; the
/// whole active structure then re-equilibrates. New top nodes start at nominal,
/// so the last layer adds only its own one-layer strain and barely moves.
///
/// For a pure-elastic uniform eigenstrain the build-order effect is **mild** (it
/// shows only where geometry couples); the large sequential effects in real AM
/// come from plasticity + bed release, not modelled here. Returns the bonded
/// field and MGCG iterations/layer.
pub fn solve_sequential_bonded(
    grid: &VoxelGrid,
    eigen: [f64; 3],
    s: &SolveSettings,
) -> Result<(Solution, Vec<usize>), SolveError> {
    let (g, _levels, u, _fe, _fl, iters) = build_bonded_inner(grid, eigen, s, &mut |_, _, _| {})?;
    let it = *iters.iter().max().unwrap_or(&0);
    Ok((solution_from(&g, u, it), iters))
}

/// Full two-state build result (§3a).
pub struct BuildResult {
    /// State 1 — distortion while bonded to the bed.
    pub bonded: Solution,
    /// State 2 — free warped shape after release from the bed (predeform target).
    pub released: Solution,
    /// Per bed-plane node: the reaction the bed exerts to hold the part (N).
    /// `+Z` = the part pulling up at that point — the peel / plate-release driver
    /// (option B, per-voxel localised). XY components are the bed shear.
    pub bed_reaction: Vec<(u32, [f64; 3])>,
    /// MGCG iterations per build layer (quiet-element conditioning watch).
    pub iters: Vec<usize>,
}

/// **Full build sim**: State 1 (sequential bonded build) → bed peel reactions →
/// State 2 (release to the free warped shape). The release reuses the final
/// accumulated `f_eig + f_lock` from the build, swapping the bonded bed for a
/// minimal 3-2-1 rigid-body pin (the part is now free).
pub fn solve_build(
    grid: &VoxelGrid,
    eigen: [f64; 3],
    s: &SolveSettings,
) -> Result<BuildResult, SolveError> {
    solve_build_progress(grid, eigen, s, |_, _, _| {})
}

/// Like [`solve_build`], but `on_layer(layers_done, total_layers, &bonded_so_far)`
/// is invoked after each activated layer — for a live progress bar + warp
/// preview. The bonded build is also where cancellation lands (a stopped solve
/// propagates `SolveError::Cancelled` out).
pub fn solve_build_progress(
    grid: &VoxelGrid,
    eigen: [f64; 3],
    s: &SolveSettings,
    mut on_layer: impl FnMut(usize, usize, &Solution),
) -> Result<BuildResult, SolveError> {
    let (g, levels, u_b, f_eig, f_lock, iters) =
        build_bonded_inner(grid, eigen, s, &mut on_layer)?;
    let it = *iters.iter().max().unwrap_or(&0);
    let eps_full = grid_eps(&g);
    let ke = ke_hex(s.e0, s.nu, g.h);

    // Peel: bed reaction R = K·u_bonded − (f_eig + f_lock), at the held bed nodes.
    let ku = apply_k(&g, &eps_full, &ke, &u_b);
    let bed_reaction: Vec<(u32, [f64; 3])> = bottom_nodes(&g)
        .into_iter()
        .map(|n| {
            let i = n as usize;
            let r = [
                ku[3 * i] - (f_eig[3 * i] + f_lock[3 * i]),
                ku[3 * i + 1] - (f_eig[3 * i + 1] + f_lock[3 * i + 1]),
                ku[3 * i + 2] - (f_eig[3 * i + 2] + f_lock[3 * i + 2]),
            ];
            (n, r)
        })
        .collect();

    // Release: free body (3-2-1 pin) under the locked-in build loads.
    let nnodes = (g.nx + 1) * (g.ny + 1) * (g.nz + 1);
    let forces: Vec<(u32, [f64; 3])> = (0..nnodes)
        .filter_map(|n| {
            let f = [f_eig[3 * n] + f_lock[3 * n], f_eig[3 * n + 1] + f_lock[3 * n + 1], f_eig[3 * n + 2] + f_lock[3 * n + 2]];
            (f[0] != 0.0 || f[1] != 0.0 || f[2] != 0.0).then_some((n as u32, f))
        })
        .collect();
    let mut np = NodeProblem { forces, ..Default::default() };
    if ground_rigid_body(&g, &mut np).is_none() {
        return Err(SolveError::NoFixedNodes);
    }
    let released = solve_nodes(&g, levels, &np, s)?;

    Ok(BuildResult {
        bonded: solution_from(&g, u_b, it),
        released,
        bed_reaction,
        iters,
    })
}

/// H-shaped test fixture as a voxel grid (no meshing): two vertical legs in X
/// joined by a horizontal cross-bar at mid-height. Build direction is Z. The
/// cross-bar is the qualitative target: under sequential activation the legs
/// shrink independently below it and get coupled at its height, leaving a
/// distinct "shrink line" there (a single-shot solve scales uniformly and
/// shows no line — that contrast is the point).
///
/// `leg_w` = leg width in cells; the cross-bar spans `[cross_z0, cross_z0+cross_h)`
/// in Z and bridges the gap between the legs. Full depth in Y.
pub fn h_grid(
    nx: usize,
    ny: usize,
    nz: usize,
    h: f64,
    leg_w: usize,
    cross_z0: usize,
    cross_h: usize,
) -> VoxelGrid {
    let mut scale = vec![0f32; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let in_leg = cx < leg_w || cx >= nx - leg_w;
                let in_cross = cz >= cross_z0 && cz < cross_z0 + cross_h;
                if in_leg || in_cross {
                    scale[(cz * ny + cy) * nx + cx] = 1.0;
                }
            }
        }
    }
    VoxelGrid { nx, ny, nz, h, origin: [0.0; 3], scale }
}

/// Apply a (exaggerated) displacement field to the solid hull and return a
/// binary STL — a viewable warp artifact for any slicer/CAD. `exaggeration`
/// scales the displacement so sub-millimetre warp is visible.
pub fn deformed_hull_stl(grid: &VoxelGrid, sol: &Solution, exaggeration: f64) -> Vec<u8> {
    let (tris, _edges) = grid.surface_mesh();
    let mut out: Vec<[f32; 9]> = Vec::with_capacity(tris.len() / 9);
    for t in tris.chunks_exact(9) {
        let mut v = [0f32; 9];
        for k in 0..3 {
            let p = [t[3 * k] as f64, t[3 * k + 1] as f64, t[3 * k + 2] as f64];
            let u = sol.sample_displacement(p);
            for d in 0..3 {
                v[3 * k + d] = (p[d] + exaggeration * u[d]) as f32;
            }
        }
        out.push(v);
    }
    crate::mesh::TriMesh::from_triangles(out).to_stl_binary()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A free body under a uniform isotropic eigenstrain ε must shrink rigidly
    /// about the pin with ZERO stress: `u = ε·(x − x_pin)`. This is the
    /// closed-form hand-calc the design note asks for as milestone 1.
    #[test]
    fn free_isotropic_shrink_matches_hand_calc() {
        let (nx, ny, nz, h) = (24usize, 4usize, 4usize, 1.0f64);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let beta = -0.01; // 1% isotropic shrink
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };

        let sol = solve_warp(&grid, [beta, beta, beta], &s).expect("solve");

        // Pin is the (0,0,0) corner node → u there is ~0, everything else
        // displaces by beta * position relative to it.
        let pin = [grid.origin[0], grid.origin[1], grid.origin[2]];
        let mut worst = 0f64;
        for z in 0..=nz {
            for y in 0..=ny {
                for x in 0..=nx {
                    let p = [
                        grid.origin[0] + x as f64 * h,
                        grid.origin[1] + y as f64 * h,
                        grid.origin[2] + z as f64 * h,
                    ];
                    let expect =
                        [beta * (p[0] - pin[0]), beta * (p[1] - pin[1]), beta * (p[2] - pin[2])];
                    let got = sol.sample_displacement(p);
                    for d in 0..3 {
                        worst = worst.max((got[d] - expect[d]).abs());
                    }
                }
            }
        }
        // Tolerance: shrink at the far corner is |beta|*L = 0.24 mm; require the
        // field to match the rigid-shrink solution to well under 1%.
        assert!(worst < 2.0e-3, "free shrink deviates from hand calc by {worst:.2e} mm");
    }

    /// Bonding the bed must change the answer: the plate nodes are held while
    /// the body above still shrinks, so the field is no longer the stress-free
    /// rigid shrink (bottom pinned, top pulled in/down).
    #[test]
    fn bonded_differs_from_free() {
        let (nx, ny, nz, h) = (24usize, 4usize, 4usize, 1.0f64);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let beta = -0.01;
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };

        let bonded = solve_bonded(&grid, [beta, beta, beta], &s).expect("bonded");

        // Bed plane (z index 0) is held ~fixed.
        let bed = bonded.sample_displacement([12.0, 2.0, grid.origin[2]]);
        let bed_mag = (bed[0] * bed[0] + bed[1] * bed[1] + bed[2] * bed[2]).sqrt();
        assert!(bed_mag < 1.0e-3, "bonded bed should be held, got {bed_mag:.2e} mm");

        // Somewhere above the bed the part has moved (shrinkage fought by the bed).
        let top = bonded.sample_displacement([12.0, 2.0, grid.origin[2] + nz as f64 * h]);
        let top_mag = (top[0] * top[0] + top[1] * top[1] + top[2] * top[2]).sqrt();
        assert!(top_mag > 1.0e-3, "bonded top should move, got {top_mag:.2e} mm");
    }

    /// On the H, a bonded shrink must pull both legs inward (toward the centre):
    /// the left leg moves +x, the right leg −x. Qualitative signature only —
    /// the distinct cross-bar shrink line needs sequential activation.
    #[test]
    fn h_legs_shrink_inward() {
        let (nx, ny, nz, h) = (40usize, 8usize, 60usize, 1.0f64);
        let grid = h_grid(nx, ny, nz, h, 8, 26, 8);
        let beta = -0.01;
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };
        let sol = solve_bonded(&grid, [beta, beta, beta], &s).expect("bonded");

        // Sample near the top of each leg (free end, largest lateral pull).
        let zt = grid.origin[2] + (nz as f64 - 2.0) * h;
        let left = sol.sample_displacement([2.0, ny as f64 * 0.5, zt]);
        let right = sol.sample_displacement([nx as f64 - 2.0, ny as f64 * 0.5, zt]);
        assert!(left[0] > 1.0e-4, "left leg should move +x (inward), got {:.2e}", left[0]);
        assert!(right[0] < -1.0e-4, "right leg should move -x (inward), got {:.2e}", right[0]);
    }

    /// Correctness gate for element activation (the bug Stefan caught): on a
    /// straight symmetric column shrinking onto a bonded base, each layer is laid
    /// down at nominal coordinates and contracts only by its own strain. So the
    /// lateral narrowing must be ~UNIFORM with height — the freshly-printed top
    /// must NOT be the most-distorted point — and the sequential result must
    /// closely track single-shot (build order barely matters for this symmetric,
    /// pure-elastic case). The earlier "inherit the deformed substrate" bug
    /// violated both (top spiked to ~5x single-shot).
    #[test]
    fn sequential_column_top_not_overdistorted() {
        let (nx, ny, nz, h) = (10usize, 10usize, 48usize, 1.0f64);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let beta = -0.01;
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };

        let ss = solve_bonded(&grid, [beta, beta, beta], &s).expect("single-shot");
        let (sq, _it) = solve_sequential_bonded(&grid, [beta, beta, beta], &s).expect("seq");

        // Inward pull at the +x surface (shrinks toward the axis => -x).
        let surf = nx as f64 - 0.5;
        let ym = ny as f64 * 0.5;
        let ux = |sol: &Solution, z: f64| sol.sample_displacement([surf, ym, z])[0];

        // Narrowing is roughly uniform: the top is not dramatically more pulled
        // in than mid-height. (Allow a modest factor for the free-end / bonded
        // boundary layers, but nothing like the 5x spike of the bug.)
        let (mid, top) = (ux(&sq, nz as f64 * 0.5), ux(&sq, nz as f64 - 1.0));
        assert!(mid < 0.0 && top < 0.0, "column should pull inward (-x): mid {mid:.4} top {top:.4}");
        assert!(
            top.abs() < 1.8 * mid.abs(),
            "top must not be far more distorted than mid: top {top:.4} vs mid {mid:.4}"
        );

        // Sequential tracks single-shot for this symmetric, pure-elastic part.
        let (ss_top, sq_top) = (ux(&ss, nz as f64 - 1.0), top);
        assert!(
            (sq_top - ss_top).abs() < 0.5 * ss_top.abs().max(1e-9),
            "sequential should track single-shot here: seq {sq_top:.4} vs ss {ss_top:.4}"
        );
    }

    /// State 2 (release) + peel: releasing the bed yields a valid free warped
    /// shape that differs from the bonded one, and the bed reactions are a
    /// self-equilibrated field (net force ~0 — peel is about LOCAL lift, not a
    /// net resultant) with a meaningful peak.
    #[test]
    fn build_release_and_peel() {
        let grid = h_grid(40, 8, 60, 1.0, 8, 26, 8);
        let beta = -0.005;
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };
        let r = solve_build(&grid, [beta, beta, beta], &s).expect("build");

        // Released free shape is valid and not identical to bonded.
        let (db, dr) = (r.bonded.max_displacement(), r.released.max_displacement());
        assert!(dr.is_finite() && dr > 1.0e-4, "released should warp: {dr:.4}");
        assert!((dr - db).abs() > 1.0e-4, "release should change the shape: bonded {db:.4} released {dr:.4}");

        // Bed reactions: self-equilibrated (net ~0) but with a real peak.
        assert!(!r.bed_reaction.is_empty());
        let mut net = [0.0f64; 3];
        let mut scale = 0.0;
        let mut peak_z = 0.0f64;
        for (_, rv) in &r.bed_reaction {
            for d in 0..3 {
                net[d] += rv[d];
            }
            scale += rv[2].abs();
            peak_z = peak_z.max(rv[2].abs());
        }
        assert!(peak_z > 1.0e-3, "expected a meaningful peel reaction, peak {peak_z:.2e}");
        assert!(
            net[2].abs() < 0.02 * scale + 1.0e-6,
            "bed reaction should be self-equilibrated in Z: net {:.2e} vs scale {:.2e}",
            net[2],
            scale
        );
    }
}
