// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Voxelization: classify cell centers of a regular grid by generalized
//! winding number. |w| >= 0.5 counts as inside, which also tolerates
//! inside-out (flipped-normal) meshes.

use crate::bvh::WindingBvh;
use crate::mesh::TriMesh;
use crate::par;

#[derive(Clone, Debug)]
pub struct VoxelGrid {
    /// Cell counts per axis.
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    /// Cubic cell edge length (mm).
    pub h: f64,
    /// World position of the grid corner (node 0,0,0).
    pub origin: [f64; 3],
    /// Per-cell relative density in [0,1]; 0 = void. Length nx*ny*nz,
    /// index (cz*ny + cy)*nx + cx.
    pub scale: Vec<f32>,
}

impl VoxelGrid {
    pub fn cell_count(&self) -> usize {
        self.nx * self.ny * self.nz
    }

    #[inline]
    pub fn cell_index(&self, cx: usize, cy: usize, cz: usize) -> usize {
        (cz * self.ny + cy) * self.nx + cx
    }

    pub fn solid_count(&self) -> usize {
        self.scale.iter().filter(|&&s| s > 0.0).count()
    }

    /// Fully solid box grid (solver tests and benchmarks without an STL).
    pub fn solid_box(nx: usize, ny: usize, nz: usize, h: f64) -> Self {
        Self { nx, ny, nz, h, origin: [0.0; 3], scale: vec![1.0; nx * ny * nz] }
    }

    /// Voxelize a triangle mesh at cell size `h`. The grid is sized to the mesh
    /// bounds plus half a cell of slack so boundary cell centers stay interior.
    pub fn voxelize(mesh: &TriMesh, h: f64) -> Self {
        let (lo, hi) = mesh.bounds().expect("empty mesh");
        let nx = (((hi[0] - lo[0]) / h).ceil() as usize).max(1);
        let ny = (((hi[1] - lo[1]) / h).ceil() as usize).max(1);
        let nz = (((hi[2] - lo[2]) / h).ceil() as usize).max(1);
        // Center the grid on the bounds (ceil rounding adds <1 cell of margin total).
        let origin = [
            lo[0] - 0.5 * (nx as f64 * h - (hi[0] - lo[0])),
            lo[1] - 0.5 * (ny as f64 * h - (hi[1] - lo[1])),
            lo[2] - 0.5 * (nz as f64 * h - (hi[2] - lo[2])),
        ];
        let bvh = WindingBvh::build(mesh);
        let mut scale = vec![0f32; nx * ny * nz];
        let (onx, ony) = (nx, ny);
        let (ox, oy, oz) = (origin[0], origin[1], origin[2]);
        par::map_indexed(&mut scale, |i| {
            let cx = i % onx;
            let cy = (i / onx) % ony;
            let cz = i / (onx * ony);
            let q = [
                ox + (cx as f64 + 0.5) * h,
                oy + (cy as f64 + 0.5) * h,
                oz + (cz as f64 + 0.5) * h,
            ];
            if bvh.winding_number(q).abs() >= 0.5 {
                1.0
            } else {
                0.0
            }
        });
        Self { nx, ny, nz, h, origin, scale }
    }

    /// Solid volume in mm³.
    pub fn solid_volume(&self) -> f64 {
        self.solid_count() as f64 * self.h * self.h * self.h
    }

    /// Exposed-face hull of the solid cells (the mesh the solver actually
    /// runs on, for display). Returns (triangle soup positions, deduplicated
    /// cell-edge segments), both flat xyz f32 in world mm.
    pub fn surface_mesh(&self) -> (Vec<f32>, Vec<f32>) {
        let (nx, ny, nz) = (self.nx, self.ny, self.nz);
        let h = self.h;
        let o = self.origin;
        // Quad corners per face direction, CCW seen from outside.
        const FACES: [([i64; 3], [[usize; 3]; 4]); 6] = [
            ([-1, 0, 0], [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]]),
            ([1, 0, 0], [[1, 0, 0], [1, 1, 0], [1, 1, 1], [1, 0, 1]]),
            ([0, -1, 0], [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]]),
            ([0, 1, 0], [[0, 1, 0], [0, 1, 1], [1, 1, 1], [1, 1, 0]]),
            ([0, 0, -1], [[0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 0, 0]]),
            ([0, 0, 1], [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]]),
        ];
        let solid = |x: i64, y: i64, z: i64| -> bool {
            if x < 0 || y < 0 || z < 0 || x >= nx as i64 || y >= ny as i64 || z >= nz as i64 {
                return false;
            }
            self.scale[(z as usize * ny + y as usize) * nx + x as usize] > 0.0
        };
        let node_id = |p: [usize; 3]| -> u64 {
            ((p[2] * (ny + 1) + p[1]) * (nx + 1) + p[0]) as u64
        };
        let mut tris: Vec<f32> = Vec::new();
        let mut edge_set: std::collections::HashSet<(u64, u64)> = Default::default();
        let mut edges: Vec<f32> = Vec::new();
        for cz in 0..nz {
            for cy in 0..ny {
                for cx in 0..nx {
                    if self.scale[(cz * ny + cy) * nx + cx] <= 0.0 {
                        continue;
                    }
                    for (dir, corners) in &FACES {
                        if solid(cx as i64 + dir[0], cy as i64 + dir[1], cz as i64 + dir[2]) {
                            continue;
                        }
                        let q: Vec<[usize; 3]> = corners
                            .iter()
                            .map(|c| [cx + c[0], cy + c[1], cz + c[2]])
                            .collect();
                        let world = |p: [usize; 3]| -> [f32; 3] {
                            [
                                (o[0] + p[0] as f64 * h) as f32,
                                (o[1] + p[1] as f64 * h) as f32,
                                (o[2] + p[2] as f64 * h) as f32,
                            ]
                        };
                        for idx in [[0, 1, 2], [0, 2, 3]] {
                            for &k in &idx {
                                tris.extend_from_slice(&world(q[k]));
                            }
                        }
                        for k in 0..4 {
                            let (a, b) = (q[k], q[(k + 1) % 4]);
                            let (ia, ib) = (node_id(a), node_id(b));
                            let key = (ia.min(ib), ia.max(ib));
                            if edge_set.insert(key) {
                                edges.extend_from_slice(&world(a));
                                edges.extend_from_slice(&world(b));
                            }
                        }
                    }
                }
            }
        }
        (tris, edges)
    }
}
