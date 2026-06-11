//! Variable-density infill optimization (DESIGN.md decision #8):
//! compliance minimization under a mass budget, using the PHYSICAL infill
//! stiffness law E(x) = E0 * x^n — intermediate densities are printable as
//! graded infill, so no artificial penalization is wanted.
//!
//! Material model (decision #7): cells within the wall thickness of the
//! surface are "skin" (solid, perimeters/shells); interior cells carry the
//! design density x in [floor, cap].
//!
//! Update scheme: classic optimality criteria with move limits + a linear
//! density filter (radius ~1.6 cells) against checkerboards and slivers.

use crate::fem::ke_hex;
use crate::mg::{Level, MgSolver};
use crate::solve::{NodeProblem, SolveSettings};
use crate::voxel::VoxelGrid;

const EMIN_REL: f32 = 1e-6;

#[derive(Clone, Copy, Debug)]
pub struct OptimizeParams {
    /// Target total mass as a fraction of the fully solid part (incl. skin).
    pub budget: f64,
    /// Gibson-Ashby law of the infill pattern: E/E0 = coeff * x^exponent.
    pub exponent: f64,
    pub coeff: f64,
    /// Printable density bounds for interior cells.
    pub floor: f64,
    pub cap: f64,
    /// Skin (wall + shell) thickness in mm.
    pub wall_mm: f64,
    pub max_iter: usize,
}

impl Default for OptimizeParams {
    fn default() -> Self {
        Self {
            budget: 0.45,
            exponent: 1.5,
            coeff: 1.0,
            floor: 0.10,
            cap: 0.70,
            wall_mm: 0.9,
            max_iter: 40,
        }
    }
}

pub struct OptimizeProgress {
    pub iteration: usize,
    pub compliance: f64,
    /// Current total mass fraction of solid.
    pub mass_frac: f64,
    pub change: f64,
}

pub struct OptimizeResult {
    /// Physical (filtered) densities per interior design cell.
    pub x: Vec<f64>,
    /// Cell ids (padded grid) of the design cells.
    pub design_cells: Vec<u32>,
    /// Cell ids of skin cells (always solid).
    pub skin_cells: Vec<u32>,
    /// Achieved budget (after feasibility clamping).
    pub effective_budget: f64,
    pub iterations: usize,
    pub compliance: f64,
    /// Last displacement field (padded node grid, f64) — warm start / reuse.
    pub u: Vec<f64>,
}

#[derive(Debug)]
pub enum OptimizeError {
    Solve(crate::solve::SolveError),
    NoInterior,
}

impl std::fmt::Display for OptimizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OptimizeError::Solve(e) => write!(f, "{e}"),
            OptimizeError::NoInterior => write!(
                f,
                "part is thinner than the wall thickness everywhere — nothing to optimize (it prints solid)"
            ),
        }
    }
}

/// Split solid cells into skin (within `wall_mm` of the surface) and interior.
pub fn classify_cells(grid: &VoxelGrid, wall_mm: f64) -> (Vec<u32>, Vec<u32>) {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let layers = (wall_mm / grid.h).round().max(1.0) as usize;
    // depth 0 = surface cell (touches void/outside), then BFS inward.
    let mut depth = vec![u32::MAX; nx * ny * nz];
    let mut queue: std::collections::VecDeque<usize> = Default::default();
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if grid.scale[ci] <= 0.0 {
                    continue;
                }
                let mut surface = cx == 0 || cx + 1 == nx || cy == 0 || cy + 1 == ny || cz == 0 || cz + 1 == nz;
                if !surface {
                    surface = grid.scale[ci - 1] <= 0.0
                        || grid.scale[ci + 1] <= 0.0
                        || grid.scale[ci - nx] <= 0.0
                        || grid.scale[ci + nx] <= 0.0
                        || grid.scale[ci - nx * ny] <= 0.0
                        || grid.scale[ci + nx * ny] <= 0.0;
                }
                if surface {
                    depth[ci] = 0;
                    queue.push_back(ci);
                }
            }
        }
    }
    while let Some(ci) = queue.pop_front() {
        let d = depth[ci];
        if d as usize + 1 >= layers {
            continue;
        }
        let cx = ci % nx;
        let cy = (ci / nx) % ny;
        let cz = ci / (nx * ny);
        let mut push = |c: usize| {
            if grid.scale[c] > 0.0 && depth[c] == u32::MAX {
                depth[c] = d + 1;
                queue.push_back(c);
            }
        };
        if cx > 0 {
            push(ci - 1);
        }
        if cx + 1 < nx {
            push(ci + 1);
        }
        if cy > 0 {
            push(ci - nx);
        }
        if cy + 1 < ny {
            push(ci + nx);
        }
        if cz > 0 {
            push(ci - nx * ny);
        }
        if cz + 1 < nz {
            push(ci + nx * ny);
        }
    }
    let mut skin = Vec::new();
    let mut interior = Vec::new();
    for ci in 0..nx * ny * nz {
        if grid.scale[ci] <= 0.0 {
            continue;
        }
        if depth[ci] != u32::MAX {
            skin.push(ci as u32);
        } else {
            interior.push(ci as u32);
        }
    }
    (skin, interior)
}

