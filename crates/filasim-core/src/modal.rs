// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Constrained undamped modal analysis (natural frequencies + mode shapes).
//!
//! Solves the generalized eigenproblem `K v = λ M v` for the lowest `num_modes`
//! pairs, with `K` the matrix-free voxel stiffness (`mg.rs`) and `M` a lumped,
//! density-scaled mass. `λ = ω²` (rad/s)²; frequency `f = √λ / 2π` (Hz).
//!
//! Method: **subspace inverse iteration with Rayleigh–Ritz** — the robust block
//! cousin of LOBPCG. Each outer step solves `K X = M V` (one matrix-free MGCG
//! solve per column, reusing the *existing* multigrid as the inverse operator),
//! M-orthonormalizes `X`, then extracts Ritz pairs from the small `p×p`
//! projected stiffness `Yᵀ K Y`. Inverse iteration maps the smallest eigenvalues
//! to the dominant directions of `K⁻¹ M`, so the lowest frequencies converge
//! first; a handful of guard vectors above `num_modes` absorb the slow tail.
//!
//! Consistent units (mm · tonne · s · N · MPa): `E` in MPa makes `K` N/mm, so a
//! lumped mass in **tonne** (density in tonne/mm³ × cell volume in mm³) yields
//! `ω` in rad/s. A wrong mass scale shows up as every frequency off by a
//! constant `√` factor — exactly what the cantilever golden test guards.

use crate::fem::NODE_OFFSETS;
use crate::mg::MgSolver;
use crate::solve::SolveError;
use std::f64::consts::TAU;

/// Inputs for [`analyze`]. `num_modes` is the user-selected count; the rest are
/// LOBPCG knobs with sane defaults via [`ModalConfig::new`].
#[derive(Clone, Copy, Debug)]
pub struct ModalConfig {
    pub num_modes: usize,
    /// LOBPCG iteration cap (one multigrid V-cycle per mode per iteration).
    pub max_iters: usize,
    /// Converged when the relative residual `‖KX−θMX‖/‖KX‖` of every requested
    /// mode falls below this.
    pub tol: f64,
}

impl ModalConfig {
    pub fn new(num_modes: usize) -> Self {
        Self { num_modes, max_iters: 100, tol: 1e-3 }
    }
}

pub struct ModalResult {
    /// Natural frequencies (Hz), ascending, length = resolved mode count.
    pub freqs_hz: Vec<f64>,
    /// Mode shapes (M-normalized nodal displacement, f32, on the padded node
    /// grid — same layout as `Solution::u`), one per frequency.
    pub shapes: Vec<Vec<f32>>,
    /// LOBPCG iterations run.
    pub outer_iters: usize,
    /// Total multigrid V-cycles (the dominant cost; the headline to watch).
    pub total_inner_iters: usize,
    pub converged: bool,
}

/// Lumped, density-scaled mass diagonal (length = ndof) for the finest level.
/// `vfrac[ci]` is the per-cell material VOLUME fraction (0 = void, 1 = fully
/// dense) — distinct from the stiffness `eps`, since mass scales with material
/// volume while stiffness follows the (possibly non-linear) `E(ρ)` law. Each
/// cell's mass `ρ · h³ · vfrac` lumps equally onto its 8 nodes; constrained /
/// inactive DOFs get zero (no inertia in the reduced system).
fn lumped_mass(solver: &MgSolver, vfrac: &[f32], density: f64) -> Vec<f64> {
    let lvl = &solver.levels[0];
    let (nx, ny, nz) = (lvl.nx, lvl.ny, lvl.nz);
    let (mx, my) = (lvl.mx, lvl.my);
    debug_assert_eq!(vfrac.len(), nx * ny * nz);
    let cell_vol = lvl.h * lvl.h * lvl.h;
    let mut m = vec![0f64; lvl.ndof()];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                let vf = vfrac[ci] as f64;
                if vf <= 0.0 {
                    continue;
                }
                let node_mass = density * cell_vol * vf / 8.0;
                for [ox, oy, oz] in NODE_OFFSETS {
                    let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
                    m[3 * n] += node_mass;
                    m[3 * n + 1] += node_mass;
                    m[3 * n + 2] += node_mass;
                }
            }
        }
    }
    for (i, &c) in lvl.constrained.iter().enumerate() {
        if c {
            m[i] = 0.0;
        }
    }
    m
}

