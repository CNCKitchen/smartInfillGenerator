// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! WASM API for the filaSim web app.
//!
//! One `Model` instance lives in a web worker and owns mesh, segmentation,
//! voxel grid, boundary conditions, and the last solution. Bulk data crosses
//! the boundary as typed arrays; small results as JSON strings.

use filasim_core::attach::{assemble, check_problem, BcKind, BcSpec, BodyLoad};
use filasim_core::bins::{extract_iso, extract_region_smooth, taubin_smooth, RegionMesh};
use filasim_core::mesh::TriMesh;
use filasim_core::pipeline::{smooth_regions, solid_keep_bins};
use filasim_core::segment::{body_count, segment, Segmentation};
use filasim_core::simp::OptimizeParams;
use filasim_core::solve::{
    active_nodes, pad_for_levels, solve_nodes_cached, SolveSettings, Solution, SolverCache,
};
use filasim_core::stress::{cell_field_eigen, material_factor, recover_nodal, FieldKind};
use filasim_core::threemf::{export_orca_3mf, export_stl_zip, import_3mf, weld};
use filasim_core::voxel::VoxelGrid;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn console_warn(s: &str);
}

/// Threaded builds expose `initThreadPool(n)` on the JS side; the worker
/// must await it before constructing a Model. Plain builds don't have it.
#[cfg(feature = "parallel")]
pub use wasm_bindgen_rayon::init_thread_pool;

/// Display cap for the safety-factor fields (`sf`/`sfm`/`sfz`). Above ~10 the
/// exact number carries no engineering meaning, and a huge cap flattens the
/// color scale's useful 1–5 band into a sliver. Also the sentinel for "cannot
/// fail" cells (compressive σzz in `sfz`).
const SF_CAP: f32 = 10.0;

/// Printable-geometry clamp bands — the single engine-side authority the
/// optimize and solve-printed paths derive the analyzed solid wall from (and
/// the floor for the standalone clamp in `voxel_mesh_cut`). The TS UI mirrors
/// these for display and is allowed to be STRICTER — e.g. it requires at least
/// a 10-point floor↔cap band where the engine only needs 5 for a non-degenerate
/// band. The UI being the tighter side is deliberate: a user can never pick a
/// value the engine then silently widens. Keeping the engine bands here keeps
/// the three call sites from drifting from each other.
const PERIMETERS_RANGE: (u32, u32) = (1, 8);
const LINE_WIDTH_MM: (f64, f64) = (0.1, 1.5);
const WALL_MM: (f64, f64) = (0.2, 5.0);

/// Analyzed solid-wall thickness (mm) from the print settings, clamping once.
/// Returns the clamped perimeter count alongside it.
fn resolve_wall(perimeters: u32, line_width: f64) -> (u32, f64) {
    let p = perimeters.clamp(PERIMETERS_RANGE.0, PERIMETERS_RANGE.1);
    let lw = line_width.clamp(LINE_WIDTH_MM.0, LINE_WIDTH_MM.1);
    (p, (p as f64 * lw).clamp(WALL_MM.0, WALL_MM.1))
}

/// Install the cooperative cancellation flag: an Int32Array over a
/// SharedArrayBuffer whose element 0 the UI thread sets to nonzero while
/// this worker is blocked inside a solve (a postMessage could never arrive
/// mid-call). The MGCG/SIMP loops poll it and bail out with "cancelled".
/// Must be called on the thread that runs the solves (the worker's message
/// loop) — the checker is thread-local.
#[wasm_bindgen]
pub fn set_cancel_flag(flag: js_sys::Int32Array) {
    filasim_core::cancel::set_checker(Some(Box::new(move || flag.get_index(0) != 0)));
}

/// Install the live residual-progress buffer: views over a SharedArrayBuffer
/// the worker writes the MGCG residual trace into while a solve is running, so
/// the UI thread can poll it and redraw the convergence plot live (a
/// postMessage can't arrive while the worker is blocked inside the solve).
/// `count[0]` holds the number of valid residuals; `data[0..count]` are the
/// relative residuals (element 0 = initial). Same thread-local rule as
/// `set_cancel_flag` — call it on the worker's solve thread. Hosts without a
/// SharedArrayBuffer simply never install it; the core's `publish` is then a
/// no-op.
#[wasm_bindgen]
pub fn set_progress_buffer(count: js_sys::Int32Array, data: js_sys::Float32Array) {
    let cap = data.length();
    filasim_core::progress::set_sink(Some(Box::new(move |trace: &[f32]| {
        let n = (trace.len() as u32).min(cap);
        // Copy into a length-matched view: `Float32Array::copy_from` asserts
        // dest.len() == src.len(), so we slice the buffer to exactly `n` (the
        // trace grows each call) instead of handing it the full-capacity view.
        // Write the data first, then publish the count — the reader polls
        // `count` and slices `data[0..count]`, so it must never see a count
        // that runs ahead of the residuals it labels.
        data.subarray(0, n).copy_from(&trace[..n as usize]);
        count.set_index(0, n as i32);
    })));
}

/// Import the raw file bytes to a base triangle soup + object count, dispatching
/// on format: 3MF (zip magic `PK`), else STL. STEP never reaches this path —
/// it tessellates in the JS meshStep worker and enters via `Model::from_mesh`
/// (DESIGN §18; the truck-based in-wasm STEP path was deleted 2026-07-24).
/// Returns (base mesh, object count, per-base-triangle BREP-face id — always
/// None here; `from_mesh` supplies it for STEP).
fn import_any(bytes: &[u8]) -> Result<(TriMesh, usize, Option<Vec<u32>>), JsValue> {
    if bytes.len() >= 2 && &bytes[..2] == b"PK" {
        let (mesh, objects) = import_3mf(bytes).map_err(err)?;
        return Ok((mesh, objects, None));
    }
    Ok((TriMesh::from_stl(bytes).map_err(err)?, 1, None))
}

/// Build a [`Segmentation`] whose patches ARE the BREP faces (one patch per CAD
/// face). `face_of_orig` is indexed by ORIGINAL-mesh triangle, like the dihedral
/// segmentation, so it remaps onto the working mesh the same way. (Not gated on
/// `step`: it only touches `Segmentation`, and keeps `Model::new` compiling when
/// the feature is off, where `cad_face_of_orig` is always None.)
fn cad_segmentation(face_of_orig: &[u32]) -> Segmentation {
    let count = face_of_orig.iter().copied().max().map_or(0, |m| m as usize + 1);
    Segmentation { patch_of_tri: face_of_orig.to_vec(), patch_count: count }
}

struct OptOutput {
    /// Smoothed regions (display + export); regenerated by resmooth_regions.
    regions: Vec<RegionMesh>,
    /// Raw marching-tets regions before smoothing (re-smoothing source).
    regions_raw: Vec<RegionMesh>,
    base_density: f64,
    /// Final per-design-cell binned density.
    cell_density: std::collections::HashMap<u32, f64>,
    /// Continuous (pre-binning) field for the density-threshold isosurface.
    design_cells: Vec<u32>,
    x_cont: Vec<f64>,
    /// Perimeter count the analysis skin assumed — exported as the part-level
    /// wall_loops so the print matches the simulation.
    perimeters: u32,
    /// Top/bottom shell layer count the analysis assumed — exported as the
    /// part-level top/bottom shell layers.
    top_bottom_layers: u32,
    /// Per-modifier sparse_infill_pattern for the export (binary mode's
    /// rectilinear/concentric solid fill); None = profile default.
    solid_pattern: Option<String>,
    summary: String,
    /// SOLID topology result: the shape is one connected body, not modifiers.
    solid: bool,
    /// Frozen load/support cells (solid mode) — anchors for the connected-keep
    /// when the export isosurface threshold is re-tuned.
    anchor_cells: Vec<u32>,
    /// Current export isosurface density (0..1): the level the exported shape /
    /// dense modifier is extracted from the CONTINUOUS field at. User-tunable
    /// after the run (a higher value keeps less material). Default 0.5.
    iso_threshold: f64,
    // ---- project-save snapshot: the inputs needed to re-derive this design's
    // regions + stress eps on reload (and re-verify it), so a `.filasim` file
    // stores only a compact density field, not the heavy region meshes. ----
    /// Bin index per design cell (parallel to `design_cells`).
    bins: Vec<u8>,
    /// Density level per bin (centers[0] = base/floor).
    centers: Vec<f64>,
    /// Binned density per design cell (parallel to `design_cells`).
    x_binned: Vec<f32>,
    /// Skin band geometry the solve assumed (mm) — re-classifies the grid.
    wall_mm: f64,
    tb_mm: f64,
    /// Calibrated E(ρ) law the eps was built with.
    eval_exp: f64,
    eval_coeff: f64,
    /// Region smoothing passes (re-applied on restore).
    smooth_iters: u32,
    binary: bool,
}

/// Options for `Model::optimize`, passed as one JSON object from the worker.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct OptimizeOpts {
    /// Target mean interior infill density in percent.
    budget_pct: f64,
    /// Calibrated infill law E/E0 = coeff·ρ^exponent of the chosen pattern —
    /// used for ALL stiffness evaluation (verification, baselines, stress).
    exponent: f64,
    coeff: f64,
    perimeters: u32,
    line_width: f64,
    smooth_iters: u32,
    n_bins: u32,
    /// Printable density band in percent. Graded default 10–70.
    floor_pct: f64,
    cap_pct: f64,
    /// Manual level override in percent; None/empty = auto placement.
    levels_pct: Option<Vec<f64>>,
    /// Binary (hollow/solid) mode: the OPTIMIZER runs with SIMP penalization
    /// p = 3 so the field converges toward {floor, 1} before quantization;
    /// evaluation still uses the calibrated law (exact at both endpoints).
    binary: bool,
    /// SOLID topology mode (material removal, DESIGN.md #15): no skin, ersatz
    /// void lower bound, linear eval law, output is one optimized shape. The
    /// budget is the retained volume fraction. Takes precedence over `binary`.
    solid: bool,
    /// Solid mode: keep the cells under loads/supports solid (default true).
    /// Off ⇒ pure topology optimization that may carve those regions too.
    retain_bc: bool,
    /// Self-supporting (AM) overhang filter — only used in solid mode.
    self_support: bool,
    /// Overhang angle from horizontal for the self-supporting filter (degrees).
    overhang_deg: f64,
    /// Solid-fill pattern for the export in binary mode.
    solid_pattern: Option<String>,
    /// "budget" = stiffest design at the given mean infill (one pass);
    /// "match" = LIGHTEST design as stiff as a uniform print at budget_pct —
    /// a guarded secant on the budget, each pass warm-started, until the
    /// BINNED design's compliance meets the uniform reference within 2%;
    /// "strength" (DESIGN §17) = LIGHTEST design whose trimmed-percentile
    /// safety factor SF_crit meets `sf_target` on every included load step —
    /// an all-at-cap pre-flight decides feasibility, then the same secant
    /// machinery walks the budget against SF_crit (never accepting below).
    goal: String,
    /// Strength goal: required SF_crit (§17 dec. 1; advisory design aid, not
    /// a certified safety factor — strengths come from preset/measured values
    /// and Gibson–Ashby scaling).
    sf_target: f64,
    /// Strength goal SF measure (§17 dec. 2): "material" (von Mises vs
    /// in-plane strength) | "layer" (§15 adhesion interaction) | "both".
    sf_measure: String,
    /// Planar symmetry constraint: [nx, ny, nz, c] of the plane n·p = c
    /// (world mm). None = unconstrained.
    symmetry: Option<Vec<f64>>,
    /// Solid top/bottom shells: layers × layer height. 0 layers = none.
    top_bottom_layers: u32,
    layer_height: f64,
    /// Minimum member size in mm (printability length scale). Drives the
    /// density-filter radius; 0 = off (numerical floor only). Resolved on the
    /// JS side (defaults to 2× line width when the user leaves it on "auto").
    min_member_mm: f64,
}

impl Default for OptimizeOpts {
    fn default() -> Self {
        Self {
            budget_pct: 25.0,
            exponent: 1.5,
            coeff: 1.0,
            perimeters: 2,
            line_width: 0.45,
            smooth_iters: 15,
            n_bins: 3,
            floor_pct: 10.0,
            cap_pct: 70.0,
            levels_pct: None,
            binary: false,
            solid: false,
            retain_bc: true,
            self_support: false,
            overhang_deg: 45.0,
            solid_pattern: None,
            goal: "budget".into(),
            sf_target: 2.0,
            sf_measure: "both".into(),
            symmetry: None,
            top_bottom_layers: 5,
            layer_height: 0.2,
            min_member_mm: 0.0,
        }
    }
}

/// Options for `Model::solve_printed` — analyze the part AS PRINTED:
/// skin (perimeters × line width) solid, interior at a uniform infill
/// ratio through the calibrated pattern law.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PrintedOpts {
    infill_pct: f64,
    exponent: f64,
    coeff: f64,
    perimeters: u32,
    line_width: f64,
    /// Solid top/bottom shells: layers × layer height. 0 layers = none
    /// (open-top showpieces print sparse right to the surface).
    top_bottom_layers: u32,
    layer_height: f64,
}

impl Default for PrintedOpts {
    fn default() -> Self {
        Self {
            infill_pct: 25.0,
            exponent: 1.5,
            coeff: 1.0,
            perimeters: 2,
            line_width: 0.45,
            top_bottom_layers: 5,
            layer_height: 0.2,
        }
    }
}

/// Options for `Model::modal_analysis` — constrained undamped modal analysis.
/// `num_modes` natural frequencies + mode shapes, force-free, on the supports
/// already in `bcs` (the store binds these to the first load case). `solid`
/// picks the stiffness/mass model: solid reference (E₀, full density) vs the
/// as-printed skin+infill model (same printed params as `PrintedOpts`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ModalOpts {
    num_modes: u32,
    /// true = solid reference (E₀ + full density); false = as-printed.
    solid: bool,
    /// Free-free: run WITHOUT supports (soft-anchored), discarding the 6
    /// rigid-body modes — for an unconstrained part. false = constrained.
    free: bool,
    infill_pct: f64,
    exponent: f64,
    coeff: f64,
    perimeters: u32,
    line_width: f64,
    top_bottom_layers: u32,
    layer_height: f64,
}

