// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

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
use crate::mg::Level;
use crate::solve::{solve_cached, NodeProblem, SolverCache, SolveSettings};
use crate::voxel::VoxelGrid;

const EMIN_REL: f32 = 1e-6;

#[derive(Clone, Copy, Debug)]
pub struct OptimizeParams {
    /// Target mean INFILL density of the interior (design) cells — the number
    /// a user compares to a slicer's uniform infill percentage. The solid
    /// skin is always 100% and is NOT part of this budget.
    pub budget: f64,
    /// Gibson-Ashby law of the infill pattern: E/E0 = coeff * x^exponent.
    pub exponent: f64,
    pub coeff: f64,
    /// Printable density bounds for interior cells.
    pub floor: f64,
    pub cap: f64,
    /// Skin (wall + shell) thickness in mm.
    pub wall_mm: f64,
    /// Composite skin: surface cells the wall band only partially covers
    /// stay design cells with a blended stiffness, instead of rounding the
    /// band to whole cell layers (legacy).
    pub composite_skin: bool,
    pub max_iter: usize,
}

impl Default for OptimizeParams {
    fn default() -> Self {
        Self {
            budget: 0.25,
            exponent: 1.5,
            coeff: 1.0,
            floor: 0.10,
            cap: 0.70,
            wall_mm: 0.9,
            composite_skin: false,
            max_iter: 40,
        }
    }
}

pub struct OptimizeProgress {
    pub iteration: usize,
    pub compliance: f64,
    /// Current total mass fraction of solid (skin + interior).
    pub mass_frac: f64,
    /// Current mean infill density over the interior cells.
    pub mean_infill: f64,
    /// Max per-cell density change of this design update.
    pub change: f64,
    /// Mean per-cell density change (the convergence signal).
    pub mean_change: f64,
    /// MGCG iterations spent on this outer iteration's solve.
    pub inner_iters: usize,
    /// Relative residual that inner solve reached.
    pub inner_residual: f64,
}