/// `⟨a, b⟩_M = Σ mᵢ aᵢ bᵢ` (lumped mass is diagonal).
#[inline]
fn m_dot(a: &[f64], b: &[f64], m: &[f64]) -> f64 {
    let mut s = 0.0;
    for i in 0..a.len() {
        s += m[i] * a[i] * b[i];
    }
    s
}

/// M-orthonormalize the columns in place (modified Gram–Schmidt in the
/// M-inner-product), so afterwards `colsᵀ M cols = I`. Near-dependent columns
/// (norm underflow) are left as-is; Rayleigh–Ritz then deflates them.
fn m_orthonormalize(cols: &mut [Vec<f64>], m: &[f64]) {
    let p = cols.len();
    for j in 0..p {
        // Pull column j out so we can borrow the earlier columns immutably.
        let mut cj = std::mem::take(&mut cols[j]);
        for ck in cols.iter().take(j) {
            let dot = m_dot(&cj, ck, m);
            for i in 0..cj.len() {
                cj[i] -= dot * ck[i];
            }
        }
        let nrm = m_dot(&cj, &cj, m).max(0.0).sqrt();
        if nrm > 1e-150 {
            let inv = 1.0 / nrm;
            for v in cj.iter_mut() {
                *v *= inv;
            }
        }
        cols[j] = cj;
    }
}

/// Cyclic Jacobi eigensolver for a small symmetric `n×n` matrix (`a` row-major).
/// Returns eigenvalues ASCENDING and eigenvectors as a row-major `n×n` matrix
/// whose column `k` is the eigenvector of eigenvalue `k`. `n` is tiny here
/// (mode count + guard ≤ ~28), so this is cheap and robust.
fn jacobi_eig(a_in: &[f64], n: usize) -> (Vec<f64>, Vec<f64>) {
    let mut a = a_in.to_vec();
    let mut v = vec![0f64; n * n];
    for i in 0..n {
        v[i * n + i] = 1.0;
    }
    for _sweep in 0..100 {
        let mut off = 0.0;
        for p in 0..n {
            for q in (p + 1)..n {
                off += a[p * n + q] * a[p * n + q];
            }
        }
        if off < 1e-30 {
            break;
        }
        for p in 0..n {
            for q in (p + 1)..n {
                let apq = a[p * n + q];
                if apq.abs() < 1e-300 {
                    continue;
                }
                let phi = 0.5 * (a[q * n + q] - a[p * n + p]) / apq;
                let t = if phi == 0.0 {
                    1.0
                } else {
                    phi.signum() / (phi.abs() + (phi * phi + 1.0).sqrt())
                };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                // A ← Jᵀ A J (rotate columns p,q then rows p,q).
                for k in 0..n {
                    let akp = a[k * n + p];
                    let akq = a[k * n + q];
                    a[k * n + p] = c * akp - s * akq;
                    a[k * n + q] = s * akp + c * akq;
                }
                for k in 0..n {
                    let apk = a[p * n + k];
                    let aqk = a[q * n + k];
                    a[p * n + k] = c * apk - s * aqk;
                    a[q * n + k] = s * apk + c * aqk;
                }
                // V ← V J.
                for k in 0..n {
                    let vkp = v[k * n + p];
                    let vkq = v[k * n + q];
                    v[k * n + p] = c * vkp - s * vkq;
                    v[k * n + q] = s * vkp + c * vkq;
                }
            }
        }
    }
    let theta: Vec<f64> = (0..n).map(|i| a[i * n + i]).collect();
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&i, &j| theta[i].partial_cmp(&theta[j]).unwrap_or(std::cmp::Ordering::Equal));
    let theta_s: Vec<f64> = idx.iter().map(|&i| theta[i]).collect();
    let mut vs = vec![0f64; n * n];
    for (newc, &oldc) in idx.iter().enumerate() {
        for r in 0..n {
            vs[r * n + newc] = v[r * n + oldc];
        }
    }
    (theta_s, vs)
}

/// Deterministic pseudo-random start vector (reproducible solves), zeroed at
/// constrained DOFs. `seed` decorrelates columns of the starting block.
fn random_vec(n: usize, constrained: &[bool], seed: u32) -> Vec<f64> {
    let mut v = vec![0f64; n];
    for (i, vi) in v.iter_mut().enumerate() {
        let mut x = (i as u32)
            .wrapping_mul(2654435761)
            .wrapping_add(seed.wrapping_mul(40503))
            .wrapping_add(0x9E37_79B1);
        x ^= x >> 13;
        x = x.wrapping_mul(0x9E37_79B1);
        x ^= x >> 16;
        *vi = x as f64 / u32::MAX as f64 - 0.5;
    }
    for (i, &c) in constrained.iter().enumerate() {
        if c {
            v[i] = 0.0;
        }
    }
    v
}

