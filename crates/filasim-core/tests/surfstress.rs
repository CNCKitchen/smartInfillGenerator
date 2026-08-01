// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! SURFACE-STRESS accuracy harness (Step 0 of the surface-stress work).
//!
//! `meshbench.rs` answered "which cut-cell convention is least biased?" by
//! reading RAW CELL stress at a single peak location, on axis-aligned geometry.
//! That is not the same question as "how accurate is the stress we SHOW on the
//! part surface", and it is systematically flattering: the Kirsch peak sits at a
//! rim point whose normal is exactly ±y, and the shoulder fillet's peak sits on
//! a flank normal to ±y. Both are the best case for a staircase.
//!
//! This harness measures the thing we actually display — the **nodal-recovered
//! stress tensor evaluated on the true surface** — and reports it as a
//! DISTRIBUTION, binned by how far the surface normal is from a grid axis.
//!
//! Two metrics:
//!
//! * **Traction residual** ‖σ·n‖ / σ_ref on a traction-free surface. This needs
//!   NO analytic solution — on any free surface σ·n must vanish, so every bit of
//!   it is pure error. It is the direct probe of the defect we are chasing (the
//!   staircase can only enforce σ·n = 0 on normals quantized to ±x/±y/±z).
//! * **Analytic error** where a closed form exists (round cantilever: σxx = M·z/I).
//!
//! Geometry is supplied as an exact INDICATOR, not a tessellation, so the STL
//! faceting error cannot contaminate the measurement — what is left is the voxel
//! error alone. The cut-cell convention replicates production exactly
//! (occupancy ≥ 0.15 ⇒ solid, stiffness = occupancy).
//!
//! Run:  cargo test -p filasim-core --test surfstress -- --ignored --nocapture

use filasim_core::cutcell::{CutGeometry, CutStiffness};
use filasim_core::solve::{active_nodes, grid_eps, solve_cached, SolverCache};
use filasim_core::spr::{project_traction, recover_nodal_spr};
use filasim_core::stress::{
    cell_field_cut, cut_normals, material_factor, recover_nodal, recover_surface, FieldKind,
};
use filasim_core::{
    pad_for_levels, solve_nodes, NodeProblem, SolveSettings, VoxelGrid,
};

/// Production cut-cell guard (`voxel.rs` BOUNDARY_FLOOR). Mirrored here so the
/// harness measures the shipping convention, not a variant of it.
const BOUNDARY_FLOOR: f64 = 0.15;

const E0: f64 = 2000.0;
const NU: f64 = 0.3;

// ---------------------------------------------------------------- voxelizing

/// Voxelize an exact inside/outside indicator with the PRODUCTION convention:
/// occupancy from `ss³` supersampling, solid iff occupancy ≥ BOUNDARY_FLOOR,
/// per-cell stiffness = occupancy. `pad` adds a void ring so inflated boundary
/// cells have somewhere to go.
fn voxelize_production(
    inside: &dyn Fn([f64; 3]) -> bool,
    lo: [f64; 3],
    hi: [f64; 3],
    h: f64,
    ss: usize,
    pad: usize,
) -> VoxelGrid {
    let nx = ((hi[0] - lo[0]) / h).ceil() as usize + 2 * pad;
    let ny = ((hi[1] - lo[1]) / h).ceil() as usize + 2 * pad;
    let nz = ((hi[2] - lo[2]) / h).ceil() as usize + 2 * pad;
    let origin = [
        lo[0] - 0.5 * (((nx - 2 * pad) as f64) * h - (hi[0] - lo[0])) - pad as f64 * h,
        lo[1] - 0.5 * (((ny - 2 * pad) as f64) * h - (hi[1] - lo[1])) - pad as f64 * h,
        lo[2] - 0.5 * (((nz - 2 * pad) as f64) * h - (hi[2] - lo[2])) - pad as f64 * h,
    ];
    let occ_at = |cx: usize, cy: usize, cz: usize| -> f64 {
        let mut n = 0u32;
        for a in 0..ss {
            for b in 0..ss {
                for c in 0..ss {
                    let q = [
                        origin[0] + (cx as f64 + (a as f64 + 0.5) / ss as f64) * h,
                        origin[1] + (cy as f64 + (b as f64 + 0.5) / ss as f64) * h,
                        origin[2] + (cz as f64 + (c as f64 + 0.5) / ss as f64) * h,
                    ];
                    if inside(q) {
                        n += 1;
                    }
                }
            }
        }
        n as f64 / (ss * ss * ss) as f64
    };
    let mut scale = vec![0f32; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let occ = occ_at(cx, cy, cz);
                if occ >= BOUNDARY_FLOOR {
                    scale[(cz * ny + cy) * nx + cx] = occ as f32;
                }
            }
        }
    }
    VoxelGrid { nx, ny, nz, h, origin, scale }
}

// ------------------------------------------------------- surface stress probe

/// The six nodal-recovered stress components, sampled on the true surface —
/// i.e. exactly the field the app paints on the part.
///
/// Uses `material_factor` (the shipping default, Theory Manual §11.1) so a cut
/// boundary cell reports true MATERIAL stress rather than an occupancy-scaled
/// one; that is the field the user reads.
/// Which recovery is under test. `Mean` is what ships today.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Recovery {
    /// Volume-averaged nodal recovery (`stress::recover_nodal`) — production.
    Mean,
    /// Quadratic superconvergent patch recovery (`spr::recover_nodal_spr`).
    Spr,
    /// SPR plus the closed-form traction projection at the sample point.
    SprProjected,
    /// F1 only: directional occupancy decoupling on cut cells
    /// (`stress::cut_normals` + `decouple_traction`), then the production mean
    /// recovery. Isolates the cut-cell fix from the surface fit.
    Cut,
    /// F1 + F3: decoupling, then `stress::recover_surface` (degree-1 patch fit
    /// from clean interior cells, evaluated at the boundary cell centre), then
    /// mean recovery. THIS is the shipping wasm read-back path — `cell_field_cut`
    /// → `recover_surface` → `recover_nodal`, as wired in `Model::cell_values_opt`.
    CutSurface,
}

impl Recovery {
    fn label(self) -> &'static str {
        match self {
            Recovery::Mean => "mean (today)",
            Recovery::Spr => "SPR",
            Recovery::SprProjected => "SPR+proj",
            Recovery::Cut => "cut-decoupled",
            Recovery::CutSurface => "cut+surf-rec",
        }
    }
}

/// The h-refinement case (B′) is the expensive one — ~107 min, ~13 GB — so it
/// carries only the modes that can change its verdict: production, and the two
/// read-back fixes. SPR was already measured as no better than the mean there.
const REFINE_MODES: [Recovery; 3] = [Recovery::Mean, Recovery::Cut, Recovery::CutSurface];

const RECOVERIES: [Recovery; 5] = [
    Recovery::Mean,
    Recovery::Spr,
    Recovery::SprProjected,
    Recovery::Cut,
    Recovery::CutSurface,
];

struct SurfaceStress {
    grid: VoxelGrid,
    /// nodal σ interleaved, Voigt order [xx, yy, zz, xy, yz, zx]
    nodal: Vec<f32>,
    mode: Recovery,
}

