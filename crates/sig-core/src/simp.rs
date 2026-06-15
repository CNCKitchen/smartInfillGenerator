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
use crate::selfsupport::SelfSupportFilter;
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
    /// Lateral wall (perimeters × line width) thickness in mm.
    pub wall_mm: f64,
    /// Top/bottom shell thickness in mm (layers × layer height); 0 = none.
    pub top_mm: f64,
    pub bottom_mm: f64,
    /// Composite skin: surface cells the wall band only partially covers
    /// stay design cells with a blended stiffness, instead of rounding the
    /// band to whole cell layers (legacy).
    pub composite_skin: bool,
    /// Planar symmetry constraint: [nx, ny, nz, c] of the plane n·p = c
    /// (world mm, n need not be unit). Mirror-paired design cells are
    /// averaged each iteration (field and sensitivities), so the optimized
    /// density comes out symmetric about the plane.
    pub symmetry: Option<[f64; 4]>,
    /// Minimum member size in mm (printability length scale). Drives the
    /// density-filter radius (`r = min_member/2`, clamped to a cell range);
    /// members narrower than this blur below the bin threshold and drop out.
    /// 0 = off ⇒ only the numerical anti-checkerboard floor applies. Advisory,
    /// not a hard guarantee (see `filter_radius_cells`).
    pub min_member_mm: f64,
    /// SOLID topology mode (DESIGN.md #15): material-removal optimization, not
    /// infill. The skin band is bypassed (`build_solid_split`): the auto-frozen
    /// load/support cells are the only always-solid cells, every other solid
    /// cell is a design cell, and the lower bound is ersatz void. The caller
    /// sets the penalized optimizer law (p=3) and linear eval law.
    pub solid_mode: bool,
    /// Self-supporting (AM) filter: constrain the printed field to overhang at
    /// most `overhang_deg` from the build plate (global +Z). Off = no
    /// constraint. See `crate::selfsupport`.
    pub self_support: bool,
    /// Overhang angle from horizontal for the self-supporting filter (degrees).
    pub overhang_deg: f64,
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
            top_mm: 1.0, // 5 layers x 0.2 mm
            bottom_mm: 1.0,
            composite_skin: false,
            symmetry: None,
            min_member_mm: 0.0,
            solid_mode: false,
            self_support: false,
            overhang_deg: 45.0,
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
    /// The embedder requested a stop (see `crate::cancel`).
    Cancelled,
}

impl std::fmt::Display for OptimizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OptimizeError::Solve(e) => write!(f, "{e}"),
            OptimizeError::NoInterior => write!(
                f,
                "part is thinner than the wall thickness everywhere — nothing to optimize (it prints solid)"
            ),
            OptimizeError::Cancelled => write!(f, "cancelled"),
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

