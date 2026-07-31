// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Exact element stiffness for a CUT cell — the boundary element done properly.
//!
//! # What is wrong today
//!
//! A boundary cell is currently handled as `KE_cut = occupancy · KE_full`
//! (Finite-Cell / ersatz, Theory Manual §4.5). That gets the stiffness
//! MAGNITUDE approximately right and the stiffness SHAPE wrong: it models the
//! material as uniformly smeared through the whole cube, when in reality it
//! occupies one particular sub-region. A cell cut so that a thin slab remains is
//! genuinely stiff in-plane and soft out-of-plane; occupancy scaling makes it
//! uniformly soft. That directional error is what puts a spurious peak in the
//! strain field at a re-entrant feature — measured in `tests/surfstress.rs` as a
//! fillet Kt that DIVERGES under refinement (+9% at h=0.5 → +14% at h=0.25).
//!
//! The fix is the real integral over the material actually inside the cell:
//!
//! ```text
//! KE_cut = ∫_(cell ∩ part) Bᵀ C B dV
//! ```
//!
//! # Why this does not cost a 24×24 matrix per cell to describe
//!
//! On a trilinear hex each `dN_l/dx` is bilinear in the OTHER two local
//! coordinates, so every entry of `BᵀCB` is a polynomial of degree ≤ 2 **in each
//! of** ξ, η, ζ. The whole integrand therefore lives in the 3×3×3 = **27**
//! monomial space `ξ^a η^b ζ^c, a,b,c ∈ {0,1,2}`, and
//!
//! ```text
//! KE_cut = (h/2) · Σ_abc  M_abc · G_abc
//! ```
//!
//! where `G_abc` are 27 fixed 24×24 matrices (material only — computed once) and
//! `M_abc = ∫ ξ^a η^b ζ^c dξdηdζ` over the cut region are 27 **geometric
//! moments**, ~108 bytes per cell as f32. So the per-cell geometry is 27 numbers
//! rather than a 1.2 kB matrix — an 11× cut in the data that has to be carried
//! per boundary cell, which matters because this solver is memory-bandwidth
//! bound, not compute bound (modal profiling, design note §12b).
//!
//! Both paths are implemented: [`ke_cut_reference`] integrates `BᵀCB` directly
//! (slow, the ground truth) and [`ke_from_moments`] reconstructs from the 27
//! moments. The tests assert they agree, which is what licenses storing only the
//! moments.

use crate::fem::{iso_stiffness, NODE_SIGNS};

/// The 27 geometric moments `∫ ξ^a η^b ζ^c dξdηdζ` over a region of the
/// reference cell `[-1,1]³`, indexed `a*9 + b*3 + c`. A full cell has
/// `M[0] = 8` (its volume in reference coordinates).
pub type CellMoments = [f64; 27];

/// Dimensionless shape-gradient of node `l` at local `(x, y, z)`: this is
/// `(h/2) · dN_l/dx`, so all the `h` dependence is factored out and the
/// integrand below is pure geometry × material.
#[inline]
fn dshape(l: usize, x: f64, y: f64, z: f64) -> [f64; 3] {
    let [sx, sy, sz] = NODE_SIGNS[l];
    [
        sx * (1.0 + y * sy) * (1.0 + z * sz) / 8.0,
        sy * (1.0 + x * sx) * (1.0 + z * sz) / 8.0,
        sz * (1.0 + x * sx) * (1.0 + y * sy) / 8.0,
    ]
}

/// Dimensionless B matrix (engineering Voigt order xx yy zz xy yz zx).
fn b_hat(x: f64, y: f64, z: f64) -> [[f64; 24]; 6] {
    let mut b = [[0.0f64; 24]; 6];
    for l in 0..8 {
        let d = dshape(l, x, y, z);
        let col = 3 * l;
        b[0][col] = d[0];
        b[1][col + 1] = d[1];
        b[2][col + 2] = d[2];
        b[3][col] = d[1];
        b[3][col + 1] = d[0];
        b[4][col + 1] = d[2];
        b[4][col + 2] = d[1];
        b[5][col] = d[2];
        b[5][col + 2] = d[0];
    }
    b
}

