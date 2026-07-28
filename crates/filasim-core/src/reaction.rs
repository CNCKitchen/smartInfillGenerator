// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Support reaction recovery: the force (and moment) each support exerts on
//! the part, computed from a solved displacement field.
//!
//! Two enforcement mechanisms, two exact recoveries:
//! - **Dirichlet-eliminated** supports ([`BcKind::Fixed`]): nodal equilibrium
//!   of the unconstrained operator, `Rₙ = (K·u)ₙ − fₙ` — the same recovery the
//!   build sim uses for its bed reaction (`buildsim::apply_k`), restricted to
//!   the cells adjacent to the support's nodes. `K` is the element blend the
//!   solve actually ran (isotropic solid share + optional TI infill share,
//!   DESIGN §22) — recovering with a different material would report a
//!   reaction the displacement field never produced.
//! - **Penalty-spring** supports (Frictionless / Displacement / Cylindrical /
//!   Elastic): the spring force itself, `Rₙ = −k·(uₙ·d̂)·d̂` per spring, plus
//!   the BC's own RHS forces (a Displacement support's prescribed value rides
//!   the RHS as `k·value` — see `attach.rs`), so a non-zero prescribed motion
//!   reports `−k·(uₙ·d̂ − value)·d̂` exactly. No `K·u` pass needed.
//!
//! Sign convention: the returned force is the force the support exerts ON the
//! part (a part pushed down reports an upward reaction). Summed over all
//! supports it balances the applied loads: `Σ R = −Σ F_applied`.

use crate::attach::{Assembled, BcKind, BcSpec};
use crate::fem::{ke_hex, ke_hex_c, NODE_OFFSETS};
use crate::solve::SolveSettings;
use crate::ti::TiRatios;
use crate::voxel::VoxelGrid;
use std::collections::HashMap;

/// Resultant reaction of one support BC.
#[derive(Clone, Debug)]
pub struct BcReaction {
    /// Total force the support exerts on the part (N).
    pub force: [f64; 3],
    /// Resultant moment about `centroid` (N·mm).
    pub moment: [f64; 3],
    /// Mean position of the attached nodes (mm) — the natural arrow anchor.
    pub centroid: [f64; 3],
    /// Attached node count (diagnostic).
    pub nodes: usize,
}

/// Is this BC kind a support (reports a reaction)? Loads are inputs, not
/// reactions; a rigid Mass mount constrains but prescribes nothing, so its
/// "reaction" is internal load transfer, not a support force.
pub fn is_support(kind: &BcKind) -> bool {
    matches!(
        kind,
        BcKind::Fixed
            | BcKind::Frictionless
            | BcKind::Displacement(_, _)
            | BcKind::Cylindrical(_)
            | BcKind::Elastic(_)
    )
}

