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
    /// Maximum principal stress σ₁ (MPa) — the largest eigenvalue of the stress
    /// tensor. This, not von Mises, is what a textbook Kt and a max-normal-
    /// stress check are defined on, and it is the tensile driver for brittle
    /// and layer-anisotropic materials, where von Mises (a shear criterion)
    /// under-reads a biaxial tensile state.
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

/// Outward surface normal of every cut cell, from the OCCUPANCY gradient
/// (central differences over the six face neighbours, out-of-grid = void).
/// `[0,0,0]` where the gradient is degenerate — an isolated cell, or a fully
/// enclosed one whose neighbours are symmetric.
///
/// This is the direction the finite-cell surface passes through the cell, and
/// it is what makes the occupancy decoupling correct (see
/// [`decouple_traction`]). Occupancy VALUES are differenced rather than a
/// binary in/out mask, so the normal follows the surface sub-cell instead of
/// snapping to an axis on a staircase.
pub fn cut_normals(grid: &VoxelGrid) -> Vec<[f32; 3]> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let mut out = vec![[0f32; 3]; nx * ny * nz];
    let occ = |x: i64, y: i64, z: i64| -> f32 {
        if x < 0 || y < 0 || z < 0 || x >= nx as i64 || y >= ny as i64 || z >= nz as i64 {
            return 0.0;
        }
        grid.scale[(z as usize * ny + y as usize) * nx + x as usize]
    };
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if grid.scale[ci] <= 0.0 {
                    continue;
                }
                let (x, y, z) = (cx as i64, cy as i64, cz as i64);
                // Outward = toward decreasing occupancy = −∇occ.
                let g = [
                    occ(x - 1, y, z) - occ(x + 1, y, z),
                    occ(x, y - 1, z) - occ(x, y + 1, z),
                    occ(x, y, z - 1) - occ(x, y, z + 1),
                ];
                let len = (g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt();
                if len > 1e-6 {
                    out[ci] = [g[0] / len, g[1] / len, g[2] / len];
                }
            }
        }
    }
    out
}

