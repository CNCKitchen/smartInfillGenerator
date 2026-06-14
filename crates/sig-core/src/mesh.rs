// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Triangle soup container and a robust STL reader (binary + ASCII).
//! No manifoldness, welding, or orientation assumptions: downstream
//! winding-number voxelization tolerates dirty input by design.

#[derive(Clone, Debug, Default)]
pub struct TriMesh {
    /// Flat triangle list: 9 f32 per triangle (v0 v1 v2).
    pub tris: Vec<[f32; 9]>,
}

#[derive(Debug)]
pub enum MeshError {
    TooShort,
    Empty,
    Malformed(String),
}

impl std::fmt::Display for MeshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeshError::TooShort => write!(f, "file too short to be an STL"),
            MeshError::Empty => write!(f, "no valid triangles found"),
            MeshError::Malformed(s) => write!(f, "malformed STL: {s}"),
        }
    }
}

impl std::error::Error for MeshError {}

impl TriMesh {
    pub fn from_triangles(tris: Vec<[f32; 9]>) -> Self {
        Self { tris }
    }

    /// Uniformly subdivide so no triangle edge exceeds `target_edge`
    /// (each triangle becomes an n² barycentric grid). Coarse STLs (a box
    /// face = 2 triangles) can otherwise only display LINEAR deformation
    /// between their corner vertices — the bending curve needs vertices to
    /// exist. Total output is capped near `max_tris` by scaling n down, so
    /// already-dense meshes pass through unchanged.
    pub fn subdivided(&self, target_edge: f64, max_tris: usize) -> TriMesh {
        self.subdivided_with_parents(target_edge, max_tris).0
    }

    /// Like `subdivided`, additionally returning the ORIGINAL triangle index
    /// for every output triangle. Needed by anything semantic (segmentation
    /// patches): neighbors subdivide at different n, so T-junction vertices
    /// along shared edges do not weld and per-child analysis would fragment.
    pub fn subdivided_with_parents(&self, target_edge: f64, max_tris: usize) -> (TriMesh, Vec<u32>) {
        let target = target_edge.max(1e-9);
        let mut ns: Vec<usize> = Vec::with_capacity(self.tris.len());
        let mut total: usize = 0;
        for t in &self.tris {
            let mut max_e2 = 0f64;
            for (u, v) in [(0, 1), (1, 2), (2, 0)] {
                let mut e2 = 0f64;
                for d in 0..3 {
                    let diff = t[3 * u + d] as f64 - t[3 * v + d] as f64;
                    e2 += diff * diff;
                }
                max_e2 = max_e2.max(e2);
            }
            let n = ((max_e2.sqrt() / target).ceil() as usize).clamp(1, 64);
            ns.push(n);
            total += n * n;
        }
        if total > max_tris {
            let s = (max_tris as f64 / total as f64).sqrt();
            for n in ns.iter_mut() {
                *n = ((*n as f64 * s).floor() as usize).max(1);
            }
        }
        let mut out: Vec<[f32; 9]> = Vec::new();
        let mut parents: Vec<u32> = Vec::new();
        for (ti, (t, &n)) in self.tris.iter().zip(&ns).enumerate() {
            let before = out.len();
            if n <= 1 {
                out.push(*t);
                parents.push(ti as u32);
                continue;
            }
            let p = |k: usize| [t[3 * k] as f64, t[3 * k + 1] as f64, t[3 * k + 2] as f64];
            let (a, b, c) = (p(0), p(1), p(2));
            let v = |i: usize, j: usize| -> [f64; 3] {
                let (fi, fj) = (i as f64 / n as f64, j as f64 / n as f64);
                [
                    a[0] + (b[0] - a[0]) * fi + (c[0] - a[0]) * fj,
                    a[1] + (b[1] - a[1]) * fi + (c[1] - a[1]) * fj,
                    a[2] + (b[2] - a[2]) * fi + (c[2] - a[2]) * fj,
                ]
            };
            let mut push = |p0: [f64; 3], p1: [f64; 3], p2: [f64; 3]| {
                out.push([
                    p0[0] as f32, p0[1] as f32, p0[2] as f32,
                    p1[0] as f32, p1[1] as f32, p1[2] as f32,
                    p2[0] as f32, p2[1] as f32, p2[2] as f32,
                ]);
            };
            for i in 0..n {
                for j in 0..n - i {
                    push(v(i, j), v(i + 1, j), v(i, j + 1));
                    if i + j < n - 1 {
                        push(v(i + 1, j), v(i + 1, j + 1), v(i, j + 1));
                    }
                }
            }
            for _ in before..out.len() {
                parents.push(ti as u32);
            }
        }
        (TriMesh::from_triangles(out), parents)
    }

