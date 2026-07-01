// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Least-squares cylinder fit for identifying cylindrical surface selections.
//!
//! Bearing loads (Ansys-style: a pin pushing the wall of a bore) only make
//! sense on a cylindrical face, so a selection has to be *verified* cylindrical
//! and its axis + radius extracted before the cosine-distributed force can be
//! applied. We have no BREP for STL input, so we recover the cylinder from the
//! selected triangles directly:
//!
//! 1. **Axis** from the surface-normal covariance. Every normal on a cylinder is
//!    perpendicular to the axis, so the normals span the plane ⟂ axis and the
//!    axis is the *least-represented* normal direction — the eigenvector of the
//!    smallest eigenvalue of `Σ aᵢ nᵢnᵢᵀ` (area-weighted). A plane (all normals
//!    parallel) or a cone (normals sweep a cone) fail the residual test below.
//! 2. **Radius + center** from an algebraic (Kåsa) circle fit of the points
//!    projected onto the plane ⟂ the axis.
//! 3. **Residual** = RMS radial deviation / radius — the cylindricity score the
//!    caller thresholds (planes/cones/spheres score high and are rejected).

/// Default acceptance threshold on `Cylinder::residual` (RMS radial deviation as
/// a fraction of the radius). Tight enough to reject planes/cones, loose enough
/// to tolerate a faceted STL cylinder's chordal deviation.
pub const DEFAULT_TOL: f64 = 0.07;