/// Decouple the geometric occupancy from a cut cell's stress **in the
/// directions where that is physically valid**, in place. Voigt order
/// `[σxx, σyy, σzz, σxy, σyz, σzx]`; `n` is the outward cut normal from
/// [`cut_normals`], `occ` the cell's geometric occupancy.
///
/// The caller hands in the stress already divided by the occupancy — the
/// scalar [`crate::eps::material_factor`] correction. That correction is right
/// for a cell cut PARALLEL to the stress (material spans the cell's full
/// length and covers part of its cross-section, so the cell's strain IS the
/// material strain, and the ersatz stress `E₀·occ·ε` under-reads by exactly
/// `occ`). It is wrong for a cell cut PERPENDICULAR to it: there the cell is a
/// soft link in SERIES, equilibrium pushes the same section force through it,
/// its ersatz stress is ALREADY the material stress, and its strain is
/// inflated by `1/occ`. Dividing by the occupancy then applies that factor a
/// second time — +50 % on a 2/3-occupancy column, 6.7× at the
/// `BOUNDARY_FLOOR` of 0.15, in the FAR FIELD, nowhere near a notch
/// (measured: `bin/kirschbench`).
///
/// Which of the two a component gets is decided by the cut normal. Split the
/// tensor into the traction on the cut face and the in-plane block,
/// `σ = PσP + T` with `P = I − n̂n̂ᵀ` and
/// `T = n̂tᵀ + tn̂ᵀ − (n̂ᵀσn̂)n̂n̂ᵀ`, `t = σn̂`. The in-plane block keeps the
/// full `1/occ`; the traction keeps none of it. Since the input is already
/// scaled, that is one subtraction: `σ ← σ − (1−occ)·T`.
///
/// At `occ = 1` (every interior cell, and every part with no cut cells) this
/// is exactly zero — the correction cannot perturb a converged interior.
#[inline]
pub fn decouple_traction(s: &mut [f64; 6], n: [f32; 3], occ: f64) {
    if occ >= 1.0 || n == [0.0; 3] {
        return;
    }
    let n = [n[0] as f64, n[1] as f64, n[2] as f64];
    let (sxx, syy, szz, sxy, syz, szx) = (s[0], s[1], s[2], s[3], s[4], s[5]);
    // Traction on the cut face, t = σ·n̂.
    let t = [
        sxx * n[0] + sxy * n[1] + szx * n[2],
        sxy * n[0] + syy * n[1] + syz * n[2],
        szx * n[0] + syz * n[1] + szz * n[2],
    ];
    let nn = t[0] * n[0] + t[1] * n[1] + t[2] * n[2];
    let w = 1.0 - occ;
    s[0] -= w * (2.0 * n[0] * t[0] - nn * n[0] * n[0]);
    s[1] -= w * (2.0 * n[1] * t[1] - nn * n[1] * n[1]);
    s[2] -= w * (2.0 * n[2] * t[2] - nn * n[2] * n[2]);
    s[3] -= w * (n[0] * t[1] + t[0] * n[1] - nn * n[0] * n[1]);
    s[4] -= w * (n[1] * t[2] + t[1] * n[2] - nn * n[1] * n[2]);
    s[5] -= w * (n[2] * t[0] + t[2] * n[0] - nn * n[2] * n[0]);
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


/// Half-width, in cells, of the patch [`recover_surface`] fits over. 2 gives a
/// 5×5×5 candidate box — enough clean cells for a stable linear fit at every
/// resolution the app runs, while the fit region shrinks with `h` so the
/// recovery converges with the mesh.
pub const SURFACE_PATCH_CELLS: i64 = 2;

/// Is this cell fully solid AND fully surrounded by fully solid cells? These
/// are the cells whose strain is the plain trilinear-hex answer, untouched by
/// the finite-cell boundary — the only ones a recovery should trust.
#[inline]
fn is_clean(grid: &VoxelGrid, cx: usize, cy: usize, cz: usize) -> bool {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let at = |x: i64, y: i64, z: i64| -> bool {
        x >= 0
            && y >= 0
            && z >= 0
            && x < nx as i64
            && y < ny as i64
            && z < nz as i64
            && grid.scale[(z as usize * ny + y as usize) * nx + x as usize] >= 1.0
    };
    let (x, y, z) = (cx as i64, cy as i64, cz as i64);
    at(x, y, z)
        && at(x - 1, y, z)
        && at(x + 1, y, z)
        && at(x, y - 1, z)
        && at(x, y + 1, z)
        && at(x, y, z - 1)
        && at(x, y, z + 1)
}

/// **Surface recovery**: replace every BOUNDARY cell's value by a linear
/// least-squares fit of the clean interior cells around it, evaluated at the
/// boundary cell's own centre. Interior cells pass through untouched.
///
/// The peak of a voxel stress field is read off the cells the staircase runs
/// through, and that maximum is not a convergent quantity: on the Kirsch plate
/// it scatters −37 %…+30 % from 2 484 to 2.5 M cells with no trend, because a
/// voxelised curve is a ring of square steps and the reported peak depends on
/// where the steps happen to fall — effectively random in `h`. The same solve's
/// clean interior converges monotonically (`bin/kirschbench`), so the field is
/// fine and only the last cell is not.
///
/// This is the textbook remedy (superconvergent patch recovery, degree 1): fit
/// `v ≈ a + b·dx + c·dy + d·dz` over the clean cells within
/// [`SURFACE_PATCH_CELLS`], take `a`. Because the patch is one-sided the fit
/// extrapolates outward by a cell or so — which is exactly the step from the
/// last trustworthy sample to the surface. Degenerate patches degrade in
/// order: too few clean cells for a fit → the nearest clean value; none in
/// range → the original value, unchanged.
///
/// Recovering the derived SCALAR rather than the stress tensor is a deliberate
/// simplification — the field pipeline produces one scalar at a time, and the
/// alternative (six recoveries plus a re-derivation) buys accuracy the
/// staircase does not justify.
pub fn recover_surface(grid: &VoxelGrid, cells: &[f32]) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let clean: Vec<bool> = {
        let mut c = vec![false; nx * ny * nz];
        for cz in 0..nz {
            for cy in 0..ny {
                for cx in 0..nx {
                    c[(cz * ny + cy) * nx + cx] = is_clean(grid, cx, cy, cz);
                }
            }
        }
        c
    };
    let r = SURFACE_PATCH_CELLS;
    let mut out = cells.to_vec();
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if grid.scale[ci] <= 0.0 || clean[ci] {
                    continue;
                }
                // Normal equations of the linear fit, in CELL units centred on
                // this cell (so the answer is the constant term).
                let mut a = [[0f64; 4]; 4];
                let mut b = [0f64; 4];
                let mut n = 0usize;
                let mut near: Option<(i64, f32)> = None;
                for dz in -r..=r {
                    for dy in -r..=r {
                        for dx in -r..=r {
                            let (x, y, z) = (cx as i64 + dx, cy as i64 + dy, cz as i64 + dz);
                            if x < 0
                                || y < 0
                                || z < 0
                                || x >= nx as i64
                                || y >= ny as i64
                                || z >= nz as i64
                            {
                                continue;
                            }
                            let cj = (z as usize * ny + y as usize) * nx + x as usize;
                            if !clean[cj] {
                                continue;
                            }
                            let v = cells[cj] as f64;
                            let p = [1.0, dx as f64, dy as f64, dz as f64];
                            for i in 0..4 {
                                for j in 0..4 {
                                    a[i][j] += p[i] * p[j];
                                }
                                b[i] += p[i] * v;
                            }
                            n += 1;
                            let d2 = dx * dx + dy * dy + dz * dz;
                            if near.map_or(true, |(bd, _)| d2 < bd) {
                                near = Some((d2, cells[cj]));
                            }
                        }
                    }
                }
                if n == 0 {
                    continue; // nothing clean in range — leave the raw value
                }
                if n >= 4 {
                    if let Some(x) = solve4(a, b) {
                        out[ci] = x[0] as f32;
                        continue;
                    }
                }
                out[ci] = near.unwrap().1;
            }
        }
    }
    out
}