/// `B̂ᵀ C B̂` at one local point — the integrand, degree ≤2 per variable.
fn integrand(c: &[[f64; 6]; 6], x: f64, y: f64, z: f64) -> [[f64; 24]; 24] {
    let b = b_hat(x, y, z);
    let mut cb = [[0.0f64; 24]; 6];
    for k in 0..6 {
        for j in 0..24 {
            let mut s = 0.0;
            for m in 0..6 {
                s += c[k][m] * b[m][j];
            }
            cb[k][j] = s;
        }
    }
    let mut g = [[0.0f64; 24]; 24];
    for i in 0..24 {
        for j in 0..24 {
            let mut s = 0.0;
            for k in 0..6 {
                s += b[k][i] * cb[k][j];
            }
            g[i][j] = s;
        }
    }
    g
}

/// The 27 monomial coefficient matrices `G_abc` for material tensor `c`.
///
/// The integrand is exactly degree ≤2 per variable, so sampling it at the three
/// nodes {−1, 0, +1} on each axis and inverting the 3×3 Vandermonde per axis
/// recovers the monomial coefficients EXACTLY — no quadrature error, no symbolic
/// algebra. For nodes {−1,0,1}: `c₀ = f(0)`, `c₁ = (f(1)−f(−1))/2`,
/// `c₂ = (f(1)+f(−1))/2 − f(0)`.
pub fn moment_coeffs(c: &[[f64; 6]; 6]) -> Vec<[[f64; 24]; 24]> {
    let nodes = [-1.0f64, 0.0, 1.0];
    // Sample the integrand on the 3×3×3 node lattice.
    let mut f = vec![[[0.0f64; 24]; 24]; 27];
    for (p, &x) in nodes.iter().enumerate() {
        for (q, &y) in nodes.iter().enumerate() {
            for (r, &z) in nodes.iter().enumerate() {
                f[p * 9 + q * 3 + r] = integrand(c, x, y, z);
            }
        }
    }
    // Apply the inverse Vandermonde along each axis in turn.
    let along = |src: &Vec<[[f64; 24]; 24]>, stride: usize| -> Vec<[[f64; 24]; 24]> {
        let mut out = vec![[[0.0f64; 24]; 24]; 27];
        for idx in 0..27 {
            // Decompose the index and only process the "0" slot of this axis.
            let digit = (idx / stride) % 3;
            if digit != 0 {
                continue;
            }
            let (i0, i1, i2) = (idx, idx + stride, idx + 2 * stride);
            for a in 0..24 {
                for b in 0..24 {
                    let (fm, f0, fp) = (src[i0][a][b], src[i1][a][b], src[i2][a][b]);
                    out[i0][a][b] = f0;
                    out[i1][a][b] = (fp - fm) / 2.0;
                    out[i2][a][b] = (fp + fm) / 2.0 - f0;
                }
            }
        }
        out
    };
    let t = along(&f, 9); // a axis
    let t = along(&t, 3); // b axis
    along(&t, 1) // c axis
}

/// Exact monomial moments of an axis-aligned sub-box of the reference cell.
fn box_moments(lo: [f64; 3], hi: [f64; 3]) -> CellMoments {
    // ∫_lo^hi t^k dt = (hi^{k+1} − lo^{k+1}) / (k+1)
    let ax = |l: f64, u: f64| [u - l, (u * u - l * l) / 2.0, (u.powi(3) - l.powi(3)) / 3.0];
    let (ix, iy, iz) = (ax(lo[0], hi[0]), ax(lo[1], hi[1]), ax(lo[2], hi[2]));
    let mut m = [0.0f64; 27];
    for a in 0..3 {
        for b in 0..3 {
            for c in 0..3 {
                m[a * 9 + b * 3 + c] = ix[a] * iy[b] * iz[c];
            }
        }
    }
    m
}

/// Geometric moments of `cell ∩ part`, by adaptive octree subdivision.
///
/// `origin` is the cell's minimum corner in world space and `h` its edge;
/// `inside` is the part indicator in WORLD coordinates (the same generalized
/// winding-number test the voxelizer uses). Sub-boxes that are entirely in or
/// entirely out are integrated in closed form and never subdivided, so the cost
/// is proportional to surface area, not volume.
pub fn cut_moments(
    origin: [f64; 3],
    h: f64,
    inside: &dyn Fn([f64; 3]) -> bool,
    depth: u32,
) -> CellMoments {
    // local ξ ∈ [-1,1]  →  world
    let world = |q: [f64; 3]| {
        [
            origin[0] + (q[0] + 1.0) * 0.5 * h,
            origin[1] + (q[1] + 1.0) * 0.5 * h,
            origin[2] + (q[2] + 1.0) * 0.5 * h,
        ]
    };
    let mut acc = [0.0f64; 27];
    subdivide(&world, inside, [-1.0; 3], [1.0; 3], depth, &mut acc);
    acc
}

