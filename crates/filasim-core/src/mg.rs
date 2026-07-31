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
//! - Coarsening: rediscretization with averaged cell stiffness, trilinear
//!   prolongation, restriction = P^T. SEMICOARSENING: each axis is halved only
//!   while it stays a usable grid, so a plate too thin to coarsen through
//!   keeps its full in-plane hierarchy (coarse cells are then bricks, and
//!   their KE is re-integrated — a brick's KE is not a multiple of a cube's).
//! - Dirichlet/inactive DOFs are masked: vectors stay zero there throughout.
//!   The LIVE SET (`Level::build_live`) exploits that: on a voxelized part most
//!   of the padded node grid is dead (18 % live on a 3DBenchy), and every
//!   node-wise kernel skips wholly dead blocks instead of streaming zeros.
//!
//! Where the time goes (3DBenchy, 262 k solid cells, 16 threads): ~65 % in the
//! Chebyshev smoother, ~12 % residual, ~9 % the f64 outer operator, ~5 % the
//! transfers, ~3 % the coarse solve. The finest level is ~88 % of all multigrid
//! work, so that is where optimization pays.

use crate::eps::average_coarse_eps;
use crate::fem::{invert3, ke_diag_blocks, NODE_OFFSETS};
use crate::par::{self, UnsafeSlice};
use crate::rigid::RigidGroup;

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
/// Inner form of the 24x24 element matvec. KE is SYMMETRIC, so "column j" IS
/// row j: both forms stream the same contiguous rows, but the COLUMN form
/// accumulates into 24 registers (24 broadcasts x 24 FMAs, no cross-lane
/// reduction) while the ROW form does 24 dot products with a horizontal reduce
/// each. Which wins is TARGET-DEPENDENT - measured on the 3DBenchy at 262k
/// solid cells, not guessed:
///   x86-64 AVX2 (256-bit):    column 1.53x faster than row
///   wasm32 simd128 (128-bit): row 1.26x faster than column - the 24-wide
///     column accumulator does not fit in 4-lane registers and spills.
/// Re-measure BOTH targets before changing this.
#[cfg(target_arch = "wasm32")]
const KERNEL_COLUMN: bool = false;
#[cfg(not(target_arch = "wasm32"))]
const KERNEL_COLUMN: bool = true;
/// Node-block granularity for the live-set skip (see `Level::build_live`).
/// Must divide NODE_CHUNK.
const BLK: usize = 16;

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

/// Transverse-isotropic infill overlay (DESIGN §22.4).
///
/// When present, a cell's stiffness is the Voigt blend of two DIFFERENT
/// tensors — `eps[c]·KE_solid + ti.eps[c]·KE_infill` — instead of one tensor
/// scaled by one number. `Level.eps` becomes the SOLID (skin) weight and this
/// carries the infill weight; a pure-infill cell therefore has `Level.eps == 0`
/// and is still active, which is why every "is this cell live" test has to go
/// through [`Level::cell_active`] rather than reading `eps` directly.
///
/// `None` is not an optimization of "isotropic infill" — it is the untouched
/// original code path, so a project with no TI data is bit-identical to before.
struct TiOverlay {
    ke: [[f32; 24]; 24],
    ke64: [[f64; 24]; 24],
    /// Per-cell infill weight `occ·(1−skin_frac)·rel(ρ)`.
    eps: Vec<f32>,
    /// Normalized tensor (`Ep = 1`), kept for the same reason `e0`/`nu` are:
    /// a semicoarsened brick level must RE-INTEGRATE its KE, because a brick's
    /// KE is not a scalar multiple of a cube's.
    c_unit: [[f64; 6]; 6],
}

pub struct Level {
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub mx: usize,
    pub my: usize,
    pub mz: usize,
    /// Cell size of the FINEST level's x axis. Only the finest level is
    /// guaranteed isotropic (`hv == [h; 3]`); semicoarsened levels are bricks.
    pub h: f64,
    /// Per-axis cell size — coarsening halves only the axes it can (see
    /// `MgSolver::new`), so coarse cells are rectangular bricks.
    hv: [f64; 3],
    /// Material constants, kept so a semicoarsened level can re-integrate its
    /// brick KE (a brick's KE is not a multiple of a cube's).
    e0: f64,
    nu: f64,
    /// Which axes were halved to produce this level from its parent — replayed
    /// by `update_eps` so the eps cascade matches the geometry cascade.
    from_parent: [bool; 3],
    ke: [[f32; 24]; 24],
    ke64: [[f64; 24]; 24],
    /// Per-cell stiffness factor; exactly 0.0 = cell skipped entirely.
    /// With a [`TiOverlay`] present this is only the SOLID share — use
    /// [`Level::cell_active`], never `eps[ci] > 0.0`, to test liveness.
    pub eps: Vec<f32>,
    /// Transverse-isotropic infill (DESIGN §22). `None` = the original
    /// single-tensor kernel, unchanged.
    ti: Option<TiOverlay>,
    /// Exact cut-cell element matrices (`cutcell.rs`). FINEST LEVEL ONLY —
    /// [`Level::coarsen`] deliberately drops it, so the coarse grids keep the
    /// cheap `occupancy · KE_full` ersatz and the V-cycle stays a pure
    /// preconditioner. The outer CG operator is exact either way, so
    /// convergence is unaffected; this is the same fine-level-only discipline
    /// the rigid remote-mass coupling uses. `None` = byte-identical to the
    /// pre-cut-cell code path.
    cut: Option<crate::cutcell::CutStiffness>,
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
    /// Per-`BLK`-node-block "has an active node" flag — the live set every
    /// node-wise kernel iterates over (see `build_live`).
    live: Vec<bool>,
    /// Per-node inverted 3x3 diagonal block, stored SYMMETRIC-packed as 6
    /// floats [d00,d01,d02,d11,d12,d22] (the block is symmetric; `invert3` of
    /// a symmetric matrix is bitwise-symmetric since the paired cofactors are
    /// identical expressions). Zeroed at constrained rows/cols. 6/9 of the
    /// memory — this is the dominant stream of the smoother.
    dinv: Vec<f32>,
    /// Penalty springs (node, unit direction, stiffness N/mm) — frictionless supports.
    springs: Vec<(u32, [f64; 3], f64)>,
    /// Rigid remote-mass couplings (DESIGN §16 milestone 4). A penalty rank-6
    /// term per group; the matvec applies it as a post-pass after the hex
    /// scatter (like `springs`), the exact diagonal folds into `build_dinv`.
    /// FINEST LEVEL ONLY — coarsening drops it (MG pass-through), so the coarse
    /// correction ignores the rigid constraint and the fine smoother handles it.
    rigid: Vec<RigidGroup>,
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

impl TiOverlay {
    /// Integrate the infill reference element for cell size `hv`. `c_unit` is
    /// normalized to `Ep = 1`, so it is scaled by `e0` here and the per-cell
    /// weight then carries `rel(ρ)` — the same split as the isotropic path,
    /// where `ke_hex(e0, …)` is scaled by `eps`.
    fn new(c_unit: &[[f64; 6]; 6], hv: [f64; 3], e0: f64, eps: Vec<f32>) -> Self {
        let mut c = *c_unit;
        for row in c.iter_mut() {
            for v in row.iter_mut() {
                *v *= e0;
            }
        }
        let ke64 = crate::fem::ke_hex_c(&c, hv);
        let mut ke = [[0f32; 24]; 24];
        for i in 0..24 {
            for j in 0..24 {
                ke[i][j] = ke64[i][j] as f32;
            }
        }
        Self { ke, ke64, eps, c_unit: *c_unit }
    }
}

impl Level {
    pub fn node_count(&self) -> usize {
        self.mx * self.my * self.mz
    }