/// Band width below which the recovered peak is trustworthy, and above which
/// it is not a number at all. Calibrated on the Kirsch plate
/// (`bin/kirschbench`), where the truth is known: a band under 8 % went with a
/// peak error of +2…+4 %, 8–20 % with −10…+5 %, and everything above 20 % with
/// −25…−52 % — i.e. a mesh that does not resolve the feature.
///
/// Deliberately generous at the top: the point of `Unresolved` is to stop the
/// app quoting a peak, not to nag.
pub const BAND_RESOLVED: f64 = 0.08;
pub const BAND_MARGINAL: f64 = 0.20;

/// How much of the reported peak is discretization, in three buckets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshQuality {
    /// Band < [`BAND_RESOLVED`] — the peak is a property of the part.
    Resolved,
    /// Band < [`BAND_MARGINAL`] — usable with the bound in hand.
    Marginal,
    /// The mesh does not resolve the feature; the peak is not a number.
    Unresolved,
}

impl MeshQuality {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Marginal => "marginal",
            Self::Unresolved => "unresolved",
        }
    }
}

/// Discretization uncertainty of a recovered peak — the result that says how
/// much to believe the other result.
#[derive(Clone, Copy, Debug)]
pub struct SurfaceBand {
    /// Peak of the RECOVERED field: the estimate. Mesh-stable, so it is the
    /// one to compare between designs.
    pub peak: f64,
    /// Peak of the UN-recovered field: the conservative bound. Never below
    /// `peak` (clamped), and equal to what the app reported before surface
    /// recovery existed — so a verdict taken from here can never be less
    /// conservative than the old behaviour.
    pub bound: f64,
    /// `(bound − peak) / |peak|`, the relative width of the band.
    pub band: f64,
    pub quality: MeshQuality,
    /// Cell center of the recovered peak.
    pub at: [f64; 3],
}