fn subdivide(
    world: &dyn Fn([f64; 3]) -> [f64; 3],
    inside: &dyn Fn([f64; 3]) -> bool,
    lo: [f64; 3],
    hi: [f64; 3],
    depth: u32,
    acc: &mut CellMoments,
) {
    // 8 corners + centre: enough to detect a boundary crossing at this scale.
    let mut n_in = 0u32;
    for i in 0..8 {
        let q = [
            if i & 1 == 0 { lo[0] } else { hi[0] },
            if i & 2 == 0 { lo[1] } else { hi[1] },
            if i & 4 == 0 { lo[2] } else { hi[2] },
        ];
        if inside(world(q)) {
            n_in += 1;
        }
    }
    let mid = [
        0.5 * (lo[0] + hi[0]),
        0.5 * (lo[1] + hi[1]),
        0.5 * (lo[2] + hi[2]),
    ];
    let centre_in = inside(world(mid));
    let all_in = n_in == 8 && centre_in;
    let all_out = n_in == 0 && !centre_in;

    if all_in {
        let m = box_moments(lo, hi);
        for k in 0..27 {
            acc[k] += m[k];
        }
        return;
    }
    if all_out {
        return;
    }
    if depth == 0 {
        // Terminal: weight the box by the sampled fill fraction. Keeps the
        // volume unbiased at the cut without pretending to resolve it further.
        let frac = (n_in as f64 + if centre_in { 1.0 } else { 0.0 }) / 9.0;
        let m = box_moments(lo, hi);
        for k in 0..27 {
            acc[k] += frac * m[k];
        }
        return;
    }
    for i in 0..8 {
        let (clo, chi) = (
            [
                if i & 1 == 0 { lo[0] } else { mid[0] },
                if i & 2 == 0 { lo[1] } else { mid[1] },
                if i & 4 == 0 { lo[2] } else { mid[2] },
            ],
            [
                if i & 1 == 0 { mid[0] } else { hi[0] },
                if i & 2 == 0 { mid[1] } else { hi[1] },
                if i & 4 == 0 { mid[2] } else { hi[2] },
            ],
        );
        subdivide(world, inside, clo, chi, depth - 1, acc);
    }
}

/// Reconstruct the cut-cell stiffness from its 27 moments.
///
/// `coeffs` comes from [`moment_coeffs`] (material only, computed once per
/// material); `m` is the per-cell geometry. `KE = (h/2) · Σ M_abc G_abc`.
pub fn ke_from_moments(
    coeffs: &[[[f64; 24]; 24]],
    h: f64,
    m: &CellMoments,
) -> [[f64; 24]; 24] {
    let mut ke = [[0.0f64; 24]; 24];
    for k in 0..27 {
        let w = m[k];
        if w == 0.0 {
            continue;
        }
        let g = &coeffs[k];
        for i in 0..24 {
            for j in 0..24 {
                ke[i][j] += w * g[i][j];
            }
        }
    }
    let s = h / 2.0;
    for row in ke.iter_mut() {
        for v in row.iter_mut() {
            *v *= s;
        }
    }
    ke
}

/// Ground-truth cut-cell stiffness by direct adaptive quadrature of `BᵀCB`.
///
/// Slow — it exists so the tests can prove the 27-moment reconstruction is
/// exact rather than merely plausible. Production should use the moment path.
pub fn ke_cut_reference(
    c: &[[f64; 6]; 6],
    origin: [f64; 3],
    h: f64,
    inside: &dyn Fn([f64; 3]) -> bool,
    depth: u32,
) -> [[f64; 24]; 24] {
    let world = |q: [f64; 3]| {
        [
            origin[0] + (q[0] + 1.0) * 0.5 * h,
            origin[1] + (q[1] + 1.0) * 0.5 * h,
            origin[2] + (q[2] + 1.0) * 0.5 * h,
        ]
    };
    let mut ke = [[0.0f64; 24]; 24];
    quad(c, &world, inside, [-1.0; 3], [1.0; 3], depth, &mut ke);
    let s = h / 2.0;
    for row in ke.iter_mut() {
        for v in row.iter_mut() {
            *v *= s;
        }
    }
    ke
}