impl Default for ModalOpts {
    fn default() -> Self {
        Self {
            num_modes: 6,
            solid: false,
            free: false,
            infill_pct: 25.0,
            exponent: 1.5,
            coeff: 1.0,
            perimeters: 2,
            line_width: 0.45,
            top_bottom_layers: 5,
            layer_height: 0.2,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct BuildSimOpts {
    /// In-plane (XY) shrink fraction applied per layer (negative = shrink).
    shrink: f64,
    /// Through-layer (Z) shrink fraction — transverse isotropy. Defaults to the
    /// in-plane value (isotropic) when omitted.
    shrink_z: Option<f64>,
    /// Which state to leave as the live solution for the deformed view:
    /// "released" (off-bed sprung shape) or "bonded" (held on the bed).
    state: String,
    /// Display exaggeration baked into the live preview hull positions.
    exaggeration: f64,
    /// Material yield stress (MPa). `> 0` enables the elastic–perfectly-plastic
    /// correction so the released warp depends on geometry/infill density;
    /// `0`/omitted falls back to the pure-elastic (density-blind) release.
    yield_strength: f64,
    /// Temperature ladder (all °C; lock+bed+chamber+final required to enable):
    /// splits the shrink between the bonded build (lock → local in-build
    /// steady temperature) and the post-release cooldown (steady → ambient).
    /// Omitted = legacy behavior (full strain while bonded).
    t_lock: Option<f64>,
    t_bed: Option<f64>,
    t_chamber: Option<f64>,
    t_final: Option<f64>,
    /// Bed heat-penetration depth (mm) of the ladder. Default 3.0 mm.
    decay_mm: Option<f64>,
}

impl BuildSimOpts {
    fn ladder(&self) -> Option<filasim_core::buildsim::ThermalLadder> {
        match (self.t_lock, self.t_bed, self.t_chamber, self.t_final) {
            (Some(t_lock), Some(t_bed), Some(t_env), Some(t_final)) => {
                Some(filasim_core::buildsim::ThermalLadder {
                    t_lock,
                    t_bed,
                    t_env,
                    t_final,
                    decay_mm: self.decay_mm.unwrap_or(3.0),
                })
            }
            _ => None,
        }
    }
}

impl Default for BuildSimOpts {
    fn default() -> Self {
        Self {
            shrink: -0.003,
            shrink_z: None,
            state: "released".into(),
            exaggeration: 10.0,
            yield_strength: 0.0,
            t_lock: None,
            t_bed: None,
            t_chamber: None,
            t_final: None,
            decay_mm: None,
        }
    }
}

/// True when all 8 nodes of cell `ci` are active (the cell has been printed).
fn cell_activated(grid: &VoxelGrid, ci: usize, active: &[bool]) -> bool {
    let (nx, ny) = (grid.nx, grid.ny);
    let (cz, cy, cx) = (ci / (nx * ny), (ci / nx) % ny, ci % nx);
    let (mx, my) = (nx + 1, ny + 1);
    for oz in 0..2 {
        for oy in 0..2 {
            for ox in 0..2 {
                if !active[((cz + oz) * my + cy + oy) * mx + cx + ox] {
                    return false;
                }
            }
        }
    }
    true
}

/// Resample a per-cell stiffness field (`fine_eps`, on the finer analysis grid)
/// onto a coarser `coarse` grid, by nearest-cell sampling at each coarse cell's
/// centre. Both grids share the part's world frame (same `origin`), so the
/// centre maps directly to a fine-grid index. A coarse solid cell whose centre
/// falls over fine void (a thin boundary feature) keeps its occupancy stiffness
/// (`grid_eps`) rather than collapsing to zero. Void coarse cells stay 0. Used
/// to carry the optimized infill density into the coarser build-sim grid.
fn resample_eps(fine: &VoxelGrid, fine_eps: &[f32], coarse: &VoxelGrid) -> Vec<f32> {
    let occ = filasim_core::solve::grid_eps(coarse);
    let mut out = vec![0f32; coarse.cell_count()];
    for cz in 0..coarse.nz {
        for cy in 0..coarse.ny {
            for cx in 0..coarse.nx {
                let ci = (cz * coarse.ny + cy) * coarse.nx + cx;
                if occ[ci] <= 0.0 {
                    continue;
                }
                let p = [
                    coarse.origin[0] + (cx as f64 + 0.5) * coarse.h,
                    coarse.origin[1] + (cy as f64 + 0.5) * coarse.h,
                    coarse.origin[2] + (cz as f64 + 0.5) * coarse.h,
                ];
                let fx = ((p[0] - fine.origin[0]) / fine.h).floor();
                let fy = ((p[1] - fine.origin[1]) / fine.h).floor();
                let fz = ((p[2] - fine.origin[2]) / fine.h).floor();
                let sampled = if fx >= 0.0 && fy >= 0.0 && fz >= 0.0 {
                    let (ix, iy, iz) = (fx as usize, fy as usize, fz as usize);
                    if ix < fine.nx && iy < fine.ny && iz < fine.nz {
                        fine_eps[(iz * fine.ny + iy) * fine.nx + ix]
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };
                out[ci] = if sampled > 0.0 { sampled } else { occ[ci] };
            }
        }
    }
    out
}

/// Voxel hull of the ALREADY-ACTIVATED cells (the live build-preview geometry,
/// grows + warps as layers activate). Returns the deformed positions (flat xyz,
/// `exag·u` applied), the per-vertex displacement magnitude NORMALISED to
/// `[0,1]` (for jet coloring), and the raw max |u| in mm (for the legend).
fn deformed_activated_hull(grid: &VoxelGrid, sol: &Solution, exag: f64) -> (Vec<f32>, Vec<f32>, f64) {
    let (tris, _e, _c) = grid.surface_mesh_where(&|ci| cell_activated(grid, ci, &sol.active));
    let mut pos = Vec::with_capacity(tris.len());
    let mut mags = Vec::with_capacity(tris.len() / 3);
    let mut maxu = 0f64;
    for c in tris.chunks_exact(3) {
        let p = [c[0] as f64, c[1] as f64, c[2] as f64];
        let u = sol.sample_displacement(p);
        pos.push((p[0] + exag * u[0]) as f32);
        pos.push((p[1] + exag * u[1]) as f32);
        pos.push((p[2] + exag * u[2]) as f32);
        let mag = (u[0] * u[0] + u[1] * u[1] + u[2] * u[2]).sqrt();
        mags.push(mag as f32);
        maxu = maxu.max(mag);
    }
    let inv = if maxu > 0.0 { 1.0 / maxu } else { 0.0 };
    for m in &mut mags {
        *m = (*m as f64 * inv) as f32;
    }
    (pos, mags, maxu)
}

/// Sample a solution's displacement onto each soup vertex (9 floats/triangle) —
/// the warp mapped onto the REAL mesh. Shared by `vertex_displacements` and the
/// build-sim live preview.
fn map_displacements(mesh: &TriMesh, sol: &Solution) -> Vec<f32> {
    let mut out = Vec::with_capacity(mesh.tris.len() * 9);
    for t in &mesh.tris {
        for v in 0..3 {
            let p = [t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64];
            let u = sol.sample_displacement(p);
            out.extend_from_slice(&[u[0] as f32, u[1] as f32, u[2] as f32]);
        }
    }
    out
}

#[wasm_bindgen]
pub struct Model {
    /// Working mesh: display + segmentation + BC attachment + voxelization.
    /// Subdivided at load so deformed shapes can actually bend (a coarse STL
    /// face has no vertices to carry the curve).
    mesh: TriMesh,
    /// Original tessellation as imported — exported 3MFs carry this one.
    mesh_orig: TriMesh,
    /// Original-triangle index per working-mesh triangle. Segmentation runs
    /// on the original mesh and is mapped through this: subdivision creates
    /// T-junctions along shared edges (neighbors get different n), so
    /// segmenting the subdivided soup would fragment flat surfaces.
    parents: Vec<u32>,
    name: String,
    mesh_objects: usize,
    /// Disconnected solid bodies in the imported mesh (internal cavity shells
    /// and debris excluded — see [`body_count`]). UI warns when > 1: the
    /// solver never joins separate bodies, they only fuse where voxelization
    /// bridges sub-cell gaps.
    bodies: usize,
    seg: Segmentation,
    /// STEP only: BREP-face id per ORIGINAL-mesh triangle. Lets the user switch
    /// surface patches to exact CAD faces (`use_cad_faces`). None for STL/3MF.
    cad_face_of_orig: Option<Vec<u32>>,
    bcs: Vec<BcSpec>,
    /// Registered MULTI-LOAD optimization cases (DESIGN §13): each a snapshot of
    /// `bcs` + its weight + the step's summed acceleration (DESIGN §16 dec. 4 —
    /// the self-weight the optimizer recomputes per iteration). Empty ⇒
    /// single-load optimize (the byte-identical path). The first case is the
    /// primary; the rest are weighted extras. The front end rebuilds this
    /// (clear + add per included step) before optimizing.
    load_cases: Vec<(Vec<BcSpec>, f64, [f64; 3])>,
    settings: SolveSettings,
    /// tonne/mm³
    density: f64,
    /// Tensile strength (MPa) of the solid material — safety-factor plot.
    strength: f64,
    /// Layer-adhesion strength (MPa): tension PERPENDICULAR to the layers
    /// (σzz, build direction Z-up). Drives the conservative SF variants.
    strength_z: f64,
    /// Interlayer SHEAR strength (MPa): sliding ALONG the layer plane
    /// (τ = √(σyz²+σzx²), build direction Z-up). None = no measured value,
    /// fall back to 0.6·strength_z (DESIGN §15). Second axis of the layer
    /// failure criterion next to `strength_z`.
    shear_strength_z: Option<f64>,
    /// Include the shear term in the layer criterion (DESIGN §15 dec. 1).
    /// Off ⇒ the effective shear allowable is infinite, so `sfz`, the
    /// orientation sweep and the preview field all reduce to the pure
    /// tension-across-layers criterion in one place. Display-side derived —
    /// toggling never invalidates the solution.
    layer_shear_on: bool,
    /// Active world acceleration for self-weight + remote masses (mm/s²; DESIGN
    /// §16). The worker sums each load step's active accel entities into this
    /// before a solve; `[0,0,0]` = no inertial load (accel-free byte-identical
    /// path). Replaces the old hard-coded `gravity_on` 1g-down toggle.
    accel: [f64; 3],
    target_cells: u32,
    /// Custom mode: pin the cell size to this exact value (mm) instead of
    /// sizing it from the part volume + target cell count. None = auto (preset).
    fixed_h: Option<f64>,
    /// Snap the voxel size to wall/k (0 = off) so the printed skin is
    /// resolved by an integer number of cell layers.
    snap_wall: f64,
    /// Composite skin: cells the wall band only partially covers carry a
    /// blended (skin-fraction) stiffness instead of rounding the band to
    /// whole cell layers — thin walls stay representable on coarse grids.
    composite_skin: bool,
    /// Smoothed stress display: result fields are recovered to the nodes
    /// (volume-averaged) and sampled at the true surface, instead of painting
    /// each cell's center value flat. Removes the staircase checkerboard.
    /// Display-side only — the solution is untouched.
    smooth_stress: bool,
    /// Material (occupancy-decoupled) stress display. The reported stress and
    /// the SF allowable are evaluated with the cell's MATERIAL density factor
    /// (`eps ÷ occupancy`) instead of the occupancy-scaled `eps`. A finite-cell
    /// cut cell is fully dense material partially covering its cube; scaling its
    /// stress by the geometric occupancy under-reads the true stress and paints
    /// the staircase stripes seen on curved skins. Display-side only; the safety
    /// factor is unchanged (the same factor cancels in allowable / stress).
    material_stress: bool,
    grid: Option<(VoxelGrid, usize)>, // padded grid + level count
    /// Reused solver hierarchy + warm start across solves (self-validating:
    /// falls back to a cold rebuild when grid/material/BCs/topology change).
    solver_cache: Option<SolverCache>,
    solution: Option<Solution>,
    /// Per-cell stiffness factors the CURRENT solution was computed with:
    /// None = plain solid solve (grid.scale), Some = binned-infill field.
    /// Stress evaluation must use the same eps as the solve.
    solution_eps: Option<Vec<f32>>,
    /// The optimized design's stiffness field (`solution_eps` at optimize time),
    /// kept ACROSS load-step changes so `solve_optimized` can re-evaluate the one
    /// design under every load step (DESIGN §13). `solution_eps` is reset by BC
    /// edits (the live solution no longer matches); this isn't — it depends only
    /// on the geometry, so it stays valid until the GRID rebuilds (cleared in
    /// `ensure_grid`) or the skin classification changes (`set_composite_skin`).
    opt_eps: Option<Vec<f32>>,
    /// Finished solutions kept for the Results view's instant result switcher,
    /// keyed by a stable id (e.g. "optimized", "uniform", "solid", "asprinted").
    /// `activate_result` swaps one back into `solution`/`solution_eps`; all
    /// share the current grid, so each entry is just its displacement field +
    /// stress eps. Dropped when the geometry/grid changes (`clear_results`).
    results: std::collections::HashMap<String, StashedResult>,
    opt: Option<OptOutput>,
    /// Both build-sim states from the last `solve_build_sim`, kept so the UI can
    /// flip between "on bed" and "released" without re-running the (expensive)
    /// sequential build. `set_build_state` swaps the chosen one into `solution`.
    /// Cleared whenever the grid/geometry changes (`clear_results`).
    build_bonded: Option<Solution>,
    build_released: Option<Solution>,
    /// Bed-peel reaction field from the last build sim (on-bed state): the
    /// per-bed-node lift / shear, as a 2D grid over the coarse build footprint
    /// so it can be sampled onto the mesh for the "peel risk" result field.
    build_peel: Option<PeelField>,
    /// The coarse build grid + the per-cell eps it was solved with, kept so
    /// build-sim stress fields (residual print stress) can be evaluated on the
    /// SAME grid the build solution lives on. `build_eigen` is the eigenstrain
    /// that was applied — subtracted from the total strain to get residual
    /// stress. Cleared with the other build state.
    build_grid: Option<VoxelGrid>,
    build_eps: Option<Vec<f32>>,
    build_eigen: [f64; 3],
    /// Cumulative orientation transform applied since import (3×3 row-major +
    /// translation). Saved in a project so re-importing the original file +
    /// replaying this one matrix reproduces the exact oriented working mesh
    /// (and thus the load/support triangle indices). Identity at import.
    transform_accum: [f64; 12],
    /// Live orientation sweep (DESIGN §15): compact stress tensors + the
    /// pitch/roll grid, built by `orientation_sweep_begin` and consumed
    /// row-chunk by row-chunk by `orientation_sweep_rows` (so the worker can
    /// post progress between calls). Owns copies — never dangles — but goes
    /// stale with the results it was built from; the frontend runs
    /// begin→rows→end as one operation. Dropped on `transform`.
    sweep: Option<SweepCtx>,
}

/// See [`Model::sweep`]. Pixel layout (n per axis, roll fastest) is reported
/// by `orientation_sweep_begin`'s meta JSON; here only the flat dir list.
struct SweepCtx {
    field: filasim_core::orient::SweepField,
    dirs: Vec<[f32; 3]>,
}

/// One stashed solution for the result switcher: the displacement field and the
/// stress eps it was solved with (None = plain solid solve → grid.scale).
struct StashedResult {
    sol: Solution,
    eps: Option<Vec<f32>>,
}

/// Bed-peel reaction sampled as a 2D field over the build footprint (the coarse
/// build grid's z=0 node plane). `lift`/`shear` are `mx·my` flat arrays of
/// TRACTION in MPa (reaction force ÷ nodal tributary bed area — mesh-independent,
/// unlike the raw nodal force). lift = +Z, the part pulling up = peel; shear =
/// in-plane magnitude. `peel_field`/`peel_map` sample these, ramped to zero over
/// `falloff` mm of height. Uncalibrated — a relative indicator.
struct PeelField {
    mx: usize,
    my: usize,
    origin: [f64; 3],
    h: f64,
    lift: Vec<f32>,
    shear: Vec<f32>,
    falloff: f64,
}

fn err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// Patch ids computed on the original mesh, carried onto the subdivided
/// working mesh (each child inherits its parent triangle's patch).
fn remap_segmentation(orig: &Segmentation, parents: &[u32]) -> Segmentation {
    Segmentation {
        patch_of_tri: parents.iter().map(|&p| orig.patch_of_tri[p as usize]).collect(),
        patch_count: orig.patch_count,
    }
}

// ---- project (.filasim) binary serialization ----

const DESIGN_MAGIC: &[u8; 4] = b"SIGD";

fn put_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_f64(v: &mut Vec<u8>, x: f64) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put_f32s(v: &mut Vec<u8>, xs: &[f32]) {
    put_u32(v, xs.len() as u32);
    for &x in xs {
        v.extend_from_slice(&x.to_le_bytes());
    }
}
fn put_f64s(v: &mut Vec<u8>, xs: &[f64]) {
    put_u32(v, xs.len() as u32);
    for &x in xs {
        v.extend_from_slice(&x.to_le_bytes());
    }
}
fn put_bytes(v: &mut Vec<u8>, xs: &[u8]) {
    put_u32(v, xs.len() as u32);
    v.extend_from_slice(xs);
}

/// Cursor over a little-endian project blob with bounds-checked reads.
struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn need(&self, n: usize) -> Result<(), JsValue> {
        if self.p + n > self.b.len() {
            return Err(err("project file is truncated or corrupt"));
        }
        Ok(())
    }
    fn u32(&mut self) -> Result<u32, JsValue> {
        self.need(4)?;
        let x = u32::from_le_bytes(self.b[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        Ok(x)
    }
    fn byte(&mut self) -> Result<u8, JsValue> {
        self.need(1)?;
        let x = self.b[self.p];
        self.p += 1;
        Ok(x)
    }
    fn f64(&mut self) -> Result<f64, JsValue> {
        self.need(8)?;
        let x = f64::from_le_bytes(self.b[self.p..self.p + 8].try_into().unwrap());
        self.p += 8;
        Ok(x)
    }
    fn f32s(&mut self) -> Result<Vec<f32>, JsValue> {
        let n = self.u32()? as usize;
        self.need(n * 4)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(f32::from_le_bytes(self.b[self.p..self.p + 4].try_into().unwrap()));
            self.p += 4;
        }
        Ok(out)
    }
    fn f64s(&mut self) -> Result<Vec<f64>, JsValue> {
        let n = self.u32()? as usize;
        self.need(n * 8)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(f64::from_le_bytes(self.b[self.p..self.p + 8].try_into().unwrap()));
            self.p += 8;
        }
        Ok(out)
    }
    fn bytes(&mut self) -> Result<Vec<u8>, JsValue> {
        let n = self.u32()? as usize;
        self.need(n)?;
        let out = self.b[self.p..self.p + n].to_vec();
        self.p += n;
        Ok(out)
    }
    fn tag(&mut self, expect: &[u8; 4]) -> Result<(), JsValue> {
        self.need(4)?;
        if &self.b[self.p..self.p + 4] != expect {
            return Err(err("not a valid filaSim project (bad design magic)"));
        }
        self.p += 4;
        Ok(())
    }
}

/// Serialize the compact optimized design (density field + the inputs to
/// re-derive its regions and stress eps). No region meshes — they rebuild.
fn design_blob(opt: &OptOutput) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(DESIGN_MAGIC);
    put_u32(&mut v, 1); // format version
    put_f64(&mut v, opt.base_density);
    put_f64(&mut v, opt.iso_threshold);
    put_f64(&mut v, opt.eval_exp);
    put_f64(&mut v, opt.eval_coeff);
    put_f64(&mut v, opt.wall_mm);
    put_f64(&mut v, opt.tb_mm);
    put_u32(&mut v, opt.perimeters);
    put_u32(&mut v, opt.top_bottom_layers);
    put_u32(&mut v, opt.smooth_iters);
    v.push(opt.solid as u8);
    v.push(opt.binary as u8);
    put_bytes(&mut v, opt.solid_pattern.as_deref().unwrap_or("").as_bytes());
    put_f64s(&mut v, &opt.centers);
    put_f32s(&mut v, &opt.x_binned);
    let xcont: Vec<f32> = opt.x_cont.iter().map(|&x| x as f32).collect();
    put_f32s(&mut v, &xcont);
    put_bytes(&mut v, &opt.bins);
    v
}

/// Read the manifest (project.json) out of a `.filasim` project file.
#[wasm_bindgen]
pub fn project_manifest(bytes: &[u8]) -> Result<String, JsValue> {
    let entries = filasim_core::zip::read_zip(bytes).map_err(|e| err(format!("{e:?}")))?;
    for (name, data) in entries {
        if name == "project.json" {
            return String::from_utf8(data).map_err(err);
        }
    }
    Err(err("not a filaSim project (no project.json)"))
}

/// Extract the embedded original model bytes from a project file.
#[wasm_bindgen]
pub fn project_model(bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
    let entries = filasim_core::zip::read_zip(bytes).map_err(|e| err(format!("{e:?}")))?;
    for (name, data) in entries {
        if name.starts_with("model.") {
            return Ok(data);
        }
    }
    Err(err("project file has no embedded model"))
}

impl Model {
    /// Shared tail of every import path: refine the tessellation, build the
    /// default segmentation, and assemble the Model with its defaults. Called
    /// by `new` (bytes → `import_any`) and `from_mesh` (pre-tessellated STEP
    /// from meshStep, DESIGN §18) — keep it the ONLY place a Model is built so
    /// both paths stay behaviorally identical.
    fn from_import(
        mesh_orig: TriMesh,
        mesh_objects: usize,
        cad_face_of_orig: Option<Vec<u32>>,
        name: &str,
    ) -> Model {
        // Refine the display/analysis tessellation: edges capped at ~1/60 of
        // the diagonal so deflection curves are visible on coarse meshes.
        // Dense meshes pass through unchanged (160k-triangle budget).
        let (mesh, parents) = match mesh_orig.bounds() {
            Some((lo, hi)) => {
                let diag = ((hi[0] - lo[0]).powi(2)
                    + (hi[1] - lo[1]).powi(2)
                    + (hi[2] - lo[2]).powi(2))
                .sqrt();
                let target = diag / 60.0;
                if cad_face_of_orig.is_some() {
                    // STEP (via meshStep + from_mesh): longest-edge bisection
                    // as a safety cap — CAD tessellations can carry long thin
                    // triangles on developable faces; a barycentric split
                    // would shatter those into needles, so split only the
                    // long edge. meshStep meshes usually pass through.
                    mesh_orig.capped_edges(target, 160_000)
                } else {
                    mesh_orig.subdivided_with_parents(target, 160_000)
                }
            }
            None => (mesh_orig.clone(), (0..mesh_orig.len() as u32).collect()),
        };
        // Default surface patches: exact BREP faces for STEP (one-click CAD-face
        // picking), dihedral region-growing (10°) for STL/3MF.
        let seg = match &cad_face_of_orig {
            Some(fot) => remap_segmentation(&cad_segmentation(fot), &parents),
            None => remap_segmentation(&segment(&mesh_orig, 10.0), &parents),
        };
        let bodies = body_count(&mesh_orig);
        Model {
            mesh,
            mesh_orig,
            parents,
            name: name.to_string(),
            mesh_objects,
            bodies,
            seg,
            cad_face_of_orig,
            bcs: Vec::new(),
            load_cases: Vec::new(),
            settings: SolveSettings::default(),
            density: 1.24e-9,  // PLA
            strength: 50.0,    // PLA tensile, MPa
            strength_z: 35.0,  // PLA layer adhesion, MPa
            shear_strength_z: None, // derived 0.6·strength_z until measured
            layer_shear_on: true,
            accel: [0.0; 3],
            target_cells: 300_000,
            fixed_h: None,
            snap_wall: 0.0,
            composite_skin: false,
            smooth_stress: false,
            material_stress: true,
            grid: None,
            solver_cache: None,
            solution: None,
            solution_eps: None,
            opt_eps: None,
            results: std::collections::HashMap::new(),
            opt: None,
            build_bonded: None,
            build_released: None,
            build_peel: None,
            sweep: None,
            build_grid: None,
            build_eps: None,
            build_eigen: [0.0; 3],
            transform_accum: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
        }
    }
}

#[wasm_bindgen]
impl Model {
    /// Parse STL (binary/ASCII) or 3MF (zip magic); segment at 10° (fine
    /// patches pick better; the slider re-segments live). STEP loads through
    /// `from_mesh` — tessellated JS-side by meshStep (DESIGN §18).
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: &[u8], name: &str) -> Result<Model, JsValue> {
        let (mesh_orig, mesh_objects, cad_face_of_orig) = import_any(bytes)?;
        Ok(Self::from_import(mesh_orig, mesh_objects, cad_face_of_orig, name))
    }

    /// Pre-tessellated import (DESIGN §18): meshStep runs in a JS import
    /// worker and hands over an indexed mesh (mm, welded) with per-triangle
    /// CAD-face and solid ids. Both id arrays must be DENSE indices — the JS
    /// side densifies the STEP entity record numbers and keeps the dense→
    /// entity-id tables for persistence, so `cad_segmentation`'s max+1 patch
    /// sizing stays bounded by the real face count. Refinement + segmentation
    /// are identical to the bytes path with CAD faces present.
    pub fn from_mesh(
        positions: &[f32],
        indices: &[u32],
        face_of_tri: &[u32],
        solid_of_tri: &[u32],
        name: &str,
    ) -> Result<Model, JsValue> {
        if positions.len() % 3 != 0 {
            return Err(err("positions length must be a multiple of 3"));
        }
        if indices.len() % 3 != 0 {
            return Err(err("indices length must be a multiple of 3"));
        }
        let ntri = indices.len() / 3;
        if ntri == 0 {
            return Err(err("mesh has no triangles"));
        }
        if face_of_tri.len() != ntri {
            return Err(err("face_of_tri length must equal the triangle count"));
        }
        if !solid_of_tri.is_empty() && solid_of_tri.len() != ntri {
            return Err(err("solid_of_tri length must equal the triangle count"));
        }
        let nvert = positions.len() / 3;
        let mut tris: Vec<[f32; 9]> = Vec::with_capacity(ntri);
        for t in 0..ntri {
            let mut tri = [0f32; 9];
            for v in 0..3 {
                let i = indices[3 * t + v] as usize;
                if i >= nvert {
                    return Err(err("triangle index out of range"));
                }
                tri[3 * v..3 * v + 3].copy_from_slice(&positions[3 * i..3 * i + 3]);
            }
            tris.push(tri);
        }
        let mesh_orig = TriMesh { tris };
        // Distinct-solid count plays the mesh-object role (UI warns > 1 only
        // for 3MF; the geometric `body_count` drives the multi-body warning).
        let objects = solid_of_tri.iter().copied().max().map_or(1, |m| m as usize + 1);
        Ok(Self::from_import(mesh_orig, objects, Some(face_of_tri.to_vec()), name))
    }

    /// Number of mesh objects found in the imported file (UI warns when >1).
    pub fn mesh_object_count(&self) -> u32 {
        self.mesh_objects as u32
    }

    /// Disconnected solid bodies in the working mesh (cavity shells and debris
    /// don't count). UI warns when > 1 — the solver can't join separate bodies.
    pub fn body_count(&self) -> u32 {
        self.bodies as u32
    }

    pub fn triangle_count(&self) -> u32 {
        self.mesh.len() as u32
    }

    /// Triangle soup positions, 9 floats per triangle (three.js non-indexed).
    pub fn positions(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.mesh.tris.len() * 9);
        for t in &self.mesh.tris {
            out.extend_from_slice(t);
        }
        out
    }

    /// Patch id per triangle.
    pub fn patch_ids(&self) -> Vec<u32> {
        self.seg.patch_of_tri.clone()
    }

    pub fn patch_count(&self) -> u32 {
        self.seg.patch_count as u32
    }

    /// Rigid-transform the part in place: p' = R·p + t with `m` =
    /// [r00,r01,r02, r10,r11,r12, r20,r21,r22, tx,ty,tz] (row-major R, mm t).
    /// Both the working and the original tessellation move, so exports carry
    /// the print orientation. Segmentation patches and BC triangle lists are
    /// index-based and survive; the grid and every result drop. Z stays the
    /// build direction — this is how the user orients the print.
    pub fn transform(&mut self, m: &[f64]) -> Result<(), JsValue> {
        if m.len() != 12 {
            return Err(err("transform expects 12 values (3x3 row-major + translation)"));
        }
        let apply = |mesh: &mut TriMesh| {
            for t in mesh.tris.iter_mut() {
                for v in 0..3 {
                    let p = [t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64];
                    for r in 0..3 {
                        t[3 * v + r] =
                            (m[3 * r] * p[0] + m[3 * r + 1] * p[1] + m[3 * r + 2] * p[2] + m[9 + r])
                                as f32;
                    }
                }
            }
        };
        apply(&mut self.mesh);
        apply(&mut self.mesh_orig);
        // Compose into the cumulative orientation (this transform applied AFTER
        // the accumulated one): R' = Rm·Ra, t' = Rm·ta + tm.
        let a = self.transform_accum;
        let mut na = [0f64; 12];
        for r in 0..3 {
            for c in 0..3 {
                na[3 * r + c] = m[3 * r] * a[c] + m[3 * r + 1] * a[3 + c] + m[3 * r + 2] * a[6 + c];
            }
            na[9 + r] = m[3 * r] * a[9] + m[3 * r + 1] * a[10] + m[3 * r + 2] * a[11] + m[9 + r];
        }
        self.transform_accum = na;
        self.grid = None;
        self.solver_cache = None;
        self.solution = None;
        self.solution_eps = None;
        self.opt = None;
        self.results.clear(); // geometry moved — stashed results no longer align
        self.sweep = None;
        Ok(())
    }

    /// Cumulative orientation transform since import (for project save).
    pub fn transform_matrix(&self) -> Vec<f64> {
        self.transform_accum.to_vec()
    }

    /// Re-run segmentation with a different crease angle (degrees). Switches the
    /// surface patches back to dihedral region-growing (away from CAD faces).
    pub fn resegment(&mut self, angle_deg: f64) {
        self.seg =
            remap_segmentation(&segment(&self.mesh_orig, angle_deg.clamp(1.0, 89.0)), &self.parents);
    }

    /// True when this model was imported from STEP and exact BREP faces are
    /// available as a surface-patch source.
    pub fn has_cad_faces(&self) -> bool {
        self.cad_face_of_orig.is_some()
    }

    /// Switch surface patches to the STEP file's exact BREP faces (one patch per
    /// CAD face). No-op for STL/3MF models.
    pub fn use_cad_faces(&mut self) {
        if let Some(fot) = self.cad_face_of_orig.clone() {
            self.seg = remap_segmentation(&cad_segmentation(&fot), &self.parents);
        }
    }