    /// Refine via recursive **longest-edge bisection** (Rivara) until every
    /// triangle is BOTH short enough (longest edge ≤ `target`) AND reasonably
    /// shaped (longest edge ≤ `ASPECT` × shortest edge). Unlike
    /// [`subdivided`](Self::subdivided)'s n×n barycentric split, it halves only
    /// the LONGEST edge.
    ///
    /// The aspect criterion is the important one for STEP: a chord-tolerance
    /// tessellator (truck) leaves developable faces (cylinders, cones,
    /// extrusions — flat along one direction → no chord error → no split there)
    /// as full-length slivers. Capping length alone turns one 98 mm sliver into
    /// a stack of *short* slivers (still thin → looks broken in wireframe);
    /// capping aspect splits the long edge until the triangles are ~square,
    /// i.e. a clean grid. A `floor` (target/4) stops the aspect rule from
    /// exploding on a genuinely tiny short edge.
    ///
    /// Non-conforming (T-junctions along shared edges), matching `subdivided`'s
    /// convention — fine for winding-number voxelization and adequate for
    /// display. Returns the ORIGINAL triangle index per output triangle, like
    /// [`subdivided_with_parents`](Self::subdivided_with_parents).
    pub fn capped_edges(&self, target: f64, max_tris: usize) -> (TriMesh, Vec<u32>) {
        const ASPECT: f64 = 2.5; // split slivers until longest ≤ 2.5 × shortest
        let t2 = target.max(1e-9).powi(2);
        let aspect2 = ASPECT * ASPECT;
        let floor2 = (target.max(1e-9) / 4.0).powi(2); // don't refine finer than this
        let mut out: Vec<[f32; 9]> = Vec::with_capacity(self.tris.len());
        let mut parents: Vec<u32> = Vec::new();
        let mut stack: Vec<[f64; 3]> = Vec::new(); // flat verts, 3 per triangle
        let len2 = |a: [f64; 3], b: [f64; 3]| {
            (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
        };
        for (ti, t) in self.tris.iter().enumerate() {
            stack.clear();
            stack.push([t[0] as f64, t[1] as f64, t[2] as f64]);
            stack.push([t[3] as f64, t[4] as f64, t[5] as f64]);
            stack.push([t[6] as f64, t[7] as f64, t[8] as f64]);
            while stack.len() >= 3 {
                let c = stack.pop().unwrap();
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                let (lab, lbc, lca) = (len2(a, b), len2(b, c), len2(c, a));
                let longest = lab.max(lbc).max(lca);
                let shortest = lab.min(lbc).min(lca);
                let median = lab + lbc + lca - longest - shortest; // middle value
                // Size: cap the MEDIAN edge, not the longest. A target-sized
                // square grid cell has a diagonal of target·√2 > target, so
                // capping the longest edge would split that grid forever; capping
                // the median makes a square grid stable. Aspect: split slivers
                // until ~2.5:1, but never below the floor.
                let too_big = median > t2;
                let too_thin = longest > aspect2 * shortest && longest > floor2;
                // Stop once acceptable OR the (approximate) budget is spent.
                // Pending stacked triangles count toward the budget so the final
                // total stays close to `max_tris` (it can't be exact — geometry
                // already on the stack must still be emitted).
                if (!too_big && !too_thin) || out.len() + stack.len() / 3 >= max_tris {
                    out.push([
                        a[0] as f32, a[1] as f32, a[2] as f32,
                        b[0] as f32, b[1] as f32, b[2] as f32,
                        c[0] as f32, c[1] as f32, c[2] as f32,
                    ]);
                    parents.push(ti as u32);
                    continue;
                }
                // Bisect the longest edge; the opposite vertex is shared by both
                // children (keeps the short direction intact).
                let mid = |p: [f64; 3], q: [f64; 3]| {
                    [(p[0] + q[0]) * 0.5, (p[1] + q[1]) * 0.5, (p[2] + q[2]) * 0.5]
                };
                let (t1, t2_) = if lab >= lbc && lab >= lca {
                    let m = mid(a, b);
                    ([a, m, c], [m, b, c])
                } else if lbc >= lca {
                    let m = mid(b, c);
                    ([a, b, m], [a, m, c])
                } else {
                    let m = mid(c, a);
                    ([a, b, m], [m, b, c])
                };
                stack.extend_from_slice(&t1);
                stack.extend_from_slice(&t2_);
            }
        }
        (TriMesh::from_triangles(out), parents)
    }

    pub fn len(&self) -> usize {
        self.tris.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tris.is_empty()
    }

    /// Axis-aligned bounds over all vertices. None when empty.
    pub fn bounds(&self) -> Option<([f64; 3], [f64; 3])> {
        if self.tris.is_empty() {
            return None;
        }
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        for t in &self.tris {
            for v in 0..3 {
                for d in 0..3 {
                    let x = t[3 * v + d] as f64;
                    lo[d] = lo[d].min(x);
                    hi[d] = hi[d].max(x);
                }
            }
        }
        Some((lo, hi))
    }

    /// Parse STL from bytes, auto-detecting binary vs ASCII.
    /// Robustness: drops non-finite and zero-area-degenerate triangles, ignores
    /// stored normals, tolerates a binary file whose header starts with "solid".
    pub fn from_stl(data: &[u8]) -> Result<Self, MeshError> {
        if data.len() < 15 {
            return Err(MeshError::TooShort);
        }
        // Guard: STEP is ASCII text, not an STL. Without this the binary fallback
        // below reinterprets the text bytes as f32 coordinates and yields absurd
        // geometry (e.g. ~1e34 mm). Fail clearly. (When the `step` feature is on,
        // callers route STEP to truck before reaching here; this protects builds
        // that lack it.)
        if data[..data.len().min(256)].windows(12).any(|w| w == b"ISO-10303-21") {
            return Err(MeshError::Malformed(
                "input looks like a STEP file, not an STL (STEP import unavailable in this build)"
                    .into(),
            ));
        }
        // Binary check first: header(80) + count(4) + 50*count == len is decisive.
        if data.len() >= 84 {
            let n = u32::from_le_bytes([data[80], data[81], data[82], data[83]]) as usize;
            if data.len() == 84 + 50 * n {
                return Self::from_stl_binary(data, n);
            }
        }
        let head = String::from_utf8_lossy(&data[..data.len().min(512)]).to_lowercase();
        if head.trim_start().starts_with("solid") && head.contains("facet") {
            return Self::from_stl_ascii(data);
        }
        // Fall back to binary with whatever count fits (some exporters lie in the count field).
        if data.len() >= 84 {
            let n = (data.len() - 84) / 50;
            if n > 0 {
                return Self::from_stl_binary(data, n);
            }
        }
        Err(MeshError::Malformed("neither valid binary nor ASCII STL".into()))
    }

    fn from_stl_binary(data: &[u8], n: usize) -> Result<Self, MeshError> {
        let mut tris = Vec::with_capacity(n);
        for i in 0..n {
            let base = 84 + 50 * i + 12; // skip the 12-byte normal
            let mut t = [0f32; 9];
            for k in 0..9 {
                let o = base + 4 * k;
                t[k] = f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
            }
            if keep_triangle(&t) {
                tris.push(t);
            }
        }
        if tris.is_empty() {
            return Err(MeshError::Empty);
        }
        Ok(Self { tris })
    }

    fn from_stl_ascii(data: &[u8]) -> Result<Self, MeshError> {
        let text = String::from_utf8_lossy(data);
        let mut tris = Vec::new();
        let mut cur: Vec<f32> = Vec::with_capacity(9);
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("vertex") {
                let mut vals = rest.split_whitespace().map(|s| s.parse::<f32>());
                let (x, y, z) = match (vals.next(), vals.next(), vals.next()) {
                    (Some(Ok(x)), Some(Ok(y)), Some(Ok(z))) => (x, y, z),
                    _ => return Err(MeshError::Malformed("bad vertex line".into())),
                };
                cur.extend_from_slice(&[x, y, z]);
                if cur.len() == 9 {
                    let t: [f32; 9] = cur[..].try_into().unwrap();
                    if keep_triangle(&t) {
                        tris.push(t);
                    }
                    cur.clear();
                }
            }
        }
        if tris.is_empty() {
            return Err(MeshError::Empty);
        }
        Ok(Self { tris })
    }