impl SurfaceStress {
    fn new(grid: &VoxelGrid, u: &[f32], mode: Recovery) -> Self {
        let eps = material_factor(grid, &grid_eps(grid));
        let kinds = [
            FieldKind::Sxx,
            FieldKind::Syy,
            FieldKind::Szz,
            FieldKind::Sxy,
            FieldKind::Syz,
            FieldKind::Szx,
        ];
        // F1: the decoupling only means anything against the occupancy-corrected
        // `eps` above, which is exactly what this probe uses.
        let cut = matches!(mode, Recovery::Cut | Recovery::CutSurface)
            .then(|| cut_normals(grid));
        let mut cellf: Vec<Vec<f32>> = kinds
            .iter()
            .map(|&k| cell_field_cut(grid, u, E0, NU, &eps, None, [0.0; 3], k, cut.as_deref()))
            .collect();
        // F3: fit the boundary cells from the clean interior, per component,
        // BEFORE nodal recovery — the order the shipping path uses.
        if mode == Recovery::CutSurface {
            cellf = cellf.iter().map(|c| recover_surface(grid, c)).collect();
        }
        let nodal = if mode != Recovery::Spr && mode != Recovery::SprProjected {
            // Interleave the six independently mean-recovered components so both
            // paths share one sampler.
            let per: Vec<Vec<f32>> = cellf.iter().map(|c| recover_nodal(grid, c)).collect();
            let nn = per[0].len();
            let mut v = vec![0f32; nn * 6];
            for n in 0..nn {
                for c in 0..6 {
                    v[n * 6 + c] = per[c][n];
                }
            }
            v
        } else {
            let refs: [&[f32]; 6] = [
                &cellf[0], &cellf[1], &cellf[2], &cellf[3], &cellf[4], &cellf[5],
            ];
            recover_nodal_spr(grid, &refs)
        };
        Self { grid: grid.clone(), nodal, mode }
    }

    /// Sample and, for `SprProjected`, enforce `σ·n = 0` at the point.
    fn sigma_on_free_surface(&self, p: [f64; 3], n: [f64; 3]) -> Option<[f64; 6]> {
        let mut s = self.sigma(p)?;
        if self.mode == Recovery::SprProjected {
            project_traction(&mut s, n, [0.0; 3]);
        }
        Some(s)
    }

    /// Active-aware trilinear sample: NaN nodes (touching no solid cell) are
    /// skipped and the remaining weights renormalized, so a surface point just
    /// outside the voxel hull still reads the material it belongs to instead of
    /// being diluted toward zero.
    fn sigma(&self, p: [f64; 3]) -> Option<[f64; 6]> {
        let g = &self.grid;
        let (mx, my, mz) = (g.nx + 1, g.ny + 1, g.nz + 1);
        let f = [
            (p[0] - g.origin[0]) / g.h,
            (p[1] - g.origin[1]) / g.h,
            (p[2] - g.origin[2]) / g.h,
        ];
        let base = [
            (f[0].floor() as i64).clamp(0, mx as i64 - 2) as usize,
            (f[1].floor() as i64).clamp(0, my as i64 - 2) as usize,
            (f[2].floor() as i64).clamp(0, mz as i64 - 2) as usize,
        ];
        let t = [
            (f[0] - base[0] as f64).clamp(0.0, 1.0),
            (f[1] - base[1] as f64).clamp(0.0, 1.0),
            (f[2] - base[2] as f64).clamp(0.0, 1.0),
        ];
        let mut acc = [0f64; 6];
        let mut wsum = 0f64;
        for oz in 0..2 {
            for oy in 0..2 {
                for ox in 0..2 {
                    let n = ((base[2] + oz) * my + (base[1] + oy)) * mx + (base[0] + ox);
                    let w = (if ox == 1 { t[0] } else { 1.0 - t[0] })
                        * (if oy == 1 { t[1] } else { 1.0 - t[1] })
                        * (if oz == 1 { t[2] } else { 1.0 - t[2] });
                    if w <= 0.0 || self.nodal[n * 6].is_nan() {
                        continue;
                    }
                    for c in 0..6 {
                        acc[c] += w * self.nodal[n * 6 + c] as f64;
                    }
                    wsum += w;
                }
            }
        }
        if wsum <= 1e-12 {
            return None;
        }
        for a in acc.iter_mut() {
            *a /= wsum;
        }
        Some(acc)
    }
}

/// Traction `t = σ·n` for Voigt-ordered σ = [xx, yy, zz, xy, yz, zx].
fn traction(s: &[f64; 6], n: [f64; 3]) -> [f64; 3] {
    [
        s[0] * n[0] + s[3] * n[1] + s[5] * n[2],
        s[3] * n[0] + s[1] * n[1] + s[4] * n[2],
        s[5] * n[0] + s[4] * n[1] + s[2] * n[2],
    ]
}

fn norm3(v: [f64; 3]) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

/// Angle (deg) between a unit normal and the NEAREST grid axis. 0° = the
/// staircase can represent this face exactly; 54.7° = body diagonal, the worst
/// case a voxel boundary can present.
fn axis_angle_deg(n: [f64; 3]) -> f64 {
    let m = n[0].abs().max(n[1].abs()).max(n[2].abs()).clamp(0.0, 1.0);
    m.acos().to_degrees()
}

/// Error accumulator that reports RMS and max, binned by surface-normal angle.
#[derive(Default)]
struct ErrBins {
    /// (sum of squares, count, max) per bin
    b: Vec<(f64, usize, f64)>,
}

const BIN_EDGES: [f64; 5] = [10.0, 20.0, 30.0, 40.0, 90.0];
const BIN_NAMES: [&str; 5] = ["0-10", "10-20", "20-30", "30-40", "40-55"];

impl ErrBins {
    fn new() -> Self {
        Self { b: vec![(0.0, 0, 0.0); BIN_EDGES.len()] }
    }
    fn add(&mut self, angle_deg: f64, err: f64) {
        let i = BIN_EDGES.iter().position(|&e| angle_deg < e).unwrap_or(BIN_EDGES.len() - 1);
        self.b[i].0 += err * err;
        self.b[i].1 += 1;
        self.b[i].2 = self.b[i].2.max(err.abs());
    }
    fn rms(&self) -> f64 {
        let (s, c) = self.b.iter().fold((0.0, 0usize), |(s, c), x| (s + x.0, c + x.1));
        if c == 0 {
            0.0
        } else {
            (s / c as f64).sqrt()
        }
    }
    fn max(&self) -> f64 {
        self.b.iter().fold(0.0f64, |m, x| m.max(x.2))
    }
    fn print_row(&self, label: &str) {
        print!("    {label:<22}");
        for (ss, c, _) in &self.b {
            if *c == 0 {
                print!("{:>9}", "-");
            } else {
                print!("{:>8.1}%", (ss / *c as f64).sqrt() * 100.0);
            }
        }
        println!("  | {:>7.1}% {:>7.1}%", self.rms() * 100.0, self.max() * 100.0);
    }
    fn header(title: &str) {
        println!("\n    {title}");
        print!("    {:<22}", "normal-to-axis angle:");
        for n in BIN_NAMES {
            print!("{n:>9}");
        }
        println!("  | {:>8} {:>8}", "RMS", "MAX");
    }
}

// ------------------------------------------------- case A: round cantilever

/// Solid round cantilever, radius `r`, length `l`, tip load `-z`.
///
/// The centrepiece case: a cylinder's surface presents EVERY normal orientation
/// relative to the grid in a single solve, so one run sweeps the whole
/// axis-alignment range instead of testing one lucky angle. Both metrics are
/// available — the lateral surface is traction-free (residual metric) and beam
/// theory gives σxx = M·z/I (analytic metric).
fn round_cantilever(r: f64, l: f64, h: f64, exact_cut: bool) -> (VoxelGrid, Vec<f32>, f64) {
    let inside = move |p: [f64; 3]| {
        p[0] >= 0.0 && p[0] <= l && (p[1] * p[1] + p[2] * p[2]) <= r * r
    };
    let grid = voxelize_production(&inside, [0.0, -r, -r], [l, r, r], h, 4, 1);
    let settings = SolveSettings { e0: E0, nu: NU, tol: 1e-7, max_iter: 900, ..Default::default() };
    let (pg, levels) = pad_for_levels(&grid, settings.max_levels);
    let (mx, my, mz) = (pg.nx + 1, pg.ny + 1, pg.nz + 1);
    let active = active_nodes(&pg);
    let npos = |n: usize| -> [f64; 3] {
        let x = n % mx;
        let y = (n / mx) % my;
        let z = n / (mx * my);
        [
            pg.origin[0] + x as f64 * pg.h,
            pg.origin[1] + y as f64 * pg.h,
            pg.origin[2] + z as f64 * pg.h,
        ]
    };
    let f_tip = -10.0f64;
    let mut np = NodeProblem::default();
    let mut load_nodes = Vec::new();
    for n in 0..mx * my * mz {
        if !active[n] {
            continue;
        }
        let p = npos(n);
        if p[0] <= 0.5 * pg.h {
            np.fixed.push(n as u32);
        } else if p[0] >= l - 0.5 * pg.h {
            load_nodes.push(n);
        }
    }
    let inv = 1.0 / load_nodes.len() as f64;
    for n in load_nodes {
        np.forces.push((n as u32, [0.0, 0.0, f_tip * inv]));
    }
    let sol = solve_with_optional_cut(&pg, levels, &np, &settings, inside_ref(&inside), exact_cut);
    (pg, sol, f_tip)
}

