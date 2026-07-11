// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Rigid remote point mass (DESIGN §16 milestone 4): a patch of surface nodes
//! rigidly bolted to a 6-DOF virtual master at the mass CG, realised as a
//! penalty coupling with the master **statically condensed out** so no DOFs are
//! added to the matrix-free solver.
//!
//! Kinematics: patch node `i` (arm `rᵢ = xᵢ − p` from the CG `p`) is tied to the
//! master `m = [t; θ]` by `uᵢ ≈ t + θ×rᵢ = Bᵢ m`, with the 3×6 matrix
//! `Bᵢ = [I | −[rᵢ]×]`. Minimizing the penalty energy `½k Σ|uᵢ − Bᵢm|²` over the
//! free master `m = G⁻¹ Σ Bⱼᵀuⱼ` (`G = Σ Bⱼᵀ Bⱼ`, 6×6) condenses to
//!
//! ```text
//!   K_rigid = k (I − B G⁻¹ Bᵀ)      (stacked over the patch; SPD, symmetric)
//! ```
//!
//! — a per-node `k·I` diagonal (elastic-foundation character) plus ONE rank-6
//! coupling `−k B G⁻¹ Bᵀ` per mass. The matvec is a two-pass reduce/scatter,
//! O(patch) with a fixed 6×6 solve, so it threads into the matrix-free `apply`
//! right after the hex scatter, exactly like a penalty spring. A master force
//! `f_master = [F; M]` distributes to the patch as `f_load,i = Bᵢ G⁻¹ f_master`
//! (k-independent), statically equivalent to `f_master` applied at the CG.

use crate::fem::invert3;

/// A rigid remote-mass coupling on one patch. Node indices are on the finest
/// level (the only level that carries the term — DESIGN §16: MG pass-through).
#[derive(Clone, Debug, PartialEq)]
pub struct RigidGroup {
    /// Patch node indices (finest grid).
    pub nodes: Vec<u32>,
    /// Arm `rᵢ = xᵢ − p` (mm) per node, from the mass CG `p`.
    pub arms: Vec<[f64; 3]>,
    /// Penalty stiffness `k` (N/mm).
    pub k: f64,
    /// Condensed master Gram inverse `G⁻¹` (6×6, symmetric).
    ginv: [[f64; 6]; 6],
    /// Per-node exact diagonal block `k(I − Bᵢ G⁻¹ Bᵢᵀ)` for the block-Jacobi
    /// smoother (the (i,i) block of `K_rigid`, PSD). Symmetric 3×3.
    diag: Vec<[[f64; 3]; 3]>,
}

impl RigidGroup {
    /// Build from patch nodes + arms + penalty stiffness. Returns `None` when the
    /// master Gram `G` is singular — a patch of fewer than 3 non-collinear nodes
    /// cannot define a rigid body (the caller then falls back to deformable).
    pub fn build(nodes: Vec<u32>, arms: Vec<[f64; 3]>, k: f64) -> Option<Self> {
        debug_assert_eq!(nodes.len(), arms.len());
        if nodes.len() < 3 || k <= 0.0 {
            return None;
        }
        // G = Σ Bᵢᵀ Bᵢ (6×6).
        let mut g = [[0f64; 6]; 6];
        for r in &arms {
            let b = bmat(*r);
            for a in 0..6 {
                for c in 0..6 {
                    let mut s = 0.0;
                    for row in 0..3 {
                        s += b[row][a] * b[row][c];
                    }
                    g[a][c] += s;
                }
            }
        }
        let ginv = invert6(&g)?;
        // Per-node diagonal block k(I − Bᵢ G⁻¹ Bᵢᵀ).
        let mut diag = Vec::with_capacity(nodes.len());
        for r in &arms {
            let b = bmat(*r);
            // W = G⁻¹ Bᵢᵀ (6×3): W[a][col] = Σ_j ginv[a][j] · Bᵢᵀ[j][col].
            let mut w = [[0f64; 3]; 6];
            for a in 0..6 {
                for col in 0..3 {
                    let mut s = 0.0;
                    for j in 0..6 {
                        s += ginv[a][j] * b[col][j]; // Bᵢᵀ[j][col] = b[col][j]
                    }
                    w[a][col] = s;
                }
            }
            // M = Bᵢ W (3×3); block = k(I − M).
            let mut blk = [[0f64; 3]; 3];
            for row in 0..3 {
                for col in 0..3 {
                    let mut s = 0.0;
                    for a in 0..6 {
                        s += b[row][a] * w[a][col];
                    }
                    blk[row][col] = k * (if row == col { 1.0 } else { 0.0 } - s);
                }
            }
            diag.push(blk);
        }
        Some(Self { nodes, arms, k, ginv, diag })
    }

