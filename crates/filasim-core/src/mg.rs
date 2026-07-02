// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Geometric multigrid preconditioned CG (MGCG) for voxel-grid elasticity.
//!
//! - Matrix-free: one reference KE per level, scaled per cell by a stiffness
//!   factor; cells are processed in 8 parity colors so scatter-adds never race.
//! - Smoother: Chebyshev polynomial over block-Jacobi (3x3 node blocks).
//!   A fixed polynomial in Dinv*K is a constant symmetric operator, so the
//!   V-cycle remains a valid SPD preconditioner for CG — but it damps the
//!   upper spectrum far better than a fixed-weight Jacobi sweep, which is
//!   what cuts MGCG iterations on high-contrast (thin shell + soft infill)
//!   grids. Cost per step is identical (one apply + one Dinv).
//! - Coarsening: rediscretization with averaged cell stiffness (KE scales
//!   linearly with h on cube cells), trilinear prolongation, restriction = P^T.
//! - Dirichlet/inactive DOFs are masked: vectors stay zero there throughout.

use crate::eps::average_coarse_eps;
use crate::fem::{invert3, ke_diag_blocks, NODE_OFFSETS};
use crate::par::{self, UnsafeSlice};

pub const NU1: usize = 3;
pub const NU2: usize = 3;
/// Chebyshev smoothing interval: [lmax/CHEB_EIG_RATIO, lmax]. Smaller ratio =
/// gentler polynomial; larger targets more of the spectrum per sweep.
const CHEB_EIG_RATIO: f32 = 8.0;
/// Safety headroom on the power-iteration lmax estimate (an UNDER-estimated
/// lmax makes Chebyshev amplify the top modes, which diverges). Power
/// iteration approaches lmax from below, so headroom is mandatory.
const CHEB_LMAX_SAFETY: f32 = 1.1;
/// Power-iteration counts: cold start vs warm restart from the stored
/// eigenvector (the SIMP loop refreshes eps every iteration — tiny spectral
/// shifts, so a few warm steps suffice).
const LMAX_ITERS_COLD: usize = 8;
const LMAX_ITERS_WARM: usize = 3;
const NODE_CHUNK: usize = 4096;

/// Map (dx,dy,dz) in {0,1}^3 to the local hex node index.
const OFF_TO_LOCAL: [[[usize; 2]; 2]; 2] = {
    let mut m = [[[0usize; 2]; 2]; 2];
    m[0][0][0] = 0;
    m[1][0][0] = 1;
    m[1][1][0] = 2;
    m[0][1][0] = 3;
    m[0][0][1] = 4;
    m[1][0][1] = 5;
    m[1][1][1] = 6;
    m[0][1][1] = 7;
    m
};

pub struct Level {
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub mx: usize,
    pub my: usize,
    pub mz: usize,
    pub h: f64,
    ke: [[f32; 24]; 24],
    ke64: [[f64; 24]; 24],
    /// Per-cell stiffness factor; exactly 0.0 = cell skipped entirely.
    pub eps: Vec<f32>,
    /// Non-void cells grouped into contiguous z-slab ranges for two-phase
    /// parallel scatter: even-indexed slabs run concurrently (phase A), then
    /// odd (phase B). Every slab spans ≥1 whole z-plane of cells, so two
    /// same-phase slabs are ≥2 cells apart in z and never share nodes. Cells
    /// are natural-order within a slab and slab cuts are balanced by active-
    /// cell count — one contiguous sweep per thread instead of the former 8
    /// stride-2 parity-color passes (2 joins per apply instead of 8, and far
    /// better cache locality).
    slabs: Vec<Vec<u32>>,
    /// Per-DOF mask: Dirichlet-fixed or inactive (no solid neighbor cell).
    pub constrained: Vec<bool>,
    /// Per-node inverted 3x3 diagonal block, stored SYMMETRIC-packed as 6
    /// floats [d00,d01,d02,d11,d12,d22] (the block is symmetric; `invert3` of
    /// a symmetric matrix is bitwise-symmetric since the paired cofactors are
    /// identical expressions). Zeroed at constrained rows/cols. 6/9 of the
    /// memory — this is the dominant stream of the smoother.
    dinv: Vec<f32>,
    /// Penalty springs (node, unit direction, stiffness N/mm) — frictionless supports.
    springs: Vec<(u32, [f64; 3], f64)>,
    /// Largest eigenvalue estimate of Dinv*K (power iteration), the Chebyshev
    /// smoothing interval's upper end. Refreshed on every eps update.
    lmax: f32,
    /// Last power-iteration eigenvector — warm restart across eps updates.
    eigvec: Vec<f32>,
    /// Reusable power-iteration scratch (K*v and Dinv*K*v). Hoisted out of
    /// `refresh_lmax` so an eps update — once per SIMP iteration, per level —
    /// no longer allocates 2×ndof f32 it immediately frees.
    pi_t: Vec<f32>,
    pi_w: Vec<f32>,
}

impl Level {
    pub fn node_count(&self) -> usize {
        self.mx * self.my * self.mz
    }

    pub fn ndof(&self) -> usize {
        3 * self.node_count()
    }

    #[inline]
    pub fn node_index(&self, x: usize, y: usize, z: usize) -> usize {
        (z * self.my + y) * self.mx + x
    }

