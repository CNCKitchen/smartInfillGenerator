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
    /// Alternative stop: converged when the requested eigenvalues stop moving
    /// by this much relatively, iteration to iteration. The eigenVALUES settle
    /// well before the eigenVECTOR residual (see the modal design note §12a),
    /// so for a frequency readout this is what actually fires — 4-figure
    /// agreement is far more than a display needs.
    ///
    /// A *gradient* consumer needs more: a finite-difference check of the
    /// eigenvalue sensitivity resolves changes near this tolerance, so
    /// `freq::tests` drives it to 1e-12 to keep convergence noise out of the
    /// comparison. The optimizer itself runs the default (its own move limits
    /// and density filter absorb far more noise than this).
    pub eig_tol: f64,
}

impl ModalConfig {
    pub fn new(num_modes: usize) -> Self {
        Self { num_modes, max_iters: 300, tol: 1e-3, eig_tol: 1e-4 }
    }
}

pub struct ModalResult {
    /// Natural frequencies (Hz), ascending, length = resolved mode count.
    pub freqs_hz: Vec<f64>,
    /// Mode shapes (M-normalized nodal displacement on the padded node grid —
    /// same layout as `Solution::u`), one per frequency. `f64` because the
    /// frequency-objective sensitivity is a difference of two comparable terms
    /// (`φᵀK'φ − λ·φᵀM'φ`) and cancels digits; the display path downcasts.
    pub shapes: Vec<Vec<f64>>,
    /// Eigenvalues `λ = ω²` of the returned modes ((rad/s)²), ascending. The
    /// frequency-objective sensitivity works in λ (where the Rayleigh quotient
    /// is linear in `K`), not in Hz — see `simp::eigen_sensitivity`.
    pub lambdas: Vec<f64>,
    /// LOBPCG iterations run.
    pub outer_iters: usize,
    /// Total multigrid V-cycles (the dominant cost; the headline to watch).
    pub total_inner_iters: usize,
    pub converged: bool,
}

/// A converged LOBPCG subspace, kept to warm-start the NEXT analysis of a
/// slightly different design (the frequency-objective optimizer re-analyzes
/// every outer iteration, and consecutive designs differ by one move-limited
/// OC step). Opaque: the caller only stores it and hands it back.
///
/// Reuse is only valid while the free-DOF layout is unchanged — same grid, same
/// constraints, same void pattern. Only the stiffness/mass VALUES may move.
/// `analyze_warm` checks `nf` and silently cold-starts on a mismatch, so a stale
/// block costs time, never correctness.
#[derive(Clone)]
pub struct ModalBlock {
    /// The full `p`-column block on the compact free DOFs — guard columns
    /// included, since they carry the subspace above the requested modes and
    /// are what keeps a clustered spectrum converging fast.
    cols: Vec<Vec<f64>>,
    /// Free-DOF count at capture time; the reuse guard.
    nf: usize,
}

impl ModalBlock {
    /// Number of columns retained (requested modes + guard).
    pub fn width(&self) -> usize {
        self.cols.len()
    }
}

/// Lumped, density-scaled mass diagonal (length = ndof) for the finest level.
/// `vfrac[ci]` is the per-cell material VOLUME fraction (0 = void, 1 = fully
/// dense) — distinct from the stiffness `eps`, since mass scales with material
/// volume while stiffness follows the (possibly non-linear) `E(ρ)` law. Each
/// cell's mass `ρ · h³ · vfrac` lumps equally onto its 8 nodes; constrained /
/// inactive DOFs get zero (no inertia in the reduced system).
///
/// `extra_mass` carries remote point masses (DESIGN §16): `(node, tonne)` pairs
/// added to all three translational DOFs of each node BEFORE the constrained
/// zeroing, so a mass bolted to a support drops out (rides the fixed wall) while
/// a mass on a free patch drags that patch's modes down. Pass `&[]` for none.
fn lumped_mass(
    solver: &MgSolver,
    vfrac: &[f32],
    density: f64,
    extra_mass: &[(u32, f64)],
) -> Vec<f64> {
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
    // Remote point masses: add each node's lumped translational share. Done
    // before the constrained-DOF zeroing so a mass on a fixed patch (rare, but
    // valid) contributes no free inertia — it simply rides the support.
    for &(n, dm) in extra_mass {
        let b = 3 * n as usize;
        if b + 2 < m.len() {
            m[b] += dm;
            m[b + 1] += dm;
            m[b + 2] += dm;
        }
    }
    for (i, &c) in lvl.constrained.iter().enumerate() {
        if c {
            m[i] = 0.0;
        }
    }
    m
}

