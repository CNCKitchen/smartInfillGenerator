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
            Self::VonMises | Self::Sxx | Self::Syy | Self::Szz | Self::Sxy | Self::Syz | Self::Szx
        )
    }
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
                                // von Mises
                                (0.5
                                    * ((sxx - syy).powi(2)
                                        + (syy - szz).powi(2)
                                        + (szz - sxx).powi(2))
                                    + 3.0 * (sxy * sxy + syz * syz + szx * szx))
                                    .sqrt()
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
