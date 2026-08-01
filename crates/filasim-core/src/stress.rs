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

pub use crate::eps::material_factor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// von Mises stress (MPa).
    VonMises,
    /// von Mises stress carrying the sign of the first stress invariant
    /// (σxx+σyy+σzz = σ₁+σ₂+σ₃): + in net tension, − in net compression (MPa).
    SignedVonMises,
    Sxx,
    Syy,
    Szz,
    Sxy,
    Syz,
    Szx,
    /// Maximum principal stress σ₁ (MPa) — the largest eigenvalue of the
    /// stress tensor. This, not von Mises, is what a textbook Kt and a
    /// max-normal-stress (brittle) check are defined on.
    S1,
    /// Intermediate principal stress σ₂ (MPa).
    S2,
    /// Minimum principal stress σ₃ (MPa) — the most compressive.
    S3,
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
            "s1" => Self::S1,
            "s2" => Self::S2,
            "s3" => Self::S3,
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
                | Self::S1
                | Self::S2
                | Self::S3
        )
    }
}

/// Principal stresses [σ₁, σ₂, σ₃] — the eigenvalues of the symmetric tensor
/// given in Voigt order [sxx, syy, szz, sxy, syz, szx] — sorted DESCENDING.
///
/// Closed form (the trigonometric solution of the characteristic cubic): no
/// iteration, and exact for an already-diagonal state. σ₂ comes from the trace
/// (σ₁+σ₂+σ₃ = I₁) rather than a third cosine.
pub fn principals(s: [f64; 6]) -> [f64; 3] {
    let [sxx, syy, szz, sxy, syz, szx] = s;
    let p1 = sxy * sxy + syz * syz + szx * szx;
    if p1 == 0.0 {
        let (mut a, mut b, mut c) = (sxx, syy, szz);
        if a < b {
            std::mem::swap(&mut a, &mut b);
        }
        if b < c {
            std::mem::swap(&mut b, &mut c);
        }
        if a < b {
            std::mem::swap(&mut a, &mut b);
        }
        return [a, b, c];
    }
    let q = (sxx + syy + szz) / 3.0;
    let p2 = (sxx - q).powi(2) + (syy - q).powi(2) + (szz - q).powi(2) + 2.0 * p1;
    let p = (p2 / 6.0).sqrt();
    // r = det((A − qI)/p) / 2, clamped against fp drift outside acos' domain.
    let (bxx, byy, bzz) = ((sxx - q) / p, (syy - q) / p, (szz - q) / p);
    let (bxy, byz, bzx) = (sxy / p, syz / p, szx / p);
    let det = bxx * (byy * bzz - byz * byz) - bxy * (bxy * bzz - byz * bzx)
        + bzx * (bxy * byz - byy * bzx);
    let r = (det / 2.0).clamp(-1.0, 1.0);
    let phi = r.acos() / 3.0;
    // Eigenvalues are q + 2p·cos(φ + 2πk/3), φ ∈ [0, π/3]: k = 0 is the
    // largest; k = 1 lands in [2π/3, π] where cos is most negative — the
    // smallest.
    let s1 = q + 2.0 * p * phi.cos();
    let s3 = q + 2.0 * p * (phi + 2.0 * std::f64::consts::FRAC_PI_3).cos();
    [s1, 3.0 * q - s1 - s3, s3]
}