    /// Build a level from per-cell stiffness factors and per-DOF fixed flags.
    /// `fixed` marks user Dirichlet DOFs; inactive DOFs are added internally.
    pub fn new(
        nx: usize,
        ny: usize,
        nz: usize,
        h: f64,
        eps: Vec<f32>,
        ke64: [[f64; 24]; 24],
        fixed: &[bool],
        springs: Vec<(u32, [f64; 3], f64)>,
    ) -> Self {
        assert_eq!(eps.len(), nx * ny * nz);
        let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
        let ndof = 3 * mx * my * mz;
        assert_eq!(fixed.len(), ndof);

        let mut ke = [[0f32; 24]; 24];
        for i in 0..24 {
            for j in 0..24 {
                ke[i][j] = ke64[i][j] as f32;
            }
        }

        let mut level = Self {
            nx,
            ny,
            nz,
            mx,
            my,
            mz,
            h,
            ke,
            ke64,
            eps,
            slabs: Vec::new(),
            constrained: vec![false; ndof],
            dinv: Vec::new(),
            springs,
            lmax: 1.0,
            eigvec: Vec::new(),
            pi_t: Vec::new(),
            pi_w: Vec::new(),
        };
        level.build_slabs();
        level.build_constrained(fixed);
        level.build_dinv();
        level.refresh_lmax();
        level
    }

    /// Power-iteration estimate of the largest eigenvalue of Dinv*K. Must be
    /// re-run whenever eps (and hence K and Dinv) changes; warm-restarts from
    /// the previous eigenvector when one exists.
    fn refresh_lmax(&mut self) {
        let n = self.ndof();
        let mut v = std::mem::take(&mut self.eigvec);
        let iters = if v.len() == n {
            LMAX_ITERS_WARM
        } else {
            // Deterministic pseudo-random start so solves are reproducible.
            v = vec![0f32; n];
            for (i, vi) in v.iter_mut().enumerate() {
                let mut x = (i as u32).wrapping_mul(2654435761).wrapping_add(40503);
                x ^= x >> 13;
                x = x.wrapping_mul(0x9E37_79B1);
                x ^= x >> 16;
                *vi = x as f32 / u32::MAX as f32 - 0.5;
            }
            LMAX_ITERS_COLD
        };
        par::mask_zero(&mut v, &self.constrained);
        // Reuse the scratch buffers across eps updates (apply/diag_apply fully
        // overwrite them; the zero-resize just sizes the reused allocation).
        let mut t = std::mem::take(&mut self.pi_t);
        let mut w = std::mem::take(&mut self.pi_w);
        t.clear();
        t.resize(n, 0.0);
        w.clear();
        w.resize(n, 0.0);
        let mut lambda = 1.0f64;
        for _ in 0..iters {
            self.apply(&v, &mut t);
            self.diag_apply(&t, &mut w);
            let nw = par::norm2(&w);
            if !(nw > 0.0) {
                break;
            }
            lambda = nw;
            par::axpby(&mut v, 0.0, (1.0 / nw) as f32, &w);
        }
        self.lmax = lambda as f32;
        self.eigvec = v;
        self.pi_t = t;
        self.pi_w = w;
    }

    /// Rediscretized coarse level: half resolution, child-averaged stiffness.
    pub fn coarsen(&self) -> Self {
        let eps = average_coarse_eps(&self.eps, self.nx, self.ny, self.nz);
        let (nx, ny, nz) = (self.nx / 2, self.ny / 2, self.nz / 2);
        // KE scales linearly with h for cube cells.
        let mut ke64 = self.ke64;
        for i in 0..24 {
            for j in 0..24 {
                ke64[i][j] *= 2.0;
            }
        }
        // Inject fine Dirichlet flags at coincident nodes (2X,2Y,2Z).
        let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
        let mut fixed = vec![false; 3 * mx * my * mz];
        for z in 0..mz {
            for y in 0..my {
                for x in 0..mx {
                    let nf = self.node_index(2 * x, 2 * y, 2 * z);
                    let nc = (z * my + y) * mx + x;
                    for d in 0..3 {
                        fixed[3 * nc + d] = self.constrained[3 * nf + d];
                    }
                }
            }
        }
        // Springs move to the nearest coarse node (keeps the penalty visible
        // to the preconditioner; exactness is not required there).
        let springs = self
            .springs
            .iter()
            .map(|&(n, dir, k)| {
                let n = n as usize;
                let x = ((n % self.mx + 1) / 2).min(mx - 1);
                let y = ((n / self.mx % self.my + 1) / 2).min(my - 1);
                let z = ((n / (self.mx * self.my) + 1) / 2).min(mz - 1);
                (((z * my + y) * mx + x) as u32, dir, k)
            })
            .collect();
        Self::new(nx, ny, nz, self.h * 2.0, eps, ke64, &fixed, springs)
    }

    /// Cut the active cells into z-slabs balanced by active-cell count (cuts
    /// only at whole z-plane boundaries — the two-phase safety invariant).
    /// Depends only on the void set, which `update_eps` never changes, so it
    /// is built once per level.
    fn build_slabs(&mut self) {
        let plane_cells = self.nx * self.ny;
        let mut plane = vec![0usize; self.nz];
        for (cz, cnt) in plane.iter_mut().enumerate() {
            *cnt = self.eps[cz * plane_cells..(cz + 1) * plane_cells]
                .iter()
                .filter(|&&e| e > 0.0)
                .count();
        }
        let total: usize = plane.iter().sum();
        // ~4 slabs per thread in each phase for load balance; ≥1 plane per slab.
        #[cfg(feature = "parallel")]
        let want = (8 * rayon::current_num_threads()).clamp(1, self.nz.max(1));
        #[cfg(not(feature = "parallel"))]
        let want = 1;
        let per = (total / want).max(1);
        let mut slabs: Vec<Vec<u32>> = Vec::new();
        let mut cur: Vec<u32> = Vec::new();
        let mut acc = 0usize;
        for cz in 0..self.nz {
            for i in cz * plane_cells..(cz + 1) * plane_cells {
                if self.eps[i] > 0.0 {
                    cur.push(i as u32);
                }
            }
            acc += plane[cz];
            if acc >= per && slabs.len() + 1 < want {
                slabs.push(std::mem::take(&mut cur));
                acc = 0;
            }
        }
        if !cur.is_empty() {
            slabs.push(cur);
        }
        self.slabs = slabs;
    }