/// Erase the closure type so both cases can share the solve helper.
fn inside_ref<'a>(f: &'a dyn Fn([f64; 3]) -> bool) -> &'a dyn Fn([f64; 3]) -> bool {
    f
}

/// Solve with either the shipping ersatz boundary cells or exact cut-cell
/// element matrices, so the harness can A/B the OPERATOR, not just the readout.
fn solve_with_optional_cut(
    pg: &VoxelGrid,
    levels: usize,
    np: &NodeProblem,
    settings: &SolveSettings,
    inside: &dyn Fn([f64; 3]) -> bool,
    exact_cut: bool,
) -> Vec<f32> {
    if !exact_cut {
        return solve_nodes(pg, levels, np, settings).expect("solve").u;
    }
    let cg = CutGeometry::build(pg, inside, 4);
    let cut = CutStiffness::build(&cg, pg.cell_count(), settings.e0, settings.nu, pg.h);
    let mut cache = SolverCache::build(pg, levels, np, settings, grid_eps(pg));
    cache.set_cut(cut);
    let mut slot = Some(cache);
    let r = solve_cached(
        &mut slot,
        pg,
        levels,
        np,
        settings,
        grid_eps(pg),
        settings.tol,
        settings.max_iter,
    )
    .expect("cut solve");
    r.u.iter().map(|&v| v as f32).collect()
}

#[test]
#[ignore]
fn surf_round_cantilever() {
    use std::f64::consts::PI;
    println!("\n=== CASE A: round cantilever — surface stress vs analytic ===");
    println!("  r=8 L=100, tip load 10 N. Lateral surface is traction-free and");
    println!("  sweeps every normal orientation, so one solve covers all angles.");
    println!("  Errors are % of the peak reference stress in the sampled band.");

    let (r, l) = (8.0f64, 100.0f64);
    let inertia = PI * r.powi(4) / 4.0;

    for &h in &[1.0, 0.5] {
      for &exact_cut in &[false, true] {
        let (pg, u, f_tip) = round_cantilever(r, l, h, exact_cut);

        // Sample the mid-span band only: away from the clamped root and the
        // loaded tip, where St-Venant end effects and BC singularities live.
        let (x0, x1) = (0.30 * l, 0.70 * l);
        let sigma_ref = (-f_tip) * (l - x0) * r / inertia; // peak |σxx| in the band
        println!(
            "\n  h={h}  ({:.0} cells across the diameter, σ_ref={sigma_ref:.2} MPa)  boundary cells: {}",
            2.0 * r / h,
            if exact_cut { "EXACT cut-cell KE" } else { "occupancy ersatz (today)" }
        );

        let mut resid_rows = Vec::new();
        let mut sxx_rows = Vec::new();
        for mode in RECOVERIES {
            let probe = SurfaceStress::new(&pg, &u, mode);
            let mut resid = ErrBins::new();
            let mut sxx_err = ErrBins::new();
            let n_theta = 360;
            let n_x = 21;
            for ix in 0..n_x {
                let x = x0 + (x1 - x0) * ix as f64 / (n_x - 1) as f64;
                for it in 0..n_theta {
                    let th = 2.0 * PI * it as f64 / n_theta as f64;
                    let (cy, cz) = (r * th.cos(), r * th.sin());
                    // Sample a hair inside the true surface so the trilinear
                    // stencil is dominated by material nodes, not the void ring.
                    let p = [x, cy * (1.0 - 0.25 * h / r), cz * (1.0 - 0.25 * h / r)];
                    let n = [0.0, th.cos(), th.sin()];
                    let Some(s) = probe.sigma_on_free_surface(p, n) else { continue };
                    let ang = axis_angle_deg(n);
                    // Metric 1: traction residual on a free surface (exact zero).
                    resid.add(ang, norm3(traction(&s, n)) / sigma_ref);
                    // Metric 2: bending stress vs beam theory, σxx = -F(L-x)z/I.
                    let exact = (-f_tip) * (l - x) * cz / inertia;
                    sxx_err.add(ang, (s[0] - exact) / sigma_ref);
                }
            }
            resid_rows.push((mode, resid));
            sxx_rows.push((mode, sxx_err));
        }

        ErrBins::header("traction residual ‖σ·n‖/σ_ref  (exact answer: 0)");
        for (m, b) in &resid_rows {
            b.print_row(m.label());
        }
        ErrBins::header("bending stress error (σxx − M·z/I)/σ_ref");
        for (m, b) in &sxx_rows {
            b.print_row(m.label());
        }
      }
    }
    println!("\n  Read the BINS, not the totals: if the 0-10° column is small and");
    println!("  the 40-55° column is large, today's axis-aligned benchmarks are");
    println!("  flattering us and boundary-normal work will pay off.");
}

// -------------------------------------- rotated in-plane plate cases (B and C)

/// Solve a plate-like local geometry rotated by `phi` about z, pulled along its
/// LOCAL +x. Rotating the geometry (rather than the load) moves the stress
/// concentration onto a surface whose normal sits at `phi` to the grid axes —
/// which is the whole point, since both textbook cases place their peak on an
/// axis-aligned face at phi = 0.
///
/// Returns the padded grid and displacement field; readout is case-specific.
fn rotated_plate(
    inside_local: &dyn Fn([f64; 3]) -> bool,
    lo_l: [f64; 3],
    hi_l: [f64; 3],
    phi: f64,
    h: f64,
    f_total: f64,
    exact_cut: bool,
) -> (VoxelGrid, Vec<f32>, Box<dyn Fn([f64; 3]) -> [f64; 3]>) {
    let (c, s) = (phi.cos(), phi.sin());
    let to_local = move |p: [f64; 3]| -> [f64; 3] {
        [p[0] * c + p[1] * s, -p[0] * s + p[1] * c, p[2]]
    };
    let inside_world = |p: [f64; 3]| inside_local(to_local(p));

    // World AABB = rotated local box corners.
    let (mut lo, mut hi) = ([f64::MAX; 3], [f64::MIN; 3]);
    for i in 0..8 {
        let q = [
            if i & 1 == 0 { lo_l[0] } else { hi_l[0] },
            if i & 2 == 0 { lo_l[1] } else { hi_l[1] },
            if i & 4 == 0 { lo_l[2] } else { hi_l[2] },
        ];
        let w = [q[0] * c - q[1] * s, q[0] * s + q[1] * c, q[2]];
        for k in 0..3 {
            lo[k] = lo[k].min(w[k]);
            hi[k] = hi[k].max(w[k]);
        }
    }

    let grid = voxelize_production(&inside_world, lo, hi, h, 6, 1);
    let settings = SolveSettings { e0: E0, nu: NU, tol: 1e-7, max_iter: 1200, ..Default::default() };
    let (pg, levels) = pad_for_levels(&grid, settings.max_levels);
    let (mx, my, mz) = (pg.nx + 1, pg.ny + 1, pg.nz + 1);
    let active = active_nodes(&pg);
    let npos = |n: usize| -> [f64; 3] {
        let x = n % mx;
        let y = (n / mx) % my;
        let z = n / (mx * my);
        [
            pg.origin[0] + x as f64 * pg.h,
            pg.origin[1] + y as f64 * pg.h,
            pg.origin[2] + z as f64 * pg.h,
        ]
    };
    let mut np = NodeProblem::default();
    let mut load_nodes = Vec::new();
    for n in 0..mx * my * mz {
        if !active[n] {
            continue;
        }
        let q = to_local(npos(n));
        if q[0] <= lo_l[0] + 0.5 * pg.h {
            np.fixed.push(n as u32);
        } else if q[0] >= hi_l[0] - 0.5 * pg.h {
            load_nodes.push(n);
        }
    }
    assert!(!load_nodes.is_empty(), "no load nodes at phi={phi}");
    let inv = 1.0 / load_nodes.len() as f64;
    // Pull along LOCAL +x, expressed in world coordinates.
    let dir = [c, s, 0.0];
    for n in load_nodes {
        np.forces.push((
            n as u32,
            [f_total * inv * dir[0], f_total * inv * dir[1], 0.0],
        ));
    }
    let u = solve_with_optional_cut(&pg, levels, &np, &settings, &inside_world, exact_cut);
    (pg, u, Box::new(to_local))
}

