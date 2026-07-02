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

use crate::eps::resolve_eps;
use crate::fem::{ke_hex, NODE_OFFSETS, NODE_SIGNS};
use crate::par::{self, UnsafeSlice};
use crate::solve::{
    active_nodes, grid_eps, pad_for_levels, solve_nodes, NodeProblem, SolveSettings, SolveError,
    SolveSession, Solution,
};
use crate::voxel::VoxelGrid;

/// Temperature ladder (design review A1): splits the total locking strain
/// between the bonded build and the post-release cooldown.
///
/// Material locks at `t_lock` (Tg for amorphous, crystallization temperature
/// for semi-crystalline) and, while the part is on the bed, cools only to its
/// LOCAL in-build steady temperature — near the plate that is the bed
/// temperature, a few millimetres up it is the chamber temperature (heat
/// penetration of the bed's influence is ~3 mm in PLA, T4F3/KU Leuven 2022).
/// The remainder of the shrink happens after removal, when the whole part
/// cools to `t_final`. Splitting the strain this way is what makes bed and
/// chamber temperature real inputs: a PLA part on a 60 °C bed locks almost
/// nothing near the plate while bonded (T_bed ≈ Tg) — most of its near-bed
/// shrink arrives only after release.
#[derive(Clone, Copy, Debug)]
pub struct ThermalLadder {
    /// Locking temperature (°C): Tg (amorphous) or Tc (semi-crystalline).
    pub t_lock: f64,
    /// Bed temperature (°C) during the build.
    pub t_bed: f64,
    /// Chamber/ambient temperature (°C) during the build.
    pub t_env: f64,
    /// Ambient temperature (°C) after removal (room temp).
    pub t_final: f64,
    /// Height scale (mm) over which the bed's influence decays to chamber.
    pub decay_mm: f64,
}

impl ThermalLadder {
    /// In-build steady temperature at height `z_mm` above the bed.
    fn t_steady(&self, z_mm: f64) -> f64 {
        self.t_env + (self.t_bed - self.t_env) * (-z_mm / self.decay_mm.max(1e-6)).exp()
    }

    /// Fraction of the TOTAL (t_lock → t_final) strain that locks in during
    /// the bonded build at height `z_mm`; the remainder is applied in the
    /// release solve. Clamped to [0, 1].
    pub fn build_fraction(&self, z_mm: f64) -> f64 {
        let total = self.t_lock - self.t_final;
        if total.abs() < 1e-9 {
            return 1.0;
        }
        ((self.t_lock - self.t_steady(z_mm)) / total).clamp(0.0, 1.0)
    }

    /// Per-cell-layer build fractions for a padded grid (cell centres).
    fn layer_fractions(&self, g: &VoxelGrid) -> Vec<f64> {
        (0..g.nz).map(|k| self.build_fraction((k as f64 + 0.5) * g.h)).collect()
    }
}

/// Penalty stiffness for a 3-2-1 pin DOF, relative to a cell's stiffness
/// (`~e0·h`). Large enough that the pinned displacement error is negligible
/// (~1e-4 of the shrinkage), small enough that the rigid-body penalty doesn't
/// wreck MGCG conditioning on the free-body release solve.
const PIN_REL: f64 = 1.0e4;

/// Stiffness factor for not-yet-printed "quiet" cells in sequential activation.
/// Small enough to be mechanically negligible, large enough to keep the
/// active/dormant contrast (here 1e4) from wrecking multigrid conditioning.
const QUIET_EPS: f32 = 1.0e-4;

/// Max passes of the plastic (radial-return) fixed-point correction (§ yield).
/// The initial-strain method uses the *elastic* operator (so the cached
/// hierarchy is reused), which converges linearly — capped here for cost.
const MAX_PLASTIC_ITERS: usize = 12;

/// Convergence tol for the plastic loop: stop once the largest plastic-strain
/// increment in a pass drops below this fraction of the eigenstrain magnitude.
const PLASTIC_TOL: f64 = 1.0e-2;

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

    let acc = eigen_forces_dense(grid, eps, [coeff * s0[0], coeff * s0[1], coeff * s0[2]], None);
    let _ = (nx, ny, nz, mx, my);
    acc.chunks_exact(3)
        .enumerate()
        .filter(|(_, f)| f[0] != 0.0 || f[1] != 0.0 || f[2] != 0.0)
        .map(|(n, f)| (n as u32, [f[0], f[1], f[2]]))
        .collect()
}

