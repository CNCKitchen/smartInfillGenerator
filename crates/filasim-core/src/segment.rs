// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Surface segmentation: weld the triangle soup, build edge adjacency, and
//! region-grow patches across edges whose dihedral angle is below a threshold.
//! CAD-derived STLs decompose into face-like patches; organic meshes fall back
//! to the brush tools in the UI.

use crate::mesh::TriMesh;
use std::collections::HashMap;

pub struct Segmentation {
    /// Patch id per triangle (same length/order as mesh.tris).
    pub patch_of_tri: Vec<u32>,
    pub patch_count: usize,
}

pub fn segment(mesh: &TriMesh, max_dihedral_deg: f64) -> Segmentation {
    let nt = mesh.tris.len();
    if nt == 0 {
        return Segmentation { patch_of_tri: Vec::new(), patch_count: 0 };
    }
    let (lo, hi) = mesh.bounds().unwrap();
    let diag = ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2) + (hi[2] - lo[2]).powi(2)).sqrt();
    let q = (diag * 1e-6).max(1e-9);

    // Weld vertices by quantized position.
    let mut vert_ids: HashMap<(i64, i64, i64), u32> = HashMap::new();
    let mut tri_verts: Vec<[u32; 3]> = Vec::with_capacity(nt);
    for t in &mesh.tris {
        let mut ids = [0u32; 3];
        for v in 0..3 {
            let key = (
                (t[3 * v] as f64 / q).round() as i64,
                (t[3 * v + 1] as f64 / q).round() as i64,
                (t[3 * v + 2] as f64 / q).round() as i64,
            );
            let next = vert_ids.len() as u32;
            ids[v] = *vert_ids.entry(key).or_insert(next);
        }
        tri_verts.push(ids);
    }

    // Unit normals.
    let normals: Vec<[f64; 3]> = mesh
        .tris
        .iter()
        .map(|t| {
            let e1 = [
                (t[3] - t[0]) as f64,
                (t[4] - t[1]) as f64,
                (t[5] - t[2]) as f64,
            ];
            let e2 = [
                (t[6] - t[0]) as f64,
                (t[7] - t[1]) as f64,
                (t[8] - t[2]) as f64,
            ];
            let n = [
                e1[1] * e2[2] - e1[2] * e2[1],
                e1[2] * e2[0] - e1[0] * e2[2],
                e1[0] * e2[1] - e1[1] * e2[0],
            ];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if len > 0.0 {
                [n[0] / len, n[1] / len, n[2] / len]
            } else {
                [0.0, 0.0, 0.0]
            }
        })
        .collect();

    // Edge -> incident triangles. Only 2-manifold edges carry adjacency;
    // border and non-manifold edges act as patch boundaries.
    let mut edges: HashMap<(u32, u32), Vec<u32>> = HashMap::with_capacity(nt * 3 / 2);
    for (ti, vs) in tri_verts.iter().enumerate() {
        for e in 0..3 {
            let (a, b) = (vs[e], vs[(e + 1) % 3]);
            if a == b {
                continue; // quantize-degenerate edge
            }
            let key = (a.min(b), a.max(b));
            edges.entry(key).or_default().push(ti as u32);
        }
    }

    let cos_thresh = max_dihedral_deg.to_radians().cos();
    let mut patch_of_tri = vec![u32::MAX; nt];
    let mut patch_count = 0u32;
    let mut stack: Vec<u32> = Vec::new();
    for seed in 0..nt {
        if patch_of_tri[seed] != u32::MAX {
            continue;
        }
        patch_of_tri[seed] = patch_count;
        stack.push(seed as u32);
        while let Some(ti) = stack.pop() {
            let vs = tri_verts[ti as usize];
            for e in 0..3 {
                let (a, b) = (vs[e], vs[(e + 1) % 3]);
                if a == b {
                    continue;
                }
                let key = (a.min(b), a.max(b));
                let list = &edges[&key];
                if list.len() != 2 {
                    continue;
                }
                let other = if list[0] == ti { list[1] } else { list[0] };
                if patch_of_tri[other as usize] != u32::MAX {
                    continue;
                }
                let n1 = &normals[ti as usize];
                let n2 = &normals[other as usize];
                let dot = n1[0] * n2[0] + n1[1] * n2[1] + n1[2] * n2[2];
                if dot >= cos_thresh {
                    patch_of_tri[other as usize] = patch_count;
                    stack.push(other);
                }
            }
        }
        patch_count += 1;
    }

    Segmentation { patch_of_tri, patch_count: patch_count as usize }
}

/// Area-weighted average normal of a triangle selection (unit length, or zero).
pub fn average_normal(mesh: &TriMesh, tris: &[u32]) -> [f64; 3] {
    let mut acc = [0f64; 3];
    for &ti in tris {
        let t = &mesh.tris[ti as usize];
        let av = crate::mesh::triangle_area_vector(t);
        for d in 0..3 {
            acc[d] += av[d] as f64;
        }
    }
    let len = (acc[0] * acc[0] + acc[1] * acc[1] + acc[2] * acc[2]).sqrt();
    if len > 0.0 {
        [acc[0] / len, acc[1] / len, acc[2] / len]
    } else {
        [0.0, 0.0, 0.0]
    }
}