    /// [lox, loy, loz, hix, hiy, hiz] in mm.
    pub fn bbox(&self) -> Vec<f64> {
        match self.mesh.bounds() {
            Some((lo, hi)) => vec![lo[0], lo[1], lo[2], hi[0], hi[1], hi[2]],
            None => vec![0.0; 6],
        }
    }

    /// e0 in MPa, density in g/cm³, strengths in MPa (in-layer tensile and
    /// layer-adhesion / cross-layer). `shear_strength_z_mpa` is the interlayer
    /// SHEAR strength; pass undefined/None to derive it as 0.6·strength_z.
    pub fn set_material(
        &mut self,
        e0: f64,
        nu: f64,
        density_g_cm3: f64,
        strength_mpa: f64,
        strength_z_mpa: f64,
        shear_strength_z_mpa: Option<f64>,
    ) {
        self.settings.e0 = e0;
        self.settings.nu = nu;
        self.density = density_g_cm3 * 1e-9;
        self.strength = strength_mpa.max(0.1);
        self.strength_z = strength_z_mpa.max(0.1);
        self.shear_strength_z = shear_strength_z_mpa.map(|v| v.max(0.1));
        self.solution = None;
        self.opt = None;
    }

    /// Effective interlayer shear strength (MPa): the measured value if set,
    /// else the DESIGN §15 default 0.6·strength_z. INFINITE when the shear
    /// term is toggled off — every consumer's τ term vanishes identically.
    fn shear_strength_z_eff(&self) -> f64 {
        if !self.layer_shear_on {
            return f64::INFINITY;
        }
        self.shear_strength_z.unwrap_or(0.6 * self.strength_z)
    }

    /// Toggle the shear term of the layer criterion (display-side derived —
    /// affects `sfz`/`sf` fields, the orientation sweep and its preview; the
    /// solution stays valid).
    pub fn set_layer_shear(&mut self, on: bool) {
        self.layer_shear_on = on;
    }

    /// Set the active world acceleration (mm/s²) for self-weight + remote masses
    /// (DESIGN §16). The worker resolves each load step's active acceleration
    /// entities to ONE summed vector and sets it here before solving; `[0,0,0]`
    /// clears the inertial load. Invalidates the cached solution/design.
    pub fn set_accel(&mut self, ax: f64, ay: f64, az: f64) {
        self.accel = [ax, ay, az];
        self.solution = None;
        self.opt = None;
    }

    pub fn set_resolution(&mut self, target_cells: u32) {
        let clamped = target_cells.clamp(10_000, 4_000_000);
        if clamped != self.target_cells || self.fixed_h.is_some() {
            self.target_cells = clamped;
            self.fixed_h = None; // back to auto (part-volume) sizing
            self.grid = None;
            self.solution = None;
            self.opt = None;
        }
    }

    /// Custom resolution: pin the analysis cell size to exactly `h` mm (still
    /// snapped to the wall when snapping is on), bypassing the automatic
    /// part-volume sizing. Clamped to a sane range; capped by the cell budget.
    pub fn set_voxel_size(&mut self, h: f64) {
        let h = h.clamp(0.02, 50.0);
        if self.fixed_h != Some(h) {
            self.fixed_h = Some(h);
            self.grid = None;
            self.solution = None;
            self.opt = None;
        }
    }

    /// Snap the voxel size to an integer fraction of the wall thickness
    /// (h = wall/k) so the printed skin is exactly k cell layers on flat
    /// faces. 0 disables. Changing the value invalidates grid and results.
    pub fn set_snap_wall(&mut self, wall_mm: f64) {
        let w = if wall_mm > 0.0 { wall_mm.clamp(WALL_MM.0, WALL_MM.1) } else { 0.0 };
        if (w - self.snap_wall).abs() > 1e-9 {
            self.snap_wall = w;
            self.grid = None;
            self.solution = None;
            self.solution_eps = None;
            self.opt = None;
        }
    }

    /// Composite skin on/off (see the `composite_skin` field). The grid
    /// itself is unaffected — only results depend on the classification.
    pub fn set_composite_skin(&mut self, on: bool) {
        if on != self.composite_skin {
            self.composite_skin = on;
            self.solution = None;
            self.solution_eps = None;
            self.opt = None;
            // Skin/design split changes → the optimized stiffness field (same
            // cell count, different values) is stale; the length guard in
            // solve_optimized can't see this, so drop it here.
            self.opt_eps = None;
        }
    }

    /// Smoothed stress display on/off (see the `smooth_stress` field).
    /// Pure post-processing: the solution stays valid, fields are simply
    /// recomputed on the next fetch.
    pub fn set_smooth_stress(&mut self, on: bool) {
        self.smooth_stress = on;
    }

    /// Material (occupancy-decoupled) stress display on/off (see the
    /// `material_stress` field). Pure post-processing: the solution stays valid,
    /// fields are recomputed on the next fetch; the safety factor is unaffected.
    pub fn set_material_stress(&mut self, on: bool) {
        self.material_stress = on;
    }

    pub fn clear_bcs(&mut self) {
        self.bcs.clear();
        self.solution = None;
        self.opt = None;
    }

    /// Drop all registered multi-load optimization cases (DESIGN §13). The front
    /// end calls this before EACH optimize, then `add_load_case` per included
    /// step (none ⇒ single-load).
    pub fn clear_load_cases(&mut self) {
        self.load_cases.clear();
    }

    /// Snapshot the CURRENT `bcs` (+ the active acceleration `set_bcs` resolved
    /// for this step) as a weighted load case for the multi-load optimizer. Call
    /// after `set_bcs`/`add_*` have populated this step's BCs. The accel snapshot
    /// lets the optimizer recompute each case's design-dependent self-weight
    /// (DESIGN §16 dec. 4).
    pub fn add_load_case(&mut self, weight: f64) {
        self.load_cases.push((self.bcs.clone(), weight.max(0.0), self.accel));
    }

    pub fn add_fixed(&mut self, tris: &[u32]) {
        self.bcs.push(BcSpec { kind: BcKind::Fixed, tris: tris.to_vec() });
        self.solution = None;
        self.opt = None;
    }

    pub fn add_frictionless(&mut self, tris: &[u32]) {
        self.bcs.push(BcSpec { kind: BcKind::Frictionless, tris: tris.to_vec() });
        self.solution = None;
        self.opt = None;
    }

    /// Displacement support: prescribe the selected global axes (x/y/z) to a
    /// value in mm (0 = pin to zero), leaving the unselected ones free.
    #[allow(clippy::too_many_arguments)]
    pub fn add_displacement(
        &mut self,
        tris: &[u32],
        fx: bool,
        fy: bool,
        fz: bool,
        vx: f64,
        vy: f64,
        vz: f64,
    ) {
        self.bcs.push(BcSpec {
            kind: BcKind::Displacement([fx, fy, fz], [vx, vy, vz]),
            tris: tris.to_vec(),
        });
        self.solution = None;
        self.opt = None;
    }

    /// Elastic ("soft") support: Winkler foundation, bedding modulus k in
    /// N/mm³ (surface pressure per unit displacement, σ = k·u).
    pub fn add_elastic(&mut self, tris: &[u32], k: f64) {
        self.bcs.push(BcSpec { kind: BcKind::Elastic(k.clamp(1e-4, 1e7)), tris: tris.to_vec() });
        self.solution = None;
        self.opt = None;
    }

    pub fn add_force(&mut self, tris: &[u32], fx: f64, fy: f64, fz: f64) {
        self.bcs.push(BcSpec { kind: BcKind::Force([fx, fy, fz]), tris: tris.to_vec() });
        self.solution = None;
        self.opt = None;
    }

    pub fn add_pressure(&mut self, tris: &[u32], mpa: f64) {
        self.bcs.push(BcSpec { kind: BcKind::Pressure(mpa), tris: tris.to_vec() });
        self.solution = None;
        self.opt = None;
    }

    /// Bearing load (N): radial force on a cylindrical bore, cosine-distributed
    /// over the loaded half (Ansys-style). Axial component is rejected.
    pub fn add_bearing(&mut self, tris: &[u32], fx: f64, fy: f64, fz: f64) {
        self.bcs.push(BcSpec { kind: BcKind::Bearing([fx, fy, fz]), tris: tris.to_vec() });
        self.solution = None;
        self.opt = None;
    }

    /// Moment (N·mm): deformable distributed couple over the selection.
    pub fn add_moment(&mut self, tris: &[u32], mx: f64, my: f64, mz: f64) {
        self.bcs.push(BcSpec { kind: BcKind::Moment([mx, my, mz]), tris: tris.to_vec() });
        self.solution = None;
        self.opt = None;
    }

    /// Remote point mass (DESIGN §16): a component of `mass` TONNE with its CG at
    /// world `(px,py,pz)` mm, bolted to the selected patch. `rigid` picks the
    /// mount: DEFORMABLE (false) loads the patch with F = m·a + the transported
    /// couple (p − c) × F, adding no stiffness; RIGID (true, milestone 4) also
    /// stiffens the patch by tying it to a 6-DOF master at the CG (the load then
    /// distributes by the rigidity kinematics). Inert (load-wise) with no accel;
    /// the rigid stiffness is present regardless.
    pub fn add_mass(&mut self, tris: &[u32], px: f64, py: f64, pz: f64, mass: f64, rigid: bool) {
        self.bcs.push(BcSpec {
            kind: BcKind::Mass { point: [px, py, pz], mass, rigid },
            tris: tris.to_vec(),
        });
        self.solution = None;
        self.opt = None;
    }

    /// Fit a triangle selection to a cylinder and return the result as JSON:
    /// `{ok, axis:[3], point:[3], radius, residual}`. `ok` is true only when the
    /// cylindricity residual is within tolerance — the front end uses it to
    /// accept/reject a bearing-load selection and to draw the axis glyph.
    pub fn fit_cylinder(&self, tris: &[u32]) -> String {
        match filasim_core::attach::fit_selection_cylinder(&self.mesh, tris) {
            Some(c) => {
                let ok = c.residual.is_finite() && c.residual <= filasim_core::cylinder::DEFAULT_TOL;
                format!(
                    "{{\"ok\":{},\"axis\":[{},{},{}],\"point\":[{},{},{}],\"radius\":{},\"residual\":{}}}",
                    ok,
                    c.axis[0], c.axis[1], c.axis[2],
                    c.point[0], c.point[1], c.point[2],
                    c.radius, c.residual
                )
            }
            None => "{\"ok\":false,\"axis\":[0,0,1],\"point\":[0,0,0],\"radius\":0,\"residual\":null}"
                .to_string(),
        }
    }

    fn ensure_grid(&mut self) -> Result<(), JsValue> {
        if self.grid.is_some() {
            return Ok(());
        }
        // A grid rebuild means any cached optimized stiffness field no longer
        // matches the cell layout — drop it (re-optimizing sets a fresh one).
        self.opt_eps = None;
        // Build-sim states are tied to the old cell layout too.
        self.build_bonded = None;
        self.build_released = None;
        self.build_peel = None;
        self.build_grid = None;
        self.build_eps = None;
        // Voxelize the ORIGINAL import, not `self.mesh`. `self.mesh` is the
        // display-refined tessellation (each face split into coplanar
        // sub-triangles so deflection curves render smoothly) — that subdivision
        // leaves the surface geometry, and hence the winding number, unchanged,
        // so it only inflates the per-cell near-field triangle count the
        // occupancy pass has to integrate. The coarse original gives the
        // identical classification far faster.
        let grid = VoxelGrid::voxelize(&self.mesh_orig, self.analysis_h()?);
        if grid.solid_count() == 0 {
            return Err(err("voxelization produced no solid cells — model too thin for this resolution"));
        }
        let (padded, levels) = pad_for_levels(&grid, self.settings.max_levels);
        self.grid = Some((padded, levels));
        Ok(())
    }

    /// The analysis cell size `h` (mm) for the current resolution setting.
    /// Sizes the grid from the part's ACTUAL volume so a part that fills only
    /// part of its bounding box still gets ~target solid cells. A floor at 2% of
    /// the bbox guards degenerate/open meshes whose signed volume is near zero
    /// from exploding the cell count. Shared by `ensure_grid` and the build sim
    /// (which scales it up for a coarser grid).
    fn analysis_h(&self) -> Result<f64, JsValue> {
        let (lo, hi) = self.mesh.bounds().ok_or_else(|| err("empty mesh"))?;
        let bbox_vol =
            (hi[0] - lo[0]).max(1e-6) * (hi[1] - lo[1]).max(1e-6) * (hi[2] - lo[2]).max(1e-6);
        let h = if let Some(fh) = self.fixed_h {
            // Custom mode: the user's exact cell size, snapped to the wall when
            // snapping is on, floored so it can't blow the cell budget.
            let h = if self.snap_wall > 0.0 {
                let k = (self.snap_wall / fh).round().max(1.0);
                (self.snap_wall / k).max(1e-3)
            } else {
                fh
            };
            h.max((bbox_vol / 4_000_000.0).cbrt())
        } else {
            let part_vol = self.mesh.volume().abs();
            let fill_vol = if part_vol.is_finite() && part_vol > 1e-9 {
                part_vol.clamp(bbox_vol * 0.02, bbox_vol)
            } else {
                bbox_vol
            };
            filasim_core::voxel::pick_voxel_size(
                fill_vol,
                bbox_vol,
                self.target_cells as f64,
                self.snap_wall,
            )
        };
        Ok(h)
    }

    /// JSON: { nx, ny, nz, h, cells, solid }
    pub fn voxel_info(&mut self) -> Result<String, JsValue> {
        self.ensure_grid()?;
        let (g, _) = self.grid.as_ref().unwrap();
        Ok(serde_json::json!({
            "nx": g.nx, "ny": g.ny, "nz": g.nz, "h": g.h,
            "cells": g.cell_count(), "solid": g.solid_count(),
        })
        .to_string())
    }

