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

// ------------------------------------------- case Bâ€´: does submodeling work now?

/// **Voxel submodeling, re-evaluated.** 7.6 rejected it on the grounds that a
/// submodel buys exactly one thing â€” smaller `h` at the hot spot â€” and smaller
/// `h` made the displayed peak WORSE. With `recover_surface` in the read-back
/// that premise is gone: the recovered peak converges (âˆ’31.3% â†’ âˆ’13.6% â†’ âˆ’3.2%
/// â†’ +2.1% at 5/10/20/40 cells per hole radius). So the mechanism is live again
/// and the question is whether it survives contact with a real submodel.
///
/// The interesting comparison is NOT 20 â†’ 40 cells per radius (one point of Kt
/// for 12Ã— the runtime). It is **5 â†’ 20**, which is 28 points, and which is
/// where real parts sit: a 3 mm fillet meshed at h=0.5 has six cells across it,
/// and refining a whole bracket to fix that is usually infeasible.
///
/// The risk this test exists to measure: submodeling assumes the coarse GLOBAL
/// displacement field is trustworthy at the box boundary even though its local
/// peak is not. That is usually safe â€” displacements converge far better than
/// peak stress â€” but at 5 cells per radius the hole's local COMPLIANCE is also
/// wrong, and that error is in the very field being interpolated. If the
/// submodel inherits the coarse answer, the idea is dead for good; if it lands
/// near the global-fine answer, it works.
///
/// Reads: `sub` should approach `global fine`, not `global coarse`.
#[test]
#[ignore]
fn surf_kirsch_submodel() {
    use std::f64::consts::PI;
    use std::time::Instant;
    println!("\n=== CASE B triple-prime: VOXEL SUBMODELING vs global refinement ===");
    println!("  Same Kirsch plate (hw=50 t=8 a=5, sigma_inf=1). A box of +-3a around");
    println!("  the hole is re-voxelized fine and driven by displacements");
    println!("  interpolated from the COARSE global solve. The box's +-x/+-y faces");
    println!("  are the artificial cuts; its z faces are the plate's own free");
    println!("  surfaces and are left free.");

    let (hw, t, a) = (50.0f64, 8.0f64, 5.0f64);
    let sigma_inf = 1.0f64;
    let inside = move |p: [f64; 3]| -> bool {
        p[0] >= -hw
            && p[0] <= hw
            && p[1] >= -hw
            && p[1] <= hw
            && p[2] >= 0.0
            && p[2] <= t
            && (p[0] * p[0] + p[1] * p[1]) >= a * a
    };
    let f_total = sigma_inf * (2.0 * hw) * t;
    let bw = 3.0 * a; // half-width of the submodel box

    // Peak sigma1 on the rim + traction residual, exactly as 7.6 reads them.
    let read_rim = |pg: &VoxelGrid, u: &[f32], mode: Recovery, h: f64| -> (f64, f64, f64) {
        let probe = SurfaceStress::new(pg, u, mode);
        let mut kt = 0.0f64;
        let mut resid = ErrBins::new();
        let n_theta = (720.0 / h.min(1.0)) as usize;
        let rr = a + 0.30 * h;
        for it in 0..n_theta {
            let th = 2.0 * PI * it as f64 / n_theta as f64;
            let p = [rr * th.cos(), rr * th.sin(), t / 2.0];
            let n = [-th.cos(), -th.sin(), 0.0];
            let Some(sg) = probe.sigma_on_free_surface(p, n) else { continue };
            kt = kt.max(principal_max(&sg) / sigma_inf);
            resid.add(axis_angle_deg(n), norm3(traction(&sg, n)) / (3.0 * sigma_inf));
        }
        (kt, resid.rms() * 100.0, resid.max() * 100.0)
    };

    // Trilinear sample of a padded nodal displacement field, skipping inactive
    // nodes â€” the same weighting the stress sampler uses.
    let sample_u = |pg: &VoxelGrid, u: &[f32], act: &[bool], p: [f64; 3]| -> Option<[f64; 3]> {
        let (mx, my) = (pg.nx + 1, pg.ny + 1);
        let mz = pg.nz + 1;
        let f = [
            (p[0] - pg.origin[0]) / pg.h,
            (p[1] - pg.origin[1]) / pg.h,
            (p[2] - pg.origin[2]) / pg.h,
        ];
        let base = [
            (f[0].floor() as i64).clamp(0, mx as i64 - 2) as usize,
            (f[1].floor() as i64).clamp(0, my as i64 - 2) as usize,
            (f[2].floor() as i64).clamp(0, mz as i64 - 2) as usize,
        ];
        let tt = [
            (f[0] - base[0] as f64).clamp(0.0, 1.0),
            (f[1] - base[1] as f64).clamp(0.0, 1.0),
            (f[2] - base[2] as f64).clamp(0.0, 1.0),
        ];
        let (mut acc, mut wsum) = ([0f64; 3], 0f64);
        for oz in 0..2 {
            for oy in 0..2 {
                for ox in 0..2 {
                    let n = ((base[2] + oz) * my + (base[1] + oy)) * mx + (base[0] + ox);
                    let w = (if ox == 1 { tt[0] } else { 1.0 - tt[0] })
                        * (if oy == 1 { tt[1] } else { 1.0 - tt[1] })
                        * (if oz == 1 { tt[2] } else { 1.0 - tt[2] });
                    if w <= 0.0 || !act[n] {
                        continue;
                    }
                    for c in 0..3 {
                        acc[c] += w * u[3 * n + c] as f64;
                    }
                    wsum += w;
                }
            }
        }
        if wsum <= 1e-12 {
            return None;
        }
        Some([acc[0] / wsum, acc[1] / wsum, acc[2] / wsum])
    };

    println!(
        "\n  {:<26} {:>9} {:>10} {:>9} {:>10} {:>10} {:>9}",
        "solve", "cells/a", "cells", "Kt(s1)", "err vs 3", "residRMS", "time s"
    );

    // Global solves at both ends. The coarse ones drive every submodel.
    let mut globals: Vec<(f64, VoxelGrid, Vec<f32>)> = Vec::new();
    for &h in &[1.0f64, 0.5, 0.25] {
        let t0 = Instant::now();
        let (pg, u, _tl) =
            rotated_plate(&inside, [-hw, -hw, 0.0], [hw, hw, t], 0.0, h, f_total, false);
        let secs = t0.elapsed().as_secs_f64();
        let (kt, rrms, _rmax) = read_rim(&pg, &u, Recovery::CutSurface, h);
        println!(
            "  {:<26} {:>9.0} {:>10} {kt:>9.3} {:>+9.1}% {:>9.1}% {secs:>9.0}",
            format!("global h={h}"),
            a / h,
            pg.cell_count(),
            (kt - 3.0) / 3.0 * 100.0,
            rrms
        );
        globals.push((h, pg, u));
    }

    // Submodels: box of +-3a re-voxelized fine, Dirichlet-driven by a coarse global.
    for &(h_g, h_s) in &[(1.0f64, 0.5f64), (1.0, 0.25), (0.5, 0.25)] {
        let (_, pg_g, u_g) = globals.iter().find(|(h, _, _)| *h == h_g).unwrap();
        let act_g = active_nodes(pg_g);

        let t0 = Instant::now();
        let inside_sub =
            |p: [f64; 3]| -> bool { inside(p) && p[0].abs() <= bw && p[1].abs() <= bw };
        let grid = voxelize_production(&inside_sub, [-bw, -bw, 0.0], [bw, bw, t], h_s, 6, 1);
        let settings =
            SolveSettings { e0: E0, nu: NU, tol: 1e-7, max_iter: 1200, ..Default::default() };
        let (pg, levels) = pad_for_levels(&grid, settings.max_levels);
        let (mx, my, mz) = (pg.nx + 1, pg.ny + 1, pg.nz + 1);
        let act = active_nodes(&pg);
        let npos = |n: usize| -> [f64; 3] {
            let (x, y, z) = (n % mx, (n / mx) % my, n / (mx * my));
            [
                pg.origin[0] + x as f64 * pg.h,
                pg.origin[1] + y as f64 * pg.h,
                pg.origin[2] + z as f64 * pg.h,
            ]
        };
        // Penalty springs + equivalent forces on the four ARTIFICIAL faces only.
        let k = 300.0 * E0 * pg.h; // attach.rs SPRING_FACTOR
        let mut np = NodeProblem::default();
        let mut driven = 0usize;
        for n in 0..mx * my * mz {
            if !act[n] {
                continue;
            }
            let p = npos(n);
            let on_cut = (p[0].abs() - bw).abs() <= 0.5 * pg.h
                || (p[1].abs() - bw).abs() <= 0.5 * pg.h;
            if !on_cut {
                continue;
            }
            let Some(ud) = sample_u(pg_g, u_g, &act_g, p) else { continue };
            driven += 1;
            for d in 0..3 {
                let mut dir = [0f64; 3];
                dir[d] = 1.0;
                np.springs.push((n as u32, dir, k));
                if ud[d] != 0.0 {
                    let mut f = [0f64; 3];
                    f[d] = k * ud[d];
                    np.forces.push((n as u32, f));
                }
            }
        }
        assert!(driven > 0, "no driven nodes on the submodel cut faces");
        let u = solve_with_optional_cut(&pg, levels, &np, &settings, &inside_sub, false);
        let secs = t0.elapsed().as_secs_f64();
        let (kt, rrms, _rmax) = read_rim(&pg, &u, Recovery::CutSurface, h_s);
        println!(
            "  {:<26} {:>9.0} {:>10} {kt:>9.3} {:>+9.1}% {:>9.1}% {secs:>9.0}",
            format!("sub h={h_s} from h={h_g}"),
            a / h_s,
            pg.cell_count(),
            (kt - 3.0) / 3.0 * 100.0,
            rrms
        );
    }

    println!("\n  DECISION: compare each `sub h=X from h=Y` row against `global h=X`.");
    println!("  Close to it => submodeling recovers the fine answer at a fraction of");
    println!("  the cost. Close to `global h=Y` instead => it inherited the coarse");
    println!("  solve's defect and the idea stays dead.");
}


