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
use filasim_core::stress::{cell_field, material_factor, recover_nodal, FieldKind};
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
}

impl Recovery {
    fn label(self) -> &'static str {
        match self {
            Recovery::Mean => "mean (today)",
            Recovery::Spr => "SPR",
            Recovery::SprProjected => "SPR+proj",
        }
    }
}

const RECOVERIES: [Recovery; 3] = [Recovery::Mean, Recovery::Spr, Recovery::SprProjected];

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
        let cellf: Vec<Vec<f32>> =
            kinds.iter().map(|&k| cell_field(grid, u, E0, NU, &eps, k)).collect();
        let nodal = if mode == Recovery::Mean {
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
            "    {:<8} {:<14} {:>10} {:>10} {:>12} {:>12}",
            "rotation", "recovery", "Kt(VM)", "Kt err%", "resid RMS%", "resid MAX%"
        );
        for &deg in &[0.0f64, 15.0, 30.0, 45.0] {
            let phi = deg.to_radians();
            let (pg, u, _tl) =
                rotated_plate(&inside_local, [-hw, -hw, 0.0], [hw, hw, t], phi, h, f_total, false);
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
                    kt = kt.max(von_mises(&sg) / sigma_inf);
                    resid.add(axis_angle_deg(n), norm3(traction(&sg, n)) / (3.0 * sigma_inf));
                }
                println!(
                    "    {:<8} {:<14} {kt:>10.3} {:>+9.1}% {:>11.1}% {:>11.1}%",
                    if mode == RECOVERIES[0] { format!("{deg:.0}°") } else { String::new() },
                    mode.label(),
                    (kt - 3.0) / 3.0 * 100.0,
                    resid.rms() * 100.0,
                    resid.max() * 100.0
                );
            }
        }
    }
    println!("\n  If Kt err% grows sharply from 0° to 45°, the 1-3% figure in the");
    println!("  Verification Manual is an axis-alignment artifact.");
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
                "    {:<8} {:<14} {:>10} {:>10} {:>12} {:>12}",
                "rotation", "recovery", "Kt(VM)", "Kt err%", "resid RMS%", "resid MAX%"
            );
            for &deg in &[0.0f64, 45.0] {
              for &exact_cut in &[false, true] {
                let phi = deg.to_radians();
                let (pg, u, _tl) =
                    rotated_plate(&inside_local, [0.0, -hd, 0.0], [lbar, hd, t], phi, h, f_total, exact_cut);
                let (c, s) = (phi.cos(), phi.sin());
                for mode in [Recovery::Mean] {
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
                        kt = kt.max(von_mises(&sg) / sigma_nom);
                        resid
                            .add(axis_angle_deg(n), norm3(traction(&sg, n)) / (kt_ref * sigma_nom));
                    }
                    println!(
                        "    {:<8} {:<14} {kt:>10.3} {:>+9.1}% {:>11.1}% {:>11.1}%",
                        format!("{deg:.0}°"),
                        if exact_cut { "EXACT cut KE" } else { "ersatz (today)" },
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