    /// The exact diagonal 3×3 block for patch node `i` (folds into `build_dinv`).
    #[inline]
    pub fn diag_block(&self, i: usize) -> &[[f64; 3]; 3] {
        &self.diag[i]
    }

    /// `y += K_rigid x` (f64, the outer-CG operator). Two passes: gather
    /// `b = Σ Bᵢᵀuᵢ`, condense `c = G⁻¹b`, scatter `yᵢ += k(uᵢ − Bᵢc)`.
    pub fn accumulate_f64(&self, x: &[f64], y: &mut [f64]) {
        let mut b = [0f64; 6];
        for (i, &n) in self.nodes.iter().enumerate() {
            let n = n as usize;
            let (u0, u1, u2) = (x[3 * n], x[3 * n + 1], x[3 * n + 2]);
            let r = self.arms[i];
            b[0] += u0;
            b[1] += u1;
            b[2] += u2;
            b[3] += r[1] * u2 - r[2] * u1;
            b[4] += r[2] * u0 - r[0] * u2;
            b[5] += r[0] * u1 - r[1] * u0;
        }
        let c = matvec6(&self.ginv, &b);
        let (ct, cr) = ([c[0], c[1], c[2]], [c[3], c[4], c[5]]);
        for (i, &n) in self.nodes.iter().enumerate() {
            let n = n as usize;
            let r = self.arms[i];
            // Bᵢ c = ct − rᵢ × cr ; yᵢ += k(uᵢ − Bᵢc) = k(uᵢ − ct + rᵢ×cr).
            let rc = cross(r, cr);
            for d in 0..3 {
                y[3 * n + d] += self.k * (x[3 * n + d] - ct[d] + rc[d]);
            }
        }
    }

    /// `y += K_rigid x` (f32 twin; the 6×6 reduction runs in f64 to match the
    /// f32 hex `apply`, whose spring post-pass also accumulates in f64).
    pub fn accumulate_f32(&self, x: &[f32], y: &mut [f32]) {
        let mut b = [0f64; 6];
        for (i, &n) in self.nodes.iter().enumerate() {
            let n = n as usize;
            let (u0, u1, u2) = (x[3 * n] as f64, x[3 * n + 1] as f64, x[3 * n + 2] as f64);
            let r = self.arms[i];
            b[0] += u0;
            b[1] += u1;
            b[2] += u2;
            b[3] += r[1] * u2 - r[2] * u1;
            b[4] += r[2] * u0 - r[0] * u2;
            b[5] += r[0] * u1 - r[1] * u0;
        }
        let c = matvec6(&self.ginv, &b);
        let (ct, cr) = ([c[0], c[1], c[2]], [c[3], c[4], c[5]]);
        for (i, &n) in self.nodes.iter().enumerate() {
            let n = n as usize;
            let r = self.arms[i];
            let rc = cross(r, cr);
            for d in 0..3 {
                let u = x[3 * n + d] as f64;
                y[3 * n + d] += (self.k * (u - ct[d] + rc[d])) as f32;
            }
        }
    }

    /// Fraction of the patch's motion in `u` that is NOT a rigid-body mode:
    /// `‖u − B G⁻¹ Bᵀ u‖ / ‖u‖` over the patch (0 = perfectly rigid). A
    /// diagnostic for how well the penalty enforces rigidity — a rigid mount
    /// drives it toward zero, a deformable patch leaves it O(1).
    pub fn nonrigidity(&self, u: &[f64]) -> f64 {
        let mut b = [0f64; 6];
        let mut unorm = 0.0;
        for (i, &n) in self.nodes.iter().enumerate() {
            let n = n as usize;
            let uu = [u[3 * n], u[3 * n + 1], u[3 * n + 2]];
            let r = self.arms[i];
            b[0] += uu[0];
            b[1] += uu[1];
            b[2] += uu[2];
            b[3] += r[1] * uu[2] - r[2] * uu[1];
            b[4] += r[2] * uu[0] - r[0] * uu[2];
            b[5] += r[0] * uu[1] - r[1] * uu[0];
            unorm += uu[0] * uu[0] + uu[1] * uu[1] + uu[2] * uu[2];
        }
        if unorm <= 0.0 {
            return 0.0;
        }
        let c = matvec6(&self.ginv, &b);
        let (ct, cr) = ([c[0], c[1], c[2]], [c[3], c[4], c[5]]);
        let mut resid = 0.0;
        for (i, &n) in self.nodes.iter().enumerate() {
            let n = n as usize;
            let r = self.arms[i];
            let rc = cross(r, cr); // Bᵢ c = ct − rᵢ×cr
            for d in 0..3 {
                let e = u[3 * n + d] - ct[d] + rc[d];
                resid += e * e;
            }
        }
        (resid / unorm).sqrt()
    }