#[test]
#[ignore]
fn surf_kirsch_plate_with_hole_offaxis() {
    use std::f64::consts::PI;
    println!("\n=== CASE B: PLATE WITH A HOLE (Kirsch) — off-axis Kt ===");
    println!("  Plate hw=50 t=8, hole a=5, σ∞=1 MPa. Analytic hole-edge Kt = 3.0.");
    println!("  At phi=0 the peak sits on a rim point whose normal is exactly ±y");
    println!("  (the staircase's best case). Rotating moves it off-axis.");

    let (hw, t, a) = (50.0f64, 8.0f64, 5.0f64);
    let sigma_inf = 1.0f64;
    let inside_local = move |p: [f64; 3]| -> bool {
        p[0] >= -hw
            && p[0] <= hw
            && p[1] >= -hw
            && p[1] <= hw
            && p[2] >= 0.0
            && p[2] <= t
            && (p[0] * p[0] + p[1] * p[1]) >= a * a
    };
    let f_total = sigma_inf * (2.0 * hw) * t;

    for &h in &[1.0, 0.5] {
        println!("\n  h={h}  (cells per hole radius ~ {:.0})", a / h);
        println!(
            "    {:<8} {:<24} {:>10} {:>10} {:>12} {:>12}",
            "rotation", "boundary", "Kt(s1)", "Kt err%", "resid RMS%", "resid MAX%"
        );
        for &deg in &[0.0f64, 15.0, 30.0, 45.0] {
            let phi = deg.to_radians();
          for &exact_cut in &[false, true] {
            let (pg, u, _tl) =
                rotated_plate(&inside_local, [-hw, -hw, 0.0], [hw, hw, t], phi, h, f_total, exact_cut);
            for mode in RECOVERIES {
                let probe = SurfaceStress::new(&pg, &u, mode);
                // Peak von Mises on the hole rim at mid-thickness (VM is rotation
                // invariant, and at the rim σ_rr = σ_zz = 0 so VM = |σ_θθ| = Kt·σ∞).
                let mut kt = 0.0f64;
                let mut resid = ErrBins::new();
                let n_theta = 720;
                for it in 0..n_theta {
                    let th = 2.0 * PI * it as f64 / n_theta as f64;
                    // Rim point in LOCAL coords, nudged inside, then to world.
                    let rr = a + 0.30 * h;
                    let ql = [rr * th.cos(), rr * th.sin(), t / 2.0];
                    let (c, s) = (phi.cos(), phi.sin());
                    let p = [ql[0] * c - ql[1] * s, ql[0] * s + ql[1] * c, ql[2]];
                    // Outward normal of the MATERIAL at a bore points INTO the hole.
                    let nl = [-th.cos(), -th.sin(), 0.0];
                    let n = [nl[0] * c - nl[1] * s, nl[0] * s + nl[1] * c, 0.0];
                    let Some(sg) = probe.sigma_on_free_surface(p, n) else { continue };
                    kt = kt.max(principal_max(&sg) / sigma_inf);
                    resid.add(axis_angle_deg(n), norm3(traction(&sg, n)) / (3.0 * sigma_inf));
                }
                println!(
                    "    {:<8} {:<24} {kt:>10.3} {:>+9.1}% {:>11.1}% {:>11.1}%",
                    format!("{deg:.0}°"),
                    format!(
                        "{} / {}",
                        if exact_cut { "EXACT" } else { "ersatz" },
                        mode.label()
                    ),
                    (kt - 3.0) / 3.0 * 100.0,
                    resid.rms() * 100.0,
                    resid.max() * 100.0
                );
            }
          }
        }
    }
    println!("\n  If Kt err% grows sharply from 0° to 45°, the 1-3% figure in the");
    println!("  Verification Manual is an axis-alignment artifact.");
}

// -------------------------- case B′: h-refinement of the hole concentration

