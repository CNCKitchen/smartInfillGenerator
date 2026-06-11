//! High-level static solve.
//!
//! Two entry points:
//! - `solve_nodes`: node-level BCs (fixed nodes, penalty springs, nodal forces)
//!   on an already-padded grid — what the interactive app drives via attach.rs.
//! - `solve_static`: axis-aligned box-region BCs (tests, benchmarks).

use crate::fem::ke_hex;
use crate::mg::{Level, MgSolver};
use crate::voxel::VoxelGrid;

/// Relative stiffness floor for non-void gray cells (SIMP-style soft floor).
const EMIN_REL: f32 = 1e-6;

#[derive(Clone, Copy, Debug)]
pub struct BoxRegion {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl BoxRegion {
    pub fn new(min: [f64; 3], max: [f64; 3]) -> Self {
        Self { min, max }
    }

    pub fn contains(&self, p: [f64; 3]) -> bool {
        (0..3).all(|d| p[d] >= self.min[d] && p[d] <= self.max[d])
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SolveSettings {
    /// Young's modulus, MPa (N/mm²).
    pub e0: f64,
    /// Poisson ratio.
    pub nu: f64,
    /// MGCG relative-residual tolerance.
    pub tol: f64,
    pub max_iter: usize,
    pub max_levels: usize,
}

impl Default for SolveSettings {
    fn default() -> Self {
        // max_iter sized from real-part measurements: a 3DBenchy (thin-shell,
        // jagged-boundary worst case) needs 172/249/286 MGCG iterations at the
        // preview/normal/fine presets — 600 leaves 2x margin.
        Self { e0: 2400.0, nu: 0.35, tol: 1e-5, max_iter: 600, max_levels: 5 }
    }
}

pub struct StaticProblem {
    pub grid: VoxelGrid,
    /// Fully fixed supports (all 3 DOFs of active nodes inside the box).
    pub fixed: Vec<BoxRegion>,
    /// Total force vector (N) distributed equally over active nodes in the box.
    pub loads: Vec<(BoxRegion, [f64; 3])>,
    pub settings: SolveSettings,
}

#[derive(Debug)]
pub enum SolveError {
    NoSolidCells,
    NoFixedNodes,
    LoadRegionEmpty(usize),
    NotConverged { iterations: usize, rel_residual: f64 },
}

impl std::fmt::Display for SolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SolveError::NoSolidCells => write!(f, "voxelization produced no solid cells"),
            SolveError::NoFixedNodes => write!(f, "no fixed support selected (model would float)"),
            SolveError::LoadRegionEmpty(i) => write!(f, "load region {i} selects no nodes"),
            SolveError::NotConverged { iterations, rel_residual } => write!(
                f,
                "solver did not converge ({iterations} iterations, residual {rel_residual:.2e})"
            ),
        }
    }
}

impl std::error::Error for SolveError {}

/// Pad grid dimensions so the multigrid hierarchy divides evenly; padding is
/// void and costs nothing. Returns the padded grid and the level count.
pub fn pad_for_levels(grid: &VoxelGrid, max_levels: usize) -> (VoxelGrid, usize) {
    let min_dim = grid.nx.min(grid.ny).min(grid.nz);
    let mut levels = 1usize;
    while levels < max_levels && (min_dim >> levels) >= 2 && (min_dim >> (levels - 1)) >= 4 {
        levels += 1;
    }
    let mult = 1usize << (levels - 1);
    let pad = |n: usize| n.div_ceil(mult) * mult;
    let (nx, ny, nz) = (pad(grid.nx), pad(grid.ny), pad(grid.nz));
    let mut scale = vec![0f32; nx * ny * nz];
    for cz in 0..grid.nz {
        for cy in 0..grid.ny {
            let src = (cz * grid.ny + cy) * grid.nx;
            let dst = (cz * ny + cy) * nx;
            scale[dst..dst + grid.nx].copy_from_slice(&grid.scale[src..src + grid.nx]);
        }
    }
    (VoxelGrid { nx, ny, nz, h: grid.h, origin: grid.origin, scale }, levels)
}

/// Per-node "touches at least one solid cell" mask.
pub fn active_nodes(grid: &VoxelGrid) -> Vec<bool> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let mut active = vec![false; mx * my * mz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                if grid.scale[(cz * ny + cy) * nx + cx] > 0.0 {
                    for oz in 0..2 {
                        for oy in 0..2 {
                            for ox in 0..2 {
                                active[((cz + oz) * my + cy + oy) * mx + cx + ox] = true;
                            }
                        }
                    }
                }
            }
        }
    }
    active
}