    /// Serialize as binary STL (test fixtures, debug exports).
    pub fn to_stl_binary(&self) -> Vec<u8> {
        let mut out = vec![0u8; 80];
        out.extend_from_slice(&(self.tris.len() as u32).to_le_bytes());
        for t in &self.tris {
            let n = triangle_normal(t);
            for c in n {
                out.extend_from_slice(&c.to_le_bytes());
            }
            for k in 0..9 {
                out.extend_from_slice(&t[k].to_le_bytes());
            }
            out.extend_from_slice(&0u16.to_le_bytes());
        }
        out
    }
}

fn keep_triangle(t: &[f32; 9]) -> bool {
    if t.iter().any(|x| !x.is_finite()) {
        return false;
    }
    // Drop exact-degenerate triangles (zero area vector); tiny slivers are kept,
    // the winding number handles them fine.
    let n = triangle_area_vector(t);
    n != [0.0, 0.0, 0.0]
}

pub(crate) fn triangle_area_vector(t: &[f32; 9]) -> [f32; 3] {
    let e1 = [t[3] - t[0], t[4] - t[1], t[5] - t[2]];
    let e2 = [t[6] - t[0], t[7] - t[1], t[8] - t[2]];
    [
        0.5 * (e1[1] * e2[2] - e1[2] * e2[1]),
        0.5 * (e1[2] * e2[0] - e1[0] * e2[2]),
        0.5 * (e1[0] * e2[1] - e1[1] * e2[0]),
    ]
}