/// Split solid cells into skin and design cells, with per-design-cell skin
/// fractions, modeling the printed shell structure DIRECTIONALLY the way a
/// slicer builds it:
/// - WALLS (perimeters × line width = `wall_mm`): a band measured IN-PLANE
///   from each layer's outline — per-slice 2D BFS. Vertical and sloped
///   surfaces get walls; the band does not leak through top/bottom faces.
/// - TOP/BOTTOM SHELLS (`top_mm`/`bottom_mm` = layers × layer height): a
///   band measured VERTICALLY from up-/down-facing surfaces — per-column
///   contiguous solid runs, so an internal cavity gets shells above and
///   below it like sliced top/bottom layers. 0 = no shells (open-top
///   showpieces print sparse right to the surface).
///
/// With `composite` on, both bands are FRACTIONAL cell layers (partially
/// covered cells blend solid and infill); off rounds to whole layers (walls
/// minimum 1, shells may round to 0). Overlapping slabs combine exactly:
/// opposite sides along one direction add (clamped at full), orthogonal
/// bands overlap independently — f = 1 − (1 − f_wall)(1 − f_shell).
pub fn classify_cells(
    grid: &VoxelGrid,
    wall_mm: f64,
    top_mm: f64,
    bottom_mm: f64,
    composite: bool,
) -> SkinSplit {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let h = grid.h;
    let t_wall = if composite { wall_mm / h } else { (wall_mm / h).round().max(1.0) };
    let t_top = if composite { (top_mm / h).max(0.0) } else { (top_mm / h).round().max(0.0) };
    let t_bot =
        if composite { (bottom_mm / h).max(0.0) } else { (bottom_mm / h).round().max(0.0) };

    // ---- lateral wall band: 2D BFS per z-slice ----
    let mut f_lat = vec![0f32; nx * ny * nz];
    let mut depth = vec![u32::MAX; nx * ny];
    let mut faces = vec![[0u8; 2]; nx * ny];
    let mut queue: std::collections::VecDeque<usize> = Default::default();
    for cz in 0..nz {
        let base = cz * ny * nx;
        depth.iter_mut().for_each(|v| *v = u32::MAX);
        for cy in 0..ny {
            for cx in 0..nx {
                let si = cy * nx + cx;
                if grid.scale[base + si] <= 0.0 {
                    continue;
                }
                let void_at = |dx: i64, dy: i64| -> bool {
                    let (x, y) = (cx as i64 + dx, cy as i64 + dy);
                    if x < 0 || y < 0 || x >= nx as i64 || y >= ny as i64 {
                        return true; // outside the grid counts as void
                    }
                    grid.scale[base + (y as usize) * nx + x as usize] <= 0.0
                };
                let k = [
                    void_at(-1, 0) as u8 + void_at(1, 0) as u8,
                    void_at(0, -1) as u8 + void_at(0, 1) as u8,
                ];
                if k[0] + k[1] > 0 {
                    depth[si] = 0;
                    faces[si] = k;
                    queue.push_back(si);
                }
            }
        }
        while let Some(si) = queue.pop_front() {
            let d = depth[si];
            if (d as f64 + 1.0) >= t_wall - 1e-9 {
                continue;
            }
            let cx = si % nx;
            let cy = si / nx;
            let mut push = |s: usize| {
                if grid.scale[base + s] > 0.0 && depth[s] == u32::MAX {
                    depth[s] = d + 1;
                    queue.push_back(s);
                }
            };
            if cx > 0 {
                push(si - 1);
            }
            if cx + 1 < nx {
                push(si + 1);
            }
            if cy > 0 {
                push(si - nx);
            }
            if cy + 1 < ny {
                push(si + nx);
            }
        }
        for si in 0..nx * ny {
            if grid.scale[base + si] <= 0.0 {
                continue;
            }
            let f = match depth[si] {
                u32::MAX => 0.0,
                0 if composite => {
                    // Union of wall slabs from each exposed in-plane face:
                    // the uncovered core is the product of the per-axis
                    // remainders (opposite faces add via the 1 − k·t term).
                    let k = faces[si];
                    1.0 - (1.0 - k[0] as f64 * t_wall).max(0.0)
                        * (1.0 - k[1] as f64 * t_wall).max(0.0)
                }
                d => (t_wall - d as f64).clamp(0.0, 1.0),
            };
            f_lat[base + si] = f as f32;
        }
    }

    // ---- vertical shell band: contiguous solid runs per column ----
    let mut f_vert = vec![0f32; nx * ny * nz];
    if t_top > 1e-9 || t_bot > 1e-9 {
        for cy in 0..ny {
            for cx in 0..nx {
                let at = |z: usize| (z * ny + cy) * nx + cx;
                let mut z = 0usize;
                while z < nz {
                    if grid.scale[at(z)] <= 0.0 {
                        z += 1;
                        continue;
                    }
                    let z0 = z;
                    while z < nz && grid.scale[at(z)] > 0.0 {
                        z += 1;
                    }
                    let z1 = z - 1;
                    for zz in z0..=z1 {
                        let dt = (z1 - zz) as f64; // cells below the run's top
                        let db = (zz - z0) as f64;
                        let f = (t_top - dt).clamp(0.0, 1.0) + (t_bot - db).clamp(0.0, 1.0);
                        f_vert[at(zz)] = f.min(1.0) as f32;
                    }
                }
            }
        }
    }

    // ---- combine (orthogonal slabs overlap independently) + split ----
    let mut skin = Vec::new();
    let mut design = Vec::new();
    let mut skin_frac = Vec::new();
    for ci in 0..nx * ny * nz {
        if grid.scale[ci] <= 0.0 {
            continue;
        }
        let f = 1.0 - (1.0 - f_lat[ci] as f64) * (1.0 - f_vert[ci] as f64);
        if f >= 1.0 - 1e-6 {
            skin.push(ci as u32);
        } else {
            design.push(ci as u32);
            skin_frac.push(if f <= 1e-6 { 0.0 } else { f as f32 });
        }
    }
    SkinSplit { skin, design, skin_frac }
}