/// Measure the discretization band of a field, given the same field with and
/// without [`recover_surface`].
///
/// The two differ ONLY on boundary cells, so their gap is a direct per-solve
/// measure of how much staircase is leaking into the peak — free, since both
/// fields already exist. It is taken at the extreme of largest MAGNITUDE, so a
/// compression-driven field is measured at its minimum rather than at a
/// meaningless positive maximum.
///
/// `None` when nothing is solid.
pub fn surface_band(grid: &VoxelGrid, raw: &[f32], recovered: &[f32]) -> Option<SurfaceBand> {
    let (nx, ny) = (grid.nx, grid.ny);
    let mut best: Option<(f64, usize)> = None;
    for (ci, (&v, &s)) in recovered.iter().zip(&grid.scale).enumerate() {
        if s <= 0.0 || !v.is_finite() {
            continue;
        }
        let v = v as f64;
        if best.map_or(true, |(b, _)| v.abs() > b.abs()) {
            best = Some((v, ci));
        }
    }
    let (peak, ci) = best?;
    // Same-signed extreme of the raw field: the bound has to be the same
    // physical quantity, not the opposite end of the range.
    let mut bound = peak;
    for (cj, (&v, &s)) in raw.iter().zip(&grid.scale).enumerate() {
        let _ = cj;
        if s <= 0.0 || !v.is_finite() {
            continue;
        }
        let v = v as f64;
        if v.signum() == peak.signum() && v.abs() > bound.abs() {
            bound = v;
        }
    }
    let band = ((bound.abs() - peak.abs()) / peak.abs().max(1e-12)).max(0.0);
    let quality = if band < BAND_RESOLVED {
        MeshQuality::Resolved
    } else if band < BAND_MARGINAL {
        MeshQuality::Marginal
    } else {
        MeshQuality::Unresolved
    };
    let (cx, cy, cz) = (ci % nx, (ci / nx) % ny, ci / (nx * ny));
    Some(SurfaceBand {
        peak,
        bound,
        band,
        quality,
        at: [
            grid.origin[0] + (cx as f64 + 0.5) * grid.h,
            grid.origin[1] + (cy as f64 + 0.5) * grid.h,
            grid.origin[2] + (cz as f64 + 0.5) * grid.h,
        ],
    })
}

/// Gaussian elimination with partial pivoting on a 4×4 SPD normal-equation
/// system; `None` when it is singular (a degenerate, e.g. coplanar, patch).
fn solve4(mut a: [[f64; 4]; 4], mut b: [f64; 4]) -> Option<[f64; 4]> {
    // Scale-free singularity test: the patch geometry is in cell units, so the
    // pivots are O(patch size) and an absolute floor is meaningful.
    for k in 0..4 {
        let mut p = k;
        for i in k + 1..4 {
            if a[i][k].abs() > a[p][k].abs() {
                p = i;
            }
        }
        if a[p][k].abs() < 1e-9 {
            return None;
        }
        a.swap(k, p);
        b.swap(k, p);
        for i in k + 1..4 {
            let f = a[i][k] / a[k][k];
            for j in k..4 {
                a[i][j] -= f * a[k][j];
            }
            b[i] -= f * b[k];
        }
    }
    let mut x = [0f64; 4];
    for k in (0..4).rev() {
        let mut s = b[k];
        for j in k + 1..4 {
            s -= a[k][j] * x[j];
        }
        x[k] = s / a[k][k];
    }
    Some(x)
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
    cell_field_cut(grid, u, e0, nu, eps, ti, eigen, kind, None)
}