    /// Assemble the inertial body load for a solve whose per-cell material
    /// volume fraction is `vfrac` (DESIGN §16 dec. 3). `None` when no
    /// acceleration is active, so the accel-free path stays byte-identical.
    fn body_arg<'a>(&self, vfrac: &'a [f32]) -> Option<BodyLoad<'a>> {
        if self.accel != [0.0; 3] {
            Some(BodyLoad { accel: self.accel, density: self.density, vfrac })
        } else {
            None
        }
    }

    /// MASS-ONLY body load for the OPTIMIZER (DESIGN §16 dec. 4): an empty vfrac
    /// realizes remote point masses (F = m·a, design-independent) and marks solid
    /// cells for the RBM check, but skips the distributed self-weight FORCE — the
    /// optimizer recomputes that from the live density each SIMP iteration.
    /// `None` when no acceleration is active (byte-identical no-body path).
    fn mass_only_body(accel: [f64; 3], density: f64) -> Option<BodyLoad<'static>> {
        if accel != [0.0; 3] {
            Some(BodyLoad { accel, density, vfrac: &[] })
        } else {
            None
        }
    }

    /// The optimizer's per-case design-dependent self-weight descriptor
    /// (recomputed every SIMP iteration). `None` when no acceleration is active.
    fn opt_body_accel(accel: [f64; 3], density: f64) -> Option<filasim_core::simp::BodyAccel> {
        if accel != [0.0; 3] {
            Some(filasim_core::simp::BodyAccel { accel, density })
        } else {
            None
        }
    }

    /// Island + rigid-body-mode check. JSON CheckReport.
    pub fn check(&mut self) -> Result<String, JsValue> {
        self.ensure_grid()?;
        let (grid, _) = self.grid.as_ref().unwrap();
        // Occupancy is enough for the RBM has-loads flag — any solid cell under
        // an active accel carries self-weight (dec. 11); the magnitude is moot.
        let asm = assemble(&self.mesh, grid, &self.bcs, self.body_arg(&grid.scale), &self.settings)
            .map_err(err)?;
        let report = check_problem(grid, &asm);
        let comps: Vec<serde_json::Value> = report
            .components
            .iter()
            .map(|c| {
                serde_json::json!({
                    "cells": c.cells,
                    "constrained": c.constrained,
                    "lambdaRatio": c.lambda_ratio,
                    "hasLoads": c.has_loads,
                    "mode": c.mode.as_ref().map(|m| serde_json::json!({
                        "t": m.t, "r": m.r, "center": m.center,
                    })),
                })
            })
            .collect();
        Ok(serde_json::json!({
            "ok": report.ok,
            "islandCount": report.island_count,
            "components": comps,
        })
        .to_string())
    }

    /// Run the static solve. JSON: { iterations, relResidual, maxDisplacement }.
    pub fn solve(&mut self) -> Result<String, JsValue> {
        self.ensure_grid()?;
        let (grid, levels) = self.grid.as_ref().unwrap();
        // Plain solid solve: eps = grid occupancy, so self-weight uses the same
        // occupancy field for its per-cell material volume fraction.
        let asm = assemble(&self.mesh, grid, &self.bcs, self.body_arg(&grid.scale), &self.settings)
            .map_err(err)?;
        let report = check_problem(grid, &asm);
        if !report.ok {
            return Err(err("model is under-constrained — run check() for details"));
        }
        let sol = solve_nodes_cached(&mut self.solver_cache, grid, *levels, &asm.problem, &self.settings)
            .map_err(err)?;
        let out = serde_json::json!({
            "iterations": sol.iterations,
            "relResidual": sol.rel_residual,
            "converged": sol.converged,
            "maxDisplacement": sol.max_displacement(),
            // MGCG relative-residual convergence target — the live plot's limit.
            "tol": self.settings.tol,
            // Per-MGCG-iteration relative residual (element 0 = initial) —
            // the nerd-log convergence plot.
            "residuals": sol.residuals.clone(),
        })
        .to_string();
        self.solution = Some(sol);
        self.solution_eps = None; // plain solid solve: eps = grid.scale
        Ok(out)
    }

    /// FDM build simulation (inherent strain, see `filasim_core::buildsim`): predict
    /// warping + bed peel from the current voxelization. Ignores structural BCs —
    /// the only "loads" are the per-layer eigenstrain and the build plate. Leaves
    /// the chosen state (released = off-bed sprung shape, or bonded) as the live
    /// solution, so `vertex_displacements()` maps the warp onto the REAL mesh and
    /// the existing deformed view renders it. JSON: max displacements + peel peaks.
    /// `on_layer(layersDone, totalLayers, displacements)` is called per activated
    /// layer for the live progress bar + warp preview. `displacements` is the
    /// accumulating bonded warp mapped onto the mesh, but only on throttled frames
    /// (~30 over the build); it is an empty array on the other layers (progress-
    /// only). A stopped run (cancel flag) propagates as an error.
    pub fn solve_build_sim(
        &mut self,
        opts_json: &str,
        on_layer: &js_sys::Function,
    ) -> Result<String, JsValue> {
        let opts: BuildSimOpts = serde_json::from_str(opts_json).map_err(err)?;
        let eigen = [opts.shrink, opts.shrink, opts.shrink_z.unwrap_or(opts.shrink)];
        let exag = opts.exaggeration;
        // The sequential build does one multigrid solve PER layer, so cell count
        // drives the cost. Run on a deliberately COARSER grid than analysis:
        // cbrt(2) ≈ 1.26× the cell size → ~half the cells. The warp shape is
        // resolution-robust, so this buys a much faster preview cheaply. (Kept
        // local — never touches self.grid / opt_eps, which stay at analysis res.)
        let h_build = self.analysis_h()? * 2f64.cbrt();
        // Coarse original, not `self.mesh` — see the note in `ensure_grid`:
        // display subdivision doesn't change the winding number, only the cost.
        let braw = VoxelGrid::voxelize(&self.mesh_orig, h_build);
        if braw.solid_count() == 0 {
            return Err(err("voxelization produced no solid cells — model too thin for this resolution"));
        }
        let (grid, _levels) = pad_for_levels(&braw, self.settings.max_levels);
        // Drive the build sim with the as-printed infill density when an
        // optimized design exists (sparse infill is softer AND lays down less
        // contracting material — both scale by the same per-cell eps). The
        // optimized field lives on the FINER analysis grid, so resample it onto
        // the coarse build grid. Falls back to the solid hull when nothing has
        // been optimized.
        let eps_override: Option<Vec<f32>> = match (&self.opt_eps, &self.grid) {
            (Some(fine_eps), Some((fine, _))) => Some(resample_eps(fine, fine_eps, &grid)),
            _ => None,
        };
        let density_aware = eps_override.is_some();
        // Yield stress (>0) turns on the plastic correction so the released warp
        // responds to geometry/infill density (otherwise a uniform eigenstrain
        // releases to the same density-blind compatible shrink).
        let yield_strength = (opts.yield_strength > 0.0).then_some(opts.yield_strength);
        let ladder = opts.ladder();
        let r = filasim_core::buildsim::solve_build_progress(
            &grid,
            eigen,
            &self.settings,
            eps_override.as_deref(),
            yield_strength,
            ladder.as_ref(),
            |done, total, sol| {
                // Throttle the (expensive) hull build to ~30 preview frames. On
                // sent frames the payload is the deformed ACTIVATED voxel hull
                // (positions + normalised |u| + max |u|); empty otherwise.
                let stride = (total / 30).max(1);
                let (pos, mags, maxu) = if done == total || done % stride == 0 {
                    deformed_activated_hull(&grid, sol, exag)
                } else {
                    (Vec::new(), Vec::new(), 0.0)
                };
                let args = js_sys::Array::new();
                args.push(&JsValue::from(done as u32));
                args.push(&JsValue::from(total as u32));
                args.push(&JsValue::from(js_sys::Float32Array::from(pos.as_slice())));
                args.push(&JsValue::from(js_sys::Float32Array::from(mags.as_slice())));
                args.push(&JsValue::from(maxu));
                let _ = on_layer.apply(&JsValue::NULL, &args);
            },
        )
        .map_err(err)?;

        // Lay the bed reactions out as a 2D lift/shear field over the footprint
        // (the z=0 node plane of the coarse build grid) for the peel-risk view.
        // Convert each NODAL reaction force to a TRACTION (stress) by dividing by
        // the node's tributary bed area — the mesh-INDEPENDENT quantity: a finer
        // grid splits the same total reaction over more nodes (force per node
        // shrinks ∝ area) but the traction converges. Tributary area = (number of
        // adjacent bottom-layer solid cells) × h²/4. Units: N/mm² = MPa.
        let (nx, ny) = (grid.nx, grid.ny);
        let (mx, my) = (nx + 1, ny + 1);
        let cell_area = (grid.h * grid.h) as f32;
        let mut lift = vec![0f32; mx * my];
        let mut shear = vec![0f32; mx * my];
        for (n, rv) in &r.bed_reaction {
            let i = *n as usize; // z=0 plane: i = iy*mx + ix
            if i >= mx * my {
                continue;
            }
            let (ix, iy) = (i % mx, i / mx);
            let mut cells = 0u32;
            for (ox, oy) in [(-1i64, -1i64), (0, -1), (-1, 0), (0, 0)] {
                let (cx, cy) = (ix as i64 + ox, iy as i64 + oy);
                if cx >= 0
                    && cy >= 0
                    && cx < nx as i64
                    && cy < ny as i64
                    && grid.scale[cy as usize * nx + cx as usize] > 0.0
                {
                    cells += 1;
                }
            }
            let area = cells as f32 * cell_area * 0.25;
            if area <= 0.0 {
                continue;
            }
            lift[i] = rv[2] as f32 / area;
            shear[i] = (rv[0] * rv[0] + rv[1] * rv[1]).sqrt() as f32 / area;
        }
        let mut peak_lift = 0f64;
        let mut peak_shear = 0f64;
        for i in 0..mx * my {
            peak_lift = peak_lift.max(lift[i] as f64);
            peak_shear = peak_shear.max(shear[i] as f64);
        }
        self.build_peel = Some(PeelField {
            mx,
            my,
            origin: grid.origin,
            h: grid.h,
            lift,
            shear,
            // Ramp the risk to zero over the first ~5 coarse layers, capped so a
            // short part still fades within itself.
            falloff: (grid.h * 5.0).min(0.3 * grid.nz as f64 * grid.h).max(grid.h),
        });
        let (bonded_max, released_max) = (r.bonded.max_displacement(), r.released.max_displacement());
        let iters_max = r.iters.iter().copied().max().unwrap_or(0);
        let iters_mean = if r.iters.is_empty() {
            0.0
        } else {
            r.iters.iter().sum::<usize>() as f64 / r.iters.len() as f64
        };
        let cells = grid.solid_count();
        let (gnx, gny, gnz, gh) = (grid.nx, grid.ny, grid.nz, grid.h);
        // Keep the coarse grid + the eps/eigen it was solved with so residual
        // print-stress fields can be evaluated on the SAME grid + state later.
        self.build_eps = Some(eps_override.unwrap_or_else(|| filasim_core::solve::grid_eps(&grid)));
        self.build_eigen = eigen;
        self.build_grid = Some(grid);
        // Keep BOTH states so the UI can flip on bed ⇄ released with no re-solve.
        self.build_bonded = Some(r.bonded);
        self.build_released = Some(r.released);
        let sol = self.build_state_solution(&opts.state).clone();
        let out = serde_json::json!({
            "maxDisplacement": sol.max_displacement(),
            "bondedMax": bonded_max,
            "releasedMax": released_max,
            "peakLift": peak_lift,
            "peakShear": peak_shear,
            "layers": r.iters.len(),
            "itersMax": iters_max,
            "itersMean": iters_mean,
            "cells": cells,
            "densityAware": density_aware,
            // Coarse build-grid dims (≠ analysis grid) for the log.
            "nx": gnx, "ny": gny, "nz": gnz, "h": gh,
        })
        .to_string();
        self.solution = Some(sol);
        self.solution_eps = None;
        Ok(out)
    }

    /// The (grid, eps, eigenstrain) the CURRENT solution lives on. For a
    /// build-sim result — detected because the solution's node dims match the
    /// coarse build grid — that's the coarse grid, its build eps, and the
    /// applied eigenstrain (so stress evaluates as the RESIDUAL print stress,
    /// `σ = C:(ε(u) − ε₀)`). Otherwise the analysis grid + its solve eps + no
    /// eigenstrain (ordinary structural stress).
    fn solution_grid(&self) -> Result<(&VoxelGrid, Option<&[f32]>, [f64; 3]), JsValue> {
        let sol = self
            .solution
            .as_ref()
            .ok_or_else(|| err("no solution — run Solve or Optimize"))?;
        if let Some(bg) = &self.build_grid {
            if bg.nx + 1 == sol.mx && bg.ny + 1 == sol.my && bg.nz + 1 == sol.mz {
                return Ok((bg, self.build_eps.as_deref(), self.build_eigen));
            }
        }
        let (g, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        Ok((g, self.solution_eps.as_deref(), [0.0; 3]))
    }

    /// Pick the stored build-sim solution for a state string ("bonded" → on
    /// bed, anything else → released). Build-sim only; assumes a build ran.
    fn build_state_solution(&self, state: &str) -> &Solution {
        let s = if state == "bonded" { &self.build_bonded } else { &self.build_released };
        s.as_ref().unwrap()
    }

    /// Flip the active build-sim result between "bonded" (on bed) and "released"
    /// (off bed) WITHOUT re-running the build — both were saved by the last
    /// `solve_build_sim`. The chosen field becomes `self.solution`, so the
    /// deformed Results view (and `vertex_displacements`) re-renders it.
    /// JSON: `{ maxDisplacement }`. Errors if no build has been run.
    pub fn set_build_state(&mut self, state: &str) -> Result<String, JsValue> {
        if self.build_released.is_none() {
            return Err(err("no build simulation result to switch — run the build sim first"));
        }
        let sol = self.build_state_solution(state).clone();
        let max = sol.max_displacement();
        self.solution = Some(sol);
        self.solution_eps = None;
        Ok(serde_json::json!({ "maxDisplacement": max }).to_string())
    }

    /// Bed-peel as a flat heatmap sitting ON the build plate: a triangle soup
    /// covering the part's FOOTPRINT (bottom-layer solid cells of the coarse
    /// build grid) at z = bed, plus a per-vertex value (lift or shear). Lets the
    /// risk be read from a normal top/iso view instead of from under the part.
    /// Returns `[positions Float32Array (9/tri), values Float32Array (3/tri)]`.
    pub fn peel_map(&self, kind: &str) -> Result<js_sys::Array, JsValue> {
        let pf = self
            .build_peel
            .as_ref()
            .ok_or_else(|| err("no peel data — run the build sim first"))?;
        let bg = self.build_grid.as_ref().ok_or_else(|| err("no build grid"))?;
        let src = if kind == "peelshear" { &pf.shear } else { &pf.lift };
        let (nx, ny) = (bg.nx, bg.ny);
        let mx = pf.mx;
        // Float the map a hair above the plate so it doesn't z-fight the grid.
        let z = (pf.origin[2] + 0.02 * pf.h) as f32;
        let node = |ix: usize, iy: usize| -> ([f32; 3], f32) {
            (
                [
                    (pf.origin[0] + ix as f64 * pf.h) as f32,
                    (pf.origin[1] + iy as f64 * pf.h) as f32,
                    z,
                ],
                src[iy * mx + ix].max(0.0), // only upward lift = risk
            )
        };
        let mut pos: Vec<f32> = Vec::new();
        let mut val: Vec<f32> = Vec::new();
        for cy in 0..ny {
            for cx in 0..nx {
                if bg.scale[cy * nx + cx] <= 0.0 {
                    continue; // bottom layer (cz = 0): index is cy*nx + cx
                }
                let corners = [(cx, cy), (cx + 1, cy), (cx + 1, cy + 1), (cx, cy + 1)];
                let n: Vec<([f32; 3], f32)> = corners.iter().map(|&(a, b)| node(a, b)).collect();
                for &i in &[0usize, 1, 2, 0, 2, 3] {
                    pos.extend_from_slice(&n[i].0);
                    val.push(n[i].1);
                }
            }
        }
        let arr = js_sys::Array::new();
        arr.push(&js_sys::Float32Array::from(pos.as_slice()));
        arr.push(&js_sys::Float32Array::from(val.as_slice()));
        Ok(arr)
    }

    /// Bed-peel risk as a per-mesh-vertex scalar (3 per display triangle, same
    /// layout as `result_field`), sampled from the last build sim's bed
    /// reactions. `kind`: "peel" = upward lift (+Z, the peel driver), "peelshear"
    /// = in-plane bed shear magnitude. Each vertex takes the reaction at its
    /// nearest footprint node, ramped to zero over the first layers so the risk
    /// reads at the base. Traction in MPa, uncalibrated (a RELATIVE indicator).
    /// Errors if no build sim has been run.
    pub fn peel_field(&self, kind: &str) -> Result<Vec<f32>, JsValue> {
        let pf = self
            .build_peel
            .as_ref()
            .ok_or_else(|| err("no peel data — run the build sim first"))?;
        let src = if kind == "peelshear" { &pf.shear } else { &pf.lift };
        let mut out = Vec::with_capacity(self.mesh.tris.len() * 3);
        for t in &self.mesh.tris {
            for v in 0..3 {
                let (px, py, pz) = (t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64);
                let ix = (((px - pf.origin[0]) / pf.h).round() as i64)
                    .clamp(0, pf.mx as i64 - 1) as usize;
                let iy = (((py - pf.origin[1]) / pf.h).round() as i64)
                    .clamp(0, pf.my as i64 - 1) as usize;
                let base = src[iy * pf.mx + ix].max(0.0); // only positive lift = risk
                let d = (pz - pf.origin[2]).max(0.0);
                let att = (1.0 - d / pf.falloff).clamp(0.0, 1.0) as f32;
                out.push(base * att);
            }
        }
        Ok(out)
    }

    /// Analyze the part AS PRINTED: skin (perimeters × line width) at 100%,
    /// interior at a uniform infill ratio through the calibrated pattern law.
    /// Same machinery as the optimizer's verification solves — the accuracy
    /// is the accuracy of the calibrated E(ρ) curve. Stress/SF fields use
    /// the stored eps, so the safety factor of the printed part falls out.
    /// JSON: solve() fields + massGrams/massSolidGrams/skinCells/
    /// interiorCells/skinLayers for the results dock.
    pub fn solve_printed(&mut self, opts_json: &str) -> Result<String, JsValue> {
        let opts: PrintedOpts = serde_json::from_str(opts_json).map_err(err)?;
        self.ensure_grid()?;
        let (grid, levels) = self.grid.as_ref().unwrap();
        let eval_exp = opts.exponent.clamp(1.0, 3.5);
        let eval_coeff = opts.coeff.clamp(0.05, 2.0);
        let (_, wall_mm) = resolve_wall(opts.perimeters, opts.line_width);
        let tb_mm =
            (opts.top_bottom_layers.min(20) as f64 * opts.layer_height.clamp(0.04, 0.6)).min(5.0);
        let infill = (opts.infill_pct / 100.0).clamp(0.01, 1.0);
        // A part thinner than the wall everywhere simply prints solid —
        // design is empty and the solve degenerates to the solid case.
        let split =
            filasim_core::simp::classify_cells(grid, wall_mm, tb_mm, tb_mm, self.composite_skin);
        let (skin, design, skin_frac) = (split.skin, split.design, split.skin_frac);
        let x = vec![infill; design.len()];
        // Self-weight (DESIGN §16 dec. 3) needs the MATERIAL volume fraction of
        // each cell — built here, BEFORE assembly, so the body force rides the
        // RHS. Distinct from the E(ρ) stiffness `eps` built just below.
        let vfrac = filasim_core::simp::build_vfrac(grid, &design, &skin_frac, &x);
        let asm = assemble(&self.mesh, grid, &self.bcs, self.body_arg(&vfrac), &self.settings)
            .map_err(err)?;
        let report = check_problem(grid, &asm);
        if !report.ok {
            return Err(err("model is under-constrained — run check() for details"));
        }
        let eps =
            filasim_core::simp::build_eps(grid, &skin, &design, &skin_frac, &x, eval_exp, eval_coeff);
        let (sol, _compliance) = filasim_core::simp::solve_with_eps_cached(
            &mut self.solver_cache,
            grid,
            *levels,
            &asm.problem,
            &self.settings,
            eps.clone(),
        )
        .map_err(err)?;
        // Mass at these print settings: solid skin + interior at the ratio;
        // composite cells contribute their wall-band fraction as solid, the
        // rest at the infill ratio, everything weighted by the cell's
        // occupancy (cut boundary cells count their actual inside share).
        let cell_vol = grid.h * grid.h * grid.h;
        let vol_skin: f64 = skin.iter().map(|&c| grid.scale[c as usize] as f64).sum();
        let mut vol_wall = 0f64; // wall band inside design cells
        let mut vol_inf = 0f64; // infill share of design cells
        for (k, &c) in design.iter().enumerate() {
            let occ = grid.scale[c as usize] as f64;
            let f = skin_frac[k] as f64;
            vol_wall += occ * f;
            vol_inf += occ * (1.0 - f);
        }
        let mass = (vol_skin + vol_wall + infill * vol_inf) * cell_vol * self.density * 1e6;
        let mass_solid = (vol_skin + vol_wall + vol_inf) * cell_vol * self.density * 1e6;
        let out = serde_json::json!({
            "iterations": sol.iterations,
            "relResidual": sol.rel_residual,
            "converged": sol.converged,
            "maxDisplacement": sol.max_displacement(),
            "tol": self.settings.tol,
            "residuals": sol.residuals.clone(),
            "massGrams": mass,
            "massSolidGrams": mass_solid,
            "skinCells": skin.len(),
            "interiorCells": design.len(),
            // Cell layers the skin is modeled with: the legacy model rounds
            // (minimum one full layer), composite skin is exact — fractional
            // values (< 1 included) are real and handled by blending.
            "skinLayers": if self.composite_skin {
                wall_mm / grid.h
            } else {
                (wall_mm / grid.h).round().max(1.0)
            },
            "compositeSkin": self.composite_skin,
        })
        .to_string();
        self.solution = Some(sol);
        self.solution_eps = Some(eps);
        Ok(out)
    }

    /// Constrained undamped modal analysis (`filasim_core::modal`): the lowest
    /// `num_modes` natural frequencies + mode shapes of the part as supported by
    /// the CURRENT `bcs` (the store sets these to the first load case before the
    /// call). Force-free — applied forces/pressures are ignored; only supports
    /// constrain the eigenproblem. Remote point masses (DESIGN §16) DO count:
    /// each adds its translational inertia to the mass matrix on its attachment
    /// patch, so a heavy payload lowers the frequencies (the offset's rotatory
    /// inertia is not representable on a translational-DOF mesh — see
    /// `point_mass_lumping`). Each mode shape is stashed as a result keyed `modal::mode-i`
    /// (mass-normalized, then rescaled to unit peak for display), and mode 0 is
    /// left live. JSON: `{ converged, outerIters, modes: [{id, freqHz}] }`. The
    /// store builds one `ResultEntry` per mode from this and switches modes via
    /// `activate_result`, reusing the deformed-view + animation path.
    pub fn modal_analysis(
        &mut self,
        opts_json: &str,
        on_progress: &js_sys::Function,
    ) -> Result<String, JsValue> {
        let opts: ModalOpts = serde_json::from_str(opts_json).map_err(err)?;
        let free = opts.free;
        let num_modes = (opts.num_modes.clamp(1, 20)) as usize;
        // Free-free: also compute the 6 rigid-body modes so they can be dropped.
        let n_compute = if free { num_modes + 6 } else { num_modes };
        self.ensure_grid()?;
        // Build the modal stiffness (eps) + lumped-mass material fraction (vfrac)
        // and the transient solver hierarchy inside the grid borrow; everything
        // else needed to assemble the mode Solutions is captured owned so the
        // borrow ends before we mutate the result roster.
        let (mut cache, eps, vfrac, extra_mass, mx, my, mz, h, origin, active) = {
            let (grid, levels) = self.grid.as_ref().unwrap();
            let mut asm = assemble(&self.mesh, grid, &self.bcs, None, &self.settings).map_err(err)?;
            // Remote point masses add inertia to the eigenproblem M (DESIGN §16):
            // the static force path only realises F = m·a, so without this a
            // payload would leave the natural frequencies untouched. Resolved to
            // per-node lumped masses on the SAME patch the static force loads.
            let extra_mass = filasim_core::attach::point_mass_lumping(&self.mesh, grid, &self.bcs);
            let report = check_problem(grid, &asm);
            // Constrained modal needs supports; free-free deliberately runs
            // WITHOUT them (the rigid-body modes are lifted + dropped below).
            if !free && !report.ok {
                return Err(err(
                    "model is under-constrained for modal analysis — add supports, or enable free-free (run check() for details)",
                ));
            }
            let (eps, vfrac) = if opts.solid {
                (filasim_core::solve::grid_eps(grid), grid.scale.clone())
            } else {
                let eval_exp = opts.exponent.clamp(1.0, 3.5);
                let eval_coeff = opts.coeff.clamp(0.05, 2.0);
                let (_, wall_mm) = resolve_wall(opts.perimeters, opts.line_width);
                let tb_mm = (opts.top_bottom_layers.min(20) as f64
                    * opts.layer_height.clamp(0.04, 0.6))
                .min(5.0);
                let infill = (opts.infill_pct / 100.0).clamp(0.01, 1.0);
                let split =
                    filasim_core::simp::classify_cells(grid, wall_mm, tb_mm, tb_mm, self.composite_skin);
                let (skin, design, skin_frac) = (split.skin, split.design, split.skin_frac);
                let x = vec![infill; design.len()];
                let eps = filasim_core::simp::build_eps(
                    grid, &skin, &design, &skin_frac, &x, eval_exp, eval_coeff,
                );
                // Material volume fraction per cell for the lumped mass: solid
                // skin (occupancy), design cells = occ·(wall band solid + infill
                // ratio over the rest). Distinct from the E(ρ) stiffness eps.
                let mut vfrac = grid.scale.clone();
                for (k, &c) in design.iter().enumerate() {
                    let occ = grid.scale[c as usize] as f64;
                    let f = skin_frac[k] as f64;
                    vfrac[c as usize] = (occ * (f + infill * (1.0 - f))) as f32;
                }
                (eps, vfrac)
            };
            let active = active_nodes(grid);
            if free {
                // Soft anchor springs lift the 6 rigid-body modes so K becomes
                // invertible (the unsupported part has a singular stiffness).
                // Weak (≈1e-4·E·h) so the flexible frequencies are ~unperturbed.
                let k = 1e-4 * self.settings.e0 * grid.h;
                let anchors = filasim_core::modal::rigid_body_anchor_springs(
                    grid.nx + 1,
                    grid.ny + 1,
                    grid.nz + 1,
                    &active,
                    k,
                );
                asm.problem.springs.extend(anchors);
            }
            let cache = SolverCache::build(grid, *levels, &asm.problem, &self.settings, eps.clone());
            (
                cache,
                eps,
                vfrac,
                extra_mass,
                grid.nx + 1,
                grid.ny + 1,
                grid.nz + 1,
                grid.h,
                grid.origin,
                active,
            )
        };

        let cfg = filasim_core::modal::ModalConfig::new(n_compute);
        // Stream the current Ritz frequency estimates to JS once per outer step
        // (live progress / convergence readout).
        let progress = |outer: usize, max_outer: usize, freqs: &[f64]| {
            let arr = js_sys::Float64Array::from(freqs);
            let _ = on_progress.call3(
                &JsValue::NULL,
                &JsValue::from(outer as u32),
                &JsValue::from(max_outer as u32),
                &arr,
            );
        };
        let res = filasim_core::modal::analyze(
            &mut cache.solver,
            &vfrac,
            self.density,
            &extra_mass,
            &cfg,
            progress,
        )
        .map_err(err)?;

        // Free-free: drop the lowest modes (the lifted rigid-body modes), keeping
        // the flexible ones the user asked for.
        let drop = if free { res.shapes.len().saturating_sub(num_modes) } else { 0 };

        // Replace any prior modal modes; keep other (static/optimized) results.
        self.results.retain(|k, _| !k.starts_with("modal::"));
        let nnode = mx * my * mz;
        let mut modes_json = Vec::with_capacity(res.freqs_hz.len().saturating_sub(drop));
        for (i, shape) in res.shapes.iter().enumerate().skip(drop) {
            let mi = i - drop; // 0-based index among the KEPT modes
            // Rescale the (arbitrary-magnitude) mode shape to unit peak over
            // active nodes — the viewer re-normalizes anyway, but this keeps the
            // stashed field tidy and gives each mode the same nominal amplitude.
            let mut maxmag = 0f64;
            for n in 0..nnode {
                if active[n] {
                    let (ux, uy, uz) =
                        (shape[3 * n] as f64, shape[3 * n + 1] as f64, shape[3 * n + 2] as f64);
                    maxmag = maxmag.max((ux * ux + uy * uy + uz * uz).sqrt());
                }
            }
            let inv = if maxmag > 0.0 { 1.0 / maxmag } else { 0.0 };
            let u: Vec<f32> = shape.iter().map(|&v| (v as f64 * inv) as f32).collect();
            let sol = Solution {
                u,
                mx,
                my,
                mz,
                h,
                origin,
                active: active.clone(),
                iterations: res.outer_iters,
                rel_residual: 0.0,
                converged: res.converged,
                residuals: Vec::new(),
            };
            let id = format!("modal::mode-{mi}");
            if mi == 0 {
                self.solution = Some(sol.clone());
                self.solution_eps = Some(eps.clone());
            }
            self.results
                .insert(id.clone(), StashedResult { sol, eps: Some(eps.clone()) });
            modes_json.push(serde_json::json!({ "id": id, "freqHz": res.freqs_hz[i] }));
        }
        Ok(serde_json::json!({
            "converged": res.converged,
            "outerIters": res.outer_iters,
            "totalInnerIters": res.total_inner_iters,
            "modes": modes_json,
        })
        .to_string())
    }

    /// Re-solve the CURRENT optimized design under the CURRENT BCs — the per-step
    /// pass that fills the Results roster after a multi-load optimization
    /// (DESIGN §13). The optimized design's stress eps (density → element
    /// stiffness) is BC-independent, so every load step reuses it and only the
    /// loads/supports change; steps that share fixtures hit the cached multigrid
    /// hierarchy (cheap RHS swap). Requires a prior optimize()/restore — the live
    /// solution + eps are left on this step so stash_result snapshots it. JSON
    /// matches solve().
    pub fn solve_optimized(&mut self) -> Result<String, JsValue> {
        // ensure_grid first: a stale grid rebuilds here and clears opt_eps, so a
        // design that predates a resolution change falls through to a clean error.
        self.ensure_grid()?;
        // opt_eps is the optimized stiffness field, kept across load-step edits
        // (unlike solution_eps, which BC changes reset). Set by optimize/restore.
        let eps = self
            .opt_eps
            .clone()
            .ok_or_else(|| err("no optimized design to evaluate — run Optimize first"))?;
        let (grid, levels) = self.grid.as_ref().unwrap();
        if eps.len() != grid.cell_count() {
            return Err(err("the optimized design predates the current grid — re-run Optimize"));
        }
        // Self-weight for the optimized design uses ITS OWN per-cell mass field
        // (dec. 3), composed from the binned density — not the stiffness eps.
        // Built only when an acceleration is active; falls back to occupancy if
        // the stored design and current grid ever disagree.
        let vfrac: Vec<f32> = if self.accel != [0.0; 3] {
            match &self.opt {
                Some(opt) => {
                    let split = filasim_core::simp::classify_cells(
                        grid, opt.wall_mm, opt.tb_mm, opt.tb_mm, self.composite_skin,
                    );
                    let x: Vec<f64> = opt.x_binned.iter().map(|&v| v as f64).collect();
                    if split.design.len() == x.len() {
                        filasim_core::simp::build_vfrac(grid, &split.design, &split.skin_frac, &x)
                    } else {
                        grid.scale.clone()
                    }
                }
                None => grid.scale.clone(),
            }
        } else {
            Vec::new()
        };
        let asm = assemble(&self.mesh, grid, &self.bcs, self.body_arg(&vfrac), &self.settings)
            .map_err(err)?;
        let report = check_problem(grid, &asm);
        if !report.ok {
            return Err(err("model is under-constrained — run check() for details"));
        }
        let (sol, _compliance) = filasim_core::simp::solve_with_eps_cached(
            &mut self.solver_cache,
            grid,
            *levels,
            &asm.problem,
            &self.settings,
            eps.clone(),
        )
        .map_err(err)?;
        let out = serde_json::json!({
            "iterations": sol.iterations,
            "relResidual": sol.rel_residual,
            "converged": sol.converged,
            "maxDisplacement": sol.max_displacement(),
            "tol": self.settings.tol,
            "residuals": sol.residuals.clone(),
        })
        .to_string();
        self.solution = Some(sol);
        self.solution_eps = Some(eps);
        Ok(out)
    }

    /// Displacement vector (mm) per soup vertex: 9 floats per triangle.
    pub fn vertex_displacements(&self) -> Result<Vec<f32>, JsValue> {
        let sol = self.solution.as_ref().ok_or_else(|| err("no solution — call solve() first"))?;
        Ok(map_displacements(&self.mesh, sol))
    }

    /// Snapshot the CURRENT live solution under `id` so the Results view can
    /// recall it instantly later. Re-stashing the same id replaces it.
    pub fn stash_result(&mut self, id: &str) -> Result<(), JsValue> {
        let sol = self
            .solution
            .as_ref()
            .ok_or_else(|| err("no live solution to stash — run Solve or Optimize first"))?;
        self.results.insert(
            id.to_string(),
            StashedResult { sol: sol.clone(), eps: self.solution_eps.clone() },
        );
        Ok(())
    }

    /// Make a previously stashed result the live solution (for displacement +
    /// stress/strain queries) and return its per-soup-vertex displacements so
    /// the viewport can re-deform without a second round-trip.
    pub fn activate_result(&mut self, id: &str) -> Result<Vec<f32>, JsValue> {
        let (sol, eps) = {
            let r = self
                .results
                .get(id)
                .ok_or_else(|| err("no such result — it was never stashed or has been cleared"))?;
            (r.sol.clone(), r.eps.clone())
        };
        self.solution = Some(sol);
        self.solution_eps = eps;
        self.vertex_displacements()
    }

    /// Drop every stashed result (geometry/grid change — all are stale and the
    /// node grid they sample no longer matches the part).
    pub fn clear_results(&mut self) {
        self.results.clear();
        self.build_bonded = None;
        self.build_released = None;
        self.build_peel = None;
        self.build_grid = None;
        self.build_eps = None;
    }

    /// Assemble a `.filasim` project zip: the original model bytes, the JS-built
    /// manifest (settings + BCs + result roster), the compact optimized design,
    /// and — when `include_results` — every stashed result's displacement + eps.
    pub fn export_project(
        &self,
        model_bytes: &[u8],
        model_entry: &str,
        manifest: &str,
        include_results: bool,
    ) -> Vec<u8> {
        let mut zip = filasim_core::zip::ZipWriter::new();
        zip.add(model_entry, model_bytes);
        zip.add("project.json", manifest.as_bytes());
        if let Some(opt) = &self.opt {
            zip.add("design.bin", &design_blob(opt));
        }
        if include_results {
            for (id, r) in &self.results {
                let mut blob = Vec::new();
                put_f32s(&mut blob, &r.sol.u);
                put_f32s(&mut blob, r.eps.as_deref().unwrap_or(&[]));
                zip.add(&format!("results/{id}.f32"), &blob);
            }
        }
        zip.finish()
    }

    /// Rebuild `self.opt` (+ the optimized stress eps) from a saved design blob.
    /// Settings (material/resolution/snap/composite) and the orientation must be
    /// applied first so the grid + cell classification match the saved run.
    pub fn restore_optimization(&mut self, blob: &[u8]) -> Result<(), JsValue> {
        self.ensure_grid()?;
        let mut rd = Reader::new(blob);
        rd.tag(DESIGN_MAGIC)?;
        let _version = rd.u32()?;
        let base_density = rd.f64()?;
        let iso_threshold = rd.f64()?;
        let eval_exp = rd.f64()?;
        let eval_coeff = rd.f64()?;
        let wall_mm = rd.f64()?;
        let tb_mm = rd.f64()?;
        let perimeters = rd.u32()?;
        let top_bottom_layers = rd.u32()?;
        let smooth_iters = rd.u32()?;
        let solid = rd.byte()? != 0;
        let binary = rd.byte()? != 0;
        let pat = String::from_utf8(rd.bytes()?).map_err(err)?;
        let solid_pattern = if pat.is_empty() { None } else { Some(pat) };
        let centers = rd.f64s()?;
        let x_binned = rd.f32s()?;
        let x_cont32 = rd.f32s()?;
        let bins = rd.bytes()?;

        let split = {
            let (grid, _) = self.grid.as_ref().unwrap();
            filasim_core::simp::classify_cells(grid, wall_mm, tb_mm, tb_mm, self.composite_skin)
        };
        let (skin, design, skin_frac) = (split.skin, split.design, split.skin_frac);
        if design.len() != x_binned.len()
            || design.len() != bins.len()
            || design.len() != x_cont32.len()
        {
            return Err(err(
                "the saved design no longer matches the model + settings — re-run the optimization",
            ));
        }
        let x_binned_f64: Vec<f64> = x_binned.iter().map(|&x| x as f64).collect();
        let x_cont: Vec<f64> = x_cont32.iter().map(|&x| x as f64).collect();
        let mut cell_density: std::collections::HashMap<u32, f64> = Default::default();
        let mut bin_of_cell: std::collections::HashMap<u32, u8> = Default::default();
        for (k, &c) in design.iter().enumerate() {
            cell_density.insert(c, x_binned_f64[k]);
            bin_of_cell.insert(c, bins[k]);
        }
        if solid {
            for &c in &skin {
                bin_of_cell.insert(c, 1);
            }
        }
        let (regions_raw, eps) = {
            let (grid, _) = self.grid.as_ref().unwrap();
            let mut regions_raw = Vec::new();
            for level in 1..centers.len() {
                let inside = |ci: usize| -> bool {
                    bin_of_cell.get(&(ci as u32)).is_some_and(|&b| b as usize >= level)
                };
                let mut r = filasim_core::bins::extract_region_smooth(grid, &inside, 0.4);
                if r.indices.is_empty() {
                    continue;
                }
                r.density = centers[level];
                regions_raw.push(r);
            }
            let eps = filasim_core::simp::build_eps(
                grid, &skin, &design, &skin_frac, &x_binned_f64, eval_exp, eval_coeff,
            );
            (regions_raw, eps)
        };
        let regions = filasim_core::pipeline::smooth_regions(
            &regions_raw,
            smooth_iters as usize,
            self.grid.as_ref().unwrap().0.h,
        );
        self.solution = None;
        self.solution_eps = Some(eps);
        // A restored design is evaluable under every load step too (DESIGN §13).
        self.opt_eps = self.solution_eps.clone();
        self.opt = Some(OptOutput {
            regions,
            regions_raw,
            base_density,
            cell_density,
            design_cells: design,
            x_cont,
            perimeters,
            top_bottom_layers,
            solid_pattern,
            summary: String::new(),
            solid,
            anchor_cells: skin,
            iso_threshold,
            bins,
            centers,
            x_binned,
            wall_mm,
            tb_mm,
            eval_exp,
            eval_coeff,
            smooth_iters,
            binary,
        });
        Ok(())
    }

    /// Inject a saved result's displacement (+eps) into the stash so the Results
    /// view can show/switch to it without re-solving. The grid must already be
    /// built (restore the design, or ensure the grid, first).
    pub fn restore_result(&mut self, id: &str, blob: &[u8]) -> Result<(), JsValue> {
        let (mx, my, mz, h, origin, active) = {
            let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid for result restore"))?;
            (grid.nx + 1, grid.ny + 1, grid.nz + 1, grid.h, grid.origin, active_nodes(grid))
        };
        let mut rd = Reader::new(blob);
        let u = rd.f32s()?;
        let eps_v = rd.f32s()?;
        if u.len() != mx * my * mz * 3 {
            return Err(err("saved result doesn't fit the grid — re-run the analysis"));
        }
        let eps = if eps_v.is_empty() { None } else { Some(eps_v) };
        let sol = Solution {
            u,
            mx,
            my,
            mz,
            h,
            origin,
            active,
            iterations: 0,
            rel_residual: 0.0,
            converged: true,
            residuals: Vec::new(),
        };
        self.results.insert(id.to_string(), StashedResult { sol, eps });
        Ok(())
    }

    /// Read design.bin + results/*.f32 out of a project zip and restore them.
    /// The model + its settings/orientation must already be set up. Returns JSON
    /// { restoredResults: [...ids], hasDesign }.
    pub fn restore_project(&mut self, project_bytes: &[u8]) -> Result<String, JsValue> {
        self.ensure_grid()?;
        let entries = filasim_core::zip::read_zip(project_bytes).map_err(|e| err(format!("{e:?}")))?;
        for (name, data) in &entries {
            if name == "design.bin" {
                self.restore_optimization(data)?;
            }
        }
        let mut restored: Vec<String> = Vec::new();
        for (name, data) in &entries {
            if let Some(rest) = name.strip_prefix("results/") {
                if let Some(id) = rest.strip_suffix(".f32") {
                    self.restore_result(id, data)?;
                    restored.push(id.to_string());
                }
            }
        }
        Ok(serde_json::json!({
            "restoredResults": restored,
            "hasDesign": self.opt.is_some(),
        })
        .to_string())
    }

    /// Sample a density field (cell -> density map) at every soup vertex,
    /// probing slightly inward and falling back to nearby solid cells.
    fn sample_cell_field(&self, grid: &VoxelGrid, field: &std::collections::HashMap<u32, f64>) -> Vec<f32> {
        let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
        let h = grid.h;
        let cell_at = |p: [f64; 3]| -> Option<usize> {
            let cx = ((p[0] - grid.origin[0]) / h).floor() as i64;
            let cy = ((p[1] - grid.origin[1]) / h).floor() as i64;
            let cz = ((p[2] - grid.origin[2]) / h).floor() as i64;
            if cx < 0 || cy < 0 || cz < 0 || cx >= nx as i64 || cy >= ny as i64 || cz >= nz as i64 {
                return None;
            }
            Some(((cz as usize) * ny + cy as usize) * nx + cx as usize)
        };
        let mut out = Vec::with_capacity(self.mesh.tris.len() * 3);
        for t in &self.mesh.tris {
            for v in 0..3 {
                let p = [t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64];
                // Search a small neighborhood: prefer a design cell, else skin.
                let mut val = 1.0f64;
                let mut found = false;
                'search: for r in 0..3i64 {
                    for dz in -r..=r {
                        for dy in -r..=r {
                            for dx in -r..=r {
                                let q = [
                                    p[0] + dx as f64 * h,
                                    p[1] + dy as f64 * h,
                                    p[2] + dz as f64 * h,
                                ];
                                if let Some(ci) = cell_at(q) {
                                    if let Some(&x) = field.get(&(ci as u32)) {
                                        val = x;
                                        found = true;
                                        break 'search;
                                    }
                                    if !found && grid.scale[ci] > 0.0 {
                                        found = true;
                                        val = 1.0; // skin
                                    }
                                }
                            }
                        }
                    }
                    if found {
                        break;
                    }
                }
                out.push(val as f32);
            }
        }
        out
    }

    /// Run the density optimization + binning + verification. `opts_json`
    /// is an OptimizeOpts object; budgetPct is the target mean INFILL
    /// density of the interior in percent — the number a user compares to a
    /// slicer's uniform infill setting (the solid skin is on top of it).
    /// Progress gets
    /// called per iteration with (jsonString, Float32Array vertexDensity,
    /// Float32Array skeletonPositions, Uint32Array skeletonIndices) — the
    /// skeleton is the evolving isosurface of cells denser than 40%.
    /// The perimeter count is also written into the
    /// exported 3MF as the PART's wall_loops (modifiers never pin walls), so
    /// the print matches the analysis assumption.
    pub fn optimize(
        &mut self,
        opts_json: &str,
        progress: &js_sys::Function,
    ) -> Result<String, JsValue> {
        self.ensure_grid()?;
        let opts: OptimizeOpts = serde_json::from_str(opts_json).map_err(err)?;
        // Phase pushes ride the SAME JS progress callback, as a single
        // `{"phase": ...}` JSON argument with no buffers — the worker forwards
        // them so the busy chip can narrate the otherwise-silent stages
        // (assembly, verification, baselines, region extraction).
        let emit_phase = |v: serde_json::Value| {
            let _ = progress.call1(&JsValue::NULL, &JsValue::from_str(&v.to_string()));
        };
        emit_phase(serde_json::json!({"phase": "assemble"}));
        let solid = opts.solid;
        // Calibrated pattern law: every stiffness EVALUATION uses this. In SOLID
        // topology mode the kept material is plain solid plastic, so the eval
        // law is LINEAR E = ρ — exact at the {void, solid} endpoints.
        let (eval_exp, eval_coeff) = if solid {
            (1.0, 1.0)
        } else {
            (opts.exponent.clamp(1.0, 3.5), opts.coeff.clamp(0.05, 2.0))
        };
        // Optimizer law: binary AND solid modes swap in SIMP penalization p=3 so
        // the continuous field converges toward the extremes ({floor, solid} for
        // binary, {void, solid} for topology) before quantization — quantizing a
        // physically-graded (n≈1.5) field straight to two levels throws away far
        // more.
        let (opt_exp, opt_coeff) =
            if opts.binary || solid { (3.0, 1.0) } else { (eval_exp, eval_coeff) };
        // SOLID mode lower bound is ersatz VOID (material removed), not a
        // printability floor; everything else uses the printable band.
        let floor = if solid { 1e-3 } else { (opts.floor_pct / 100.0).clamp(0.01, 0.5) };
        let cap = if solid { 1.0 } else { (opts.cap_pct / 100.0).clamp(floor + 0.05, 1.0) };
        let budget_pct = opts.budget_pct;
        let (perimeters, wall_mm) = resolve_wall(opts.perimeters, opts.line_width);
        let smooth_iters = (opts.smooth_iters as usize).min(60);
        let n_bins = (opts.n_bins as usize).clamp(2, 4);

        // Assemble + check before burning time. MULTI-LOAD (DESIGN §13): when
        // load cases are registered, the FIRST is the primary (drives the cache
        // + skin classification) and the rest are weighted extras; the optimizer
        // minimizes the weighted-sum compliance. No cases ⇒ single-load via
        // `self.bcs` (the byte-identical path).
        let (grid, levels) = self.grid.as_ref().unwrap();
        let primary_bcs: &[BcSpec] =
            if self.load_cases.is_empty() { &self.bcs } else { &self.load_cases[0].0 };
        // DESIGN §16 dec. 4: acceleration steps ARE optimized now. Each case's
        // summed accel drives a MASS-ONLY assemble — remote masses (F = m·a) and
        // the RBM load flag are realized, but the distributed self-weight FORCE
        // is left out (empty vfrac) and recomputed from the LIVE density every
        // SIMP iteration via `LoadSet::{primary,extra}_body`. No accel ⇒
        // `mass_only_body` is None ⇒ the plain surface-load assembly, byte-
        // identical to the pre-accel path.
        let primary_accel =
            if self.load_cases.is_empty() { self.accel } else { self.load_cases[0].2 };
        let asm = assemble(
            &self.mesh,
            grid,
            primary_bcs,
            Self::mass_only_body(primary_accel, self.density),
            &self.settings,
        )
        .map_err(err)?;
        let report = check_problem(grid, &asm);
        if !report.ok {
            return Err(err("model is under-constrained — fix the setup first (run Check)"));
        }
        let mut load_set = filasim_core::simp::LoadSet {
            primary_body: Self::opt_body_accel(primary_accel, self.density),
            ..Default::default()
        };
        if !self.load_cases.is_empty() {
            load_set.primary_weight = self.load_cases[0].1;
            for (case_bcs, w, case_accel) in &self.load_cases[1..] {
                let a = assemble(
                    &self.mesh,
                    grid,
                    case_bcs,
                    Self::mass_only_body(*case_accel, self.density),
                    &self.settings,
                )
                .map_err(err)?;
                if !check_problem(grid, &a).ok {
                    return Err(err("a load case is under-constrained — fix the setup first (run Check)"));
                }
                load_set.extra.push((a.problem, *w));
                load_set.extra_body.push(Self::opt_body_accel(*case_accel, self.density));
            }
        }
        // DESIGN §16 dec. 10: a self-weight-loaded optimization compares designs
        // that each carry their OWN true weight — surfaced with a fine-print note.
        let has_self_weight = load_set.has_self_weight();

        let params = OptimizeParams {
            // Budget = target mean INFILL density of the interior — the
            // engine clamps it to the printable [floor, cap] band.
            budget: (budget_pct / 100.0).clamp(0.01, 1.0),
            exponent: opt_exp,
            coeff: opt_coeff,
            floor,
            cap,
            wall_mm,
            top_mm: (opts.top_bottom_layers.min(20) as f64
                * opts.layer_height.clamp(0.04, 0.6))
            .min(5.0),
            bottom_mm: (opts.top_bottom_layers.min(20) as f64
                * opts.layer_height.clamp(0.04, 0.6))
            .min(5.0),
            composite_skin: self.composite_skin,
            symmetry: opts.symmetry.as_ref().and_then(|v| {
                if v.len() == 4 && (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]) > 1e-12 {
                    Some([v[0], v[1], v[2], v[3]])
                } else {
                    None
                }
            }),
            // Minimum member size: clamp to a sane mm range; the core maps it
            // to a filter radius and applies its own cell-radius bounds.
            min_member_mm: opts.min_member_mm.clamp(0.0, 10.0),
            // SOLID topology mode: material removal (no skin). The
            // self-supporting overhang filter is available in every mode (in
            // infill modes it shapes the dense regions; unsupported cells fall
            // to the floor density instead of void).
            solid_mode: solid,
            retain_bc: opts.retain_bc,
            self_support: opts.self_support,
            overhang_deg: opts.overhang_deg.clamp(0.0, 90.0),
            // Cap is a safety net; the change-based convergence criterion
            // normally stops the loop earlier. Raised from 40 — complex parts
            // can still be moving at 40.
            max_iter: 80,
            ..Default::default()
        };

        // ---- pipeline (core) ----
        // The orchestration — goal-match budget secant, binning, the binned
        // verification + uniform/solid reference solves, and region extraction —
        // lives in filasim_core::pipeline::run_optimization. This adapter resolves
        // params (above), marshals the per-iteration callback to JS, and
        // serializes the outcome. The grid/mesh borrows are disjoint fields from
        // the &mut solver_cache the pipeline takes.
        let goal_match = opts.goal == "match" && !solid;
        // Strength goal (§17): all three modes (dec. 7). Allowables are the
        // material's solid strengths; graded knockdown happens per cell in the
        // core (same Gibson–Ashby factor as the SF display, so "optimizer says
        // SF 2.0" matches the plot). Target clamped inside the meaningful band
        // (≥ SF_CAP would always be infeasible by construction).
        let strength_goal = (opts.goal == "strength").then(|| filasim_core::pipeline::StrengthGoal {
            target: opts.sf_target.clamp(1.0, 9.5),
            spec: filasim_core::strength::StrengthSpec {
                measure: filasim_core::strength::SfMeasure::parse(&opts.sf_measure)
                    .unwrap_or(filasim_core::strength::SfMeasure::Both),
                strength: self.strength,
                strength_z: self.strength_z,
                shear_z: self.shear_strength_z_eff(),
            },
        });
        let ref_frac = (budget_pct / 100.0).clamp(params.floor, params.cap);
        // Manual level override (binary {floor,1} or user densities): clamp,
        // sort, dedup once here; the pipeline takes the list verbatim.
        let levels_clean: Option<Vec<f64>> = opts.levels_pct.as_ref().and_then(|user| {
            if user.is_empty() {
                None
            } else {
                let mut l: Vec<f64> = user.iter().map(|&p| (p / 100.0).clamp(0.01, 1.0)).collect();
                l.sort_by(|a, b| a.partial_cmp(b).unwrap());
                l.dedup_by(|a, b| (*a - *b).abs() < 0.005);
                Some(l)
            }
        });
        let cfg = filasim_core::pipeline::PipelineCfg {
            eval: filasim_core::pipeline::EvalLaw { exp: eval_exp, coeff: eval_coeff },
            goal_match,
            strength: strength_goal,
            ref_frac,
            n_bins,
            levels_pct: levels_clean.as_deref(),
            smooth_iters,
        };
        let tris = &self.mesh.tris;
        let max_iter = params.max_iter;
        // Isosurface threshold for the live "shape emerging" preview.
        const SKEL_DENSITY: f64 = 0.4;
        let oc = filasim_core::pipeline::run_optimization(
            &mut self.solver_cache,
            grid,
            *levels,
            &asm.problem,
            &self.settings,
            &params,
            &cfg,
            &load_set,
            |upd, x_phys, design_cells| {
                let mut field: std::collections::HashMap<u32, f64> =
                    std::collections::HashMap::with_capacity(design_cells.len());
                for (k, &c) in design_cells.iter().enumerate() {
                    field.insert(c, x_phys[k]);
                }
                // Inline vertex sampling (cannot call &self methods here).
                let vd = sample_field_static(tris, grid, &field);
                // Evolving dense-core isosurface on the CONTINUOUS filtered field
                // (smooth level set; a per-cell binary indicator grows tent spikes
                // wherever a single cell crosses the threshold).
                let value = |ci: usize| field.get(&(ci as u32)).copied().unwrap_or(0.0);
                let mut skel = extract_iso(grid, &value, SKEL_DENSITY);
                taubin_smooth(&mut skel.positions, &skel.indices, 3);
                let skel_density = sample_points_static(&skel.positions, grid, &field);
                let json = serde_json::json!({
                    "iteration": upd.progress.iteration,
                    "maxIter": max_iter,
                    "pass": upd.pass,
                    "passes": upd.passes,
                    "budgetNow": upd.budget,
                    "compliance": upd.progress.compliance,
                    "massFrac": upd.progress.mass_frac,
                    "meanInfill": upd.progress.mean_infill,
                    "change": upd.progress.change,
                    "meanChange": upd.progress.mean_change,
                    "innerIters": upd.progress.inner_iters,
                    "innerRes": upd.progress.inner_residual,
                })
                .to_string();
                let args = js_sys::Array::of5(
                    &JsValue::from_str(&json),
                    &js_sys::Float32Array::from(vd.as_slice()),
                    &js_sys::Float32Array::from(skel.positions.as_slice()),
                    &js_sys::Uint32Array::from(skel.indices.as_slice()),
                    &js_sys::Float32Array::from(skel_density.as_slice()),
                );
                let _ = progress.apply(&JsValue::NULL, &args);
            },
            |phase| {
                use filasim_core::pipeline::PipelinePhase as P;
                emit_phase(match phase {
                    P::ReferenceSolve => serde_json::json!({"phase": "reference"}),
                    P::Preflight => serde_json::json!({"phase": "preflight"}),
                    P::SfEval => serde_json::json!({"phase": "sf_eval"}),
                    P::OptimizePass { pass, passes } => {
                        serde_json::json!({"phase": "optimize_pass", "pass": pass, "passes": passes})
                    }
                    P::Binning => serde_json::json!({"phase": "binning"}),
                    P::VerifySolve => serde_json::json!({"phase": "verify"}),
                    P::UniformSolve => serde_json::json!({"phase": "uniform"}),
                    P::SolidSolve => serde_json::json!({"phase": "solid_ref"}),
                    P::StressRecovery => serde_json::json!({"phase": "stress"}),
                    P::Regions => serde_json::json!({"phase": "regions"}),
                    P::Smoothing => serde_json::json!({"phase": "smoothing"}),
                });
            },
        )
        .map_err(err)?;
        // Everything after the pipeline (summary, stress eps, baseline stashes,
        // and the worker's region/vertex-buffer collection once this returns).
        emit_phase(serde_json::json!({"phase": "finalize"}));

        // ---- mass + summary (gram conversion needs the material density) ----
        let cell_vol = grid.h * grid.h * grid.h;
        let grams =
            |infill_vol: f64| (oc.vol_skin + oc.sum_f + infill_vol) * cell_vol * self.density * 1e6;
        let mass_part = grams(oc.infill_vol_binned);
        let mass_solid = grams(oc.w_sum);
        let n_solid = oc.vol_skin + oc.sum_f + oc.w_sum;
        let mass_frac = (oc.vol_skin + oc.sum_f + oc.infill_vol_binned) / n_solid;

        let bin_counts: Vec<usize> = (0..oc.centers.len())
            .map(|c| oc.bins.iter().filter(|&&b| b as usize == c).count())
            .collect();
        let mut summary_v = serde_json::json!({
            "iterations": oc.total_iters,
            "converged": oc.design_converged,
            "bins": oc.centers.iter().zip(&bin_counts).map(|(&d, &n)| serde_json::json!({
                "density": d, "cells": n,
            })).collect::<Vec<_>>(),
            "baseDensity": oc.centers[0],
            "regionCount": oc.regions.len(),
            "massGrams": mass_part,
            "massSolidGrams": mass_solid,
            "massFrac": mass_frac,
            // Achieved mean infill of the binned layout — the uniform-print
            // percentage the comparison card references ("vs X% uniform").
            "meanInfill": oc.mean_binned,
            // Requested budget after printable-floor/cap clamping.
            "targetInfill": oc.effective_budget,
            "stiffnessVsSolid": oc.c_solid / oc.c_binned,
            "gainVsUniform": oc.c_uniform / oc.c_binned - 1.0,
            "maxDisplacement": oc.max_disp,
            // Max |u| of the equal-mass uniform + fully-solid baseline solves,
            // for the Results-view provenance card. Baselines are stashed (and
            // selectable) for infill modes only — see the stash block below.
            "uniformMaxDisp": oc.max_disp_uniform,
            "solidMaxDisp": oc.max_disp_solid,
            "hasBaselines": !solid,
            // DESIGN §16 dec. 10: with acceleration active every design carries
            // its OWN self-weight (the fully-solid baseline is heavier and sags
            // more) — the comparison card shows a fine-print note.
            "selfWeight": has_self_weight,
            "binary": opts.binary,
            "solid": solid,
            "goal": if goal_match {
                "match"
            } else if strength_goal.is_some() {
                "strength"
            } else {
                "budget"
            },
            "passes": oc.pass_trace.len(),
        });
        if let Some(sg) = &strength_goal {
            // Strength summary (§17): target vs achieved, the pre-flight's
            // best-achievable ceiling, and the binding-region diagnosis the
            // infeasibility banner narrates (dec. 6).
            let o = summary_v.as_object_mut().unwrap();
            o.insert("sfTarget".into(), serde_json::json!(sg.target));
            o.insert("sfAchieved".into(), serde_json::json!(oc.sf_crit));
            o.insert("sfBest".into(), serde_json::json!(oc.sf_crit_cap));
            o.insert("sfFeasible".into(), serde_json::json!(oc.sf_feasible));
            o.insert(
                "sfMeasure".into(),
                serde_json::json!(match sg.spec.measure {
                    filasim_core::strength::SfMeasure::Material => "material",
                    filasim_core::strength::SfMeasure::Layer => "layer",
                    filasim_core::strength::SfMeasure::Both => "both",
                }),
            );
            o.insert("sfPerStep".into(), serde_json::json!(oc.sf_per_step));
            o.insert("bindingCellCount".into(), serde_json::json!(oc.binding_cells.len()));
            o.insert("bindingSkinShare".into(), serde_json::json!(oc.binding_skin_share));
            o.insert(
                "sfTrace".into(),
                serde_json::json!(oc.sf_trace
                    .iter()
                    .map(|&(b, sf)| serde_json::json!({"budget": b, "sf": sf}))
                    .collect::<Vec<_>>()),
            );
        }
        if goal_match {
            let o = summary_v.as_object_mut().unwrap();
            o.insert("refUniformPct".into(), serde_json::json!(ref_frac * 100.0));
            o.insert("targetCompliance".into(), serde_json::json!(oc.c_target));
            o.insert("achievedCompliance".into(), serde_json::json!(oc.c_binned));
            o.insert("matchDeviation".into(), serde_json::json!(oc.c_binned / oc.c_target - 1.0));
            // Mass of the uniform reference print (same skin, ref% interior).
            o.insert(
                "massUniformRefGrams".into(),
                serde_json::json!(grams(ref_frac * oc.design_cells.len() as f64)),
            );
            o.insert(
                "passTrace".into(),
                serde_json::json!(oc.pass_trace
                    .iter()
                    .map(|&(b, c)| serde_json::json!({"budget": b, "compliance": c}))
                    .collect::<Vec<_>>()),
            );
        }
        let summary = summary_v.to_string();

        // ---- deformed-view Solution + stress eps ----
        let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
        self.solution_eps = Some(oc.solution_eps);
        // Keep the optimized stiffness field for the per-step Results roster: it
        // survives load-step changes so solve_optimized can re-evaluate this one
        // design under every load step (DESIGN §13).
        self.opt_eps = self.solution_eps.clone();
        self.solution = Some(Solution {
            u: oc.u_binned.iter().map(|&v| v as f32).collect(),
            mx,
            my,
            mz,
            h: grid.h,
            origin: grid.origin,
            active: active_nodes(grid),
            // The optimizer's outer count of the final pass (what the progress UI
            // reports); residual/converged are the binned verification solve's.
            iterations: oc.design_iters,
            rel_residual: oc.verify_residual,
            converged: oc.verify_converged,
            residuals: Vec::new(),
        });

        // ---- stash the equal-mass uniform + solid baseline solutions ----
        // The pipeline already solved these for the comparison card and we now
        // keep their displacement fields (otherwise discarded). Stashing them
        // here lets the Results view switch to them instantly (same grid, so a
        // stash is just the u vector + its eps). Infill modes only: in solid
        // (Part Topo) mode the baselines live on the full envelope, not the
        // carved body, so they would be misleading — drop any stale ones.
        // The optimized solution itself is stashed by the caller (stash_result).
        if !solid {
            let active = active_nodes(grid);
            let (gh, gorigin) = (grid.h, grid.origin);
            let mk = |u: &[f64], eps: &[f32]| StashedResult {
                sol: Solution {
                    u: u.iter().map(|&v| v as f32).collect(),
                    mx,
                    my,
                    mz,
                    h: gh,
                    origin: gorigin,
                    active: active.clone(),
                    iterations: 0,
                    rel_residual: 0.0,
                    converged: true,
                    residuals: Vec::new(),
                },
                eps: Some(eps.to_vec()),
            };
            self.results.insert("uniform".into(), mk(&oc.u_uniform, &oc.eps_uniform));
            self.results.insert("solid".into(), mk(&oc.u_solid, &oc.eps_solid));
        } else {
            self.results.remove("uniform");
            self.results.remove("solid");
        }

        // ---- store the optimization result ----
        let mut field: std::collections::HashMap<u32, f64> = Default::default();
        for (i, &c) in oc.design_cells.iter().enumerate() {
            field.insert(c, oc.x_binned[i]);
        }
        self.opt = Some(OptOutput {
            regions: oc.regions,
            regions_raw: oc.regions_raw,
            base_density: oc.centers[0],
            cell_density: field,
            design_cells: oc.design_cells,
            x_cont: oc.x_cont,
            perimeters,
            top_bottom_layers: opts.top_bottom_layers.min(20),
            solid_pattern: opts.solid_pattern.clone(),
            summary: summary.clone(),
            solid,
            anchor_cells: oc.skin_cells,
            iso_threshold: 0.5,
            bins: oc.bins,
            centers: oc.centers,
            x_binned: oc.x_binned.iter().map(|&x| x as f32).collect(),
            wall_mm,
            tb_mm: params.top_mm,
            eval_exp,
            eval_coeff,
            smooth_iters: smooth_iters as u32,
            binary: opts.binary,
        });
        Ok(summary)
    }

    /// Re-apply constrained smoothing to the extracted regions (live preview
    /// of the smoothing slider). Affects display AND subsequent exports.
    pub fn resmooth_regions(&mut self, iters: u32) -> Result<(), JsValue> {
        let h = self.grid.as_ref().ok_or_else(|| err("no grid"))?.0.h;
        let opt = self.opt.as_mut().ok_or_else(|| err("no optimization result"))?;
        opt.regions = smooth_regions(&opt.regions_raw, (iters as usize).min(60), h);
        Ok(())
    }

    /// Re-extract the EXPORTED geometry from the CONTINUOUS optimized field at a
    /// new isosurface density `threshold` (0..1) — the post-run "fine-tune how
    /// the part looks" knob. SOLID mode rebuilds the single body (with the
    /// connected-keep anchored to the load/support regions); BINARY mode rebuilds
    /// the dense modifier region. The budget/density target is NOT touched — this
    /// only moves the level the shape is cut at. Affects display + exports; the
    /// regions are re-smoothed with `smooth_iters`. No-op for graded infill (its
    /// regions are bin sets, not a single isosurface level).
    pub fn set_iso_threshold(&mut self, threshold: f64, smooth_iters: u32) -> Result<(), JsValue> {
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        let opt = self.opt.as_mut().ok_or_else(|| err("no optimization result"))?;
        let t = threshold.clamp(0.05, 0.95);
        let n = grid.cell_count();
        // Build a BINARY keep indicator, then extract its watertight surface —
        // the same robust path the run uses (`extract_region_smooth` on a set,
        // iso 0.4, one indicator blur pass so no vertex knots/needle spikes).
        // Extracting the CONTINUOUS field at an arbitrary level instead produces
        // sliver/degenerate triangles wherever the level grazes the flat
        // boundary nodes (≈0.5), which smoothing then tears into holes/spikes.
        let mut inside = vec![false; n];
        if opt.solid {
            for &c in &opt.anchor_cells {
                inside[c as usize] = true;
            }
            let kept =
                solid_keep_bins(grid, &opt.anchor_cells, &opt.design_cells, &opt.x_cont, t);
            for (i, &c) in opt.design_cells.iter().enumerate() {
                if kept[i] == 1 {
                    inside[c as usize] = true;
                }
            }
        } else {
            // Binary: dense modifier = the design cells denser than the level.
            // No connected-keep (sparse infill supports interior dense islands).
            for (i, &c) in opt.design_cells.iter().enumerate() {
                if opt.x_cont[i] >= t {
                    inside[c as usize] = true;
                }
            }
        }
        let mut r = extract_region_smooth(grid, &|ci| inside[ci], 0.4);
        r.density = 1.0; // solid body / dense modifier
        opt.regions_raw = vec![r];
        opt.regions = smooth_regions(&opt.regions_raw, (smooth_iters as usize).min(60), grid.h);
        opt.iso_threshold = t;
        Ok(())
    }

    /// Isosurface of the final continuous density field at `threshold`
    /// (0..1): everything denser than the threshold. Returns
    /// [positions f32, indices u32, per-vertex density f32] for the
    /// cutaway view (colored with the density legend ramp).
    pub fn density_isosurface(&self, threshold: f64) -> Result<js_sys::Array, JsValue> {
        let opt = self.opt.as_ref().ok_or_else(|| err("no optimization result"))?;
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        let mut field: std::collections::HashMap<u32, f64> =
            std::collections::HashMap::with_capacity(opt.design_cells.len());
        for (i, &c) in opt.design_cells.iter().enumerate() {
            field.insert(c, opt.x_cont[i]);
        }
        // Part Topo: the frozen load/support cells are solid — show them in the
        // cutaway so the preview matches the exported body. NOT in infill modes,
        // where `anchor_cells` is the whole wall skin (it would hide the
        // interior — the cutaway must stay the dense core only).
        if opt.solid {
            for &c in &opt.anchor_cells {
                field.insert(c, 1.0);
            }
        }
        let t = threshold.clamp(0.0, 1.0);
        // Continuous level set of the filtered field — smooth by nature.
        let value = |ci: usize| field.get(&(ci as u32)).copied().unwrap_or(0.0);
        let mut r = extract_iso(grid, &value, t);
        taubin_smooth(&mut r.positions, &r.indices, 3);
        let density = sample_points_static(&r.positions, grid, &field);
        Ok(js_sys::Array::of3(
            &js_sys::Float32Array::from(r.positions.as_slice()),
            &js_sys::Uint32Array::from(r.indices.as_slice()),
            &js_sys::Float32Array::from(density.as_slice()),
        ))
    }

    pub fn region_count(&self) -> u32 {
        self.opt.as_ref().map_or(0, |o| o.regions.len() as u32)
    }

    pub fn region_density(&self, i: u32) -> f64 {
        self.opt.as_ref().and_then(|o| o.regions.get(i as usize)).map_or(0.0, |r| r.density)
    }

    pub fn region_positions(&self, i: u32) -> Vec<f32> {
        self.opt
            .as_ref()
            .and_then(|o| o.regions.get(i as usize))
            .map_or(Vec::new(), |r| r.positions.clone())
    }

    pub fn region_indices(&self, i: u32) -> Vec<u32> {
        self.opt
            .as_ref()
            .and_then(|o| o.regions.get(i as usize))
            .map_or(Vec::new(), |r| r.indices.clone())
    }

    /// Final binned density per soup vertex (density view).
    pub fn vertex_density(&self) -> Result<Vec<f32>, JsValue> {
        let opt = self.opt.as_ref().ok_or_else(|| err("no optimization result"))?;
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        Ok(self.sample_cell_field(grid, &opt.cell_density))
    }

    /// Per-cell scalar values of a result field on the padded grid (valid
    /// where grid.scale > 0), evaluated with the eps the solve actually used.
    fn cell_values(&self, kind: &str) -> Result<Vec<f32>, JsValue> {
        let sol =
            self.solution.as_ref().ok_or_else(|| err("no solution — run Solve or Optimize"))?;
        // Build-sim results carry a per-cell eigenstrain so stress comes out as
        // the residual print stress; analysis solves get eigen = 0 (unchanged).
        let (grid, eps_opt, eigen) = self.solution_grid()?;
        let scale_eps;
        let eps: &[f32] = match eps_opt {
            Some(e) => e,
            None => {
                scale_eps = grid.scale.clone();
                &scale_eps
            }
        };
        // Modulus / strength factor per cell. With `material_stress` on we strip
        // the geometric occupancy out of eps (material_factor → eps ÷ occupancy),
        // so a finite-cell cut cell reports its TRUE material stress instead of
        // E0·occ·ε — killing the curved-skin staircase stripes. Off = legacy
        // (eps exactly as the solve used it). The SAME factor feeds the SF
        // allowable below, so the safety factor cancels and is identical either
        // way — this toggle only ever moves the displayed stress magnitude.
        let occ_free;
        let factor: &[f32] = if self.material_stress {
            occ_free = material_factor(grid, eps);
            &occ_free
        } else {
            eps
        };
        // Safety factors: allowable = strength × the SAME relative factor as
        // the stiffness (Gibson–Ashby, first order; the skin carries full
        // strength). "sfm" checks the material against σ_vM; "sfz" checks
        // layer adhesion via the DESIGN §15 interaction of tension across the
        // layers (σzz > 0 only — compression doesn't delaminate) and shear
        // along the layer plane: (⟨σzz⟩₊/Sᵗᶻ)² + (τ/Sˢᶻ)² = 1/SF²;
        // "sf" is the per-cell worst of both. All capped at SF_CAP.
        let sf_material = || -> Vec<f32> {
            let mut c = cell_field_eigen(
                grid, &sol.u, self.settings.e0, self.settings.nu, factor, eigen, FieldKind::VonMises,
            );
            for (i, v) in c.iter_mut().enumerate() {
                let allow = self.strength as f32 * factor[i];
                *v = (allow / v.max(1e-9)).min(SF_CAP);
            }
            c
        };
        let sf_layer = || -> Result<Vec<f32>, JsValue> {
            let field = |name: &str| -> Result<Vec<f32>, JsValue> {
                let k = FieldKind::parse(name).ok_or_else(|| err("stress field missing"))?;
                Ok(cell_field_eigen(grid, &sol.u, self.settings.e0, self.settings.nu, factor, eigen, k))
            };
            let mut c = field("szz")?;
            let tyz = field("syz")?;
            let tzx = field("szx")?;
            let ss = self.shear_strength_z_eff() as f32;
            for i in 0..c.len() {
                let allow_t = (self.strength_z as f32 * factor[i]).max(1e-9);
                let allow_s = (ss * factor[i]).max(1e-9);
                let sn = (c[i] / allow_t).max(0.0); // tension only
                let tau2 = (tyz[i] * tyz[i] + tzx[i] * tzx[i]) / (allow_s * allow_s);
                let q = sn * sn + tau2;
                c[i] = if q <= 1e-18 { SF_CAP } else { (1.0 / q.sqrt()).min(SF_CAP) };
            }
            Ok(c)
        };
        Ok(match kind {
            "sfm" => sf_material(),
            "sfz" => sf_layer()?,
            "sf" => {
                let mut a = sf_material();
                let b = sf_layer()?;
                for (va, &vb) in a.iter_mut().zip(&b) {
                    *va = va.min(vb);
                }
                a
            }
            _ => {
                let k = FieldKind::parse(kind).ok_or_else(|| err("unknown result field"))?;
                cell_field_eigen(grid, &sol.u, self.settings.e0, self.settings.nu, factor, eigen, k)
            }
        })
    }

    /// Stress/strain scalar per soup vertex, from the current solution.
    /// Kinds: "vm" | "sxx" | "syy" | "szz" | "sxy" | "syz" | "szx" (MPa),
    /// "evm" | "exx" | "eyy" | "ezz" | "gxy" | "gyz" | "gzx" (strain), and
    /// "sf" — safety factor σ_allow/σ_vM, where the allowable of graded
    /// infill scales with the SAME relative factor as its stiffness
    /// (Gibson–Ashby strength tracks stiffness to first order; the skin
    /// carries the full tensile strength). Capped at `SF_CAP`.
    /// Evaluated at cell centers with the eps the solve actually used.
    /// With smooth_stress on, the per-cell field is recovered to the nodes
    /// (volume-averaged) and interpolated AT the surface vertex — the
    /// boundary cells' staircase noise averages out instead of being copied
    /// onto the surface.
    pub fn result_field(&self, kind: &str) -> Result<Vec<f32>, JsValue> {
        let cells = self.cell_values(kind)?;
        let (grid, _eps, _eigen) = self.solution_grid()?;
        if self.smooth_stress {
            let nodal = recover_nodal(grid, &cells);
            Ok(sample_nodal_values(&self.mesh.tris, grid, &nodal, &cells))
        } else {
            Ok(sample_cell_values(&self.mesh.tris, grid, &cells))
        }
    }

    /// Voxel-hull result geometry: the analysis mesh with EXACT nodal
    /// displacements (hull vertices ARE grid nodes — no surface sampling
    /// like the STL view). Returns [positions f32 (9/tri), displacements
    /// f32 (9/tri), edges f32 (6/segment), edge displacements f32].
    pub fn voxel_results(&self) -> Result<js_sys::Array, JsValue> {
        let sol =
            self.solution.as_ref().ok_or_else(|| err("no solution — run Solve or Optimize"))?;
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        let (tris, edges) = grid.surface_mesh();
        let tri_disp = node_displacements(&tris, grid, sol);
        let edge_disp = node_displacements(&edges, grid, sol);
        Ok(js_sys::Array::of4(
            &js_sys::Float32Array::from(tris.as_slice()),
            &js_sys::Float32Array::from(tri_disp.as_slice()),
            &js_sys::Float32Array::from(edges.as_slice()),
            &js_sys::Float32Array::from(edge_disp.as_slice()),
        ))
    }

    /// Result field on the voxel hull: one value per hull vertex (3 per
    /// triangle). Default: each triangle carries its OWNING CELL's value —
    /// crisp per-cell coloring. With smooth_stress on, every hull vertex IS
    /// a grid node, so it carries the recovered nodal value — the hull
    /// shades smoothly instead of flat per cell. Kinds as in `result_field`.
    pub fn voxel_result_field(&self, kind: &str) -> Result<Vec<f32>, JsValue> {
        let cells = self.cell_values(kind)?;
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        let (tris, _edges, cell_of_tri) = grid.surface_mesh_where(&|_| true);
        if self.smooth_stress {
            let nodal = recover_nodal(grid, &cells);
            let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
            let (h, o) = (grid.h, grid.origin);
            let mut out = Vec::with_capacity(tris.len() / 3);
            for p in tris.chunks_exact(3) {
                // Hull vertices lie exactly on grid nodes; nodes of a solid
                // cell always have a recovered value.
                let x = (((p[0] as f64 - o[0]) / h).round() as usize).min(mx - 1);
                let y = (((p[1] as f64 - o[1]) / h).round() as usize).min(my - 1);
                let z = (((p[2] as f64 - o[2]) / h).round() as usize).min(mz - 1);
                let v = nodal[(z * my + y) * mx + x];
                out.push(if v.is_nan() { 0.0 } else { v });
            }
            return Ok(out);
        }
        let mut out = Vec::with_capacity(cell_of_tri.len() * 3);
        for &ci in &cell_of_tri {
            let v = cells[ci as usize];
            out.extend_from_slice(&[v, v, v]);
        }
        Ok(out)
    }

    /// DESIGN §15 — begin an orientation sweep: build the compact per-cell
    /// stress-tensor field (one entry per surviving cell across all folded
    /// results) and the pitch/roll hemisphere grid. `ids` are result-stash ids
    /// to fold worst-case (multi-load-step); an EMPTY list folds the current
    /// solution only. Every folded result must live on the current analysis
    /// grid (structural solves — the build sim is out of scope). The layer
    /// criterion is the §15 tension+shear interaction against `strength_z` /
    /// the effective interlayer shear strength; allowables scale with the
    /// solve's per-cell stiffness factor exactly like the SF plots. A fixed
    /// ring around rigid-constraint nodes (Fixed / Frictionless /
    /// Displacement — NOT loads, NOT the compliant Elastic foundation) is
    /// excluded from the scored value but still reported in the all-cells
    /// value (§15 dec. 3). Returns JSON
    /// `{ n, stepDeg, pixels, cellsSeen, cellsKept, scoredCells }`;
    /// pixel index = i_pitch·n + i_roll, both axes −90°..+90°.
    /// Resolve the results an orientation sweep folds: stash ids, or the
    /// current solution for an empty list. Each must live on the current
    /// analysis grid (structural solves — the build sim is out of scope).
    /// Returns (solution, eps-with-scale-fallback) pairs.
    fn sweep_folds(&self, ids: &js_sys::Array) -> Result<Vec<(&Solution, &[f32])>, JsValue> {
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid — run Solve first"))?;
        let dims_ok = |sol: &Solution| {
            sol.mx == grid.nx + 1 && sol.my == grid.ny + 1 && sol.mz == grid.nz + 1
        };
        let mut folds: Vec<(&Solution, &[f32])> = Vec::new();
        if ids.length() == 0 {
            let sol = self
                .solution
                .as_ref()
                .ok_or_else(|| err("no solution — run Solve or Optimize"))?;
            if !dims_ok(sol) {
                return Err(err("orientation sweep needs a structural result on the analysis grid"));
            }
            folds.push((sol, self.solution_eps.as_deref().unwrap_or(grid.scale.as_slice())));
        } else {
            for id in ids.iter() {
                let id =
                    id.as_string().ok_or_else(|| err("orientation sweep: ids must be strings"))?;
                let r = self
                    .results
                    .get(&id)
                    .ok_or_else(|| err(&format!("unknown result '{id}'")))?;
                if !dims_ok(&r.sol) {
                    return Err(err(&format!("result '{id}' predates the current grid")));
                }
                folds.push((&r.sol, r.eps.as_deref().unwrap_or(grid.scale.as_slice())));
            }
        }
        Ok(folds)
    }

    /// Constraint-ring mask for the current grid + BCs (rigid constraints
    /// only — see `orientation_sweep_begin`).
    fn constraint_ring(&self) -> Result<Vec<bool>, JsValue> {
        use filasim_core::orient;
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        // Purely geometric — only the per-BC node lists are read, which don't
        // depend on any body load, so skip it entirely.
        let asm = assemble(&self.mesh, grid, &self.bcs, None, &self.settings)
            .map_err(err)?;
        let constraint_nodes: Vec<&[u32]> = self
            .bcs
            .iter()
            .zip(&asm.bc_nodes)
            .filter(|(bc, _)| {
                matches!(
                    bc.kind,
                    BcKind::Fixed | BcKind::Frictionless | BcKind::Displacement(_, _)
                )
            })
            .map(|(_, nodes)| nodes.as_slice())
            .collect();
        Ok(orient::constraint_ring_mask(grid, &constraint_nodes))
    }

    /// Per-CELL layer-adhesion SF for build direction `(a, b, c)`, min-folded
    /// across the `ids` result set (see `sweep_folds`).
    fn layer_sf_cells_folded(
        &self,
        a: f64,
        b: f64,
        c: f64,
        ids: &js_sys::Array,
    ) -> Result<Vec<f32>, JsValue> {
        use filasim_core::orient;
        let folds = self.sweep_folds(ids)?;
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        let len = (a * a + b * b + c * c).sqrt().max(1e-12);
        let n_dir = [a / len, b / len, c / len];
        let strength_s = self.shear_strength_z_eff();
        let mut cells: Option<Vec<f32>> = None;
        for (sol, eps) in folds {
            let f = orient::layer_sf_cells(
                grid,
                &sol.u,
                self.settings.e0,
                self.settings.nu,
                eps,
                [0.0; 3],
                self.strength_z,
                strength_s,
                n_dir,
                SF_CAP,
            );
            cells = Some(match cells {
                None => f,
                Some(mut acc) => {
                    for (av, fv) in acc.iter_mut().zip(&f) {
                        *av = av.min(*fv);
                    }
                    acc
                }
            });
        }
        Ok(cells.unwrap())
    }

    /// Per-vertex layer-adhesion SF for ONE build direction `n = (a, b, c)` —
    /// the preview recolor when a heatmap pixel is clicked. Folds the same
    /// result set as `orientation_sweep_begin` (elementwise min across load
    /// steps), sampled to the display surface exactly like `result_field`
    /// (smooth or per-cell). NOT ring-masked — the smoothed STL surface
    /// hides nothing (greying happens on the per-cell voxel hull, below).
    pub fn layer_sf_field(
        &self,
        a: f64,
        b: f64,
        c: f64,
        ids: js_sys::Array,
    ) -> Result<Vec<f32>, JsValue> {
        let cells = self.layer_sf_cells_folded(a, b, c, &ids)?;
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        if self.smooth_stress {
            let nodal = recover_nodal(grid, &cells);
            Ok(sample_nodal_values(&self.mesh.tris, grid, &nodal, &cells))
        } else {
            Ok(sample_cell_values(&self.mesh.tris, grid, &cells))
        }
    }

    /// Voxel-hull variant of `layer_sf_field`: one value per hull vertex
    /// (the owning cell's — crisp per-cell, like `voxel_result_field`).
    /// Constraint-ring cells (excluded from the sweep SCORE, §15 dec. 3)
    /// return NaN — the viewer paints them flat GREY, so the voxel view
    /// shows exactly which cells the score ignores.
    pub fn layer_sf_voxel_field(
        &self,
        a: f64,
        b: f64,
        c: f64,
        ids: js_sys::Array,
    ) -> Result<Vec<f32>, JsValue> {
        let mut cells = self.layer_sf_cells_folded(a, b, c, &ids)?;
        let ring = self.constraint_ring()?;
        for (v, &m) in cells.iter_mut().zip(&ring) {
            if m {
                *v = f32::NAN;
            }
        }
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        let (_tris, _edges, cell_of_tri) = grid.surface_mesh_where(&|_| true);
        let mut out = Vec::with_capacity(cell_of_tri.len() * 3);
        for &ci in &cell_of_tri {
            let v = cells[ci as usize];
            out.extend_from_slice(&[v, v, v]);
        }
        Ok(out)
    }

    pub fn orientation_sweep_begin(
        &mut self,
        ids: js_sys::Array,
        step_deg: f64,
    ) -> Result<String, JsValue> {
        use filasim_core::orient;
        self.sweep = None;
        let step = step_deg.clamp(1.0, 45.0);
        // Constraint-ring mask from the CURRENT BCs (per-BC node lists).
        let ring = self.constraint_ring()?;
        let folds = self.sweep_folds(&ids)?;
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid — run Solve first"))?;

        let strength_s = self.shear_strength_z_eff();
        let mut builder = orient::SweepBuilder::new(SF_CAP);
        // Orientation-independent MATERIAL SF floor across the same folds —
        // the readout's "orientation won't save this part" number (§15 dec. 5).
        let mut sfm_min = SF_CAP;
        for (sol, eps) in &folds {
            builder.add_result(
                grid,
                &sol.u,
                self.settings.e0,
                self.settings.nu,
                eps,
                [0.0; 3],
                self.strength_z,
                strength_s,
                &ring,
            );
            let vm = cell_field_eigen(
                grid, &sol.u, self.settings.e0, self.settings.nu, eps, [0.0; 3],
                FieldKind::VonMises,
            );
            for (i, v) in vm.iter().enumerate() {
                if eps[i] > 0.0 && *v > 1e-9 {
                    sfm_min = sfm_min.min((self.strength as f32 * eps[i] / v).min(SF_CAP));
                }
            }
        }
        let field = builder.finish();
        let (n, dirs) = orient::hemisphere_grid(step);
        let meta = serde_json::json!({
            "n": n,
            "stepDeg": step,
            "pixels": n * n,
            "cellsSeen": field.cells_seen,
            "cellsKept": field.cells_kept(),
            "scoredCells": field.scored_cells(),
            "materialSfMin": sfm_min,
        });
        self.sweep = Some(SweepCtx { field, dirs });
        Ok(meta.to_string())
    }

    /// Sweep pixels `[start, start+count)` of the flattened grid started by
    /// `orientation_sweep_begin`. Returns `[scored, all]` — two Float32Arrays
    /// of min layer-adhesion SF per pixel: `scored` excludes the constraint
    /// ring, `all` hides nothing; both capped at SF_CAP. Chunk the calls so
    /// the worker can post progress between them.
    pub fn orientation_sweep_rows(&self, start: u32, count: u32) -> Result<js_sys::Array, JsValue> {
        let ctx = self
            .sweep
            .as_ref()
            .ok_or_else(|| err("no active sweep — call orientation_sweep_begin"))?;
        let a = (start as usize).min(ctx.dirs.len());
        let b = (a + count as usize).min(ctx.dirs.len());
        let out = ctx.field.sweep(&ctx.dirs[a..b]);
        let scored: Vec<f32> = out.iter().map(|v| v.0).collect();
        let all: Vec<f32> = out.iter().map(|v| v.1).collect();
        Ok(js_sys::Array::of2(
            &js_sys::Float32Array::from(scored.as_slice()),
            &js_sys::Float32Array::from(all.as_slice()),
        ))
    }

    /// Drop the sweep context built by `orientation_sweep_begin`.
    pub fn orientation_sweep_end(&mut self) {
        self.sweep = None;
    }

    /// Volumetric section payload for the CAD-style capped section view: the
    /// recovered NODAL scalar field over the FULL solution grid (void-adjacent
    /// nodes back-filled from valid neighbors so the cap can interpolate right
    /// up to the skin), the padded nodal displacement field (3/node, straight
    /// from the solve — the cap un-deforms its sample point with it in the
    /// exaggerated view), the grid layout, and the interior (solid-cell)
    /// min/max with their cell-center locations — the true field extremes,
    /// which for a skin+infill part often sit INSIDE (perimeter/infill
    /// interface), not on the surface. Kinds as in `result_field`;
    /// displacement kinds ("u"|"ux"|"uy"|"uz") return an EMPTY values array
    /// (the cap shader derives them from `disp` directly) and NaN range.
    /// Returns [values f32 (N|0), disp f32 (3N), meta f64
    /// [mx,my,mz, ox,oy,oz, h, min,max, minx,miny,minz, maxx,maxy,maxz]].
    pub fn section_volume(&self, kind: &str) -> Result<js_sys::Array, JsValue> {
        let sol =
            self.solution.as_ref().ok_or_else(|| err("no solution — run Solve or Optimize"))?;
        let (grid, _, _) = self.solution_grid()?;
        let (mx, my, mz) = (grid.nx + 1, grid.ny + 1, grid.nz + 1);
        let mut meta = [f64::NAN; 15];
        meta[0] = mx as f64;
        meta[1] = my as f64;
        meta[2] = mz as f64;
        meta[3] = grid.origin[0];
        meta[4] = grid.origin[1];
        meta[5] = grid.origin[2];
        meta[6] = grid.h;
        let disp_kind = matches!(kind, "u" | "ux" | "uy" | "uz");
        let values: Vec<f32> = if disp_kind {
            Vec::new()
        } else {
            let cells = self.cell_values(kind)?;
            let (nx, ny) = (grid.nx, grid.ny);
            // Interior extremes over FULL cells only (occupancy ≈ 1, i.e.
            // entirely inside the part). Boundary cut cells are excluded on
            // purpose: their centers can lie OUTSIDE the mesh (the marker
            // would float in air) and with material-stress decoupling their
            // values are the SURFACE story, which the surface plot already
            // tells. The interior story is the full cells — perimeter/infill
            // interface and inward.
            let mut vmin = f32::INFINITY;
            let mut vmax = f32::NEG_INFINITY;
            let (mut cmin, mut cmax) = (0usize, 0usize);
            for (ci, (&v, &s)) in cells.iter().zip(&grid.scale).enumerate() {
                if s < 0.999 {
                    continue;
                }
                if v < vmin {
                    vmin = v;
                    cmin = ci;
                }
                if v > vmax {
                    vmax = v;
                    cmax = ci;
                }
            }
            if vmin.is_finite() {
                let center = |ci: usize| {
                    let (cx, cy, cz) = (ci % nx, (ci / nx) % ny, ci / (nx * ny));
                    [
                        grid.origin[0] + (cx as f64 + 0.5) * grid.h,
                        grid.origin[1] + (cy as f64 + 0.5) * grid.h,
                        grid.origin[2] + (cz as f64 + 0.5) * grid.h,
                    ]
                };
                meta[7] = vmin as f64;
                meta[8] = vmax as f64;
                meta[9..12].copy_from_slice(&center(cmin));
                meta[12..15].copy_from_slice(&center(cmax));
            }
            let mut nodal = recover_nodal(grid, &cells);
            fill_nodal_gaps(&mut nodal, mx, my, mz);
            nodal
        };
        Ok(js_sys::Array::of3(
            &js_sys::Float32Array::from(values.as_slice()),
            &js_sys::Float32Array::from(sol.u.as_slice()),
            &js_sys::Float64Array::from(meta.as_slice()),
        ))
    }

    /// Project 3MF with part + modifiers for the chosen slicer flavor:
    /// "orca" (default), "bambu", or "prusa". The part mesh is the ORIGINAL
    /// import tessellation (display subdivision stays internal); the part
    /// carries the perimeter count the optimization assumed. Bambu Studio
    /// (>= 2.06) renamed the "rectilinear" pattern value to "zig-zag"
    /// (still displayed as Rectilinear), so the bambu flavor maps it —
    /// otherwise every project load pops a "values replaced" dialog.
    /// `thumbnail` are PNG bytes for the plate preview (empty = embedded
    /// placeholder). Only the Orca/Bambu flavor carries a plate thumbnail.
    pub fn export_3mf(&self, slicer: &str, thumbnail: &[u8]) -> Result<Vec<u8>, JsValue> {
        let opt = self.opt.as_ref().ok_or_else(|| err("no optimization result — run optimize first"))?;
        let part = weld(&self.mesh_orig);
        let name = if self.name.is_empty() { "part" } else { &self.name };
        let pattern = opt.solid_pattern.as_deref();
        let thumb = if thumbnail.is_empty() { None } else { Some(thumbnail) };
        // A graded level pinned at 100% slices as solid, and gyroid & friends
        // fill poorly at full density — that modifier alone gets rectilinear
        // (the other levels keep the profile's own sparse pattern).
        Ok(match slicer {
            "prusa" => filasim_core::threemf::export_prusa_3mf(
                name,
                &part,
                &opt.regions,
                opt.base_density,
                opt.perimeters,
                opt.top_bottom_layers,
                pattern,
                Some("rectilinear"),
            ),
            s => export_orca_3mf(
                name,
                &part,
                &opt.regions,
                opt.base_density,
                opt.perimeters,
                opt.top_bottom_layers,
                pattern.map(|p| if s == "bambu" && p == "rectilinear" { "zig-zag" } else { p }),
                Some(if s == "bambu" { "zig-zag" } else { "rectilinear" }),
                thumb,
            ),
        })
    }

    /// Standalone colored 3MF of the active result field, painted into
    /// `steps` discrete bands on the ORIGINAL undeformed surface. The field
    /// `kind` matches `result_field` (the on-screen field); `lo`/`hi` are the
    /// active contour min/max; `colors_json` is a JSON array of `#RRGGBB`
    /// strings (one per band, low value first) the caller sampled from the
    /// contour ramp. Each display triangle is CUT along the field's iso-lines at
    /// the band boundaries (`isoband_cut`), so every emitted sub-triangle lies
    /// wholly inside one band — razor-sharp, watertight transitions instead of
    /// the streaky whole-triangle banding. Each piece is painted with its band's
    /// filament (Bambu/Orca `paint_color`); N filaments in the embedded
    /// `project_settings.config` carry the colors. No infill modifiers.
    pub fn export_color_3mf(
        &self,
        kind: &str,
        lo: f32,
        hi: f32,
        steps: u32,
        colors_json: &str,
        thumbnail: &[u8],
    ) -> Result<Vec<u8>, JsValue> {
        let steps = steps.max(1);
        let colors: Vec<String> =
            serde_json::from_str(colors_json).map_err(|e| err(&e.to_string()))?;
        if colors.len() != steps as usize {
            return Err(err("colors length must equal steps"));
        }
        // Paint the ORIGINAL CAD tessellation (the display mesh is non-watertight
        // — T-junctions from render subdivision — so iso-cutting it cracks). But
        // mesh_orig's CCW soup has huge CAD slivers; iso-cutting those gives crude
        // straight band edges across each big triangle. So first weld it and
        // CONFORMINGLY refine to a fine edge length: watertight AND fine, so the
        // band lines follow the field smoothly. Then sample the field at the
        // refined corners and cut along the iso-lines.
        let welded = weld(&self.mesh_orig);
        let diag = {
            let (mut lo3, mut hi3) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
            for v in &welded.vertices {
                for d in 0..3 {
                    lo3[d] = lo3[d].min(v[d]);
                    hi3[d] = hi3[d].max(v[d]);
                }
            }
            ((hi3[0] - lo3[0]).powi(2) + (hi3[1] - lo3[1]).powi(2) + (hi3[2] - lo3[2]).powi(2)).sqrt()
        };
        // Phase 1: conformingly refine everywhere to a coarse-ish edge so the
        // field is well sampled and there are no big triangles. Phase 2:
        // FIELD-ADAPTIVE — refine only edges whose endpoints fall in different
        // bands (i.e. a band seam runs near them) down to a much finer edge, so
        // the iso-cut band lines are smooth exactly where the eye looks, while
        // flat-field interiors stay coarse. Both phases use the SAME conforming
        // red-green machinery (shared edge midpoints) → still watertight.
        let coarse = (diag / 120.0).max(1e-4);
        let fine_edge = (diag / 240.0).max(1e-4);
        let fine2 = fine_edge * fine_edge;
        const MAX_TRIS: usize = 700_000;
        let (mut mesh, met) =
            filasim_core::threemf::subdivide_to_edge_checked(&welded, coarse, MAX_TRIS);
        if !met {
            console_warn("color-3MF: surface too large to refine fully; band edges may coarsen");
        }
        for _ in 0..7 {
            if mesh.triangles.len().saturating_mul(4) >= MAX_TRIS {
                break;
            }
            // Band index of each refined vertex (re-sampled every round so new
            // midpoints get their band; reuses the tested per-corner sampler).
            let mut pts: Vec<f32> = Vec::with_capacity(mesh.vertices.len() * 3);
            for v in &mesh.vertices {
                pts.extend_from_slice(v);
            }
            let vfield = self.field_at_points(kind, &pts)?;
            let band: Vec<u32> = vfield
                .iter()
                .map(|&s| filasim_core::threemf::band_index(lo, hi, steps, s))
                .collect();
            let (next, split) = filasim_core::threemf::subdivide_pass(&mesh, |v, a, b| {
                if band[a as usize] == band[b as usize] {
                    return false; // not on a band seam
                }
                let (p, q) = (v[a as usize], v[b as usize]);
                (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2) > fine2
            });
            if !split {
                break;
            }
            mesh = next;
        }
        // Per-vertex field on the refined indexed mesh (one value per shared
        // vertex → consistent across edges). Cut along the band iso-lines with
        // shared per-edge crossings: sharp AND exactly watertight bands.
        let mut pts: Vec<f32> = Vec::with_capacity(mesh.vertices.len() * 3);
        for v in &mesh.vertices {
            pts.extend_from_slice(v);
        }
        let vscalars = self.field_at_points(kind, &pts)?;
        let (cut_pos, cut_band) = filasim_core::threemf::isoband_cut_indexed(
            &mesh.vertices,
            &mesh.triangles,
            &vscalars,
            lo,
            hi,
            steps,
        );
        let name = if self.name.is_empty() { "part" } else { &self.name };
        let thumb = if thumbnail.is_empty() { None } else { Some(thumbnail) };
        Ok(filasim_core::threemf::export_color_3mf(name, &cut_pos, &cut_band, &colors, thumb))
    }

    /// Per-corner scalar field (3 values/triangle) sampled at an arbitrary
    /// surface `tris` (9 floats/tri). Mirrors `result_field` but on a supplied
    /// mesh: displacement kinds come from the nodal displacement (the viewer
    /// colors these client-side), all others from the per-cell engine field
    /// (recovered to nodes + interpolated when `smooth_stress` is on).
    fn field_on_tris(&self, kind: &str, tris: &[[f32; 9]]) -> Result<Vec<f32>, JsValue> {
        let disp_comp = match kind {
            "u" => Some(-1i32),
            "ux" => Some(0),
            "uy" => Some(1),
            "uz" => Some(2),
            _ => None,
        };
        if let Some(comp) = disp_comp {
            let sol =
                self.solution.as_ref().ok_or_else(|| err("no solution — run Solve or Optimize"))?;
            let mut f = Vec::with_capacity(tris.len() * 3);
            for t in tris {
                for v in 0..3 {
                    let p = [t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64];
                    let u = sol.sample_displacement(p);
                    let val = if comp < 0 {
                        (u[0] * u[0] + u[1] * u[1] + u[2] * u[2]).sqrt() as f32
                    } else {
                        u[comp as usize] as f32
                    };
                    f.push(val);
                }
            }
            return Ok(f);
        }
        let cells = self.cell_values(kind)?;
        let (grid, _) = self.grid.as_ref().ok_or_else(|| err("no grid"))?;
        if self.smooth_stress {
            let nodal = recover_nodal(grid, &cells);
            Ok(sample_nodal_values(tris, grid, &nodal, &cells))
        } else {
            Ok(sample_cell_values(tris, grid, &cells))
        }
    }

    /// The field scalar at each xyz point (3 floats/point). Reuses the tested
    /// per-corner sampler (`field_on_tris`) by packing points into dummy
    /// triangles — each corner is sampled independently, so the grouping is
    /// irrelevant. Used by the color-3MF field-adaptive refinement to band each
    /// refined vertex.
    fn field_at_points(&self, kind: &str, points: &[f32]) -> Result<Vec<f32>, JsValue> {
        let n = points.len() / 3;
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut padded = points.to_vec();
        // Pad to a whole number of 3-corner triangles.
        while (padded.len() / 3) % 3 != 0 {
            padded.extend_from_slice(&[0.0, 0.0, 0.0]);
        }
        let tris: Vec<[f32; 9]> = padded
            .chunks_exact(9)
            .map(|c| {
                let mut t = [0.0f32; 9];
                t.copy_from_slice(c);
                t
            })
            .collect();
        let mut vals = self.field_on_tris(kind, &tris)?;
        vals.truncate(n);
        Ok(vals)
    }

    /// Voxel mesh for the Mesh view, optionally cut by a plane: cells whose
    /// CENTER satisfies n·c + constant < 0 are dropped entirely, exposing the
    /// interior cells (voxel-true section — skin thickness inspectable)
    /// instead of a planar cut. Returns [positions f32 (9/tri),
    /// density f32 (3/tri, the cell's relative material density in [0,1]:
    /// skin = 1, interior = its infill ratio — the OPTIMIZED per-cell
    /// density when an optimization result exists, else `infill_pct` —
    /// composite surface cells blend the two by their wall fraction),
    /// edges f32].
    pub fn voxel_mesh_cut(
        &mut self,
        cut: bool,
        nx: f64,
        ny: f64,
        nz: f64,
        constant: f64,
        wall_mm: f64,
        top_bottom_mm: f64,
        infill_pct: f64,
    ) -> Result<js_sys::Array, JsValue> {
        self.ensure_grid()?;
        let (grid, _) = self.grid.as_ref().unwrap();
        let tb = top_bottom_mm.clamp(0.0, 5.0);
        let split = filasim_core::simp::classify_cells(
            grid,
            wall_mm.clamp(WALL_MM.0, WALL_MM.1),
            tb,
            tb,
            self.composite_skin,
        );
        let uniform = (infill_pct / 100.0).clamp(0.0, 1.0);
        let opt_density = self.opt.as_ref().map(|o| &o.cell_density);
        // Element density = occupancy × (wall fraction + infill share): a
        // cut boundary cell shows its actual inside share, not 100%.
        let mut density_of_cell = vec![0f32; grid.cell_count()];
        for &c in &split.skin {
            density_of_cell[c as usize] = grid.scale[c as usize];
        }
        for (k, &c) in split.design.iter().enumerate() {
            let x = opt_density
                .and_then(|m| m.get(&c).copied())
                .unwrap_or(uniform);
            let f = split.skin_frac[k] as f64;
            density_of_cell[c as usize] =
                (grid.scale[c as usize] as f64 * (f + (1.0 - f) * x)) as f32;
        }
        let (gnx, gny) = (grid.nx, grid.ny);
        let (h, o) = (grid.h, grid.origin);
        let keep = move |ci: usize| -> bool {
            if !cut {
                return true;
            }
            let cx = ci % gnx;
            let cy = (ci / gnx) % gny;
            let cz = ci / (gnx * gny);
            // Match three.js clipping: keep where distance = n·p + c ≥ 0.
            let d = nx * (o[0] + (cx as f64 + 0.5) * h)
                + ny * (o[1] + (cy as f64 + 0.5) * h)
                + nz * (o[2] + (cz as f64 + 0.5) * h)
                + constant;
            d >= 0.0
        };
        let (tris, edges, cell_of_tri) = grid.surface_mesh_where(&keep);
        let mut density = Vec::with_capacity(cell_of_tri.len() * 3);
        for &c in &cell_of_tri {
            let d = density_of_cell[c as usize];
            density.extend_from_slice(&[d, d, d]);
        }
        Ok(js_sys::Array::of3(
            &js_sys::Float32Array::from(tris.as_slice()),
            &js_sys::Float32Array::from(density.as_slice()),
            &js_sys::Float32Array::from(edges.as_slice()),
        ))
    }

    /// Inherent-strain preview on the analysis grid: the voxel hull of solid
    /// cells UP TO build layer `layer_max` (exclusive in Z), with a per-vertex
    /// scalar = the per-element inherent-strain SOURCE strength,
    /// `eps_cell · |σ₀_in-plane|` (MPa) where σ₀ = D:ε₀ is the eigenstress of the
    /// uniform shrink `[εxy, εxy, εz]`. Because the eigenstrain is uniform, the
    /// per-cell variation is the density (skin = full, infill = its ratio) — so
    /// this doubles as a layer-by-layer view of WHERE and HOW STRONGLY the build
    /// sim pulls. Returns `[hull f32 (9/tri), normValue f32 (3/tri, 0..1 for the
    /// ramp), edges f32, maxValueMPa f64, nz u32]`.
    pub fn inherent_strain_voxels(
        &mut self,
        layer_max: u32,
        shrink_xy: f64,
        shrink_z: f64,
    ) -> Result<js_sys::Array, JsValue> {
        self.ensure_grid()?;
        let (grid, _) = self.grid.as_ref().unwrap();
        let (gnx, gny, gnz) = (grid.nx, grid.ny, grid.nz);
        // Unit in-plane eigenstress |λ·tr(ε₀) + 2μ·εxy| at full modulus (E=e0);
        // density then scales it per cell. exy/ez are negative (shrink) — sign
        // doesn't matter for the magnitude shown.
        let (e0, nu) = (self.settings.e0, self.settings.nu);
        let lam = e0 * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
        let mu = e0 / (2.0 * (1.0 + nu));
        let tr = 2.0 * shrink_xy + shrink_z;
        let s0_unit = (lam * tr + 2.0 * mu * shrink_xy).abs() as f32;
        // Per-cell density (occupancy × infill): the optimized field when present,
        // else full (a solid part has a uniform source).
        let opt_density = self.opt.as_ref().map(|o| &o.cell_density);
        let mut value = vec![0f32; grid.cell_count()];
        let mut maxv = 0f32;
        for c in 0..grid.cell_count() {
            let occ = grid.scale[c];
            if occ <= 0.0 {
                continue;
            }
            let infill = opt_density.and_then(|m| m.get(&(c as u32)).copied()).unwrap_or(1.0) as f32;
            let v = occ * infill * s0_unit;
            value[c] = v;
            maxv = maxv.max(v);
        }
        let lmax = layer_max.min(gnz as u32) as usize;
        let keep = move |ci: usize| -> bool {
            let cz = ci / (gnx * gny);
            cz < lmax
        };
        let (tris, edges, cell_of_tri) = grid.surface_mesh_where(&keep);
        let inv = if maxv > 0.0 { 1.0 / maxv } else { 0.0 };
        let mut norm = Vec::with_capacity(cell_of_tri.len() * 3);
        for &c in &cell_of_tri {
            let v = value[c as usize] * inv;
            norm.extend_from_slice(&[v, v, v]);
        }
        let arr = js_sys::Array::new();
        arr.push(&js_sys::Float32Array::from(tris.as_slice()));
        arr.push(&js_sys::Float32Array::from(norm.as_slice()));
        arr.push(&js_sys::Float32Array::from(edges.as_slice()));
        arr.push(&JsValue::from_f64(maxv as f64));
        arr.push(&JsValue::from_f64(gnz as f64));
        Ok(arr)
    }

    /// Exposed-face hull of the analysis voxel grid (triangle soup, xyz f32).
    pub fn voxel_hull(&mut self) -> Result<Vec<f32>, JsValue> {
        self.ensure_grid()?;
        let (grid, _) = self.grid.as_ref().unwrap();
        Ok(grid.surface_mesh().0)
    }

    /// Deduplicated cell-edge segments of the hull (pairs of xyz f32 points).
    pub fn voxel_edges(&mut self) -> Result<Vec<f32>, JsValue> {
        self.ensure_grid()?;
        let (grid, _) = self.grid.as_ref().unwrap();
        Ok(grid.surface_mesh().1)
    }

    /// Zip of one binary STL per modifier region.
    pub fn export_stls(&self) -> Result<Vec<u8>, JsValue> {
        let opt = self.opt.as_ref().ok_or_else(|| err("no optimization result — run optimize first"))?;
        Ok(export_stl_zip(&opt.regions))
    }

    /// SOLID topology mode: the single optimized body as one binary STL (the
    /// regions hold exactly one kept-material mesh). Re-sliceable / re-CAD-able.
    pub fn export_solid_stl(&self) -> Result<Vec<u8>, JsValue> {
        let opt = self.opt.as_ref().ok_or_else(|| err("no optimization result — run optimize first"))?;
        let r = opt.regions.first().ok_or_else(|| err("no optimized shape"))?;
        let mut tris: Vec<[f32; 9]> = Vec::with_capacity(r.indices.len() / 3);
        for f in r.indices.chunks_exact(3) {
            let v = |i: u32| {
                let o = (i as usize) * 3;
                [r.positions[o], r.positions[o + 1], r.positions[o + 2]]
            };
            let (a, b, c) = (v(f[0]), v(f[1]), v(f[2]));
            tris.push([a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]);
        }
        Ok(TriMesh::from_triangles(tris).to_stl_binary())
    }
}