    /// Per-node consistent load for a master force `f_master = [F; M]`:
    /// `f_load,i = Bᵢ G⁻¹ f_master` — statically equivalent to `f_master` at the
    /// CG (zero net spurious moment). Returns one force vector per patch node.
    pub fn load(&self, f_master: [f64; 6]) -> Vec<[f64; 3]> {
        let c = matvec6(&self.ginv, &f_master);
        let (ct, cr) = ([c[0], c[1], c[2]], [c[3], c[4], c[5]]);
        self.arms
            .iter()
            .map(|&r| {
                let rc = cross(r, cr); // Bᵢ c = ct − rᵢ × cr
                [ct[0] - rc[0], ct[1] - rc[1], ct[2] - rc[2]]
            })
            .collect()
    }
}

#[inline]
fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// `Bᵢ = [I | −[rᵢ]×]`, the 3×6 rigid-kinematics matrix for arm `r`
/// (`−[r]× v = v × r ... ` chosen so `Bᵢ m = t + θ×r`).
#[inline]
fn bmat(r: [f64; 3]) -> [[f64; 6]; 3] {
    [
        [1.0, 0.0, 0.0, 0.0, r[2], -r[1]],
        [0.0, 1.0, 0.0, -r[2], 0.0, r[0]],
        [0.0, 0.0, 1.0, r[1], -r[0], 0.0],
    ]
}

#[inline]
fn matvec6(m: &[[f64; 6]; 6], v: &[f64; 6]) -> [f64; 6] {
    let mut out = [0f64; 6];
    for (a, row) in m.iter().enumerate() {
        let mut s = 0.0;
        for c in 0..6 {
            s += row[c] * v[c];
        }
        out[a] = s;
    }
    out
}

