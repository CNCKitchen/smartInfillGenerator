// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Transverse-isotropic infill tensors from measured calibration (DESIGN §22).
//!
//! FDM infill is isotropic in the layer plane and different along the build
//! axis. This module owns the frozen anisotropy RATIOS — the shape of the
//! tensor — while the magnitude stays with the existing scalar law
//! `rel(ρ) = coeff·ρ^exponent` in [`crate::eps`]. Every tensor here is
//! normalized to `Ep = 1`, so `rel(ρ) · Ĉ` is the cell's infill stiffness.
//!
//! Constants are measured by periodic homogenization of the real sliced
//! toolpath and are 3-rung Richardson extrapolations. A single-grid voxel-FE
//! value is 30–60 % too stiff and biased NON-UNIFORMLY per constant, so a
//! single-rung ratio is wrong by 10–40 % — more than the whole ±15 % gate that
//! qualified this model. Never enter a single-rung number here.

/// The five independent constants of a transverse-isotropic solid, normalized
/// to `Ep = 1`. "p" is the layer plane (x,y), "z" the build axis.
///
/// `Gxy` is deliberately ABSENT: transverse isotropy has five independent
/// constants and fixes `Gxy = Ep/(2(1+ν_p))`. Storing it would let a data-entry
/// slip produce a tensor that is not actually TI while the kernel accepted it
/// anyway. The separately measured value is kept as a test fixture instead —
/// see `frozen_cubic_is_self_consistent`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TiRatios {
    /// Build-axis modulus over in-plane modulus.
    pub ez_ep: f64,
    /// Transverse (z-plane) shear modulus over in-plane modulus.
    pub gz_ep: f64,
    /// In-plane Poisson ratio (in-plane stress → in-plane contraction).
    pub nu_p: f64,
    /// In-plane stress → build-axis contraction. The MAJOR ratio (`ν_pz > ν_zp`
    /// because `Ep > Ez`); its partner follows from Maxwell reciprocity
    /// `ν_zp = ν_pz·Ez/Ep`.
    pub nu_pz: f64,
}

/// Cubic infill, band mean over 20–70 % (DESIGN §22.3).
///
/// The band mean rather than any single density, because the ratios drift
/// across the band (`Ez/Ep` climbs 0.762 → 0.871 from 30 % to 70 %) and no
/// single density represents it.
///
/// Measured under FLOW CALIBRATION (`exact-period-v4-flowcal`), which is not
/// a detail: the earlier uncalibrated raster loses bead-overlap volume at line
/// crossings, under-reading the material fraction by 10–22 % (worse at high
/// density) and the build-axis ratios by ~5 %. Under flow calibration the
/// measured `rho_rel` matches the nominal infill the slicer was asked for to
/// better than 0.3 % at every density — that agreement is the check that this
/// convention, not the older one, is the correct one. Do not mix conventions
/// when adding a pattern.
///
/// Below 20 % these are held flat — extrapolated, not measured, because
/// cubic's 10 % unit cell is ~54× the volume of its 30 % cell and needs
/// ~440 M elements per rung to converge.
pub const CUBIC: TiRatios =
    TiRatios { ez_ep: 0.8029, gz_ep: 0.4247, nu_p: 0.2713, nu_pz: 0.3651 };

/// The separately MEASURED in-plane shear ratio for [`CUBIC`], kept only to
/// test that the frozen set is self-consistent (it agrees with the derived
/// `1/(2(1+ν_p))` to 0.05 %). Not used to build the tensor — see [`TiRatios`].
pub const CUBIC_GXY_EP_MEASURED: f64 = 0.3935;

impl TiRatios {
    /// Derived in-plane shear ratio. TI fixes this; it is not free.
    pub fn gxy_ep(&self) -> f64 {
        1.0 / (2.0 * (1.0 + self.nu_p))
    }

    /// Build-axis stress → in-plane contraction, by Maxwell reciprocity.
    pub fn nu_zp(&self) -> f64 {
        self.nu_pz * self.ez_ep
    }