/// Per-support reaction recovery. One entry per input BC, `None` for loads.
///
/// `u` is the padded nodal displacement field the solve produced (must match
/// `grid`'s node dimensions), `eps_solid` the per-cell SOLID stiffness share
/// the solve used (`grid.scale` for a plain solid solve), and `ti` the infill
/// weights + tensor ratios when the solve ran anisotropic (DESIGN §22) —
/// `None` otherwise. `asm` must be assembled from the same `bcs` on the same
/// grid with the same settings (spring stiffnesses must match the solve).
pub fn support_reactions(
    grid: &VoxelGrid,
    bcs: &[BcSpec],
    asm: &Assembled,
    u: &[f32],
    eps_solid: &[f32],
    ti: Option<(&[f32], &TiRatios)>,
    settings: &SolveSettings,
) -> Vec<Option<BcReaction>> {
    let h = grid.h;
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    debug_assert_eq!(u.len(), 3 * mx * my * (nz + 1));
    debug_assert_eq!(eps_solid.len(), grid.cell_count());
    let node_pos = |n: u32| -> [f64; 3] {
        let n = n as usize;
        [
            grid.origin[0] + (n % mx) as f64 * h,
            grid.origin[1] + ((n / mx) % my) as f64 * h,
            grid.origin[2] + (n / (mx * my)) as f64 * h,
        ]
    };

    // Element matrices of the solve's material blend (see module docs).
    let ke_iso = ke_hex(settings.e0, settings.nu, h);
    let ke_ti = ti.map(|(_, ratios)| {
        // Mirror of mg::TiOverlay::new — c_unit is normalized to Ep = 1 and
        // scaled by e0 here; the per-cell weight carries rel(ρ).
        let mut c = ratios.stiffness();
        for row in c.iter_mut() {
            for v in row.iter_mut() {
                *v *= settings.e0;
            }
        }
        ke_hex_c(&c, [h; 3])
    });
    let ti_eps = ti.map(|(e, _)| e);

    // A node on a shared edge/corner attaches to every adjacent BC, so two
    // Fixed supports can both claim it. Its (single) nodal reaction is split
    // evenly between the claimants — otherwise it would be double-counted and
    // Σ R would stop balancing the applied loads.
    let mut fixed_claims: HashMap<u32, u32> = HashMap::new();
    for (bc, nodes) in bcs.iter().zip(&asm.bc_nodes) {
        if matches!(bc.kind, BcKind::Fixed) {
            for &n in nodes {
                *fixed_claims.entry(n).or_insert(0) += 1;
            }
        }
    }

    // Applied nodal forces at the fixed-support nodes (all sources: surface
    // loads sharing an edge node, self-weight of adjacent cells, …).
    let mut fixed_f: HashMap<u32, [f64; 3]> = fixed_claims.keys().map(|&n| (n, [0.0; 3])).collect();
    if !fixed_f.is_empty() {
        for &(n, f) in &asm.problem.forces {
            if let Some(acc) = fixed_f.get_mut(&n) {
                for d in 0..3 {
                    acc[d] += f[d];
                }
            }
        }
    }

    // (K·u)ₙ for one node: rows 3l..3l+3 of each adjacent solid cell's element
    // matrix. Exact — the sum over a node's ≤8 incident cells is exactly the
    // full matrix-free K·u restricted to that node.
    let ku_at = |n: u32| -> [f64; 3] {
        let ni = n as usize;
        let (x, y, z) = (ni % mx, (ni / mx) % my, ni / (mx * my));
        let mut out = [0.0f64; 3];
        for l in 0..8 {
            let [ox, oy, oz] = NODE_OFFSETS[l];
            // The cell whose local node `l` is this node.
            let (Some(cx), Some(cy), Some(cz)) =
                (x.checked_sub(ox), y.checked_sub(oy), z.checked_sub(oz))
            else {
                continue;
            };
            if cx >= nx || cy >= ny || cz >= nz {
                continue;
            }
            let ci = (cz * ny + cy) * nx + cx;
            let es = eps_solid[ci] as f64;
            let ei = ti_eps.map_or(0.0, |t| t[ci] as f64);
            if es <= 0.0 && ei <= 0.0 {
                continue;
            }
            let mut ul = [0.0f64; 24];
            for (m, off) in NODE_OFFSETS.iter().enumerate() {
                let nn = ((cz + off[2]) * my + cy + off[1]) * mx + cx + off[0];
                for d in 0..3 {
                    ul[3 * m + d] = u[3 * nn + d] as f64;
                }
            }
            for d in 0..3 {
                let row = 3 * l + d;
                let mut s = 0.0;
                for j in 0..24 {
                    let mut k = es * ke_iso[row][j];
                    if let Some(kt) = &ke_ti {
                        k += ei * kt[row][j];
                    }
                    s += k * ul[j];
                }
                out[d] += s;
            }
        }
        out
    };

    bcs.iter()
        .enumerate()
        .map(|(bi, bc)| {
            if !is_support(&bc.kind) {
                return None;
            }
            let nodes = &asm.bc_nodes[bi];
            // Per-node reaction — kept per node so the moment resultant about
            // the centroid comes out of the same data.
            let mut r: HashMap<u32, [f64; 3]> = HashMap::new();
            match bc.kind {
                BcKind::Fixed => {
                    for &n in nodes {
                        let ku = ku_at(n);
                        let f = fixed_f.get(&n).copied().unwrap_or([0.0; 3]);
                        let share = 1.0 / fixed_claims.get(&n).copied().unwrap_or(1) as f64;
                        let acc = r.entry(n).or_insert([0.0; 3]);
                        for d in 0..3 {
                            acc[d] += (ku[d] - f[d]) * share;
                        }
                    }
                }
                _ => {
                    for &(n, dir, k) in &asm.problem.springs[asm.bc_springs[bi].clone()] {
                        let ni = 3 * n as usize;
                        let un = [u[ni] as f64, u[ni + 1] as f64, u[ni + 2] as f64];
                        let s = -k * (un[0] * dir[0] + un[1] * dir[1] + un[2] * dir[2]);
                        let acc = r.entry(n).or_insert([0.0; 3]);
                        for d in 0..3 {
                            acc[d] += s * dir[d];
                        }
                    }
                    // A Displacement support's prescribed value is a k·value
                    // RHS force — part of the spring force, so part of R.
                    for &(n, f) in &asm.problem.forces[asm.bc_forces[bi].clone()] {
                        let acc = r.entry(n).or_insert([0.0; 3]);
                        for d in 0..3 {
                            acc[d] += f[d];
                        }
                    }
                }
            }
            let mut c = [0.0f64; 3];
            for &n in nodes {
                let p = node_pos(n);
                for d in 0..3 {
                    c[d] += p[d];
                }
            }
            for d in c.iter_mut() {
                *d /= nodes.len().max(1) as f64;
            }
            let mut force = [0.0f64; 3];
            let mut moment = [0.0f64; 3];
            for (&n, rv) in &r {
                let p = node_pos(n);
                let arm = [p[0] - c[0], p[1] - c[1], p[2] - c[2]];
                for d in 0..3 {
                    force[d] += rv[d];
                }
                moment[0] += arm[1] * rv[2] - arm[2] * rv[1];
                moment[1] += arm[2] * rv[0] - arm[0] * rv[2];
                moment[2] += arm[0] * rv[1] - arm[1] * rv[0];
            }
            Some(BcReaction { force, moment, centroid: c, nodes: nodes.len() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::{assemble, BcSpec};
    use crate::mesh::primitives;
    use crate::solve::{pad_for_levels, solve_nodes};
    use crate::voxel::VoxelGrid;

    const SEGS: usize = 48;

    /// Solid ⌀20×20 cylinder about Z + (bottom-cap tris, top-cap tris).
    fn fixture() -> (crate::mesh::TriMesh, Vec<u32>, Vec<u32>) {
        let mesh = primitives::cylinder([0.0, 0.0, 0.0], 10.0, 20.0, SEGS);
        let bottom: Vec<u32> = (2 * SEGS as u32..3 * SEGS as u32).collect();
        let top: Vec<u32> = (3 * SEGS as u32..4 * SEGS as u32).collect();
        (mesh, bottom, top)
    }

    fn solve_and_react(bcs: Vec<BcSpec>) -> Vec<Option<BcReaction>> {
        let (mesh, _, _) = fixture();
        let settings = SolveSettings { tol: 1e-8, ..Default::default() };
        let grid0 = VoxelGrid::voxelize(&mesh, 1.0);
        let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
        let asm = assemble(&mesh, &grid, &bcs, None, &settings).unwrap();
        let sol = solve_nodes(&grid, levels, &asm.problem, &settings).unwrap();
        support_reactions(&grid, &bcs, &asm, &sol.u, &grid.scale, None, &settings)
    }

    /// Global equilibrium: the fixed support's reaction balances the applied
    /// load exactly, and the reaction moment about the support centroid equals
    /// the transported load moment.
    #[test]
    fn fixed_reaction_balances_the_applied_load() {
        let (_, bottom, top) = fixture();
        let f = [30.0, 0.0, -50.0];
        let r = solve_and_react(vec![
            BcSpec { kind: BcKind::Fixed, tris: bottom },
            BcSpec { kind: BcKind::Force(f), tris: top },
        ]);
        assert!(r[1].is_none(), "a load must not report a reaction");
        let re = r[0].as_ref().expect("fixed support reaction");
        for d in 0..3 {
            assert!(
                (re.force[d] + f[d]).abs() < 1e-3 * 60.0,
                "Σ R != -F on axis {d}: {:?} vs {f:?}",
                re.force
            );
        }
        // Load acts at the top cap (z = 20), support centroid near z = 0:
        // M = -(p_load - c) × F ⇒ M_y ≈ -20 * Fx. Voxel attach smearing makes
        // this approximate — 10 % is discretization, not recovery error.
        assert!(
            (re.moment[1] + 20.0 * f[0]).abs() < 0.1 * (20.0 * f[0]).abs(),
            "reaction moment M_y = {} (expected ≈ {})",
            re.moment[1],
            -20.0 * f[0]
        );
    }

    /// Penalty-spring recovery: a three-axis Displacement support (springs, not
    /// elimination) balances the load to penalty accuracy, and the two
    /// mechanisms agree with each other.
    #[test]
    fn spring_reaction_balances_the_applied_load() {
        let (_, bottom, top) = fixture();
        let f = [30.0, 0.0, -50.0];
        let r = solve_and_react(vec![
            BcSpec { kind: BcKind::Displacement([true; 3], [0.0; 3]), tris: bottom },
            BcSpec { kind: BcKind::Force(f), tris: top },
        ]);
        let re = r[0].as_ref().expect("displacement support reaction");
        for d in 0..3 {
            assert!(
                (re.force[d] + f[d]).abs() < 1e-2 * 60.0,
                "Σ R != -F on axis {d}: {:?} vs {f:?}",
                re.force
            );
        }
    }
}
