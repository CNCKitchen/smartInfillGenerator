// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Per-cell stress/strain evaluation from a displacement field.
//!
//! Strains are evaluated at cell centers (the superconvergent point of the
//! trilinear hex), where dN_l/dx_i = s_li / (4h). Stresses use the isotropic
//! law sigma = lambda tr(eps) I + 2 mu eps with the CELL's effective Young's
//! modulus E = e0 * eps_cell — for binned-infill results that is the
//! homogenized (macro) stress of the graded cell, for plain solves it is the
//! solid material stress.

use crate::fem::{NODE_OFFSETS, NODE_SIGNS};
use crate::voxel::VoxelGrid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// von Mises stress (MPa).
    VonMises,
    /// von Mises stress carrying the sign of the dominant (largest-magnitude)
    /// principal stress: + in tension, − in compression (MPa).
    SignedVonMises,
    Sxx,
    Syy,
    Szz,
    Sxy,
    Syz,
    Szx,
    /// Equivalent (von Mises) strain, sqrt(2/3 e_dev : e_dev).
    EVonMises,
    Exx,
    Eyy,
    Ezz,
    Gxy,
    Gyz,
    Gzx,
}

impl FieldKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "vm" => Self::VonMises,
            "svm" => Self::SignedVonMises,
            "sxx" => Self::Sxx,
            "syy" => Self::Syy,
            "szz" => Self::Szz,
            "sxy" => Self::Sxy,
            "syz" => Self::Syz,
            "szx" => Self::Szx,
            "evm" => Self::EVonMises,
            "exx" => Self::Exx,
            "eyy" => Self::Eyy,
            "ezz" => Self::Ezz,
            "gxy" => Self::Gxy,
            "gyz" => Self::Gyz,
            "gzx" => Self::Gzx,
            _ => return None,
        })
    }

    pub fn is_stress(&self) -> bool {
        matches!(
            self,
            Self::VonMises
                | Self::SignedVonMises
                | Self::Sxx
                | Self::Syy
                | Self::Szz
                | Self::Sxy
                | Self::Syz
                | Self::Szx
        )
    }
}

/// Sign (+1.0 / −1.0) of the principal stress with the largest magnitude — the
/// convention that gives the always-positive von Mises stress a tension (+) /
/// compression (−) sign. Returns +1.0 for a (near-)zero or purely deviatoric
/// tensor where the sign is immaterial.
///
/// Principal stresses are the eigenvalues of the symmetric stress tensor;
/// computed here in closed form (Smith's trigonometric method for a symmetric
/// 3×3), which only needs the extreme eigenvalues σ₁ (max) and σ₃ (min).
fn signed_vm_sign(sxx: f64, syy: f64, szz: f64, sxy: f64, syz: f64, szx: f64) -> f64 {
    let p1 = sxy * sxy + syz * syz + szx * szx;
    let q = (sxx + syy + szz) / 3.0; // mean (hydrostatic) stress
    let (smax, smin) = if p1 <= 1e-30 {
        // Diagonal tensor: the principal stresses are the diagonal entries.
        (sxx.max(syy).max(szz), sxx.min(syy).min(szz))
    } else {
        let p2 =
            (sxx - q).powi(2) + (syy - q).powi(2) + (szz - q).powi(2) + 2.0 * p1;
        let p = (p2 / 6.0).sqrt();
        // r = det((A − qI)/p) / 2, in [−1, 1] up to rounding.
        let (bxx, byy, bzz) = ((sxx - q) / p, (syy - q) / p, (szz - q) / p);
        let (bxy, byz, bzx) = (sxy / p, syz / p, szx / p);
        let det = bxx * (byy * bzz - byz * byz) - bxy * (bxy * bzz - byz * bzx)
            + bzx * (bxy * byz - byy * bzx);
        let r = (det / 2.0).clamp(-1.0, 1.0);
        let phi = r.acos() / 3.0;
        let smax = q + 2.0 * p * phi.cos();
        let smin = q + 2.0 * p * (phi + 2.0 * std::f64::consts::PI / 3.0).cos();
        (smax, smin)
    };
    let dominant = if smax.abs() >= smin.abs() { smax } else { smin };
    if dominant < 0.0 {
        -1.0
    } else {
        1.0
    }
}