/// [`cell_field_ti`] with the DIRECTIONAL occupancy decoupling applied to cut
/// cells (see [`decouple_traction`]).
///
/// `cut` is [`cut_normals`] of the same grid, and is only meaningful when
/// `eps` is the occupancy-decoupled [`crate::eps::material_factor`] — i.e.
/// when the caller is displaying MATERIAL stress. Pass `None` to read the raw
/// ersatz field, which is what the `material_stress`-off path wants; that road
/// is byte-identical to the pre-existing evaluation.
#[allow(clippy::too_many_arguments)]
pub fn cell_field_cut(
    grid: &VoxelGrid,
    u: &[f32],
    e0: f64,
    nu: f64,
    eps: &[f32],
    ti: Option<(&[f32], &crate::ti::TiRatios)>,
    eigen: [f64; 3],
    kind: FieldKind,
    cut: Option<&[[f32; 3]]>,
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
                        let (mut sxx, mut syy, mut szz, mut sxy, mut syz, mut szx) = match ti {
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
                        // Directional occupancy decoupling on cut cells: the
                        // in-plane block keeps the 1/occ the material factor
                        // applied, the traction on the cut face gives it back.
                        if let Some(cut) = cut {
                            let mut s = [sxx, syy, szz, sxy, syz, szx];
                            decouple_traction(&mut s, cut[ci], grid.scale[ci] as f64);
                            [sxx, syy, szz, sxy, syz, szx] = s;
                        }
                        match kind {
                            FieldKind::Sxx => sxx,
                            FieldKind::Syy => syy,
                            FieldKind::Szz => szz,
                            FieldKind::Sxy => sxy,
                            FieldKind::Syz => syz,
                            FieldKind::Szx => szx,
                            // NOTE: this is downstream of `decouple_traction`, so
                            // the principals are of the MATERIAL tensor on cut
                            // cells. f64 throughout — no f32 round trip.
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

    /// The COMPLEMENTARY cut to `material_factor_removes_occupancy_stripe`, and
    /// the case the scalar occupancy correction gets WRONG.
    ///
    /// There the cell is cut PARALLEL to the stress: material spans the cell's
    /// full length along x and covers only part of its cross-section, so the
    /// strain is the true material strain and dividing the occupancy back out
    /// is exactly right.
    ///
    /// Here the cell is cut PERPENDICULAR to the stress — a soft link in
    /// SERIES. Equilibrium forces the same section force through it, so its
    /// ersatz stress `E0·occ·ε` is ALREADY the true material stress and its
    /// strain is inflated by `1/occ`. `material_factor` then multiplies by
    /// `1/occ` a second time and over-reads by that factor (6.7× at the
    /// `BOUNDARY_FLOOR = 0.15` occupancy limit).
    ///
    /// Measured on the Kirsch plate (`bin/kirschbench`): when the x=0 symmetry
    /// plane lands mid-cell the boundary column comes out at occupancy 2/3 and
    /// reports 1.59 MPa where the true (and raw) value is 1.06 — +50 %, in the
    /// FAR FIELD, purely from this.
    ///
    /// [`decouple_traction`] is the fix, and the second half of this test is
    /// the proof: with the cut normal in hand the same cell reads the true
    /// 10 MPa. The scalar path is still checked above it, because
    /// `material_stress` off must keep behaving exactly as it always did.
    #[test]
    fn material_factor_over_reads_a_transverse_cut() {
        // 2×1×1 grid loaded along x; cell 1 is cut ACROSS the load direction.
        let h = 2.0;
        let mut grid = VoxelGrid::solid_box(2, 1, 1, h);
        let occ = 0.5f32;
        grid.scale[1] = occ;
        let eps = grid.scale.clone();
        let (e0, nu) = (1000.0, 0.0);

        // Series equilibrium: with stiffnesses E0 and E0·occ in series under the
        // same section force, cell 0 strains a and cell 1 strains a/occ. Build
        // exactly that displacement field.
        let a = 0.01f32;
        let (mx, my, mz) = (3usize, 2, 2);
        let mut u = vec![0f32; 3 * mx * my * mz];
        let ux = [0.0, a * h as f32, a * h as f32 * (1.0 + 1.0 / occ)];
        for nz in 0..mz {
            for ny in 0..my {
                for nx in 0..mx {
                    let n = (nz * my + ny) * mx + nx;
                    u[3 * n] = ux[nx];
                }
            }
        }

        let raw = cell_field(&grid, &u, e0, nu, &eps, FieldKind::Sxx);
        let mat = cell_field(&grid, &u, e0, nu, &material_factor(&grid, &eps), FieldKind::Sxx);

        // Both cells carry the same true stress, 10 MPa.
        assert!((raw[0] - 10.0).abs() < 1e-3, "raw full cell {}", raw[0]);
        // RAW is right on the cut cell here: E0·occ·(a/occ) = E0·a.
        assert!((raw[1] - 10.0).abs() < 1e-3, "raw transverse cut cell {}", raw[1]);
        // …and the SCALAR `material_factor` inflates it by 1/occ — the defect.
        assert!(
            (mat[1] - 10.0 / occ as f64 as f32).abs() < 1e-2,
            "scalar material factor should (wrongly) read {} on a transverse cut, got {}",
            10.0 / occ,
            mat[1]
        );

        // The DIRECTIONAL correction fixes it. The cut normal comes straight
        // out of the occupancy gradient: cell 1 has solid at −x and nothing at
        // +x, so n̂ = +x — the load direction, which is what makes this the
        // transverse case.
        let n = cut_normals(&grid);
        assert!((n[1][0] - 1.0).abs() < 1e-6 && n[1][1] == 0.0 && n[1][2] == 0.0, "n = {:?}", n[1]);
        let fixed = cell_field_cut(
            &grid,
            &u,
            e0,
            nu,
            &material_factor(&grid, &eps),
            None,
            [0.0; 3],
            FieldKind::Sxx,
            Some(&n),
        );
        assert!((fixed[1] - 10.0).abs() < 1e-2, "corrected transverse cut {}", fixed[1]);
        // …and it leaves the fully-solid cell exactly alone (occ = 1 ⇒ no-op).
        assert!((fixed[0] - mat[0]).abs() < 1e-6, "interior perturbed: {} vs {}", fixed[0], mat[0]);
    }

    /// The PARALLEL cut — the case the scalar correction already got right —
    /// must survive the directional one untouched. Same 2-cell grid, but now
    /// the cut is across y: material spans the cell's full length along the
    /// load, so the cell's strain IS the material strain and the full 1/occ
    /// belongs there.
    #[test]
    fn directional_correction_preserves_a_parallel_cut() {
        let h = 2.0;
        // 3×3×1, two solid rows in y with the third void: the top solid row's
        // outward normal is +y, unambiguously (a ONE-row slab would have void
        // on both sides and no defined normal — the central difference then
        // cancels and the correction correctly declines to fire).
        let mut grid = VoxelGrid::solid_box(3, 3, 1, h);
        for cx in 0..3 {
            grid.scale[2 * 3 + cx] = 0.0; // cy = 2 void
        }
        let occ = 0.5f32;
        let ci = 1 * 3 + 1; // (cx=1, cy=1, cz=0) — the cut cell
        grid.scale[ci] = occ;
        let eps = grid.scale.clone();
        let (e0, nu) = (1000.0, 0.0);

        // Uniform ε_xx = a everywhere (the strain a parallel cut really sees).
        let a = 0.01f32;
        let (mx, my, mz) = (4usize, 4, 2);
        let mut u = vec![0f32; 3 * mx * my * mz];
        for nz in 0..mz {
            for ny in 0..my {
                for nx in 0..mx {
                    let n = (nz * my + ny) * mx + nx;
                    u[3 * n] = a * (nx as f32 * h as f32);
                }
            }
        }
        let n = cut_normals(&grid);
        // Void at +y dominates the gradient, so the normal is +y — transverse
        // to the surface, PARALLEL to the σxx that is being read.
        assert!(n[ci][1] > 0.9, "expected a +y cut normal, got {:?}", n[ci]);
        let mat = material_factor(&grid, &eps);
        let scalar = cell_field(&grid, &u, e0, nu, &mat, FieldKind::Sxx);
        let directional =
            cell_field_cut(&grid, &u, e0, nu, &mat, None, [0.0; 3], FieldKind::Sxx, Some(&n));
        assert!((scalar[ci] - 10.0).abs() < 1e-2, "scalar parallel cut {}", scalar[ci]);
        assert!(
            (directional[ci] - 10.0).abs() < 1e-2,
            "the directional correction must not touch a parallel cut: {}",
            directional[ci]
        );
    }

    /// [`recover_surface`] must reproduce an exactly LINEAR field on the
    /// boundary cells — the degree-1 patch fit is exact for its own basis, so
    /// any deviation is a bug in the fit, the patch gathering, or the
    /// clean-cell predicate rather than an approximation.
    #[test]
    fn surface_recovery_is_exact_on_a_linear_field() {
        let grid = VoxelGrid::solid_box(5, 5, 5, 1.0);
        let f = |cx: usize, cy: usize, cz: usize| {
            2.0 + 3.0 * cx as f32 - 5.0 * cy as f32 + 7.0 * cz as f32
        };
        let mut cells = vec![0f32; 125];
        for cz in 0..5 {
            for cy in 0..5 {
                for cx in 0..5 {
                    cells[(cz * 5 + cy) * 5 + cx] = f(cx, cy, cz);
                }
            }
        }
        // Perturb the boundary shell so the recovery has something to fix; the
        // interior 3×3×3 stays exact and is what the fits see.
        let mut noisy = cells.clone();
        for cz in 0..5 {
            for cy in 0..5 {
                for cx in 0..5 {
                    if !is_clean(&grid, cx, cy, cz) {
                        noisy[(cz * 5 + cy) * 5 + cx] += 99.0;
                    }
                }
            }
        }
        let out = recover_surface(&grid, &noisy);
        for cz in 0..5 {
            for cy in 0..5 {
                for cx in 0..5 {
                    let i = (cz * 5 + cy) * 5 + cx;
                    assert!(
                        (out[i] - cells[i]).abs() < 1e-3,
                        "cell ({cx},{cy},{cz}): recovered {} want {}",
                        out[i],
                        cells[i]
                    );
                }
            }
        }
    }

    /// [`surface_band`] must price the gap between the two read-backs, pick the
    /// same-signed extreme, and bucket it. A clean field (the two identical)
    /// is a zero band; a boundary spike is a wide one.
    #[test]
    fn surface_band_prices_the_staircase_gap() {
        let grid = VoxelGrid::solid_box(5, 5, 5, 1.0);
        let mut recovered = vec![2.0f32; 125];
        recovered[(2 * 5 + 2) * 5 + 2] = 3.0; // the interior peak
        // Identical fields ⇒ nothing to be uncertain about.
        let b = surface_band(&grid, &recovered, &recovered).unwrap();
        assert!(b.band == 0.0 && b.quality == MeshQuality::Resolved, "{b:?}");
        assert!((b.peak - 3.0).abs() < 1e-6 && (b.bound - 3.0).abs() < 1e-6);

        // A 5 % boundary spike in the raw field is a 5 % band: still resolved,
        // and the BOUND rises to the spike while the peak stays the estimate.
        let mut raw = recovered.clone();
        raw[0] = 3.15;
        let b = surface_band(&grid, &raw, &recovered).unwrap();
        assert!((b.band - 0.05).abs() < 1e-3, "band {}", b.band);
        assert!((b.bound - 3.15).abs() < 1e-6 && (b.peak - 3.0).abs() < 1e-6);
        assert!(b.quality == MeshQuality::Resolved);

        // A 50 % spike is unresolved — the mesh is not seeing the feature.
        raw[0] = 4.5;
        assert!(surface_band(&grid, &raw, &recovered).unwrap().quality == MeshQuality::Unresolved);

        // Compression: the extreme of largest MAGNITUDE is the minimum, and
        // the bound must be the same-signed extreme, not the positive one.
        let mut rec_c = vec![1.0f32; 125];
        rec_c[(2 * 5 + 2) * 5 + 2] = -4.0;
        let mut raw_c = rec_c.clone();
        raw_c[0] = -5.0;
        let b = surface_band(&grid, &raw_c, &rec_c).unwrap();
        assert!((b.peak + 4.0).abs() < 1e-6, "peak {}", b.peak);
        assert!((b.bound + 5.0).abs() < 1e-6, "bound {}", b.bound);
        assert!((b.band - 0.25).abs() < 1e-3, "band {}", b.band);
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