/// SOLID topology mode (DESIGN.md #15): no skin band. The always-solid cells
/// are exactly the auto-frozen load/support cells (`frozen`); every other solid
/// cell is a free design cell with zero skin fraction. This reuses the skin
/// path (`skin` cells stay at eps = 1, excluded from the design vector) to pin
/// the material under loads/supports so it is never optimized away.
pub fn build_solid_split(grid: &VoxelGrid, frozen: &[u32]) -> SkinSplit {
    let mut is_frozen = vec![false; grid.cell_count()];
    for &c in frozen {
        if (c as usize) < is_frozen.len() {
            is_frozen[c as usize] = true;
        }
    }
    let mut skin = Vec::new();
    let mut design = Vec::new();
    let mut skin_frac = Vec::new();
    for ci in 0..grid.cell_count() {
        if grid.scale[ci] <= 0.0 {
            continue;
        }
        if is_frozen[ci] {
            skin.push(ci as u32);
        } else {
            design.push(ci as u32);
            skin_frac.push(0.0);
        }
    }
    SkinSplit { skin, design, skin_frac }
}

/// Cells to freeze solid in topology mode: every solid cell incident to a
/// loaded or constrained node (the assembled problem's fixed ∪ spring ∪ force
/// nodes). Keeps material under loads and supports from being deleted — the
/// "keep regions" of a commercial topology optimizer, derived automatically.
pub fn frozen_cells_from_problem(grid: &VoxelGrid, problem: &NodeProblem) -> Vec<u32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let mut hit = vec![false; grid.cell_count()];
    let mut mark = |n: u32| {
        let n = n as usize;
        let (i, j, k) = (n % mx, (n / mx) % my, n / (mx * my));
        // The (up to 8) cells sharing node (i,j,k): cx ∈ {i-1,i}, etc.
        for a in 0..2 {
            for b in 0..2 {
                for c in 0..2 {
                    let cx = i as i64 - 1 + a;
                    let cy = j as i64 - 1 + b;
                    let cz = k as i64 - 1 + c;
                    if cx < 0 || cy < 0 || cz < 0 {
                        continue;
                    }
                    let (cx, cy, cz) = (cx as usize, cy as usize, cz as usize);
                    if cx >= nx || cy >= ny || cz >= nz {
                        continue;
                    }
                    let ci = (cz * ny + cy) * nx + cx;
                    if grid.scale[ci] > 0.0 {
                        hit[ci] = true;
                    }
                }
            }
        }
    };
    for &n in &problem.fixed {
        mark(n);
    }
    for &(n, _, _) in &problem.springs {
        mark(n);
    }
    for &(n, _) in &problem.forces {
        mark(n);
    }
    (0..grid.cell_count() as u32).filter(|&c| hit[c as usize]).collect()
}

/// Mirror partner per design slot for a planar symmetry constraint:
/// reflect each design cell's center across the plane n·p = c and look up
/// the design cell containing the image (u32::MAX = no partner: the mirror
/// lands in void, skin, or outside the grid — those cells stay free). With
/// a grid-aligned plane on a cell boundary the pairing is an exact
/// involution; otherwise it is the nearest-cell approximation.
pub fn build_mirror_pairs(grid: &VoxelGrid, design_cells: &[u32], plane: [f64; 4]) -> Vec<u32> {
    let len = (plane[0] * plane[0] + plane[1] * plane[1] + plane[2] * plane[2]).sqrt();
    if len < 1e-12 {
        return vec![u32::MAX; design_cells.len()];
    }
    let n = [plane[0] / len, plane[1] / len, plane[2] / len];
    let c = plane[3] / len;
    let mut slot_of: std::collections::HashMap<u32, u32> = Default::default();
    for (k, &cell) in design_cells.iter().enumerate() {
        slot_of.insert(cell, k as u32);
    }
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let mut partner = vec![u32::MAX; design_cells.len()];
    for (k, &cell) in design_cells.iter().enumerate() {
        let cell = cell as usize;
        let p = [
            grid.origin[0] + ((cell % nx) as f64 + 0.5) * grid.h,
            grid.origin[1] + (((cell / nx) % ny) as f64 + 0.5) * grid.h,
            grid.origin[2] + ((cell / (nx * ny)) as f64 + 0.5) * grid.h,
        ];
        let d = n[0] * p[0] + n[1] * p[1] + n[2] * p[2] - c;
        let q = [p[0] - 2.0 * d * n[0], p[1] - 2.0 * d * n[1], p[2] - 2.0 * d * n[2]];
        let qx = ((q[0] - grid.origin[0]) / grid.h).floor();
        let qy = ((q[1] - grid.origin[1]) / grid.h).floor();
        let qz = ((q[2] - grid.origin[2]) / grid.h).floor();
        if qx < 0.0 || qy < 0.0 || qz < 0.0 {
            continue;
        }
        let (qx, qy, qz) = (qx as usize, qy as usize, qz as usize);
        if qx >= nx || qy >= ny || qz >= nz {
            continue;
        }
        if let Some(&j) = slot_of.get(&(((qz * ny + qy) * nx + qx) as u32)) {
            partner[k] = j;
        }
    }
    partner
}