/// Sample a per-cell density field at arbitrary points (xyz triples),
/// searching outward for the nearest design cell; skin counts as solid.
fn sample_points_static(
    points: &[f32],
    grid: &VoxelGrid,
    field: &std::collections::HashMap<u32, f64>,
) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let h = grid.h;
    let cell_at = |p: [f64; 3]| -> Option<usize> {
        let cx = ((p[0] - grid.origin[0]) / h).floor() as i64;
        let cy = ((p[1] - grid.origin[1]) / h).floor() as i64;
        let cz = ((p[2] - grid.origin[2]) / h).floor() as i64;
        if cx < 0 || cy < 0 || cz < 0 || cx >= nx as i64 || cy >= ny as i64 || cz >= nz as i64 {
            return None;
        }
        Some(((cz as usize) * ny + cy as usize) * nx + cx as usize)
    };
    let mut out = Vec::with_capacity(points.len() / 3);
    for v in points.chunks_exact(3) {
        let p = [v[0] as f64, v[1] as f64, v[2] as f64];
        let mut val = 1.0f64;
        let mut found = false;
        'search: for r in 0..3i64 {
            for dz in -r..=r {
                for dy in -r..=r {
                    for dx in -r..=r {
                        let q = [p[0] + dx as f64 * h, p[1] + dy as f64 * h, p[2] + dz as f64 * h];
                        if let Some(ci) = cell_at(q) {
                            if let Some(&x) = field.get(&(ci as u32)) {
                                val = x;
                                found = true;
                                break 'search;
                            }
                            if !found && grid.scale[ci] > 0.0 {
                                found = true;
                                val = 1.0; // skin
                            }
                        }
                    }
                }
            }
            if found {
                break;
            }
        }
        out.push(val as f32);
    }
    out
}