// ------------------------------------ case Bâ—: how big does the submodel box need to be?

/// 7.9 measured ONE box half-width (3a) driven by ONE global (5 cells/radius).
/// Both were the friendly choice, and the whole result rests on the hole's
/// perturbation having decayed by the time it reaches the artificial cut â€”
/// Kirsch falls off like `(a/r)Â²`, so 3a leaves ~11% of it at the boundary.
/// This sweeps both knobs to find where that stops being true.
///
/// Reads: every row should approach `global h=0.25` (âˆ’3.2%). A row that drifts
/// back toward its DRIVER's error has a box too small or a driver too coarse to
/// carry usable displacement data.
#[test]
#[ignore]
fn surf_kirsch_submodel_box_sweep() {
    use std::f64::consts::PI;
    println!("\n=== CASE B4: submodel box size and driver coarseness ===");
    println!("  Same plate. Box half-width in units of the hole radius a, driven");
    println!("  from globals of increasing coarseness. Target: the global h=0.25");
    println!("  answer, -3.2%. h=2 is 2.5 cells per radius â€” a deliberately");
    println!("  pathological driver, coarser than anything 7.9 tried.");

    let (hw, t, a) = (50.0f64, 8.0f64, 5.0f64);
    let sigma_inf = 1.0f64;
    let inside = move |p: [f64; 3]| -> bool {
        p[0] >= -hw
            && p[0] <= hw
            && p[1] >= -hw
            && p[1] <= hw
            && p[2] >= 0.0
            && p[2] <= t
            && (p[0] * p[0] + p[1] * p[1]) >= a * a
    };
    let f_total = sigma_inf * (2.0 * hw) * t;
    let h_s = 0.25f64;

    let read_kt = |pg: &VoxelGrid, u: &[f32], h: f64| -> f64 {
        let probe = SurfaceStress::new(pg, u, Recovery::CutSurface);
        let mut kt = 0.0f64;
        let n_theta = (720.0 / h.min(1.0)) as usize;
        let rr = a + 0.30 * h;
        for it in 0..n_theta {
            let th = 2.0 * PI * it as f64 / n_theta as f64;
            let p = [rr * th.cos(), rr * th.sin(), t / 2.0];
            let n = [-th.cos(), -th.sin(), 0.0];
            let Some(sg) = probe.sigma_on_free_surface(p, n) else { continue };
            kt = kt.max(principal_max(&sg) / sigma_inf);
        }
        kt
    };
    let sample_u = |pg: &VoxelGrid, u: &[f32], act: &[bool], p: [f64; 3]| -> Option<[f64; 3]> {
        let (mx, my) = (pg.nx + 1, pg.ny + 1);
        let mz = pg.nz + 1;
        let f = [
            (p[0] - pg.origin[0]) / pg.h,
            (p[1] - pg.origin[1]) / pg.h,
            (p[2] - pg.origin[2]) / pg.h,
        ];
        let base = [
            (f[0].floor() as i64).clamp(0, mx as i64 - 2) as usize,
            (f[1].floor() as i64).clamp(0, my as i64 - 2) as usize,
            (f[2].floor() as i64).clamp(0, mz as i64 - 2) as usize,
        ];
        let tt = [
            (f[0] - base[0] as f64).clamp(0.0, 1.0),
            (f[1] - base[1] as f64).clamp(0.0, 1.0),
            (f[2] - base[2] as f64).clamp(0.0, 1.0),
        ];
        let (mut acc, mut wsum) = ([0f64; 3], 0f64);
        for oz in 0..2 {
            for oy in 0..2 {
                for ox in 0..2 {
                    let n = ((base[2] + oz) * my + (base[1] + oy)) * mx + (base[0] + ox);
                    let w = (if ox == 1 { tt[0] } else { 1.0 - tt[0] })
                        * (if oy == 1 { tt[1] } else { 1.0 - tt[1] })
                        * (if oz == 1 { tt[2] } else { 1.0 - tt[2] });
                    if w <= 0.0 || !act[n] {
                        continue;
                    }
                    for c in 0..3 {
                        acc[c] += w * u[3 * n + c] as f64;
                    }
                    wsum += w;
                }
            }
        }
        if wsum <= 1e-12 {
            return None;
        }
        Some([acc[0] / wsum, acc[1] / wsum, acc[2] / wsum])
    };

    // Drivers, coarse to pathological.
    let mut drivers: Vec<(f64, VoxelGrid, Vec<f32>)> = Vec::new();
    for &h in &[0.5f64, 1.0, 2.0] {
        let (pg, u, _tl) =
            rotated_plate(&inside, [-hw, -hw, 0.0], [hw, hw, t], 0.0, h, f_total, false);
        let kt = read_kt(&pg, &u, h);
        println!(
            "\n  driver global h={h} ({:.1} cells/a): Kt {kt:.3} ({:+.1}%) on its own",
            a / h,
            (kt - 3.0) / 3.0 * 100.0
        );
        drivers.push((h, pg, u));
    }

    println!(
        "\n  {:<10} {:>9} {:>10} | {:>22} {:>22} {:>22}",
        "box", "cells", "cells/a", "from h=0.5", "from h=1", "from h=2"
    );

    for &bwa in &[1.25f64, 1.5, 2.0, 3.0, 4.0] {
        let bw = bwa * a;
        let mut cells_reported = 0usize;
        let mut cols: Vec<String> = Vec::new();
        for (h_g, pg_g, u_g) in &drivers {
            let act_g = active_nodes(pg_g);
            let inside_sub =
                |p: [f64; 3]| -> bool { inside(p) && p[0].abs() <= bw && p[1].abs() <= bw };
            let grid = voxelize_production(&inside_sub, [-bw, -bw, 0.0], [bw, bw, t], h_s, 6, 1);
            let settings = SolveSettings {
                e0: E0,
                nu: NU,
                tol: 1e-7,
                max_iter: 1200,
                ..Default::default()
            };
            let (pg, levels) = pad_for_levels(&grid, settings.max_levels);
            let (mx, my, mz) = (pg.nx + 1, pg.ny + 1, pg.nz + 1);
            let act = active_nodes(&pg);
            let npos = |n: usize| -> [f64; 3] {
                let (x, y, z) = (n % mx, (n / mx) % my, n / (mx * my));
                [
                    pg.origin[0] + x as f64 * pg.h,
                    pg.origin[1] + y as f64 * pg.h,
                    pg.origin[2] + z as f64 * pg.h,
                ]
            };
            let k = 300.0 * E0 * pg.h;
            let mut np = NodeProblem::default();
            for n in 0..mx * my * mz {
                if !act[n] {
                    continue;
                }
                let p = npos(n);
                if (p[0].abs() - bw).abs() > 0.5 * pg.h && (p[1].abs() - bw).abs() > 0.5 * pg.h {
                    continue;
                }
                let Some(ud) = sample_u(pg_g, u_g, &act_g, p) else { continue };
                for d in 0..3 {
                    let mut dir = [0f64; 3];
                    dir[d] = 1.0;
                    np.springs.push((n as u32, dir, k));
                    if ud[d] != 0.0 {
                        let mut f = [0f64; 3];
                        f[d] = k * ud[d];
                        np.forces.push((n as u32, f));
                    }
                }
            }
            let u = solve_with_optional_cut(&pg, levels, &np, &settings, &inside_sub, false);
            let kt = read_kt(&pg, &u, h_s);
            cells_reported = pg.cell_count();
            cols.push(format!("{kt:>9.3} ({:>+6.1}%)", (kt - 3.0) / 3.0 * 100.0));
            let _ = h_g;
        }
        println!(
            "  {:<10} {cells_reported:>9} {:>10.0} | {} {} {}",
            format!("+-{bwa}a"),
            a / h_s,
            cols[0],
            cols[1],
            cols[2]
        );
    }

    println!("\n  Reference: global h=0.25 reads 2.904 (-3.2%) at 8.31M cells.");
    println!("  A row that drifts back toward its driver's own error means the box");
    println!("  is inside the zone the hole still perturbs, or the driver is too");
    println!("  coarse to carry usable displacements across the cut.");
}

