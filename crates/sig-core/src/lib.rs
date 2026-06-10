//! sig-core: voxel-based structural analysis engine for the Smart Infill Generator.
//!
//! Pipeline: triangle mesh (STL) -> winding-number voxelization -> matrix-free
//! hex-element FEA preconditioned by geometric multigrid (MGCG).
//! f32 storage with f64 reductions; designed to run native (rayon) and in WASM
//! (sequential or wasm threads via the `parallel` feature).

pub mod bvh;
pub mod fem;
pub mod mesh;
pub mod mg;
pub mod par;
pub mod solve;
pub mod voxel;

pub use mesh::TriMesh;
pub use solve::{solve_static, BoxRegion, SolveError, SolveSettings, Solution, StaticProblem};
pub use voxel::VoxelGrid;