    fn build_constrained(&mut self, fixed: &[bool]) {
        // Active node = at least one incident non-void cell.
        for z in 0..self.mz {
            for y in 0..self.my {
                for x in 0..self.mx {
                    let n = self.node_index(x, y, z);
                    let mut active = false;
                    for dz in 0..2usize {
                        for dy in 0..2usize {
                            for dx in 0..2usize {
                                if dx > x || dy > y || dz > z {
                                    continue;
                                }
                                let (cx, cy, cz) = (x - dx, y - dy, z - dz);
                                if cx < self.nx && cy < self.ny && cz < self.nz {
                                    let ci = (cz * self.ny + cy) * self.nx + cx;
                                    if self.eps[ci] > 0.0 {
                                        active = true;
                                    }
                                }
                            }
                        }
                    }
                    for d in 0..3 {
                        self.constrained[3 * n + d] = fixed[3 * n + d] || !active;
                    }
                }
            }
        }
    }

    fn build_dinv(&mut self) {
        let blocks = ke_diag_blocks(&self.ke64);
        let mut spring_blocks: std::collections::HashMap<u32, [[f64; 3]; 3]> = Default::default();
        for &(n, dir, k) in &self.springs {
            let e = spring_blocks.entry(n).or_insert([[0.0; 3]; 3]);
            for r in 0..3 {
                for c in 0..3 {
                    e[r][c] += k * dir[r] * dir[c];
                }
            }
        }
        let mut dinv = vec![0f32; 6 * self.node_count()];
        let (nx, ny, nz) = (self.nx, self.ny, self.nz);
        let (mx, my) = (self.mx, self.my);
        let eps = &self.eps;
        let constrained = &self.constrained;
        par::chunks_mut_indexed(&mut dinv, 6 * NODE_CHUNK, |off, chunk| {
            let n0 = off / 6;
            for (k, blk) in chunk.chunks_mut(6).enumerate() {
                let n = n0 + k;
                let x = n % mx;
                let y = (n / mx) % my;
                let z = n / (mx * my);
                let mut acc = [[0f64; 3]; 3];
                let mut any = false;
                for dz in 0..2usize {
                    for dy in 0..2usize {
                        for dx in 0..2usize {
                            if dx > x || dy > y || dz > z {
                                continue;
                            }
                            let (cx, cy, cz) = (x - dx, y - dy, z - dz);
                            if cx >= nx || cy >= ny || cz >= nz {
                                continue;
                            }
                            let e = eps[(cz * ny + cy) * nx + cx];
                            if e <= 0.0 {
                                continue;
                            }
                            any = true;
                            let l = OFF_TO_LOCAL[dx][dy][dz];
                            for r in 0..3 {
                                for c in 0..3 {
                                    acc[r][c] += e as f64 * blocks[l][r][c];
                                }
                            }
                        }
                    }
                }
                if !any {
                    continue; // stays zero
                }
                if let Some(sb) = spring_blocks.get(&(n as u32)) {
                    for r in 0..3 {
                        for c in 0..3 {
                            acc[r][c] += sb[r][c];
                        }
                    }
                }
                // Reduce out constrained DOFs of this node before inverting.
                let mut anyfree = false;
                for d in 0..3 {
                    if constrained[3 * n + d] {
                        for k2 in 0..3 {
                            acc[d][k2] = 0.0;
                            acc[k2][d] = 0.0;
                        }
                        acc[d][d] = 1.0;
                    } else {
                        anyfree = true;
                    }
                }
                if !anyfree {
                    continue;
                }
                if let Some(inv) = invert3(&acc) {
                    // Symmetric-packed upper triangle [00,01,02,11,12,22].
                    for (slot, (r, c)) in
                        [(0, 0), (0, 1), (0, 2), (1, 1), (1, 2), (2, 2)].into_iter().enumerate()
                    {
                        blk[slot] = if constrained[3 * n + r] || constrained[3 * n + c] {
                            0.0
                        } else {
                            inv[r][c] as f32
                        };
                    }
                }
            }
        });
        self.dinv = dinv;
    }

    /// One cell's scatter-add contribution to y = K x.
    /// # Safety
    /// Concurrent callers must not target cells sharing nodes (color rule);
    /// sequential callers are always fine.
    #[inline(always)]
    unsafe fn apply_cell(&self, ci: usize, e: f32, x: &[f32], ys: &UnsafeSlice<f32>) {
        let (nx, ny) = (self.nx, self.ny);
        let (mx, my) = (self.mx, self.my);
        let cx = ci % nx;
        let cy = (ci / nx) % ny;
        let cz = ci / (nx * ny);
        let mut xl = [0f32; 24];
        let mut nidx = [0usize; 8];
        for l in 0..8 {
            let [ox, oy, oz] = NODE_OFFSETS[l];
            let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
            nidx[l] = n;
            xl[3 * l] = x[3 * n];
            xl[3 * l + 1] = x[3 * n + 1];
            xl[3 * l + 2] = x[3 * n + 2];
        }
        // Row-reduction form: measured FASTER than the column-axpy
        // variant under simd128 (LLVM SLP-vectorizes across rows).
        let mut yl = [0f32; 24];
        for i in 0..24 {
            let row = &self.ke[i];
            let mut s = 0f32;
            for j in 0..24 {
                s += row[j] * xl[j];
            }
            yl[i] = e * s;
        }
        for l in 0..8 {
            let n = nidx[l];
            *ys.get_mut(3 * n) += yl[3 * l];
            *ys.get_mut(3 * n + 1) += yl[3 * l + 1];
            *ys.get_mut(3 * n + 2) += yl[3 * l + 2];
        }
    }