/// Weak isotropic ground springs at a few spread-out active nodes — soft
/// inertia relief for an UNCONSTRAINED part. An unsupported part has a singular
/// `K` (6 rigid-body modes at λ = 0), which inverse iteration can't invert.
/// Anchoring the active nodes at the ± extreme of each axis with weak springs
/// (stiffness `k`) lifts those 6 modes to low — but nonzero — frequencies so
/// `K` becomes SPD; the lifted modes come out as the 6 lowest and the caller
/// drops them. `k` must be SMALL relative to the cell stiffness so the flexible
/// frequencies are barely perturbed (≈ 1e-4·E·h is a good default). The spatial
/// spread of the ± extreme anchors is what also pins the 3 rotations.
///
/// `mx,my,mz` are the node-grid dims; `active[n]` marks load-bearing nodes.
/// Returns 3 springs (x, y, z unit directions) per distinct anchor.
pub fn rigid_body_anchor_springs(
    mx: usize,
    my: usize,
    mz: usize,
    active: &[bool],
    k: f64,
) -> Vec<(u32, [f64; 3], f64)> {
    // Argmin/argmax active node along each of x, y, z (6 extremes).
    let mut ext: [Option<usize>; 6] = [None; 6];
    let mut key = [usize::MAX, 0, usize::MAX, 0, usize::MAX, 0];
    for z in 0..mz {
        for y in 0..my {
            for x in 0..mx {
                let n = (z * my + y) * mx + x;
                if !active[n] {
                    continue;
                }
                let coords = [x, x, y, y, z, z];
                for (slot, &c) in coords.iter().enumerate() {
                    let want_min = slot % 2 == 0;
                    let better = if want_min { c < key[slot] } else { c > key[slot] };
                    if ext[slot].is_none() || better {
                        key[slot] = c;
                        ext[slot] = Some(n);
                    }
                }
            }
        }
    }
    let mut anchors: Vec<usize> = ext.into_iter().flatten().collect();
    anchors.sort_unstable();
    anchors.dedup();
    let mut springs = Vec::with_capacity(anchors.len() * 3);
    for &n in &anchors {
        springs.push((n as u32, [1.0, 0.0, 0.0], k));
        springs.push((n as u32, [0.0, 1.0, 0.0], k));
        springs.push((n as u32, [0.0, 0.0, 1.0], k));
    }
    springs
}

/// L2 norm.
#[inline]
fn l2(v: &[f64]) -> f64 {
    v.iter().map(|x| x * x).sum::<f64>().sqrt()
}

/// Expand a compact free-DOF vector into the full node-grid layout `full`
/// (constrained entries of `full` must already be zero and stay zero).
#[inline]
fn scatter(free_idx: &[usize], c: &[f64], full: &mut [f64]) {
    for (j, &i) in free_idx.iter().enumerate() {
        full[i] = c[j];
    }
}

/// Gather the free DOFs of a full vector into a compact vector.
#[inline]
fn gather(free_idx: &[usize], full: &[f64], c: &mut [f64]) {
    for (j, &i) in free_idx.iter().enumerate() {
        c[j] = full[i];
    }
}

/// Rayleigh–Ritz on an M-orthonormal block `x` with `kx = K·x`: rotate both to
/// the Ritz basis (ascending eigenvalues) in place and return the eigenvalues.
/// `x` M-orthonormal ⇒ the projected mass is the identity, so this is a plain
/// symmetric eigenproblem on `H = xᵀ K x`.
fn ritz_rotate(x: &mut [Vec<f64>], kx: &mut [Vec<f64>], p: usize) -> Vec<f64> {
    let n = x[0].len();
    let mut h = vec![0f64; p * p];
    for a in 0..p {
        for b in a..p {
            let mut s = 0.0;
            for i in 0..n {
                s += x[a][i] * kx[b][i];
            }
            h[a * p + b] = s;
            h[b * p + a] = s;
        }
    }
    let (theta, c) = jacobi_eig(&h, p);
    let xo = x.to_vec();
    let kxo = kx.to_vec();
    for k in 0..p {
        for i in 0..n {
            let (mut sx, mut sk) = (0.0, 0.0);
            for j in 0..p {
                let cc = c[j * p + k];
                sx += cc * xo[j][i];
                sk += cc * kxo[j][i];
            }
            x[k][i] = sx;
            kx[k][i] = sk;
        }
    }
    theta
}