/// Dense eigen-force assembly (3 per node): per cell `f_l = eps·scale(z)·[fx·sx,
/// fy·sy, fz·sz]` scattered to its 8 nodes, where `f = coeff·σ₀` is precomputed
/// by the caller. `layer_scale` (per cell-Z-layer) is the temperature-ladder
/// hook. Parallel: two phases of alternating cell layers (same-phase layers
/// never share nodes — the mg.rs slab argument with 1-layer slabs).
fn eigen_forces_dense(
    grid: &VoxelGrid,
    eps: &[f32],
    f0: [f64; 3],
    layer_scale: Option<&[f64]>,
) -> Vec<f64> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let mut acc = vec![0f64; 3 * mx * my * (nz + 1)];
    let ys = UnsafeSlice::new(&mut acc);
    for phase in 0..2 {
        let layers: Vec<usize> = (phase..nz).step_by(2).collect();
        par::for_each(&layers, |&cz| {
            let ls = layer_scale.map_or(1.0, |s| s[cz]);
            if ls == 0.0 {
                return;
            }
            for cy in 0..ny {
                for cx in 0..nx {
                    let ci = (cz * ny + cy) * nx + cx;
                    let s = eps[ci] as f64 * ls;
                    if s <= 0.0 {
                        continue;
                    }
                    for l in 0..8 {
                        let [ox, oy, oz] = NODE_OFFSETS[l];
                        let g = ((cz + oz) * my + cy + oy) * mx + cx + ox;
                        let [sx, sy, sz] = NODE_SIGNS[l];
                        // SAFETY: same-phase layers are ≥2 apart in z.
                        unsafe {
                            *ys.get_mut(3 * g) += s * f0[0] * sx;
                            *ys.get_mut(3 * g + 1) += s * f0[1] * sy;
                            *ys.get_mut(3 * g + 2) += s * f0[2] * sz;
                        }
                    }
                }
            }
        });
    }
    drop(ys);
    acc
}

/// Cell-constant initial stress `σ₀ = D ε₀` scaled by the closed-form gradient
/// integral (`h²/4`) — the per-node force triple `eigen_forces_dense` scatters.
fn eigen_f0(grid: &VoxelGrid, e0: f64, nu: f64, eigen: [f64; 3]) -> [f64; 3] {
    let lam = e0 * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
    let mu = e0 / (2.0 * (1.0 + nu));
    let tr = eigen[0] + eigen[1] + eigen[2];
    let coeff = grid.h * grid.h / 4.0;
    [
        coeff * (lam * tr + 2.0 * mu * eigen[0]),
        coeff * (lam * tr + 2.0 * mu * eigen[1]),
        coeff * (lam * tr + 2.0 * mu * eigen[2]),
    ]
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
    eps_override: Option<&[f32]>,
    ladder: Option<&ThermalLadder>,
    on_layer: &mut dyn FnMut(usize, usize, &Solution),
) -> Result<(VoxelGrid, usize, Vec<f64>, Vec<f64>, Vec<f64>, Vec<usize>), SolveError> {
    let (g, levels) = pad_for_levels(grid, s.max_levels);
    let (nx, ny, nz) = (g.nx, g.ny, g.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let ndof = 3 * mx * my * mz;
    let eps_full = resolve_eps(&g, eps_override);
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

    let mut ses = SolveSession::new();
    let problem = NodeProblem { fixed: fixed.clone(), ..Default::default() };
    let mut iters = Vec::new();

    // Temperature ladder: fraction of the total strain locked while bonded,
    // per cell layer (1.0 everywhere when no ladder — legacy behavior).
    let frac: Vec<f64> = match ladder {
        Some(l) => l.layer_fractions(&g),
        None => vec![1.0; nz],
    };

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
        // This layer's eigenstrain, scaled by the ladder's bonded fraction at
        // its height (the cooldown remainder is applied in the release solve).
        let eigen_k = [eigen[0] * frac[k], eigen[1] * frac[k], eigen[2] * frac[k]];
        for (n, fv) in eigen_forces(&g, &eps_layer, s.e0, s.nu, eigen_k) {
            for d in 0..3 {
                f_eig[3 * n as usize + d] += fv[d];
            }
        }

        // Total equilibrium of the active structure: K u = f_eig + f_lock.
        let res = ses.solve(
            &g,
            levels,
            &problem,
            s,
            eps_cur.clone(),
            &[&f_eig, &f_lock],
            s.tol,
            s.max_iter,
        )?;
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
/// Parallel: two phases of alternating cell layers (no shared nodes in-phase).
fn apply_k(g: &VoxelGrid, eps: &[f32], ke: &[[f64; 24]; 24], u: &[f64]) -> Vec<f64> {
    let (nx, ny, nz) = (g.nx, g.ny, g.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let mut ku = vec![0f64; 3 * mx * my * mz];
    let ys = UnsafeSlice::new(&mut ku);
    for phase in 0..2 {
        let layers: Vec<usize> = (phase..nz).step_by(2).collect();
        par::for_each(&layers, |&cz| {
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
                        // SAFETY: same-phase layers are ≥2 apart in z.
                        unsafe { *ys.get_mut(3 * n8[i / 3] + (i % 3)) += e * sgi };
                    }
                }
            }
        });
    }
    drop(ys);
    ku
}

