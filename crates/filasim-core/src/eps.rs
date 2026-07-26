// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Per-cell stiffness factors ("eps") — the ONE place their lifecycle lives.
//!
//! Construction: [`grid_eps`] (occupancy-only solid solve), [`build_eps`]
//! (occupancy × composite-skin blend × infill power law), [`resolve_eps`]
//! (buildsim's override-or-hull fallback). Coarsening: [`average_coarse_eps`]
//! (the multigrid hierarchy cascade). Un-mixing: [`material_factor`] divides
//! the geometric occupancy back out for stress display. Every rule here is
//! numerically load-bearing — regression-locked, do not "clean up" the math.

use crate::voxel::VoxelGrid;

/// Relative stiffness floor for non-void cells (keeps K SPD without turning
/// near-empty cells into structure).
pub const EMIN_REL: f32 = 1e-6;

/// Per-cell stiffness factors for a plain solid solve (grid occupancy).
pub fn grid_eps(grid: &VoxelGrid) -> Vec<f32> {
    let mut eps = vec![0f32; grid.cell_count()];
    for (i, &sc) in grid.scale.iter().enumerate() {
        if sc > 0.0 {
            eps[i] = EMIN_REL + (1.0 - EMIN_REL) * sc;
        }
    }
    eps
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

/// Per-cell MATERIAL VOLUME FRACTION (the mass field): occupancy × (solid skin
/// band + interior at its infill DENSITY). The mass analog of [`build_eps`] —
/// LINEAR in the density `x` (mass ∝ material volume) where `build_eps` applies
/// the `E/E0 = coeff·x^exponent` stiffness law. Feed this to the self-weight
/// body force (DESIGN §16 dec. 3) so a 20 %-infill cell weighs 0.2× solid, not
/// its (much smaller) stiffness fraction. Skin and cut boundary cells keep
/// their occupancy; void cells stay 0. Matches the mass-readout compositing
/// (`solve_printed`, modal lumped mass) exactly.
pub fn build_vfrac(
    grid: &VoxelGrid,
    design_cells: &[u32],
    skin_frac: &[f32],
    x: &[f64],
) -> Vec<f32> {
    // Skin/solid = occupancy, void = 0 (both already in grid.scale); only the
    // design cells blend wall band (solid) with interior infill density.
    let mut vf = grid.scale.clone();
    for (k, &c) in design_cells.iter().enumerate() {
        let occ = grid.scale[c as usize] as f64;
        let f = skin_frac[k] as f64;
        vf[c as usize] = (occ * (f + (1.0 - f) * x[k])) as f32;
    }
    vf
}

/// Buildsim's eps: a caller-supplied as-printed field when it matches the
/// (padded) grid, else the solid-hull occupancy map. The length check is the
/// safety net — a stale override can never desync the grid. Deliberately a
/// WRAPPER around [`grid_eps`], not a construction rule of its own: the
/// `Some` path must pass the optimizer's field through untouched (see
/// `buildsim` for why stiffness and inherent-strain force share this scale).
pub fn resolve_eps(g: &VoxelGrid, over: Option<&[f32]>) -> Vec<f32> {
    match over {
        Some(e) if e.len() == g.cell_count() => e.to_vec(),
        _ => grid_eps(g),
    }
}

/// Child-averaged stiffness for the next-coarser grid. `c[a]` says whether axis
/// `a` is halved (SEMICOARSENING: a plate too thin to coarsen through-thickness
/// still coarsens in-plane); every halved axis must have an even fine dimension.
/// The average runs over the 2^(halved axes) children.
pub(crate) fn average_coarse_eps(
    fine_eps: &[f32],
    fnx: usize,
    fny: usize,
    fnz: usize,
    c: [bool; 3],
) -> Vec<f32> {
    let step = [
        if c[0] { 2usize } else { 1 },
        if c[1] { 2 } else { 1 },
        if c[2] { 2 } else { 1 },
    ];
    assert!(fnx % step[0] == 0 && fny % step[1] == 0 && fnz % step[2] == 0);
    let (nx, ny, nz) = (fnx / step[0], fny / step[1], fnz / step[2]);
    let kids = (step[0] * step[1] * step[2]) as f32;
    let mut eps = vec![0f32; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let mut s = 0f32;
                for dz in 0..step[2] {
                    for dy in 0..step[1] {
                        for dx in 0..step[0] {
                            s += fine_eps[((step[2] * cz + dz) * fny + step[1] * cy + dy) * fnx
                                + step[0] * cx
                                + dx];
                        }
                    }
                }
                // Plain child average. Occupancy-boosted variants were tried
                // for thin-shell parts (Benchy) and measurably HURT
                // convergence (+8-12% iterations) — the softer operator is
                // the better preconditioner here.
                eps[(cz * ny + cy) * nx + cx] = s / kids;
            }
        }
    }
    eps
}

/// Occupancy-decoupled (material) modulus/strength factor per cell.
///
/// The solve scales each cell's stiffness by `eps = occupancy × material
/// density` (the construction rules above), where `grid.scale` is the
/// finite-cell geometric occupancy (for a plain solid solve `eps ==
/// grid.scale`). That occupancy scaling is correct for stiffness and mass,
/// but a cut boundary cell is *fully dense material partially covering its
/// cube* — scaling its stress by the occupancy under-reads the true material
/// stress and paints the staircase stripes seen on curved skins. This
/// returns `eps / occupancy` (clamped to 1): the material density factor
/// alone (1 for solid/skin, rel(ρ) for graded infill), so a stress evaluated
/// with it is the TRUE material / homogenized macro stress with the meshing
/// artifact removed. Void cells (occupancy 0) stay 0.
///
/// Feed this to `stress::cell_field` in place of `eps`. Using the SAME factor
/// for the SF allowable leaves the safety factor unchanged (the factor
/// cancels in allowable / stress).
pub fn material_factor(grid: &VoxelGrid, eps: &[f32]) -> Vec<f32> {
    eps.iter()
        .zip(&grid.scale)
        .map(|(&e, &occ)| if occ > 0.0 { (e / occ).min(1.0) } else { 0.0 })
        .collect()
}
