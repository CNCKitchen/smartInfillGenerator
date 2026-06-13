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
    /// bounds plus half a cell of slack.
    ///
    /// Cut cells use the Finite-Cell / ersatz convention: each cell's `scale`
    /// is its 3×3×3 supersampled OCCUPANCY (the share inside the part), and the
    /// occupancy also decides the solid SET — a cell is solid when occupancy
    /// ≥ `BOUNDARY_FLOOR`. That INCLUDES cells whose center is outside but the
    /// surface still cuts (so the part never protrudes past its mesh) and DROPS
    /// sub-floor slivers (small-cut-cell guard). Stiffness, mass, the element-
    /// density plot, and the hull display all scale by / follow occupancy.
    /// Interior cells stay exactly 1. Chosen over center-inclusion by an 8-case
    /// analytic benchmark — see `tests/meshbench.rs` and DESIGN.md.
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
        // Finite-Cell occupancy pass. The center-inside result above is reused
        // only to find boundary cells fast; occupancy decides the final SET and
        // value. A cell whose center+all 6 neighbors agree is fully interior
        // (1.0) or fully exterior (0.0) and skips the 27-sample supersample.
        // Boundary cells (centers disagree) get occupancy and join the solid
        // iff occupancy ≥ BOUNDARY_FLOOR — including center-outside cells the
        // surface cuts (inflation) and dropping sub-floor slivers.
        const BOUNDARY_FLOOR: f32 = 0.15;
        let center_in = scale.clone();
        let onz = nz;
        par::map_indexed(&mut scale, |i| {
            let cx = i % onx;
            let cy = (i / onx) % ony;
            let cz = i / (onx * ony);
            let in_at = |x: i64, y: i64, z: i64| -> bool {
                x >= 0
                    && y >= 0
                    && z >= 0
                    && x < onx as i64
                    && y < ony as i64
                    && z < onz as i64
                    && center_in[((z as usize) * ony + y as usize) * onx + x as usize] > 0.0
            };
            let (xi, yi, zi) = (cx as i64, cy as i64, cz as i64);
            let self_in = center_in[i] > 0.0;
            let all_same = in_at(xi - 1, yi, zi) == self_in
                && in_at(xi + 1, yi, zi) == self_in
                && in_at(xi, yi - 1, zi) == self_in
                && in_at(xi, yi + 1, zi) == self_in
                && in_at(xi, yi, zi - 1) == self_in
                && in_at(xi, yi, zi + 1) == self_in;
            if all_same {
                return if self_in { 1.0 } else { 0.0 };
            }
            let mut inside = 0u32;
            for a in 0..3 {
                for b in 0..3 {
                    for c in 0..3 {
                        let q = [
                            ox + (cx as f64 + (2 * a + 1) as f64 / 6.0) * h,
                            oy + (cy as f64 + (2 * b + 1) as f64 / 6.0) * h,
                            oz + (cz as f64 + (2 * c + 1) as f64 / 6.0) * h,
                        ];
                        if bvh.winding_number(q).abs() >= 0.5 {
                            inside += 1;
                        }
                    }
                }
            }
            let occ = inside as f32 / 27.0;
            if occ >= BOUNDARY_FLOOR {
                occ
            } else {
                0.0
            }
        });
        Self { nx, ny, nz, h, origin, scale }
    }

    /// Solid volume in mm³, occupancy-weighted (cut boundary cells count
    /// their actual inside fraction).
    pub fn solid_volume(&self) -> f64 {
        self.scale.iter().map(|&s| s as f64).sum::<f64>() * self.h * self.h * self.h
    }

    /// Exposed-face hull of the solid cells (the mesh the solver actually
    /// runs on, for display). Returns (triangle soup positions, deduplicated
    /// cell-edge segments), both flat xyz f32 in world mm.
    pub fn surface_mesh(&self) -> (Vec<f32>, Vec<f32>) {
        let (t, e, _) = self.surface_mesh_where(&|_| true);
        (t, e)
    }

    /// Like `surface_mesh`, but only cells with `keep(ci)` participate —
    /// faces appear wherever a kept cell borders void OR a dropped cell.
    /// This is the voxel-true section view: dropping the cells on one side
    /// of a plane exposes the interior CELLS (skin thickness inspectable)
    /// instead of a planar cut. Also returns the owning cell index per
    /// emitted TRIANGLE so callers can color skin vs interior.
    pub fn surface_mesh_where(
        &self,
        keep: &dyn Fn(usize) -> bool,
    ) -> (Vec<f32>, Vec<f32>, Vec<u32>) {
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
        let visible = |x: i64, y: i64, z: i64| -> bool {
            // A face is drawn when the neighbor is void, outside, or dropped.
            if x < 0 || y < 0 || z < 0 || x >= nx as i64 || y >= ny as i64 || z >= nz as i64 {
                return false;
            }
            let ci = (z as usize * ny + y as usize) * nx + x as usize;
            self.scale[ci] > 0.0 && keep(ci)
        };
        let node_id = |p: [usize; 3]| -> u64 {
            ((p[2] * (ny + 1) + p[1]) * (nx + 1) + p[0]) as u64
        };
        let mut tris: Vec<f32> = Vec::new();
        let mut cell_of_tri: Vec<u32> = Vec::new();
        let mut edge_set: std::collections::HashSet<(u64, u64)> = Default::default();
        let mut edges: Vec<f32> = Vec::new();
        for cz in 0..nz {
            for cy in 0..ny {
                for cx in 0..nx {
                    let ci = (cz * ny + cy) * nx + cx;
                    if self.scale[ci] <= 0.0 || !keep(ci) {
                        continue;
                    }
                    for (dir, corners) in &FACES {
                        if visible(cx as i64 + dir[0], cy as i64 + dir[1], cz as i64 + dir[2]) {
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
                            cell_of_tri.push(ci as u32);
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
        (tris, edges, cell_of_tri)
    }
}

/// Voxel size for a cell budget over a bbox volume, optionally snapped to an
/// integer fraction of the wall thickness (h = wall/k) so a `wall_mm` solid
/// skin is resolved by exactly k cell layers on flat faces (the legacy skin
/// model uses layers = round(wall/h); with composite skin the snap is an
/// accuracy nicety, not a requirement). Snapping may refine the grid past
/// the budget; a hard cell cap bounds the cost, and when even k = 1
/// (h = wall) would blow past the cap the snap is abandoned for the nominal
/// size.
pub fn pick_voxel_size(bbox_volume: f64, target_cells: f64, snap_wall_mm: f64) -> f64 {
    let h0 = (bbox_volume / target_cells.max(1.0)).cbrt().max(1e-3);
    if snap_wall_mm <= 0.0 {
        return h0;
    }
    const HARD_CAP_CELLS: f64 = 4_000_000.0;
    let cells = |h: f64| bbox_volume / h.powi(3);
    let mut k = (snap_wall_mm / h0).round().max(1.0);
    while k > 1.0 && cells(snap_wall_mm / k) > HARD_CAP_CELLS {
        k -= 1.0;
    }
    let h = (snap_wall_mm / k).max(1e-3);
    if cells(h) > HARD_CAP_CELLS {
        return h0; // wall finer than the budget allows: don't snap
    }
    h
}