    /// y = K x (masked at constrained DOFs). x must be zero at constrained DOFs.
    pub fn apply(&self, x: &[f32], y: &mut [f32]) {
        debug_assert_eq!(x.len(), self.ndof());
        debug_assert_eq!(y.len(), self.ndof());
        par::fill(y, 0.0);
        {
            let ys = UnsafeSlice::new(y);
            // Threaded: two phases of z-slabs; same-phase slabs never share nodes.
            #[cfg(feature = "parallel")]
            for phase in 0..2 {
                let slabs: Vec<&Vec<u32>> = self.slabs.iter().skip(phase).step_by(2).collect();
                // SAFETY: same-phase slabs are ≥2 cells apart in z.
                par::for_each(&slabs, |slab| {
                    for &ci in slab.iter() {
                        unsafe { self.apply_cell(ci as usize, self.eps[ci as usize], x, &ys) };
                    }
                });
            }
            // Sequential: no races possible — one natural-order pass.
            #[cfg(not(feature = "parallel"))]
            for (ci, &e) in self.eps.iter().enumerate() {
                if e > 0.0 {
                    // SAFETY: sequential.
                    unsafe { self.apply_cell(ci, e, x, &ys) };
                }
            }
        }
        for &(n, dir, k) in &self.springs {
            let n = n as usize;
            let s = k
                * (dir[0] * x[3 * n] as f64
                    + dir[1] * x[3 * n + 1] as f64
                    + dir[2] * x[3 * n + 2] as f64);
            for d in 0..3 {
                y[3 * n + d] += (s * dir[d]) as f32;
            }
        }
        par::mask_zero(y, &self.constrained);
    }

    /// f64 twin of `apply_cell` (outer-CG operator).
    /// # Safety
    /// Same disjoint-scatter rule as `apply_cell`.
    #[inline(always)]
    unsafe fn apply_cell64(&self, ci: usize, e: f64, x: &[f64], ys: &UnsafeSlice<f64>) {
        let (nx, ny) = (self.nx, self.ny);
        let (mx, my) = (self.mx, self.my);
        let cx = ci % nx;
        let cy = (ci / nx) % ny;
        let cz = ci / (nx * ny);
        let mut xl = [0f64; 24];
        let mut nidx = [0usize; 8];
        for l in 0..8 {
            let [ox, oy, oz] = NODE_OFFSETS[l];
            let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
            nidx[l] = n;
            xl[3 * l] = x[3 * n];
            xl[3 * l + 1] = x[3 * n + 1];
            xl[3 * l + 2] = x[3 * n + 2];
        }
        let mut yl = [0f64; 24];
        for i in 0..24 {
            let row = &self.ke64[i];
            let mut s = 0f64;
            for j in 0..24 {
                s += row[j] * xl[j];
            }
            yl[i] = e * s;
        }
        for l in 0..8 {
            let n = nidx[l];
            *ys.get_mut(3 * n) += yl[3 * l];
            *ys.get_mut(3 * n + 1) += yl[3 * l + 1];
            *ys.get_mut(3 * n + 2) += yl[3 * l + 2];
        }
    }

    /// y = K x in f64 (used by the outer CG; the cancellation in K·u near
    /// equilibrium exceeds f32 precision, which caps attainable accuracy).
    pub fn apply64(&self, x: &[f64], y: &mut [f64]) {
        self.apply64_eps(&self.eps, x, y);
    }