/// Linear density filter over interior cells (conic weights, radius in cells).
struct DensityFilter {
    /// For each design cell: (neighbor slot, weight) pairs and the row sum.
    neighbors: Vec<Vec<(u32, f32)>>,
    row_sum: Vec<f32>,
}

impl DensityFilter {
    fn build(grid: &VoxelGrid, design_cells: &[u32], radius_cells: f64) -> Self {
        let (nx, ny) = (grid.nx, grid.ny);
        let mut slot_of: std::collections::HashMap<u32, u32> = Default::default();
        for (i, &c) in design_cells.iter().enumerate() {
            slot_of.insert(c, i as u32);
        }
        let r = radius_cells;
        let ri = r.ceil() as i64;
        let n = design_cells.len();
        let mut neighbors = vec![Vec::new(); n];
        let mut row_sum = vec![0f32; n];
        for (i, &c) in design_cells.iter().enumerate() {
            let c = c as usize;
            let cx = (c % nx) as i64;
            let cy = ((c / nx) % ny) as i64;
            let cz = (c / (nx * ny)) as i64;
            for dz in -ri..=ri {
                for dy in -ri..=ri {
                    for dx in -ri..=ri {
                        let d = ((dx * dx + dy * dy + dz * dz) as f64).sqrt();
                        if d > r {
                            continue;
                        }
                        let (x, y, z) = (cx + dx, cy + dy, cz + dz);
                        if x < 0 || y < 0 || z < 0 {
                            continue;
                        }
                        let (x, y, z) = (x as usize, y as usize, z as usize);
                        if x >= nx || y >= ny || z >= grid.nz {
                            continue;
                        }
                        let nc = ((z * ny + y) * nx + x) as u32;
                        if let Some(&slot) = slot_of.get(&nc) {
                            let w = (1.0 - d / r) as f32;
                            neighbors[i].push((slot, w));
                            row_sum[i] += w;
                        }
                    }
                }
            }
        }
        Self { neighbors, row_sum }
    }

    /// y = W x (row-normalized).
    fn apply(&self, x: &[f64], y: &mut [f64]) {
        for i in 0..x.len() {
            let mut s = 0f64;
            for &(j, w) in &self.neighbors[i] {
                s += w as f64 * x[j as usize];
            }
            y[i] = s / self.row_sum[i] as f64;
        }
    }

    /// y = W^T g (uses symmetry of the unnormalized weights).
    fn apply_t(&self, g: &[f64], y: &mut [f64]) {
        y.iter_mut().for_each(|v| *v = 0.0);
        for i in 0..g.len() {
            let gi = g[i] / self.row_sum[i] as f64;
            for &(j, w) in &self.neighbors[i] {
                y[j as usize] += w as f64 * gi;
            }
        }
    }
}

/// Per-cell strain energy u_e^T KE u_e for the given cells (unit-eps KE).
fn cell_strain_energy(
    level: &Level,
    ke64: &[[f64; 24]; 24],
    u: &[f64],
    cells: &[u32],
    out: &mut [f64],
) {
    let (nx, ny) = (level.nx, level.ny);
    let (mx, my) = (level.mx, level.my);
    for (k, &ci) in cells.iter().enumerate() {
        let ci = ci as usize;
        let cx = ci % nx;
        let cy = (ci / nx) % ny;
        let cz = ci / (nx * ny);
        let mut ul = [0f64; 24];
        for l in 0..8 {
            let [ox, oy, oz] = crate::fem::NODE_OFFSETS[l];
            let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
            ul[3 * l] = u[3 * n];
            ul[3 * l + 1] = u[3 * n + 1];
            ul[3 * l + 2] = u[3 * n + 2];
        }
        let mut se = 0f64;
        for i in 0..24 {
            let mut s = 0f64;
            for j in 0..24 {
                s += ke64[i][j] * ul[j];
            }
            se += ul[i] * s;
        }
        out[k] = se.max(0.0);
    }
}