/// `⟨a, b⟩_M = Σ mᵢ aᵢ bᵢ` (lumped mass is diagonal). Chunk-parallel.
#[inline]
fn m_dot(a: &[f64], b: &[f64], m: &[f64]) -> f64 {
    crate::par::dot_w64(m, a, b)
}

/// Blocked chunk size for the Rayleigh–Ritz kernels: big enough to amortize
/// the parallel dispatch, small enough that a chunk of every basis column
/// (q ≤ ~3·28) plus the outputs stays cache-resident.
const RR_CHUNK: usize = 2048;

/// Symmetric Gram matrix `g[a·q+b] = Σᵢ colsₐ[i]·kcolsᵦ[i]` (K symmetric, so
/// only the upper triangle is computed and mirrored — same convention as the
/// scalar loops it replaces). ONE blocked pass over the data instead of q²/2
/// full-length passes: the former per-pair dot loops were the modal
/// bottleneck (sequential + one cache line per column per element).
fn gram_sym(cols: &[Vec<f64>], kcols: &[Vec<f64>]) -> Vec<f64> {
    let q = cols.len();
    let n = cols[0].len();
    let mut g = crate::par::map_reduce_ranges(
        n,
        RR_CHUNK,
        |s, e| {
            let mut part = vec![0f64; q * q];
            for a in 0..q {
                let ca = &cols[a][s..e];
                for b in a..q {
                    let kb = &kcols[b][s..e];
                    let mut acc = 0.0;
                    for i in 0..ca.len() {
                        acc += ca[i] * kb[i];
                    }
                    part[a * q + b] += acc;
                }
            }
            part
        },
        |mut x, y| {
            for (xi, yi) in x.iter_mut().zip(&y) {
                *xi += yi;
            }
            x
        },
        || vec![0f64; q * q],
    );
    for a in 0..q {
        for b in (a + 1)..q {
            g[b * q + a] = g[a * q + b];
        }
    }
    g
}

