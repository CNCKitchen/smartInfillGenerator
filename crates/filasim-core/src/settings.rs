// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Settings Optimizer (DESIGN §20): the lightest UNIFORM print settings that
//! still hold a target safety factor.
//!
//! The graded/binary/solid optimizers answer "where should material go?". This
//! answers the question most users actually ask their slicer: *"how many walls
//! and how much infill do I need?"* — a search over PRINT SETTINGS on the
//! as-printed model, not over a density field.
//!
//! Structure:
//! - [`WallGeometry`] — one wall count's cell classification (`classify_cells`
//!   with `topBottomLayers = ceil(perimeters·lineWidth / layerHeight)`, §20
//!   dec. 4) plus its volume components. Built once per wall count; the mass of
//!   every density on that row then needs NO solve (dec. 8's weight pruning).
//! - [`Sweep`] — the frozen grid + per-load-step solver caches. Every candidate
//!   is classified on the SAME grid and enters through composite-skin fractions,
//!   so the safety factors compare apples to apples (dec. 9) and consecutive
//!   solves reuse the multigrid hierarchy.
//! - [`search`] — per wall count, a bisection on the density grid for the
//!   smallest feasible density (≤ 4 solves), wall counts pruned once their
//!   lightest possible print is heavier than the best already found.
//!
//! The evaluation scalar is the §17 criterion — `sf_cells → smooth_masked →
//! sf_min` over the §20 [`crate::strength::criterion_mask`] (BC
//! singularity zone excluded), min over every included load step (§17 dec. 5:
//! safety is worst-case, never weighted).

use crate::eps::{build_eps, build_vfrac};
use crate::pipeline::EvalLaw;
use crate::simp::{classify_cells, step_displacement, LoadSet, OptimizeError};
use crate::solve::{solve_cached, solve_cached_rhs, NodeProblem, SolveSettings, SolverCache};
use crate::strength::{self, StrengthSpec};
use crate::voxel::VoxelGrid;

/// Density search band (§20 dec. 3): 10–70 % in 5 % steps. Above 70 % the
/// honest answer is "more walls", not more infill — Gibson–Ashby validity ends
/// and the skin dominates anyway.
pub const DENSITY_MIN: f64 = 0.10;
pub const DENSITY_MAX: f64 = 0.70;
pub const DENSITY_STEP: f64 = 0.05;

/// Wall band (§20 dec. 4). The store clamps perimeters to 1–8, but the search
/// starts at **2**: a single perimeter is not a print anyone ships — one
/// damaged extrusion is a hole through the wall, and the composite-skin model
/// is at its least trustworthy when the band is thinner than a cell. The
/// optimizer must not "save weight" by recommending it.
pub const WALLS_MIN: u32 = 2;
pub const WALLS_MAX: u32 = 8;

/// Weight tie band (§20 dec. 8): candidates within this of the best weight are
/// a tie, decided by the higher measured safety factor.
pub const WEIGHT_TIE: f64 = 0.01;

/// The 13-step delivered density grid, ascending.
pub fn density_grid() -> Vec<f64> {
    let n = ((DENSITY_MAX - DENSITY_MIN) / DENSITY_STEP).round() as usize;
    (0..=n).map(|i| DENSITY_MIN + i as f64 * DENSITY_STEP).collect()
}

/// The wall-count axis, ascending.
pub fn wall_grid() -> Vec<u32> {
    (WALLS_MIN..=WALLS_MAX).collect()
}

/// Top/bottom shell layers implied by a wall count (§20 dec. 4): CEILING, so
/// the adhesion-critical shell is never thinner than the walls just justified.
pub fn top_bottom_layers(wall: u32, line_width: f64, layer_height: f64) -> u32 {
    let h = layer_height.max(1e-6);
    ((wall as f64 * line_width) / h).ceil().max(1.0) as u32
}