// -------------------------------------------- case E: the FAR field, where F1 lives

/// **The one claim F1 was built for, and the one this tier never measured.**
/// `decouple_traction` was written for a far-field over-read: a cell cut
/// PERPENDICULAR to the stress is a soft link in series, its ersatz stress is
/// already the material stress, and the scalar `material_factor` divides by the
/// occupancy a second time. Every other case in this file samples a stress
/// CONCENTRATION, where 7.6 shows F1 changing the peak by nothing at all.
///
/// The probe: a bar in pure uniaxial tension with FRICTIONLESS ends. The exact
/// solution is `Ïƒxx = Ïƒâˆž` everywhere â€” including inside the boundary cell column
/// at `x=0`, which is cut perpendicular to the stress whenever the face lands
/// mid-cell. So the reference needs no St-Venant argument and no analytic
/// series: it is a constant, and any deviation is read-back error.
///
/// `h` is swept over values whose `L/h` have different fractional parts, so the
/// face lands at a range of sub-cell positions â€” the "5 times in 6" the
/// production voxelizer produces by centring the grid.
///
/// Reads: `col mean` is the boundary column, `col cut` the same column with F1.
/// If F1 is doing what it was built for, `col cut` sits near 0% while
/// `col mean` spikes wherever the column occupancy is low.
#[test]
#[ignore]
fn far_field_cut_perpendicular_to_stress() {
    println!("\n=== CASE E: FAR-FIELD cut column â€” the F1 claim ===");
    println!("  Bar 40x10x4, pure tension sigma_inf=1 MPa, frictionless ends.");
    println!("  Exact answer: sigma_xx = 1.000 MPa EVERYWHERE, so the boundary");
    println!("  column at x=0 has a constant reference and any error is read-back.");
    println!("  occ = mean occupancy of that column; low occ = face landed mid-cell.");

    let (l, w, t) = (40.0f64, 10.0f64, 4.0f64);
    let sigma_inf = 1.0f64;
    let inside = move |p: [f64; 3]| -> bool {
        p[0] >= 0.0 && p[0] <= l && p[1].abs() <= 0.5 * w && p[2] >= 0.0 && p[2] <= t
    };
    let f_total = sigma_inf * w * t;

    println!(
        "\n  {:>6} {:>7} {:>8} | {:>11} {:>11} | {:>11} {:>11}",
        "h", "L/h", "occ", "col mean", "col cut", "mid mean", "mid cut"
    );

    for &h in &[1.0f64, 0.9, 0.8, 0.7, 0.6, 0.55, 0.5, 0.45, 0.4, 0.35, 0.3] {
        let grid = voxelize_production(&inside, [0.0, -0.5 * w, 0.0], [l, 0.5 * w, t], h, 6, 1);
        let settings =
            SolveSettings { e0: E0, nu: NU, tol: 1e-8, max_iter: 2000, ..Default::default() };
        let (pg, levels) = pad_for_levels(&grid, settings.max_levels);
        let (mx, my, mz) = (pg.nx + 1, pg.ny + 1, pg.nz + 1);
        let act = active_nodes(&pg);
        let npos = |n: usize| -> [f64; 3] {
            let (x, y, z) = (n % mx, (n / mx) % my, n / (mx * my));
            [
                pg.origin[0] + x as f64 * pg.h,
                pg.origin[1] + y as f64 * pg.h,
                pg.origin[2] + z as f64 * pg.h,
            ]
        };
        let k = 300.0 * E0 * pg.h;
        let mut np = NodeProblem::default();
        let mut load_nodes = Vec::new();
        // x = 0 is a roller (symmetry) plane: u_x = 0, y and z free so Poisson
        // contraction is unimpeded and the state stays exactly uniaxial.
        let (mut anchor_yz, mut anchor_z) = (usize::MAX, usize::MAX);
        let (mut best_a, mut best_b) = (f64::MAX, f64::MAX);
        for n in 0..mx * my * mz {
            if !act[n] {
                continue;
            }
            let p = npos(n);
            if p[0] <= 0.5 * pg.h {
                np.springs.push((n as u32, [1.0, 0.0, 0.0], k));
                // Two more anchors kill lateral translation and roll about x
                // without touching the axial field.
                let da = p[1].abs() + (p[2] - 0.5 * t).abs();
                if da < best_a {
                    best_a = da;
                    anchor_yz = n;
                }
                let db = (p[1] - 0.5 * w).abs() + (p[2] - 0.5 * t).abs();
                if db < best_b {
                    best_b = db;
                    anchor_z = n;
                }
            } else if p[0] >= l - 0.5 * pg.h {
                load_nodes.push(n);
            }
        }
        assert!(anchor_yz != usize::MAX && !load_nodes.is_empty(), "bar BCs at h={h}");
        np.springs.push((anchor_yz as u32, [0.0, 1.0, 0.0], k));
        np.springs.push((anchor_yz as u32, [0.0, 0.0, 1.0], k));
        if anchor_z != anchor_yz && anchor_z != usize::MAX {
            np.springs.push((anchor_z as u32, [0.0, 0.0, 1.0], k));
        }
        let inv = 1.0 / load_nodes.len() as f64;
        for n in load_nodes {
            np.forces.push((n as u32, [f_total * inv, 0.0, 0.0]));
        }
        let u = solve_nodes(&pg, levels, &np, &settings).expect("bar solve").u;

        // Raw cell fields: production (scalar material_factor) vs F1.
        let eps = material_factor(&pg, &grid_eps(&pg));
        let cut = cut_normals(&pg);
        let f_mean =
            cell_field_cut(&pg, &u, E0, NU, &eps, None, [0.0; 3], FieldKind::Sxx, None);
        let f_cut = cell_field_cut(
            &pg, &u, E0, NU, &eps, None, [0.0; 3], FieldKind::Sxx, Some(&cut),
        );

        // The first solid column in x, and a far-field column at mid-length.
        let col_of = |x_target: f64| -> usize {
            (((x_target - pg.origin[0]) / pg.h).floor() as i64).clamp(0, pg.nx as i64 - 1) as usize
        };
        let (cx_b, cx_m) = (col_of(0.5 * pg.h), col_of(0.5 * l));
        let mut stat = |cx: usize, f: &[f32]| -> (f64, f64) {
            let (mut s, mut occ, mut cnt) = (0f64, 0f64, 0usize);
            for cz in 0..pg.nz {
                for cy in 0..pg.ny {
                    let ci = (cz * pg.ny + cy) * pg.nx + cx;
                    if pg.scale[ci] <= 0.0 {
                        continue;
                    }
                    // Interior of the cross-section only: the lateral skin is a
                    // cut PARALLEL to the stress, a different (and already
                    // correct) case that would otherwise contaminate the mean.
                    let p = [
                        pg.origin[1] + (cy as f64 + 0.5) * pg.h,
                        pg.origin[2] + (cz as f64 + 0.5) * pg.h,
                    ];
                    if p[0].abs() > 0.5 * w - 1.5 * pg.h
                        || p[1] < 1.5 * pg.h
                        || p[1] > t - 1.5 * pg.h
                    {
                        continue;
                    }
                    s += f[ci] as f64;
                    occ += pg.scale[ci] as f64;
                    cnt += 1;
                }
            }
            if cnt == 0 { (f64::NAN, f64::NAN) } else { (s / cnt as f64, occ / cnt as f64) }
        };
        let (b_mean, occ_b) = stat(cx_b, &f_mean);
        let (b_cut, _) = stat(cx_b, &f_cut);
        let (m_mean, _) = stat(cx_m, &f_mean);
        let (m_cut, _) = stat(cx_m, &f_cut);
        let e = |v: f64| (v - sigma_inf) / sigma_inf * 100.0;
        println!(
            "  {h:>6} {:>7.1} {occ_b:>8.2} | {:>10.1}% {:>10.1}% | {:>10.1}% {:>10.1}%",
            l / h,
            e(b_mean),
            e(b_cut),
            e(m_mean),
            e(m_cut)
        );
    }

    println!("\n  If `col mean` spikes where occ is low and `col cut` does not, F1");
    println!("  does what it was built for and this tier simply never sampled the");
    println!("  place it acts. If both are flat, the defect needs a constrained");
    println!("  face (not a free one) to appear and the probe must be rebuilt.");
}