fn quad(
    c: &[[f64; 6]; 6],
    world: &dyn Fn([f64; 3]) -> [f64; 3],
    inside: &dyn Fn([f64; 3]) -> bool,
    lo: [f64; 3],
    hi: [f64; 3],
    depth: u32,
    acc: &mut [[f64; 24]; 24],
) {
    let mut n_in = 0u32;
    for i in 0..8 {
        let q = [
            if i & 1 == 0 { lo[0] } else { hi[0] },
            if i & 2 == 0 { lo[1] } else { hi[1] },
            if i & 4 == 0 { lo[2] } else { hi[2] },
        ];
        if inside(world(q)) {
            n_in += 1;
        }
    }
    let mid = [
        0.5 * (lo[0] + hi[0]),
        0.5 * (lo[1] + hi[1]),
        0.5 * (lo[2] + hi[2]),
    ];
    let centre_in = inside(world(mid));
    if n_in == 0 && !centre_in {
        return;
    }
    let full = n_in == 8 && centre_in;
    if !full && depth > 0 {
        for i in 0..8 {
            let (clo, chi) = (
                [
                    if i & 1 == 0 { lo[0] } else { mid[0] },
                    if i & 2 == 0 { lo[1] } else { mid[1] },
                    if i & 4 == 0 { lo[2] } else { mid[2] },
                ],
                [
                    if i & 1 == 0 { mid[0] } else { hi[0] },
                    if i & 2 == 0 { mid[1] } else { hi[1] },
                    if i & 4 == 0 { mid[2] } else { hi[2] },
                ],
            );
            quad(c, world, inside, clo, chi, depth - 1, acc);
        }
        return;
    }
    // Integrate this sub-box with 2×2×2 Gauss (exact for the degree-2-per-axis
    // integrand), weighted by the fill fraction at a terminal partial box.
    let frac = if full {
        1.0
    } else {
        (n_in as f64 + if centre_in { 1.0 } else { 0.0 }) / 9.0
    };
    let g = 1.0 / 3.0f64.sqrt();
    let half = [
        0.5 * (hi[0] - lo[0]),
        0.5 * (hi[1] - lo[1]),
        0.5 * (hi[2] - lo[2]),
    ];
    let jw = half[0] * half[1] * half[2] * frac;
    for gp in 0..8 {
        let s = NODE_SIGNS[gp];
        let p = [
            mid[0] + g * s[0] * half[0],
            mid[1] + g * s[1] * half[1],
            mid[2] + g * s[2] * half[2],
        ];
        let f = integrand(c, p[0], p[1], p[2]);
        for i in 0..24 {
            for j in 0..24 {
                acc[i][j] += jw * f[i][j];
            }
        }
    }
}

/// Convenience: isotropic material, cut cell, via the moment path.
pub fn ke_cut_iso(
    e: f64,
    nu: f64,
    origin: [f64; 3],
    h: f64,
    inside: &dyn Fn([f64; 3]) -> bool,
    depth: u32,
) -> [[f64; 24]; 24] {
    let coeffs = moment_coeffs(&iso_stiffness(e, nu));
    let m = cut_moments(origin, h, inside, depth);
    ke_from_moments(&coeffs, h, &m)
}

/// Per-cell cut geometry for a whole grid — the 27 moments of every boundary
/// cell, keyed by cell index.
///
/// Interior and void cells are absent: an interior cell's moments are the exact
/// full-cell set (reproducing `ke_hex`), so storing them would be redundant.
/// Only cells the surface actually crosses carry data, which is why this is
/// sparse and why the cost scales with surface area rather than volume.
///
/// Kept BESIDE `VoxelGrid` rather than inside it so no existing construction
/// site changes and the whole feature stays opt-in — the same discipline the TI
/// overlay uses in `mg.rs`.
#[derive(Clone, Default)]
pub struct CutGeometry {
    /// (cell index, 27 moments) sorted by cell index.
    cells: Vec<(u32, [f32; 27])>,
}