/// Gauss–Jordan inverse of a 6×6 with partial pivoting; `None` if singular
/// (relative to the matrix scale). The 3×3 fast path reuses `invert3` for the
/// pure-translation degenerate block is not needed — a valid patch is 6-rank.
fn invert6(a: &[[f64; 6]; 6]) -> Option<[[f64; 6]; 6]> {
    let maxabs = a.iter().flatten().fold(0f64, |m, &v| m.max(v.abs())).max(1.0);
    let thresh = 1e-10 * maxabs;
    let mut m = *a;
    let mut inv = [[0f64; 6]; 6];
    for (i, row) in inv.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    for col in 0..6 {
        // Partial pivot on the current column.
        let mut piv = col;
        let mut best = m[col][col].abs();
        for r in (col + 1)..6 {
            if m[r][col].abs() > best {
                best = m[r][col].abs();
                piv = r;
            }
        }
        if best < thresh {
            return None;
        }
        if piv != col {
            m.swap(col, piv);
            inv.swap(col, piv);
        }
        let dinv = 1.0 / m[col][col];
        for c in 0..6 {
            m[col][c] *= dinv;
            inv[col][c] *= dinv;
        }
        for r in 0..6 {
            if r == col {
                continue;
            }
            let f = m[r][col];
            if f != 0.0 {
                for c in 0..6 {
                    m[r][c] -= f * m[col][c];
                    inv[r][c] -= f * inv[col][c];
                }
            }
        }
    }
    // Touch invert3 so the import stays meaningful even if a future degenerate
    // (planar, <6-rank) fallback wants the 3×3 translation-only inverse.
    let _ = invert3;
    Some(inv)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 20×20 square patch (4 corner nodes) in the z=0 plane, CG at its center.
    fn sample_group(k: f64) -> RigidGroup {
        let nodes = vec![0u32, 1, 2, 3];
        let pts = [
            [-10.0, -10.0, 0.0],
            [10.0, -10.0, 0.0],
            [10.0, 10.0, 0.0],
            [-10.0, 10.0, 0.0],
        ];
        let arms: Vec<[f64; 3]> = pts.iter().map(|q| *q).collect();
        RigidGroup::build(nodes, arms, k).expect("non-degenerate patch")
    }

    fn apply(g: &RigidGroup, x: &[f64]) -> Vec<f64> {
        let mut y = vec![0f64; x.len()];
        g.accumulate_f64(x, &mut y);
        y
    }

    #[test]
    fn rigid_body_motion_is_in_the_null_space() {
        // A patch moving as a rigid body (uᵢ = t + θ×rᵢ) stores zero penalty
        // energy — the defining property of the condensed operator.
        let g = sample_group(1000.0);
        let t = [0.3, -0.5, 0.2];
        let th = [0.01, -0.02, 0.03];
        let mut x = vec![0f64; 12];
        for (i, r) in g.arms.iter().enumerate() {
            let c = cross(th, *r);
            for d in 0..3 {
                x[3 * i + d] = t[d] + c[d];
            }
        }
        let y = apply(&g, &x);
        let nrm = y.iter().map(|v| v * v).sum::<f64>().sqrt();
        assert!(nrm < 1e-6, "rigid-body motion must be in the null space, |Ku|={nrm}");
    }

    #[test]
    fn operator_is_symmetric_and_psd() {
        let g = sample_group(750.0);
        let rand = |seed: u64| -> Vec<f64> {
            (0..12)
                .map(|i| {
                    let mut z = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(seed);
                    z ^= z >> 30;
                    z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
                    z ^= z >> 27;
                    (z as f64) / (u64::MAX as f64) - 0.5
                })
                .collect()
        };
        let (x, y) = (rand(1), rand(2));
        let kx = apply(&g, &x);
        let ky = apply(&g, &y);
        let ytkx: f64 = y.iter().zip(&kx).map(|(a, b)| a * b).sum();
        let xtky: f64 = x.iter().zip(&ky).map(|(a, b)| a * b).sum();
        assert!((ytkx - xtky).abs() < 1e-9 * (1.0 + ytkx.abs()), "operator not symmetric");
        let xtkx: f64 = x.iter().zip(&kx).map(|(a, b)| a * b).sum();
        assert!(xtkx >= -1e-9, "operator not PSD: xᵀKx = {xtkx}");
    }

    #[test]
    fn stored_diagonal_matches_the_operator() {
        // The stored block equals the operator's (i,i) 3×3 sub-block: apply to a
        // unit vector at node i, read the response at node i.
        let g = sample_group(500.0);
        for i in 0..g.nodes.len() {
            let blk = *g.diag_block(i);
            for col in 0..3 {
                let mut x = vec![0f64; 12];
                x[3 * i + col] = 1.0;
                let y = apply(&g, &x);
                for row in 0..3 {
                    assert!(
                        (y[3 * i + row] - blk[row][col]).abs() < 1e-6,
                        "diag[{i}][{row}][{col}] mismatch"
                    );
                }
            }
        }
    }

    #[test]
    fn load_is_statically_equivalent_to_the_master_force() {
        let g = sample_group(1000.0);
        // Pure force at the CG (no master moment): resultant = F, zero moment.
        let f = [5.0, -3.0, 8.0];
        let loads = g.load([f[0], f[1], f[2], 0.0, 0.0, 0.0]);
        let mut fsum = [0f64; 3];
        let mut msum = [0f64; 3];
        for (i, fi) in loads.iter().enumerate() {
            let m = cross(g.arms[i], *fi);
            for d in 0..3 {
                fsum[d] += fi[d];
                msum[d] += m[d];
            }
        }
        for d in 0..3 {
            assert!((fsum[d] - f[d]).abs() < 1e-9, "resultant force off axis {d}");
            assert!(msum[d].abs() < 1e-9, "spurious moment on axis {d}: {}", msum[d]);
        }
    }

    #[test]
    fn degenerate_patch_returns_none() {
        // Two nodes cannot define a rigid body.
        assert!(RigidGroup::build(vec![0, 1], vec![[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0]], 1.0).is_none());
    }
}