/// **Does the concentration error keep converging?** — the question that decides
/// whether SUBMODELING (a local box re-voxelized 4–8× finer, driven by Dirichlet
/// data interpolated from the global solve) can pay.
///
/// Submodeling buys exactly one thing: smaller `h` in the neighbourhood of the
/// hot spot. So it is worth building iff the error is genuinely mesh-driven and
/// still falling at the rate it fell between the two points case B already has
/// (−16.6% at 5 cells/radius, −7.6% at 10). If the sequence flattens instead,
/// the residue is a property of the ELEMENT (trilinear hex, constant strain per
/// direction) and no amount of local refinement of the same element removes it.
///
/// Case B's matrix is the wrong shape for this: it crosses four rotations with
/// two boundary operators, and a 45° rotation inflates the world AABB by √2 in
/// both in-plane directions — 2× the cells — for a variable already shown not to
/// matter. This test therefore fixes ONE configuration (φ=0, shipping ersatz
/// boundary, shipping mean recovery) and spends the budget on `h` instead.
///
/// # Read the order, not the error
///
/// It reports the decision rule (error vs `Kt = 3.0`) *and* a **reference-free**
/// observed order and Richardson limit taken from the Kt sequence alone. That
/// second column is not decoration: 3.0 is the INFINITE-plate, PLANE-STRESS
/// value, and this plate is neither. At `d/W = 0.1` the finite-width (Howland)
/// correction puts the gross-stress Kt near 3.02, and a `t/d = 0.8` section
/// carries some 3-D constraint on top of that. If the discretization converges
/// to, say, 3.05, then "error vs 3.0" flattens at −1.7% for a reason that has
/// nothing to do with mesh size — and reading that as "refinement stopped
/// paying" would be exactly the wrong conclusion. The order `p` computed from
/// successive Kt differences is immune to a wrong reference.
///
/// The free-surface traction residual is reported for the same reason: its exact
/// answer is 0 by construction, so it needs no reference at all.
///
/// # MEASURED VERDICT — refinement does NOT fix the displayed peak
///
/// | h | cells/a | Kt(σ₁) | err vs 3 | ratio p50 | p90 | max | resid RMS | resid MAX |
/// |---|---|---|---|---|---|---|---|---|
/// | 1.0   | 5  | 2.501 | −16.6% | 0.934 | 1.036 | 1.119 | 8.8% | 12.1% |
/// | 0.5   | 10 | 2.773 |  −7.6% | 0.976 | 1.001 | 1.022 | 5.3% |  9.2% |
/// | 0.25  | 20 | 2.976 |  −0.8% | 1.002 | 1.026 | 1.033 | 3.9% | 11.5% |
/// | 0.125 | 40 | 3.138 |  +4.6% | 0.997 | 1.051 | 1.075 | 2.9% | 11.1% |
///
/// **The distribution splits.** The median rim point converges — `ratio_p50`
/// reaches 1.00 by 20 cells per radius and stays — but the upper tail does not:
/// p90 goes 1.001 → 1.026 → 1.051 and the max 1.022 → 1.033 → 1.075, both rising
/// monotonically once the median has settled. The same split shows up with no
/// analytic reference at all in the traction residual, whose exact answer is 0:
/// **RMS falls 3× (8.8 → 2.9%) while its MAX sits flat at ~11% through an 8×
/// refinement.**
///
/// That `p50 → 1.00` is the load-bearing number. Had the scheme been converging
/// to a genuinely higher 3-D answer (finite width plus `t/d = 0.8` constraint
/// could plausibly justify ~1.04), the median would have settled there too, and
/// the h=0.125 reading of 3.138 would be *correct*. It settles at 1.00 instead,
/// so this specimen's true Kt really is ≈3.0 and 3.138 is an over-read.
///
/// **Consequence for submodeling: it does not pay, and this test is the direct
/// evidence.** A submodel buys exactly one thing — smaller `h` around the hot
/// spot — and the h=0.125 column IS that result, computed globally. Going from
/// 10 to 40 cells per radius moves the displayed peak from −7.6% to +4.6%: it
/// does not halve, it overshoots and keeps climbing, and the Richardson limit on
/// the raw Kt sequence *rises* under refinement (3.59 → 3.76), which is what a
/// non-converging sequence looks like. The −0.8% at h=0.25 is not accuracy, it
/// is two errors cancelling — peak-flattening from volume averaging on the way
/// up, boundary roughness on the way down.
///
/// The mechanism is the staircase, and refinement makes it worse in the only
/// statistic the app shows: the number of boundary corners on the rim grows like
/// `1/h`, so a maximum taken over them grows even as each one's typical error
/// shrinks. This also retro-explains the shoulder fillet (7.3 / meshbench 2.6)
/// diverging under refinement — same mechanism, second topology, and a better
/// explanation than the 3-D-constraint guess previously recorded for it.
///
/// See [`surf_kirsch_probe_standoff`] for the control on the one harness choice
/// that could have faked this, and for the caveat it leaves behind.
///
/// Cost warning: h=0.125 is a ~53 M-cell solve (~160 M DOF). It took **98 min
/// and ~13 GB**; the whole test is ~107 min. Do not put it in a routine sweep.
#[test]
#[ignore]
fn surf_kirsch_h_refinement() {
    use std::f64::consts::PI;
    use std::time::Instant;
    println!("\n=== CASE B′: PLATE WITH A HOLE — h-refinement of the concentration ===");
    println!("  Plate hw=50 t=8, hole a=5, σ∞=1 MPa, φ=0, ersatz boundary, mean");
    println!("  recovery — i.e. exactly what ships. Only h moves.");

    let (hw, t, a) = (50.0f64, 8.0f64, 5.0f64);
    let sigma_inf = 1.0f64;
    let inside_local = move |p: [f64; 3]| -> bool {
        p[0] >= -hw
            && p[0] <= hw
            && p[1] >= -hw
            && p[1] <= hw
            && p[2] >= 0.0
            && p[2] <= t
            && (p[0] * p[0] + p[1] * p[1]) >= a * a
    };
    let f_total = sigma_inf * (2.0 * hw) * t;

    println!("\n  Kt = max σ₁ over the whole rim (what the app paints). ratio_* are");
    println!("  σ₁ divided by the Kirsch σ_θθ AT THE SAME POINT, over the tensile");
    println!("  half of the rim — an h-independent normalizer, so it separates 'the");
    println!("  whole field drifts up' (p50 rises) from 'only the extremes do'");
    println!("  (p50 flat, max rises).");
    println!(
        "\n  {:<7} {:<14} {:>7} {:>11} {:>8} {:>9} | {:>8} {:>8} {:>8} | {:>9} {:>9} {:>8}",
        "h",
        "readback",
        "cells/a",
        "cells",
        "Kt(s1)",
        "err vs 3",
        "ratio_p50",
        "ratio_p90",
        "ratio_max",
        "residRMS",
        "residMAX",
        "solve s"
    );

    // The solve dominates this case, so every read-back rides the SAME solve —
    // adding modes costs sampling time, not the 107 minutes. SPR is left out
    // deliberately: it was already measured as no better than the mean here, and
    // the question now is whether the read-back FIXES (F1, F1+F3) converge.
    let mut kts: Vec<Vec<(f64, f64)>> = vec![Vec::new(); REFINE_MODES.len()]; // (h, Kt max)
    let mut p50s: Vec<Vec<(f64, f64)>> = vec![Vec::new(); REFINE_MODES.len()]; // (h, median)
    for &h in &[1.0f64, 0.5, 0.25, 0.125] {
        let t0 = Instant::now();
        let (pg, u, _tl) =
            rotated_plate(&inside_local, [-hw, -hw, 0.0], [hw, hw, t], 0.0, h, f_total, false);
        let secs = t0.elapsed().as_secs_f64();
        let cells = pg.cell_count();

        for (mi, &mode) in REFINE_MODES.iter().enumerate() {
            let probe = SurfaceStress::new(&pg, &u, mode);
            let mut kt = 0.0f64;
            let mut resid = ErrBins::new();
            let mut ratios: Vec<f64> = Vec::new();
            // Angular sampling refines WITH the mesh so the number of samples per
            // boundary cell is constant (~23). Otherwise the fine grids would be
            // undersampled and the peak would look artificially low.
            let n_theta = (720.0 / h.min(1.0)) as usize;
            let rr = a + 0.30 * h;
            for it in 0..n_theta {
                let th = 2.0 * PI * it as f64 / n_theta as f64;
                let p = [rr * th.cos(), rr * th.sin(), t / 2.0];
                let n = [-th.cos(), -th.sin(), 0.0];
                let Some(sg) = probe.sigma_on_free_surface(p, n) else { continue };
                let s1 = principal_max(&sg) / sigma_inf;
                kt = kt.max(s1);
                resid.add(axis_angle_deg(n), norm3(traction(&sg, n)) / (3.0 * sigma_inf));
                // Ratio statistics only where the analytic hoop stress is safely
                // tensile (θ ∈ [45°,135°] ∪ [225°,315°]); near θ=0 it goes
                // compressive, σ₁ saturates at ~0 and the ratio is meaningless.
                let exact = kirsch_stt(a, rr, th, sigma_inf);
                if exact >= 1.0 * sigma_inf {
                    ratios.push(s1 / exact);
                }
            }
            ratios.sort_by(|x, y| x.partial_cmp(y).unwrap());
            let q = |f: f64| ratios[((ratios.len() - 1) as f64 * f) as usize];
            let (p50, p90, rmax) = (q(0.50), q(0.90), q(1.0));
            println!(
                "  {:<7} {:<14} {:>7.0} {cells:>11} {kt:>8.3} {:>+8.1}% | {p50:>8.3} {p90:>8.3} {rmax:>8.3} | {:>8.1}% {:>8.1}% {:>8}",
                format!("{h}"),
                mode.label(),
                a / h,
                (kt - 3.0) / 3.0 * 100.0,
                resid.rms() * 100.0,
                resid.max() * 100.0,
                // The solve is shared across modes; only the first row pays it.
                if mi == 0 { format!("{secs:.0}") } else { "—".to_string() }
            );
            kts[mi].push((h, kt));
            p50s[mi].push((h, p50));
        }
    }

    // Reference-free convergence: observed order from successive differences,
    // and the Richardson limit the sequence is heading for.
    println!("\n  Reference-free convergence (needs no textbook Kt):");
    println!(
        "    {:<12} {:<20} {:>10} {:>9} {:>12}",
        "quantity", "h triple", "Δ", "order p", "extrapolated"
    );
    for (mi, &mode) in REFINE_MODES.iter().enumerate() {
        for (name, seq) in [("Kt (max)", &kts[mi]), ("ratio_p50", &p50s[mi])] {
            for w in seq.windows(3) {
                let (d1, d2) = (w[1].1 - w[0].1, w[2].1 - w[1].1);
                let p = (d1 / d2).abs().log2();
                let lim = w[2].1 + d2 / ((2f64).powf(p) - 1.0);
                println!(
                    "    {:<12} {:<20} {d2:>10.4} {p:>9.2} {lim:>12.3}",
                    format!("{name} [{}]", mode.label()),
                    format!("{}/{}/{}", w[0].0, w[1].0, w[2].0)
                );
            }
        }
    }

    println!("\n  MEASURED (see the doc comment): ratio_p50 settles at 1.00 by 20");
    println!("  cells/radius while p90 and the rim MAX keep climbing, and the");
    println!("  traction-residual MAX stays flat at ~11% through an 8x refinement.");
    println!("  The field converges; the MAX over it does not. Submodeling buys");
    println!("  only smaller h, so it does not fix the displayed peak — decided,");
    println!("  do not re-derive.");
}