// ------------------------------- case F: stepped round bar vs an ANSYS solution

/// **The 3-D reference this tier has been missing.** Â§10 says case 2.6 (the
/// shoulder fillet) cannot be used as an accuracy gate until a cross-code check
/// gives it a genuine 3-D reference, because the 2-D Peterson chart is not the
/// right answer for a thick section. This is that reference: a stepped ROUND bar
/// solved in Ansys Mechanical (structural steel, Î½=0.3), three load cases.
///
/// ```text
/// D = 12 (x 0..30), fillet r_f = 1 (x 30..31), d = 6 (x 31..60)
/// fixed support on the x=0 face, load on the x=60 face
///
/// LC              load          max SEQV   max S1
/// 1  axial        1000 N          59.50     65.83
/// 2  bending        50 N          98.16    108.23
/// 3  torsion      1000 NÂ·mm       51.82     29.92
/// ```
///
/// The reference is internally consistent before we compare anything to it:
/// implied Kt = 1.86 / 1.58 / 1.27 against Peterson's ~1.85 / ~1.55-1.60 /
/// ~1.25-1.30 for D/d = 2, r/d = 0.167, and LC3 gives S1/SEQV = 0.5774 = 1/âˆš3
/// exactly, which is pure shear â€” what a torsion fillet root must be.
///
/// STRESS IS INDEPENDENT OF E for a homogeneous isotropic body under any mix of
/// traction and displacement BCs, so running at the harness `E0` rather than
/// steel's 200 GPa costs nothing. Î½ must match, and does (0.3).
///
/// **The hard part is the fillet is TINY** â€” r_f = 1 mm on a 60 mm bar. Cells
/// across the fillet radius are 1/h: just 8 even at h=0.125 and 4.4M cells.
/// Reaching the ~15 cells/radius crossover where the recovered read-back is
/// trustworthy (Â§6b) would need h=0.07, i.e. ~69M cells globally. This case is
/// therefore the realistic illustration of why 7.9's submodel matters: the
/// global mesh cannot resolve the feature that sets the answer.
#[test]
#[ignore]
fn stepped_round_bar_vs_ansys() {
    use std::f64::consts::PI;
    println!("\n=== CASE F: STEPPED ROUND BAR vs ANSYS ===");
    println!("  D=12 (x 0..30), fillet r_f=1 (x 30..31), d=6 (x 31..60).");
    println!("  Fixed at x=0, loaded at x=60. nu=0.3 both codes.");
    println!("  Peak read on the FILLET surface â€” max over the blend, which is");
    println!("  where Ansys puts its Max marker in all three load cases.");

    const XA: f64 = 30.0; // shoulder plane / fillet start
    const XB: f64 = 31.0; // fillet end / small shaft start
    const RF: f64 = 1.0; // fillet radius
    const RBIG: f64 = 6.0;
    const RSML: f64 = 3.0;
    const LEN: f64 = 60.0;

    let inside = |p: [f64; 3]| -> bool {
        let (x, r) = (p[0], (p[1] * p[1] + p[2] * p[2]).sqrt());
        if !(0.0..=LEN).contains(&x) {
            return false;
        }
        if x <= XA {
            r <= RBIG
        } else if x <= XB {
            // Concave blend: material is everything at least RF from the arc
            // centre (XB, RSML+RF), plus the shaft core itself.
            r <= RSML
                || (r <= RSML + RF
                    && (x - XB).powi(2) + (r - (RSML + RF)).powi(2) >= RF * RF)
        } else {
            r <= RSML
        }
    };

    // Nominal stresses on the small section, for the Kt columns.
    let area = PI * RSML * RSML;
    let z_bend = PI * (2.0 * RSML).powi(3) / 32.0;
    let z_pol = PI * (2.0 * RSML).powi(3) / 16.0;
    let nom = [
        1000.0 / area,                  // LC1 axial
        50.0 * (LEN - XB) / z_bend,     // LC2 bending, moment at the fillet root
        1000.0 / z_pol,                 // LC3 torsion (shear)
    ];
    // (name, ansys SEQV, ansys S1)
    let cases = [
        ("LC1 axial 1000N", 59.50f64, 65.83f64),
        ("LC2 bending 50N", 98.16, 108.23),
        ("LC3 torsion 1000Nmm", 51.82, 29.92),
    ];

    println!(
        "\n  {:<22} {:>5} {:>7} {:<14} {:>9} {:>9} {:>9} {:>9} {:>7}",
        "case", "h", "cells/rf", "readback", "SEQV", "vs ansys", "S1", "vs ansys", "Kt(S1)"
    );

    for &h in &[1.0f64, 0.5, 0.25, 0.125] {
        let grid = voxelize_production(
            &inside,
            [0.0, -RBIG, -RBIG],
            [LEN, RBIG, RBIG],
            h,
            6,
            1,
        );
        let settings =
            SolveSettings { e0: E0, nu: NU, tol: 1e-8, max_iter: 3000, ..Default::default() };
        let (pg, levels) = pad_for_levels(&grid, settings.max_levels);
        let (mx, my, mz) = (pg.nx + 1, pg.ny + 1, pg.nz + 1);
        let act = active_nodes(&pg);
        let npos = |n: usize| -> [f64; 3] {
            let (x, y, z) = (n % mx, (n / mx) % my, n / (mx * my));
            [
                pg.origin[0] + x as f64 * pg.h,
                pg.origin[1] + y as f64 * pg.h,
                pg.origin[2] + z as f64 * pg.h,
            ]
        };
        let mut fixed = Vec::new();
        let mut tip: Vec<(usize, [f64; 3])> = Vec::new();
        for n in 0..mx * my * mz {
            if !act[n] {
                continue;
            }
            let p = npos(n);
            if p[0] <= 0.5 * pg.h {
                fixed.push(n as u32);
            } else if p[0] >= LEN - 0.5 * pg.h {
                tip.push((n, p));
            }
        }
        assert!(!fixed.is_empty() && !tip.is_empty(), "bar BCs at h={h}");
        let inv = 1.0 / tip.len() as f64;
        // Torsion: tangential forces f = cÂ·(0,âˆ’z,y) give Mx = cÂ·Î£rÂ², so scaling
        // by c = T/Î£rÂ² lands the resultant torque exactly.
        let sum_r2: f64 = tip.iter().map(|(_, p)| p[1] * p[1] + p[2] * p[2]).sum();

        for (ci, &(name, a_seqv, a_s1)) in cases.iter().enumerate() {
            let mut np = NodeProblem::default();
            np.fixed = fixed.clone();
            for &(n, p) in &tip {
                let f = match ci {
                    0 => [1000.0 * inv, 0.0, 0.0],
                    1 => [0.0, 50.0 * inv, 0.0],
                    _ => {
                        let c = 1000.0 / sum_r2;
                        [0.0, -c * p[2], c * p[1]]
                    }
                };
                np.forces.push((n as u32, f));
            }
            let u = solve_nodes(&pg, levels, &np, &settings).expect("bar solve").u;

            for mode in [Recovery::Mean, Recovery::CutSurface] {
                let probe = SurfaceStress::new(&pg, &u, mode);
                let (mut seqv, mut s1) = (0f64, f64::MIN);
                // Walk the fillet blend: meridian angle alpha over the quarter
                // arc, full circumference.
                let n_a = (60.0 / h.min(1.0)).max(24.0) as usize;
                let n_t = (360.0 / h.min(1.0)) as usize;
                for ia in 0..=n_a {
                    let alpha = PI + 0.5 * PI * ia as f64 / n_a as f64;
                    let (xs, rs) = (XB + alpha.cos() * RF, RSML + RF + alpha.sin() * RF);
                    // Outward normal points at the arc centre (concave blend).
                    let (nx, nr) = (-alpha.cos(), -alpha.sin());
                    for it in 0..n_t {
                        let th = 2.0 * PI * it as f64 / n_t as f64;
                        let (ct, st) = (th.cos(), th.sin());
                        let nn = [nx, nr * ct, nr * st];
                        let p = [
                            xs - 0.30 * h * nn[0],
                            rs * ct - 0.30 * h * nn[1],
                            rs * st - 0.30 * h * nn[2],
                        ];
                        let Some(sg) = probe.sigma_on_free_surface(p, nn) else { continue };
                        seqv = seqv.max(von_mises(&sg));
                        s1 = s1.max(principal_max(&sg));
                    }
                }
                println!(
                    "  {:<22} {h:>5} {:>7.0} {:<14} {seqv:>9.2} {:>+8.1}% {s1:>9.2} {:>+8.1}% {:>7.2}",
                    if mode == Recovery::Mean { name } else { "" },
                    RF / h,
                    mode.label(),
                    (seqv - a_seqv) / a_seqv * 100.0,
                    (s1 - a_s1) / a_s1 * 100.0,
                    s1 / nom[ci]
                );
            }
        }
    }

    println!("\n  Ansys implied Kt(S1): 1.86 axial / 1.58 bending / 1.27 torsion");
    println!("  (Peterson D/d=2 r/d=0.167: ~1.85 / ~1.55-1.60 / ~1.25-1.30).");
    println!("  cells/rf is cells per FILLET radius â€” the resolution that matters.");
    println!("  Per 6b the recovered read-back only becomes trustworthy above ~15,");
    println!("  which no global mesh here reaches: h=0.125 is 8, at 4.4M cells.");
}