pub struct OptimizeResult {
    /// Physical (filtered) densities per interior design cell.
    pub x: Vec<f64>,
    /// Cell ids (padded grid) of the design cells.
    pub design_cells: Vec<u32>,
    /// Cell ids of skin cells (always solid).
    pub skin_cells: Vec<u32>,
    /// Per-design-cell wall-band fraction (composite skin; zeros when off).
    pub skin_frac: Vec<f32>,
    /// Target mean infill actually used (budget clamped to [floor, cap]).
    pub effective_budget: f64,
    pub iterations: usize,
    /// True if the design-change criterion fired before the iteration cap.
    pub converged: bool,
    pub compliance: f64,
    /// Last displacement field (padded node grid, f64) — warm start / reuse.
    pub u: Vec<f64>,
    /// Per-design-cell strain energy (unit relative stiffness) of the final
    /// iterate — the compliance sensitivity used for level placement and
    /// mass-constrained bin assignment.
    pub se: Vec<f64>,
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

/// Result of `classify_cells`: which solid cells are fully skin, which carry
/// a design density, and how much of each design cell the wall band covers.
pub struct SkinSplit {
    /// Cells fully inside the wall band (always solid).
    pub skin: Vec<u32>,
    /// Cells carrying a design/infill density — includes COMPOSITE cells the
    /// wall band partially covers.
    pub design: Vec<u32>,
    /// Per-design-cell fraction of the cell inside the wall band, in [0, 1).
    /// All zeros in legacy (non-composite) mode.
    pub skin_frac: Vec<f32>,
}

/// Split solid cells into skin (within `wall_mm` of the surface) and design
/// cells. With `composite` on, the wall band is measured in FRACTIONAL cell
/// layers: a cell the band only partially covers (wall thinner than the cell,
/// or a non-integer wall/h) stays a design cell but records the covered
/// fraction, and its stiffness/mass are later blended (Voigt) between solid
/// and infill — so the skin no longer needs h <= wall to be representable.
/// Surface cells exposed on several sides count the overlapping slabs
/// (a convex corner at wall/h = 0.5 is 7/8 skin, not 1/2). With `composite`
/// off, the band is the legacy round(wall/h), minimum one full layer.
pub fn classify_cells(grid: &VoxelGrid, wall_mm: f64, composite: bool) -> SkinSplit {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    // Wall thickness in cell layers; the legacy model rounds to >= 1.
    let t = if composite {
        wall_mm / grid.h
    } else {
        (wall_mm / grid.h).round().max(1.0)
    };
    // depth 0 = surface cell (touches void/outside), then BFS inward while
    // the band still reaches the next layer. Void faces of surface cells are
    // counted per axis for the overlapping-slab fraction.
    let mut depth = vec![u32::MAX; nx * ny * nz];
    let mut void_faces = vec![[0u8; 3]; 0];
    let mut surface_slot = vec![u32::MAX; 0];
    if composite {
        surface_slot = vec![u32::MAX; nx * ny * nz];
    }
    let mut queue: std::collections::VecDeque<usize> = Default::default();
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if grid.scale[ci] <= 0.0 {
                    continue;
                }
                let void_at = |dx: i64, dy: i64, dz: i64| -> bool {
                    let (x, y, z) = (cx as i64 + dx, cy as i64 + dy, cz as i64 + dz);
                    if x < 0 || y < 0 || z < 0 || x >= nx as i64 || y >= ny as i64 || z >= nz as i64
                    {
                        return true; // outside the grid counts as void
                    }
                    grid.scale[((z as usize) * ny + y as usize) * nx + x as usize] <= 0.0
                };
                let faces = [
                    void_at(-1, 0, 0) as u8 + void_at(1, 0, 0) as u8,
                    void_at(0, -1, 0) as u8 + void_at(0, 1, 0) as u8,
                    void_at(0, 0, -1) as u8 + void_at(0, 0, 1) as u8,
                ];
                if faces[0] + faces[1] + faces[2] > 0 {
                    depth[ci] = 0;
                    queue.push_back(ci);
                    if composite {
                        surface_slot[ci] = void_faces.len() as u32;
                        void_faces.push(faces);
                    }
                }
            }
        }
    }
    while let Some(ci) = queue.pop_front() {
        let d = depth[ci];
        // The band reaches layer d+1 only while d+1 < t.
        if (d as f64 + 1.0) >= t - 1e-9 {
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
    let mut design = Vec::new();
    let mut skin_frac = Vec::new();
    for ci in 0..nx * ny * nz {
        if grid.scale[ci] <= 0.0 {
            continue;
        }
        let f = match depth[ci] {
            u32::MAX => 0.0,
            0 if composite => {
                // Union of wall slabs from each exposed face: the uncovered
                // core is the product of the per-axis remainders.
                let faces = void_faces[surface_slot[ci] as usize];
                let mut core = 1.0f64;
                for a in 0..3 {
                    core *= (1.0 - faces[a] as f64 * t).max(0.0);
                }
                1.0 - core
            }
            d => (t - d as f64).clamp(0.0, 1.0),
        };
        if f >= 1.0 - 1e-6 {
            skin.push(ci as u32);
        } else {
            design.push(ci as u32);
            skin_frac.push(if f <= 1e-6 { 0.0 } else { f as f32 });
        }
    }
    SkinSplit { skin, design, skin_frac }
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
/// Infill law E/E0 = coeff * x^exponent, capped at solid (1.0). A design
/// cell partially covered by the wall band (`skin_frac` > 0, composite skin)
/// gets the volume-fraction blend of solid and infill — the same
/// homogenization step as the infill law itself, applied at the surface.
pub fn build_eps(
    grid: &VoxelGrid,
    skin: &[u32],
    design_cells: &[u32],
    skin_frac: &[f32],
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
        let e_infill = EMIN_REL as f64 + (1.0 - EMIN_REL as f64) * rel;
        let f = skin_frac[k] as f64;
        eps[c as usize] = (f + (1.0 - f) * e_infill) as f32;
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

/// Run the optimization. `progress` is called once per iteration.
/// `x0`/`u0` warm-start the design and displacement fields — the
/// stiffness-match outer loop re-runs at slightly different budgets, where
/// a warm pass converges in a fraction of the iterations (the OC volume
/// bisection shifts the mean to the new budget in the first update).
pub fn optimize(
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    params: &OptimizeParams,
    x0: Option<&[f64]>,
    u0: Option<&[f64]>,
    progress: impl FnMut(&OptimizeProgress, &[f64], &[u32]),
) -> Result<OptimizeResult, OptimizeError> {
    optimize_cached(&mut None, grid, levels, problem, settings, params, x0, u0, progress)
}

/// `optimize` reusing (and leaving behind) the hierarchy + displacement in
/// `slot` — the verification/baseline solves right after, and the warm
/// re-passes of the stiffness-match loop, then skip the full solver rebuild.
#[allow(clippy::too_many_arguments)]
pub fn optimize_cached(
    slot: &mut Option<SolverCache>,
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    params: &OptimizeParams,
    x0: Option<&[f64]>,
    u0: Option<&[f64]>,
    mut progress: impl FnMut(&OptimizeProgress, &[f64], &[u32]),
) -> Result<OptimizeResult, OptimizeError> {
    let SkinSplit { skin, design: design_cells, skin_frac } =
        classify_cells(grid, params.wall_mm, params.composite_skin);
    if design_cells.is_empty() {
        return Err(OptimizeError::NoInterior);
    }
    let n_solid = (skin.len() + design_cells.len()) as f64;
    // Infill volume share per design cell — a composite cell is only
    // (1 - skin_frac) infill; all 1 with composite skin off. Means and the
    // mass budget weight by it so "mean infill" keeps meaning what a slicer
    // percentage means.
    let w: Vec<f64> = skin_frac.iter().map(|&f| 1.0 - f as f64).collect();
    let w_sum: f64 = w.iter().sum();
    let sum_f: f64 = skin_frac.iter().map(|&f| f as f64).sum();

    // The budget IS the target interior mean (infill %), so the result is
    // directly comparable to a uniform print at the same slicer percentage.
    let target_mean = params.budget.clamp(params.floor, params.cap);
    let effective_budget = target_mean;

    let filter = DensityFilter::build(grid, &design_cells, 1.6);
    let ke64 = ke_hex(settings.e0, settings.nu, grid.h);
    let b = build_rhs(grid, problem);

    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let ndof = 3 * (nx + 1) * (ny + 1) * (nz + 1);
    let mut u = vec![0f64; ndof];
    if let Some(w) = u0 {
        if w.len() == ndof {
            u.copy_from_slice(w);
        }
    }
    let mut x = vec![target_mean; design_cells.len()];
    if let Some(w) = x0 {
        if w.len() == x.len() {
            x.copy_from_slice(w);
            for v in x.iter_mut() {
                *v = v.clamp(params.floor, params.cap);
            }
        }
    }
    let mut x_phys = vec![0f64; design_cells.len()];
    let mut se = vec![0f64; design_cells.len()];
    let mut sens_phys = vec![0f64; design_cells.len()];
    let mut sens = vec![0f64; design_cells.len()];
    let mut compliance = f64::INFINITY;
    let mut iterations = 0;
    let mut converged = false;
    let mut small_streak = 0usize;
    let mut last_mean_change = f64::INFINITY;

    // Build the hierarchy ONCE (or reuse a cached one — same grid/BC/void
    // pattern); iterations only swap stiffness values in.
    let eps0 = build_eps(grid, &skin, &design_cells, &skin_frac, &x, params.exponent, params.coeff);
    let cache = SolverCache::prepare(slot, grid, levels, problem, settings, eps0);
    let mut bb = b.clone();
    for (i, c) in cache.solver.levels[0].constrained.iter().enumerate() {
        if *c {
            bb[i] = 0.0;
        }
    }

    for it in 0..params.max_iter {
        iterations = it + 1;
        filter.apply(&x, &mut x_phys);
        for v in x_phys.iter_mut() {
            *v = v.clamp(params.floor, params.cap);
        }
        let eps =
            build_eps(grid, &skin, &design_cells, &skin_frac, &x_phys, params.exponent, params.coeff);
        cache.solver.update_eps(eps);
        // Inexact inner solves are standard in topology optimization: while
        // the layout is forming, sensitivity noise is tolerated (filter +
        // move limits), so cap the MGCG work. Once the design slows down,
        // spend real solve effort — otherwise u (and the sensitivities)
        // keep creeping toward the true solution for tens of iterations and
        // the design never becomes stationary.
        let (tol_i, cap_i) = if last_mean_change < 0.012 { (2e-4, 60) } else { (5e-4, 15) };
        let inner = cache.solver.solve_warm(&bb, &mut u, tol_i, cap_i);
        compliance = 0.0;
        for i in 0..ndof {
            compliance += bb[i] * u[i];
        }

        cell_strain_energy(&cache.solver.levels[0], &ke64, &u, &design_cells, &mut se);
        // Composite cells: only the infill share of the cell responds to x —
        // scaling the energy by it makes se the honest dC/dx weight (also
        // for the bin placement that reuses the stored se).
        for k in 0..design_cells.len() {
            se[k] *= w[k];
        }
        // dC/dx_phys = -(1-emin) * (1-f) * c * n * x^(n-1) * se
        for k in 0..design_cells.len() {
            sens_phys[k] = -(1.0 - EMIN_REL as f64)
                * params.coeff
                * params.exponent
                * x_phys[k].powf(params.exponent - 1.0)
                * se[k];
        }
        filter.apply_t(&sens_phys, &mut sens);

        // OC update with bisection on the volume multiplier.
        // Move-limit continuation: full steps while the layout forms, then
        // geometric decay. This kills the OC 2-cycle (cells ping-ponging at
        // +/-move forever) so the design actually becomes stationary instead
        // of dithering until the iteration cap.
        let move_limit =
            if it < 10 { 0.15 } else { (0.15 * 0.92f64.powi(it as i32 - 9)).max(0.05) };
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
                mean_phys += w[k] * x_phys[k];
            }
            mean_phys /= w_sum.max(1e-12);
            if mean_phys > target_mean {
                lo = lambda;
            } else {
                hi = lambda;
            }
            if (hi / lo) < 1.0001 {
                break;
            }
        }
        // Oscillation damping: OC settles into a global 2-cycle once the
        // layout has formed (the field flips between two states forever).
        // Averaging consecutive designs cancels the cycle geometrically
        // while passing real drift through; without it the convergence
        // criterion never fires.
        if it >= 12 {
            for k in 0..x.len() {
                x_new[k] = 0.5 * (x_new[k] + x[k]);
            }
        }
        let mut change = 0f64;
        let mut mean_change = 0f64;
        for k in 0..x.len() {
            let d = (x_new[k] - x[k]).abs();
            change = change.max(d);
            mean_change += d;
        }
        mean_change /= x.len().max(1) as f64;
        last_mean_change = mean_change;
        x.copy_from_slice(&x_new);

        let sum_wx = w.iter().zip(&x_phys).map(|(&wk, &xk)| wk * xk).sum::<f64>();
        let mass_frac = (skin.len() as f64 + sum_f + sum_wx) / n_solid;
        progress(
            &OptimizeProgress {
                iteration: it + 1,
                compliance,
                mass_frac,
                mean_infill: sum_wx / w_sum.max(1e-12),
                change,
                mean_change,
                inner_iters: inner.iterations,
                inner_residual: inner.rel_residual,
            },
            &x_phys,
            &design_cells,
        );
        // Converged when the design is stationary in the mean on two
        // consecutive iterations. Max-change is not usable (single boundary
        // cells oscillate), and the compliance estimate from inexact
        // warm-started solves creeps for many iterations, so the field
        // itself is the only honest signal. The iteration cap is a safety net.
        small_streak = if mean_change < 0.005 { small_streak + 1 } else { 0 };
        if small_streak >= 2 && it >= 6 {
            converged = true;
            break;
        }
    }

    // Final physical field.
    filter.apply(&x, &mut x_phys);
    for v in x_phys.iter_mut() {
        *v = v.clamp(params.floor, params.cap);
    }
    // Leave the final displacement in the cache: the verification solves
    // that follow warm-start from it.
    cache.last_u.copy_from_slice(&u);

    Ok(OptimizeResult {
        x: x_phys,
        design_cells,
        skin_cells: skin,
        skin_frac,
        effective_budget,
        iterations,
        converged,
        compliance,
        u,
        se,
    })
}

/// Compliance of a given interior density field (one tight solve).
/// Returns (compliance, max nodal displacement, u).
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    skin: &[u32],
    design_cells: &[u32],
    skin_frac: &[f32],
    x: &[f64],
    exponent: f64,
    coeff: f64,
    warm: Option<&[f64]>,
) -> Result<(f64, f64, Vec<f64>), crate::solve::SolveError> {
    let mut slot = None;
    if let Some(w) = warm {
        // Seed the warm start through a prepared cache.
        let eps = build_eps(grid, skin, design_cells, skin_frac, x, exponent, coeff);
        let cache = SolverCache::prepare(&mut slot, grid, levels, problem, settings, eps);
        if cache.last_u.len() == w.len() {
            cache.last_u.copy_from_slice(w);
        }
    }
    evaluate_cached(
        &mut slot, grid, levels, problem, settings, skin, design_cells, skin_frac, x, exponent,
        coeff,
    )
}