/// Active nodes with at least one void/out-of-grid incident cell — the
/// surface nodes that loads and supports attach to.
pub fn boundary_nodes(grid: &VoxelGrid) -> Vec<u32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let mut out = Vec::new();
    for z in 0..mz {
        for y in 0..my {
            for x in 0..mx {
                let mut solid = 0usize;
                let mut total = 0usize;
                for dz in 0..2usize {
                    for dy in 0..2usize {
                        for dx in 0..2usize {
                            if dx > x || dy > y || dz > z {
                                continue;
                            }
                            let (cx, cy, cz) = (x - dx, y - dy, z - dz);
                            if cx < nx && cy < ny && cz < nz {
                                total += 1;
                                if grid.scale[(cz * ny + cy) * nx + cx] > 0.0 {
                                    solid += 1;
                                }
                            }
                        }
                    }
                }
                if solid >= 1 && (solid < total || total < 8) {
                    out.push(((z * my + y) * mx + x) as u32);
                }
            }
        }
    }
    out
}

/// Node-level boundary conditions on a padded grid.
#[derive(Clone, Debug, Default)]
pub struct NodeProblem {
    pub fixed: Vec<u32>,
    /// (node, unit direction, stiffness N/mm) penalty springs.
    pub springs: Vec<(u32, [f64; 3], f64)>,
    /// Per-node force vectors (N); duplicates accumulate.
    pub forces: Vec<(u32, [f64; 3])>,
}

pub struct Solution {
    /// Nodal displacements (mm), 3 per node, on the PADDED node grid.
    pub u: Vec<f32>,
    pub mx: usize,
    pub my: usize,
    pub mz: usize,
    pub h: f64,
    pub origin: [f64; 3],
    /// Node is attached to at least one solid cell.
    pub active: Vec<bool>,
    pub iterations: usize,
    pub rel_residual: f64,
    /// False when the iteration cap hit before `tol` — the field is still
    /// the best available approximation (residual reported above).
    pub converged: bool,
}

impl Solution {
    #[inline]
    pub fn node_pos(&self, n: usize) -> [f64; 3] {
        let x = n % self.mx;
        let y = (n / self.mx) % self.my;
        let z = n / (self.mx * self.my);
        [
            self.origin[0] + x as f64 * self.h,
            self.origin[1] + y as f64 * self.h,
            self.origin[2] + z as f64 * self.h,
        ]
    }

    pub fn node_count(&self) -> usize {
        self.mx * self.my * self.mz
    }

    /// Mean displacement vector over active nodes inside the region.
    pub fn mean_displacement(&self, region: &BoxRegion) -> Option<[f64; 3]> {
        let mut acc = [0f64; 3];
        let mut count = 0usize;
        for n in 0..self.node_count() {
            if self.active[n] && region.contains(self.node_pos(n)) {
                for d in 0..3 {
                    acc[d] += self.u[3 * n + d] as f64;
                }
                count += 1;
            }
        }
        if count == 0 {
            return None;
        }
        Some([acc[0] / count as f64, acc[1] / count as f64, acc[2] / count as f64])
    }

    /// Largest nodal displacement magnitude (mm).
    pub fn max_displacement(&self) -> f64 {
        let mut best = 0f64;
        for n in 0..self.node_count() {
            if !self.active[n] {
                continue;
            }
            let (ux, uy, uz) =
                (self.u[3 * n] as f64, self.u[3 * n + 1] as f64, self.u[3 * n + 2] as f64);
            best = best.max(ux * ux + uy * uy + uz * uz);
        }
        best.sqrt()
    }