/// Build the per-cell stiffness factors for a given interior density field.
/// Infill law E/E0 = coeff * x^exponent, capped at solid (1.0).
pub fn build_eps(
    grid: &VoxelGrid,
    skin: &[u32],
    design_cells: &[u32],
    x: &[f64],
    exponent: f64,
    coeff: f64,
) -> Vec<f32> {
    let mut eps = vec![0f32; grid.cell_count()];
    for &c in skin {
        eps[c as usize] = 1.0;
    }
    for (k, &c) in design_cells.iter().enumerate() {
        let rel = (coeff * x[k].powf(exponent)).min(1.0);
        let e = EMIN_REL as f64 + (1.0 - EMIN_REL as f64) * rel;
        eps[c as usize] = e as f32;
    }
    eps
}

/// Assemble the rhs once (forces are density-independent).
pub fn build_rhs(grid: &VoxelGrid, problem: &NodeProblem) -> Vec<f64> {
    let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
    let mut b = vec![0f64; 3 * mx * my * mz];
    for &(n, f) in &problem.forces {
        for d in 0..3 {
            b[3 * n as usize + d] += f[d];
        }
    }
    b
}

fn solve_once(
    grid: &VoxelGrid,
    levels: usize,
    eps: Vec<f32>,
    problem: &NodeProblem,
    settings: &SolveSettings,
    b: &[f64],
    u: &mut [f64],
    tol: f64,
) -> Result<(Level, f64), crate::solve::SolveError> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let ndof = 3 * mx * my * mz;
    let mut fixed = vec![false; ndof];
    for &n in &problem.fixed {
        for d in 0..3 {
            fixed[3 * n as usize + d] = true;
        }
    }
    let ke64 = ke_hex(settings.e0, settings.nu, grid.h);
    let finest = Level::new(nx, ny, nz, grid.h, eps, ke64, &fixed, problem.springs.clone());
    let mut bb = b.to_vec();
    for (i, c) in finest.constrained.iter().enumerate() {
        if *c {
            bb[i] = 0.0;
        }
    }
    // The level is consumed by the solver; rebuild a cheap copy for energy
    // evaluation by keeping KE outside. We return the finest via a fresh build
    // — instead, solve and recompute compliance from b·u (cheaper, exact).
    let mut solver = MgSolver::new(finest, levels);
    let stats = solver.solve_warm(&bb, u, tol, 600);
    if !stats.converged {
        return Err(crate::solve::SolveError::NotConverged {
            iterations: stats.iterations,
            rel_residual: stats.rel_residual,
        });
    }
    let mut compliance = 0f64;
    for i in 0..ndof {
        compliance += bb[i] * u[i];
    }
    let level = solver.levels.swap_remove(0);
    Ok((level, compliance))
}