/// `evaluate` reusing the hierarchy + warm start in `slot`.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_cached(
    slot: &mut Option<SolverCache>,
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    skin: &[u32],
    design_cells: &[u32],
    skin_frac: &[f32],
    x: &[f64],
    exponent: f64,
    coeff: f64,
) -> Result<(f64, f64, Vec<f64>), crate::solve::SolveError> {
    let eps = build_eps(grid, skin, design_cells, skin_frac, x, exponent, coeff);
    // Hitting the cap is acceptable here: the verification/baseline solves
    // only feed the comparison card, and the warm-started iterate at the cap
    // is accurate to ~1e-4 — aborting a finished optimization over the last
    // decimals would be far worse UX.
    let r = solve_cached(slot, grid, levels, problem, settings, eps, settings.tol, 600)?;
    let mut max2 = 0f64;
    for n in 0..r.u.len() / 3 {
        let m = r.u[3 * n] * r.u[3 * n]
            + r.u[3 * n + 1] * r.u[3 * n + 1]
            + r.u[3 * n + 2] * r.u[3 * n + 2];
        max2 = max2.max(m);
    }
    Ok((r.compliance, max2.sqrt(), r.u))
}

/// Solve with an explicit per-cell stiffness field and return a full
/// `Solution` (iteration count, residual trace) plus the compliance b·u.
/// This is the "as printed" verify solve: build eps from skin at 100% and a
/// uniform interior ratio via `build_eps`, then call this.
pub fn solve_with_eps(
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    eps: Vec<f32>,
) -> Result<(crate::solve::Solution, f64), crate::solve::SolveError> {
    solve_with_eps_cached(&mut None, grid, levels, problem, settings, eps)
}

/// `solve_with_eps` reusing the hierarchy + warm start in `slot`.
pub fn solve_with_eps_cached(
    slot: &mut Option<SolverCache>,
    grid: &VoxelGrid,
    levels: usize,
    problem: &NodeProblem,
    settings: &SolveSettings,
    eps: Vec<f32>,
) -> Result<(crate::solve::Solution, f64), crate::solve::SolveError> {
    let r = solve_cached(slot, grid, levels, problem, settings, eps, settings.tol, settings.max_iter)?;
    let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
    Ok((
        crate::solve::Solution {
            u: r.u.iter().map(|&v| v as f32).collect(),
            mx,
            my,
            mz,
            h: grid.h,
            origin: grid.origin,
            active: crate::solve::active_nodes(grid),
            iterations: r.stats.iterations,
            rel_residual: r.stats.rel_residual,
            converged: r.stats.converged,
            residuals: r.residuals,
        },
        r.compliance,
    ))
}