/// Sample a per-cell scalar (dense Vec over the padded grid; valid where
/// grid.scale > 0) at every soup vertex — nearest solid cell wins.
fn sample_cell_values(tris: &[[f32; 9]], grid: &VoxelGrid, values: &[f32]) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let h = grid.h;
    let cell_at = |p: [f64; 3]| -> Option<usize> {
        let cx = ((p[0] - grid.origin[0]) / h).floor() as i64;
        let cy = ((p[1] - grid.origin[1]) / h).floor() as i64;
        let cz = ((p[2] - grid.origin[2]) / h).floor() as i64;
        if cx < 0 || cy < 0 || cz < 0 || cx >= nx as i64 || cy >= ny as i64 || cz >= nz as i64 {
            return None;
        }
        Some(((cz as usize) * ny + cy as usize) * nx + cx as usize)
    };
    let mut out = Vec::with_capacity(tris.len() * 3);
    for t in tris {
        for v in 0..3 {
            let p = [t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64];
            let mut val = 0.0f32;
            'search: for r in 0..3i64 {
                for dz in -r..=r {
                    for dy in -r..=r {
                        for dx in -r..=r {
                            let q = [
                                p[0] + dx as f64 * h,
                                p[1] + dy as f64 * h,
                                p[2] + dz as f64 * h,
                            ];
                            if let Some(ci) = cell_at(q) {
                                if grid.scale[ci] > 0.0 {
                                    val = values[ci];
                                    break 'search;
                                }
                            }
                        }
                    }
                }
            }
            out.push(val);
        }
    }
    out
}