/// Everything the sweep needs about the print settings + criterion.
pub struct SweepCfg<'a> {
    /// Wall counts to consider, ascending.
    pub walls: &'a [u32],
    /// Infill fractions to consider, ascending.
    pub densities: &'a [f64],
    /// Held at the user's current values (printer choices, not strength knobs).
    pub line_width: f64,
    pub layer_height: f64,
    pub composite_skin: bool,
    /// Calibrated E(ρ) law of the current infill pattern.
    pub eval: EvalLaw,
    /// Required minimum safety factor.
    pub target: f64,
    /// Solid allowables + which measure to enforce (§17 dec. 2).
    pub spec: StrengthSpec,
    /// BC singularity exclusion per cell (§20 dec. 5); empty ⇒ none.
    pub bc_excl: &'a [bool],
    /// Material bulk density, tonne/mm³ (mass comes out in grams).
    pub material_density: f64,
}

/// One wall count's classification of the frozen grid + its volume components.
/// The three volumes are occupancy-weighted cell counts; multiply by `h³ · ρ`
/// for mass, so every density on this row is a multiplication, not a solve.
pub struct WallGeometry {
    pub wall: u32,
    pub wall_mm: f64,
    pub top_bottom_layers: u32,
    pub shell_mm: f64,
    skin: Vec<u32>,
    design: Vec<u32>,
    skin_frac: Vec<f32>,
    /// Fully-solid skin cells.
    pub vol_skin: f64,
    /// Wall band INSIDE design cells (composite skin's solid share).
    pub vol_wall: f64,
    /// Infill-capable volume (occupancy × (1 − wall fraction)).
    pub vol_infill: f64,
}

impl WallGeometry {
    /// Mass (g) of this wall count at infill fraction `density` — no solve.
    pub fn mass_g(&self, grid: &VoxelGrid, material_density: f64, density: f64) -> f64 {
        let cell_vol = grid.h * grid.h * grid.h;
        (self.vol_skin + self.vol_wall + density * self.vol_infill)
            * cell_vol
            * material_density
            * 1e6
    }

    /// True when the wall/shell settings leave nothing to fill — the part
    /// prints solid and the density axis is meaningless for this row.
    pub fn prints_solid(&self) -> bool {
        self.vol_infill <= 1e-9
    }
}

/// One solved candidate of the walls × density landscape.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub wall: u32,
    pub wall_index: usize,
    pub density: f64,
    pub density_index: usize,
    pub top_bottom_layers: u32,
    pub mass_g: f64,
    /// SF_crit of the envelope — min over every included load step.
    pub sf: f64,
    /// SF_crit per load step (primary first, then the extras in `LoadSet` order).
    pub sf_steps: Vec<f64>,
    pub max_disp: f64,
    pub converged: bool,
}

impl Candidate {
    pub fn feasible(&self, target: f64) -> bool {
        self.sf >= target
    }
}

/// The frozen-grid sweep context: one classification per wall count, one
/// multigrid cache per load step (consecutive candidates differ only in the
/// stiffness field, so the hierarchy is reused instead of rebuilt).
pub struct Sweep<'a> {
    grid: &'a VoxelGrid,
    levels: usize,
    settings: &'a SolveSettings,
    problem: &'a NodeProblem,
    loads: &'a LoadSet,
    cfg: SweepCfg<'a>,
    geoms: Vec<WallGeometry>,
    /// Criterion mask — solid cells minus the BC exclusion. Candidate-
    /// independent here (no solid-topology mode), so it is built once.
    mask: Vec<bool>,
    /// Multigrid cache of the PRIMARY step. Extra steps re-solve cold (they
    /// each carry their own constraint set; one slot would thrash).
    cache: Option<SolverCache>,
    solves: usize,
}

