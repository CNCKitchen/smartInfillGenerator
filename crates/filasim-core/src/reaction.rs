// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Support reaction recovery: the force (and moment) each support exerts on
//! the part, computed from a solved displacement field.
//!
//! ONE recovery for both enforcement mechanisms — **nodal equilibrium of the
//! unconstrained element operator**, `Rₙ = (K·u)ₙ − fₙ^ext`, where `fₙ^ext` is
//! whatever EXTERNAL load (surface traction, self-weight, …) also lands on that
//! node. It is the same recovery the build sim uses for its bed reaction
//! (`buildsim::apply_k`), restricted to the cells adjacent to the support's
//! nodes. `K` is the element blend the solve actually ran (isotropic solid share
//! + optional TI infill share, DESIGN §22) — recovering with a different
//! material would report a reaction the displacement field never produced.
//!
//! For a Dirichlet-eliminated support ([`BcKind::Fixed`]) this is the textbook
//! recovery. For a PENALTY support (Frictionless / Displacement / Cylindrical /
//! Elastic) it is algebraically identical to summing the spring forces: the node
//! row reads `(K·u)ₙ + Σ springs = fₙ^bc + fₙ^ext`, and the support force is
//! `−Σ springs + fₙ^bc`, i.e. `(K·u)ₙ − fₙ^ext`.
//!
//! Algebraically identical, but NOT numerically. A spring carries
//! `k = SPRING_FACTOR·E0·h`, a few hundred times the element stiffness, so the
//! spring form is a difference of two large numbers: it multiplies the solve's
//! displacement error by `k`, while the equilibrium form multiplies it by
//! `E0·h`. On a prescribed-motion clamp at the default tolerance that is the
//! difference between reactions good to 0.2 % and reactions good to 5 % that
//! wander by half a newton between runs. Using ONE form for every support also
//! makes `Σ R` close exactly — the element operator has zero row sums, so the
//! nodal balances telescope.
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
    // supports can both claim it. Its (single) nodal reaction is split evenly
    // between the claimants — otherwise it would be double-counted and Σ R would
    // stop balancing the applied loads.
    let mut claims: HashMap<u32, u32> = HashMap::new();
    for (bc, nodes) in bcs.iter().zip(&asm.bc_nodes) {
        if is_support(&bc.kind) {
            for &n in nodes {
                *claims.entry(n).or_insert(0) += 1;
            }
        }
    }

    // EXTERNAL load at each support node — everything on the force RHS except
    // what a support itself put there. A Displacement support's prescribed value
    // rides the RHS as `k·value` (see `attach.rs`): that is part of the support
    // force, not a load acting on the part, so it must not be subtracted off.
    // Everything else (a surface load sharing an edge node, the self-weight of
    // the adjacent cells, a remote mass) is a real external load and must be.
    let mut ext_f: HashMap<u32, [f64; 3]> = claims.keys().map(|&n| (n, [0.0; 3])).collect();
    if !ext_f.is_empty() {
        let mut own = vec![false; asm.problem.forces.len()];
        for (bi, bc) in bcs.iter().enumerate() {
            if is_support(&bc.kind) {
                own[asm.bc_forces[bi].clone()].fill(true);
            }
        }
        for (i, &(n, f)) in asm.problem.forces.iter().enumerate() {
            if own[i] {
                continue;
            }
            if let Some(acc) = ext_f.get_mut(&n) {
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
            for &n in nodes {
                let ku = ku_at(n);
                let f = ext_f.get(&n).copied().unwrap_or([0.0; 3]);
                let share = 1.0 / claims.get(&n).copied().unwrap_or(1) as f64;
                let acc = r.entry(n).or_insert([0.0; 3]);
                for d in 0..3 {
                    acc[d] += (ku[d] - f[d]) * share;
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
    use crate::mesh::{primitives, TriMesh};
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
        solve_and_react_tol(bcs, 1e-8)
    }

    fn solve_and_react_tol(bcs: Vec<BcSpec>, tol: f64) -> Vec<Option<BcReaction>> {
        react_on(&fixture().0, bcs, tol)
    }

    fn react_on(mesh: &TriMesh, bcs: Vec<BcSpec>, tol: f64) -> Vec<Option<BcReaction>> {
        let settings = SolveSettings { tol, ..Default::default() };
        let grid0 = VoxelGrid::voxelize(mesh, 1.0);
        let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
        let asm = assemble(mesh, &grid, &bcs, None, &settings).unwrap();
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

    /// A support that PRESCRIBES a motion enforces it with penalty forces
    /// `k·value`, `k = SPRING_FACTOR·E0·h` — orders of magnitude above any real
    /// force in the model. Measured against that inflated ‖b‖ the solver's
    /// relative tolerance is meaningless, and the reactions it feeds come out
    /// pure noise: before `solve::prescribed_ref_norm` rescaled the convergence
    /// test, this fixture reported a fixed support carrying MORE than the sum of
    /// the two supports that actually load it, i.e. `Σ R ≠ 0` by 400 %.
    ///
    /// So assert the equilibrium the recovery cannot fake: nothing is applied to
    /// this cylinder except two prescribed motions, so all three supports must
    /// sum to zero. The tolerance is on the reaction scale, not on ‖b‖ (which is
    /// ~10⁵ times larger) — that difference IS the bug.
    #[test]
    fn prescribed_motion_reactions_are_in_equilibrium() {
        // A SLENDER cantilever is the worst case for the inflation: the tip
        // stiffness (~1.2 N/mm) is ~10⁶ below the penalty (300·E0·h per node),
        // so ‖b‖ says nothing at all about the size of the answer. A stubby
        // fixture would hide the bug.
        let mesh = primitives::boxx([0.0, 0.0, 0.0], [64.0, 8.0, 4.0]);
        let root: Vec<u32> = vec![0, 1]; // −x face
        let tip: Vec<u32> = vec![2, 3]; //  +x face
        let bcs = vec![
            BcSpec { kind: BcKind::Fixed, tris: root },
            // Pull the tip 1 mm across the beam: the root is then the ONLY other
            // support, so the two reactions must be equal and opposite exactly.
            BcSpec {
                kind: BcKind::Displacement([false, false, true], [0.0, 0.0, 1.0]),
                tris: tip,
            },
        ];
        // The PRODUCTION tolerance, deliberately: the bug is that `tol` was
        // being applied to the wrong norm, so a fixture that over-converges
        // would hide it.
        let r = react_on(&mesh, bcs, SolveSettings::default().tol);
        let f: Vec<[f64; 3]> = r.iter().map(|e| e.as_ref().expect("support").force).collect();
        let scale = f
            .iter()
            .map(|v| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt())
            .fold(0f64, f64::max);
        assert!(scale > 0.1, "degenerate fixture: no support carries anything ({f:?})");
        let sum = [0, 1, 2].map(|d| f[0][d] + f[1][d]);
        let residual = (sum[0] * sum[0] + sum[1] * sum[1] + sum[2] * sum[2]).sqrt();
        assert!(
            residual < 0.01 * scale,
            "Σ R = {sum:?} (|Σ R| = {residual}) is not zero against a reaction scale of {scale}: {f:?}"
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