/// Sample a recovered NODAL field (NaN = no adjacent solid cell) at every
/// soup vertex by trilinear interpolation in the containing cell, weights
/// renormalized over the valid nodes — this evaluates the field AT the true
/// surface point instead of copying the nearest boundary cell. Falls back to
/// nearest-cell sampling for the rare vertex whose cell corners are all void.
fn sample_nodal_values(
    tris: &[[f32; 9]],
    grid: &VoxelGrid,
    nodal: &[f32],
    cells: &[f32],
) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let h = grid.h;
    let o = grid.origin;
    let mut out = Vec::with_capacity(tris.len() * 3);
    let mut fallback: Vec<usize> = Vec::new();
    for (ti, t) in tris.iter().enumerate() {
        for v in 0..3 {
            let p = [t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64];
            // Containing cell, clamped into the grid.
            let f = |d: usize| ((p[d] - o[d]) / h).clamp(0.0, [nx, ny, nz][d] as f64 - 1e-9);
            let (fx, fy, fz) = (f(0), f(1), f(2));
            let (cx, cy, cz) = (fx.floor() as usize, fy.floor() as usize, fz.floor() as usize);
            let (tx, ty, tz) = (fx - cx as f64, fy - cy as f64, fz - cz as f64);
            let mut val = 0f64;
            let mut wsum = 0f64;
            for oz in 0..2 {
                for oy in 0..2 {
                    for ox in 0..2 {
                        let nv =
                            nodal[((cz + oz) * my + (cy + oy)) * mx + (cx + ox)];
                        if nv.is_nan() {
                            continue;
                        }
                        let w = (if ox == 1 { tx } else { 1.0 - tx })
                            * (if oy == 1 { ty } else { 1.0 - ty })
                            * (if oz == 1 { tz } else { 1.0 - tz });
                        val += w * nv as f64;
                        wsum += w;
                    }
                }
            }
            if wsum > 1e-9 {
                out.push((val / wsum) as f32);
            } else {
                out.push(0.0);
                fallback.push(ti * 3 + v);
            }
        }
    }
    if !fallback.is_empty() {
        let near = sample_cell_values(tris, grid, cells);
        for i in fallback {
            out[i] = near[i];
        }
    }
    out
}