/// Engineering normal+shear strain `[εxx, εyy, εzz, γxy, γyz, γzx]` at the
/// centre of cell `(cx,cy,cz)` from the padded nodal field `u` (mirror of
/// `stress::cell_field_eigen`'s strain evaluation).
fn cell_strain(g: &VoxelGrid, u: &[f64], cx: usize, cy: usize, cz: usize) -> [f64; 6] {
    let (mx, my) = (g.nx + 1, g.ny + 1);
    let inv4h = 1.0 / (4.0 * g.h);
    let mut e = [0f64; 6];
    for l in 0..8 {
        let [ox, oy, oz] = NODE_OFFSETS[l];
        let [sx, sy, sz] = NODE_SIGNS[l];
        let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
        let (ux, uy, uz) = (u[3 * n], u[3 * n + 1], u[3 * n + 2]);
        e[0] += sx * ux;
        e[1] += sy * uy;
        e[2] += sz * uz;
        e[3] += sy * ux + sx * uy;
        e[4] += sz * uy + sy * uz;
        e[5] += sx * uz + sz * ux;
    }
    for v in &mut e {
        *v *= inv4h;
    }
    e
}

/// Isotropic stress `σ = D : ε` (engineering strain in, `[σxx,σyy,σzz,σxy,σyz,σzx]`
/// out) for cell modulus `e` and Poisson `nu`.
fn stress_from_strain(eng: [f64; 6], e: f64, nu: f64) -> [f64; 6] {
    let lam = e * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
    let mu = e / (2.0 * (1.0 + nu));
    let tr = eng[0] + eng[1] + eng[2];
    [
        lam * tr + 2.0 * mu * eng[0],
        lam * tr + 2.0 * mu * eng[1],
        lam * tr + 2.0 * mu * eng[2],
        mu * eng[3],
        mu * eng[4],
        mu * eng[5],
    ]
}

/// J2 (von Mises) radial return for **perfect plasticity**. Given a cell's
/// trial stress `sig` (`[σxx,σyy,σzz,σxy,σyz,σzx]`), shear modulus `mu` and
/// yield `sy`, return the increment of **plastic strain** (engineering comps)
/// that brings the stress back onto the yield surface, or `None` if still
/// elastic. Pure J2 is pressure-insensitive, so a perfectly-constrained
/// *isotropic* shrink (hydrostatic) never yields — the warp source is the
/// deviatoric stress the bed/anisotropy create near the plate.
fn return_map(sig: [f64; 6], mu: f64, sy: f64) -> Option<[f64; 6]> {
    let p = (sig[0] + sig[1] + sig[2]) / 3.0;
    let s = [sig[0] - p, sig[1] - p, sig[2] - p, sig[3], sig[4], sig[5]];
    let sds =
        s[0] * s[0] + s[1] * s[1] + s[2] * s[2] + 2.0 * (s[3] * s[3] + s[4] * s[4] + s[5] * s[5]);
    let vm = (1.5 * sds).sqrt();
    if vm <= sy || vm <= 1.0e-12 {
        return None;
    }
    let dlam = (vm - sy) / (3.0 * mu); // perfect plasticity (no hardening)
    // Δεᵖ (tensor) = Δλ·(3/2)·s/σvm; engineering shear = 2× the tensor shear.
    let c = 1.5 * dlam / vm;
    Some([c * s[0], c * s[1], c * s[2], 2.0 * c * s[3], 2.0 * c * s[4], 2.0 * c * s[5]])
}