/// Blocked basis combination `outs[k] = Σ_{j≥j0} coeff[j·stride + k0+k]·cols[j]`
/// (outs fully overwritten). One parallel pass over the data; per chunk, every
/// source column is streamed once and the p accumulators stay in cache —
/// replaces the per-element j-loop that touched q cache lines per entry.
fn combine(cols: &[Vec<f64>], coeff: &[f64], stride: usize, j0: usize, k0: usize, outs: &mut [Vec<f64>]) {
    let n = cols[0].len();
    let slices: Vec<crate::par::UnsafeSlice<f64>> =
        outs.iter_mut().map(|o| crate::par::UnsafeSlice::new(o)).collect();
    crate::par::for_each_range(n, RR_CHUNK, |s, e| {
        let len = e - s;
        // SAFETY: chunk ranges are disjoint across parallel calls, and each
        // `slice_mut` reborrow is the only live borrow of that column's range.
        for sl in &slices {
            unsafe { sl.slice_mut(s, len) }.fill(0.0);
        }
        for (j, cj) in cols.iter().enumerate().skip(j0) {
            let cj = &cj[s..e];
            for (k, sl) in slices.iter().enumerate() {
                let w = coeff[j * stride + k0 + k];
                if w == 0.0 {
                    continue;
                }
                let o = unsafe { sl.slice_mut(s, len) };
                for i in 0..len {
                    o[i] += w * cj[i];
                }
            }
        }
    });
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
            crate::par::axpy64(&mut cj, -dot, ck);
        }
        let nrm = m_dot(&cj, &cj, m).max(0.0).sqrt();
        if nrm > 1e-150 {
            crate::par::scale64(&mut cj, 1.0 / nrm);
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

/// Expand a compact free-DOF vector into the full node-grid layout `full`
/// (constrained entries of `full` must already be zero and stay zero).
/// Parallel: free_idx entries are distinct, so writes never collide.
#[inline]
fn scatter(free_idx: &[usize], c: &[f64], full: &mut [f64]) {
    let fs = crate::par::UnsafeSlice::new(full);
    crate::par::for_each_range(c.len(), crate::par::CHUNK64, |s, e| {
        for j in s..e {
            // SAFETY: free-DOF indices are unique.
            unsafe { *fs.get_mut(free_idx[j]) = c[j] };
        }
    });
}

/// Gather the free DOFs of a full vector into a compact vector.
#[inline]
fn gather(free_idx: &[usize], full: &[f64], c: &mut [f64]) {
    crate::par::chunks_mut_indexed64(c, crate::par::CHUNK64, |off, cc| {
        for (jj, v) in cc.iter_mut().enumerate() {
            *v = full[free_idx[off + jj]];
        }
    });
}

/// Rayleigh–Ritz on an M-orthonormal block `x` with `kx = K·x`: rotate both to
/// the Ritz basis (ascending eigenvalues) in place and return the eigenvalues.
/// `x` M-orthonormal ⇒ the projected mass is the identity, so this is a plain
/// symmetric eigenproblem on `H = xᵀ K x`.
fn ritz_rotate(x: &mut [Vec<f64>], kx: &mut [Vec<f64>], p: usize) -> Vec<f64> {
    let h = gram_sym(&x[..p], &kx[..p]);
    let (theta, c) = jacobi_eig(&h, p);
    let xo = x.to_vec();
    let kxo = kx.to_vec();
    combine(&xo, &c, p, 0, 0, &mut x[..p]);
    combine(&kxo, &c, p, 0, 0, &mut kx[..p]);
    theta
}

/// Weighted rectangular Gram `g[a·nb+b] = Σᵢ m[i]·colsₐ[i]·colsᵦ[i]` in one
/// blocked parallel pass.
fn gram_w(a: &[Vec<f64>], b: &[Vec<f64>], m: &[f64]) -> Vec<f64> {
    let (na, nb) = (a.len(), b.len());
    let n = m.len();
    crate::par::map_reduce_ranges(
        n,
        RR_CHUNK,
        |s, e| {
            let mut part = vec![0f64; na * nb];
            for ai in 0..na {
                let ac = &a[ai][s..e];
                let mc = &m[s..e];
                for bi in 0..nb {
                    let bc = &b[bi][s..e];
                    let mut acc = 0.0;
                    for i in 0..ac.len() {
                        acc += mc[i] * ac[i] * bc[i];
                    }
                    part[ai * nb + bi] += acc;
                }
            }
            part
        },
        |mut x, y| {
            for (xi, yi) in x.iter_mut().zip(&y) {
                *xi += yi;
            }
            x
        },
        || vec![0f64; na * nb],
    )
}

/// Blocked `outs[k] -= Σⱼ coeff[j·stride + k]·cols[j]` (the projection
/// subtraction of a blocked classical Gram–Schmidt step).
fn combine_sub(cols: &[Vec<f64>], coeff: &[f64], stride: usize, outs: &mut [Vec<f64>]) {
    let n = cols[0].len();
    let slices: Vec<crate::par::UnsafeSlice<f64>> =
        outs.iter_mut().map(|o| crate::par::UnsafeSlice::new(o)).collect();
    crate::par::for_each_range(n, RR_CHUNK, |s, e| {
        let len = e - s;
        for (j, cj) in cols.iter().enumerate() {
            let cj = &cj[s..e];
            for (k, sl) in slices.iter().enumerate() {
                let w = coeff[j * stride + k];
                if w == 0.0 {
                    continue;
                }
                // SAFETY: chunk ranges are disjoint across parallel calls.
                let o = unsafe { sl.slice_mut(s, len) };
                for i in 0..len {
                    o[i] -= w * cj[i];
                }
            }
        }
    });
}

/// Orthonormalize the W/P block against the TRUSTED M-orthonormal X block
/// (one blocked classical-GS projection — X comes straight out of the Ritz
/// rotation, so it is M-orthonormal by construction and is left untouched,
/// which also keeps its K-images exactly valid), then among themselves
/// (modified Gram–Schmidt, DROPPING columns that collapse below tolerance so
/// the LOBPCG basis `[X | W | P]` stays full rank). Returns the kept columns.
fn m_orthonormalize_rest(xb: &[Vec<f64>], mut rest: Vec<Vec<f64>>, m: &[f64]) -> Vec<Vec<f64>> {
    if rest.is_empty() {
        return rest;
    }
    // rest_j -= Σ_k ⟨x_k, rest_j⟩_M · x_k, all columns in two blocked passes.
    let g = gram_w(xb, &rest, m); // g[k·nr + j] = ⟨x_k, rest_j⟩_M
    combine_sub(xb, &g, rest.len(), &mut rest);
    // MGS with drop among the remaining columns.
    let mut kept: Vec<Vec<f64>> = Vec::new();
    for mut cj in rest {
        for k in &kept {
            let dot = m_dot(&cj, k, m);
            crate::par::axpy64(&mut cj, -dot, k);
        }
        let nrm = m_dot(&cj, &cj, m).max(0.0).sqrt();
        if nrm > 1e-7 {
            crate::par::scale64(&mut cj, 1.0 / nrm);
            kept.push(cj);
        }
    }
    kept
}

/// Solve `K v = λ M v` for the lowest `cfg.num_modes` pairs.
///
/// `solver` must already hold the modal stiffness (printed or solid eps).
/// `vfrac` is the per-cell material volume fraction for the lumped mass;
/// `density` is in consistent mass units (tonne/mm³). `extra_mass` are remote
/// point masses as `(node, tonne)` pairs (DESIGN §16) added to the lumped mass
/// diagonal — pass `&[]` when there are none. The returned mode shapes are
/// M-normalized (`vᵀ M v = 1`) — their absolute magnitude is arbitrary, so the
/// viewer normalizes per mode for display.
///
/// `on_progress(outer_done, max_outer, freqs_hz)` is called once per outer
/// subspace-iteration step with the current Ritz frequency estimates — for a
/// live progress bar / convergence readout. Pass a no-op closure to ignore it.
pub fn analyze(
    solver: &mut MgSolver,
    vfrac: &[f32],
    density: f64,
    extra_mass: &[(u32, f64)],
    cfg: &ModalConfig,
    on_progress: impl FnMut(usize, usize, &[f64]),
) -> Result<ModalResult, SolveError> {
    analyze_warm(solver, vfrac, density, extra_mass, cfg, None, on_progress).map(|(r, _)| r)
}

/// [`analyze`] with an optional warm start, returning the converged subspace for
/// the next call.
///
/// `start` is a block from a PREVIOUS analysis of a nearby design. Cold-starting
/// costs ~60-70 LOBPCG iterations on a real part (see the modal design note
/// §12b); a warm block that is already nearly invariant converges in a handful,
/// which is what makes re-analyzing inside an optimizer loop affordable. The
/// block is re-M-orthonormalized against the CURRENT mass before use, so a mass
/// change between calls is handled; only the free-DOF layout must match.
///
/// Missing / surplus columns are tolerated: extra columns are dropped, and a
/// short block is topped up with the same deterministic random vectors a cold
/// start would use, so changing the mode count mid-sequence degrades gracefully
/// instead of failing.
pub fn analyze_warm(
    solver: &mut MgSolver,
    vfrac: &[f32],
    density: f64,
    extra_mass: &[(u32, f64)],
    cfg: &ModalConfig,
    start: Option<&ModalBlock>,
    mut on_progress: impl FnMut(usize, usize, &[f64]),
) -> Result<(ModalResult, ModalBlock), SolveError> {
    let n = solver.levels[0].ndof();
    let m = lumped_mass(solver, vfrac, density, extra_mass);
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
    // Warm columns first (previous design's converged subspace), then the
    // deterministic random fill — so a short or absent block still yields
    // exactly `p` columns, and a cold start is bit-identical to before.
    let warm: &[Vec<f64>] = match start {
        Some(b) if b.nf == nf => &b.cols,
        _ => &[],
    };
    let mut x: Vec<Vec<f64>> = (0..p)
        .map(|j| match warm.get(j) {
            Some(c) => c.clone(),
            None => {
                let v = random_vec(n, &constrained, j as u32 + 1);
                free_idx.iter().map(|&i| v[i]).collect()
            }
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
    // The eigenVALUES (frequencies — what the user wants) converge well before
    // the eigenVECTOR residual, especially when the multigrid preconditioner is
    // weak (slender/thin parts). So stop when the requested frequencies stop
    // moving, not only when the residual is tiny. 4-figure relative frequency
    // agreement is far more than enough (the model is uncalibrated anyway).
    let mut prev_theta = vec![f64::INFINITY; num_modes];
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
            {
                let (kxk, xk, th) = (&kx[k], &x[k], theta[k]);
                crate::par::chunks_mut_indexed64(&mut r, crate::par::CHUNK64, |off, rc| {
                    for (t, ri) in rc.iter_mut().enumerate() {
                        let i = off + t;
                        *ri = kxk[i] - th * mc[i] * xk[i];
                    }
                });
            }
            if k < num_modes {
                // Relative residual of the requested (non-guard) modes.
                max_rel =
                    max_rel.max(crate::par::norm2_64(&r) / crate::par::norm2_64(&kx[k]).max(1e-300));
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
            let mut w = vec![0f64; nf];
            gather(&free_idx, &full_out, &mut w);
            total_inner_iters += PRECOND_CYCLES;
            wblk.push(w);
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
        if max_rel < cfg.tol || (it >= 2 && eig_change < cfg.eig_tol) {
            converged = true;
            break;
        }
        // Rayleigh–Ritz over S = [X | W | P]. X comes out of the Ritz rotation
        // M-orthonormal by construction, so it enters the basis untouched and
        // its K-images (kx) stay exactly valid — only the kept W/P columns
        // need orthonormalization and a fresh K-apply.
        let nx = x.len();
        let mut rest: Vec<Vec<f64>> = Vec::with_capacity(2 * p);
        rest.extend(wblk);
        rest.append(&mut pblk);
        let rest = m_orthonormalize_rest(&x, rest, &mc);
        let mut basis = std::mem::take(&mut x);
        let mut kb = std::mem::take(&mut kx);
        for col in rest {
            let mut o = vec![0f64; nf];
            apply_kc!(&col, &mut o);
            basis.push(col);
            kb.push(o);
        }
        let q = basis.len();
        // Projected stiffness SK = Sᵀ K S (Sᵀ M S = I); smallest p eigenpairs.
        let sk = gram_sym(&basis, &kb);
        let (th_all, cmat) = jacobi_eig(&sk, q);
        // New X / KX = basis · C[:,0:p]; P = the [W,P]-part of that combination
        // (the "locally optimal" conjugate direction).
        let mut xnew = vec![vec![0f64; nf]; p];
        let mut kxnew = vec![vec![0f64; nf]; p];
        let mut pnew = vec![vec![0f64; nf]; p];
        combine(&basis, &cmat, q, 0, 0, &mut xnew);
        combine(&kb, &cmat, q, 0, 0, &mut kxnew);
        combine(&basis, &cmat, q, nx, 0, &mut pnew);
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

    let mut freqs_hz = Vec::with_capacity(num_modes);
    let mut lambdas = Vec::with_capacity(num_modes);
    let mut shapes = Vec::with_capacity(num_modes);
    for k in 0..num_modes {
        // λ = ω²; clamp tiny negatives (round-off / near-rigid modes) to 0.
        let lambda = theta[k].max(0.0);
        freqs_hz.push(lambda.sqrt() / TAU);
        lambdas.push(lambda);
        shapes.push(std::mem::take(&mut x_full[k]));
    }
    // The compact block (guard columns included) becomes the next call's warm
    // start; it is M-orthonormal in the CURRENT mass, which is the right basis
    // for a design one move-limit step away.
    let block = ModalBlock { cols: x, nf };
    Ok((
        ModalResult { freqs_hz, lambdas, shapes, outer_iters: iters, total_inner_iters, converged },
        block,
    ))
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
        let problem = NodeProblem { fixed, springs: Vec::new(), forces: Vec::new(), rigid: Vec::new(), prescribed: Vec::new() };
        let eps = grid_eps(&grid);
        let mut cache = SolverCache::build(&grid, levels, &problem, &s, eps);
        let cfg = ModalConfig::new(num_modes);
        let res = analyze(&mut cache.solver, &grid.scale, density, &[], &cfg, |_, _, _| {}).unwrap();
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

    #[test]
    fn tip_point_mass_lowers_frequency() {
        // A remote point mass bolted to the free tip must drag the fundamental
        // frequency down (f ∝ 1/√M_eff). Rayleigh estimate with a tip mass M and
        // beam mass m_b: f_M/f_bare = √( 0.2357·m_b / (M + 0.2357·m_b) ), where
        // 33/140 ≈ 0.2357 is the cantilever's tip-effective self-mass. Choosing
        // M = m_b predicts ≈0.437 — this guards the modal mass wiring (a broken
        // one leaves the ratio at 1.0; a unit slip pushes it far off).
        let (nxc, nthick, h) = (40usize, 4usize, 1.0);
        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };
        let density = 1.24e-9; // PLA, tonne/mm³
        let raw = beam(nxc, nthick, nthick, h);
        let (grid, levels) = pad_for_levels(&raw, s.max_levels);
        let active = active_nodes(&grid);
        let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
        // Root plane (x index 0) fixed; free tip plane (x index nxc) takes the mass.
        let (mut fixed, mut tip) = (Vec::new(), Vec::new());
        for z in 0..mz {
            for y in 0..my {
                let root = (z * my + y) * mx;
                if active[root] {
                    fixed.push(root as u32);
                }
                let t = (z * my + y) * mx + nxc;
                if t < active.len() && active[t] {
                    tip.push(t as u32);
                }
            }
        }
        let problem = NodeProblem { fixed, springs: Vec::new(), forces: Vec::new(), rigid: Vec::new(), prescribed: Vec::new() };
        let mut cache = SolverCache::build(&grid, levels, &problem, &s, grid_eps(&grid));
        let cfg = ModalConfig::new(1);

        let beam_mass = density * (nxc as f64 * h) * (nthick as f64 * h).powi(2);
        let f_bare = analyze(&mut cache.solver, &grid.scale, density, &[], &cfg, |_, _, _| {})
            .unwrap()
            .freqs_hz[0];
        // Tip mass M = beam mass, split equally over the tip-plane nodes.
        let m_tip = beam_mass;
        let per = m_tip / tip.len() as f64;
        let extra: Vec<(u32, f64)> = tip.iter().map(|&n| (n, per)).collect();
        let f_load = analyze(&mut cache.solver, &grid.scale, density, &extra, &cfg, |_, _, _| {})
            .unwrap()
            .freqs_hz[0];

        assert!(f_load < f_bare, "tip mass must lower f1: bare {f_bare}, loaded {f_load}");
        let ratio = f_load / f_bare;
        let pred = (0.2357 * beam_mass / (m_tip + 0.2357 * beam_mass)).sqrt();
        assert!(
            (ratio - pred).abs() < 0.15,
            "f1 ratio {ratio:.3} vs Rayleigh {pred:.3} out of band (bare {f_bare:.1}, loaded {f_load:.1} Hz)"
        );
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
        let problem = NodeProblem { fixed, springs: Vec::new(), forces: Vec::new(), rigid: Vec::new(), prescribed: Vec::new() };
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
        let res = analyze(&mut cache.solver, &grid.scale, 1.24e-9, &[], &cfg, |_, _, _| {}).unwrap();
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
        let problem = NodeProblem { fixed: Vec::new(), springs, forces: Vec::new(), rigid: Vec::new(), prescribed: Vec::new() };
        let mut cache = SolverCache::build(&grid, levels, &problem, &s, grid_eps(&grid));
        let cfg = ModalConfig::new(1 + 6); // 6 rigid-body + 1 flexible
        let res = analyze(&mut cache.solver, &grid.scale, density, &[], &cfg, |_, _, _| {}).unwrap();
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

    /// M0 go/no-go for the frequency objective (DESIGN §26): re-analyzing a
    /// design that moved by one move-limited OC step must be much cheaper than
    /// a cold analysis, or maximizing f1 inside the optimizer loop is not
    /// affordable in a browser. Mimics the optimizer exactly — same design
    /// split, same `build_eps`/`build_vfrac`, same ±0.1 move limit — and runs a
    /// cold and a warm analysis at each step so the two are directly comparable
    /// on identical designs.
    ///
    /// Also a CORRECTNESS test: warm and cold must land on the same
    /// frequencies. A warm start that quietly converged to the wrong subspace
    /// (e.g. locking onto a stale mode after a crossing) would show up here.
    ///
    /// `cargo test -p filasim-core --lib --release warm_start_beats_cold -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn warm_start_beats_cold_on_a_moving_design() {
        use crate::simp::{build_eps, build_vfrac, design_split, LoadSet, OptimizeParams};

        let s = SolveSettings { e0: 2400.0, nu: 0.35, ..Default::default() };
        let density = 1.24e-9;
        let (nxc, nthick, h) = (60usize, 10usize, 1.0);
        let raw = beam(nxc, nthick, nthick, h);
        let (grid, levels) = pad_for_levels(&raw, s.max_levels);
        let active = active_nodes(&grid);
        let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
        let mut fixed = Vec::new();
        for z in 0..mz {
            for y in 0..my {
                let n = (z * my + y) * mx;
                if active[n] {
                    fixed.push(n as u32);
                }
            }
        }
        let problem =
            NodeProblem { fixed, springs: Vec::new(), forces: Vec::new(), rigid: Vec::new(), prescribed: Vec::new() };

        let params = OptimizeParams::default();
        let split = design_split(&grid, &params, &problem, &LoadSet::default());
        let (skin, design, skin_frac) = (split.skin, split.design, split.skin_frac);
        let mut x = vec![0.25f64; design.len()];
        let mk_eps = |x: &[f64]| {
            build_eps(&grid, &skin, &design, &skin_frac, x, params.exponent, params.coeff)
        };
        let mk_vf = |x: &[f64]| build_vfrac(&grid, &design, &skin_frac, x);

        let mut cache = SolverCache::build(&grid, levels, &problem, &s, mk_eps(&x));
        let cfg = ModalConfig::new(4); // p = 4 + guard 3 = 7 columns
        eprintln!(
            "grid {}x{}x{} = {} cells, {} design cells, block p=7",
            grid.nx,
            grid.ny,
            grid.nz,
            grid.cell_count(),
            design.len()
        );

        // Seed the warm chain with one cold analysis (what iteration 0 pays).
        let (seed, mut block) =
            analyze_warm(&mut cache.solver, &mk_vf(&x), density, &[], &cfg, None, |_, _, _| {})
                .unwrap();
        eprintln!(
            "iter  0  COLD {:>4} V-cycles ({:>3} iters)  f1 = {:8.2} Hz   [seed]",
            seed.total_inner_iters, seed.outer_iters, seed.freqs_hz[0]
        );

        let (mut cold_total, mut warm_total) = (0usize, 0usize);
        for it in 1..=6 {
            // One move-limited OC-sized step: a deterministic, spatially smooth
            // ±0.1 perturbation — the scale of change the optimizer actually
            // makes between iterations.
            for (k, xv) in x.iter_mut().enumerate() {
                let c = design[k] as usize;
                let (cx, cy) = ((c % grid.nx) as f64, ((c / grid.nx) % grid.ny) as f64);
                let d = 0.1 * ((0.21 * cx + 0.37 * cy + 0.9 * it as f64).sin());
                *xv = (*xv + d).clamp(params.floor, params.cap);
            }
            cache.solver.update_eps(mk_eps(&x));
            let vf = mk_vf(&x);

            let (cold, _) =
                analyze_warm(&mut cache.solver, &vf, density, &[], &cfg, None, |_, _, _| {}).unwrap();
            let (warm, next) =
                analyze_warm(&mut cache.solver, &vf, density, &[], &cfg, Some(&block), |_, _, _| {})
                    .unwrap();
            block = next;
            cold_total += cold.total_inner_iters;
            warm_total += warm.total_inner_iters;

            eprintln!(
                "iter {:>2}  COLD {:>4} V-cycles ({:>3} iters)  f1 = {:8.2} Hz   \
                 WARM {:>4} V-cycles ({:>3} iters)  f1 = {:8.2} Hz   speedup {:.1}x",
                it,
                cold.total_inner_iters,
                cold.outer_iters,
                cold.freqs_hz[0],
                warm.total_inner_iters,
                warm.outer_iters,
                warm.freqs_hz[0],
                cold.total_inner_iters as f64 / warm.total_inner_iters.max(1) as f64,
            );

            // Same design ⇒ same spectrum, whichever way we started.
            for k in 0..cfg.num_modes {
                let (a, b) = (cold.freqs_hz[k], warm.freqs_hz[k]);
                let rel = (a - b).abs() / a.max(1e-12);
                assert!(
                    rel < 0.02,
                    "mode {k}: warm {b:.3} Hz vs cold {a:.3} Hz disagree by {:.2}%",
                    rel * 100.0
                );
            }
        }

        let speedup = cold_total as f64 / warm_total.max(1) as f64;
        eprintln!(
            "\nTOTAL over 6 steps: cold {cold_total} V-cycles, warm {warm_total} V-cycles \
             — {speedup:.1}x\nper-iteration warm cost: {:.0} V-cycles",
            warm_total as f64 / 6.0
        );
        // The whole feature rests on this: if a warm re-analysis is not
        // dramatically cheaper, the objective is not viable in the browser.
        assert!(
            speedup > 2.0,
            "warm start only {speedup:.1}x cheaper than cold — frequency objective not viable"
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