/// Back-fill NaN nodes (no adjacent solid cell — see `recover_nodal`) with
/// the mean of their valid 6-neighbors, two passes: the section cap samples
/// the field wherever the SMOOTH surface is solid, which can overhang the
/// voxelization by a node or two. Remaining deep-void NaNs become 0 (never
/// sampled — no cross-section pixel is that far from a solid cell).
fn fill_nodal_gaps(nodal: &mut [f32], mx: usize, my: usize, mz: usize) {
    for _ in 0..2 {
        let src = nodal.to_vec();
        let mut changed = false;
        for z in 0..mz {
            for y in 0..my {
                for x in 0..mx {
                    let i = (z * my + y) * mx + x;
                    if !src[i].is_nan() {
                        continue;
                    }
                    let mut s = 0f32;
                    let mut n = 0u32;
                    let mut add = |j: usize| {
                        if !src[j].is_nan() {
                            s += src[j];
                            n += 1;
                        }
                    };
                    if x > 0 {
                        add(i - 1);
                    }
                    if x + 1 < mx {
                        add(i + 1);
                    }
                    if y > 0 {
                        add(i - mx);
                    }
                    if y + 1 < my {
                        add(i + mx);
                    }
                    if z > 0 {
                        add(i - mx * my);
                    }
                    if z + 1 < mz {
                        add(i + mx * my);
                    }
                    drop(add);
                    if n > 0 {
                        nodal[i] = s / n as f32;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    for v in nodal.iter_mut() {
        if v.is_nan() {
            *v = 0.0;
        }
    }
}

/// Nodal displacement for xyz points that lie ON grid nodes (voxel hull
/// vertices and cell-edge endpoints) — exact lookup, no interpolation.
fn node_displacements(points: &[f32], grid: &VoxelGrid, sol: &Solution) -> Vec<f32> {
    let h = grid.h;
    let o = grid.origin;
    let mut out = Vec::with_capacity(points.len());
    for p in points.chunks_exact(3) {
        let x = (((p[0] as f64 - o[0]) / h).round() as usize).min(sol.mx - 1);
        let y = (((p[1] as f64 - o[1]) / h).round() as usize).min(sol.my - 1);
        let z = (((p[2] as f64 - o[2]) / h).round() as usize).min(sol.mz - 1);
        let n = (z * sol.my + y) * sol.mx + x;
        out.extend_from_slice(&sol.u[3 * n..3 * n + 3]);
    }
    out
}

/// Free-function clone of sample_cell_field for use inside the progress
/// closure (no &self available there).
fn sample_field_static(
    tris: &[[f32; 9]],
    grid: &VoxelGrid,
    field: &std::collections::HashMap<u32, f64>,
) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let h = grid.h;
    let cell_at = |p: [f64; 3]| -> Option<usize> {
        let cx = ((p[0] - grid.origin[0]) / h).floor() as i64;
        let cy = ((p[1] - grid.origin[1]) / h).floor() as i64;
        let cz = ((p[2] - grid.origin[2]) / h).floor() as i64;
        if cx < 0 || cy < 0 || cz < 0 || cx >= nx as i64 || cy >= ny as i64 || cz >= nz as i64 {
            return None;
        }
        Some(((cz as usize) * ny + cy as usize) * nx + cx as usize)
    };
    let mut out = Vec::with_capacity(tris.len() * 3);
    for t in tris {
        for v in 0..3 {
            let p = [t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64];
            let mut val = 1.0f64;
            let mut found = false;
            'search: for r in 0..3i64 {
                for dz in -r..=r {
                    for dy in -r..=r {
                        for dx in -r..=r {
                            let q =
                                [p[0] + dx as f64 * h, p[1] + dy as f64 * h, p[2] + dz as f64 * h];
                            if let Some(ci) = cell_at(q) {
                                if let Some(&x) = field.get(&(ci as u32)) {
                                    val = x;
                                    found = true;
                                    break 'search;
                                }
                                if !found && grid.scale[ci] > 0.0 {
                                    found = true;
                                    val = 1.0;
                                }
                            }
                        }
                    }
                }
                if found {
                    break;
                }
            }
            out.push(val as f32);
        }
    }
    out
}

// ---- Phase-1 raw benchmark exports (used by wasm-bench.js via raw cargo build) ----

use filasim_core::mesh::primitives;
use filasim_core::{solve_static, BoxRegion, StaticProblem};

#[no_mangle]
pub extern "C" fn bench_voxelize(h: f64) -> u32 {
    let sph = primitives::sphere([0.0; 3], 25.0, 128, 64);
    let grid = VoxelGrid::voxelize(&sph, h);
    grid.solid_count() as u32
}

#[no_mangle]
pub extern "C" fn bench_solve(nx: u32, ny: u32, nz: u32, h: f64) -> f64 {
    let (nx, ny, nz) = (nx as usize, ny as usize, nz as usize);
    let (e0, nu, f) = (2000.0f64, 0.3f64, -10.0f64);
    let (l, bdim, hdim) = (nx as f64 * h, ny as f64 * h, nz as f64 * h);
    let grid = VoxelGrid::solid_box(nx, ny, nz, h);
    let problem = StaticProblem {
        grid,
        fixed: vec![BoxRegion::new([-0.1, -1.0, -1.0], [0.1, bdim + 1.0, hdim + 1.0])],
        loads: vec![(
            BoxRegion::new([l - 0.1 * h, -1.0, -1.0], [l + h, bdim + 1.0, hdim + 1.0]),
            [0.0, 0.0, f],
        )],
        settings: SolveSettings { e0, nu, tol: 1e-5, max_iter: 300, ..Default::default() },
    };
    let sol = match solve_static(&problem) {
        Ok(s) => s,
        Err(_) => return -1.0,
    };
    let tip = match sol.mean_displacement(&BoxRegion::new(
        [l - 0.1 * h, -1.0, -1.0],
        [l + h, bdim + 1.0, hdim + 1.0],
    )) {
        Some(t) => t,
        None => return -2.0,
    };
    let inertia = bdim * hdim.powi(3) / 12.0;
    let area = bdim * hdim;
    let g = e0 / (2.0 * (1.0 + nu));
    let kappa = 10.0 * (1.0 + nu) / (12.0 + 11.0 * nu);
    let exact = f * l.powi(3) / (3.0 * e0 * inertia) + f * l / (kappa * g * area);
    tip[2] / exact
}