    /// Trilinear displacement sample at an arbitrary point (clamped to grid).
    pub fn sample_displacement(&self, p: [f64; 3]) -> [f64; 3] {
        let t = [
            ((p[0] - self.origin[0]) / self.h).clamp(0.0, (self.mx - 1) as f64),
            ((p[1] - self.origin[1]) / self.h).clamp(0.0, (self.my - 1) as f64),
            ((p[2] - self.origin[2]) / self.h).clamp(0.0, (self.mz - 1) as f64),
        ];
        let i0 = [
            (t[0] as usize).min(self.mx - 2),
            (t[1] as usize).min(self.my - 2),
            (t[2] as usize).min(self.mz - 2),
        ];
        let f = [t[0] - i0[0] as f64, t[1] - i0[1] as f64, t[2] - i0[2] as f64];
        let mut out = [0f64; 3];
        for dz in 0..2 {
            for dy in 0..2 {
                for dx in 0..2 {
                    let w = (if dx == 1 { f[0] } else { 1.0 - f[0] })
                        * (if dy == 1 { f[1] } else { 1.0 - f[1] })
                        * (if dz == 1 { f[2] } else { 1.0 - f[2] });
                    let n = ((i0[2] + dz) * self.my + i0[1] + dy) * self.mx + i0[0] + dx;
                    for d in 0..3 {
                        out[d] += w * self.u[3 * n + d] as f64;
                    }
                }
            }
        }
        out
    }
}

/// Solve with node-level BCs. `grid` must already be padded (`pad_for_levels`).
pub fn solve_nodes(
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    s: &SolveSettings,
) -> Result<Solution, SolveError> {
    if grid.solid_count() == 0 {
        return Err(SolveError::NoSolidCells);
    }
    if problem.fixed.is_empty() && problem.springs.is_empty() {
        return Err(SolveError::NoFixedNodes);
    }
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let ndof = 3 * mx * my * mz;

    let mut eps = vec![0f32; nx * ny * nz];
    for (i, &sc) in grid.scale.iter().enumerate() {
        if sc > 0.0 {
            eps[i] = EMIN_REL + (1.0 - EMIN_REL) * sc;
        }
    }

    let mut fixed = vec![false; ndof];
    for &n in &problem.fixed {
        for d in 0..3 {
            fixed[3 * n as usize + d] = true;
        }
    }

    let mut b = vec![0f64; ndof];
    for &(n, f) in &problem.forces {
        for d in 0..3 {
            b[3 * n as usize + d] += f[d];
        }
    }

    let ke64 = ke_hex(s.e0, s.nu, grid.h);
    let finest = Level::new(nx, ny, nz, grid.h, eps, ke64, &fixed, problem.springs.clone());
    for (i, c) in finest.constrained.iter().enumerate() {
        if *c {
            b[i] = 0.0;
        }
    }

    let active = active_nodes(grid);
    let mut solver = MgSolver::new(finest, levels);
    let mut u = vec![0f64; ndof];
    let stats = solver.solve(&b, &mut u, s.tol, s.max_iter);
    // Hitting the cap is reported, not fatal: the iterate is the best
    // available approximation and usually visually indistinguishable.
    Ok(Solution {
        u: u.iter().map(|&v| v as f32).collect(),
        mx,
        my,
        mz,
        h: grid.h,
        origin: grid.origin,
        active,
        iterations: stats.iterations,
        rel_residual: stats.rel_residual,
        converged: stats.converged,
    })
}

pub fn solve_static(problem: &StaticProblem) -> Result<Solution, SolveError> {
    let s = &problem.settings;
    if problem.grid.solid_count() == 0 {
        return Err(SolveError::NoSolidCells);
    }
    let (grid, levels) = pad_for_levels(&problem.grid, s.max_levels);
    let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
    let active = active_nodes(&grid);
    let node_pos = |n: usize| -> [f64; 3] {
        let x = n % mx;
        let y = (n / mx) % my;
        let z = n / (mx * my);
        [
            grid.origin[0] + x as f64 * grid.h,
            grid.origin[1] + y as f64 * grid.h,
            grid.origin[2] + z as f64 * grid.h,
        ]
    };

    let mut np = NodeProblem::default();
    for n in 0..mx * my * mz {
        if active[n] && problem.fixed.iter().any(|r| r.contains(node_pos(n))) {
            np.fixed.push(n as u32);
        }
    }
    if np.fixed.is_empty() {
        return Err(SolveError::NoFixedNodes);
    }
    for (li, (region, force)) in problem.loads.iter().enumerate() {
        let nodes: Vec<usize> =
            (0..mx * my * mz).filter(|&n| active[n] && region.contains(node_pos(n))).collect();
        if nodes.is_empty() {
            return Err(SolveError::LoadRegionEmpty(li));
        }
        let inv = 1.0 / nodes.len() as f64;
        for n in nodes {
            np.forces.push((n as u32, [force[0] * inv, force[1] * inv, force[2] * inv]));
        }
    }
    solve_nodes(&grid, levels, &np, s)
}