/// Run the optimization. `progress` is called once per iteration.
pub fn optimize(
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    params: &OptimizeParams,
    mut progress: impl FnMut(&OptimizeProgress, &[f64], &[u32]),
) -> Result<OptimizeResult, OptimizeError> {
    let (skin, design_cells) = classify_cells(grid, params.wall_mm);
    if design_cells.is_empty() {
        return Err(OptimizeError::NoInterior);
    }
    let n_solid = (skin.len() + design_cells.len()) as f64;
    let n_int = design_cells.len() as f64;

    // Feasible interior mean density for the requested budget.
    let raw = (params.budget * n_solid - skin.len() as f64) / n_int;
    let target_mean = raw.clamp(params.floor, params.cap);
    let effective_budget = (skin.len() as f64 + target_mean * n_int) / n_solid;

    let filter = DensityFilter::build(grid, &design_cells, 1.6);
    let ke64 = ke_hex(settings.e0, settings.nu, grid.h);
    let b = build_rhs(grid, problem);

    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let ndof = 3 * (nx + 1) * (ny + 1) * (nz + 1);
    let mut u = vec![0f64; ndof];
    let mut x = vec![target_mean; design_cells.len()];
    let mut x_phys = vec![0f64; design_cells.len()];
    let mut se = vec![0f64; design_cells.len()];
    let mut sens_phys = vec![0f64; design_cells.len()];
    let mut sens = vec![0f64; design_cells.len()];
    let mut compliance = f64::INFINITY;
    let mut iterations = 0;

    // Build the hierarchy ONCE; iterations only swap stiffness values in.
    let mut fixed = vec![false; ndof];
    for &n in &problem.fixed {
        for d in 0..3 {
            fixed[3 * n as usize + d] = true;
        }
    }
    let eps0 = build_eps(grid, &skin, &design_cells, &x, params.exponent, params.coeff);
    let finest = Level::new(nx, ny, nz, grid.h, eps0, ke64, &fixed, problem.springs.clone());
    let mut bb = b.clone();
    for (i, c) in finest.constrained.iter().enumerate() {
        if *c {
            bb[i] = 0.0;
        }
    }
    let mut solver = MgSolver::new(finest, levels);

    for it in 0..params.max_iter {
        iterations = it + 1;
        filter.apply(&x, &mut x_phys);
        for v in x_phys.iter_mut() {
            *v = v.clamp(params.floor, params.cap);
        }
        let eps = build_eps(grid, &skin, &design_cells, &x_phys, params.exponent, params.coeff);
        solver.update_eps(eps);
        // Inexact inner solves are standard in topology optimization: the
        // design update tolerates sensitivity noise (filter + move limits),
        // so cap the MGCG work per iteration instead of converging tightly.
        // Only the final verification solve needs full accuracy.
        let _ = solver.solve_warm(&bb, &mut u, 5e-4, 15);
        compliance = 0.0;
        for i in 0..ndof {
            compliance += bb[i] * u[i];
        }

        cell_strain_energy(&solver.levels[0], &ke64, &u, &design_cells, &mut se);
        // dC/dx_phys = -(1-emin) * c * n * x^(n-1) * se
        for k in 0..design_cells.len() {
            sens_phys[k] = -(1.0 - EMIN_REL as f64)
                * params.coeff
                * params.exponent
                * x_phys[k].powf(params.exponent - 1.0)
                * se[k];
        }
        filter.apply_t(&sens_phys, &mut sens);

        // OC update with bisection on the volume multiplier.
        let move_limit = 0.15;
        let (mut lo, mut hi) = (1e-12f64, 1e12f64);
        let mut x_new = vec![0f64; x.len()];
        for _ in 0..60 {
            let lambda = (lo * hi).sqrt();
            let mut mean_phys = 0f64;
            for k in 0..x.len() {
                let be = (-sens[k] / lambda).max(0.0);
                let xn = (x[k] * be.sqrt())
                    .clamp(x[k] - move_limit, x[k] + move_limit)
                    .clamp(params.floor, params.cap);
                x_new[k] = xn;
            }
            // Volume measured on filtered densities for consistency.
            filter.apply(&x_new, &mut x_phys);
            for v in x_phys.iter_mut() {
                *v = v.clamp(params.floor, params.cap);
            }
            for k in 0..x.len() {
                mean_phys += x_phys[k];
            }
            mean_phys /= n_int;
            if mean_phys > target_mean {
                lo = lambda;
            } else {
                hi = lambda;
            }
            if (hi / lo) < 1.0001 {
                break;
            }
        }
        let mut change = 0f64;
        for k in 0..x.len() {
            change = change.max((x_new[k] - x[k]).abs());
        }
        x.copy_from_slice(&x_new);

        let mass_frac = (skin.len() as f64
            + x_phys.iter().sum::<f64>())
            / n_solid;
        progress(
            &OptimizeProgress { iteration: it + 1, compliance, mass_frac, change },
            &x_phys,
            &design_cells,
        );
        if change < 0.02 && it >= 8 {
            break;
        }
    }

    // Final physical field.
    filter.apply(&x, &mut x_phys);
    for v in x_phys.iter_mut() {
        *v = v.clamp(params.floor, params.cap);
    }

    Ok(OptimizeResult {
        x: x_phys,
        design_cells,
        skin_cells: skin,
        effective_budget,
        iterations,
        compliance,
        u,
    })
}

/// Compliance of a given interior density field (one tight solve).
/// Returns (compliance, max nodal displacement, u).
pub fn evaluate(
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    skin: &[u32],
    design_cells: &[u32],
    x: &[f64],
    exponent: f64,
    coeff: f64,
    warm: Option<&[f64]>,
) -> Result<(f64, f64, Vec<f64>), crate::solve::SolveError> {
    let eps = build_eps(grid, skin, design_cells, x, exponent, coeff);
    let b = build_rhs(grid, problem);
    let ndof = b.len();
    let mut u = vec![0f64; ndof];
    if let Some(w) = warm {
        u.copy_from_slice(w);
    }
    let (_, compliance) =
        solve_once(grid, levels, eps, problem, settings, &b, &mut u, settings.tol)?;
    let mut max2 = 0f64;
    for n in 0..ndof / 3 {
        let m = u[3 * n] * u[3 * n] + u[3 * n + 1] * u[3 * n + 1] + u[3 * n + 2] * u[3 * n + 2];
        max2 = max2.max(m);
    }
    Ok((compliance, max2.sqrt(), u))
}