    /// Whether these constants describe a physically admissible (SPD)
    /// material. User-supplied property sets (DESIGN §24) reach the solver as
    /// four free numbers; a non-SPD tensor makes K indefinite and CG diverges
    /// on the first real part, so the wasm layer rejects them here instead.
    ///
    /// Closed form: the shear diagonal must be positive and the 3×3 normal
    /// compliance block SPD, i.e. its leading principal minors positive
    /// (Sylvester). `S` is built with `Ep = 1` exactly as [`Self::stiffness`].
    pub fn is_physical(&self) -> bool {
        if !(self.ez_ep > 0.0 && self.gz_ep > 0.0 && self.nu_p > -1.0) {
            return false;
        }
        let (nu_p, nu_pz, ez) = (self.nu_p, self.nu_pz, self.ez_ep);
        let minor2 = 1.0 - nu_p * nu_p;
        // det of [[1,-νp,-νpz],[-νp,1,-νpz],[-νpz,-νpz,1/Ez]] expanded.
        let det = (1.0 - nu_p * nu_p) / ez - 2.0 * nu_pz * nu_pz * (1.0 + nu_p);
        minor2 > 0.0 && det > 0.0 && self.ez_ep.is_finite() && self.gz_ep.is_finite()
    }

    /// The 6×6 stiffness tensor, `Ep = 1`, engineering strain order
    /// xx yy zz xy yz zx (matching [`crate::fem::ke_hex_c`]).
    ///
    /// Built by inverting the compliance matrix, which is the form the
    /// constants are naturally expressed in and the form whose symmetry is
    /// obvious. The 3×3 normal block is inverted in closed form; the three
    /// shear terms are diagonal and invert individually.
    pub fn stiffness(&self) -> [[f64; 6]; 6] {
        let (ep, ez) = (1.0, self.ez_ep);
        let (nu_p, nu_pz) = (self.nu_p, self.nu_pz);
        // Compliance, normal block: [1/Ep, -nu_p/Ep, -nu_zp/Ez; ... ]
        // Row 2 uses nu_zp/Ez == nu_pz/Ep (reciprocity), so S is symmetric by
        // construction rather than by luck.
        let s = [
            [1.0 / ep, -nu_p / ep, -nu_pz / ep],
            [-nu_p / ep, 1.0 / ep, -nu_pz / ep],
            [-nu_pz / ep, -nu_pz / ep, 1.0 / ez],
        ];
        let inv = invert3_sym(&s).expect("TI compliance must be invertible");

        let mut c = [[0.0f64; 6]; 6];
        for i in 0..3 {
            for j in 0..3 {
                c[i][j] = inv[i][j];
            }
        }
        // Engineering shear: C44..C66 are the moduli directly.
        c[3][3] = self.gxy_ep(); // xy — in the layer plane
        c[4][4] = self.gz_ep; // yz — through layers
        c[5][5] = self.gz_ep; // zx — through layers
        c
    }
}

/// The six stress components of a cell whose material is the Voigt blend of a
/// solid share and a TI infill share — the constitutive twin of the two-tensor
/// element kernel (DESIGN §22.4), for evaluating stress from a solved strain.
///
/// `fs`/`fi` are the SOLID and INFILL material factors (occupancy already
/// divided out — see [`crate::eps::material_factor`]). `strain` is engineering
/// order `[εxx, εyy, εzz, γxy, γyz, γzx]`; the return is the matching stress.
///
/// This must stay consistent with the stiffness blend or the reported stress
/// stops being the stress the solve actually produced — a displacement field
/// solved with one material and read out with another is wrong in a way no
/// convergence check can catch.
pub fn blended_stress(
    e0: f64,
    nu: f64,
    fs: f64,
    fi: f64,
    ratios: &TiRatios,
    strain: [f64; 6],
) -> [f64; 6] {
    let iso = crate::fem::iso_stiffness(e0 * fs, nu);
    let ti = ratios.stiffness();
    let mut out = [0.0f64; 6];
    for i in 0..6 {
        let mut s = 0.0;
        for j in 0..6 {
            s += (iso[i][j] + e0 * fi * ti[i][j]) * strain[j];
        }
        out[i] = s;
    }
    out
}