/// Volume-averaged nodal recovery of a per-cell field: each node gets the
/// mean of its adjacent SOLID cells' values (all cells share one volume, so
/// the volume weighting is a plain mean). This is the standard remedy for
/// the staircase checkerboard of voxel surface stresses — cell-center values
/// are individually accurate (superconvergent point); the noise lives in the
/// flat per-cell pattern between them. Nodes touching no solid cell get NaN
/// so samplers can renormalize around them. Returns (nx+1)(ny+1)(nz+1)
/// values, node index (z*(ny+1) + y)*(nx+1) + x.
pub fn recover_nodal(grid: &VoxelGrid, cell_values: &[f32]) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let mut sum = vec![0f32; mx * my * mz];
    let mut count = vec![0u16; mx * my * mz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if grid.scale[ci] <= 0.0 {
                    continue;
                }
                let v = cell_values[ci];
                for oz in 0..2 {
                    for oy in 0..2 {
                        for ox in 0..2 {
                            let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
                            sum[n] += v;
                            count[n] += 1;
                        }
                    }
                }
            }
        }
    }
    for n in 0..sum.len() {
        sum[n] = if count[n] > 0 { sum[n] / count[n] as f32 } else { f32::NAN };
    }
    sum
}

/// Occupancy-decoupled (material) modulus/strength factor per cell.
///
/// The solve scales each cell's stiffness by `eps = occupancy × material
/// density`, where `grid.scale` is the finite-cell geometric occupancy (for a
/// plain solid solve `eps == grid.scale`). That occupancy scaling is correct
/// for stiffness and mass, but a cut boundary cell is *fully dense material
/// partially covering its cube* — scaling its stress by the occupancy
/// under-reads the true material stress and paints the staircase stripes seen
/// on curved skins. This returns `eps / occupancy` (clamped to 1): the material
/// density factor alone (1 for solid/skin, rel(ρ) for graded infill), so a
/// stress evaluated with it is the TRUE material / homogenized macro stress
/// with the meshing artifact removed. Void cells (occupancy 0) stay 0.
///
/// Feed this to `cell_field` in place of `eps`. Using the SAME factor for the
/// SF allowable leaves the safety factor unchanged (the factor cancels in
/// allowable / stress).
pub fn material_factor(grid: &VoxelGrid, eps: &[f32]) -> Vec<f32> {
    eps.iter()
        .zip(&grid.scale)
        .map(|(&e, &occ)| if occ > 0.0 { (e / occ).min(1.0) } else { 0.0 })
        .collect()
}

