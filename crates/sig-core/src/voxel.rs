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
}
