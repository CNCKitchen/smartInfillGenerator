//! Minimal C-ABI WASM surface for the Phase-1 benchmark harness.
//! (The real product API arrives with wasm-bindgen + threads in Phase 2;
//! this exists to measure raw single-thread WASM solver performance.)

use sig_core::mesh::primitives;
use sig_core::{solve_static, BoxRegion, SolveSettings, StaticProblem, VoxelGrid};

/// Voxelize a 16k-triangle sphere (r=25mm) at cell size `h`; returns solid cells.
#[no_mangle]
pub extern "C" fn bench_voxelize(h: f64) -> u32 {
    let sph = primitives::sphere([0.0; 3], 25.0, 128, 64);
    let grid = VoxelGrid::voxelize(&sph, h);
    grid.solid_count() as u32
}

/// Cantilever solve (nx x ny x nz cells); returns FE/Timoshenko tip ratio,
/// or a negative error code.
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

/// Iteration count of the last solve is impractical to thread through the C ABI
/// cheaply; the Node harness reports wall time, which is what Phase 1 needs.
#[no_mangle]
pub extern "C" fn version() -> u32 {
    1
}