/// Average each value with its mirror partner's (no-op for unpaired slots).
fn symmetrize(values: &mut [f64], partner: &[u32], buf: &mut Vec<f64>) {
    buf.clear();
    buf.extend_from_slice(values);
    for k in 0..values.len() {
        let j = partner[k];
        if j != u32::MAX {
            values[k] = 0.5 * (buf[k] + buf[j as usize]);
        }
    }
}

/// Anti-checkerboard / anti-sliver floor for the density-filter radius (cells).
/// The OC update relies on at least this much smoothing; `min_member = 0` keeps
/// exactly this radius, so it reproduces the pre-feature-size behavior.
const MIN_FILTER_RADIUS_CELLS: f64 = 1.6;
/// Upper bound on the explicit filter radius (cells). The stencil cost grows as
/// (2r+1)³, so cap it; a minimum member size that would need a larger radius on
/// a very fine mesh is honored only up to `2 · MAX · h` (the UI warns).
const MAX_FILTER_RADIUS_CELLS: f64 = 8.0;

/// Density-filter radius in CELLS for a physical minimum member size (mm).
/// A linear (conic) density filter of radius `r` suppresses solid members
/// narrower than roughly its diameter `2r`, so `r = min_member / 2`, then to
/// cells via the voxel size `h`, clamped to `[MIN, MAX]`. Expressing the length
/// scale in mm (not cells) makes it mesh-independent: refining the mesh no
/// longer shrinks the protected feature size.
fn filter_radius_cells(min_member_mm: f64, h: f64) -> f64 {
    (min_member_mm / (2.0 * h)).clamp(MIN_FILTER_RADIUS_CELLS, MAX_FILTER_RADIUS_CELLS)
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
        // Dense cell→slot lookup (sentinel u32::MAX): O(1) indexing instead of
        // hashing keeps the (2r+1)³ neighbour sweep affordable at large radii.
        let mut slot_of = vec![u32::MAX; grid.cell_count()];
        for (i, &c) in design_cells.iter().enumerate() {
            slot_of[c as usize] = i as u32;
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
                        let nc = (z * ny + y) * nx + x;
                        let slot = slot_of[nc];
                        if slot != u32::MAX {
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

/// Forward density projection: design variables → PRINTED densities. The
/// linear density filter (min-member / anti-checkerboard) runs first into
/// `x_tilde` (the filtered "blueprint", clamped to the printable band), then
/// the optional self-supporting AM filter maps it to the printed field. With
/// no self-support this is just the filtered, clamped field as before.
fn project(
    filter: &DensityFilter,
    ss: Option<&SelfSupportFilter>,
    x: &[f64],
    x_tilde: &mut [f64],
    x_phys: &mut [f64],
    floor: f64,
    cap: f64,
) {
    filter.apply(x, x_tilde);
    for v in x_tilde.iter_mut() {
        *v = v.clamp(floor, cap);
    }
    match ss {
        Some(s) => s.apply(x_tilde, x_phys),
        None => x_phys.copy_from_slice(x_tilde),
    }
    for v in x_phys.iter_mut() {
        *v = v.clamp(floor, cap);
    }
}

/// Transpose of `project`: dC/d(printed) → dC/d(design variable). Chains the
/// self-support adjoint (recomputed from this iteration's `x_tilde`) then the
/// density filter transpose. The clamps in `project` are inactive at the
/// solution (interior of the band), so they are omitted from the adjoint.
fn project_t(
    filter: &DensityFilter,
    ss: Option<&SelfSupportFilter>,
    x_tilde: &[f64],
    sens_phys: &[f64],
    sens_tilde: &mut [f64],
    sens: &mut [f64],
) {
    match ss {
        Some(s) => s.apply_t(x_tilde, sens_phys, sens_tilde),
        None => sens_tilde.copy_from_slice(sens_phys),
    }
    filter.apply_t(sens_tilde, sens);
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
/// Everything additionally scales by the cell's OCCUPANCY (`grid.scale`,
/// cut boundary cells < 1) so staircase cells don't carry full stiffness.
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
        eps[c as usize] = grid.scale[c as usize];
    }
    for (k, &c) in design_cells.iter().enumerate() {
        let rel = (coeff * x[k].powf(exponent)).min(1.0);
        let e_infill = EMIN_REL as f64 + (1.0 - EMIN_REL as f64) * rel;
        let f = skin_frac[k] as f64;
        eps[c as usize] =
            (grid.scale[c as usize] as f64 * (f + (1.0 - f) * e_infill)) as f32;
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
    // SOLID topology mode bypasses the skin band: the auto-frozen load/support
    // cells become the only always-solid cells (reusing the skin path), every
    // other solid cell is a free design cell. Infill modes keep the wall/shell
    // skin model.
    let SkinSplit { skin, design: design_cells, skin_frac } = if params.solid_mode {
        build_solid_split(grid, &frozen_cells_from_problem(grid, problem))
    } else {
        classify_cells(
            grid,
            params.wall_mm,
            params.top_mm,
            params.bottom_mm,
            params.composite_skin,
        )
    };
    if design_cells.is_empty() {
        return Err(OptimizeError::NoInterior);
    }
    // Infill volume share per design cell — occupancy × (1 − wall-band
    // fraction): a composite cell is partly wall, a cut boundary cell is
    // partly outside the part. Means and the mass budget weight by it so
    // "mean infill" keeps meaning what a slicer percentage means.
    let w: Vec<f64> = design_cells
        .iter()
        .zip(&skin_frac)
        .map(|(&c, &f)| grid.scale[c as usize] as f64 * (1.0 - f as f64))
        .collect();
    let w_sum: f64 = w.iter().sum();
    // Solid volume of the wall band inside design cells (occupancy-weighted).
    let sum_f: f64 = design_cells
        .iter()
        .zip(&skin_frac)
        .map(|(&c, &f)| grid.scale[c as usize] as f64 * f as f64)
        .sum();
    // Skin volume + design volume = the part's solid volume (in cells).
    let vol_skin: f64 = skin.iter().map(|&c| grid.scale[c as usize] as f64).sum();
    let n_solid = vol_skin + sum_f + w_sum;
    // Planar symmetry: mirror partner per design slot (empty = off).
    let sym_partner: Vec<u32> = match params.symmetry {
        Some(plane) => build_mirror_pairs(grid, &design_cells, plane),
        None => Vec::new(),
    };
    let mut sym_buf: Vec<f64> = Vec::new();

    // The budget IS the target interior mean (infill %), so the result is
    // directly comparable to a uniform print at the same slicer percentage.
    let target_mean = params.budget.clamp(params.floor, params.cap);
    let effective_budget = target_mean;

    let filter =
        DensityFilter::build(grid, &design_cells, filter_radius_cells(params.min_member_mm, grid.h));
    // Self-supporting (AM) projection — the always-solid `skin` cells are full
    // supporters. Off unless requested (UI exposes it only in solid mode).
    let ss: Option<SelfSupportFilter> = if params.self_support {
        Some(SelfSupportFilter::build(grid, &design_cells, &skin, params.overhang_deg))
    } else {
        None
    };
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
    // Filtered blueprint (post density-filter, pre self-support); the transpose
    // re-uses this iteration's value.
    let mut x_tilde = vec![0f64; design_cells.len()];
    let mut se = vec![0f64; design_cells.len()];
    let mut sens_phys = vec![0f64; design_cells.len()];
    let mut sens_tilde = vec![0f64; design_cells.len()];
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
        project(&filter, ss.as_ref(), &x, &mut x_tilde, &mut x_phys, params.floor, params.cap);
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
        if crate::cancel::requested() {
            return Err(OptimizeError::Cancelled);
        }
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
        project_t(&filter, ss.as_ref(), &x_tilde, &sens_phys, &mut sens_tilde, &mut sens);
        if !sym_partner.is_empty() {
            symmetrize(&mut sens, &sym_partner, &mut sym_buf);
        }

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
            // Volume measured on the PRINTED densities (filter + self-support)
            // for consistency with what the solve and the export will see.
            project(&filter, ss.as_ref(), &x_new, &mut x_tilde, &mut x_phys, params.floor, params.cap);
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
        // Symmetry projection: mirror-paired cells share their average. The
        // pairwise mean preserves the budget and keeps the OC update stable.
        if !sym_partner.is_empty() {
            symmetrize(&mut x_new, &sym_partner, &mut sym_buf);
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
        let mass_frac = (vol_skin + sum_f + sum_wx) / n_solid;
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

    // Final physical field — symmetrized AFTER the projection so the output
    // (and the bins/regions built from it) is exactly mirror-symmetric even
    // when the filter stencil isn't.
    project(&filter, ss.as_ref(), &x, &mut x_tilde, &mut x_phys, params.floor, params.cap);
    if !sym_partner.is_empty() {
        symmetrize(&mut x_phys, &sym_partner, &mut sym_buf);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radius_off_is_the_floor() {
        // min_member = 0 ⇒ exactly the pre-feature-size radius (today's value),
        // independent of mesh size.
        assert_eq!(filter_radius_cells(0.0, 0.5), MIN_FILTER_RADIUS_CELLS);
        assert_eq!(filter_radius_cells(0.0, 0.1), MIN_FILTER_RADIUS_CELLS);
    }

    #[test]
    fn radius_grows_on_fine_meshes_and_caps() {
        // r = min_member / (2h): a fixed physical size needs a bigger cell
        // radius as the mesh refines, until the perf cap bites.
        let coarse = filter_radius_cells(2.0, 1.0); // 2/(2·1) = 1.0 → floored to 1.6
        let fine = filter_radius_cells(2.0, 0.25); // 2/(2·0.25) = 4.0
        assert_eq!(coarse, MIN_FILTER_RADIUS_CELLS);
        assert!((fine - 4.0).abs() < 1e-12);
        // Huge request on a fine mesh is clamped to the cap.
        assert_eq!(filter_radius_cells(50.0, 0.1), MAX_FILTER_RADIUS_CELLS);
    }

    #[test]
    fn radius_is_mesh_independent_in_mm() {
        // The protected size in mm (≈ 2·r·h) is the same at h and h/2 while
        // the radius is in the unclamped band.
        let mm_a = 2.0 * filter_radius_cells(3.0, 0.5) * 0.5;
        let mm_b = 2.0 * filter_radius_cells(3.0, 0.25) * 0.25;
        assert!((mm_a - 3.0).abs() < 1e-12);
        assert!((mm_b - 3.0).abs() < 1e-12);
        assert!((mm_a - mm_b).abs() < 1e-12);
    }

    /// Filtered peak of a one-cell-thick high-density wall (a thin member):
    /// a large minimum member size must blur it below a mid threshold, while
    /// the off (floor) radius leaves it standing.
    fn thin_wall_filtered_peak(min_member_mm: f64) -> f64 {
        let (nx, ny, nz, h) = (25usize, 5usize, 5usize, 0.5);
        let grid = VoxelGrid::solid_box(nx, ny, nz, h);
        let design: Vec<u32> = (0..grid.cell_count() as u32).collect();
        let filter = DensityFilter::build(&grid, &design, filter_radius_cells(min_member_mm, h));
        // Floor 0.1 everywhere; a single x-plane (cx = 12) at 1.0 — a thin wall.
        let wall_cx = 12usize;
        let mut x = vec![0.1f64; design.len()];
        for (slot, &c) in design.iter().enumerate() {
            if (c as usize) % nx == wall_cx {
                x[slot] = 1.0;
            }
        }
        let mut x_phys = vec![0.0f64; design.len()];
        filter.apply(&x, &mut x_phys);
        // Sample an interior wall cell (full stencil in every direction).
        let sample = grid.cell_index(wall_cx, ny / 2, nz / 2) as usize;
        x_phys[sample]
    }

    #[test]
    fn min_member_blurs_out_a_thin_wall() {
        let off = thin_wall_filtered_peak(0.0); // r = 1.6
        let big = thin_wall_filtered_peak(50.0); // r = 8 (capped)
        assert!(off > 0.5, "thin wall should survive with min_member off, got {off}");
        assert!(big < 0.5, "thin wall should blur out under a large min_member, got {big}");
        assert!(big < off, "more smoothing must lower the thin feature's peak");
    }
}