/// Selected scalar per cell (cell-center evaluation); 0.0 for void cells.
/// `u` is the padded nodal displacement field (3 per node), `eps` the
/// per-cell stiffness factors actually used in the solve.
pub fn cell_field(
    grid: &VoxelGrid,
    u: &[f32],
    e0: f64,
    nu: f64,
    eps: &[f32],
    kind: FieldKind,
) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let inv4h = 1.0 / (4.0 * grid.h);
    let mut out = vec![0f32; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if eps[ci] <= 0.0 {
                    continue;
                }
                // Strain at the cell center.
                let (mut exx, mut eyy, mut ezz) = (0f64, 0f64, 0f64);
                let (mut gxy, mut gyz, mut gzx) = (0f64, 0f64, 0f64);
                for l in 0..8 {
                    let [ox, oy, oz] = NODE_OFFSETS[l];
                    let [sx, sy, sz] = NODE_SIGNS[l];
                    let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
                    let (ux, uy, uz) =
                        (u[3 * n] as f64, u[3 * n + 1] as f64, u[3 * n + 2] as f64);
                    exx += sx * ux;
                    eyy += sy * uy;
                    ezz += sz * uz;
                    gxy += sy * ux + sx * uy;
                    gyz += sz * uy + sy * uz;
                    gzx += sx * uz + sz * ux;
                }
                exx *= inv4h;
                eyy *= inv4h;
                ezz *= inv4h;
                gxy *= inv4h;
                gyz *= inv4h;
                gzx *= inv4h;

                let v = match kind {
                    FieldKind::Exx => exx,
                    FieldKind::Eyy => eyy,
                    FieldKind::Ezz => ezz,
                    FieldKind::Gxy => gxy,
                    FieldKind::Gyz => gyz,
                    FieldKind::Gzx => gzx,
                    FieldKind::EVonMises => {
                        // sqrt(2/3 e_dev : e_dev) with tensor shear e_ij = g_ij/2.
                        let tr = (exx + eyy + ezz) / 3.0;
                        let (dx, dy, dz) = (exx - tr, eyy - tr, ezz - tr);
                        let dev2 = dx * dx
                            + dy * dy
                            + dz * dz
                            + 0.5 * (gxy * gxy + gyz * gyz + gzx * gzx);
                        (2.0 / 3.0 * dev2).sqrt()
                    }
                    _ => {
                        let e = e0 * eps[ci] as f64;
                        let lam = e * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
                        let mu = e / (2.0 * (1.0 + nu));
                        let tr = exx + eyy + ezz;
                        let sxx = lam * tr + 2.0 * mu * exx;
                        let syy = lam * tr + 2.0 * mu * eyy;
                        let szz = lam * tr + 2.0 * mu * ezz;
                        let sxy = mu * gxy;
                        let syz = mu * gyz;
                        let szx = mu * gzx;
                        match kind {
                            FieldKind::Sxx => sxx,
                            FieldKind::Syy => syy,
                            FieldKind::Szz => szz,
                            FieldKind::Sxy => sxy,
                            FieldKind::Syz => syz,
                            FieldKind::Szx => szx,
                            _ => {
                                // von Mises (and its signed variant)
                                let vm = (0.5
                                    * ((sxx - syy).powi(2)
                                        + (syy - szz).powi(2)
                                        + (szz - sxx).powi(2))
                                    + 3.0 * (sxy * sxy + syz * syz + szx * szx))
                                    .sqrt();
                                if matches!(kind, FieldKind::SignedVonMises) {
                                    vm * signed_vm_sign(sxx, syy, szz, sxy, syz, szx)
                                } else {
                                    vm
                                }
                            }
                        }
                    }
                };
                out[ci] = v as f32;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cut boundary cell is fully dense material partially filling its cube,
    /// so under a uniform strain field its TRUE stress must equal a full cell's.
    /// `material_factor` delivers that; raw `eps` under-reads it by the
    /// occupancy — the curved-skin "stripe" bug, distilled to two cells
    /// (quarter-cylinder in tension).
    #[test]
    fn material_factor_removes_occupancy_stripe() {
        // 2×1×1 grid: cell 0 fully solid, cell 1 a 40%-occupancy cut cell.
        let h = 2.0;
        let mut grid = VoxelGrid::solid_box(2, 1, 1, h);
        grid.scale[1] = 0.4;
        let eps = grid.scale.clone(); // plain solid solve: eps == occupancy

        // Linear field u_x = a·X (u_y = u_z = 0) ⇒ uniform ε_xx = a in BOTH
        // cells, independent of occupancy.
        let a = 0.01f32;
        let (mx, my, mz) = (3usize, 2, 2);
        let mut u = vec![0f32; 3 * mx * my * mz];
        for nz in 0..mz {
            for ny in 0..my {
                for nx in 0..mx {
                    let n = (nz * my + ny) * mx + nx;
                    u[3 * n] = a * (nx as f32 * h as f32);
                }
            }
        }

        let (e0, nu) = (1000.0, 0.0);
        let raw = cell_field(&grid, &u, e0, nu, &eps, FieldKind::Sxx);
        let mat = cell_field(&grid, &u, e0, nu, &material_factor(&grid, &eps), FieldKind::Sxx);

        // Raw: the cut cell under-reads by its occupancy (10 vs 4 MPa) — stripe.
        assert!((raw[0] - 10.0).abs() < 1e-3, "raw full cell {}", raw[0]);
        assert!((raw[1] - 4.0).abs() < 1e-3, "raw cut cell {}", raw[1]);
        // Decoupled: both cells report the true material stress, uniform.
        assert!((mat[0] - 10.0).abs() < 1e-3, "mat full cell {}", mat[0]);
        assert!((mat[1] - 10.0).abs() < 1e-3, "mat cut cell {}", mat[1]);
        assert!((mat[0] - mat[1]).abs() < 1e-4, "decoupled stress must be uniform");
    }

    /// Signed von Mises must equal the plain von Mises in magnitude and carry
    /// the sign of the load: + under tension, − under compression.
    #[test]
    fn signed_von_mises_carries_tension_compression_sign() {
        let h = 2.0;
        let grid = VoxelGrid::solid_box(1, 1, 1, h);
        let eps = grid.scale.clone();
        let (mx, my, mz) = (2usize, 2, 2);
        let (e0, nu) = (1000.0, 0.0);

        // Uniaxial strain field u_x = a·X (u_y = u_z = 0) ⇒ ε_xx = a, a single
        // nonzero principal stress σ_xx with the sign of a.
        let build = |a: f32| {
            let mut u = vec![0f32; 3 * mx * my * mz];
            for nz in 0..mz {
                for ny in 0..my {
                    for nx in 0..mx {
                        let n = (nz * my + ny) * mx + nx;
                        u[3 * n] = a * (nx as f32 * h as f32);
                    }
                }
            }
            u
        };
        let vm = |u: &[f32]| cell_field(&grid, u, e0, nu, &eps, FieldKind::VonMises)[0];
        let svm = |u: &[f32]| cell_field(&grid, u, e0, nu, &eps, FieldKind::SignedVonMises)[0];

        let ut = build(0.01); // tension
        let uc = build(-0.01); // compression
        // Same magnitude as the unsigned von Mises, either sign.
        assert!((svm(&ut).abs() - vm(&ut)).abs() < 1e-3, "|svm| == vm (tension)");
        assert!((svm(&uc).abs() - vm(&uc)).abs() < 1e-3, "|svm| == vm (compression)");
        // Sign tracks the dominant principal stress.
        assert!(svm(&ut) > 0.0, "tension ⇒ +von Mises (got {})", svm(&ut));
        assert!(svm(&uc) < 0.0, "compression ⇒ −von Mises (got {})", svm(&uc));
    }
}