/// Sign (+1.0 / −1.0) for the signed von Mises stress: the sign of the first
/// stress invariant I₁ = σxx + σyy + σzz = σ₁ + σ₂ + σ₃ (the hydrostatic /
/// volumetric part). + = net tension, − = net compression. This is the
/// `(s1+s2+s3)/|s1+s2+s3|` convention; a purely deviatoric state (I₁ = 0, e.g.
/// pure shear) is treated as + rather than the 0/0 = NaN that the bare ratio
/// would give.
fn signed_vm_sign(sxx: f64, syy: f64, szz: f64) -> f64 {
    if sxx + syy + szz < 0.0 {
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
    recover_nodal_where(grid, cell_values, &|_| true)
}

/// `recover_nodal` restricted to cells with `keep(ci)`: dropped cells
/// contribute nothing to their nodes, so a masked display surface (e.g. the
/// Part Topo retained body) doesn't have its boundary values dragged toward
/// the near-void SIMP-floor cells outside it.
pub fn recover_nodal_where(
    grid: &VoxelGrid,
    cell_values: &[f32],
    keep: &dyn Fn(usize) -> bool,
) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my, mz) = (nx + 1, ny + 1, nz + 1);
    let mut sum = vec![0f32; mx * my * mz];
    let mut count = vec![0u16; mx * my * mz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if grid.scale[ci] <= 0.0 || !keep(ci) {
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
    cell_field_eigen(grid, u, e0, nu, eps, [0.0; 3], kind)
}

/// Like [`cell_field`], but subtracts a uniform **eigenstrain** `eigen`
/// (`[εx, εy, εz]`, shear-free) from the total strain before evaluating, so the
/// result is the **residual elastic** state `σ = C : (ε(u) − ε₀)` (and the
/// returned strains are the elastic strains). This is what the build sim needs:
/// the locked-in print stress that drives delamination (σzz tension across
/// layers). With `eigen = [0,0,0]` it is identical to [`cell_field`].
pub fn cell_field_eigen(
    grid: &VoxelGrid,
    u: &[f32],
    e0: f64,
    nu: f64,
    eps: &[f32],
    eigen: [f64; 3],
    kind: FieldKind,
) -> Vec<f32> {
    cell_field_ti(grid, u, e0, nu, eps, None, eigen, kind)
}

/// [`cell_field_eigen`] with a transverse-isotropic infill share (DESIGN §22).
///
/// `ti` is `(infill material factors, ratios)`. When `None` this is the
/// original isotropic evaluation, arithmetic unchanged.
///
/// STRESS MUST FOLLOW STIFFNESS. If the solve ran with a TI overlay and the
/// stress is read back with the isotropic law, every stress and every safety
/// factor is wrong while the solve itself converges perfectly — so the TI
/// eps field has to reach here, not just the solver.
#[allow(clippy::too_many_arguments)]
pub fn cell_field_ti(
    grid: &VoxelGrid,
    u: &[f32],
    e0: f64,
    nu: f64,
    eps: &[f32],
    ti: Option<(&[f32], &crate::ti::TiRatios)>,
    eigen: [f64; 3],
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
                let fi = ti.map_or(0.0, |(f, _)| f[ci] as f64);
                if eps[ci] <= 0.0 && fi <= 0.0 {
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
                // Residual ELASTIC strain = total − eigenstrain (shear-free ε₀).
                exx -= eigen[0];
                eyy -= eigen[1];
                ezz -= eigen[2];

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
                        let (sxx, syy, szz, sxy, syz, szx) = match ti {
                            // Same Voigt blend the element kernel uses, so the
                            // stress is the one the solve actually produced.
                            Some((_, ratios)) => {
                                let s = crate::ti::blended_stress(
                                    e0,
                                    nu,
                                    eps[ci] as f64,
                                    fi,
                                    ratios,
                                    [exx, eyy, ezz, gxy, gyz, gzx],
                                );
                                (s[0], s[1], s[2], s[3], s[4], s[5])
                            }
                            None => {
                                let e = e0 * eps[ci] as f64;
                                let lam = e * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
                                let mu = e / (2.0 * (1.0 + nu));
                                let tr = exx + eyy + ezz;
                                (
                                    lam * tr + 2.0 * mu * exx,
                                    lam * tr + 2.0 * mu * eyy,
                                    lam * tr + 2.0 * mu * ezz,
                                    mu * gxy,
                                    mu * gyz,
                                    mu * gzx,
                                )
                            }
                        };
                        match kind {
                            FieldKind::Sxx => sxx,
                            FieldKind::Syy => syy,
                            FieldKind::Szz => szz,
                            FieldKind::Sxy => sxy,
                            FieldKind::Syz => syz,
                            FieldKind::Szx => szx,
                            FieldKind::S1 | FieldKind::S2 | FieldKind::S3 => {
                                let p = principals([sxx, syy, szz, sxy, syz, szx]);
                                match kind {
                                    FieldKind::S1 => p[0],
                                    FieldKind::S2 => p[1],
                                    _ => p[2],
                                }
                            }
                            _ => {
                                // von Mises (and its signed variant)
                                let vm = (0.5
                                    * ((sxx - syy).powi(2)
                                        + (syy - szz).powi(2)
                                        + (szz - sxx).powi(2))
                                    + 3.0 * (sxy * sxy + syz * syz + szx * szx))
                                    .sqrt();
                                if matches!(kind, FieldKind::SignedVonMises) {
                                    vm * signed_vm_sign(sxx, syy, szz)
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
        // nonzero stress σ_xx (so I₁ = σ_xx) with the sign of a.
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
        // Sign tracks the first invariant I₁ = σxx+σyy+σzz.
        assert!(svm(&ut) > 0.0, "tension ⇒ +von Mises (got {})", svm(&ut));
        assert!(svm(&uc) < 0.0, "compression ⇒ −von Mises (got {})", svm(&uc));

        // Uniaxial: σ₁ IS σxx and the other two principals vanish (ν = 0).
        let p = |u: &[f32], k| cell_field(&grid, u, e0, nu, &eps, k)[0];
        let sxx = p(&ut, FieldKind::Sxx);
        assert!((p(&ut, FieldKind::S1) - sxx).abs() < 1e-3, "σ₁ == σxx in tension");
        assert!(p(&ut, FieldKind::S2).abs() < 1e-3, "σ₂ ≈ 0");
        assert!(p(&ut, FieldKind::S3).abs() < 1e-3, "σ₃ ≈ 0");
        // Compression puts the same magnitude in σ₃ instead — the ordering is
        // by VALUE, not magnitude, which is the whole point of plotting both.
        assert!((p(&uc, FieldKind::S3) - p(&uc, FieldKind::Sxx)).abs() < 1e-3, "σ₃ == σxx");
        assert!(p(&uc, FieldKind::S1).abs() < 1e-3, "σ₁ ≈ 0 under pure compression");
    }

    /// The principal solver against the tensor invariants: eigenvalues sorted
    /// descending, Σσᵢ = I₁ = tr, Σσᵢσⱼ = I₂, Πσᵢ = I₃ = det.
    #[test]
    fn principals_reproduce_the_stress_invariants() {
        let cases: [[f64; 6]; 5] = [
            [5.0, -2.0, 1.0, 0.0, 0.0, 0.0],      // diagonal (shear-free branch)
            [0.0, 0.0, 0.0, 3.0, 0.0, 0.0],       // pure shear ⇒ ±3, 0
            [12.0, -4.0, 7.0, 2.5, -1.5, 0.8],    // general
            [-9.0, -9.0, -9.0, 0.0, 0.0, 0.0],    // hydrostatic (triple root)
            [1.0, 1.0, 1.0, 0.4, 0.4, 0.4],       // repeated eigenvalue with shear
        ];
        for s in cases {
            let [sxx, syy, szz, sxy, syz, szx] = s;
            let p = principals(s);
            assert!(p[0] >= p[1] && p[1] >= p[2], "descending order, got {p:?}");
            let i1 = sxx + syy + szz;
            let i2 = sxx * syy + syy * szz + szz * sxx - sxy * sxy - syz * syz - szx * szx;
            let i3 = sxx * (syy * szz - syz * syz) - sxy * (sxy * szz - syz * szx)
                + szx * (sxy * syz - syy * szx);
            let tol = 1e-9 * i1.abs().max(1.0);
            assert!((p[0] + p[1] + p[2] - i1).abs() < tol, "I₁ for {s:?}: {p:?}");
            assert!(
                (p[0] * p[1] + p[1] * p[2] + p[2] * p[0] - i2).abs() < 1e-8 * i2.abs().max(1.0),
                "I₂ for {s:?}: {p:?}"
            );
            assert!(
                (p[0] * p[1] * p[2] - i3).abs() < 1e-7 * i3.abs().max(1.0),
                "I₃ for {s:?}: {p:?}"
            );
        }
        // Pure shear τ=3 ⇒ (3, 0, −3): the case where von Mises (5.196) and the
        // max principal disagree most.
        let p = principals([0.0, 0.0, 0.0, 3.0, 0.0, 0.0]);
        assert!((p[0] - 3.0).abs() < 1e-9 && p[1].abs() < 1e-9 && (p[2] + 3.0).abs() < 1e-9);
    }
}