/// Control for the one harness choice that could fake case B′'s result.
///
/// B′ samples at `r = a + 0.30·h`, so the probe sits a FIXED fraction of a cell
/// inside the surface and therefore moves physically closer to the jagged
/// boundary as `h` shrinks. That is faithful to production — the app paints the
/// true surface whatever the mesh — but it means "the rim max grows under
/// refinement" could in principle be a property of the probe rather than of the
/// field.
///
/// This sweeps the standoff independently of `h`. If the max ratio grows with
/// refinement at EVERY standoff, the B′ finding is a property of the displayed
/// field. If it only grows when the probe is hard against the boundary, B′ is
/// measuring its own sampling and must be discounted.
///
/// Cheap on purpose: only h = 0.5 and 0.25, which is where the trend reverses.
///
/// # MEASURED — B′ survives, but with a stated caveat
///
/// ```text
/// ratio_p50                        ratio_max
/// h \ standoff  0.15  0.30  0.60  1.00      0.15  0.30  0.60  1.00
/// 0.5          0.953 0.976 1.018 1.069     1.010 1.022 1.052 1.105
/// 0.25         0.985 1.003 1.030 1.055     1.029 1.033 1.049 1.084
/// ```
///
/// **The caveat, stated plainly:** the rim MAX rises under refinement at 0.15
/// and 0.30 cells of standoff, is flat at 0.60, and *falls* at 1.00. So "the
/// displayed peak grows as the mesh refines" is NOT unconditional — it is a
/// property of sampling within about a third of a cell of the boundary, which is
/// what the app does (it paints the true surface at whatever `h` the user has),
/// but it is not a statement about the interior field.
///
/// **What survives regardless of standoff, and is what B′ actually rests on:**
///
/// * `ratio_p50` converges in every column, and the SPREAD across columns
///   narrows with refinement (11.6 points at h=0.5 → 7.0 at h=0.25). The field
///   is converging; only the extreme-value statistic taken over it is not.
/// * `ratio_p50` converges to ≈1.00, not to ≈1.04. That is the number that
///   rules out "the h=0.125 over-read is really convergence to a higher 3-D
///   truth", which was the one reading under which submodeling would have paid.
///
/// Note the standoff trend itself is a real effect, not noise: reading farther
/// inside gives a HIGHER ratio because the analytic hoop stress falls steeply
/// with `r` while the cell-averaged discrete field cannot. That is the same
/// peak-flattening documented in `spr.rs`, seen radially.
#[test]
#[ignore]
fn surf_kirsch_probe_standoff() {
    use std::f64::consts::PI;
    println!("\n=== CASE B″: is B′'s rising rim MAX a probe artifact? ===");
    println!("  Same plate, same solve, sampled at several standoffs from the rim.");
    println!("  ratio = σ₁ / Kirsch σ_θθ at the SAME point, tensile half of the rim.");

    let (hw, t, a) = (50.0f64, 8.0f64, 5.0f64);
    let sigma_inf = 1.0f64;
    let inside_local = move |p: [f64; 3]| -> bool {
        p[0] >= -hw
            && p[0] <= hw
            && p[1] >= -hw
            && p[1] <= hw
            && p[2] >= 0.0
            && p[2] <= t
            && (p[0] * p[0] + p[1] * p[1]) >= a * a
    };
    let f_total = sigma_inf * (2.0 * hw) * t;
    const STANDOFFS: [f64; 4] = [0.15, 0.30, 0.60, 1.00];

    let mut rows: Vec<(f64, Vec<(f64, f64)>)> = Vec::new(); // h -> [(p50, max)]
    for &h in &[0.5f64, 0.25] {
        let (pg, u, _tl) =
            rotated_plate(&inside_local, [-hw, -hw, 0.0], [hw, hw, t], 0.0, h, f_total, false);
        let probe = SurfaceStress::new(&pg, &u, Recovery::Mean);
        let n_theta = (720.0 / h) as usize;
        let mut per_standoff = Vec::new();
        for &so in &STANDOFFS {
            let rr = a + so * h;
            let mut ratios: Vec<f64> = Vec::new();
            for it in 0..n_theta {
                let th = 2.0 * PI * it as f64 / n_theta as f64;
                let p = [rr * th.cos(), rr * th.sin(), t / 2.0];
                let n = [-th.cos(), -th.sin(), 0.0];
                let Some(sg) = probe.sigma_on_free_surface(p, n) else { continue };
                let exact = kirsch_stt(a, rr, th, sigma_inf);
                if exact >= 1.0 * sigma_inf {
                    ratios.push(principal_max(&sg) / sigma_inf / exact);
                }
            }
            ratios.sort_by(|x, y| x.partial_cmp(y).unwrap());
            let q = |f: f64| ratios[((ratios.len() - 1) as f64 * f) as usize];
            per_standoff.push((q(0.50), q(1.0)));
        }
        rows.push((h, per_standoff));
    }

    for (stat, idx) in [("ratio_p50", 0usize), ("ratio_max", 1usize)] {
        println!("\n    {stat} by standoff (fraction of a cell inside the rim)");
        print!("    {:<10}", "h \\ standoff");
        for so in STANDOFFS {
            print!("{so:>10.2}");
        }
        println!();
        for (h, per) in &rows {
            print!("    {h:<10}");
            for s in per {
                print!("{:>10.3}", if idx == 0 { s.0 } else { s.1 });
            }
            println!();
        }
    }
    println!("\n  Read the ratio_max block ACROSS rows at a fixed column. If every");
    println!("  column rises from h=0.5 to h=0.25, refinement really does inflate");
    println!("  the displayed peak and B′ stands.");
}

/// Kirsch hoop stress `σ_θθ(r, θ)` for a remote uniaxial `σ∞` along +x around a
/// hole of radius `a` (infinite plate, plane stress).
///
/// Used only as an **h-independent normalizer**. Its absolute value carries the
/// usual offsets for this specimen — finite width at `d/W = 0.1` and 3-D
/// constraint at `t/d = 0.8`, together worth a few percent — but those do not
/// move with the mesh, so a TREND in `σ₁/σ_θθ` is pure discretization error.
fn kirsch_stt(a: f64, r: f64, th: f64, s_inf: f64) -> f64 {
    let k = a * a / (r * r);
    0.5 * s_inf * ((1.0 + k) - (1.0 + 3.0 * k * k) * (2.0 * th).cos())
}