    /// `apply64` with an explicit per-cell stiffness field: the outer CG runs
    /// on the EXACT eps while the level hierarchy (preconditioner only) may
    /// hold contrast-clamped values. `eps` must share this level's void set.
    pub fn apply64_eps(&self, eps: &[f32], x: &[f64], y: &mut [f64]) {
        debug_assert_eq!(x.len(), self.ndof());
        debug_assert_eq!(y.len(), self.ndof());
        debug_assert_eq!(eps.len(), self.eps.len());
        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            y.par_chunks_mut(1 << 14).for_each(|c| c.fill(0.0));
        }
        #[cfg(not(feature = "parallel"))]
        y.fill(0.0);
        {
            let ys = UnsafeSlice::new(y);
            #[cfg(feature = "parallel")]
            for phase in 0..2 {
                let slabs: Vec<&Vec<u32>> = self.slabs.iter().skip(phase).step_by(2).collect();
                // SAFETY: same-phase slabs are ≥2 cells apart in z.
                par::for_each(&slabs, |slab| {
                    for &ci in slab.iter() {
                        unsafe {
                            self.apply_cell64(ci as usize, eps[ci as usize] as f64, x, &ys)
                        };
                    }
                });
            }
            #[cfg(not(feature = "parallel"))]
            for (ci, &e) in eps.iter().enumerate() {
                if e > 0.0 {
                    // SAFETY: sequential.
                    unsafe { self.apply_cell64(ci, e as f64, x, &ys) };
                }
            }
        }
        for &(n, dir, k) in &self.springs {
            let n = n as usize;
            let s = k * (dir[0] * x[3 * n] + dir[1] * x[3 * n + 1] + dir[2] * x[3 * n + 2]);
            for d in 0..3 {
                y[3 * n + d] += s * dir[d];
            }
        }
        // Mask constrained DOFs.
        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            y.par_chunks_mut(1 << 14)
                .zip(self.constrained.par_chunks(1 << 14))
                .for_each(|(yc, mc)| {
                    for (yi, m) in yc.iter_mut().zip(mc) {
                        if *m {
                            *yi = 0.0;
                        }
                    }
                });
        }
        #[cfg(not(feature = "parallel"))]
        for (yi, m) in y.iter_mut().zip(&self.constrained) {
            if *m {
                *yi = 0.0;
            }
        }
    }

    /// Fused Chebyshev step: per node `w = Dinv·(r − t); d = a·d + b·w;
    /// z += d` in ONE pass (element-wise identical to the former
    /// dinv_residual → axpby → axpy sequence, which made three). The smoother
    /// is bandwidth-bound and dinv is its dominant stream, so fewer passes is
    /// the whole game.
    fn cheb_step_fused(&self, z: &mut [f32], d: &mut [f32], r: &[f32], t: &[f32], a: f32, b: f32) {
        let dinv = &self.dinv;
        par::chunks2_mut_indexed(z, d, 3 * NODE_CHUNK, |off, zc, dc| {
            let n0 = off / 3;
            for (k, (zn, dn)) in zc.chunks_mut(3).zip(dc.chunks_mut(3)).enumerate() {
                let n = n0 + k;
                let di = &dinv[6 * n..6 * n + 6];
                let rr =
                    [r[3 * n] - t[3 * n], r[3 * n + 1] - t[3 * n + 1], r[3 * n + 2] - t[3 * n + 2]];
                let w = [
                    di[0] * rr[0] + di[1] * rr[1] + di[2] * rr[2],
                    di[1] * rr[0] + di[3] * rr[1] + di[4] * rr[2],
                    di[2] * rr[0] + di[4] * rr[1] + di[5] * rr[2],
                ];
                for c in 0..3 {
                    dn[c] = a * dn[c] + b * w[c];
                    zn[c] += dn[c];
                }
            }
        });
    }

    /// Fused zero-guess first step: `z = d = f·Dinv·r` in one pass
    /// (replaces fill + diag_apply + axpby + axpy).
    fn cheb_first_fused(&self, z: &mut [f32], d: &mut [f32], r: &[f32], f: f32) {
        let dinv = &self.dinv;
        par::chunks2_mut_indexed(z, d, 3 * NODE_CHUNK, |off, zc, dc| {
            let n0 = off / 3;
            for (k, (zn, dn)) in zc.chunks_mut(3).zip(dc.chunks_mut(3)).enumerate() {
                let n = n0 + k;
                let di = &dinv[6 * n..6 * n + 6];
                let rr = [r[3 * n], r[3 * n + 1], r[3 * n + 2]];
                let w = [
                    di[0] * rr[0] + di[1] * rr[1] + di[2] * rr[2],
                    di[1] * rr[0] + di[3] * rr[1] + di[4] * rr[2],
                    di[2] * rr[0] + di[4] * rr[1] + di[5] * rr[2],
                ];
                for c in 0..3 {
                    dn[c] = f * w[c];
                    zn[c] = dn[c];
                }
            }
        });
    }

    /// Chebyshev smoother: z gets `degree` steps toward K z = r, damping the
    /// Dinv*K spectrum on [lmax/CHEB_EIG_RATIO, lmax]. A fixed polynomial —
    /// same operator every call — so MG stays a constant SPD preconditioner.
    /// `from_zero` skips the first residual apply (pre-smoothing starts at 0).
    /// Workspaces: t (K z), d (direction).
    fn cheb_smooth(
        &self,
        z: &mut [f32],
        r: &[f32],
        t: &mut [f32],
        d: &mut [f32],
        degree: usize,
        from_zero: bool,
    ) {
        let lmax = (self.lmax * CHEB_LMAX_SAFETY) as f64;
        let lmin = lmax / CHEB_EIG_RATIO as f64;
        let theta = 0.5 * (lmax + lmin);
        let delta = 0.5 * (lmax - lmin);
        let sigma = theta / delta;
        let mut rho = 1.0 / sigma;
        // First step: d = (1/theta) * Dinv*(r - K z); z += d.
        if from_zero {
            self.cheb_first_fused(z, d, r, (1.0 / theta) as f32);
        } else {
            self.apply(z, t);
            self.cheb_step_fused(z, d, r, t, 0.0, (1.0 / theta) as f32);
        }
        for _ in 1..degree {
            self.apply(z, t);
            let rho_new = 1.0 / (2.0 * sigma - rho);
            // d = (rho_new*rho) d + (2 rho_new / delta) w; z += d
            self.cheb_step_fused(z, d, r, t, (rho_new * rho) as f32, (2.0 * rho_new / delta) as f32);
            rho = rho_new;
        }
    }

    /// z = Dinv r (undamped; used as the coarse-level CG preconditioner).
    fn diag_apply(&self, r: &[f32], z: &mut [f32]) {
        let dinv = &self.dinv;
        par::chunks_mut_indexed(z, 3 * NODE_CHUNK, |off, zc| {
            let n0 = off / 3;
            for (k, zn) in zc.chunks_mut(3).enumerate() {
                let n = n0 + k;
                let d = &dinv[6 * n..6 * n + 6];
                let rr = [r[3 * n], r[3 * n + 1], r[3 * n + 2]];
                zn[0] = d[0] * rr[0] + d[1] * rr[1] + d[2] * rr[2];
                zn[1] = d[1] * rr[0] + d[3] * rr[1] + d[4] * rr[2];
                zn[2] = d[2] * rr[0] + d[4] * rr[1] + d[5] * rr[2];
            }
        });
    }
}

