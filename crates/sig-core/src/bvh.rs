//! Triangle BVH with aggregated dipole data for fast winding numbers
//! (Barill et al. 2018, first-order approximation). Inside/outside queries are
//! robust to holes, self-intersections, duplicate and non-manifold geometry —
//! the property that lets us accept arbitrary user STLs without repair.

use crate::mesh::TriMesh;

const LEAF_SIZE: usize = 8;
/// Far-field criterion: |q - c| > BETA * node_radius -> use dipole approximation.
const BETA: f64 = 2.0;

#[derive(Clone, Debug)]
struct Node {
    // Aggregate dipole of all triangles below this node.
    centroid: [f64; 3],     // area-weighted centroid
    area_normal: [f64; 3],  // sum of triangle area vectors
    radius: f64,            // max distance from centroid to any vertex below
    // Tree topology: leaf when count > 0.
    left: u32,
    right: u32,
    start: u32,
    count: u32,
}

pub struct WindingBvh {
    nodes: Vec<Node>,
    /// Triangles reordered for leaf locality, 9 f64 each (f64 for stable solid angles).
    tris: Vec<[f64; 9]>,
}

impl WindingBvh {
    pub fn build(mesh: &TriMesh) -> Self {
        let n = mesh.tris.len();
        assert!(n > 0, "empty mesh");
        let mut order: Vec<u32> = (0..n as u32).collect();
        let centroids: Vec<[f64; 3]> = mesh
            .tris
            .iter()
            .map(|t| {
                [
                    (t[0] as f64 + t[3] as f64 + t[6] as f64) / 3.0,
                    (t[1] as f64 + t[4] as f64 + t[7] as f64) / 3.0,
                    (t[2] as f64 + t[5] as f64 + t[8] as f64) / 3.0,
                ]
            })
            .collect();
        let mut nodes = Vec::with_capacity(2 * n / LEAF_SIZE + 4);
        build_recursive(&centroids, &mut order, 0, n, &mut nodes);
        // Reorder triangles, promote to f64.
        let tris: Vec<[f64; 9]> = order
            .iter()
            .map(|&i| {
                let t = &mesh.tris[i as usize];
                let mut o = [0f64; 9];
                for k in 0..9 {
                    o[k] = t[k] as f64;
                }
                o
            })
            .collect();
        let mut bvh = Self { nodes, tris };
        bvh.compute_aggregates(0);
        bvh
    }

    /// Aggregate dipole data bottom-up.
    fn compute_aggregates(&mut self, idx: usize) -> ([f64; 3], [f64; 3], f64, f64) {
        // returns (weighted centroid sum, area normal, total |area|, placeholder)
        let node = self.nodes[idx].clone();
        let (c_sum, a_normal, a_total) = if node.count > 0 {
            let mut c_sum = [0f64; 3];
            let mut a_n = [0f64; 3];
            let mut a_tot = 0f64;
            for ti in node.start..node.start + node.count {
                let t = &self.tris[ti as usize];
                let av = tri_area_vector_f64(t);
                let area = (av[0] * av[0] + av[1] * av[1] + av[2] * av[2]).sqrt();
                let c = [
                    (t[0] + t[3] + t[6]) / 3.0,
                    (t[1] + t[4] + t[7]) / 3.0,
                    (t[2] + t[5] + t[8]) / 3.0,
                ];
                for d in 0..3 {
                    c_sum[d] += area * c[d];
                    a_n[d] += av[d];
                }
                a_tot += area;
            }
            (c_sum, a_n, a_tot)
        } else {
            let (cl, al, tl, _) = self.compute_aggregates(node.left as usize);
            let (cr, ar, tr, _) = self.compute_aggregates(node.right as usize);
            (
                [cl[0] + cr[0], cl[1] + cr[1], cl[2] + cr[2]],
                [al[0] + ar[0], al[1] + ar[1], al[2] + ar[2]],
                tl + tr,
            )
        };
        let centroid = if a_total > 0.0 {
            [c_sum[0] / a_total, c_sum[1] / a_total, c_sum[2] / a_total]
        } else {
            [0.0, 0.0, 0.0]
        };
        // Radius: max distance from centroid to any vertex below this node.
        let node2 = self.nodes[idx].clone();
        let radius = if node2.count > 0 {
            let mut r2: f64 = 0.0;
            for ti in node2.start..node2.start + node2.count {
                let t = &self.tris[ti as usize];
                for v in 0..3 {
                    let dx = t[3 * v] - centroid[0];
                    let dy = t[3 * v + 1] - centroid[1];
                    let dz = t[3 * v + 2] - centroid[2];
                    r2 = r2.max(dx * dx + dy * dy + dz * dz);
                }
            }
            r2.sqrt()
        } else {
            // Children radii are around their own centroids; bound via triangle inequality.
            let l = &self.nodes[node2.left as usize];
            let r = &self.nodes[node2.right as usize];
            let dl = dist(&l.centroid, &centroid) + l.radius;
            let dr = dist(&r.centroid, &centroid) + r.radius;
            dl.max(dr)
        };
        let n = &mut self.nodes[idx];
        n.centroid = centroid;
        n.area_normal = a_normal;
        n.radius = radius;
        (c_sum, a_normal, a_total, 0.0)
    }