/// M-orthonormalize columns in place (modified Gram–Schmidt, M-inner-product),
/// DROPPING columns that collapse below a tolerance (rank-deficient) so the
/// LOBPCG basis `[X | W | P]` stays full rank.
fn m_orthonormalize_drop(cols: &mut Vec<Vec<f64>>, m: &[f64]) {
    let mut kept: Vec<Vec<f64>> = Vec::new();
    for mut cj in std::mem::take(cols) {
        for k in &kept {
            let dot = m_dot(&cj, k, m);
            for i in 0..cj.len() {
                cj[i] -= dot * k[i];
            }
        }
        let nrm = m_dot(&cj, &cj, m).max(0.0).sqrt();
        if nrm > 1e-7 {
            let inv = 1.0 / nrm;
            for v in cj.iter_mut() {
                *v *= inv;
            }
            kept.push(cj);
        }
    }
    *cols = kept;
}

/// Solve `K v = λ M v` for the lowest `cfg.num_modes` pairs.
///
/// `solver` must already hold the modal stiffness (printed or solid eps).
/// `vfrac` is the per-cell material volume fraction for the lumped mass;
/// `density` is in consistent mass units (tonne/mm³). The returned mode shapes
/// are M-normalized (`vᵀ M v = 1`) — their absolute magnitude is arbitrary, so
/// the viewer normalizes per mode for display.
///
/// `on_progress(outer_done, max_outer, freqs_hz)` is called once per outer
/// subspace-iteration step with the current Ritz frequency estimates — for a
/// live progress bar / convergence readout. Pass a no-op closure to ignore it.
pub fn analyze(
    solver: &mut MgSolver,
    vfrac: &[f32],
    density: f64,
    cfg: &ModalConfig,
    mut on_progress: impl FnMut(usize, usize, &[f64]),
) -> Result<ModalResult, SolveError> {
    let n = solver.levels[0].ndof();
    let m = lumped_mass(solver, vfrac, density);
    let free_dofs = m.iter().filter(|&&mi| mi > 0.0).count();
    if free_dofs == 0 {
        return Err(SolveError::NoSolidCells);
    }
    // Clamp the request to what the (reduced) problem can actually support.
    let num_modes = cfg.num_modes.clamp(1, free_dofs);
    // Guard vectors lift the convergence-limiting ratio (the lowest modes
    // converge as λ_k/λ_{p+1}); a few extra block columns keep it small even for
    // clustered/degenerate spectra (e.g. a pipe's degenerate bending pair).
    let guard = (num_modes / 2).clamp(3, 8);
    let p = (num_modes + guard).min(free_dofs);
    let constrained = solver.levels[0].constrained.clone();

    // ---- LOBPCG on the COMPACT free-DOF subspace ----
    // Only ~1/4 of `n` DOFs are free (the rest are constrained/void zeros).
    // Streaming the full `n`-length vectors through the Rayleigh–Ritz work was
    // the bottleneck (memory-bandwidth bound), so the block vectors live on the
    // `nf` free DOFs; we expand to the full grid only for `K·x` and the
    // preconditioner (which need the multigrid's node layout).
    let free_idx: Vec<usize> = (0..n).filter(|&i| !constrained[i]).collect();
    let nf = free_idx.len();
    let mc: Vec<f64> = free_idx.iter().map(|&i| m[i]).collect();
    let mut full_in = vec![0f64; n]; // constrained entries stay 0 forever
    let mut full_out = vec![0f64; n];
    let mut kbuf = vec![0f64; n]; // scratch for the stationary preconditioner
    let mut kbuf2 = vec![0f64; n];

    // K·x on compact vectors: expand → apply → gather.
    macro_rules! apply_kc {
        ($c:expr, $out:expr) => {{
            scatter(&free_idx, $c, &mut full_in);
            solver.apply_k(&full_in, &mut full_out);
            gather(&free_idx, &full_out, $out);
        }};
    }

    // X: current eigenvector block (M-orthonormal, compact); KX = K·X.
    let mut x: Vec<Vec<f64>> = (0..p)
        .map(|j| {
            let v = random_vec(n, &constrained, j as u32 + 1);
            free_idx.iter().map(|&i| v[i]).collect()
        })
        .collect();
    m_orthonormalize(&mut x, &mc);
    let mut kx: Vec<Vec<f64>> = vec![vec![0f64; nf]; p];
    for k in 0..p {
        apply_kc!(&x[k], &mut kx[k]);
    }
    let mut theta = ritz_rotate(&mut x, &mut kx, p);

    let mut pblk: Vec<Vec<f64>> = Vec::new(); // conjugate search-direction block
    let mut total_inner_iters = 0usize;
    let mut converged = false;
    let mut iters = 0;
    let mut r = vec![0f64; nf];
    let mut wv = vec![0f64; nf];
    // The eigenVALUES (frequencies — what the user wants) converge well before
    // the eigenVECTOR residual, especially when the multigrid preconditioner is
    // weak (slender/thin parts). So stop when the requested frequencies stop
    // moving, not only when the residual is tiny. 4-figure relative frequency
    // agreement is far more than enough (the model is uncalibrated anyway).
    let mut prev_theta = vec![f64::INFINITY; num_modes];
    const EIG_TOL: f64 = 1e-4;
    // Preconditioner strength: a short multigrid solve (this many V-cycles), not
    // a single V-cycle. The bottleneck is the per-ITERATION scalar Rayleigh–Ritz
    // work, so a stronger preconditioner (a few cheap V-cycles) that cuts the
    // iteration count is a net win.
    const PRECOND_CYCLES: usize = 1;

    for it in 0..cfg.max_iters {
        iters = it + 1;
        // Preconditioned residual block: W_k = T (K X_k − θ_k M X_k).
        let mut wblk: Vec<Vec<f64>> = Vec::with_capacity(p);
        let mut max_rel = 0.0f64;
        for k in 0..p {
            for i in 0..nf {
                r[i] = kx[k][i] - theta[k] * mc[i] * x[k][i];
            }
            if k < num_modes {
                // Relative residual of the requested (non-guard) modes.
                max_rel = max_rel.max(l2(&r) / l2(&kx[k]).max(1e-300));
            }
            // W ≈ K⁻¹ R via PRECOND_CYCLES multigrid V-cycles (stationary
            // iteration: z += Vcycle(R − K z)). More cycles = stronger
            // preconditioner = fewer LOBPCG iterations, at more V-cycles each.
            scatter(&free_idx, &r, &mut full_in);
            solver.precondition(&full_in, &mut full_out);
            for _ in 1..PRECOND_CYCLES {
                // full_out holds z; refine: z += Vcycle(b − K z).
                solver.apply_k(&full_out, &mut kbuf);
                for i in 0..n {
                    kbuf[i] = full_in[i] - kbuf[i];
                }
                solver.precondition(&kbuf, &mut kbuf2);
                for i in 0..n {
                    full_out[i] += kbuf2[i];
                }
            }
            gather(&free_idx, &full_out, &mut wv);
            total_inner_iters += PRECOND_CYCLES;
            wblk.push(wv.clone());
            if crate::cancel::requested() {
                return Err(SolveError::Cancelled);
            }
        }
        let cur_freqs: Vec<f64> =
            (0..num_modes).map(|k| theta[k].max(0.0).sqrt() / TAU).collect();
        on_progress(it + 1, cfg.max_iters, &cur_freqs);
        // Converged when the residual is tiny OR the frequencies have stabilized.
        let mut eig_change = 0.0f64;
        for k in 0..num_modes {
            eig_change =
                eig_change.max((theta[k] - prev_theta[k]).abs() / theta[k].abs().max(1e-30));
        }
        prev_theta.copy_from_slice(&theta[..num_modes]);
        if max_rel < cfg.tol || (it >= 2 && eig_change < EIG_TOL) {
            converged = true;
            break;
        }
        // Rayleigh–Ritz over S = [X | W | P], M-orthonormalized (rank-deficient
        // columns dropped). X is already orthonormal, so it survives as the
        // first `nx` columns and the rest come from the W/P blocks.
        let nx = x.len();
        let mut basis: Vec<Vec<f64>> = Vec::with_capacity(3 * p);
        basis.extend(x.iter().cloned());
        basis.append(&mut wblk);
        basis.extend(pblk.iter().cloned());
        m_orthonormalize_drop(&mut basis, &mc);
        let q = basis.len();
        let mut kb: Vec<Vec<f64>> = vec![vec![0f64; nf]; q];
        for col in 0..q {
            apply_kc!(&basis[col], &mut kb[col]);
        }
        // Projected stiffness SK = Sᵀ K S (Sᵀ M S = I); smallest p eigenpairs.
        let mut sk = vec![0f64; q * q];
        for a in 0..q {
            for bb in a..q {
                let mut s = 0.0;
                for i in 0..nf {
                    s += basis[a][i] * kb[bb][i];
                }
                sk[a * q + bb] = s;
                sk[bb * q + a] = s;
            }
        }
        let (th_all, cmat) = jacobi_eig(&sk, q);
        // New X / KX = basis · C[:,0:p]; P = the [W,P]-part of that combination
        // (the "locally optimal" conjugate direction).
        let mut xnew = vec![vec![0f64; nf]; p];
        let mut kxnew = vec![vec![0f64; nf]; p];
        let mut pnew = vec![vec![0f64; nf]; p];
        for k in 0..p {
            for i in 0..nf {
                let (mut sx, mut skx, mut sp) = (0.0, 0.0, 0.0);
                for j in 0..q {
                    let c = cmat[j * q + k];
                    sx += c * basis[j][i];
                    skx += c * kb[j][i];
                    if j >= nx {
                        sp += c * basis[j][i];
                    }
                }
                xnew[k][i] = sx;
                kxnew[k][i] = skx;
                pnew[k][i] = sp;
            }
        }
        x = xnew;
        kx = kxnew;
        pblk = pnew;
        theta = th_all[..p].to_vec();
    }

    // Expand the converged mode shapes back to the full node grid for display.
    let mut x_full: Vec<Vec<f64>> = Vec::with_capacity(num_modes);
    for xc in x.iter().take(num_modes) {
        let mut full = vec![0f64; n];
        scatter(&free_idx, xc, &mut full);
        x_full.push(full);
    }
    let x = x_full;

    let mut freqs_hz = Vec::with_capacity(num_modes);
    let mut shapes = Vec::with_capacity(num_modes);
    for k in 0..num_modes {
        // λ = ω²; clamp tiny negatives (round-off / near-rigid modes) to 0.
        let lambda = theta[k].max(0.0);
        freqs_hz.push(lambda.sqrt() / TAU);
        shapes.push(x[k].iter().map(|&v| v as f32).collect());
    }
    Ok(ModalResult { freqs_hz, shapes, outer_iters: iters, total_inner_iters, converged })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solve::{
        active_nodes, grid_eps, pad_for_levels, NodeProblem, SolveSettings, SolverCache,
    };
    use crate::voxel::VoxelGrid;

    /// Solid rectangular beam, `nxc·nyc·nzc` cells of size `h`, origin at 0.
    fn beam(nxc: usize, nyc: usize, nzc: usize, h: f64) -> VoxelGrid {
        VoxelGrid {
            nx: nxc,
            ny: nyc,
            nz: nzc,
            h,
            origin: [0.0; 3],
            scale: vec![1.0f32; nxc * nyc * nzc],
        }
    }

    /// Euler–Bernoulli 1st-bending natural frequency (Hz) of a cantilever with a
    /// square `a×a` section, length `L`, modulus `e` (MPa), density `rho`
    /// (tonne/mm³). `f = (βL)²/2π · √(EI/(ρAL⁴))`, βL = 1.875104 (mode 1).
    fn eb_cantilever_f1(l: f64, a: f64, e: f64, rho: f64) -> f64 {
        let i = a.powi(4) / 12.0;
        let area = a * a;
        let bl = 1.875104f64;
        bl * bl / TAU * (e * i / (rho * area * l.powi(4))).sqrt()
    }

    /// Build the modal solver cache for a cantilever (root plane x=0 fixed) and
    /// return its lowest `num_modes` frequencies (Hz).
    fn cantilever_freqs(nxc: usize, nthick: usize, h: f64, num_modes: usize) -> Vec<f64> {
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };
        let density = 1.24e-9; // PLA, tonne/mm³
        let raw = beam(nxc, nthick, nthick, h);
        let (grid, levels) = pad_for_levels(&raw, s.max_levels);
        // Fix the whole root plane (node x-index 0), active nodes only.
        let active = active_nodes(&grid);
        let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
        let mut fixed = Vec::new();
        for z in 0..mz {
            for y in 0..my {
                let n = (z * my + y) * mx; // x index 0
                if active[n] {
                    fixed.push(n as u32);
                }
            }
        }
        let problem = NodeProblem { fixed, springs: Vec::new(), forces: Vec::new() };
        let eps = grid_eps(&grid);
        let mut cache = SolverCache::build(&grid, levels, &problem, &s, eps);
        let cfg = ModalConfig::new(num_modes);
        let res = analyze(&mut cache.solver, &grid.scale, density, &cfg, |_, _, _| {}).unwrap();
        eprintln!(
            "[modal] {} cells, {} modes: {} outer iters, {} total inner MGCG iters (converged={})",
            grid.cell_count(),
            num_modes,
            res.outer_iters,
            res.total_inner_iters,
            res.converged
        );
        res.freqs_hz
    }

    #[test]
    fn cantilever_matches_euler_bernoulli() {
        // L = 40 mm, square a = 4 mm (L/a = 10), 4 cells through thickness.
        let (nxc, nthick, h) = (40, 4, 1.0);
        let (l, a) = (nxc as f64 * h, nthick as f64 * h);
        let f = cantilever_freqs(nxc, nthick, h, 4);
        let theory = eb_cantilever_f1(l, a, 2400.0, 1.24e-9);

        // Ascending and positive.
        for w in f.windows(2) {
            assert!(w[1] >= w[0] - 1e-6, "frequencies must be ascending: {f:?}");
        }
        assert!(f[0] > 0.0, "first frequency must be positive: {f:?}");

        // f1 within a broad factor of analytic theory — the guard against a
        // wrong mass scale / unit slip (which shows as a constant √ factor).
        // The band is wide because a 4-cell-thick fully-integrated hex beam is
        // shear-stiff; the point is catching gross scaling bugs, not precision.
        let r = f[0] / theory;
        assert!(
            (0.6..1.8).contains(&r),
            "f1={:.1} Hz vs Euler-Bernoulli {:.1} Hz (ratio {:.2}) out of band",
            f[0],
            theory,
            r
        );

        // Square section ⇒ the first two bending modes are degenerate.
        let degen = (f[1] - f[0]).abs() / f[0];
        assert!(degen < 0.10, "modes 1&2 should be near-degenerate: {f:?}");
    }

    // Profiling harness for the real user scenario: pipe.stl, lower face fixed,
    // 6 modes. Run with:  cargo test -p filasim-core --lib --release pipe_modal_profile -- --ignored --nocapture
    #[test]
    #[ignore]
    fn pipe_modal_profile() {
        use std::time::Instant;
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../pipe.stl");
        let bytes = std::fs::read(path).expect("pipe.stl in repo root");
        let mesh = crate::mesh::TriMesh::from_stl(&bytes).expect("parse pipe");
        let (lo, hi) = mesh.bounds().unwrap();
        eprintln!(
            "pipe: {} tris, bbox {:.1}x{:.1}x{:.1}",
            mesh.len(),
            hi[0] - lo[0],
            hi[1] - lo[1],
            hi[2] - lo[2]
        );
        let s = SolveSettings { e0: 3500.0, nu: 0.35, ..Default::default() };
        let raw = VoxelGrid::voxelize(&mesh, 0.90);
        let (grid, levels) = pad_for_levels(&raw, s.max_levels);
        eprintln!(
            "grid {}x{}x{} = {} cells, {} solid, {} levels",
            grid.nx,
            grid.ny,
            grid.nz,
            grid.cell_count(),
            grid.solid_count(),
            levels
        );
        // Fix the lower face: active nodes on the z=0 plane.
        let active = active_nodes(&grid);
        let (mx, my) = (grid.nx + 1, grid.ny + 1);
        let mut fixed = Vec::new();
        for y in 0..my {
            for x in 0..mx {
                let n = y * mx + x; // z index 0
                if active[n] {
                    fixed.push(n as u32);
                }
            }
        }
        eprintln!("fixed {} lower-face nodes", fixed.len());
        let problem = NodeProblem { fixed, springs: Vec::new(), forces: Vec::new() };
        let t = Instant::now();
        let mut cache = SolverCache::build(&grid, levels, &problem, &s, grid_eps(&grid));
        eprintln!("cache build: {:.2}s", t.elapsed().as_secs_f64());

        // Micro-benchmark one V-cycle and one K-apply.
        let ndof = cache.solver.levels[0].ndof();
        let mut a = vec![1.0f64; ndof];
        for (i, c) in cache.solver.levels[0].constrained.iter().enumerate() {
            if *c {
                a[i] = 0.0;
            }
        }
        let mut b = vec![0f64; ndof];
        let t = Instant::now();
        for _ in 0..20 {
            cache.solver.precondition(&a, &mut b);
        }
        let vcyc_ms = t.elapsed().as_secs_f64() * 1000.0 / 20.0;
        let t = Instant::now();
        for _ in 0..20 {
            cache.solver.apply_k(&a, &mut b);
        }
        let apply_ms = t.elapsed().as_secs_f64() * 1000.0 / 20.0;
        eprintln!("ndof {ndof}: per V-cycle {vcyc_ms:.1}ms, per K-apply {apply_ms:.1}ms");

        let cfg = ModalConfig::new(6);
        let t = Instant::now();
        let res = analyze(&mut cache.solver, &grid.scale, 1.24e-9, &cfg, |_, _, _| {}).unwrap();
        let total = t.elapsed().as_secs_f64();
        eprintln!(
            "modal: {:.2}s — {} iters, {} V-cycles, converged={}",
            total, res.outer_iters, res.total_inner_iters, res.converged
        );
        eprintln!(
            "  est V-cycle time: {:.2}s ({} × {:.1}ms)",
            res.total_inner_iters as f64 * vcyc_ms / 1000.0,
            res.total_inner_iters,
            vcyc_ms
        );
        eprintln!("  freqs: {:?}", res.freqs_hz.iter().map(|f| f.round()).collect::<Vec<_>>());
    }

    #[test]
    fn free_free_beam_filters_rigid_body() {
        // UNCONSTRAINED beam (L=40, a=4): no fixed nodes, soft anchor springs.
        // Request 1 flexible mode + 6 rigid-body, drop the 6 lowest. The first
        // flexible mode is free-free 1st bending (βL = 4.730).
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };
        let density = 1.24e-9;
        let (nxc, nthick, h) = (40, 4, 1.0);
        let raw = beam(nxc, nthick, nthick, h);
        let (grid, levels) = pad_for_levels(&raw, s.max_levels);
        let active = active_nodes(&grid);
        let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
        // Weak anchors lift the 6 rigid-body modes without hard constraints.
        let k = 1e-4 * s.e0 * grid.h;
        let springs = rigid_body_anchor_springs(mx, my, mz, &active, k);
        let problem = NodeProblem { fixed: Vec::new(), springs, forces: Vec::new() };
        let mut cache = SolverCache::build(&grid, levels, &problem, &s, grid_eps(&grid));
        let cfg = ModalConfig::new(1 + 6); // 6 rigid-body + 1 flexible
        let res = analyze(&mut cache.solver, &grid.scale, density, &cfg, |_, _, _| {}).unwrap();
        let f = res.freqs_hz;
        // The 6 lowest are the (soft-lifted) rigid-body modes; the 7th is the
        // first flexible mode and must sit well above them.
        let flex = f[6];
        assert!(flex > 2.0 * f[5], "first flexible mode not separated from rigid-body: {f:?}");

        let (l, a) = (nxc as f64 * h, nthick as f64 * h);
        let i = a.powi(4) / 12.0;
        let area = a * a;
        let bl = 4.730041f64; // free-free 1st bending
        let theory = bl * bl / TAU * (s.e0 * i / (density * area * l.powi(4))).sqrt();
        let r = flex / theory;
        assert!(
            (0.55..1.9).contains(&r),
            "free-free f1={flex:.0} Hz vs theory {theory:.0} Hz (ratio {r:.2}) out of band"
        );
    }

    #[test]
    fn cantilever_voxel_independence() {
        // Same physical beam (L=40, a=4) at two resolutions — the verify-tab
        // voxel-independence gate: coarse and fine must agree.
        let coarse = cantilever_freqs(40, 4, 1.0, 2)[0];
        let fine = cantilever_freqs(80, 8, 0.5, 2)[0];
        let rel = (fine - coarse).abs() / coarse;
        assert!(
            rel < 0.15,
            "coarse {coarse:.1} Hz vs fine {fine:.1} Hz disagree by {:.0}%",
            rel * 100.0
        );
    }
}