// --------------------- case Fâ€²: the Ansys bar, reached by submodeling the fillet

/// Case F converges toward the Ansys answer but cannot arrive: the fillet is
/// `r_f = 1` on a 60 mm bar, so cells per fillet radius are `1/h` and even
/// h=0.125 â€” 4.4M cells â€” buys only 8. Crossing the ~15 of Â§6b globally needs
/// hâ‰ˆ0.06, about 35M cells. This is the realistic version of the problem 7.9
/// solved on a toy: the feature that sets the answer is 1/60th of the part.
///
/// Two rows, doing two different jobs:
///
/// * **validation** â€” `sub h=0.125 from h=0.5` against case F's *global* h=0.125
///   (axial âˆ’10.5%, bending âˆ’14.8%, torsion âˆ’2.5%). Same mesh, different route;
///   they should agree. This is 7.9's check repeated on a geometry with a real
///   3-D reference and three load cases including torsion.
/// * **extension** â€” `sub h=0.0625 from h=0.25` reaches 16 cells per fillet
///   radius, past the crossover, which no global mesh here can afford.
///
/// The box spans `x âˆˆ [27, 35]` â€” about 1.3 small-diameters either side of the
/// blend â€” and is cut only in `x`; the cylindrical surface inside it is the
/// bar's own free surface and stays free. Torsion is the load case that most
/// stresses the Dirichlet interpolation, since the boundary data carries the
/// whole twist across the cut.
#[test]
#[ignore]
fn stepped_round_bar_submodel_vs_ansys() {
    use std::f64::consts::PI;
    println!("\n=== CASE F': the ANSYS bar via a fillet submodel ===");
    println!("  Box x in [27,35], cut in x only. Driver -> submodel, three LCs.");

    const XA: f64 = 30.0;
    const XB: f64 = 31.0;
    const RF: f64 = 1.0;
    const RBIG: f64 = 6.0;
    const RSML: f64 = 3.0;
    const LEN: f64 = 60.0;
    const BX0: f64 = 27.0;
    const BX1: f64 = 35.0;

    let inside = |p: [f64; 3]| -> bool {
        let (x, r) = (p[0], (p[1] * p[1] + p[2] * p[2]).sqrt());
        if !(0.0..=LEN).contains(&x) {
            return false;
        }
        if x <= XA {
            r <= RBIG
        } else if x <= XB {
            r <= RSML
                || (r <= RSML + RF
                    && (x - XB).powi(2) + (r - (RSML + RF)).powi(2) >= RF * RF)
        } else {
            r <= RSML
        }
    };
    let cases = [
        ("LC1 axial 1000N", 59.50f64, 65.83f64),
        ("LC2 bending 50N", 98.16, 108.23),
        ("LC3 torsion 1000Nmm", 51.82, 29.92),
    ];

    // Peak SEQV / S1 over the fillet blend, as case F reads it.
    let read_fillet = |pg: &VoxelGrid, u: &[f32], mode: Recovery, h: f64| -> (f64, f64) {
        let probe = SurfaceStress::new(pg, u, mode);
        let (mut seqv, mut s1) = (0f64, f64::MIN);
        let n_a = (60.0 / h.min(1.0)).max(24.0) as usize;
        let n_t = (360.0 / h.min(1.0)) as usize;
        for ia in 0..=n_a {
            let alpha = PI + 0.5 * PI * ia as f64 / n_a as f64;
            let (xs, rs) = (XB + alpha.cos() * RF, RSML + RF + alpha.sin() * RF);
            let (nx, nr) = (-alpha.cos(), -alpha.sin());
            for it in 0..n_t {
                let th = 2.0 * PI * it as f64 / n_t as f64;
                let (ct, st) = (th.cos(), th.sin());
                let nn = [nx, nr * ct, nr * st];
                let p = [
                    xs - 0.30 * h * nn[0],
                    rs * ct - 0.30 * h * nn[1],
                    rs * st - 0.30 * h * nn[2],
                ];
                let Some(sg) = probe.sigma_on_free_surface(p, nn) else { continue };
                seqv = seqv.max(von_mises(&sg));
                s1 = s1.max(principal_max(&sg));
            }
        }
        (seqv, s1)
    };
    let sample_u = |pg: &VoxelGrid, u: &[f32], act: &[bool], p: [f64; 3]| -> Option<[f64; 3]> {
        let (mx, my) = (pg.nx + 1, pg.ny + 1);
        let mz = pg.nz + 1;
        let f = [
            (p[0] - pg.origin[0]) / pg.h,
            (p[1] - pg.origin[1]) / pg.h,
            (p[2] - pg.origin[2]) / pg.h,
        ];
        let base = [
            (f[0].floor() as i64).clamp(0, mx as i64 - 2) as usize,
            (f[1].floor() as i64).clamp(0, my as i64 - 2) as usize,
            (f[2].floor() as i64).clamp(0, mz as i64 - 2) as usize,
        ];
        let tt = [
            (f[0] - base[0] as f64).clamp(0.0, 1.0),
            (f[1] - base[1] as f64).clamp(0.0, 1.0),
            (f[2] - base[2] as f64).clamp(0.0, 1.0),
        ];
        let (mut acc, mut wsum) = ([0f64; 3], 0f64);
        for oz in 0..2 {
            for oy in 0..2 {
                for ox in 0..2 {
                    let n = ((base[2] + oz) * my + (base[1] + oy)) * mx + (base[0] + ox);
                    let w = (if ox == 1 { tt[0] } else { 1.0 - tt[0] })
                        * (if oy == 1 { tt[1] } else { 1.0 - tt[1] })
                        * (if oz == 1 { tt[2] } else { 1.0 - tt[2] });
                    if w <= 0.0 || !act[n] {
                        continue;
                    }
                    for c in 0..3 {
                        acc[c] += w * u[3 * n + c] as f64;
                    }
                    wsum += w;
                }
            }
        }
        if wsum <= 1e-12 {
            return None;
        }
        Some([acc[0] / wsum, acc[1] / wsum, acc[2] / wsum])
    };

    println!(
        "\n  {:<22} {:<22} {:>8} {:>10} {:>9} {:>9} {:>9} {:>9}",
        "case", "route", "cells/rf", "cells", "SEQV", "vs ansys", "S1", "vs ansys"
    );

    for &(h_g, h_s) in &[(0.5f64, 0.125f64), (0.25, 0.0625)] {
        // Driver: the whole bar, one solve per load case.
        let grid =
            voxelize_production(&inside, [0.0, -RBIG, -RBIG], [LEN, RBIG, RBIG], h_g, 6, 1);
        let settings =
            SolveSettings { e0: E0, nu: NU, tol: 1e-8, max_iter: 3000, ..Default::default() };
        let (pg_g, lv_g) = pad_for_levels(&grid, settings.max_levels);
        let (gx, gy, gz) = (pg_g.nx + 1, pg_g.ny + 1, pg_g.nz + 1);
        let act_g = active_nodes(&pg_g);
        let gpos = |n: usize| -> [f64; 3] {
            let (x, y, z) = (n % gx, (n / gx) % gy, n / (gx * gy));
            [
                pg_g.origin[0] + x as f64 * pg_g.h,
                pg_g.origin[1] + y as f64 * pg_g.h,
                pg_g.origin[2] + z as f64 * pg_g.h,
            ]
        };
        let (mut fixed, mut tip) = (Vec::new(), Vec::new());
        for n in 0..gx * gy * gz {
            if !act_g[n] {
                continue;
            }
            let p = gpos(n);
            if p[0] <= 0.5 * pg_g.h {
                fixed.push(n as u32);
            } else if p[0] >= LEN - 0.5 * pg_g.h {
                tip.push((n, p));
            }
        }
        let inv = 1.0 / tip.len() as f64;
        let sum_r2: f64 = tip.iter().map(|(_, p)| p[1] * p[1] + p[2] * p[2]).sum();

        // Submodel grid, shared across load cases (only the RHS changes).
        let inside_sub = |p: [f64; 3]| -> bool { inside(p) && p[0] >= BX0 && p[0] <= BX1 };
        let sgrid = voxelize_production(
            &inside_sub,
            [BX0, -RBIG, -RBIG],
            [BX1, RBIG, RBIG],
            h_s,
            6,
            1,
        );
        let (pg_s, lv_s) = pad_for_levels(&sgrid, settings.max_levels);
        let (sx, sy, sz) = (pg_s.nx + 1, pg_s.ny + 1, pg_s.nz + 1);
        let act_s = active_nodes(&pg_s);
        let spos = |n: usize| -> [f64; 3] {
            let (x, y, z) = (n % sx, (n / sx) % sy, n / (sx * sy));
            [
                pg_s.origin[0] + x as f64 * pg_s.h,
                pg_s.origin[1] + y as f64 * pg_s.h,
                pg_s.origin[2] + z as f64 * pg_s.h,
            ]
        };
        // The two artificial cut planes; the cylindrical surface stays free.
        let cut_nodes: Vec<usize> = (0..sx * sy * sz)
            .filter(|&n| {
                act_s[n] && {
                    let x = spos(n)[0];
                    (x - BX0).abs() <= 0.5 * pg_s.h || (x - BX1).abs() <= 0.5 * pg_s.h
                }
            })
            .collect();
        assert!(!cut_nodes.is_empty(), "no submodel cut nodes at h={h_s}");
        let k = 300.0 * E0 * pg_s.h;

        for (ci, &(name, a_seqv, a_s1)) in cases.iter().enumerate() {
            let mut np = NodeProblem::default();
            np.fixed = fixed.clone();
            for &(n, p) in &tip {
                let f = match ci {
                    0 => [1000.0 * inv, 0.0, 0.0],
                    1 => [0.0, 50.0 * inv, 0.0],
                    _ => {
                        let c = 1000.0 / sum_r2;
                        [0.0, -c * p[2], c * p[1]]
                    }
                };
                np.forces.push((n as u32, f));
            }
            let u_g = solve_nodes(&pg_g, lv_g, &np, &settings).expect("driver solve").u;
            let (gq, g1) = read_fillet(&pg_g, &u_g, Recovery::Mean, h_g);
            println!(
                "  {name:<22} {:<22} {:>8.0} {:>10} {gq:>9.2} {:>+8.1}% {g1:>9.2} {:>+8.1}%",
                format!("global h={h_g}"),
                RF / h_g,
                pg_g.cell_count(),
                (gq - a_seqv) / a_seqv * 100.0,
                (g1 - a_s1) / a_s1 * 100.0
            );

            let mut nps = NodeProblem::default();
            for &n in &cut_nodes {
                let Some(ud) = sample_u(&pg_g, &u_g, &act_g, spos(n)) else { continue };
                for d in 0..3 {
                    let mut dir = [0f64; 3];
                    dir[d] = 1.0;
                    nps.springs.push((n as u32, dir, k));
                    if ud[d] != 0.0 {
                        let mut f = [0f64; 3];
                        f[d] = k * ud[d];
                        nps.forces.push((n as u32, f));
                    }
                }
            }
            let u_s = solve_nodes(&pg_s, lv_s, &nps, &settings).expect("submodel solve").u;
            for mode in [Recovery::Mean, Recovery::CutSurface] {
                let (sq, s1) = read_fillet(&pg_s, &u_s, mode, h_s);
                println!(
                    "  {:<22} {:<22} {:>8.0} {:>10} {sq:>9.2} {:>+8.1}% {s1:>9.2} {:>+8.1}%",
                    "",
                    format!("sub h={h_s} [{}]", mode.label()),
                    RF / h_s,
                    pg_s.cell_count(),
                    (sq - a_seqv) / a_seqv * 100.0,
                    (s1 - a_s1) / a_s1 * 100.0
                );
            }
        }
    }

    println!("\n  Row 1 of each pair validates: sub h=0.125 should reproduce case F's");
    println!("  GLOBAL h=0.125 (axial -10.5%, bending -14.8%, torsion -2.5%).");
    println!("  Row 2 extends past what a global mesh can afford: 16 cells per");
    println!("  fillet radius, over the crossover, where cut+surf-rec should start");
    println!("  BEATING the production read-back rather than trailing it.");
}