fn triangle_normal(t: &[f32; 9]) -> [f32; 3] {
    let a = triangle_area_vector(t);
    let len = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
    if len > 0.0 {
        [a[0] / len, a[1] / len, a[2] / len]
    } else {
        [0.0, 0.0, 0.0]
    }
}

/// Procedural primitives for tests and benchmarks.
pub mod primitives {
    use super::TriMesh;

    /// Axis-aligned box from `lo` to `hi`, 12 triangles, outward normals.
    pub fn boxx(lo: [f32; 3], hi: [f32; 3]) -> TriMesh {
        let v = |x: usize, y: usize, z: usize| -> [f32; 3] {
            [
                if x == 0 { lo[0] } else { hi[0] },
                if y == 0 { lo[1] } else { hi[1] },
                if z == 0 { lo[2] } else { hi[2] },
            ]
        };
        // Each face as two triangles, CCW seen from outside.
        let faces: [[[usize; 3]; 4]; 6] = [
            // -x: corners (0,y,z)
            [[0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]],
            // +x
            [[1, 0, 0], [1, 1, 0], [1, 1, 1], [1, 0, 1]],
            // -y
            [[0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]],
            // +y
            [[0, 1, 0], [0, 1, 1], [1, 1, 1], [1, 1, 0]],
            // -z
            [[0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 0, 0]],
            // +z
            [[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]],
        ];
        let mut tris = Vec::with_capacity(12);
        for f in &faces {
            let c: Vec<[f32; 3]> = f.iter().map(|&[x, y, z]| v(x, y, z)).collect();
            for (a, b, cc) in [(0, 1, 2), (0, 2, 3)] {
                tris.push([
                    c[a][0], c[a][1], c[a][2], c[b][0], c[b][1], c[b][2], c[cc][0], c[cc][1],
                    c[cc][2],
                ]);
            }
        }
        TriMesh::from_triangles(tris)
    }

    /// UV sphere for voxelizer volume tests / BVH stress.
    pub fn sphere(center: [f32; 3], r: f32, segs: usize, rings: usize) -> TriMesh {
        let mut tris = Vec::new();
        let pt = |i: usize, j: usize| -> [f32; 3] {
            let theta = std::f32::consts::PI * (j as f32) / (rings as f32);
            let phi = 2.0 * std::f32::consts::PI * (i as f32) / (segs as f32);
            [
                center[0] + r * theta.sin() * phi.cos(),
                center[1] + r * theta.sin() * phi.sin(),
                center[2] + r * theta.cos(),
            ]
        };
        for j in 0..rings {
            for i in 0..segs {
                let p00 = pt(i, j);
                let p10 = pt(i + 1, j);
                let p01 = pt(i, j + 1);
                let p11 = pt(i + 1, j + 1);
                if j > 0 {
                    tris.push([
                        p00[0], p00[1], p00[2], p10[0], p10[1], p10[2], p11[0], p11[1], p11[2],
                    ]);
                }
                if j + 1 < rings {
                    tris.push([
                        p00[0], p00[1], p00[2], p11[0], p11[1], p11[2], p01[0], p01[1], p01[2],
                    ]);
                }
            }
        }
        TriMesh::from_triangles(tris)
    }
}