/// Raise non-void cells to the preconditioner stiffness floor. Returns true
/// if anything changed (callers then refresh dinv/lmax).
fn clamp_pc_eps(eps: &mut [f32]) -> bool {
    let mut changed = false;
    for e in eps.iter_mut() {
        if *e > 0.0 && *e < PC_EPS_FLOOR {
            *e = PC_EPS_FLOOR;
            changed = true;
        }
    }
    changed
}

/// Trilinear parent weights of fine node coordinate x (even: one parent).
#[inline]
fn parent_weights(x: usize) -> [(usize, f64); 2] {
    if x % 2 == 0 {
        [(x / 2, 1.0), (0, 0.0)]
    } else {
        [(x / 2, 0.5), (x / 2 + 1, 0.5)]
    }
}

// NEGATIVE RESULTS on the Benchy worst case (205 MGCG iters at 300k cells),
// kept so nobody re-treads them: (a) renormalizing transfer weights over
// active-only coarse parents — no change; (b) exact-solving the coarse level
// (2-level hierarchy, tol 1e-8) — no change, so W-cycles can't help either;
// (c) a true Galerkin two-grid (coarse operator P^T K P, measured via a
// matrix-free prolong->apply->restrict PCG) reached 142 iters — only 1.45x,
// the ceiling for ANY coarse-operator improvement. The residual iterations
// are local thin-feature modes (1-2 cell beams/rails) that a half-resolution
// trilinear space cannot represent at all; they are resolution-limited, not
// solver-limited.

/// Restriction r_c = P^T r_f (trilinear weights), masked at coarse constrained DOFs.
fn restrict(fine: &Level, fine_res: &[f32], coarse: &Level, out: &mut [f32]) {
    let (cmx, cmy) = (coarse.mx, coarse.my);
    let constrained = &coarse.constrained;
    par::chunks_mut_indexed(out, 3 * NODE_CHUNK, |off, oc| {
        let n0 = off / 3;
        for (k, on) in oc.chunks_mut(3).enumerate() {
            let nc = n0 + k;
            let xx = nc % cmx;
            let yy = (nc / cmx) % cmy;
            let zz = nc / (cmx * cmy);
            let (fx, fy, fz) = (2 * xx as isize, 2 * yy as isize, 2 * zz as isize);
            let mut acc = [0f64; 3];
            for dz in -1isize..=1 {
                let z = fz + dz;
                if z < 0 || z >= fine.mz as isize {
                    continue;
                }
                let wz = 1.0 - 0.5 * dz.abs() as f64;
                for dy in -1isize..=1 {
                    let y = fy + dy;
                    if y < 0 || y >= fine.my as isize {
                        continue;
                    }
                    let wy = 1.0 - 0.5 * dy.abs() as f64;
                    for dx in -1isize..=1 {
                        let x = fx + dx;
                        if x < 0 || x >= fine.mx as isize {
                            continue;
                        }
                        let nf = ((z as usize * fine.my + y as usize) * fine.mx) + x as usize;
                        let w = wz * wy * (1.0 - 0.5 * dx.abs() as f64);
                        for d in 0..3 {
                            acc[d] += w * fine_res[3 * nf + d] as f64;
                        }
                    }
                }
            }
            for d in 0..3 {
                on[d] = if constrained[3 * nc + d] { 0.0 } else { acc[d] as f32 };
            }
        }
    });
}

/// z_f += P z_c (trilinear interpolation), skipping fine constrained DOFs.
fn prolong_add(fine: &Level, coarse: &Level, zc: &[f32], zf: &mut [f32]) {
    let (cmx, cmy) = (coarse.mx, coarse.my);
    let (fmx, fmy) = (fine.mx, fine.my);
    let constrained = &fine.constrained;
    par::chunks_mut_indexed(zf, 3 * NODE_CHUNK, |off, fc| {
        let n0 = off / 3;
        for (k, fnode) in fc.chunks_mut(3).enumerate() {
            let nf = n0 + k;
            let x = nf % fmx;
            let y = (nf / fmx) % fmy;
            let z = nf / (fmx * fmy);
            let (xw, yw, zw) = (parent_weights(x), parent_weights(y), parent_weights(z));
            let mut acc = [0f64; 3];
            for &(zi, wz) in &zw {
                if wz == 0.0 {
                    continue;
                }
                for &(yi, wy) in &yw {
                    if wy == 0.0 {
                        continue;
                    }
                    for &(xi, wx) in &xw {
                        if wx == 0.0 {
                            continue;
                        }
                        let ncn = (zi * cmy + yi) * cmx + xi;
                        let w = wz * wy * wx;
                        for d in 0..3 {
                            acc[d] += w * zc[3 * ncn + d] as f64;
                        }
                    }
                }
            }
            for d in 0..3 {
                if !constrained[3 * nf + d] {
                    fnode[d] += acc[d] as f32;
                }
            }
        }
    });
}

struct Workspaces {
    r: Vec<Vec<f32>>,
    z: Vec<Vec<f32>>,
    t: Vec<Vec<f32>>,
    t2: Vec<Vec<f32>>,
    d: Vec<Vec<f32>>,
    /// Coarse-PCG scratch (residual, preconditioned residual, A·p, search dir).
    /// Sized once to the coarsest level's ndof — reused on every V-cycle so the
    /// coarse solve no longer allocates four vecs per MGCG iteration.
    coarse_scratch: [Vec<f32>; 4],
}