#[test]
#[ignore]
fn surf_stepped_shaft_fillet_offaxis() {
    println!("\n=== CASE C: STEPPED SHAFT (shoulder fillet) — off-axis Kt ===");
    println!("  Stepped flat bar D=30 d=20 t=3, axial tension, σ_nom=1 MPa.");
    println!("  Textbook Kt (Pilkey/Peterson): r/d=0.10→1.68, r/d=0.15→1.55.");

    let (big_d, small_d, t) = (30.0f64, 20.0f64, 3.0f64);
    let (x0, lbar) = (25.0f64, 50.0f64);
    let (hd, hn) = (big_d / 2.0, small_d / 2.0);
    let sigma_nom = 1.0f64;
    let f_total = sigma_nom * small_d * t;

    for &(r, kt_ref) in &[(2.0f64, 1.68f64), (3.0f64, 1.55f64)] {
        let (cx, cy) = (x0 + r, hn + r);
        let inside_local = move |p: [f64; 3]| -> bool {
            let (x, ay, z) = (p[0], p[1].abs(), p[2]);
            if z < 0.0 || z > t || x < 0.0 || x > lbar {
                return false;
            }
            if ay <= hn {
                return true;
            }
            if x <= x0 {
                return ay <= hd;
            }
            if x <= cx && ay <= cy && ay <= hd {
                let (dx, dy) = (x - cx, ay - cy);
                return dx * dx + dy * dy >= r * r;
            }
            false
        };
        println!("\n  fillet r={r} (r/d={:.2}), reference Kt≈{kt_ref}", r / small_d);
        for &h in &[0.5, 0.25] {
            println!("    h={h}  (cells across the fillet ~ {:.0})", r / h);
            println!(
                "    {:<8} {:<24} {:>10} {:>10} {:>12} {:>12}",
                "rotation", "boundary", "Kt(s1)", "Kt err%", "resid RMS%", "resid MAX%"
            );
            for &deg in &[0.0f64, 45.0] {
              for &exact_cut in &[false, true] {
                let phi = deg.to_radians();
                let (pg, u, _tl) =
                    rotated_plate(&inside_local, [0.0, -hd, 0.0], [lbar, hd, t], phi, h, f_total, exact_cut);
                let (c, s) = (phi.cos(), phi.sin());
                for mode in RECOVERIES {
                    let probe = SurfaceStress::new(&pg, &u, mode);
                    // Walk the fillet arc (local frame), top flank, mid-thickness.
                    let mut kt = 0.0f64;
                    let mut resid = ErrBins::new();
                    let n_arc = 240;
                    for ia in 0..=n_arc {
                        // Arc from the shoulder face (angle pi) to the narrow
                        // flank (3pi/2): the concave quarter round.
                        let ang = std::f64::consts::PI * (1.0 + 0.5 * ia as f64 / n_arc as f64);
                        // Point ON the fillet surface, nudged into the material.
                        let rr = r + 0.30 * h;
                        let ql = [cx + rr * ang.cos(), cy + rr * ang.sin(), t / 2.0];
                        // Material outward normal points toward the fillet centre.
                        let nl = [-ang.cos(), -ang.sin(), 0.0];
                        let p = [ql[0] * c - ql[1] * s, ql[0] * s + ql[1] * c, ql[2]];
                        let n = [nl[0] * c - nl[1] * s, nl[0] * s + nl[1] * c, 0.0];
                        let Some(sg) = probe.sigma_on_free_surface(p, n) else { continue };
                        kt = kt.max(principal_max(&sg) / sigma_nom);
                        resid
                            .add(axis_angle_deg(n), norm3(traction(&sg, n)) / (kt_ref * sigma_nom));
                    }
                    println!(
                        "    {:<8} {:<24} {kt:>10.3} {:>+9.1}% {:>11.1}% {:>11.1}%",
                        format!("{deg:.0}°"),
                        format!(
                            "{} / {}",
                            if exact_cut { "EXACT" } else { "ersatz" },
                            mode.label()
                        ),
                        (kt - kt_ref) / kt_ref * 100.0,
                        resid.rms() * 100.0,
                        resid.max() * 100.0
                    );
                }
              }
            }
        }
    }
    println!("\n  Same read as Case B: watch whether the error is orientation-driven.");
}

// ------------------------------------------------------------- cost benchmark

/// Runtime and memory of the exact cut-cell path against the shipping ersatz,
/// on solids of increasing size. Reports the numbers that decide whether this
/// can ship: setup cost, solve cost, MGCG iteration count (does the smoother
/// still see a consistent operator?) and bytes of per-cell geometry.
#[test]
#[ignore]
fn bench_cut_cell_cost() {
    use std::time::Instant;
    println!("\n=== cut-cell cost: exact vs ersatz ===");
    println!("  solid round cantilever r=8 L=100, tip load. 'iters' = MGCG count;");
    println!("  a big jump there would mean the smoother no longer matches the operator.");
    println!(
        "\n  {:<7} {:>9} {:>8} {:>9} {:>9} {:>8} {:>8} {:>7} {:>7} {:>10}",
        "h", "cells", "solid", "cut", "setup ms", "old ms", "new ms", "it_old", "it_new", "cut mem"
    );

    let (r, l) = (8.0f64, 100.0f64);
    for &h in &[1.0f64, 0.6, 0.4] {
        let inside =
            move |p: [f64; 3]| p[0] >= 0.0 && p[0] <= l && (p[1] * p[1] + p[2] * p[2]) <= r * r;
        let grid = voxelize_production(&inside, [0.0, -r, -r], [l, r, r], h, 4, 1);
        let settings =
            SolveSettings { e0: E0, nu: NU, tol: 1e-6, max_iter: 900, ..Default::default() };
        let (pg, levels) = pad_for_levels(&grid, settings.max_levels);
        let np = cantilever_problem(&pg, l);

        // Setup: moments + element assembly (one-off, geometry only).
        let t0 = Instant::now();
        let cg = CutGeometry::build(&pg, &inside, 4);
        let cut = CutStiffness::build(&cg, pg.cell_count(), settings.e0, settings.nu, pg.h);
        let setup_ms = t0.elapsed().as_secs_f64() * 1e3;

        let t1 = Instant::now();
        let old = solve_nodes(&pg, levels, &np, &settings).expect("ersatz solve");
        let old_ms = t1.elapsed().as_secs_f64() * 1e3;

        let t2 = Instant::now();
        let mut cache = SolverCache::build(&pg, levels, &np, &settings, grid_eps(&pg));
        cache.set_cut(cut.clone());
        let mut slot = Some(cache);
        let new = solve_cached(
            &mut slot,
            &pg,
            levels,
            &np,
            &settings,
            grid_eps(&pg),
            settings.tol,
            settings.max_iter,
        )
        .expect("cut solve");
        let new_ms = t2.elapsed().as_secs_f64() * 1e3;

        println!(
            "  {h:<7} {:>9} {:>8} {:>9} {setup_ms:>9.0} {old_ms:>8.0} {new_ms:>8.0} {:>7} {:>7} {:>9.1}MB",
            pg.cell_count(),
            pg.solid_count(),
            cut.len(),
            old.iterations,
            new.stats.iterations,
            cut.bytes() as f64 / 1048576.0
        );
    }
    println!("\n  Moment form (what the geometry COSTS to carry) vs assembled matrices");
    println!("  (what the matvec needs) is the storage trade — see cutcell.rs.");
}

/// Shared cantilever BC set: clamp x≈0, load the far face in −z.
fn cantilever_problem(pg: &VoxelGrid, l: f64) -> NodeProblem {
    let (mx, my, mz) = (pg.nx + 1, pg.ny + 1, pg.nz + 1);
    let active = active_nodes(pg);
    let mut np = NodeProblem::default();
    let mut load_nodes = Vec::new();
    for n in 0..mx * my * mz {
        if !active[n] {
            continue;
        }
        let x = (n % mx) as f64 * pg.h + pg.origin[0];
        if x <= 0.5 * pg.h {
            np.fixed.push(n as u32);
        } else if x >= l - 0.5 * pg.h {
            load_nodes.push(n);
        }
    }
    let inv = 1.0 / load_nodes.len() as f64;
    for n in load_nodes {
        np.forces.push((n as u32, [0.0, 0.0, -10.0 * inv]));
    }
    np
}

/// Largest principal stress of a Voigt-ordered symmetric tensor.
///
/// This — not von Mises — is what a textbook Kt is defined on. At a free surface
/// they coincide only under plane STRESS; at the mid-thickness of a 3D bar the
/// state tends toward plane STRAIN, where σzz = ν(σxx+σyy) is nonzero and von
/// Mises is simply a different quantity from the reference. Reading σ1 removes
/// that mismatch, which is what made the fillet Kt look divergent.
fn principal_max(s: &[f64; 6]) -> f64 {
    let (sxx, syy, szz, sxy, syz, szx) = (s[0], s[1], s[2], s[3], s[4], s[5]);
    let p1 = sxy * sxy + syz * syz + szx * szx;
    if p1 <= 1e-18 {
        return sxx.max(syy).max(szz);
    }
    let q = (sxx + syy + szz) / 3.0;
    let p2 = (sxx - q).powi(2) + (syy - q).powi(2) + (szz - q).powi(2) + 2.0 * p1;
    let p = (p2 / 6.0).sqrt();
    let (b00, b11, b22) = ((sxx - q) / p, (syy - q) / p, (szz - q) / p);
    let (b01, b12, b02) = (sxy / p, syz / p, szx / p);
    let det = b00 * (b11 * b22 - b12 * b12) - b01 * (b01 * b22 - b12 * b02)
        + b02 * (b01 * b12 - b11 * b02);
    let r = (det / 2.0).clamp(-1.0, 1.0);
    let phi = r.acos() / 3.0;
    q + 2.0 * p * phi.cos()
}

