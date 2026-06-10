//! WASM API for the Smart Infill Generator web app.
//!
//! One `Model` instance lives in a web worker and owns mesh, segmentation,
//! voxel grid, boundary conditions, and the last solution. Bulk data crosses
//! the boundary as typed arrays; small results as JSON strings.

use sig_core::attach::{assemble, check_problem, BcKind, BcSpec};
use sig_core::mesh::TriMesh;
use sig_core::segment::{segment, Segmentation};
use sig_core::solve::{pad_for_levels, solve_nodes, SolveSettings, Solution};
use sig_core::voxel::VoxelGrid;
use wasm_bindgen::prelude::*;

const GRAVITY_MM_S2: [f64; 3] = [0.0, 0.0, -9810.0];

#[wasm_bindgen]
pub struct Model {
    mesh: TriMesh,
    seg: Segmentation,
    bcs: Vec<BcSpec>,
    settings: SolveSettings,
    /// tonne/mm³
    density: f64,
    gravity_on: bool,
    target_cells: u32,
    grid: Option<(VoxelGrid, usize)>, // padded grid + level count
    solution: Option<Solution>,
}

fn err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

#[wasm_bindgen]
impl Model {
    /// Parse an STL (binary or ASCII) and segment at the default 30° crease angle.
    #[wasm_bindgen(constructor)]
    pub fn new(stl: &[u8]) -> Result<Model, JsValue> {
        let mesh = TriMesh::from_stl(stl).map_err(err)?;
        let seg = segment(&mesh, 30.0);
        Ok(Model {
            mesh,
            seg,
            bcs: Vec::new(),
            settings: SolveSettings::default(),
            density: 1.24e-9, // PLA
            gravity_on: false,
            target_cells: 300_000,
            grid: None,
            solution: None,
        })
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

    /// Re-run segmentation with a different crease angle (degrees).
    pub fn resegment(&mut self, angle_deg: f64) {
        self.seg = segment(&self.mesh, angle_deg.clamp(1.0, 89.0));
    }

    /// [lox, loy, loz, hix, hiy, hiz] in mm.
    pub fn bbox(&self) -> Vec<f64> {
        match self.mesh.bounds() {
            Some((lo, hi)) => vec![lo[0], lo[1], lo[2], hi[0], hi[1], hi[2]],
            None => vec![0.0; 6],
        }
    }

    /// e0 in MPa, density in g/cm³.
    pub fn set_material(&mut self, e0: f64, nu: f64, density_g_cm3: f64) {
        self.settings.e0 = e0;
        self.settings.nu = nu;
        self.density = density_g_cm3 * 1e-9;
        self.solution = None;
    }

    pub fn set_gravity(&mut self, on: bool) {
        self.gravity_on = on;
        self.solution = None;
    }

    pub fn set_resolution(&mut self, target_cells: u32) {
        if target_cells != self.target_cells {
            self.target_cells = target_cells.clamp(10_000, 4_000_000);
            self.grid = None;
            self.solution = None;
        }
    }

    pub fn clear_bcs(&mut self) {
        self.bcs.clear();
        self.solution = None;
    }

    pub fn add_fixed(&mut self, tris: &[u32]) {
        self.bcs.push(BcSpec { kind: BcKind::Fixed, tris: tris.to_vec() });
        self.solution = None;
    }

    pub fn add_frictionless(&mut self, tris: &[u32]) {
        self.bcs.push(BcSpec { kind: BcKind::Frictionless, tris: tris.to_vec() });
        self.solution = None;
    }

    pub fn add_force(&mut self, tris: &[u32], fx: f64, fy: f64, fz: f64) {
        self.bcs.push(BcSpec { kind: BcKind::Force([fx, fy, fz]), tris: tris.to_vec() });
        self.solution = None;
    }

    pub fn add_pressure(&mut self, tris: &[u32], mpa: f64) {
        self.bcs.push(BcSpec { kind: BcKind::Pressure(mpa), tris: tris.to_vec() });
        self.solution = None;
    }

    fn ensure_grid(&mut self) -> Result<(), JsValue> {
        if self.grid.is_some() {
            return Ok(());
        }
        let (lo, hi) = self.mesh.bounds().ok_or_else(|| err("empty mesh"))?;
        let vol = (hi[0] - lo[0]).max(1e-6) * (hi[1] - lo[1]).max(1e-6) * (hi[2] - lo[2]).max(1e-6);
        let h = (vol / self.target_cells as f64).cbrt().max(1e-3);
        let grid = VoxelGrid::voxelize(&self.mesh, h);
        if grid.solid_count() == 0 {
            return Err(err("voxelization produced no solid cells — model too thin for this resolution"));
        }
        let (padded, levels) = pad_for_levels(&grid, self.settings.max_levels);
        self.grid = Some((padded, levels));
        Ok(())
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

    fn gravity_arg(&self) -> Option<([f64; 3], f64)> {
        if self.gravity_on {
            Some((GRAVITY_MM_S2, self.density))
        } else {
            None
        }
    }

    /// Island + rigid-body-mode check. JSON CheckReport.
    pub fn check(&mut self) -> Result<String, JsValue> {
        self.ensure_grid()?;
        let (grid, _) = self.grid.as_ref().unwrap();
        let asm = assemble(&self.mesh, grid, &self.bcs, self.gravity_arg(), &self.settings)
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
        let asm = assemble(&self.mesh, grid, &self.bcs, self.gravity_arg(), &self.settings)
            .map_err(err)?;
        let report = check_problem(grid, &asm);
        if !report.ok {
            return Err(err("model is under-constrained — run check() for details"));
        }
        let sol = solve_nodes(grid, *levels, &asm.problem, &self.settings).map_err(err)?;
        let out = serde_json::json!({
            "iterations": sol.iterations,
            "relResidual": sol.rel_residual,
            "maxDisplacement": sol.max_displacement(),
        })
        .to_string();
        self.solution = Some(sol);
        Ok(out)
    }

    /// Displacement vector (mm) per soup vertex: 9 floats per triangle.
    pub fn vertex_displacements(&self) -> Result<Vec<f32>, JsValue> {
        let sol = self.solution.as_ref().ok_or_else(|| err("no solution — call solve() first"))?;
        let mut out = Vec::with_capacity(self.mesh.tris.len() * 9);
        for t in &self.mesh.tris {
            for v in 0..3 {
                let p = [t[3 * v] as f64, t[3 * v + 1] as f64, t[3 * v + 2] as f64];
                let u = sol.sample_displacement(p);
                out.extend_from_slice(&[u[0] as f32, u[1] as f32, u[2] as f32]);
            }
        }
        Ok(out)
    }
}

// ---- Phase-1 raw benchmark exports (used by wasm-bench.js via raw cargo build) ----

use sig_core::mesh::primitives;
use sig_core::{solve_static, BoxRegion, StaticProblem};

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