fn v_cycle(levels: &[Level], ws: &mut Workspaces, l: usize) {
    if l == levels.len() - 1 {
        coarse_pcg(&levels[l], &ws.r[l], &mut ws.z[l], &mut ws.coarse_scratch);
        return;
    }
    let level = &levels[l];
    // Pre-smooth (zero initial guess).
    level.cheb_smooth(&mut ws.z[l], &ws.r[l], &mut ws.t[l], &mut ws.d[l], NU1, true);
    // Coarse-grid correction.
    level.apply(&ws.z[l], &mut ws.t[l]);
    par::sub(&mut ws.t2[l], &ws.r[l], &ws.t[l]);
    restrict(level, &ws.t2[l], &levels[l + 1], &mut ws.r[l + 1]);
    v_cycle(levels, ws, l + 1);
    {
        let (za, zb) = ws.z.split_at_mut(l + 1);
        prolong_add(level, &levels[l + 1], &zb[0], &mut za[l]);
    }
    // Post-smooth.
    level.cheb_smooth(&mut ws.z[l], &ws.r[l], &mut ws.t[l], &mut ws.d[l], NU2, false);
}

/// Block-diagonal preconditioned CG for the coarsest level (small). `scratch`
/// holds [r, z, q, p], all pre-sized to `level.ndof()` and reused across calls.
fn coarse_pcg(level: &Level, b: &[f32], x: &mut [f32], scratch: &mut [Vec<f32>; 4]) {
    par::fill(x, 0.0);
    let norm_b = par::norm2(b);
    if norm_b == 0.0 {
        return;
    }
    let [r, z, q, p] = scratch;
    r.copy_from_slice(b);
    level.diag_apply(r, z);
    p.copy_from_slice(z);
    let mut rz = par::dot(r, z);
    for _ in 0..800 {
        level.apply(p, q);
        let pq = par::dot(p, q);
        if pq <= 0.0 {
            break;
        }
        let alpha = (rz / pq) as f32;
        par::axpy(x, alpha, p);
        par::axpy(r, -alpha, q);
        if par::norm2(r) / norm_b < 1e-8 {
            break;
        }
        level.diag_apply(r, z);
        let rz_new = par::dot(r, z);
        let beta = (rz_new / rz) as f32;
        par::xpby(p, z, beta);
        rz = rz_new;
    }
}

pub struct MgSolver {
    pub levels: Vec<Level>,
    ws: Workspaces,
    /// EXACT finest-level eps, used by the outer CG operator (apply64). The
    /// level hierarchy itself — preconditioner only — runs on contrast-
    /// clamped eps (see PC_EPS_FLOOR): any SPD preconditioner converges to
    /// the same answer, and bounding the up-to-1e6:1 boundary-sliver contrast
    /// is what keeps the V-cycle effective on voxelized parts.
    eps_exact: Vec<f32>,
    /// Relative residual after each CG iteration of the LAST solve (element 0
    /// is the initial residual) — convergence-plot material, refreshed per call.
    pub last_trace: Vec<f32>,
}

/// Stiffness floor (relative to solid) applied to non-void cells INSIDE the
/// preconditioner hierarchy only. Exactness lives in `eps_exact`.
const PC_EPS_FLOOR: f32 = 0.20;

/// Stream the live residual trace every Nth MGCG iteration (see `progress`).
/// 4 is dense enough for a smooth preview curve yet keeps the cross-thread
/// publish to a few dozen copies even on the worst-case iteration counts.
const PROGRESS_STRIDE: usize = 4;

pub struct SolveStats {
    pub iterations: usize,
    pub rel_residual: f64,
    pub converged: bool,
}

impl MgSolver {
    /// Coarsen while dimensions stay even and at least 2 cells per axis.
    pub fn new(mut finest: Level, max_levels: usize) -> Self {
        let eps_exact = finest.eps.clone();
        if clamp_pc_eps(&mut finest.eps) {
            finest.build_dinv();
            finest.refresh_lmax();
        }
        let mut levels = vec![finest];
        while levels.len() < max_levels {
            let f = levels.last().unwrap();
            if f.nx % 2 != 0 || f.ny % 2 != 0 || f.nz % 2 != 0 {
                break;
            }
            if f.nx / 2 < 2 || f.ny / 2 < 2 || f.nz / 2 < 2 {
                break;
            }
            let c = f.coarsen();
            levels.push(c);
        }
        let coarse_n = levels.last().unwrap().ndof();
        let ws = Workspaces {
            r: levels.iter().map(|l| vec![0f32; l.ndof()]).collect(),
            z: levels.iter().map(|l| vec![0f32; l.ndof()]).collect(),
            t: levels.iter().map(|l| vec![0f32; l.ndof()]).collect(),
            t2: levels.iter().map(|l| vec![0f32; l.ndof()]).collect(),
            d: levels.iter().map(|l| vec![0f32; l.ndof()]).collect(),
            coarse_scratch: std::array::from_fn(|_| vec![0f32; coarse_n]),
        };
        Self { levels, ws, eps_exact, last_trace: Vec::new() }
    }

    /// The exact (unclamped) finest-level eps this solver was last given.
    pub fn eps_exact(&self) -> &[f32] {
        &self.eps_exact
    }

    /// Update per-cell stiffness factors in place (same void/solid topology!)
    /// and refresh the smoother diagonals down the hierarchy. Cheap compared
    /// to rebuilding levels — the optimization loop calls this every iteration.
    pub fn update_eps(&mut self, eps: Vec<f32>) {
        debug_assert_eq!(eps.len(), self.levels[0].eps.len());
        self.eps_exact = eps.clone();
        self.levels[0].eps = eps;
        clamp_pc_eps(&mut self.levels[0].eps);
        self.levels[0].build_dinv();
        self.levels[0].refresh_lmax();
        for l in 1..self.levels.len() {
            let f = &self.levels[l - 1];
            let coarse = average_coarse_eps(&f.eps, f.nx, f.ny, f.nz);
            self.levels[l].eps = coarse;
            self.levels[l].build_dinv();
            self.levels[l].refresh_lmax();
        }
    }