    /// Does this cell carry ANY stiffness? With a TI overlay a pure-infill
    /// cell has `eps == 0` but is fully active, so this — not `eps[ci] > 0.0`
    /// — is the void test everywhere (slabs, live blocks, active DOFs,
    /// diagonal assembly). Getting this wrong deletes the infill from the
    /// structure while leaving a solve that still converges.
    #[inline]
    fn cell_active(&self, ci: usize) -> bool {
        self.eps[ci] > 0.0 || self.ti.as_ref().is_some_and(|t| t.eps[ci] > 0.0)
    }

    pub fn ndof(&self) -> usize {
        3 * self.node_count()
    }

    /// Live-block mask and its DOF stride, for the outer CG's vector ops.
    #[inline]
    pub fn live_mask(&self) -> (&[bool], usize) {
        (&self.live, 3 * BLK)
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
        e0: f64,
        nu: f64,
        fixed: &[bool],
        springs: Vec<(u32, [f64; 3], f64)>,
        rigid: Vec<RigidGroup>,
    ) -> Self {
        // Isotropic entry point (the finest level is always a cube grid).
        let ke64 = crate::fem::ke_hex(e0, nu, h);
        Self::new_aniso(nx, ny, nz, [h; 3], e0, nu, eps, ke64, fixed, springs, rigid, [false; 3], None)
    }

    /// As [`Level::new`], plus a transverse-isotropic infill overlay
    /// (DESIGN §22): `eps` is the SOLID (skin) weight, `eps_infill` the infill
    /// weight, and `c_unit` the normalized (`Ep = 1`) infill tensor.
    #[allow(clippy::too_many_arguments)]
    pub fn new_ti(
        nx: usize,
        ny: usize,
        nz: usize,
        h: f64,
        eps: Vec<f32>,
        eps_infill: Vec<f32>,
        c_unit: [[f64; 6]; 6],
        e0: f64,
        nu: f64,
        fixed: &[bool],
        springs: Vec<(u32, [f64; 3], f64)>,
        rigid: Vec<RigidGroup>,
    ) -> Self {
        assert_eq!(eps.len(), eps_infill.len());
        let ke64 = crate::fem::ke_hex(e0, nu, h);
        let ti = Some(TiOverlay::new(&c_unit, [h; 3], e0, eps_infill));
        Self::new_aniso(nx, ny, nz, [h; 3], e0, nu, eps, ke64, fixed, springs, rigid, [false; 3], ti)
    }

    #[allow(clippy::too_many_arguments)]
    fn new_aniso(
        nx: usize,
        ny: usize,
        nz: usize,
        hv: [f64; 3],
        e0: f64,
        nu: f64,
        eps: Vec<f32>,
        ke64: [[f64; 24]; 24],
        fixed: &[bool],
        springs: Vec<(u32, [f64; 3], f64)>,
        rigid: Vec<RigidGroup>,
        from_parent: [bool; 3],
        ti: Option<TiOverlay>,
    ) -> Self {
        let h = hv[0];
        assert_eq!(eps.len(), nx * ny * nz);
        debug_assert!(ti.as_ref().is_none_or(|t| t.eps.len() == eps.len()));
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
            hv,
            e0,
            nu,
            from_parent,
            ke,
            ke64,
            eps,
            ti,
            // Attached after construction by `set_cut`, finest level only.
            cut: None,
            slabs: Vec::new(),
            constrained: vec![false; ndof],
            live: Vec::new(),
            dinv: Vec::new(),
            springs,
            rigid,
            lmax: 1.0,
            eigvec: Vec::new(),
            pi_t: Vec::new(),
            pi_w: Vec::new(),
        };
        level.build_slabs();
        level.build_live();
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

    /// Which axes this level can still be halved on. An axis is coarsened only
    /// while it stays a usable grid: even, and at least 4 cells before the halving
    /// (so ≥2 after). A 3-cell-thick plate therefore keeps its full x/y hierarchy
    /// instead of collapsing the whole solver to a single level.
    pub fn splittable(&self) -> [bool; 3] {
        let ok = |n: usize| n % 2 == 0 && n >= 4;
        [ok(self.nx), ok(self.ny), ok(self.nz)]
    }