/// Closed-form inverse of a symmetric 3×3, `None` when singular.
fn invert3_sym(m: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-300 {
        return None;
    }
    let d = 1.0 / det;
    let mut o = [[0.0f64; 3]; 3];
    o[0][0] = (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * d;
    o[0][1] = (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * d;
    o[0][2] = (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * d;
    o[1][0] = (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * d;
    o[1][1] = (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * d;
    o[1][2] = (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * d;
    o[2][0] = (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * d;
    o[2][1] = (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * d;
    o[2][2] = (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * d;
    Some(o)
}

/// The isotropic tensor written in [`TiRatios`] form — the degenerate case.
/// `stiffness()` on this must reproduce [`iso_stiffness`] exactly, which is
/// what makes "TI off" and "TI with isotropic data" the same code path.
pub fn isotropic_ratios(nu: f64) -> TiRatios {
    TiRatios { ez_ep: 1.0, gz_ep: 1.0 / (2.0 * (1.0 + nu)), nu_p: nu, nu_pz: nu }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fem::iso_stiffness;

    fn eigen_min_max(a: &[[f64; 6]; 6]) -> (f64, f64) {
        // Cyclic Jacobi — the tensor is tiny and symmetric.
        let mut m = *a;
        for _ in 0..64 {
            let mut off = 0.0;
            for i in 0..6 {
                for j in (i + 1)..6 {
                    off += m[i][j] * m[i][j];
                }
            }
            if off < 1e-24 {
                break;
            }
            for p in 0..6 {
                for q in (p + 1)..6 {
                    if m[p][q].abs() < 1e-18 {
                        continue;
                    }
                    let theta = 0.5 * (m[q][q] - m[p][p]) / m[p][q];
                    let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                    let c = 1.0 / (t * t + 1.0).sqrt();
                    let s = t * c;
                    for k in 0..6 {
                        let (mkp, mkq) = (m[k][p], m[k][q]);
                        m[k][p] = c * mkp - s * mkq;
                        m[k][q] = s * mkp + c * mkq;
                    }
                    for k in 0..6 {
                        let (mpk, mqk) = (m[p][k], m[q][k]);
                        m[p][k] = c * mpk - s * mqk;
                        m[q][k] = s * mpk + c * mqk;
                    }
                }
            }
        }
        let d: Vec<f64> = (0..6).map(|i| m[i][i]).collect();
        (d.iter().cloned().fold(f64::INFINITY, f64::min), d.iter().cloned().fold(f64::NEG_INFINITY, f64::max))
    }

    /// The shipped tensor must be a physically admissible material. A
    /// non-SPD tensor makes K indefinite and CG diverges rather than
    /// converging to something wrong — but it would do so only on the first
    /// real part, not in any unit test that never assembles K.
    #[test]
    fn frozen_cubic_tensor_is_positive_definite() {
        let c = CUBIC.stiffness();
        let (lo, hi) = eigen_min_max(&c);
        assert!(lo > 1e-6 * hi, "C not SPD: eigenvalues span {lo:e}..{hi:e}");
    }

    /// The 0.06 % agreement of DESIGN §22.3. Two INDEPENDENT checks that the
    /// four frozen numbers describe one consistent material: the derived
    /// `Gxy` against the separately measured one, and Maxwell reciprocity.
    /// If someone edits a constant, this is what catches an inconsistent set.
    #[test]
    fn frozen_cubic_is_self_consistent() {
        let derived = CUBIC.gxy_ep();
        let rel = (derived - CUBIC_GXY_EP_MEASURED).abs() / CUBIC_GXY_EP_MEASURED;
        assert!(
            rel < 0.005,
            "derived Gxy/Ep {derived:.5} vs measured {CUBIC_GXY_EP_MEASURED:.5} (rel {rel:.5})"
        );

        // nu_zp/Ez must equal nu_pz/Ep (= nu_pz, since Ep = 1). This holds by
        // construction of `nu_zp()`, so the real check is that the frozen
        // `nu_pz` agrees with the INDEPENDENTLY measured band-mean `nu_zp`.
        let lhs = CUBIC.nu_zp() / CUBIC.ez_ep;
        assert!((lhs - CUBIC.nu_pz).abs() < 1e-12);
        const CUBIC_NU_ZP_MEASURED: f64 = 0.2927;
        let rel = (CUBIC.nu_zp() - CUBIC_NU_ZP_MEASURED).abs() / CUBIC_NU_ZP_MEASURED;
        assert!(rel < 0.005, "nu_zp derived {} vs measured {CUBIC_NU_ZP_MEASURED} (rel {rel:.5})", CUBIC.nu_zp());
    }

    /// Cubic must actually BE anisotropic — a guard against someone
    /// "simplifying" the constants back to isotropy and silently reverting
    /// the whole feature to the old kernel while all other tests still pass.
    #[test]
    fn frozen_cubic_is_meaningfully_anisotropic() {
        assert!(CUBIC.ez_ep < 0.85, "Ez/Ep {} is not the measured anisotropy", CUBIC.ez_ep);
        let iso_gz = 1.0 / (2.0 * (1.0 + CUBIC.nu_p));
        assert!(
            (CUBIC.gz_ep - iso_gz).abs() / iso_gz > 0.02,
            "Gz/Ep {} collapsed onto the isotropic value {iso_gz}",
            CUBIC.gz_ep
        );
    }

    /// `is_physical` must agree with the eigenvalue truth on both sides of
    /// the line: the shipped set and isotropic sets pass; sets that violate
    /// the Poisson determinant bound (the failure a plausible-looking edit
    /// actually produces) fail BOTH the closed form and the SPD check.
    #[test]
    fn is_physical_matches_spd() {
        assert!(CUBIC.is_physical());
        for &nu in &[0.0, 0.2, 0.45] {
            assert!(isotropic_ratios(nu).is_physical());
        }
        let bad = TiRatios { nu_pz: 0.9, ..CUBIC }; // det goes negative
        assert!(!bad.is_physical());
        let (lo, hi) = eigen_min_max(&bad.stiffness());
        assert!(lo < 1e-6 * hi.abs().max(1.0), "closed form says non-SPD but eigenvalues disagree: {lo:e}..{hi:e}");
        assert!(!TiRatios { ez_ep: -0.5, ..CUBIC }.is_physical());
        assert!(!TiRatios { gz_ep: 0.0, ..CUBIC }.is_physical());
        assert!(!TiRatios { nu_p: 1.1, ..CUBIC }.is_physical());
    }

    /// The identity that makes the two-tensor kernel a strict generalization:
    /// TI built from isotropic constants IS the isotropic tensor. If this
    /// fails, the single-material path is no longer bit-identical and every
    /// regbench baseline shifts.
    #[test]
    fn isotropic_ratios_reproduce_the_isotropic_tensor() {
        for &nu in &[0.0, 0.2, 0.35, 0.45] {
            let a = isotropic_ratios(nu).stiffness();
            let b = iso_stiffness(1.0, nu);
            for i in 0..6 {
                for j in 0..6 {
                    assert!(
                        (a[i][j] - b[i][j]).abs() < 1e-10,
                        "nu={nu} C[{i}][{j}]: TI {} vs iso {}",
                        a[i][j],
                        b[i][j]
                    );
                }
            }
        }
    }

    /// The element matrix built from the TI path must equal the one built
    /// from the isotropic path — this is the level the solver actually
    /// consumes, and the place a strain-order mismatch would show up.
    #[test]
    fn isotropic_ti_element_equals_ke_hex() {
        use crate::fem::{ke_hex, ke_hex_c};
        let (e, nu, h) = (2100.0, 0.35, 0.6);
        let mut c = isotropic_ratios(nu).stiffness();
        for row in c.iter_mut() {
            for v in row.iter_mut() {
                *v *= e;
            }
        }
        let a = ke_hex_c(&c, [h; 3]);
        let b = ke_hex(e, nu, h);
        for i in 0..24 {
            for j in 0..24 {
                assert!((a[i][j] - b[i][j]).abs() < 1e-6 * e, "KE[{i}][{j}]: {} vs {}", a[i][j], b[i][j]);
            }
        }
    }

    /// A TI element must be SOFTER along z than in-plane. Compresses a single
    /// element along each axis and compares the reaction — catches a tensor
    /// transposed into the wrong axis convention, which every symmetry and
    /// SPD test above would happily pass.
    #[test]
    fn ti_element_is_softer_along_the_build_axis() {
        use crate::fem::{ke_hex_c, NODE_SIGNS};
        let ke = ke_hex_c(&CUBIC.stiffness(), [1.0; 3]);
        // Unit uniaxial strain along `axis`: u = x_axis * e_axis on each node.
        let energy = |axis: usize| {
            let mut u = [0f64; 24];
            for l in 0..8 {
                u[3 * l + axis] = 0.5 * NODE_SIGNS[l][axis];
            }
            let mut e = 0.0;
            for i in 0..24 {
                for j in 0..24 {
                    e += u[i] * ke[i][j] * u[j];
                }
            }
            e
        };
        let (ex, ey, ez) = (energy(0), energy(1), energy(2));
        assert!((ex - ey).abs() < 1e-9 * ex, "in-plane must be isotropic: {ex} vs {ey}");
        assert!(ez < ex, "z ({ez}) must be softer than in-plane ({ex})");
        // Confined (unit-strain) stiffness is C33/C11, not Ez/Ep — assert the
        // ordering and a sane magnitude rather than a ratio this test cannot
        // legitimately predict.
        assert!(ez / ex > 0.5, "z/in-plane confined ratio {} implausibly low", ez / ex);
    }
}