    /// Apply ONE multigrid V-cycle as a preconditioner: `z ≈ K⁻¹ r`. Used by
    /// the modal eigensolver (LOBPCG) as its preconditioner — a single V-cycle
    /// is far cheaper than a full converged solve and is exactly what LOBPCG
    /// wants. `r`/`z` are f64 (the bulk of the cycle runs in f32 internally);
    /// `z` is fully overwritten and is zero at constrained DOFs.
    pub fn precondition(&mut self, r: &[f64], z: &mut [f64]) {
        debug_assert_eq!(r.len(), self.levels[0].ndof());
        debug_assert_eq!(z.len(), self.levels[0].ndof());
        par::demote(&mut self.ws.r[0], r);
        v_cycle(&self.levels, &mut self.ws, 0);
        par::promote(z, &self.ws.z[0]);
    }

    /// Matrix-free `y = K x` on the finest level (exact eps), f64. The operator
    /// the modal eigensolver projects with — includes penalty springs and masks
    /// constrained DOFs.
    pub fn apply_k(&self, x: &[f64], y: &mut [f64]) {
        self.levels[0].apply64_eps(&self.eps_exact, x, y);
    }

    /// Mixed-precision MGCG: outer CG loop and operator in f64 (so attainable
    /// accuracy is not capped by f32 cancellation in K·u), V-cycle
    /// preconditioner in f32 (the bulk of the flops). `b` must be zero at
    /// constrained DOFs; `u` is overwritten (zero initial guess).
    pub fn solve(&mut self, b: &[f64], u: &mut [f64], tol: f64, max_iter: usize) -> SolveStats {
        u.fill(0.0);
        self.solve_warm(b, u, tol, max_iter)
    }

    /// Like `solve`, but uses the incoming `u` as the initial guess — the
    /// optimization loop re-solves after small density updates and converges
    /// in a few iterations from the previous displacement field.
    pub fn solve_warm(&mut self, b: &[f64], u: &mut [f64], tol: f64, max_iter: usize) -> SolveStats {
        let n = self.levels[0].ndof();
        assert_eq!(b.len(), n);
        assert_eq!(u.len(), n);
        self.last_trace.clear();
        // Guard the masking invariant for arbitrary initial guesses.
        for (i, c) in self.levels[0].constrained.iter().enumerate() {
            if *c {
                u[i] = 0.0;
            }
        }
        let norm_b = par::norm2_64(b);
        if norm_b == 0.0 {
            u.fill(0.0);
            return SolveStats { iterations: 0, rel_residual: 0.0, converged: true };
        }
        // r = b - A u0
        let mut r = vec![0f64; n];
        self.levels[0].apply64_eps(&self.eps_exact, u, &mut r);
        for i in 0..n {
            r[i] = b[i] - r[i];
        }
        let res0 = par::norm2_64(&r) / norm_b;
        self.last_trace.push(res0 as f32);
        // Stream the starting point so a live preview has something to draw
        // before the first cycle finishes (a fine solve's iteration is slow).
        crate::progress::publish(&self.last_trace);
        if res0 <= tol {
            return SolveStats { iterations: 0, rel_residual: res0, converged: true };
        }
        let mut p = vec![0f64; n];
        let mut q = vec![0f64; n];

        par::demote(&mut self.ws.r[0], &r);
        v_cycle(&self.levels, &mut self.ws, 0);
        par::promote(&mut p, &self.ws.z[0]);
        let mut rz = par::dot_mixed(&r, &self.ws.z[0]);

        let mut res = f64::INFINITY;
        for it in 0..max_iter {
            // Cooperative cancel: bail like an iteration-cap hit; the caller
            // checks `cancel::requested()` and raises the Cancelled error.
            if crate::cancel::requested() {
                return SolveStats { iterations: it, rel_residual: res, converged: false };
            }
            self.levels[0].apply64_eps(&self.eps_exact, &p, &mut q);
            let pq = par::dot64(&p, &q);
            if !pq.is_finite() || pq <= 0.0 {
                return SolveStats { iterations: it, rel_residual: res, converged: false };
            }
            let alpha = rz / pq;
            par::axpy64(u, alpha, &p);
            par::axpy64(&mut r, -alpha, &q);
            res = par::norm2_64(&r) / norm_b;
            self.last_trace.push(res as f32);
            if !res.is_finite() {
                // Diverged (singular/near-singular operator). Surface the
                // non-finite residual; `solve_cached` turns it into a hard
                // `Diverged` error rather than presenting a garbage field.
                return SolveStats { iterations: it + 1, rel_residual: res, converged: false };
            }
            if res <= tol {
                return SolveStats { iterations: it + 1, rel_residual: res, converged: true };
            }
            // Live preview: stream the trace every few iterations (not every
            // one — the UI repaints at frame cadence and the full, exact trace
            // is returned at the end). No-op unless an embedder installed a
            // sink, so native solves/benches pay only the modulo + a borrow.
            if it % PROGRESS_STRIDE == 0 {
                crate::progress::publish(&self.last_trace);
            }
            par::demote(&mut self.ws.r[0], &r);
            v_cycle(&self.levels, &mut self.ws, 0);
            let rz_new = par::dot_mixed(&r, &self.ws.z[0]);
            let beta = rz_new / rz;
            par::xpby_mixed(&mut p, &self.ws.z[0], beta);
            rz = rz_new;
        }
        SolveStats { iterations: max_iter, rel_residual: res, converged: false }
    }
}