#[allow(dead_code)]
fn von_mises(s: &[f64; 6]) -> f64 {
    (0.5 * ((s[0] - s[1]).powi(2) + (s[1] - s[2]).powi(2) + (s[2] - s[0]).powi(2))
        + 3.0 * (s[3] * s[3] + s[4] * s[4] + s[5] * s[5]))
        .sqrt()
}

// ------------------------------------------------------- CI guard (not ignored)

/// Cheap, fast guard so the harness cannot silently rot: on a coarse round
/// cantilever the free-surface traction residual must be finite and the
/// sampler must actually find material at the surface. This asserts the
/// MACHINERY works — the accuracy numbers themselves live in the #[ignore]d
/// benchmarks above, which are a decision harness, not a pass/fail gate.
#[test]
fn surface_probe_finds_material_and_reports_residual() {
    use std::f64::consts::PI;
    let (r, l, h) = (6.0f64, 40.0f64, 1.5f64);
    let (pg, u, f_tip) = round_cantilever(r, l, h, false);
    let probe = SurfaceStress::new(&pg, &u, Recovery::Mean);
    let inertia = PI * r.powi(4) / 4.0;
    let sigma_ref = (-f_tip) * l * r / inertia;

    let mut found = 0usize;
    let mut worst = 0f64;
    for it in 0..120 {
        let th = 2.0 * PI * it as f64 / 120.0;
        let p = [0.5 * l, r * th.cos() * 0.96, r * th.sin() * 0.96];
        let n = [0.0, th.cos(), th.sin()];
        if let Some(s) = probe.sigma(p) {
            found += 1;
            worst = worst.max(norm3(traction(&s, n)) / sigma_ref);
        }
    }
    assert!(found > 100, "surface sampler found material at only {found}/120 points");
    assert!(worst.is_finite(), "traction residual must be finite, got {worst}");
    // Sanity band: the staircase is bad but not unbounded. If this trips, the
    // sampler or the recovery path changed shape, not just accuracy.
    assert!(worst < 2.0, "free-surface traction residual {worst:.2}x σ_ref is implausible");
}

// ------------------------------------------ CASE D: thin wall (the FDM regime)

/// THE DECIDING CASE. Everything above is chunky solid geometry with 5–12 cells
/// across the feature — the regime where the occupancy ersatz is LEAST wrong.
///
/// The cut-cell shape error scales with how empty the cell is (24% at 79% full,
/// 66% at 21% full, `cutcell::report_occupancy_scaling_error`), so the payoff
/// should be largest where cells are slivers. A thin wall is made of slivers,
/// and a thin wall is what an FDM part IS. If exact cut cells do not pay here,
/// they do not pay anywhere that matters for this product.
///
/// Thin-walled tube cantilever (the `meshbench.rs` 7B geometry, chosen there for
/// exactly this reason): both surfaces are traction-free, the section has a
/// closed-form I, and at h=1 the wall is only 2 cells thick.
#[test]
#[ignore]
fn surf_thin_wall_tube() {
    use std::f64::consts::PI;
    println!("\n=== CASE D: THIN-WALLED TUBE — the FDM-relevant regime ===");
    println!("  Tube ro=10 ri=8 (wall 2.0), L=80, tip load 10 N. Nearly every solid");
    println!("  cell is a boundary sliver, so this is where exact cut cells should");
    println!("  earn their memory — or fail to.");

    let (ro, ri, l) = (10.0f64, 8.0f64, 80.0f64);
    let inertia = PI * (ro.powi(4) - ri.powi(4)) / 4.0;
    let f_tip = -10.0f64;
    // Euler-Bernoulli tip deflection; the FE result sits slightly below it
    // (shear softening) but the ERSATZ-vs-EXACT gap is the signal here.
    let delta_eb = f_tip * l.powi(3) / (3.0 * E0 * inertia);

    for &h in &[1.0f64, 0.667, 0.5] {
        let inside = move |p: [f64; 3]| {
            let rr = p[1] * p[1] + p[2] * p[2];
            p[0] >= 0.0 && p[0] <= l && rr <= ro * ro && rr >= ri * ri
        };
        let grid = voxelize_production(&inside, [0.0, -ro, -ro], [l, ro, ro], h, 4, 1);
        let settings =
            SolveSettings { e0: E0, nu: NU, tol: 1e-7, max_iter: 1200, ..Default::default() };
        let (pg, levels) = pad_for_levels(&grid, settings.max_levels);
        let np = cantilever_problem(&pg, l);

        println!(
            "\n  h={h}  (wall ≈ {:.1} cells, {} solid cells)",
            (ro - ri) / h,
            pg.solid_count()
        );
        println!(
            "    {:<24} {:>12} {:>12} {:>12} {:>12}",
            "boundary", "tip/EB", "σxx RMS%", "resid RMS%", "resid MAX%"
        );

        for &exact_cut in &[false, true] {
            let u = solve_with_optional_cut(&pg, levels, &np, &settings, &inside, exact_cut);

            // Tip deflection: the global-stiffness signal. A thin wall built of
            // slivers is exactly where a wrong boundary stiffness costs most.
            let (mx, my, mz) = (pg.nx + 1, pg.ny + 1, pg.nz + 1);
            let active = active_nodes(&pg);
            let (mut sum, mut cnt) = (0f64, 0usize);
            for n in 0..mx * my * mz {
                if active[n] && (n % mx) as f64 * pg.h + pg.origin[0] >= l - 0.5 * pg.h {
                    sum += u[3 * n + 2] as f64;
                    cnt += 1;
                }
            }
            let tip_ratio = if cnt > 0 { (sum / cnt as f64) / delta_eb } else { f64::NAN };

            // A thin wall is the adversarial case for `recover_surface`: the fit
            // needs clean interior cells (occupancy 1 on all six faces) within
            // two cells, and a 1.5 mm wall may have none, so F3 degrades to
            // nearest-clean or to the raw value. If it helps HERE it helps
            // anywhere; if it hurts here, that is the limit worth knowing.
            for mode in RECOVERIES {
                let probe = SurfaceStress::new(&pg, &u, mode);
                let (x0, x1) = (0.30 * l, 0.70 * l);
                let sigma_ref = (-f_tip) * (l - x0) * ro / inertia;
                let mut resid = ErrBins::new();
                let mut sxx = ErrBins::new();
                // Sample BOTH free surfaces — the bore is as traction-free as the
                // OD, and on a thin wall the two are only a couple of cells apart.
                for (rad, sgn) in [(ro, 1.0f64), (ri, -1.0f64)] {
                    for ix in 0..15 {
                        let x = x0 + (x1 - x0) * ix as f64 / 14.0;
                        for it in 0..240 {
                            let th = 2.0 * PI * it as f64 / 240.0;
                            // Nudge INTO the material: outward for the bore,
                            // inward for the OD.
                            let rr = rad - sgn * 0.25 * h;
                            let p = [x, rr * th.cos(), rr * th.sin()];
                            let n = [0.0, sgn * th.cos(), sgn * th.sin()];
                            let Some(s) = probe.sigma_on_free_surface(p, n) else { continue };
                            let ang = axis_angle_deg(n);
                            resid.add(ang, norm3(traction(&s, n)) / sigma_ref);
                            let exact = (-f_tip) * (l - x) * (rr * th.sin()) / inertia;
                            sxx.add(ang, (s[0] - exact) / sigma_ref);
                        }
                    }
                }
                println!(
                    "    {:<24} {tip_ratio:>12.4} {:>11.1}% {:>11.1}% {:>11.1}%",
                    format!(
                        "{} / {}",
                        if exact_cut { "EXACT" } else { "ersatz" },
                        mode.label()
                    ),
                    sxx.rms() * 100.0,
                    resid.rms() * 100.0,
                    resid.max() * 100.0
                );
            }
        }
    }
    println!("\n  tip/EB nearer 1.0 = better global stiffness. If the ersatz→exact");
    println!("  gap is large here and small on the solid cases, the cut-cell work");
    println!("  pays for FDM parts specifically — which is the whole question.");
}