    /// Generalized winding number at point q (1 inside a closed CCW mesh, 0 outside).
    pub fn winding_number(&self, q: [f64; 3]) -> f64 {
        let mut acc = 0f64;
        let mut stack: Vec<u32> = Vec::with_capacity(64);
        stack.push(0);
        while let Some(idx) = stack.pop() {
            let node = &self.nodes[idx as usize];
            let d = dist(&node.centroid, &q);
            if node.count == 0 && d > BETA * node.radius {
                // Far field: dipole approximation  w += A·(c - q) / (4π |c-q|³)
                let r3 = d * d * d;
                let dx = [node.centroid[0] - q[0], node.centroid[1] - q[1], node.centroid[2] - q[2]];
                acc += (node.area_normal[0] * dx[0]
                    + node.area_normal[1] * dx[1]
                    + node.area_normal[2] * dx[2])
                    / (4.0 * std::f64::consts::PI * r3);
            } else if node.count > 0 {
                if d > BETA * node.radius && node.radius > 0.0 {
                    let r3 = d * d * d;
                    let dx =
                        [node.centroid[0] - q[0], node.centroid[1] - q[1], node.centroid[2] - q[2]];
                    acc += (node.area_normal[0] * dx[0]
                        + node.area_normal[1] * dx[1]
                        + node.area_normal[2] * dx[2])
                        / (4.0 * std::f64::consts::PI * r3);
                } else {
                    for ti in node.start..node.start + node.count {
                        acc += solid_angle(&self.tris[ti as usize], &q);
                    }
                }
            } else {
                stack.push(node.left);
                stack.push(node.right);
            }
        }
        acc
    }
}

fn build_recursive(
    centroids: &[[f64; 3]],
    order: &mut [u32],
    start: usize,
    count: usize,
    nodes: &mut Vec<Node>,
) -> u32 {
    let idx = nodes.len() as u32;
    nodes.push(Node {
        centroid: [0.0; 3],
        area_normal: [0.0; 3],
        radius: 0.0,
        left: 0,
        right: 0,
        start: start as u32,
        count: 0,
    });
    let slice = &mut order[start..start + count];
    if count <= LEAF_SIZE {
        nodes[idx as usize].count = count as u32;
        return idx;
    }
    // Median split along the longest centroid extent.
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for &t in slice.iter() {
        let c = &centroids[t as usize];
        for d in 0..3 {
            lo[d] = lo[d].min(c[d]);
            hi[d] = hi[d].max(c[d]);
        }
    }
    let mut axis = 0;
    for d in 1..3 {
        if hi[d] - lo[d] > hi[axis] - lo[axis] {
            axis = d;
        }
    }
    let mid = count / 2;
    slice.select_nth_unstable_by(mid, |&a, &b| {
        centroids[a as usize][axis]
            .partial_cmp(&centroids[b as usize][axis])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let left = build_recursive(centroids, order, start, mid, nodes);
    let right = build_recursive(centroids, order, start + mid, count - mid, nodes);
    nodes[idx as usize].left = left;
    nodes[idx as usize].right = right;
    idx
}

fn tri_area_vector_f64(t: &[f64; 9]) -> [f64; 3] {
    let e1 = [t[3] - t[0], t[4] - t[1], t[5] - t[2]];
    let e2 = [t[6] - t[0], t[7] - t[1], t[8] - t[2]];
    [
        0.5 * (e1[1] * e2[2] - e1[2] * e2[1]),
        0.5 * (e1[2] * e2[0] - e1[0] * e2[2]),
        0.5 * (e1[0] * e2[1] - e1[1] * e2[0]),
    ]
}

fn dist(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Signed solid angle of triangle t seen from q, normalized by 4π
/// (Van Oosterom & Strackee 1983).
#[inline]
fn solid_angle(t: &[f64; 9], q: &[f64; 3]) -> f64 {
    let a = [t[0] - q[0], t[1] - q[1], t[2] - q[2]];
    let b = [t[3] - q[0], t[4] - q[1], t[5] - q[2]];
    let c = [t[6] - q[0], t[7] - q[1], t[8] - q[2]];
    let la = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
    let lb = (b[0] * b[0] + b[1] * b[1] + b[2] * b[2]).sqrt();
    let lc = (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt();
    if la == 0.0 || lb == 0.0 || lc == 0.0 {
        return 0.0; // query point exactly on a vertex: contribution undefined, skip
    }
    let det = a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
        + a[2] * (b[0] * c[1] - b[1] * c[0]);
    let den = la * lb * lc
        + (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]) * lc
        + (b[0] * c[0] + b[1] * c[1] + b[2] * c[2]) * la
        + (c[0] * a[0] + c[1] * a[1] + c[2] * a[2]) * lb;
    let omega = 2.0 * det.atan2(den);
    omega / (4.0 * std::f64::consts::PI)
}
