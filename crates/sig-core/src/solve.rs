//! High-level static solve: VoxelGrid + boundary conditions -> displacement
//! field. Phase-1 boundary condition selection is by axis-aligned world-space
//! boxes; the interactive surface picking UI replaces that in Phase 2.

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
        Self { e0: 2400.0, nu: 0.35, tol: 1e-5, max_iter: 200, max_levels: 5 }
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
}

pub fn solve_static(problem: &StaticProblem) -> Result<Solution, SolveError> {
    let grid = &problem.grid;
    let s = &problem.settings;
    if grid.solid_count() == 0 {
        return Err(SolveError::NoSolidCells);
    }

    // Pick the level count from the smallest axis, then pad dims so every
    // level divides evenly (padding cells are void and cost nothing).
    let min_dim = grid.nx.min(grid.ny).min(grid.nz);
    let mut levels = 1usize;
    while levels < s.max_levels && (min_dim >> levels) >= 2 && (min_dim >> (levels - 1)) >= 4 {
        levels += 1;
    }
    let mult = 1usize << (levels - 1);
    let pad = |n: usize| n.div_ceil(mult) * mult;
    let (nx, ny, nz) = (pad(grid.nx), pad(grid.ny), pad(grid.nz));

    let mut eps = vec![0f32; nx * ny * nz];
    for cz in 0..grid.nz {
        for cy in 0..grid.ny {
            for cx in 0..grid.nx {
                let sc = grid.scale[grid.cell_index(cx, cy, cz)];
                if sc > 0.0 {
                    eps[(cz * ny + cy) * nx + cx] = EMIN_REL + (1.0 - EMIN_REL) * sc;
                }
            }
        }
    }

    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
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

    // Active nodes (touching at least one solid cell).
    let mut active = vec![false; mx * my * mz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                if eps[(cz * ny + cy) * nx + cx] > 0.0 {
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

    // Fixed DOFs from user regions.
    let mut fixed = vec![false; 3 * mx * my * mz];
    let mut fixed_count = 0usize;
    for n in 0..mx * my * mz {
        if !active[n] {
            continue;
        }
        let p = node_pos(n);
        if problem.fixed.iter().any(|r| r.contains(p)) {
            for d in 0..3 {
                fixed[3 * n + d] = true;
            }
            fixed_count += 1;
        }
    }
    if fixed_count == 0 {
        return Err(SolveError::NoFixedNodes);
    }

    // Force vector: each load split equally over its selected nodes.
    let mut b = vec![0f64; 3 * mx * my * mz];
    for (li, (region, force)) in problem.loads.iter().enumerate() {
        let nodes: Vec<usize> = (0..mx * my * mz)
            .filter(|&n| active[n] && region.contains(node_pos(n)))
            .collect();
        if nodes.is_empty() {
            return Err(SolveError::LoadRegionEmpty(li));
        }
        let inv = 1.0 / nodes.len() as f64;
        for n in nodes {
            for d in 0..3 {
                b[3 * n + d] += force[d] * inv;
            }
        }
    }

    let ke64 = ke_hex(s.e0, s.nu, grid.h);
    let finest = Level::new(nx, ny, nz, grid.h, eps, ke64, &fixed);
    // Zero loads on constrained DOFs (fixed nodes swallow applied forces).
    for (i, c) in finest.constrained.iter().enumerate() {
        if *c {
            b[i] = 0.0;
        }
    }

    let mut solver = MgSolver::new(finest, levels);
    let mut u = vec![0f64; 3 * mx * my * mz];
    let stats = solver.solve(&b, &mut u, s.tol, s.max_iter);
    if !stats.converged {
        return Err(SolveError::NotConverged {
            iterations: stats.iterations,
            rel_residual: stats.rel_residual,
        });
    }

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
    })
}