#[derive(Clone, Debug)]
pub struct Cylinder {
    /// Unit axis direction.
    pub axis: [f64; 3],
    /// A point on the axis (the fitted circle center, lifted to 3D).
    pub point: [f64; 3],
    /// Fitted radius (mm).
    pub radius: f64,
    /// RMS radial deviation divided by the radius — 0 is a perfect cylinder.
    pub residual: f64,
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn norm(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

/// Fit a cylinder to surface samples (one per selected triangle): `points` are
/// triangle centroids, `normals` unit triangle normals, `weights` triangle
/// areas (heavier triangles count more). Returns `None` for degenerate input
/// (too few samples, collinear normals, zero radius). The `residual` field is
/// always populated so the caller can accept/reject against [`DEFAULT_TOL`].
pub fn fit(points: &[[f64; 3]], normals: &[[f64; 3]], weights: &[f64]) -> Option<Cylinder> {
    let n = points.len();
    if n < 6 || normals.len() != n || weights.len() != n {
        return None;
    }

    // --- 1. Axis = smallest-eigenvalue eigenvector of the normal covariance.
    let mut cov = [[0f64; 3]; 3];
    let mut wsum = 0f64;
    for i in 0..n {
        let w = weights[i].max(0.0);
        let nm = normals[i];
        let len = norm(nm);
        if len < 1e-12 || w <= 0.0 {
            continue;
        }
        let u = [nm[0] / len, nm[1] / len, nm[2] / len];
        wsum += w;
        for r in 0..3 {
            for c in 0..3 {
                cov[r][c] += w * u[r] * u[c];
            }
        }
    }
    if wsum <= 0.0 {
        return None;
    }
    let (evals, evecs) = jacobi_eigen(cov);
    // Index of the smallest eigenvalue → axis direction.
    let mut imin = 0;
    for i in 1..3 {
        if evals[i] < evals[imin] {
            imin = i;
        }
    }
    let axis = normalize([evecs[0][imin], evecs[1][imin], evecs[2][imin]])?;

    // --- 2. Circle fit in the plane ⟂ axis (basis u, v).
    let (u, v) = ortho_basis(axis);
    // Project around the area-weighted centroid for numerical centering.
    let mut c0 = [0f64; 3];
    for i in 0..n {
        let w = weights[i].max(0.0);
        for d in 0..3 {
            c0[d] += w * points[i][d];
        }
    }
    for d in 0..3 {
        c0[d] /= wsum;
    }
    // Kåsa algebraic circle fit: minimise Σ w (x²+y² − D·x − E·y − F)².
    // Normal equations for [D, E, F]; center = (D/2, E/2), r² = F + center².
    let mut a = [[0f64; 3]; 3];
    let mut b = [0f64; 3];
    for i in 0..n {
        let w = weights[i].max(0.0);
        let d = sub(points[i], c0);
        let x = dot(d, u);
        let y = dot(d, v);
        let z = x * x + y * y;
        let row = [x, y, 1.0];
        for r in 0..3 {
            for cc in 0..3 {
                a[r][cc] += w * row[r] * row[cc];
            }
            b[r] += w * row[r] * z;
        }
    }
    let sol = solve3(a, b)?;
    let (cx, cy) = (sol[0] * 0.5, sol[1] * 0.5);
    let r2 = sol[2] + cx * cx + cy * cy;
    if !(r2 > 0.0) {
        return None;
    }
    let radius = r2.sqrt();
    if radius < 1e-9 {
        return None;
    }
    // Center on the axis, lifted back to 3D.
    let point = [
        c0[0] + cx * u[0] + cy * v[0],
        c0[1] + cx * u[1] + cy * v[1],
        c0[2] + cx * u[2] + cy * v[2],
    ];

    // --- 3. Residual: RMS of (radial distance − radius), normalised by radius.
    let mut acc = 0f64;
    let mut aw = 0f64;
    for i in 0..n {
        let w = weights[i].max(0.0);
        let d = sub(points[i], point);
        let axial = dot(d, axis);
        let radial = [
            d[0] - axial * axis[0],
            d[1] - axial * axis[1],
            d[2] - axial * axis[2],
        ];
        let dev = norm(radial) - radius;
        acc += w * dev * dev;
        aw += w;
    }
    let residual = if aw > 0.0 { (acc / aw).sqrt() / radius } else { f64::INFINITY };

    Some(Cylinder { axis, point, radius, residual })
}

fn normalize(a: [f64; 3]) -> Option<[f64; 3]> {
    let l = norm(a);
    if l < 1e-12 {
        None
    } else {
        Some([a[0] / l, a[1] / l, a[2] / l])
    }
}

/// Any orthonormal pair spanning the plane perpendicular to unit vector `n`.
fn ortho_basis(n: [f64; 3]) -> ([f64; 3], [f64; 3]) {
    // Pick the world axis least aligned with n to avoid a near-zero cross.
    let seed = if n[0].abs() <= n[1].abs() && n[0].abs() <= n[2].abs() {
        [1.0, 0.0, 0.0]
    } else if n[1].abs() <= n[2].abs() {
        [0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    };
    let u = normalize(cross(n, seed)).unwrap_or([1.0, 0.0, 0.0]);
    let v = cross(n, u);
    (u, v)
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Solve a 3×3 linear system `A x = b` via Cramer's rule; `None` if singular.
pub fn solve3(a: [[f64; 3]; 3], b: [f64; 3]) -> Option<[f64; 3]> {
    let det = det3(a);
    if det.abs() < 1e-18 {
        return None;
    }
    let mut x = [0f64; 3];
    for col in 0..3 {
        let mut m = a;
        for r in 0..3 {
            m[r][col] = b[r];
        }
        x[col] = det3(m) / det;
    }
    Some(x)
}

fn det3(m: [[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// Cyclic Jacobi eigensolver for a 3×3 symmetric matrix. Returns eigenvalues
/// and eigenvectors as columns of the returned 3×3 (so eigenvector k is
/// `[v[0][k], v[1][k], v[2][k]]`).
fn jacobi_eigen(mut a: [[f64; 3]; 3]) -> ([f64; 3], [[f64; 3]; 3]) {
    let mut v = [[0f64; 3]; 3];
    for i in 0..3 {
        v[i][i] = 1.0;
    }
    for _ in 0..50 {
        // Largest off-diagonal magnitude.
        let mut p = 0;
        let mut q = 1;
        let mut max = a[0][1].abs();
        for (i, j) in [(0, 2), (1, 2)] {
            if a[i][j].abs() > max {
                max = a[i][j].abs();
                p = i;
                q = j;
            }
        }
        if max < 1e-15 {
            break;
        }
        let app = a[p][p];
        let aqq = a[q][q];
        let apq = a[p][q];
        // Jacobi rotation angle: theta = 0.5*atan2(2*apq, app-aqq).
        let theta = 0.5 * (2.0 * apq).atan2(app - aqq);
        let (s, c) = theta.sin_cos();
        // Apply the rotation J^T A J.
        let mut b = a;
        for k in 0..3 {
            b[k][p] = c * a[k][p] + s * a[k][q];
            b[k][q] = -s * a[k][p] + c * a[k][q];
        }
        let mut d = b;
        for k in 0..3 {
            d[p][k] = c * b[p][k] + s * b[q][k];
            d[q][k] = -s * b[p][k] + c * b[q][k];
        }
        a = d;
        // Accumulate the eigenvectors.
        let mut nv = v;
        for k in 0..3 {
            nv[k][p] = c * v[k][p] + s * v[k][q];
            nv[k][q] = -s * v[k][p] + c * v[k][q];
        }
        v = nv;
    }
    ([a[0][0], a[1][1], a[2][2]], v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Sample a cylinder of given axis/radius and check it's recovered.
    #[test]
    fn fits_a_clean_cylinder() {
        let radius = 5.0;
        let mut pts = Vec::new();
        let mut nrm = Vec::new();
        let mut w = Vec::new();
        // Axis along Z, center at (1, 2, *).
        for iz in 0..10 {
            for ia in 0..24 {
                let th = ia as f64 / 24.0 * 2.0 * PI;
                let (x, y) = (1.0 + radius * th.cos(), 2.0 + radius * th.sin());
                let z = iz as f64 * 0.7;
                pts.push([x, y, z]);
                nrm.push([th.cos(), th.sin(), 0.0]);
                w.push(1.0);
            }
        }
        let c = fit(&pts, &nrm, &w).expect("fit");
        assert!(c.residual < 1e-6, "residual {}", c.residual);
        assert!((c.radius - radius).abs() < 1e-6);
        assert!(c.axis[2].abs() > 0.999, "axis {:?}", c.axis);
    }

    #[test]
    fn rejects_a_plane() {
        // A flat patch in z=0: all normals +Z → not cylindrical.
        let mut pts = Vec::new();
        let mut nrm = Vec::new();
        let mut w = Vec::new();
        for i in 0..8 {
            for j in 0..8 {
                pts.push([i as f64, j as f64, 0.0]);
                nrm.push([0.0, 0.0, 1.0]);
                w.push(1.0);
            }
        }
        // A plane either fails to fit or fits with a huge residual.
        let bad = match fit(&pts, &nrm, &w) {
            None => true,
            Some(c) => c.residual > DEFAULT_TOL,
        };
        assert!(bad);
    }
}
