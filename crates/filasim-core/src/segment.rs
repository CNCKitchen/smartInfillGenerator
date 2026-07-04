// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Surface segmentation: weld the triangle soup, build edge adjacency, and
//! region-grow patches across edges whose dihedral angle is below a threshold.
//! CAD-derived STLs decompose into face-like patches; organic meshes fall back
//! to the brush tools in the UI.

use crate::mesh::TriMesh;
use crate::threemf::weld;
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

/// Number of separate solid BODIES in the soup: connected components over
/// welded vertices (any shared vertex joins — no manifoldness required),
/// minus two kinds of non-bodies:
/// - internal cavity shells (negative signed volume, bbox nested inside
///   another component) — a hollow part is ONE body, not two;
/// - dust (components under 0.1% of the largest component's diagonal) —
///   stray exporter debris shouldn't trigger a warning.
///
/// Drives the load-time multi-body warning: the solver never joins separate
/// bodies — they only fuse where voxelization bridges sub-cell gaps.
pub fn body_count(mesh: &TriMesh) -> usize {
    let im = weld(mesh);
    if im.triangles.is_empty() {
        return usize::from(!mesh.tris.is_empty());
    }

    // Union-find over welded vertices; every triangle joins its three corners.
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let grand = parent[parent[x as usize] as usize];
            parent[x as usize] = grand;
            x = grand;
        }
        x
    }
    let mut parent: Vec<u32> = (0..im.vertices.len() as u32).collect();
    for t in &im.triangles {
        let a = find(&mut parent, t[0]);
        let b = find(&mut parent, t[1]);
        let c = find(&mut parent, t[2]);
        parent[b as usize] = a;
        parent[c as usize] = a;
    }

    // Per-component bbox + signed volume (divergence theorem; positive for
    // outward-oriented closed shells, negative for cavity shells).
    struct Comp {
        lo: [f32; 3],
        hi: [f32; 3],
        vol6: f64,
    }
    let mut comp_of_root: HashMap<u32, usize> = HashMap::new();
    let mut comps: Vec<Comp> = Vec::new();
    for t in &im.triangles {
        let root = find(&mut parent, t[0]);
        let next = comps.len();
        let idx = *comp_of_root.entry(root).or_insert(next);
        if idx == comps.len() {
            comps.push(Comp { lo: [f32::INFINITY; 3], hi: [f32::NEG_INFINITY; 3], vol6: 0.0 });
        }
        let comp = &mut comps[idx];
        let p: Vec<[f64; 3]> = t
            .iter()
            .map(|&vi| {
                let v = im.vertices[vi as usize];
                for d in 0..3 {
                    comp.lo[d] = comp.lo[d].min(v[d]);
                    comp.hi[d] = comp.hi[d].max(v[d]);
                }
                [v[0] as f64, v[1] as f64, v[2] as f64]
            })
            .collect();
        comp.vol6 += p[0][0] * (p[1][1] * p[2][2] - p[1][2] * p[2][1])
            + p[0][1] * (p[1][2] * p[2][0] - p[1][0] * p[2][2])
            + p[0][2] * (p[1][0] * p[2][1] - p[1][1] * p[2][0]);
    }

    let comp_diag = |c: &Comp| {
        (((c.hi[0] - c.lo[0]) as f64).powi(2)
            + ((c.hi[1] - c.lo[1]) as f64).powi(2)
            + ((c.hi[2] - c.lo[2]) as f64).powi(2))
        .sqrt()
    };
    let max_diag = comps.iter().map(|c| comp_diag(c)).fold(0.0, f64::max);
    let dust = max_diag * 1e-3;
    let nest_tol = (max_diag * 1e-5) as f32;
    let mut count = 0usize;
    for (i, c) in comps.iter().enumerate() {
        let d = comp_diag(c);
        if d < dust {
            continue;
        }
        // A cavity must enclose real volume — a near-zero-volume nested SHEET
        // (e.g. an internal baffle) is still a separate body worth warning on.
        let is_cavity = c.vol6 / 6.0 < -(d * 1e-2).powi(3)
            && comps.iter().enumerate().any(|(j, o)| {
                j != i
                    && (0..3).all(|k| o.lo[k] - nest_tol <= c.lo[k] && c.hi[k] <= o.hi[k] + nest_tol)
            });
        if !is_cavity {
            count += 1;
        }
    }
    count.max(1)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 12-triangle axis-aligned cube; `inverted` flips the winding so normals
    /// point inward (a cavity shell).
    fn cube(center: [f32; 3], size: f32, inverted: bool) -> Vec<[f32; 9]> {
        let h = size / 2.0;
        let c: Vec<[f32; 3]> = [
            [-1., -1., -1.],
            [1., -1., -1.],
            [1., 1., -1.],
            [-1., 1., -1.],
            [-1., -1., 1.],
            [1., -1., 1.],
            [1., 1., 1.],
            [-1., 1., 1.],
        ]
        .iter()
        .map(|d| [center[0] + d[0] * h, center[1] + d[1] * h, center[2] + d[2] * h])
        .collect();
        // Outward-wound quads.
        let quads = [[0, 3, 2, 1], [4, 5, 6, 7], [0, 1, 5, 4], [2, 3, 7, 6], [0, 4, 7, 3], [1, 2, 6, 5]];
        let mut out = Vec::new();
        for q in quads {
            for tri in [[q[0], q[1], q[2]], [q[0], q[2], q[3]]] {
                let [a, b, cc] = [c[tri[0]], c[if inverted { tri[2] } else { tri[1] }], c[if inverted { tri[1] } else { tri[2] }]];
                out.push([a[0], a[1], a[2], b[0], b[1], b[2], cc[0], cc[1], cc[2]]);
            }
        }
        out
    }

    #[test]
    fn single_cube_is_one_body() {
        let m = TriMesh::from_triangles(cube([0.; 3], 10.0, false));
        assert_eq!(body_count(&m), 1);
    }

    #[test]
    fn separate_cubes_are_two_bodies() {
        let mut tris = cube([0.; 3], 10.0, false);
        tris.extend(cube([30.0, 0.0, 0.0], 10.0, false));
        assert_eq!(body_count(&TriMesh::from_triangles(tris)), 2);
    }

    #[test]
    fn face_touching_cubes_weld_into_one_body() {
        // Shared face corners are coincident → welded → one component.
        let mut tris = cube([0.; 3], 10.0, false);
        tris.extend(cube([10.0, 0.0, 0.0], 10.0, false));
        assert_eq!(body_count(&TriMesh::from_triangles(tris)), 1);
    }

    #[test]
    fn hollow_cube_is_one_body() {
        // Inverted inner shell = internal cavity, not a second body.
        let mut tris = cube([0.; 3], 10.0, false);
        tris.extend(cube([0.; 3], 5.0, true));
        assert_eq!(body_count(&TriMesh::from_triangles(tris)), 1);
    }

    #[test]
    fn enclosed_solid_is_a_second_body() {
        // Outward-oriented shell INSIDE another (captive part) still counts.
        let mut tris = cube([0.; 3], 10.0, false);
        tris.extend(cube([0.; 3], 5.0, false));
        assert_eq!(body_count(&TriMesh::from_triangles(tris)), 2);
    }

    #[test]
    fn debris_triangle_is_not_a_body() {
        let mut tris = cube([0.; 3], 100.0, false);
        tris.push([60.0, 0.0, 0.0, 60.01, 0.0, 0.0, 60.0, 0.01, 0.0]);
        assert_eq!(body_count(&TriMesh::from_triangles(tris)), 1);
    }
}
