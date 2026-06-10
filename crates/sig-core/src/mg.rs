//! Geometric multigrid preconditioned CG (MGCG) for voxel-grid elasticity.
//!
//! - Matrix-free: one reference KE per level, scaled per cell by a stiffness
//!   factor; cells are processed in 8 parity colors so scatter-adds never race.
//! - Smoother: damped block-Jacobi (3x3 node blocks), symmetric, so the
//!   V-cycle is a valid SPD preconditioner for CG.
//! - Coarsening: rediscretization with averaged cell stiffness (KE scales
//!   linearly with h on cube cells), trilinear prolongation, restriction = P^T.
//! - Dirichlet/inactive DOFs are masked: vectors stay zero there throughout.

use crate::fem::{invert3, ke_diag_blocks, NODE_OFFSETS};
use crate::par::{self, UnsafeSlice};

pub const NU1: usize = 3;
pub const NU2: usize = 3;
pub const OMEGA: f32 = 0.6;
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
    /// Non-void cell indices grouped by parity color (no shared nodes within a color).
    colors: [Vec<u32>; 8],
    /// Per-DOF mask: Dirichlet-fixed or inactive (no solid neighbor cell).
    pub constrained: Vec<bool>,
    /// Per-node inverted 3x3 diagonal block (row-major 9), zeroed at constrained rows/cols.
    dinv: Vec<f32>,
    /// Penalty springs (node, unit direction, stiffness N/mm) — frictionless supports.
    springs: Vec<(u32, [f64; 3], f64)>,
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
            colors: Default::default(),
            constrained: vec![false; ndof],
            dinv: Vec::new(),
            springs,
        };
        level.build_colors();
        level.build_constrained(fixed);
        level.build_dinv();
        level
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

    fn build_colors(&mut self) {
        let mut colors: [Vec<u32>; 8] = Default::default();
        for cz in 0..self.nz {
            for cy in 0..self.ny {
                for cx in 0..self.nx {
                    let ci = (cz * self.ny + cy) * self.nx + cx;
                    if self.eps[ci] > 0.0 {
                        let color = (cx & 1) | ((cy & 1) << 1) | ((cz & 1) << 2);
                        colors[color].push(ci as u32);
                    }
                }
            }
        }
        self.colors = colors;
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
        let mut dinv = vec![0f32; 9 * self.node_count()];
        let (nx, ny, nz) = (self.nx, self.ny, self.nz);
        let (mx, my) = (self.mx, self.my);
        let eps = &self.eps;
        let constrained = &self.constrained;
        par::chunks_mut_indexed(&mut dinv, 9 * NODE_CHUNK, |off, chunk| {
            let n0 = off / 9;
            for (k, blk) in chunk.chunks_mut(9).enumerate() {
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
                    for r in 0..3 {
                        for c in 0..3 {
                            blk[3 * r + c] = if constrained[3 * n + r] || constrained[3 * n + c] {
                                0.0
                            } else {
                                inv[r][c] as f32
                            };
                        }
                    }
                }
            }
        });
        self.dinv = dinv;
    }

    /// y = K x (masked at constrained DOFs). x must be zero at constrained DOFs.
    pub fn apply(&self, x: &[f32], y: &mut [f32]) {
        debug_assert_eq!(x.len(), self.ndof());
        debug_assert_eq!(y.len(), self.ndof());
        par::fill(y, 0.0);
        {
            let ys = UnsafeSlice::new(y);
            let (nx, ny) = (self.nx, self.ny);
            let (mx, my) = (self.mx, self.my);
            for color in 0..8 {
                par::for_each(&self.colors[color], |&ci| {
                    let ci = ci as usize;
                    let cx = ci % nx;
                    let cy = (ci / nx) % ny;
                    let cz = ci / (nx * ny);
                    let e = self.eps[ci];
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
                    let mut yl = [0f32; 24];
                    for i in 0..24 {
                        let row = &self.ke[i];
                        let mut s = 0f32;
                        for j in 0..24 {
                            s += row[j] * xl[j];
                        }
                        yl[i] = e * s;
                    }
                    // SAFETY: cells within one color never share nodes.
                    unsafe {
                        for l in 0..8 {
                            let n = nidx[l];
                            *ys.get_mut(3 * n) += yl[3 * l];
                            *ys.get_mut(3 * n + 1) += yl[3 * l + 1];
                            *ys.get_mut(3 * n + 2) += yl[3 * l + 2];
                        }
                    }
                });
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

    /// y = K x in f64 (used by the outer CG; the cancellation in K·u near
    /// equilibrium exceeds f32 precision, which caps attainable accuracy).
    pub fn apply64(&self, x: &[f64], y: &mut [f64]) {
        debug_assert_eq!(x.len(), self.ndof());
        debug_assert_eq!(y.len(), self.ndof());
        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            y.par_chunks_mut(1 << 14).for_each(|c| c.fill(0.0));
        }
        #[cfg(not(feature = "parallel"))]
        y.fill(0.0);
        {
            let ys = UnsafeSlice::new(y);
            let (nx, ny) = (self.nx, self.ny);
            let (mx, my) = (self.mx, self.my);
            for color in 0..8 {
                par::for_each(&self.colors[color], |&ci| {
                    let ci = ci as usize;
                    let cx = ci % nx;
                    let cy = (ci / nx) % ny;
                    let cz = ci / (nx * ny);
                    let e = self.eps[ci] as f64;
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
                    // SAFETY: cells within one color never share nodes.
                    unsafe {
                        for l in 0..8 {
                            let n = nidx[l];
                            *ys.get_mut(3 * n) += yl[3 * l];
                            *ys.get_mut(3 * n + 1) += yl[3 * l + 1];
                            *ys.get_mut(3 * n + 2) += yl[3 * l + 2];
                        }
                    }
                });
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

    /// First damped block-Jacobi sweep with zero initial guess: z = omega * Dinv r.
    fn smooth_first(&self, z: &mut [f32], r: &[f32]) {
        let dinv = &self.dinv;
        par::chunks_mut_indexed(z, 3 * NODE_CHUNK, |off, zc| {
            let n0 = off / 3;
            for (k, zn) in zc.chunks_mut(3).enumerate() {
                let n = n0 + k;
                let d = &dinv[9 * n..9 * n + 9];
                let rr = [r[3 * n], r[3 * n + 1], r[3 * n + 2]];
                for row in 0..3 {
                    zn[row] = OMEGA
                        * (d[3 * row] * rr[0] + d[3 * row + 1] * rr[1] + d[3 * row + 2] * rr[2]);
                }
            }
        });
    }

    /// z += omega * Dinv (r - t)  where t = K z was computed by the caller.
    fn smooth_update(&self, z: &mut [f32], r: &[f32], t: &[f32]) {
        let dinv = &self.dinv;
        par::chunks_mut_indexed(z, 3 * NODE_CHUNK, |off, zc| {
            let n0 = off / 3;
            for (k, zn) in zc.chunks_mut(3).enumerate() {
                let n = n0 + k;
                let d = &dinv[9 * n..9 * n + 9];
                let rr =
                    [r[3 * n] - t[3 * n], r[3 * n + 1] - t[3 * n + 1], r[3 * n + 2] - t[3 * n + 2]];
                for row in 0..3 {
                    zn[row] += OMEGA
                        * (d[3 * row] * rr[0] + d[3 * row + 1] * rr[1] + d[3 * row + 2] * rr[2]);
                }
            }
        });
    }

    /// z = Dinv r (undamped; used as the coarse-level CG preconditioner).
    fn diag_apply(&self, r: &[f32], z: &mut [f32]) {
        let dinv = &self.dinv;
        par::chunks_mut_indexed(z, 3 * NODE_CHUNK, |off, zc| {
            let n0 = off / 3;
            for (k, zn) in zc.chunks_mut(3).enumerate() {
                let n = n0 + k;
                let d = &dinv[9 * n..9 * n + 9];
                let rr = [r[3 * n], r[3 * n + 1], r[3 * n + 2]];
                for row in 0..3 {
                    zn[row] =
                        d[3 * row] * rr[0] + d[3 * row + 1] * rr[1] + d[3 * row + 2] * rr[2];
                }
            }
        });
    }
}

/// Child-averaged stiffness for the next-coarser grid (fine dims must be even).
fn average_coarse_eps(fine_eps: &[f32], fnx: usize, fny: usize, fnz: usize) -> Vec<f32> {
    assert!(fnx % 2 == 0 && fny % 2 == 0 && fnz % 2 == 0);
    let (nx, ny, nz) = (fnx / 2, fny / 2, fnz / 2);
    let mut eps = vec![0f32; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let mut s = 0f32;
                for dz in 0..2 {
                    for dy in 0..2 {
                        for dx in 0..2 {
                            s += fine_eps[((2 * cz + dz) * fny + 2 * cy + dy) * fnx + 2 * cx + dx];
                        }
                    }
                }
                eps[(cz * ny + cy) * nx + cx] = s / 8.0;
            }
        }
    }
    eps
}

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
                        let w = wz * wy * (1.0 - 0.5 * dx.abs() as f64);
                        let nf = ((z as usize * fine.my + y as usize) * fine.mx) + x as usize;
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
            let xw: [(usize, f64); 2] =
                if x % 2 == 0 { [(x / 2, 1.0), (0, 0.0)] } else { [(x / 2, 0.5), (x / 2 + 1, 0.5)] };
            let yw: [(usize, f64); 2] =
                if y % 2 == 0 { [(y / 2, 1.0), (0, 0.0)] } else { [(y / 2, 0.5), (y / 2 + 1, 0.5)] };
            let zw: [(usize, f64); 2] =
                if z % 2 == 0 { [(z / 2, 1.0), (0, 0.0)] } else { [(z / 2, 0.5), (z / 2 + 1, 0.5)] };
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
}

fn v_cycle(levels: &[Level], ws: &mut Workspaces, l: usize) {
    if l == levels.len() - 1 {
        coarse_pcg(&levels[l], &ws.r[l], &mut ws.z[l]);
        return;
    }
    let level = &levels[l];
    // Pre-smooth (zero initial guess).
    level.smooth_first(&mut ws.z[l], &ws.r[l]);
    for _ in 1..NU1 {
        level.apply(&ws.z[l], &mut ws.t[l]);
        level.smooth_update(&mut ws.z[l], &ws.r[l], &ws.t[l]);
    }
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
    for _ in 0..NU2 {
        level.apply(&ws.z[l], &mut ws.t[l]);
        level.smooth_update(&mut ws.z[l], &ws.r[l], &ws.t[l]);
    }
}

/// Block-diagonal preconditioned CG for the coarsest level (small).
fn coarse_pcg(level: &Level, b: &[f32], x: &mut [f32]) {
    let n = level.ndof();
    par::fill(x, 0.0);
    let norm_b = par::norm2(b);
    if norm_b == 0.0 {
        return;
    }
    let mut r = b.to_vec();
    let mut z = vec![0f32; n];
    let mut q = vec![0f32; n];
    level.diag_apply(&r, &mut z);
    let mut p = z.clone();
    let mut rz = par::dot(&r, &z);
    for _ in 0..800 {
        level.apply(&p, &mut q);
        let pq = par::dot(&p, &q);
        if pq <= 0.0 {
            break;
        }
        let alpha = (rz / pq) as f32;
        par::axpy(x, alpha, &p);
        par::axpy(&mut r, -alpha, &q);
        if par::norm2(&r) / norm_b < 1e-8 {
            break;
        }
        level.diag_apply(&r, &mut z);
        let rz_new = par::dot(&r, &z);
        let beta = (rz_new / rz) as f32;
        par::xpby(&mut p, &z, beta);
        rz = rz_new;
    }
}

pub struct MgSolver {
    pub levels: Vec<Level>,
    ws: Workspaces,
}

pub struct SolveStats {
    pub iterations: usize,
    pub rel_residual: f64,
    pub converged: bool,
}

impl MgSolver {
    /// Coarsen while dimensions stay even and at least 2 cells per axis.
    pub fn new(finest: Level, max_levels: usize) -> Self {
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
        let ws = Workspaces {
            r: levels.iter().map(|l| vec![0f32; l.ndof()]).collect(),
            z: levels.iter().map(|l| vec![0f32; l.ndof()]).collect(),
            t: levels.iter().map(|l| vec![0f32; l.ndof()]).collect(),
            t2: levels.iter().map(|l| vec![0f32; l.ndof()]).collect(),
        };
        Self { levels, ws }
    }

    /// Update per-cell stiffness factors in place (same void/solid topology!)
    /// and refresh the smoother diagonals down the hierarchy. Cheap compared
    /// to rebuilding levels — the optimization loop calls this every iteration.
    pub fn update_eps(&mut self, eps: Vec<f32>) {
        debug_assert_eq!(eps.len(), self.levels[0].eps.len());
        self.levels[0].eps = eps;
        self.levels[0].build_dinv();
        for l in 1..self.levels.len() {
            let f = &self.levels[l - 1];
            let coarse = average_coarse_eps(&f.eps, f.nx, f.ny, f.nz);
            self.levels[l].eps = coarse;
            self.levels[l].build_dinv();
        }
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
        self.levels[0].apply64(u, &mut r);
        for i in 0..n {
            r[i] = b[i] - r[i];
        }
        let res0 = par::norm2_64(&r) / norm_b;
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
            self.levels[0].apply64(&p, &mut q);
            let pq = par::dot64(&p, &q);
            if pq <= 0.0 {
                return SolveStats { iterations: it, rel_residual: res, converged: false };
            }
            let alpha = rz / pq;
            par::axpy64(u, alpha, &p);
            par::axpy64(&mut r, -alpha, &q);
            res = par::norm2_64(&r) / norm_b;
            if res <= tol {
                return SolveStats { iterations: it + 1, rel_residual: res, converged: true };
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
