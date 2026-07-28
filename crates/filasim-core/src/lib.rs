// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! filasim-core: voxel-based structural analysis engine for filaSim
//! (formerly "Smart Infill Generator" — hence the `sig` crate prefix).
//!
//! Pipeline: triangle mesh (STL) -> winding-number voxelization -> matrix-free
//! hex-element FEA preconditioned by geometric multigrid (MGCG).
//! f32 storage with f64 reductions; designed to run native (rayon) and in WASM
//! (sequential or wasm threads via the `parallel` feature).

pub mod attach;
pub mod bins;
pub mod buildsim;
pub mod bvh;
pub mod cancel;
pub mod check;
pub mod cylinder;
pub mod eps;
pub mod fem;
pub mod mesh;
pub mod mg;
pub mod modal;
pub mod orient;
pub mod par;
pub mod pipeline;
pub mod progress;
pub mod reaction;
pub mod rigid;
pub mod segment;
pub mod selfsupport;
pub mod settings;
pub mod simp;
pub mod solve;
pub mod strength;
pub mod stress;
pub mod threemf;
pub mod ti;
pub mod voxel;
pub mod zip;

pub use mesh::TriMesh;
pub use solve::{
    pad_for_levels, solve_nodes, solve_static, BoxRegion, NodeProblem, SolveError, SolveSettings,
    Solution, StaticProblem,
};
pub use voxel::VoxelGrid;