impl<'a> Sweep<'a> {
    /// Classify the frozen grid for every wall count. `problem` is the primary
    /// load step's assembled problem (mass-only when self-weight is active, as
    /// in the optimizer — the body force is rebuilt per candidate from its own
    /// density, §16 dec. 4).
    pub fn new(
        grid: &'a VoxelGrid,
        levels: usize,
        problem: &'a NodeProblem,
        settings: &'a SolveSettings,
        loads: &'a LoadSet,
        cfg: SweepCfg<'a>,
    ) -> Self {
        let geoms = cfg
            .walls
            .iter()
            .map(|&w| {
                let wall_mm = w as f64 * cfg.line_width;
                let tb = top_bottom_layers(w, cfg.line_width, cfg.layer_height);
                let shell_mm = (tb as f64 * cfg.layer_height).min(5.0);
                let split = classify_cells(grid, wall_mm, shell_mm, shell_mm, cfg.composite_skin);
                let vol_skin: f64 =
                    split.skin.iter().map(|&c| grid.scale[c as usize] as f64).sum();
                let (mut vol_wall, mut vol_infill) = (0f64, 0f64);
                for (k, &c) in split.design.iter().enumerate() {
                    let occ = grid.scale[c as usize] as f64;
                    let f = split.skin_frac[k] as f64;
                    vol_wall += occ * f;
                    vol_infill += occ * (1.0 - f);
                }
                WallGeometry {
                    wall: w,
                    wall_mm,
                    top_bottom_layers: tb,
                    shell_mm,
                    skin: split.skin,
                    design: split.design,
                    skin_frac: split.skin_frac,
                    vol_skin,
                    vol_wall,
                    vol_infill,
                }
            })
            .collect();
        // Solid cells minus the singularity zone (§20 dec. 5/7).
        let mask = strength::criterion_mask(grid, &[], &[], false, cfg.bc_excl);
        Sweep {
            grid,
            levels,
            settings,
            problem,
            loads,
            cfg,
            geoms,
            mask,
            cache: None,
            solves: 0,
        }
    }