impl CutGeometry {
    /// Compute moments for every cell the surface crosses.
    ///
    /// A cell is treated as cut when its own inside/outside classification
    /// disagrees with any of its 6 face neighbours — the same boundary test the
    /// voxelizer already uses to decide where to supersample, so this visits
    /// exactly the cells that pay for supersampling today.
    pub fn build(
        grid: &crate::voxel::VoxelGrid,
        inside: &dyn Fn([f64; 3]) -> bool,
        depth: u32,
    ) -> Self {
        let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
        let centre_in = |cx: usize, cy: usize, cz: usize| {
            inside([
                grid.origin[0] + (cx as f64 + 0.5) * grid.h,
                grid.origin[1] + (cy as f64 + 0.5) * grid.h,
                grid.origin[2] + (cz as f64 + 0.5) * grid.h,
            ])
        };
        let mut ci_flags = vec![false; nx * ny * nz];
        for cz in 0..nz {
            for cy in 0..ny {
                for cx in 0..nx {
                    ci_flags[(cz * ny + cy) * nx + cx] = centre_in(cx, cy, cz);
                }
            }
        }
        let at = |x: i64, y: i64, z: i64| -> Option<bool> {
            if x < 0 || y < 0 || z < 0 || x >= nx as i64 || y >= ny as i64 || z >= nz as i64 {
                None
            } else {
                Some(ci_flags[((z as usize) * ny + y as usize) * nx + x as usize])
            }
        };

        let mut cells = Vec::new();
        for cz in 0..nz {
            for cy in 0..ny {
                for cx in 0..nx {
                    let ci = (cz * ny + cy) * nx + cx;
                    if grid.scale[ci] <= 0.0 {
                        continue; // void: nothing to integrate
                    }
                    let me = ci_flags[ci];
                    let (x, y, z) = (cx as i64, cy as i64, cz as i64);
                    let boundary = [
                        (x - 1, y, z),
                        (x + 1, y, z),
                        (x, y - 1, z),
                        (x, y + 1, z),
                        (x, y, z - 1),
                        (x, y, z + 1),
                    ]
                    .iter()
                    .any(|&(a, b, c)| at(a, b, c) != Some(me));
                    if !boundary {
                        continue; // fully interior: exact full-cell moments
                    }
                    let origin = [
                        grid.origin[0] + cx as f64 * grid.h,
                        grid.origin[1] + cy as f64 * grid.h,
                        grid.origin[2] + cz as f64 * grid.h,
                    ];
                    let m = cut_moments(origin, grid.h, inside, depth);
                    if m[0] <= 0.0 {
                        continue;
                    }
                    let mut f = [0f32; 27];
                    for k in 0..27 {
                        f[k] = m[k] as f32;
                    }
                    cells.push((ci as u32, f));
                }
            }
        }
        Self { cells }
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Moments of a cut cell, or `None` when the cell is interior/void.
    pub fn get(&self, ci: usize) -> Option<&[f32; 27]> {
        self.cells
            .binary_search_by_key(&(ci as u32), |&(c, _)| c)
            .ok()
            .map(|i| &self.cells[i].1)
    }

    /// Volume fraction implied by the moments (reference-cell volume is 8).
    pub fn occupancy(&self, ci: usize) -> Option<f64> {
        self.get(ci).map(|m| m[0] as f64 / 8.0)
    }

    /// Bytes of per-cell geometry carried. The point of the moment form: 27 f32
    /// per boundary cell instead of a 24×24 element matrix.
    pub fn bytes(&self) -> usize {
        self.cells.len() * (4 + 27 * 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fem::ke_hex;

    fn frob(a: &[[f64; 24]; 24]) -> f64 {
        let mut s = 0.0;
        for r in a.iter() {
            for v in r.iter() {
                s += v * v;
            }
        }
        s.sqrt()
    }
    fn frob_diff(a: &[[f64; 24]; 24], b: &[[f64; 24]; 24]) -> f64 {
        let mut s = 0.0;
        for i in 0..24 {
            for j in 0..24 {
                s += (a[i][j] - b[i][j]).powi(2);
            }
        }
        s.sqrt()
    }

    /// The 27-moment reconstruction must reproduce the existing full-cell
    /// element matrix EXACTLY. If this drifts, the moment basis is wrong.
    #[test]
    fn moments_reproduce_the_full_cell_ke() {
        let (e, nu, h) = (2000.0, 0.3, 0.7);
        let all_in = |_: [f64; 3]| true;
        let got = ke_cut_iso(e, nu, [0.0; 3], h, &all_in, 0);
        let want = ke_hex(e, nu, h);
        let rel = frob_diff(&got, &want) / frob(&want);
        assert!(rel < 1e-12, "full-cell moment KE differs by {rel:.3e} (relative Frobenius)");
    }

    /// The moment path and direct quadrature must agree on a genuinely cut
    /// cell. This is what licenses storing 27 numbers instead of a 24×24 matrix.
    #[test]
    fn moment_path_matches_direct_quadrature_on_a_cut_cell() {
        let (e, nu, h) = (2000.0, 0.3, 1.0);
        let c = iso_stiffness(e, nu);
        let coeffs = moment_coeffs(&c);
        // An oblique plane cut — not aligned with any axis or diagonal.
        let cut = |p: [f64; 3]| 0.43 * p[0] + 0.81 * p[1] + 0.40 * p[2] < 0.77;
        for depth in [2u32, 4] {
            let m = cut_moments([0.0; 3], h, &cut, depth);
            let via_moments = ke_from_moments(&coeffs, h, &m);
            let direct = ke_cut_reference(&c, [0.0; 3], h, &cut, depth);
            let rel = frob_diff(&via_moments, &direct) / frob(&direct);
            assert!(
                rel < 1e-10,
                "depth {depth}: moment path vs direct quadrature differ by {rel:.3e}"
            );
        }
    }

    /// A cut-cell KE must still annihilate all six rigid-body modes — otherwise
    /// the element stores energy for a free motion and the whole solve is
    /// poisoned. True for ANY integration region because B·(rigid) = 0
    /// pointwise, but it is exactly the property a bad quadrature destroys.
    #[test]
    fn cut_cell_ke_preserves_rigid_body_modes() {
        let (e, nu, h) = (1500.0, 0.32, 0.9);
        let cut = |p: [f64; 3]| p[0] * p[0] + p[1] * p[1] + p[2] * p[2] > 0.30;
        let ke = ke_cut_iso(e, nu, [-0.45, -0.45, -0.45], h, &cut, 4);
        let scale = frob(&ke);

        // 3 translations + 3 rotations, sampled at the node positions.
        let mut modes: Vec<[f64; 24]> = Vec::new();
        for axis in 0..3 {
            let mut m = [0.0f64; 24];
            for l in 0..8 {
                m[3 * l + axis] = 1.0;
            }
            modes.push(m);
        }
        for axis in 0..3 {
            let mut m = [0.0f64; 24];
            for l in 0..8 {
                let p = NODE_SIGNS[l];
                // ω × r for ω along `axis`
                let (i, j) = ((axis + 1) % 3, (axis + 2) % 3);
                m[3 * l + i] = -p[j];
                m[3 * l + j] = p[i];
            }
            modes.push(m);
        }
        for (k, m) in modes.iter().enumerate() {
            let mut r = 0.0f64;
            for i in 0..24 {
                let mut s = 0.0;
                for j in 0..24 {
                    s += ke[i][j] * m[j];
                }
                r += s * s;
            }
            let rel = r.sqrt() / scale;
            assert!(rel < 1e-12, "rigid mode {k} stores energy: |KE·m|/‖KE‖ = {rel:.3e}");
        }
    }

    /// Grid-level sanity: on a sphere the moments must reproduce the occupancy
    /// the voxelizer computed independently, only boundary cells may be stored,
    /// and the memory has to stay in the "27 numbers" regime rather than the
    /// "24×24 matrix" regime.
    #[test]
    fn cut_geometry_matches_voxelizer_occupancy_on_a_sphere() {
        use crate::bvh::WindingBvh;
        use crate::mesh::primitives;
        use crate::voxel::VoxelGrid;

        let mesh = primitives::sphere([0.0; 3], 8.0, 96, 48);
        let grid = VoxelGrid::voxelize(&mesh, 1.0);
        let bvh = WindingBvh::build(&mesh);
        let inside = |q: [f64; 3]| bvh.winding_number(q).abs() >= 0.5;
        let cg = CutGeometry::build(&grid, &inside, 4);

        assert!(!cg.is_empty(), "a sphere must have cut cells");
        // Only boundary cells are stored — far fewer than the solid count.
        let solid = grid.solid_count();
        assert!(
            cg.len() < solid,
            "stored {} cut cells for {solid} solid cells — should be surface-only",
            cg.len()
        );

        // Moment-derived occupancy must track the voxelizer's supersampled one.
        let mut worst = 0f64;
        let mut n = 0usize;
        for (ci, _) in cg.cells.iter() {
            let ci = *ci as usize;
            let vox = grid.scale[ci] as f64;
            let mom = cg.occupancy(ci).unwrap();
            worst = worst.max((vox - mom).abs());
            n += 1;
        }
        assert!(n > 100, "expected many cut cells, got {n}");
        assert!(
            worst < 0.12,
            "moment occupancy disagrees with the voxelizer by {worst:.3} (max)"
        );

        // The memory claim: ~112 B per cut cell, vs 1200 B for a packed
        // symmetric 24×24 f32 element matrix.
        let per_cell = cg.bytes() as f64 / cg.len() as f64;
        assert!(per_cell < 200.0, "per-cut-cell geometry is {per_cell:.0} B");
        println!(
            "\n  sphere r=8 @h=1: {} cut cells of {solid} solid, {:.0} kB total ({:.0} B/cell)",
            cg.len(),
            cg.bytes() as f64 / 1024.0,
            per_cell
        );
    }

    /// THE MEASUREMENT that justifies wiring this into the solver: how far is
    /// the shipping `occupancy · KE_full` from the true cut-cell stiffness?
    /// Reported as a relative Frobenius distance for a range of plane cuts.
    #[test]
    fn report_occupancy_scaling_error() {
        let (e, nu, h) = (2000.0, 0.3, 1.0);
        let full = ke_hex(e, nu, h);
        println!("\n=== occupancy·KE_full vs the true cut-cell KE ===");
        println!("  plane cuts through a unit cell; error = ‖KE_true − occ·KE_full‖_F / ‖KE_true‖_F");
        println!("    {:<26} {:>10} {:>12}", "cut", "occupancy", "shape error");
        let cases: [(&str, [f64; 3]); 4] = [
            ("axis-aligned  (1,0,0)", [1.0, 0.0, 0.0]),
            ("face diagonal (1,1,0)", [1.0, 1.0, 0.0]),
            ("body diagonal (1,1,1)", [1.0, 1.0, 1.0]),
            ("oblique (0.43,0.81,0.40)", [0.43, 0.81, 0.40]),
        ];
        let mut worst = 0f64;
        for (name, nrm) in cases {
            let len = (nrm[0] * nrm[0] + nrm[1] * nrm[1] + nrm[2] * nrm[2]).sqrt();
            let u = [nrm[0] / len, nrm[1] / len, nrm[2] / len];
            for &off in &[-0.5f64, 0.0, 0.5] {
                let cut = move |p: [f64; 3]| {
                    // p is world; the cell spans [-0.5, 0.5]³ here.
                    u[0] * p[0] + u[1] * p[1] + u[2] * p[2] < off * 0.5
                };
                let m = cut_moments([-0.5, -0.5, -0.5], h, &cut, 5);
                let occ = m[0] / 8.0; // reference-cell volume fraction
                if occ < 1e-6 {
                    continue;
                }
                let coeffs = moment_coeffs(&iso_stiffness(e, nu));
                let truth = ke_from_moments(&coeffs, h, &m);
                let mut ersatz = full;
                for row in ersatz.iter_mut() {
                    for v in row.iter_mut() {
                        *v *= occ;
                    }
                }
                let rel = frob_diff(&truth, &ersatz) / frob(&truth);
                worst = worst.max(rel);
                println!("    {:<26} {occ:>10.3} {:>11.1}%", name, rel * 100.0);
            }
        }
        println!("\n  worst shape error: {:.1}%", worst * 100.0);
        // Guard: if this ever collapses toward zero the ersatz model would be
        // adequate and this module would be dead weight — that would be news.
        assert!(worst > 0.05, "occupancy scaling looks adequate ({worst:.3}) — re-evaluate");
    }
}
