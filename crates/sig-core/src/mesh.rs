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