    pub fn cfg(&self) -> &SweepCfg<'a> {
        &self.cfg
    }

    pub fn geometry(&self, wall_index: usize) -> &WallGeometry {
        &self.geoms[wall_index]
    }

    /// Cells the criterion scores (mask size — the honest denominator behind
    /// "N cells excluded near supports").
    pub fn scored_cells(&self) -> usize {
        self.mask.iter().filter(|&&m| m).count()
    }

    /// Total solves issued so far (all load steps).
    pub fn solves(&self) -> usize {
        self.solves
    }

    /// Mass (g) of a landscape cell — no solve (§20 dec. 8's pruning input).
    pub fn mass_of(&self, wall_index: usize, density_index: usize) -> f64 {
        self.geoms[wall_index].mass_g(
            self.grid,
            self.cfg.material_density,
            self.cfg.densities[density_index],
        )
    }

    /// Solve ONE landscape cell (see [`Sweep::evaluate_keep`]), discarding the
    /// displacement field.
    pub fn evaluate(
        &mut self,
        wall_index: usize,
        density_index: usize,
    ) -> Result<Candidate, OptimizeError> {
        self.evaluate_keep(wall_index, density_index).map(|(c, _, _)| c)
    }

    /// Solve ONE landscape cell: build the candidate's stiffness field, solve
    /// every included load step, and reduce each to SF_crit. The envelope (min
    /// over steps) is the candidate's score. Returns the candidate plus the
    /// PRIMARY step's displacement and the stiffness field it was solved with,
    /// so the caller can render this candidate (the live preview) or promote
    /// the winner to a real result without re-solving.
    pub fn evaluate_keep(
        &mut self,
        wall_index: usize,
        density_index: usize,
    ) -> Result<(Candidate, Vec<f64>, Vec<f32>), OptimizeError> {
        let density = self.cfg.densities[density_index];
        let g = &self.geoms[wall_index];
        let x = vec![density; g.design.len()];
        let eps = build_eps(
            self.grid,
            &g.skin,
            &g.design,
            &g.skin_frac,
            &x,
            self.cfg.eval.exp,
            self.cfg.eval.coeff,
        );
        // Self-weight (§16 dec. 4): this candidate's own material fraction, so
        // a 10 %-infill print is not asked to carry a 70 %-infill body load.
        let vfrac = if self.loads.has_self_weight() {
            build_vfrac(self.grid, &g.design, &g.skin_frac, &x)
        } else {
            Vec::new()
        };

        // ---- primary step (cached hierarchy) ----
        let primary = if let Some(b) = self.loads.primary_body {
            let sw = crate::simp::self_weight_rhs(self.grid, b, &vfrac);
            solve_cached_rhs(
                &mut self.cache,
                self.grid,
                self.levels,
                self.problem,
                self.settings,
                eps.clone(),
                &[&sw],
                self.settings.tol,
                self.settings.max_iter,
            )
        } else {
            solve_cached(
                &mut self.cache,
                self.grid,
                self.levels,
                self.problem,
                self.settings,
                eps.clone(),
                self.settings.tol,
                self.settings.max_iter,
            )
        }
        .map_err(OptimizeError::Solve)?;
        self.solves += 1;
        let converged = primary.stats.converged;
        let max_disp = max_norm(&primary.u);

        let mut sf_steps = vec![self.sf_crit(&primary.u, &eps)];
        for (j, (p, _w)) in self.loads.extra.iter().enumerate() {
            let u_j = step_displacement(
                self.grid,
                self.levels,
                p,
                self.settings,
                eps.clone(),
                self.loads.extra_body(j),
                &vfrac,
            )
            .map_err(OptimizeError::Solve)?;
            self.solves += 1;
            sf_steps.push(self.sf_crit(&u_j, &eps));
        }
        let sf = sf_steps.iter().copied().fold(f64::INFINITY, f64::min);
        let candidate = Candidate {
            wall: g.wall,
            wall_index,
            density,
            density_index,
            top_bottom_layers: g.top_bottom_layers,
            mass_g: g.mass_g(self.grid, self.cfg.material_density, density),
            sf,
            sf_steps,
            max_disp,
            converged,
        };
        Ok((candidate, primary.u, eps))
    }

    /// The criterion mask this sweep scores (solid cells minus the §20 BC
    /// singularity zone) — exposed so a caller can locate the binding cell.
    pub fn mask(&self) -> &[bool] {
        &self.mask
    }

    /// The §17 criterion chain on one load step's displacement field.
    fn sf_crit(&self, u64s: &[f64], eps: &[f32]) -> f64 {
        let u: Vec<f32> = u64s.iter().map(|&v| v as f32).collect();
        let cells = strength::sf_cells(
            self.grid,
            &u,
            self.settings.e0,
            self.settings.nu,
            eps,
            &self.mask,
            &self.cfg.spec,
        );
        let sm = strength::smooth_masked(self.grid, &cells, &self.mask);
        strength::sf_min(self.grid, &sm, &self.mask)
    }
}