    /// Rediscretized coarse level: `c[a]` halves axis `a`, child-averaged
    /// stiffness. Semicoarsening (some `c[a]` false) makes the coarse cell a
    /// BRICK, so its KE is re-integrated rather than scaled.
    pub fn coarsen(&self, c: [bool; 3]) -> Self {
        debug_assert!(c.iter().any(|&v| v), "coarsening must halve at least one axis");
        let eps = average_coarse_eps(&self.eps, self.nx, self.ny, self.nz, c);
        let step = [
            if c[0] { 2usize } else { 1 },
            if c[1] { 2 } else { 1 },
            if c[2] { 2 } else { 1 },
        ];
        let (nx, ny, nz) = (self.nx / step[0], self.ny / step[1], self.nz / step[2]);
        let hv = [
            self.hv[0] * step[0] as f64,
            self.hv[1] * step[1] as f64,
            self.hv[2] * step[2] as f64,
        ];
        let ke64 = crate::fem::ke_hex_aniso(self.e0, self.nu, hv);
        // The TI overlay cascades the SAME way: its weight field is child-
        // averaged and its reference element re-integrated for the (possibly
        // brick) coarse cell — the two tensors must stay on the same geometry
        // or the coarse operator stops approximating the fine one.
        let ti = self.ti.as_ref().map(|t| {
            let te = average_coarse_eps(&t.eps, self.nx, self.ny, self.nz, c);
            TiOverlay::new(&t.c_unit, hv, self.e0, te)
        });
        // Inject fine Dirichlet flags at coincident nodes (step·X, step·Y, step·Z).
        let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
        let mut fixed = vec![false; 3 * mx * my * mz];
        for z in 0..mz {
            for y in 0..my {
                for x in 0..mx {
                    let nf = self.node_index(step[0] * x, step[1] * y, step[2] * z);
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
                let map = |v: usize, st: usize, lim: usize| {
                    if st == 1 { v.min(lim - 1) } else { ((v + 1) / 2).min(lim - 1) }
                };
                let x = map(n % self.mx, step[0], mx);
                let y = map(n / self.mx % self.my, step[1], my);
                let z = map(n / (self.mx * self.my), step[2], mz);
                (((z * my + y) * mx + x) as u32, dir, k)
            })
            .collect();
        // Rigid couplings are NOT coarsened (DESIGN §16: MG pass-through) — the
        // fine-level smoother handles the stiff local constraint, the coarse
        // grids reduce only the smooth global error.
        Self::new_aniso(
            nx, ny, nz, hv, self.e0, self.nu, eps, ke64, &fixed, springs, Vec::new(), c, ti,
        )
    }

    /// Cut the active cells into z-slabs balanced by active-cell count (cuts
    /// only at whole z-plane boundaries — the two-phase safety invariant).
    /// Depends only on the void set, which `update_eps` never changes, so it
    /// is built once per level.
    fn build_slabs(&mut self) {
        let plane_cells = self.nx * self.ny;
        let mut plane = vec![0usize; self.nz];
        for (cz, cnt) in plane.iter_mut().enumerate() {
            *cnt = (cz * plane_cells..(cz + 1) * plane_cells)
                .filter(|&i| self.cell_active(i))
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
                if self.cell_active(i) {
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

    /// Per-BLK-node-block "contains at least one ACTIVE node" flag.
    ///
    /// On a voxelized part the padded node grid is mostly dead: a 3DBenchy at
    /// 300 k solid cells has 18 % live nodes, a flat plate ~25 %. Every DOF
    /// outside the active set is provably ZERO for the whole solve (`r` and
    /// `t` are masked, `dinv` is zeroed there, so the smoother writes 0 into 0
    /// and the transfers read/write 0), yet the smoother and the transfer
    /// operators used to stream all of them. Skipping a wholly dead block
    /// therefore changes NO value — it only stops touching cache lines that
    /// hold zeros. Block granularity is a trade: coarser blocks miss more
    /// skips, finer ones cost more branch overhead per useful byte.
    fn build_live(&mut self) {
        let (nx, ny, nz) = (self.nx, self.ny, self.nz);
        let (mx, my) = (self.mx, self.my);
        let nodes = self.node_count();
        let mut live = vec![false; nodes.div_ceil(BLK)];
        for (b, l) in live.iter_mut().enumerate() {
            let lo = b * BLK;
            let hi = ((b + 1) * BLK).min(nodes);
            'block: for n in lo..hi {
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
                            if cx < nx
                                && cy < ny
                                && cz < nz
                                && self.cell_active((cz * ny + cy) * nx + cx)
                            {
                                *l = true;
                                break 'block;
                            }
                        }
                    }
                }
            }
        }
        self.live = live;
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
                                    if self.cell_active(ci) {
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
        // Rigid remote masses: the exact per-node diagonal block
        // k(I − Bᵢ G⁻¹ Bᵢᵀ) folds into the same block-Jacobi diagonal (the
        // off-diagonal rank-6 coupling is left to Chebyshev/CG, as with any
        // incomplete smoother). Empty (skipped) when no rigid group is present.
        for g in &self.rigid {
            for (i, &n) in g.nodes.iter().enumerate() {
                let e = spring_blocks.entry(n).or_insert([[0.0; 3]; 3]);
                let blk = g.diag_block(i);
                for r in 0..3 {
                    for c in 0..3 {
                        e[r][c] += blk[r][c];
                    }
                }
            }
        }
        let mut dinv = vec![0f32; 6 * self.node_count()];
        let (nx, ny, nz) = (self.nx, self.ny, self.nz);
        let (mx, my) = (self.mx, self.my);
        let eps = &self.eps;
        // TI: the diagonal is the weighted sum of BOTH tensors' diagonals,
        // matching the two-KE matvec exactly (an approximate diagonal would
        // still converge, just slower — but a WRONG one hides real errors).
        let ti_blocks = self.ti.as_ref().map(|t| (ke_diag_blocks(&t.ke64), &t.eps));
        let cut = self.cut.as_ref();
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
                            let ci = (cz * ny + cy) * nx + cx;
                            let e = eps[ci];
                            let ei = ti_blocks.as_ref().map_or(0.0, |(_, te)| te[ci]);
                            if e <= 0.0 && ei <= 0.0 {
                                continue;
                            }
                            any = true;
                            let l = OFF_TO_LOCAL[dx][dy][dz];
                            if e > 0.0 {
                                // Must mirror the matvec exactly: a cut cell
                                // contributes ITS diagonal, not the reference
                                // cube's. A merely-approximate diagonal still
                                // converges, just slower — but a wrong one
                                // masks real errors, so keep them identical.
                                match cut.and_then(|c| c.get(ci)) {
                                    Some((ke, inv_occ)) => {
                                        let w = (e * inv_occ) as f64;
                                        for r in 0..3 {
                                            for c in 0..3 {
                                                acc[r][c] +=
                                                    w * ke[3 * l + r][3 * l + c] as f64;
                                            }
                                        }
                                    }
                                    None => {
                                        for r in 0..3 {
                                            for c in 0..3 {
                                                acc[r][c] += e as f64 * blocks[l][r][c];
                                            }
                                        }
                                    }
                                }
                            }
                            if ei > 0.0 {
                                let (tb, _) = ti_blocks.as_ref().unwrap();
                                for r in 0..3 {
                                    for c in 0..3 {
                                        acc[r][c] += ei as f64 * tb[l][r][c];
                                    }
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

    /// One cell's full contribution to `y = K x`: solid tensor, plus the TI
    /// infill tensor when an overlay is present.
    ///
    /// Cost is bounded by the cell partition, not doubled: almost every cell
    /// is pure — skin (`ei == 0`) or pure infill (`e == 0`) — and only the
    /// composite-skin band pays for both. The zero test costs one compare
    /// against the 576 FMAs it skips.
    ///
    /// # Safety
    /// Concurrent callers must not target cells sharing nodes (color rule);
    /// sequential callers are always fine.
    #[inline(always)]
    unsafe fn apply_cell_both<const COLUMN: bool>(
        &self,
        ci: usize,
        x: &[f32],
        ys: &UnsafeSlice<f32>,
    ) {
        let e = self.eps[ci];
        if e > 0.0 {
            // A cut cell carries its own exactly-integrated matrix; `eps` then
            // has to shed the occupancy it already contains (see CutStiffness).
            match self.cut.as_ref().and_then(|c| c.get(ci)) {
                Some((ke, inv_occ)) => unsafe {
                    self.apply_cell::<COLUMN>(ci, e * inv_occ, ke, x, ys)
                },
                None => unsafe { self.apply_cell::<COLUMN>(ci, e, &self.ke, x, ys) },
            }
        }
        if let Some(t) = &self.ti {
            let ei = t.eps[ci];
            if ei > 0.0 {
                unsafe { self.apply_cell::<COLUMN>(ci, ei, &t.ke, x, ys) };
            }
        }
    }

    /// One cell's scatter-add contribution to y = K x for ONE reference
    /// element matrix.
    /// # Safety
    /// Concurrent callers must not target cells sharing nodes (color rule);
    /// sequential callers are always fine.
    #[inline(always)]
    unsafe fn apply_cell<const COLUMN: bool>(
        &self,
        ci: usize,
        e: f32,
        ke: &[[f32; 24]; 24],
        x: &[f32],
        ys: &UnsafeSlice<f32>,
    ) {
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
        // See KERNEL_COLUMN: identical arithmetic, different reduction order.
        let mut yl = [0f32; 24];
        if COLUMN {
            for j in 0..24 {
                let xj = xl[j];
                let row = &ke[j];
                for i in 0..24 {
                    yl[i] += row[i] * xj;
                }
            }
            for v in yl.iter_mut() {
                *v *= e;
            }
        } else {
            for i in 0..24 {
                let row = &ke[i];
                let mut s = 0f32;
                for j in 0..24 {
                    s += row[j] * xl[j];
                }
                yl[i] = e * s;
            }
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
        if KERNEL_COLUMN {
            self.apply_k32::<true>(x, y)
        } else {
            self.apply_k32::<false>(x, y)
        }
    }

    fn apply_k32<const COLUMN: bool>(&self, x: &[f32], y: &mut [f32]) {
        debug_assert_eq!(x.len(), self.ndof());
        debug_assert_eq!(y.len(), self.ndof());
        self.fill_live_f32(y);
        {
            let ys = UnsafeSlice::new(y);
            // Threaded: two phases of z-slabs; same-phase slabs never share nodes.
            #[cfg(feature = "parallel")]
            for phase in 0..2 {
                let slabs: Vec<&Vec<u32>> = self.slabs.iter().skip(phase).step_by(2).collect();
                // SAFETY: same-phase slabs are ≥2 cells apart in z.
                par::for_each(&slabs, |slab| {
                    for &ci in slab.iter() {
                        // SAFETY: same-phase slabs are ≥2 cells apart in z.
                        unsafe { self.apply_cell_both::<COLUMN>(ci as usize, x, &ys) };
                    }
                });
            }
            // Sequential: no races possible — one natural-order pass.
            #[cfg(not(feature = "parallel"))]
            for ci in 0..self.eps.len() {
                if self.cell_active(ci) {
                    // SAFETY: sequential.
                    unsafe { self.apply_cell_both::<COLUMN>(ci, x, &ys) };
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
        // Rigid remote masses: rank-6 post-pass, empty (skipped) for no-rigid.
        for g in &self.rigid {
            g.accumulate_f32(x, y);
        }
        self.mask_live_f32(y);
    }

    /// f64 twin of `apply_cell_both` (outer-CG operator), on caller-supplied
    /// exact stiffness fields.
    /// # Safety
    /// Same disjoint-scatter rule as `apply_cell`.
    #[inline(always)]
    unsafe fn apply_cell64_both<const COLUMN: bool>(
        &self,
        ci: usize,
        eps: &[f32],
        ti_eps: Option<&[f32]>,
        x: &[f64],
        ys: &UnsafeSlice<f64>,
    ) {
        let e = eps[ci];
        if e > 0.0 {
            match self.cut.as_ref().and_then(|c| c.get(ci)) {
                // Cut cells are stored f32-only; the cast is 576 ops against
                // 576 multiply-adds, and this is the outer-CG operator, not the
                // V-cycle bulk. Storing an f64 twin would double the dominant
                // memory stream for no accuracy — the matrix entries came from
                // an f32 round-trip either way.
                Some((ke, inv_occ)) => unsafe {
                    self.apply_cell64_f32ke::<COLUMN>(ci, (e * inv_occ) as f64, ke, x, ys)
                },
                None => unsafe { self.apply_cell64::<COLUMN>(ci, e as f64, &self.ke64, x, ys) },
            }
        }
        if let (Some(t), Some(te)) = (&self.ti, ti_eps) {
            let ei = te[ci];
            if ei > 0.0 {
                unsafe { self.apply_cell64::<COLUMN>(ci, ei as f64, &t.ke64, x, ys) };
            }
        }
    }

    /// `apply_cell64` reading an f32 element matrix — the cut-cell path, which
    /// stores f32 only. Arithmetic stays f64; just the matrix entries are cast.
    /// # Safety
    /// Same disjoint-scatter rule as `apply_cell`.
    #[inline(always)]
    unsafe fn apply_cell64_f32ke<const COLUMN: bool>(
        &self,
        ci: usize,
        e: f64,
        ke: &[[f32; 24]; 24],
        x: &[f64],
        ys: &UnsafeSlice<f64>,
    ) {
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
        if COLUMN {
            for j in 0..24 {
                let xj = xl[j];
                let row = &ke[j];
                for i in 0..24 {
                    yl[i] += row[i] as f64 * xj;
                }
            }
            for v in yl.iter_mut() {
                *v *= e;
            }
        } else {
            for i in 0..24 {
                let row = &ke[i];
                let mut s = 0f64;
                for j in 0..24 {
                    s += row[j] as f64 * xl[j];
                }
                yl[i] = e * s;
            }
        }
        for l in 0..8 {
            let n = nidx[l];
            *ys.get_mut(3 * n) += yl[3 * l];
            *ys.get_mut(3 * n + 1) += yl[3 * l + 1];
            *ys.get_mut(3 * n + 2) += yl[3 * l + 2];
        }
    }

    /// Attach exact cut-cell element matrices to THIS level (must be the
    /// finest). Rebuilds the block-Jacobi diagonal and the Chebyshev eigenvalue
    /// estimate, both of which depend on the element matrices.
    pub fn set_cut(&mut self, cut: crate::cutcell::CutStiffness) {
        self.cut = Some(cut);
        self.build_dinv();
        self.refresh_lmax();
    }

    /// f64 twin of `apply_cell` (outer-CG operator).
    /// # Safety
    /// Same disjoint-scatter rule as `apply_cell`.
    #[inline(always)]
    unsafe fn apply_cell64<const COLUMN: bool>(
        &self,
        ci: usize,
        e: f64,
        ke: &[[f64; 24]; 24],
        x: &[f64],
        ys: &UnsafeSlice<f64>,
    ) {
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
        if COLUMN {
            for j in 0..24 {
                let xj = xl[j];
                let row = &ke[j];
                for i in 0..24 {
                    yl[i] += row[i] * xj;
                }
            }
            for v in yl.iter_mut() {
                *v *= e;
            }
        } else {
            for i in 0..24 {
                let row = &ke[i];
                let mut s = 0f64;
                for j in 0..24 {
                    s += row[j] * xl[j];
                }
                yl[i] = e * s;
            }
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
        let ti_eps = self.ti.as_ref().map(|t| t.eps.as_slice());
        self.apply64_eps(&self.eps, ti_eps, x, y);
    }

    /// `apply64` with explicit per-cell stiffness fields: the outer CG runs
    /// on the EXACT eps while the level hierarchy (preconditioner only) may
    /// hold contrast-clamped values. Both must share this level's void set.
    /// `ti_eps` must be `Some` exactly when this level has a TI overlay —
    /// passing `None` to a TI level would silently solve the skin alone.
    pub fn apply64_eps(&self, eps: &[f32], ti_eps: Option<&[f32]>, x: &[f64], y: &mut [f64]) {
        debug_assert_eq!(ti_eps.is_some(), self.ti.is_some());
        if KERNEL_COLUMN {
            self.apply64_k::<true>(eps, ti_eps, x, y)
        } else {
            self.apply64_k::<false>(eps, ti_eps, x, y)
        }
    }

    fn apply64_k<const COLUMN: bool>(
        &self,
        eps: &[f32],
        ti_eps: Option<&[f32]>,
        x: &[f64],
        y: &mut [f64],
    ) {
        debug_assert_eq!(x.len(), self.ndof());
        debug_assert_eq!(y.len(), self.ndof());
        debug_assert_eq!(eps.len(), self.eps.len());
        self.fill_live_f64(y);
        {
            let ys = UnsafeSlice::new(y);
            #[cfg(feature = "parallel")]
            for phase in 0..2 {
                let slabs: Vec<&Vec<u32>> = self.slabs.iter().skip(phase).step_by(2).collect();
                // SAFETY: same-phase slabs are ≥2 cells apart in z.
                par::for_each(&slabs, |slab| {
                    for &ci in slab.iter() {
                        // SAFETY: same-phase slabs are ≥2 cells apart in z.
                        unsafe { self.apply_cell64_both::<COLUMN>(ci as usize, eps, ti_eps, x, &ys) };
                    }
                });
            }
            #[cfg(not(feature = "parallel"))]
            for ci in 0..eps.len() {
                if eps[ci] > 0.0 || ti_eps.is_some_and(|t| t[ci] > 0.0) {
                    // SAFETY: sequential.
                    unsafe { self.apply_cell64_both::<COLUMN>(ci, eps, ti_eps, x, &ys) };
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
        // Rigid remote masses: rank-6 post-pass, empty (skipped) for no-rigid.
        for g in &self.rigid {
            g.accumulate_f64(x, y);
        }
        // Mask constrained DOFs.
        self.mask_live_f64(y);
    }

    /// Fused Chebyshev step: per node `w = Dinv·(r − t); d = a·d + b·w;
    /// z += d` in ONE pass (element-wise identical to the former
    /// dinv_residual → axpby → axpy sequence, which made three). The smoother
    /// is bandwidth-bound and dinv is its dominant stream, so fewer passes is
    /// the whole game.
    fn cheb_step_fused(&self, z: &mut [f32], d: &mut [f32], r: &[f32], t: &[f32], a: f32, b: f32) {
        let dinv = &self.dinv;
        let live = &self.live;
        par::chunks2_mut_indexed(z, d, 3 * NODE_CHUNK, |off, zc, dc| {
            let n0 = off / 3;
            for (bi, (zb, db)) in zc.chunks_mut(3 * BLK).zip(dc.chunks_mut(3 * BLK)).enumerate() {
                let nb = n0 + bi * BLK;
                if !live[nb / BLK] {
                    continue; // dead block: every value here is (and stays) 0
                }
                for (k, (zn, dn)) in zb.chunks_mut(3).zip(db.chunks_mut(3)).enumerate() {
                    let n = nb + k;
                    let di = &dinv[6 * n..6 * n + 6];
                    let rr = [
                        r[3 * n] - t[3 * n],
                        r[3 * n + 1] - t[3 * n + 1],
                        r[3 * n + 2] - t[3 * n + 2],
                    ];
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
            }
        });
    }

    /// Fused zero-guess first step: `z = d = f·Dinv·r` in one pass
    /// (replaces fill + diag_apply + axpby + axpy).
    fn cheb_first_fused(&self, z: &mut [f32], d: &mut [f32], r: &[f32], f: f32) {
        let dinv = &self.dinv;
        let live = &self.live;
        par::chunks2_mut_indexed(z, d, 3 * NODE_CHUNK, |off, zc, dc| {
            let n0 = off / 3;
            for (bi, (zb, db)) in zc.chunks_mut(3 * BLK).zip(dc.chunks_mut(3 * BLK)).enumerate() {
                let nb = n0 + bi * BLK;
                if !live[nb / BLK] {
                    // Dead block: `dinv` is zero here, so the value this would
                    // write IS zero, and nothing else in the cycle ever writes a
                    // non-zero into a dead DOF (`prolong_add` skips constrained
                    // DOFs, `coarse_pcg` zero-fills). The invariant is asserted
                    // in debug builds; release just skips the cache lines.
                    debug_assert!(zb.iter().chain(db.iter()).all(|v| *v == 0.0));
                    continue;
                }
                for (k, (zn, dn)) in zb.chunks_mut(3).zip(db.chunks_mut(3)).enumerate() {
                    let n = nb + k;
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
        if degree == 0 {
            return;
        }
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
    /// `y[..] = 0` and `y[constrained] = 0`, restricted to LIVE blocks: the
    /// scatter in `apply` only ever writes nodes incident to a non-void cell,
    /// which are live by construction, so dead blocks are already zero and
    /// need neither clearing nor masking.
    fn fill_live_f32(&self, y: &mut [f32]) {
        let live = &self.live;
        par::chunks_mut_indexed(y, 3 * NODE_CHUNK, |off, yc| {
            let n0 = off / 3;
            for (bi, yb) in yc.chunks_mut(3 * BLK).enumerate() {
                if live[(n0 + bi * BLK) / BLK] {
                    yb.fill(0.0);
                } else {
                    // CONTRACT: callers hand in a buffer that is already zero at
                    // dead DOFs (all of them allocate zeroed and only ever write
                    // through masked kernels). Nothing clears them, so a dirty
                    // buffer would leak stale values into the result.
                    debug_assert!(yb.iter().all(|v| *v == 0.0));
                }
            }
        });
    }

    fn mask_live_f32(&self, y: &mut [f32]) {
        let live = &self.live;
        let constrained = &self.constrained;
        par::chunks_mut_indexed(y, 3 * NODE_CHUNK, |off, yc| {
            let n0 = off / 3;
            for (bi, yb) in yc.chunks_mut(3 * BLK).enumerate() {
                let nb = n0 + bi * BLK;
                if !live[nb / BLK] {
                    continue;
                }
                for (i, v) in yb.iter_mut().enumerate() {
                    if constrained[3 * nb + i] {
                        *v = 0.0;
                    }
                }
            }
        });
    }

    fn fill_live_f64(&self, y: &mut [f64]) {
        let live = &self.live;
        par::chunks_mut_indexed64(y, 3 * NODE_CHUNK, |off, yc| {
            let n0 = off / 3;
            for (bi, yb) in yc.chunks_mut(3 * BLK).enumerate() {
                if live[(n0 + bi * BLK) / BLK] {
                    yb.fill(0.0);
                } else {
                    // CONTRACT: callers hand in a buffer that is already zero at
                    // dead DOFs (all of them allocate zeroed and only ever write
                    // through masked kernels). Nothing clears them, so a dirty
                    // buffer would leak stale values into the result.
                    debug_assert!(yb.iter().all(|v| *v == 0.0));
                }
            }
        });
    }

    fn mask_live_f64(&self, y: &mut [f64]) {
        let live = &self.live;
        let constrained = &self.constrained;
        par::chunks_mut_indexed64(y, 3 * NODE_CHUNK, |off, yc| {
            let n0 = off / 3;
            for (bi, yb) in yc.chunks_mut(3 * BLK).enumerate() {
                let nb = n0 + bi * BLK;
                if !live[nb / BLK] {
                    continue;
                }
                for (i, v) in yb.iter_mut().enumerate() {
                    if constrained[3 * nb + i] {
                        *v = 0.0;
                    }
                }
            }
        });
    }

    /// `out = a - b` over LIVE blocks (both are zero elsewhere).
    fn sub_live(&self, out: &mut [f32], a: &[f32], b: &[f32]) {
        let live = &self.live;
        par::chunks_mut_indexed(out, 3 * NODE_CHUNK, |off, oc| {
            let n0 = off / 3;
            for (bi, ob) in oc.chunks_mut(3 * BLK).enumerate() {
                let nb = n0 + bi * BLK;
                if !live[nb / BLK] {
                    debug_assert!(ob.iter().all(|v| *v == 0.0));
                    continue;
                }
                for (i, o) in ob.iter_mut().enumerate() {
                    *o = a[3 * nb + i] - b[3 * nb + i];
                }
            }
        });
    }

    fn diag_apply(&self, r: &[f32], z: &mut [f32]) {
        let dinv = &self.dinv;
        let live = &self.live;
        par::chunks_mut_indexed(z, 3 * NODE_CHUNK, |off, zc| {
            let n0 = off / 3;
            for (bi, zb) in zc.chunks_mut(3 * BLK).enumerate() {
                let nb = n0 + bi * BLK;
                if !live[nb / BLK] {
                    // `z` is fully OVERWRITTEN here, so a dead block must end up
                    // zero — which it already is (dinv = 0 ⇒ the product is 0).
                    debug_assert!(zb.iter().all(|v| *v == 0.0));
                    continue;
                }
                for (k, zn) in zb.chunks_mut(3).enumerate() {
                    let n = nb + k;
                    let d = &dinv[6 * n..6 * n + 6];
                    let rr = [r[3 * n], r[3 * n + 1], r[3 * n + 2]];
                    zn[0] = d[0] * rr[0] + d[1] * rr[1] + d[2] * rr[2];
                    zn[1] = d[1] * rr[0] + d[3] * rr[1] + d[4] * rr[2];
                    zn[2] = d[2] * rr[0] + d[4] * rr[1] + d[5] * rr[2];
                }
            }
        });
    }
}

/// Raise non-void cells to the preconditioner stiffness floor. Returns true
/// if anything changed (callers then refresh dinv/lmax).
fn clamp_pc_eps(eps: &mut [f32]) -> bool {
    let floor = PC_EPS_FLOOR;
    let mut changed = false;
    for e in eps.iter_mut() {
        if *e > 0.0 && *e < floor {
            *e = floor;
            changed = true;
        }
    }
    changed
}

/// Trilinear parent weights of fine node coordinate x (even: one parent).
#[inline]
fn parent_weights(x: usize, coarsened: bool) -> [(usize, f64); 2] {
    if !coarsened {
        // Axis not halved: the coarse node IS this node.
        return [(x, 1.0), (0, 0.0)];
    }
    if x % 2 == 0 {
        [(x / 2, 1.0), (0, 0.0)]
    } else {
        [(x / 2, 0.5), (x / 2 + 1, 0.5)]
    }
}

// NEGATIVE RESULTS, round 2 (2026-07-25 solver study; fixtures = 3DBenchy,
// MicHolder, hook, surface, solid beam, plates, each at 100k/300k/1M solid
// cells). Everything here was measured and REJECTED — do not re-tread:
//   (a) HIERARCHY DEPTH is saturated. 3/4/5/6/7/8 levels on the Benchy give
//       205/222/223/223/223/223 iterations; 5 is the wall-clock optimum. More
//       coarse levels buy nothing.
//   (b) EXTRA COARSE-LEVEL SMOOTHING (nu growing 1.4-3.5x per level, nearly
//       free at 8^-l cost) cuts iterations 223 -> 187 but LOSES on wall clock:
//       coarse levels are not 8x cheaper on a shell, where the node count
//       shrinks far slower than the cell count.
//   (c) SMOOTHER DEGREE is at its optimum. nu=1/2/3/4 give 389/264/223/196
//       iterations; nu=2 is ~10 % faster on the HARD fixtures but blows up the
//       easy ones (solid-beam bending 7 -> 31 iterations, modal outer iterations
//       5 -> 11), which is the wrong trade. NU1 must equal NU2 — an asymmetric
//       V-cycle is not an SPD preconditioner and CG stalls outright (nu 2/3
//       needed 3000+ iterations).
//   (d) CHEBYSHEV INTERVAL: ratio 4/8/12/20/30 -> 244/223/215/211/214. Ratio 20
//       is ~5 % better on hard parts and worse on easy ones; not worth it.
//   (e) lmax ESTIMATE is not the limit: 8/20/50 power iterations and safety
//       1.1/1.3/1.6/2.0 all make it monotonically WORSE (a gentler polynomial).
//   (f) CONTRAST is not the limit. The occupancy field has no near-zero cells
//       (0 below eps 0.05 on the Benchy), and flooring the EXACT operator's eps
//       at 1e-4..0.2 changes the iteration count by at most 1.
//   (g) FLEXIBLE (Polak-Ribiere) CG — the standard fix for an inexact
//       preconditioner — changes NO fixture's iteration count. See solve_warm.
//   (h) NESTED / FMG START (solve at 2h, prolong as the initial guess) cuts
//       iterations only 5-12 % (Benchy 223 -> 196) and the coarse solve costs
//       more than that: the coarse problem is nearly as ill-conditioned as the
//       fine one (187 iterations for MicHolder at 2h). The difficulty is
//       present at EVERY scale, so no hierarchy trick removes it.
// Conclusion: the residual iteration count on jagged, thin-walled voxel parts
// is a property of the GEOMETRY, not of the cycle. Optimize cost per iteration.
//
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
    let live = &coarse.live;
    // Per-axis: a HALVED axis gathers the 3-point trilinear stencil; an axis the
    // coarsening skipped is identity (one point, weight 1).
    let c = coarse.from_parent;
    let rng = |on: bool| if on { -1isize..=1 } else { 0..=0 };
    let step = |on: bool| if on { 2isize } else { 1 };
    par::chunks_mut_indexed(out, 3 * NODE_CHUNK, |off, oc| {
        let n0 = off / 3;
        for (bi, ob) in oc.chunks_mut(3 * BLK).enumerate() {
            let nb = n0 + bi * BLK;
            if !live[nb / BLK] {
                // Every DOF of a dead coarse node is constrained, so the write
                // below would be 0 — and `out` is already 0 there.
                debug_assert!(ob.iter().all(|v| *v == 0.0));
                continue;
            }
            for (k, on) in ob.chunks_mut(3).enumerate() {
            let nc = nb + k;
            let xx = nc % cmx;
            let yy = (nc / cmx) % cmy;
            let zz = nc / (cmx * cmy);
            let (fx, fy, fz) =
                (step(c[0]) * xx as isize, step(c[1]) * yy as isize, step(c[2]) * zz as isize);
            let mut acc = [0f64; 3];
            for dz in rng(c[2]) {
                let z = fz + dz;
                if z < 0 || z >= fine.mz as isize {
                    continue;
                }
                let wz = 1.0 - 0.5 * dz.abs() as f64;
                for dy in rng(c[1]) {
                    let y = fy + dy;
                    if y < 0 || y >= fine.my as isize {
                        continue;
                    }
                    let wy = 1.0 - 0.5 * dy.abs() as f64;
                    for dx in rng(c[0]) {
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
        }
    });
}

/// z_f += P z_c (trilinear interpolation), skipping fine constrained DOFs.
fn prolong_add(fine: &Level, coarse: &Level, zc: &[f32], zf: &mut [f32]) {
    let (cmx, cmy) = (coarse.mx, coarse.my);
    let (fmx, fmy) = (fine.mx, fine.my);
    let constrained = &fine.constrained;
    let live = &fine.live;
    let c = coarse.from_parent;
    par::chunks_mut_indexed(zf, 3 * NODE_CHUNK, |off, fc| {
        let n0 = off / 3;
        for (bi, fb) in fc.chunks_mut(3 * BLK).enumerate() {
            let nb = n0 + bi * BLK;
            if !live[nb / BLK] {
                continue; // all DOFs constrained ⇒ the += below is skipped anyway
            }
            for (k, fnode) in fb.chunks_mut(3).enumerate() {
            let nf = nb + k;
            let x = nf % fmx;
            let y = (nf / fmx) % fmy;
            let z = nf / (fmx * fmy);
            let (xw, yw, zw) =
                (parent_weights(x, c[0]), parent_weights(y, c[1]), parent_weights(z, c[2]));
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
    {
        level.cheb_smooth(&mut ws.z[l], &ws.r[l], &mut ws.t[l], &mut ws.d[l], NU1, true);
    }
    // Coarse-grid correction.
    {
        level.apply(&ws.z[l], &mut ws.t[l]);
        level.sub_live(&mut ws.t2[l], &ws.r[l], &ws.t[l]);
    }
    {
        restrict(level, &ws.t2[l], &levels[l + 1], &mut ws.r[l + 1]);
    }
    v_cycle(levels, ws, l + 1);
    {
        let (za, zb) = ws.z.split_at_mut(l + 1);
        prolong_add(level, &levels[l + 1], &zb[0], &mut za[l]);
    }
    // Post-smooth.
    {
        level.cheb_smooth(&mut ws.z[l], &ws.r[l], &mut ws.t[l], &mut ws.d[l], NU2, false);
    }
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
    /// EXACT finest-level TI infill weights, the overlay's twin of
    /// `eps_exact`. `Some` exactly when the finest level has a TI overlay.
    ti_eps_exact: Option<Vec<f32>>,
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

#[derive(Clone)]
pub struct SolveStats {
    pub iterations: usize,
    pub rel_residual: f64,
    pub converged: bool,
}

impl MgSolver {
    /// Attach exact cut-cell element matrices to the FINEST level.
    ///
    /// Coarse levels keep the `occupancy · KE_full` ersatz on purpose: they are
    /// preconditioner-only, and the outer CG operator (`apply64`) runs on the
    /// finest level, so the answer is exact regardless. Same fine-level-only
    /// discipline as the rigid remote-mass coupling.
    pub fn set_cut(&mut self, cut: crate::cutcell::CutStiffness) {
        self.levels[0].set_cut(cut);
    }

    /// Coarsen while dimensions stay even and at least 2 cells per axis.
    pub fn new(mut finest: Level, max_levels: usize) -> Self {
        let eps_exact = finest.eps.clone();
        let ti_eps_exact = finest.ti.as_ref().map(|t| t.eps.clone());
        // Both fields take the floor independently. A pure-infill cell keeps
        // `eps == 0` (the clamp never lifts a zero), so flooring the solid
        // share cannot invent skin where there is none.
        let mut changed = clamp_pc_eps(&mut finest.eps);
        if let Some(t) = finest.ti.as_mut() {
            changed |= clamp_pc_eps(&mut t.eps);
        }
        if changed {
            finest.build_dinv();
            finest.refresh_lmax();
        }
        let mut levels = vec![finest];
        while levels.len() < max_levels {
            // SEMICOARSENING: halve every axis that can still take it, and stop
            // only when NO axis can. Full coarsening required all three axes to
            // be splittable, so one thin axis (a 3-cell plate, a slender rib)
            // collapsed the whole hierarchy to a single level and left the
            // coarse-grid PCG doing the entire solve.
            let c = levels.last().unwrap().splittable();
            if !c.iter().any(|&v| v) {
                break;
            }
            let coarse = levels.last().unwrap().coarsen(c);
            levels.push(coarse);
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
        Self { levels, ws, eps_exact, ti_eps_exact, last_trace: Vec::new() }
    }

    /// The exact (unclamped) finest-level eps this solver was last given.
    pub fn eps_exact(&self) -> &[f32] {
        &self.eps_exact
    }

    /// The exact finest-level TI infill weights, `Some` iff this solver was
    /// built with a TI overlay (DESIGN §22).
    pub fn ti_eps_exact(&self) -> Option<&[f32]> {
        self.ti_eps_exact.as_deref()
    }

    /// Update per-cell stiffness factors in place (same void/solid topology!)
    /// and refresh the smoother diagonals down the hierarchy. Cheap compared
    /// to rebuilding levels — the optimization loop calls this every iteration.
    pub fn update_eps(&mut self, eps: Vec<f32>) {
        assert!(
            self.ti_eps_exact.is_none(),
            "a TI solver must be updated through update_eps_ti — update_eps \
             would leave the infill weights frozen at their initial values"
        );
        self.update_eps_fields(eps, None);
    }

    /// [`Self::update_eps`] for a TI solver (DESIGN §22): both weight fields
    /// change together every optimizer iteration.
    pub fn update_eps_ti(&mut self, eps: Vec<f32>, eps_infill: Vec<f32>) {
        assert!(
            self.ti_eps_exact.is_some(),
            "update_eps_ti called on a solver built without a TI overlay"
        );
        self.update_eps_fields(eps, Some(eps_infill));
    }

    fn update_eps_fields(&mut self, eps: Vec<f32>, eps_infill: Option<Vec<f32>>) {
        debug_assert_eq!(eps.len(), self.levels[0].eps.len());
        self.eps_exact = eps.clone();
        self.levels[0].eps = eps;
        clamp_pc_eps(&mut self.levels[0].eps);
        if let Some(ei) = eps_infill {
            debug_assert_eq!(ei.len(), self.levels[0].eps.len());
            self.ti_eps_exact = Some(ei.clone());
            let t = self.levels[0].ti.as_mut().expect("TI overlay checked by caller");
            t.eps = ei;
            clamp_pc_eps(&mut t.eps);
        }
        self.levels[0].build_dinv();
        self.levels[0].refresh_lmax();
        for l in 1..self.levels.len() {
            let f = &self.levels[l - 1];
            // Replay the SAME per-axis coarsening the hierarchy was built with.
            let from_parent = self.levels[l].from_parent;
            let coarse = average_coarse_eps(&f.eps, f.nx, f.ny, f.nz, from_parent);
            let coarse_ti = f
                .ti
                .as_ref()
                .map(|t| average_coarse_eps(&t.eps, f.nx, f.ny, f.nz, from_parent));
            self.levels[l].eps = coarse;
            if let (Some(t), Some(ct)) = (self.levels[l].ti.as_mut(), coarse_ti) {
                t.eps = ct;
            }
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
        self.levels[0].apply64_eps(&self.eps_exact, self.ti_eps_exact.as_deref(), x, y);
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
        // Live-set mask of the finest level: every solver vector is identically
        // zero outside it, so the CG vector ops skip those blocks (bit-identical
        // results, ~80 % less traffic on a sparse part).
        let (lv, lb) = {
            let (m, s) = self.levels[0].live_mask();
            (m.to_vec(), s)
        };
        let lv: &[bool] = &lv;
        let norm_b = par::norm2_64(b);
        if norm_b == 0.0 {
            u.fill(0.0);
            return SolveStats { iterations: 0, rel_residual: 0.0, converged: true };
        }
        // r = b - A u0
        let mut r = vec![0f64; n];
        self.levels[0].apply64_eps(&self.eps_exact, self.ti_eps_exact.as_deref(), u, &mut r);
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

        par::demote_live(&mut self.ws.r[0], &r, lv, lb);
        v_cycle(&self.levels, &mut self.ws, 0);
        par::promote(&mut p, &self.ws.z[0]);
        let mut rz = par::dot_mixed_live(&r, &self.ws.z[0], lv, lb);

        let mut res = f64::INFINITY;
        for it in 0..max_iter {
            // Cooperative cancel: bail like an iteration-cap hit; the caller
            // checks `cancel::requested()` and raises the Cancelled error.
            if crate::cancel::requested() {
                return SolveStats { iterations: it, rel_residual: res, converged: false };
            }
            {
                self.levels[0].apply64_eps(&self.eps_exact, self.ti_eps_exact.as_deref(), &p, &mut q);
            }
            let pq = par::dot64_live(&p, &q, lv, lb);
            if !pq.is_finite() || pq <= 0.0 {
                return SolveStats { iterations: it, rel_residual: res, converged: false };
            }
            let alpha = rz / pq;
            par::axpy64_live(u, alpha, &p, lv, lb);
            par::axpy64_live(&mut r, -alpha, &q, lv, lb);
            res = par::norm2_64_live(&r, lv, lb) / norm_b;
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
            par::demote_live(&mut self.ws.r[0], &r, lv, lb);
            v_cycle(&self.levels, &mut self.ws, 0);
            let rz_new = par::dot_mixed_live(&r, &self.ws.z[0], lv, lb);
            // NEGATIVE RESULT: a flexible (Polak-Ribiere) beta, the textbook
            // remedy for an inexact preconditioner, moved the iteration count on
            // NO fixture. The f32 V-cycle really does act as a fixed SPD
            // operator; the residual bumps seen on jagged parts are ordinary CG
            // non-monotonicity (norm(r) oscillates while the A-norm falls).
            let beta = rz_new / rz;
            par::xpby_mixed_live(&mut p, &self.ws.z[0], beta, lv, lb);
            rz = rz_new;
        }
        SolveStats { iterations: max_iter, rel_residual: res, converged: false }
    }
}