/// Equivalent nodal force of a per-cell **plastic eigenstrain** `ep`
/// (engineering comps), accumulated into `f`. Same `f = ∫ Bᵀ(D εᵖ) dV` path as
/// [`eigen_forces`] but carrying the shear components, so it can be added to
/// `f_eig + f_lock` on the RHS exactly like the inherent strain.
fn add_plastic_forces(g: &VoxelGrid, eps: &[f32], e0: f64, nu: f64, ep: &[[f64; 6]], f: &mut [f64]) {
    let (nx, ny, nz) = (g.nx, g.ny, g.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let coeff = g.h * g.h / 4.0;
    let ys = UnsafeSlice::new(f);
    for phase in 0..2 {
        let layers: Vec<usize> = (phase..nz).step_by(2).collect();
        par::for_each(&layers, |&cz| {
            for cy in 0..ny {
                for cx in 0..nx {
                    let ci = (cz * ny + cy) * nx + cx;
                    let e = eps[ci] as f64;
                    if e <= 0.0 {
                        continue;
                    }
                    let sp = stress_from_strain(ep[ci], e0 * e, nu);
                    for l in 0..8 {
                        let [ox, oy, oz] = NODE_OFFSETS[l];
                        let [sx, sy, sz] = NODE_SIGNS[l];
                        let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
                        // SAFETY: same-phase layers are ≥2 apart in z.
                        unsafe {
                            *ys.get_mut(3 * n) += coeff * (sp[0] * sx + sp[3] * sy + sp[5] * sz);
                            *ys.get_mut(3 * n + 1) +=
                                coeff * (sp[1] * sy + sp[3] * sx + sp[4] * sz);
                            *ys.get_mut(3 * n + 2) +=
                                coeff * (sp[2] * sz + sp[4] * sy + sp[5] * sx);
                        }
                    }
                }
            }
        });
    }
}

/// Elastic–perfectly-plastic correction on the **bonded** state (§ yield, the
/// physical fix for infill-blind warp). The free released shrink of a *uniform*
/// eigenstrain is the stress-free compatible field `u = ε₀·x`, independent of
/// the stiffness/density distribution — so a pure-elastic release warps the same
/// for 0 % and 100 % infill. Plasticity breaks that: while bonded to the plate
/// the part is constrained, the deviatoric stress near the bed exceeds yield,
/// and the locked-in **incompatible** plastic strain `εᵖ` does *not* relax on
/// release → density-dependent curl.
///
/// Solved as the classic initial-strain (modified-Newton) fixed point on the
/// *final* bonded state: radial-return each cell against `sy`, accumulate `εᵖ`,
/// re-solve `K u = f_eig + f_lock + f_plastic(εᵖ)` (bed fixed, RHS-only so the
/// cached hierarchy is never invalidated), repeat. Yield is compared on the
/// MACRO (homogenized) stress vs a single material `sy`; per-density strength
/// homogenization is a follow-up. Returns the updated bonded field, locked `εᵖ`,
/// and the assembled plastic force.
#[allow(clippy::too_many_arguments)]
fn plastic_correct_bonded(
    g: &VoxelGrid,
    levels: usize,
    s: &SolveSettings,
    eps_full: &[f32],
    fixed: &[u32],
    eigen: [f64; 3],
    frac: &[f64],
    f_eig: &[f64],
    f_lock: &[f64],
    sy: f64,
    u_b: Vec<f64>,
) -> Result<(Vec<f64>, Vec<[f64; 6]>, Vec<f64>), SolveError> {
    let (nx, ny, nz) = (g.nx, g.ny, g.nz);
    let ndof = f_eig.len();
    let mut u_b = u_b;
    let mut ep = vec![[0f64; 6]; g.cell_count()];
    let mut f_plastic = vec![0f64; ndof];
    let eigen_mag =
        1.0e-9_f64.max(eigen[0].abs().max(eigen[1].abs()).max(eigen[2].abs()));
    let mut ses = SolveSession::new();
    let problem = NodeProblem { fixed: fixed.to_vec(), ..Default::default() };

    for _it in 0..MAX_PLASTIC_ITERS {
        // Radial-return every cell against the current bonded stress, growing
        // the locked-in plastic strain. Parallel over cell layers: each cell
        // writes only its own ep entry; the reduction is the max increment.
        let max_dep = {
            let ep_s = UnsafeSlice::new(&mut ep);
            let u_ref = &u_b;
            par::map_reduce_ranges(
                nz,
                1,
                |z0, z1| {
                    let mut local = 0f64;
                    for cz in z0..z1 {
                        for cy in 0..ny {
                            for cx in 0..nx {
                                let ci = (cz * ny + cy) * nx + cx;
                                let ec = eps_full[ci] as f64;
                                if ec <= 0.0 {
                                    continue;
                                }
                                let e = s.e0 * ec;
                                let mu = e / (2.0 * (1.0 + s.nu));
                                let tot = cell_strain(g, u_ref, cx, cy, cz);
                                // SAFETY: each cell index is visited exactly once.
                                let epc = unsafe { ep_s.get_mut(ci) };
                                // Elastic strain = total − (bonded-stage)
                                // eigenstrain − locked plastic strain.
                                let el = [
                                    tot[0] - frac[cz] * eigen[0] - epc[0],
                                    tot[1] - frac[cz] * eigen[1] - epc[1],
                                    tot[2] - frac[cz] * eigen[2] - epc[2],
                                    tot[3] - epc[3],
                                    tot[4] - epc[4],
                                    tot[5] - epc[5],
                                ];
                                let sig = stress_from_strain(el, e, s.nu);
                                if let Some(dep) = return_map(sig, mu, sy) {
                                    for k in 0..6 {
                                        epc[k] += dep[k];
                                        local = local.max(dep[k].abs());
                                    }
                                }
                            }
                        }
                    }
                    local
                },
                f64::max,
                || 0.0,
            )
        };
        if max_dep == 0.0 {
            break;
        }

        // Re-equilibrate the bonded structure with the updated plastic load.
        f_plastic.iter_mut().for_each(|v| *v = 0.0);
        add_plastic_forces(g, eps_full, s.e0, s.nu, &ep, &mut f_plastic);
        // Intermediate iterates feed the next return-map, not the final field, so
        // a relaxed tol + iteration cap keeps each pass cheap (warm-started from
        // the previous pass via the session). The final release runs its own solve.
        let res = ses.solve(
            g,
            levels,
            &problem,
            s,
            eps_full.to_vec(),
            &[f_eig, f_lock, &f_plastic],
            s.tol.max(2.0e-3),
            s.max_iter.min(400),
        )?;
        u_b = res.u;

        if max_dep < PLASTIC_TOL * eigen_mag {
            break;
        }
    }
    Ok((u_b, ep, f_plastic))
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
    let (g, _levels, u, _fe, _fl, iters) =
        build_bonded_inner(grid, eigen, s, None, None, &mut |_, _, _| {})?;
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
///
/// `yield_strength` (MPa): when `Some`, an elastic–perfectly-plastic correction
/// runs on the bonded state so the released warp depends on geometry/density
/// (see [`plastic_correct_bonded`]). `None` = the pure-elastic model, whose free
/// release is the density-independent compatible shrink.
pub fn solve_build(
    grid: &VoxelGrid,
    eigen: [f64; 3],
    s: &SolveSettings,
    yield_strength: Option<f64>,
) -> Result<BuildResult, SolveError> {
    solve_build_progress(grid, eigen, s, None, yield_strength, None, |_, _, _| {})
}

/// Like [`solve_build`], but `on_layer(layers_done, total_layers, &bonded_so_far)`
/// is invoked after each activated layer — for a live progress bar + warp
/// preview. The bonded build is also where cancellation lands (a stopped solve
/// propagates `SolveError::Cancelled` out).
///
/// `eps_override` lets the caller drive the build sim with the **as-printed
/// infill density** (the optimizer's stiffness field) instead of the solid
/// hull; see [`resolve_eps`]. `None` = solid hull. `yield_strength` enables the
/// plastic correction (see [`solve_build`]).
pub fn solve_build_progress(
    grid: &VoxelGrid,
    eigen: [f64; 3],
    s: &SolveSettings,
    eps_override: Option<&[f32]>,
    yield_strength: Option<f64>,
    ladder: Option<&ThermalLadder>,
    mut on_layer: impl FnMut(usize, usize, &Solution),
) -> Result<BuildResult, SolveError> {
    let (g, levels, u_b, f_eig, mut f_lock, iters) =
        build_bonded_inner(grid, eigen, s, eps_override, ladder, &mut on_layer)?;
    let it = *iters.iter().max().unwrap_or(&0);
    let eps_full = resolve_eps(&g, eps_override);
    let ke = ke_hex(s.e0, s.nu, g.h);
    let frac: Vec<f64> = match ladder {
        Some(l) => l.layer_fractions(&g),
        None => vec![1.0; g.nz],
    };

    // Plastic correction (§ yield): lock in the incompatible plastic strain the
    // bonded constraint generates, so the released warp stops being the
    // density-blind stress-free shrink. Folded into `f_lock` as an extra
    // inelastic source so peel + release see it identically to the elastic terms.
    let u_b = match yield_strength {
        Some(sy) if sy > 0.0 => {
            let bottom = bottom_nodes(&g);
            let (u_p, _ep, f_plastic) = plastic_correct_bonded(
                &g, levels, s, &eps_full, &bottom, eigen, &frac, &f_eig, &f_lock, sy, u_b,
            )?;
            for (l, fp) in f_lock.iter_mut().zip(&f_plastic) {
                *l += *fp;
            }
            u_p
        }
        _ => u_b,
    };

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

    // Cooldown remainder (temperature ladder): after removal the whole part
    // cools from its in-build steady temperature to ambient — every cell gets
    // the (1 − bonded fraction) share of its eigenstrain, applied only in the
    // release state. Without a ladder this is identically zero.
    let cool_scale: Vec<f64> = frac.iter().map(|&f| 1.0 - f).collect();
    let f_cool = if cool_scale.iter().any(|&c| c > 0.0) {
        eigen_forces_dense(&g, &eps_full, eigen_f0(&g, s.e0, s.nu, eigen), Some(&cool_scale))
    } else {
        vec![0f64; f_eig.len()]
    };

    // Release: free body (3-2-1 pin) under the locked-in build loads (now
    // including the plastic source folded into f_lock above) plus the
    // post-release cooldown strain.
    let mut np = NodeProblem::default();
    if ground_rigid_body(&g, &mut np).is_none() {
        return Err(SolveError::NoFixedNodes);
    }
    // The release is a free body (only the 3-2-1 pin), so its operator is
    // near-singular in the rigid-body modes and MGCG converges slowly. The warp
    // SHAPE is fine well before machine tolerance, so relax it and cap the
    // iterations rather than grinding to the global cap.
    let s_release = SolveSettings { tol: s.tol.max(2.0e-3), max_iter: s.max_iter.min(600), ..*s };
    let released = SolveSession::new()
        .solve(
            &g,
            levels,
            &np,
            &s_release,
            eps_full.clone(),
            &[&f_eig, &f_lock, &f_cool],
            s_release.tol,
            s_release.max_iter,
        )?
        .into_solution(&g);

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

    /// Residual stress must subtract the eigenstrain: a freely-shrunk body is
    /// stress-FREE (`ε(u) = ε₀` everywhere), so the residual σzz (eigen
    /// subtracted) is ~0, while the naive `C:ε(u)` (eigen = 0) reads large. (von
    /// Mises can't show this — an isotropic shrink is purely hydrostatic, so its
    /// von Mises is zero either way; a normal component is the right probe.)
    #[test]
    fn residual_stress_of_free_shrink_is_zero() {
        use crate::stress::{cell_field_eigen, FieldKind};
        let (nx, ny, nz, h) = (16usize, 8usize, 8usize, 1.0f64);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let beta = -0.01;
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };

        let (g, _lv) = pad_for_levels(&grid, s.max_levels);
        let eps = grid_eps(&g);
        let sol = solve_warp(&grid, [beta, beta, beta], &s).expect("warp");

        let resid =
            cell_field_eigen(&g, &sol.u, s.e0, s.nu, &eps, [beta, beta, beta], FieldKind::Szz);
        let plain = cell_field_eigen(&g, &sol.u, s.e0, s.nu, &eps, [0.0; 3], FieldKind::Szz);
        let mx = |v: &[f32]| v.iter().copied().fold(0f32, |a, b| a.max(b.abs()));
        let (rmax, pmax) = (mx(&resid), mx(&plain));
        assert!(pmax > 1.0, "naive σzz should be large, got {pmax:.3} MPa");
        assert!(
            rmax < 0.02 * pmax,
            "residual σzz of a free shrink must be ~0: {rmax:.4} vs naive {pmax:.4} MPa"
        );
    }

    /// Feeding the as-printed infill density (a graded `eps_override`) must
    /// change the BONDED warp versus the solid hull: a soft sparse core is more
    /// compliant, so the constrained shrink redistributes. (The free RELEASED
    /// shape is the compatible uniform shrink `u = ε₀·x`, stress-free and hence
    /// stiffness-independent — so the override only bites where the part is
    /// constrained, which is exactly the bonded state and the bed peel.) This is
    /// the end-to-end check that the override actually reaches the assembly.
    #[test]
    fn density_field_changes_bonded_warp() {
        let (nx, ny, nz, h) = (16usize, 8usize, 24usize, 1.0f64);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let beta = -0.01;
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };

        // Solid-hull baseline (override = None).
        let base = solve_build(&grid, [beta, beta, beta], &s, None).expect("base");

        // As-printed: a soft 20%-density core inside a full-density skin, built on
        // the PADDED grid so the override length matches the assembly.
        let (g, _lv) = pad_for_levels(&grid, s.max_levels);
        let mut eps = grid_eps(&g);
        for cz in 0..g.nz {
            for cy in 0..g.ny {
                for cx in 0..g.nx {
                    let ci = (cz * g.ny + cy) * g.nx + cx;
                    if eps[ci] <= 0.0 {
                        continue;
                    }
                    let core = cx >= 2 && cx + 2 < nx && cy >= 1 && cy + 1 < ny;
                    if core {
                        eps[ci] = 0.2;
                    }
                }
            }
        }
        let graded =
            solve_build_progress(&grid, [beta, beta, beta], &s, Some(&eps), None, None, |_, _, _| {})
                .expect("graded");

        let (db, dg) = (base.bonded.max_displacement(), graded.bonded.max_displacement());
        assert!(
            (db - dg).abs() > 1.0e-3 * db.max(1e-9),
            "infill density should change the bonded warp: solid {db:.4} vs graded {dg:.4}"
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
        let r = solve_build(&grid, [beta, beta, beta], &s, None).expect("build");

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

    /// Largest nodal displacement difference between two solutions on the same
    /// grid, over nodes active in BOTH — a "how much did the warp change" metric
    /// that cancels the common uniform shrink and isolates the curl.
    fn max_node_diff(a: &Solution, b: &Solution) -> f64 {
        let mut worst = 0f64;
        for n in 0..a.node_count().min(b.node_count()) {
            if !a.active[n] || !b.active[n] {
                continue;
            }
            for d in 0..3 {
                worst = worst.max((a.u[3 * n + d] as f64 - b.u[3 * n + d] as f64).abs());
            }
        }
        worst
    }

    /// Temperature ladder invariants. (a) For a PURE-ELASTIC uniform shrink the
    /// released shape must be ladder-independent: build-stage + cooldown-stage
    /// strains sum to the same total, and the elastic release of a compatible
    /// field forgets the path. (b) With the bed at the locking temperature the
    /// near-bed layers lock almost nothing while bonded, so the bonded bed
    /// tractions (peel) must DROP versus the no-ladder run.
    #[test]
    fn ladder_elastic_release_invariant_and_peel_reduction() {
        let (nx, ny, nz, h) = (16usize, 6usize, 20usize, 1.0f64);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let eigen = [-0.008, -0.008, -0.004];
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };
        // PLA-like: bed at Tg (locks ~nothing near the plate), cold chamber.
        let ladder = ThermalLadder {
            t_lock: 60.0,
            t_bed: 60.0,
            t_env: 25.0,
            t_final: 20.0,
            decay_mm: 3.0,
        };

        let base = solve_build(&grid, eigen, &s, None).expect("base");
        let lad = solve_build_progress(&grid, eigen, &s, None, None, Some(&ladder), |_, _, _| {})
            .expect("ladder");

        // (a) Same TOTAL strain either way, so the released warp magnitude must
        // land in the same band. (Not identical: the ladder changes each
        // layer's birth configuration, so f_lock — and hence the release — is
        // legitimately path-dependent. That path shift is the physics: with a
        // hot bed the warp happens after removal, not on the plate.)
        let (db, dl) = (base.released.max_displacement(), lad.released.max_displacement());
        assert!(
            dl > 0.7 * db && dl < 1.4 * db,
            "ladder release must stay in the same magnitude band: {dl:.4} vs {db:.4}"
        );

        // (b) Bonded peel drops: near-bed layers hold back most of their strain.
        let peak = |r: &BuildResult| {
            r.bed_reaction.iter().map(|(_, f)| f[2].abs()).fold(0.0f64, f64::max)
        };
        let (p0, p1) = (peak(&base), peak(&lad));
        assert!(
            p1 < 0.6 * p0,
            "bed-at-Tg ladder must cut peel substantially: {p1:.3} vs {p0:.3} N"
        );
    }

    /// Ladder fraction sanity: bed at t_lock → fraction ≈ small near the bed,
    /// rising toward the chamber value with height; everything clamped [0,1].
    #[test]
    fn ladder_fractions_monotone() {
        let l = ThermalLadder { t_lock: 60.0, t_bed: 60.0, t_env: 25.0, t_final: 20.0, decay_mm: 3.0 };
        let f0 = l.build_fraction(0.0);
        let f5 = l.build_fraction(5.0);
        let f20 = l.build_fraction(20.0);
        assert!(f0 < 0.05, "at the bed nothing locks while bonded: {f0}");
        assert!(f0 < f5 && f5 < f20, "fraction must rise with height: {f0} {f5} {f20}");
        assert!(f20 <= 1.0 && f20 > 0.8, "far from the bed most strain locks in-build: {f20}");
    }

    /// The fix for Stefan's infill bug: a pure-elastic release of a uniform
    /// eigenstrain is the stress-free compatible shrink `u = ε₀·x`, so it is
    /// stiffness-independent and warps the same regardless of infill. Turning on
    /// a yield stress locks in incompatible plastic strain near the bonded bed,
    /// which does NOT relax on release → the released warp genuinely changes.
    #[test]
    fn plasticity_changes_released_warp() {
        let (nx, ny, nz, h) = (12usize, 4usize, 16usize, 1.0f64);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        // Transversely isotropic shrink (Z = half XY) → a deviatoric source the
        // bed constraint can drive past yield (pure isotropic + full constraint
        // is hydrostatic, which J2 never yields).
        let eigen = [-0.01, -0.01, -0.005];
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };

        let elastic = solve_build(&grid, eigen, &s, None).expect("elastic");
        // Yield well below the constrained stress (~E·β ≈ 24 MPa) so it bites.
        let plastic = solve_build(&grid, eigen, &s, Some(8.0)).expect("plastic");

        let d = max_node_diff(&elastic.released, &plastic.released);
        let scale = elastic.released.max_displacement();
        assert!(
            d > 0.02 * scale,
            "plastic release must differ from elastic: diff {d:.4} vs scale {scale:.4}"
        );
    }

    /// With plasticity on, the locked-in warp source scales with how much
    /// constrained material there is: a solid part curls more than the same part
    /// with a soft, sparse infill core. (Elastically the two release IDENTICALLY,
    /// which is exactly the bug — so the test also asserts the elastic gap is
    /// negligible next to the plastic one.)
    #[test]
    fn plastic_warp_scales_with_density() {
        let (nx, ny, nz, h) = (12usize, 4usize, 16usize, 1.0f64);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let eigen = [-0.01, -0.01, -0.005];
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };
        let sy = Some(8.0);

        // Soft 15%-density core inside a full-density skin (padded grid).
        let (g, _lv) = pad_for_levels(&grid, s.max_levels);
        let mut eps = grid_eps(&g);
        for cz in 0..g.nz {
            for cy in 0..g.ny {
                for cx in 0..g.nx {
                    let ci = (cz * g.ny + cy) * g.nx + cx;
                    if eps[ci] > 0.0 && cx >= 2 && cx + 2 < nx && cy >= 1 && cy + 1 < ny {
                        eps[ci] = 0.15;
                    }
                }
            }
        }

        let solid = solve_build(&grid, eigen, &s, sy).expect("solid");
        let hollow = solve_build_progress(&grid, eigen, &s, Some(&eps), sy, None, |_, _, _| {})
            .expect("hollow");
        let solid_e = solve_build(&grid, eigen, &s, None).expect("solid elastic");
        let hollow_e = solve_build_progress(&grid, eigen, &s, Some(&eps), None, None, |_, _, _| {})
            .expect("hollow elastic");

        // Plastic curl = how far each released shape departs from its elastic
        // (stress-free) counterpart. Solid locks in more than the sparse core.
        let curl_solid = max_node_diff(&solid.released, &solid_e.released);
        let curl_hollow = max_node_diff(&hollow.released, &hollow_e.released);
        // Band recalibrated after the release solve switched to the CONSISTENT
        // graded operator (eps_full — the same stiffness the loads were
        // assembled with; it previously released against the solid-hull
        // stiffness, which broke the compatible-shrink property for graded
        // parts). The density contrast in plastic curl is real but modest.
        assert!(
            curl_solid > curl_hollow * 1.05,
            "denser part should curl more: solid {curl_solid:.4} vs hollow {curl_hollow:.4}"
        );

        // The elastic releases barely differ (the bug): density is invisible
        // without plasticity.
        let elastic_gap = max_node_diff(&solid_e.released, &hollow_e.released);
        assert!(
            elastic_gap < curl_solid,
            "elastic release should be ~density-blind: gap {elastic_gap:.4} vs plastic curl {curl_solid:.4}"
        );
    }
}