fn max_norm(u: &[f64]) -> f64 {
    let mut m = 0f64;
    for n in u.chunks_exact(3) {
        m = m.max((n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt());
    }
    m
}

/// Outcome of the §20 search.
pub struct SearchOutcome {
    /// The delivered settings: the lightest feasible candidate, or — when the
    /// target is unreachable anywhere on the grid — the best achievable one
    /// (§20 dec. 11 / §17 dec. 6: infeasibility is routine, not an error).
    pub winner: Candidate,
    pub feasible: bool,
    /// The highest SF seen anywhere (what "best achievable" quotes).
    pub best_sf: f64,
    /// Every candidate actually solved — the landscape (dec. 10).
    pub evaluated: Vec<Candidate>,
    /// Wall counts skipped because their LIGHTEST print already weighed more
    /// than the best feasible candidate. Reported, never silent.
    pub pruned_walls: Vec<u32>,
    /// The search stopped on the CEILING PROBE: the strongest settings the
    /// band can deliver (most walls, densest infill) already missed the
    /// target, so nothing below it can hold. One solve instead of eight rows
    /// to reach the same "not with these settings" — reported, never silent.
    pub ceiling_stop: bool,
}

/// Evaluate one landscape cell, memoized against `out` (the bisection revisits
/// its bracket) and reported through `progress`. The callback receives the
/// candidate plus the displacement/stiffness fields it was solved with, so a
/// front end can paint this candidate live; a memo HIT re-solves nothing and
/// therefore reports nothing.
pub fn eval_cell(
    sweep: &mut Sweep,
    out: &mut Vec<Candidate>,
    wall_index: usize,
    density_index: usize,
    progress: &mut impl FnMut(&Candidate, &[f64], &[f32], usize),
) -> Result<Candidate, OptimizeError> {
    if let Some(c) =
        out.iter().find(|c| c.wall_index == wall_index && c.density_index == density_index)
    {
        return Ok(c.clone());
    }
    let (c, u, eps) = sweep.evaluate_keep(wall_index, density_index)?;
    progress(&c, &u, &eps, sweep.solves());
    out.push(c.clone());
    Ok(c)
}

/// Is `c` a better delivery than `best`? Lighter wins; inside the
/// [`WEIGHT_TIE`] band the higher measured safety factor wins (§20 dec. 8).
fn better(c: &Candidate, best: &Candidate) -> bool {
    if c.mass_g < best.mass_g * (1.0 - WEIGHT_TIE) {
        return true;
    }
    if c.mass_g <= best.mass_g * (1.0 + WEIGHT_TIE) {
        return c.sf > best.sf;
    }
    false
}

/// **The §20 dec. 8 search.** Opens with the CEILING PROBE — the strongest
/// print the band can deliver (most walls at the densest infill). If that
/// misses the target, no lighter setting can reach it either: the search stops
/// after one solve and reports the ceiling as the best achievable, instead of
/// walking every wall count to the same conclusion.
///
/// Otherwise: wall counts ascending; for each, bisect the density grid for the
/// smallest feasible density (SF is monotone-increasing in both axes to first
/// order, and bisection rides out small blips). A wall count whose lightest
/// possible print already outweighs the best feasible candidate is skipped
/// without a single solve — and since mass grows with wall count, so is every
/// heavier one.
///
/// `progress` fires after each solved candidate — see [`eval_cell`].
pub fn search(
    sweep: &mut Sweep,
    mut progress: impl FnMut(&Candidate, &[f64], &[f32], usize),
) -> Result<SearchOutcome, OptimizeError> {
    let target = sweep.cfg.target;
    let n_walls = sweep.cfg.walls.len();
    let n_dens = sweep.cfg.densities.len();
    let mut evaluated: Vec<Candidate> = Vec::new();
    let mut pruned: Vec<u32> = Vec::new();
    let mut best: Option<Candidate> = None;

    // Ceiling probe. Costs one solve when the target IS reachable (the top row
    // is usually pruned on weight before it would be evaluated) and saves the
    // whole sweep when it is not — the case where every row would otherwise be
    // solved to its top density before the driver could say "impossible".
    if n_walls == 0 || n_dens == 0 {
        return Err(OptimizeError::NoInterior);
    }
    let ceiling = {
        let last = n_walls - 1;
        let di = if sweep.geometry(last).prints_solid() { 0 } else { n_dens - 1 };
        eval_cell(sweep, &mut evaluated, last, di, &mut progress)?
    };
    if !ceiling.feasible(target) {
        return Ok(SearchOutcome {
            best_sf: ceiling.sf,
            winner: ceiling,
            feasible: false,
            evaluated,
            pruned_walls: pruned,
            ceiling_stop: true,
        });
    }

    for wi in 0..n_walls {
        // Weight pruning — no solve needed to know this row's floor.
        if let Some(b) = &best {
            let lightest = sweep.mass_of(wi, 0);
            if lightest > b.mass_g * (1.0 + WEIGHT_TIE) {
                pruned.extend(sweep.cfg.walls[wi..].iter().copied());
                break;
            }
        }
        // A row that prints solid has no density axis — one candidate is the
        // whole row (any density gives the same part).
        let solid_row = sweep.geometry(wi).prints_solid();
        let top_di = if solid_row { 0 } else { n_dens - 1 };
        let top = eval_cell(sweep, &mut evaluated, wi, top_di, &mut progress)?;
        if !top.feasible(target) {
            continue; // this wall count cannot reach the target at any density
        }
        let win = if solid_row {
            top
        } else {
            // Bisect [0, n-1] for the smallest feasible index; `hi` stays feasible.
            let (mut lo, mut hi) = (0usize, n_dens - 1);
            while lo < hi {
                let mid = (lo + hi) / 2;
                let c = eval_cell(sweep, &mut evaluated, wi, mid, &mut progress)?;
                if c.feasible(target) {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            eval_cell(sweep, &mut evaluated, wi, hi, &mut progress)?
        };
        match &best {
            Some(b) if !better(&win, b) => {}
            _ => best = Some(win),
        }
    }

    let best_sf = evaluated.iter().map(|c| c.sf).fold(f64::NEG_INFINITY, f64::max);
    let feasible = best.is_some();
    let winner = match best {
        Some(b) => b,
        // Nothing reached the target: deliver the strongest candidate seen
        // (every wall count's top density was evaluated, so this IS the best
        // the settings axis can do).
        None => evaluated
            .iter()
            .max_by(|a, b| a.sf.total_cmp(&b.sf).then(b.mass_g.total_cmp(&a.mass_g)))
            .cloned()
            .ok_or(OptimizeError::NoInterior)?,
    };
    Ok(SearchOutcome {
        winner,
        feasible,
        best_sf,
        evaluated,
        pruned_walls: pruned,
        ceiling_stop: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::{assemble, BcKind, BcSpec};
    use crate::mesh::primitives;
    use crate::pad_for_levels;
    use crate::strength::SfMeasure;

    /// End-to-end §20 search on a tip-loaded cantilever: the driver must land
    /// on a FEASIBLE setting, spend the dec. 8 solve budget (not 104 solves),
    /// prune heavier wall counts, and deliver the lightest of what it found.
    #[test]
    fn search_delivers_the_lightest_feasible_settings() {
        let beam = primitives::boxx([0.0; 3], [60.0, 10.0, 10.0]);
        let grid0 = VoxelGrid::voxelize(&beam, 1.0);
        let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
        let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
        let bcs = vec![
            BcSpec { kind: BcKind::Fixed, tris: vec![0, 1] },
            BcSpec { kind: BcKind::Force([0.0, 0.0, -30.0]), tris: vec![2, 3] },
        ];
        let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
        // Exclude the clamped face's singularity, exactly as the app does.
        let excl = strength::bc_exclusion(&grid, &[&asm.bc_nodes[0]]);
        assert!(excl.iter().any(|&e| e), "the clamped face excludes something");
        let walls = wall_grid();
        let densities = density_grid();
        let cfg = SweepCfg {
            walls: &walls,
            densities: &densities,
            line_width: 0.45,
            layer_height: 0.2,
            composite_skin: true,
            eval: EvalLaw { exp: 1.5, coeff: 1.0 },
            target: 2.0,
            spec: StrengthSpec {
                measure: SfMeasure::Material,
                strength: 50.0,
                strength_z: 35.0,
                shear_z: 21.0,
            },
            bc_excl: &excl,
            material_density: 1.24e-9,
        };
        let loads = LoadSet::default();
        let mut sweep = Sweep::new(&grid, levels, &asm.problem, &settings, &loads, cfg);
        // Mass is a pure function of the settings — monotone in both axes.
        let g0 = sweep.geometry(0);
        assert!(
            g0.mass_g(&grid, 1.24e-9, 0.7) > g0.mass_g(&grid, 1.24e-9, 0.1),
            "denser infill weighs more"
        );
        assert!(
            sweep.mass_of(3, 0) > sweep.mass_of(0, 0),
            "more walls weigh more at the same infill"
        );
        assert_eq!(walls[0], 2, "a single perimeter is never recommended (§20 dec. 4)");

        let out = search(&mut sweep, |_, _, _, _| {}).expect("search");
        assert!(out.feasible, "target 2.0 is reachable on a 60 mm beam");
        assert!(out.winner.sf >= 2.0, "winner meets the target: {}", out.winner.sf);
        // Delivered at a grid density, rounded UP to a 5 % step (dec. 3).
        assert!(
            densities.iter().any(|&d| (d - out.winner.density).abs() < 1e-12),
            "winner sits on the 5 % grid: {}",
            out.winner.density
        );
        // Nothing lighter in the landscape was feasible.
        for c in &out.evaluated {
            assert!(
                !(c.sf >= 2.0 && c.mass_g < out.winner.mass_g * (1.0 - WEIGHT_TIE)),
                "a lighter feasible candidate was passed over: {c:?}"
            );
        }
        // dec. 8's budget: ~15–30 solves, nowhere near the 104-cell full map.
        assert!(
            sweep.solves() <= 40,
            "search stayed inside its solve budget: {}",
            sweep.solves()
        );
        // Heavier wall counts get pruned once a feasible one is banked.
        assert!(
            !out.pruned_walls.is_empty() || out.winner.wall == *walls.last().unwrap(),
            "either the tail was pruned or the winner is the last wall count"
        );
    }

    /// An unreachable target must cost ONE solve, not one per wall count: the
    /// ceiling probe (most walls, densest infill) is the strongest print the
    /// settings axis can deliver, so its failure settles the whole band.
    #[test]
    fn an_unreachable_target_stops_at_the_ceiling() {
        let beam = primitives::boxx([0.0; 3], [60.0, 10.0, 10.0]);
        let grid0 = VoxelGrid::voxelize(&beam, 1.0);
        let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
        let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
        let bcs = vec![
            BcSpec { kind: BcKind::Fixed, tris: vec![0, 1] },
            // Two orders of magnitude past what the beam can carry.
            BcSpec { kind: BcKind::Force([0.0, 0.0, -3000.0]), tris: vec![2, 3] },
        ];
        let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
        let excl = strength::bc_exclusion(&grid, &[&asm.bc_nodes[0]]);
        let walls = wall_grid();
        let densities = density_grid();
        let cfg = SweepCfg {
            walls: &walls,
            densities: &densities,
            line_width: 0.45,
            layer_height: 0.2,
            composite_skin: true,
            eval: EvalLaw { exp: 1.5, coeff: 1.0 },
            target: 2.0,
            spec: StrengthSpec {
                measure: SfMeasure::Material,
                strength: 50.0,
                strength_z: 35.0,
                shear_z: 21.0,
            },
            bc_excl: &excl,
            material_density: 1.24e-9,
        };
        let loads = LoadSet::default();
        let mut sweep = Sweep::new(&grid, levels, &asm.problem, &settings, &loads, cfg);
        let out = search(&mut sweep, |_, _, _, _| {}).expect("search");
        assert!(!out.feasible, "3 kN on a 60 mm beam holds nothing");
        assert!(out.ceiling_stop, "the probe ended the search");
        assert_eq!(sweep.solves(), 1, "one solve settled the whole band");
        assert_eq!(out.evaluated.len(), 1, "and only the probe reached the landscape");
        // The probe IS the strongest print in the band, and is delivered as the
        // best achievable (§20 dec. 11: infeasibility is an answer, not an error).
        assert_eq!(out.winner.wall, *walls.last().unwrap());
        assert!((out.winner.density - DENSITY_MAX).abs() < 1e-9);
        assert!(out.winner.sf < 2.0);
        assert!((out.best_sf - out.winner.sf).abs() < 1e-12);
        assert!(out.pruned_walls.is_empty(), "nothing was pruned on weight");
    }

    /// The exclusion moves the CRITERION only — never the delivered weight,
    /// and never the cells outside the constrained neighborhood. (How far it
    /// moves SF_crit is setup-dependent: a fully-supported end face is barely
    /// singular, a small pad on a big body strongly so — which is exactly why
    /// the radius scales with the patch instead of being a fixed ring.)
    #[test]
    fn exclusion_rescopes_the_criterion_without_touching_mass() {
        let beam = primitives::boxx([0.0; 3], [60.0, 10.0, 10.0]);
        let grid0 = VoxelGrid::voxelize(&beam, 1.0);
        let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
        let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
        let bcs = vec![
            BcSpec { kind: BcKind::Fixed, tris: vec![0, 1] },
            BcSpec { kind: BcKind::Force([0.0, 0.0, -30.0]), tris: vec![2, 3] },
        ];
        let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
        let excl = strength::bc_exclusion(&grid, &[&asm.bc_nodes[0]]);
        let walls = vec![2u32];
        let densities = vec![0.3];
        let loads = LoadSet::default();
        let spec = StrengthSpec {
            measure: SfMeasure::Material,
            strength: 50.0,
            strength_z: 35.0,
            shear_z: 21.0,
        };
        fn cfg_with<'a>(
            walls: &'a [u32],
            densities: &'a [f64],
            spec: StrengthSpec,
            bc_excl: &'a [bool],
        ) -> SweepCfg<'a> {
            SweepCfg {
                walls,
                densities,
                line_width: 0.45,
                layer_height: 0.2,
                composite_skin: true,
                eval: EvalLaw { exp: 1.5, coeff: 1.0 },
                target: 2.0,
                spec,
                bc_excl,
                material_density: 1.24e-9,
            }
        }
        let mut with = Sweep::new(
            &grid,
            levels,
            &asm.problem,
            &settings,
            &loads,
            cfg_with(&walls, &densities, spec, &excl),
        );
        let mut without = Sweep::new(
            &grid,
            levels,
            &asm.problem,
            &settings,
            &loads,
            cfg_with(&walls, &densities, spec, &[]),
        );
        assert!(
            with.scored_cells() < without.scored_cells(),
            "the constrained neighborhood leaves the scored set: {} vs {}",
            with.scored_cells(),
            without.scored_cells()
        );
        let a = with.evaluate(0, 0).expect("with exclusion");
        let b = without.evaluate(0, 0).expect("without exclusion");
        assert!((a.mass_g - b.mass_g).abs() < 1e-9, "the exclusion changes no mass");
        assert!(a.sf > 0.0 && b.sf > 0.0 && (a.sf - b.sf).abs() < 0.25 * b.sf,
            "same design, same ballpark criterion: {} vs {}", a.sf, b.sf);
    }

    #[test]
    fn shell_layers_ceil_the_wall_thickness() {
        // 3 walls × 0.45 = 1.35 mm of wall; at 0.2 mm layers that is 6.75
        // layers — the shell takes 7, never 6 (§20 dec. 4).
        assert_eq!(top_bottom_layers(3, 0.45, 0.2), 7);
        assert_eq!(top_bottom_layers(1, 0.4, 0.2), 2);
        // Never zero, however thin the walls.
        assert_eq!(top_bottom_layers(1, 0.05, 0.6), 1);
    }

    #[test]
    fn density_grid_is_the_13_step_band() {
        let g = density_grid();
        assert_eq!(g.len(), 13);
        assert!((g[0] - 0.10).abs() < 1e-12);
        assert!((g[12] - 0.70).abs() < 1e-12);
        assert!(g.windows(2).all(|w| (w[1] - w[0] - DENSITY_STEP).abs() < 1e-12));
    }

    #[test]
    fn tie_break_prefers_the_stronger_of_two_equal_weights() {
        let base = Candidate {
            wall: 2,
            wall_index: 1,
            density: 0.3,
            density_index: 4,
            top_bottom_layers: 5,
            mass_g: 100.0,
            sf: 2.1,
            sf_steps: vec![2.1],
            max_disp: 0.0,
            converged: true,
        };
        // Same weight, higher SF ⇒ better.
        let stronger = Candidate { sf: 2.4, ..base.clone() };
        assert!(better(&stronger, &base));
        assert!(!better(&base, &stronger));
        // Clearly lighter ⇒ better even at a lower SF.
        let lighter = Candidate { mass_g: 90.0, sf: 2.0, ..base.clone() };
        assert!(better(&lighter, &base));
        // Inside the tie band but weaker ⇒ not better.
        let heavier_weak = Candidate { mass_g: 100.5, sf: 2.0, ..base.clone() };
        assert!(!better(&heavier_weak, &base));
    }
}
