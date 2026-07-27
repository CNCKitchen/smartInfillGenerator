// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Orientation sweep (DESIGN §15): score candidate build directions by the
//! worst layer-adhesion safety factor, WITHOUT re-solving.
//!
//! The structural solve is isotropic and loads/constraints rotate with the
//! part, so the stress field in the part's frame is orientation-independent —
//! only the failure criterion depends on the build direction `n`. We therefore
//! extract the full per-cell stress tensor once per result and, per candidate
//! `n`, evaluate the traction on the layer plane:
//!
//! # THIS ASSUMPTION IS FALSE UNDER TRANSVERSE ISOTROPY (DESIGN §22)
//!
//! A TI infill tensor is defined ABOUT THE BUILD AXIS, so rotating the part
//! rotates the material with it and the stress field in the part's frame is no
//! longer orientation-independent. A correct TI sweep must RE-SOLVE per
//! candidate — the "solve once, re-score" shortcut this whole module is built
//! on stops being valid.
//!
//! The sweep therefore runs on the ISOTROPIC operator and says so, rather than
//! silently rescoring a TI stress field it cannot legitimately rotate. Callers
//! must pass an isotropic result here; `sweep` asserts it via
//! [`assert_isotropic_result`]. Making the sweep TI-aware is a scheduled
//! follow-up, not a bug — see §22.4.
//!
//!   σₙₙ = n·σ·n          (normal, tension-only: compression can't delaminate)
//!   τ²  = |σ·n|² − σₙₙ²  (sliding along the layer plane)
//!   (⟨σₙₙ⟩₊/Sᵗᶻ)² + (τ/Sˢᶻ)² = 1/SF²   (quadratic interaction, §15 dec. 1)
//!
//! The criterion is quadratic in `n`, so SF(n) = SF(−n) and the hemisphere
//! pitch×roll ∈ [−90°, +90°]² covers every layer-plane choice exactly once
//! (§15 dec. 4): n = Rx(pitch)·Ry(roll)·ẑ = (sin r, −sin p·cos r, cos p·cos r).
//!
//! Two scores per pixel (§15 dec. 3): the SCORED min-SF excludes cells inside
//! a fixed ring around rigid constraints (mesh-divergent singularities), the
//! ALL min-SF hides nothing. Cells whose best-possible SF can't drop below the
//! cap at ANY orientation are pruned up front via principal-stress bounds
//! (σₙₙ ≤ σ₁, τ ≤ (σ₁−σ₃)/2).

use crate::fem::{NODE_OFFSETS, NODE_SIGNS};
use crate::par;
use crate::voxel::VoxelGrid;

/// Ring width in DILATION PASSES around constraint-adjacent cells (26-neighbor
/// dilation; the node→cell seeding already covers the first layer, so the
/// masked band is RING_DILATIONS+1 ≈ 3 cells — §15 dec. 3 "fixed 2–3 cells").
pub const RING_DILATIONS: usize = 2;

/// Layer normal for a heatmap pixel: n = Rx(pitch)·Ry(roll)·ẑ, degrees.
pub fn layer_normal(pitch_deg: f64, roll_deg: f64) -> [f64; 3] {
    let (p, r) = (pitch_deg.to_radians(), roll_deg.to_radians());
    [r.sin(), -p.sin() * r.cos(), p.cos() * r.cos()]
}

/// The full pitch×roll hemisphere grid at `step_deg` (both axes −90..=+90,
/// endpoints included). Returns (per-axis sample count, normals) with the
/// flattened index `i = i_pitch * n + i_roll` (roll fastest).
pub fn hemisphere_grid(step_deg: f64) -> (usize, Vec<[f32; 3]>) {
    let n = (180.0 / step_deg).round() as usize + 1;
    let mut dirs = Vec::with_capacity(n * n);
    for ip in 0..n {
        let pitch = -90.0 + ip as f64 * step_deg;
        for ir in 0..n {
            let d = layer_normal(pitch, -90.0 + ir as f64 * step_deg);
            dirs.push([d[0] as f32, d[1] as f32, d[2] as f32]);
        }
    }
    (n, dirs)
}

/// (σ₁, σ₃): largest / smallest principal stress of a symmetric 3×3 given as
/// [sxx, syy, szz, sxy, syz, szx]. Closed form (trigonometric), f64 internally.
pub fn principal_range(s: [f32; 6]) -> (f64, f64) {
    let [sxx, syy, szz, sxy, syz, szx] =
        [s[0] as f64, s[1] as f64, s[2] as f64, s[3] as f64, s[4] as f64, s[5] as f64];
    let p1 = sxy * sxy + syz * syz + szx * szx;
    if p1 == 0.0 {
        return (sxx.max(syy).max(szz), sxx.min(syy).min(szz));
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
    let s1 = q + 2.0 * p * phi.cos();
    // Eigenvalues are q + 2p·cos(φ + 2πk/3), φ ∈ [0, π/3]; k = 1 lands in
    // [2π/3, π] where cos is most negative — the smallest.
    let s3 = q + 2.0 * p * (phi + 2.0 * std::f64::consts::FRAC_PI_3).cos();
    (s1, s3)
}

/// Cells inside the constraint ring: seeded from the cells adjacent to each
/// constraint node (padded-grid node ids, `n = (z·my + y)·mx + x`), then
/// dilated `RING_DILATIONS` passes over the 26-neighborhood. Void cells are
/// marked too (harmless — they carry no stress).
pub fn constraint_ring_mask(grid: &VoxelGrid, constraint_nodes: &[&[u32]]) -> Vec<bool> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let mut mask = vec![false; nx * ny * nz];
    for nodes in constraint_nodes {
        for &node in *nodes {
            let n = node as usize;
            let (x, y, z) = (n % mx, (n / mx) % my, n / (mx * my));
            // The (up to) 8 cells sharing this node.
            for cz in z.saturating_sub(1)..=z.min(nz - 1) {
                for cy in y.saturating_sub(1)..=y.min(ny - 1) {
                    for cx in x.saturating_sub(1)..=x.min(nx - 1) {
                        mask[(cz * ny + cy) * nx + cx] = true;
                    }
                }
            }
        }
    }
    for _ in 0..RING_DILATIONS {
        let prev = mask.clone();
        for cz in 0..nz {
            for cy in 0..ny {
                for cx in 0..nx {
                    let ci = (cz * ny + cy) * nx + cx;
                    if mask[ci] {
                        continue;
                    }
                    'scan: for dz in cz.saturating_sub(1)..=(cz + 1).min(nz - 1) {
                        for dy in cy.saturating_sub(1)..=(cy + 1).min(ny - 1) {
                            for dx in cx.saturating_sub(1)..=(cx + 1).min(nx - 1) {
                                if prev[(dz * ny + dy) * nx + dx] {
                                    mask[ci] = true;
                                    break 'scan;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    mask
}

/// Compact, prune-filtered sweep input: one entry per surviving cell (across
/// ALL folded results/load steps — the per-pixel score is a min over
/// steps × cells, so steps simply concatenate). SoA layout; the first
/// `n_scored` entries are outside the constraint ring, ring cells follow.
pub struct SweepField {
    s: [Vec<f32>; 6],
    /// 1 / (Sᵗᶻ · factor) per cell — tension allowable, inverted.
    inv_t: Vec<f32>,
    /// 1 / (Sˢᶻ · factor) per cell — shear allowable, inverted.
    inv_s: Vec<f32>,
    n_scored: usize,
    sf_cap: f32,
    /// Solid cells seen before pruning (summed over folded results).
    pub cells_seen: usize,
}

impl SweepField {
    /// Cells that survived pruning (scored + ring).
    pub fn cells_kept(&self) -> usize {
        self.inv_t.len()
    }

    /// Kept cells OUTSIDE the constraint ring (the scored subset).
    pub fn scored_cells(&self) -> usize {
        self.n_scored
    }

    /// Sweep a slice of layer normals: for each, the min layer-adhesion SF
    /// over the scored cells (`.0`) and over ALL kept cells (`.1`), both
    /// capped at `sf_cap`. Parallel over pixels; each pixel streams the SoA
    /// arrays once. `q` = 1/SF² folds via max.
    pub fn sweep(&self, dirs: &[[f32; 3]]) -> Vec<(f32, f32)> {
        let q_cap = 1.0 / (self.sf_cap * self.sf_cap);
        let [sxx, syy, szz, sxy, syz, szx] =
            [&self.s[0], &self.s[1], &self.s[2], &self.s[3], &self.s[4], &self.s[5]];
        let mut out = vec![(0f32, 0f32); dirs.len()];
        par::map_indexed(&mut out, |pi| {
            let [a, b, c] = dirs[pi];
            let mut q_scored = q_cap;
            let mut q_all = q_cap;
            for i in 0..self.inv_t.len() {
                let tx = sxx[i] * a + sxy[i] * b + szx[i] * c;
                let ty = sxy[i] * a + syy[i] * b + syz[i] * c;
                let tz = szx[i] * a + syz[i] * b + szz[i] * c;
                let snn = tx * a + ty * b + tz * c;
                let tau2 = (tx * tx + ty * ty + tz * tz - snn * snn).max(0.0);
                let qt = snn.max(0.0) * self.inv_t[i];
                let qs2 = tau2 * self.inv_s[i] * self.inv_s[i];
                let q = qt * qt + qs2;
                q_all = q_all.max(q);
                if i < self.n_scored {
                    q_scored = q_scored.max(q);
                }
            }
            (1.0 / q_scored.sqrt(), 1.0 / q_all.sqrt())
        });
        out
    }
}

/// Per-cell layer-adhesion SF for ONE build direction `n` (the preview
/// recolor when a heatmap pixel is clicked): the §15 interaction criterion,
/// full grid layout (0.0 for void cells), capped at `sf_cap`. Fold several
/// results (load steps) by taking the elementwise min of their fields.
#[allow(clippy::too_many_arguments)]
pub fn layer_sf_cells(
    grid: &VoxelGrid,
    u: &[f32],
    e0: f64,
    nu: f64,
    eps: &[f32],
    eigen: [f64; 3],
    strength_t: f64,
    strength_s: f64,
    n_dir: [f64; 3],
    sf_cap: f32,
) -> Vec<f32> {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let (mx, my) = (nx + 1, ny + 1);
    let inv4h = 1.0 / (4.0 * grid.h);
    let (a, b, c) = (n_dir[0], n_dir[1], n_dir[2]);
    let mut out = vec![0f32; nx * ny * nz];
    for cz in 0..nz {
        for cy in 0..ny {
            for cx in 0..nx {
                let ci = (cz * ny + cy) * nx + cx;
                if eps[ci] <= 0.0 {
                    continue;
                }
                let (mut exx, mut eyy, mut ezz) = (0f64, 0f64, 0f64);
                let (mut gxy, mut gyz, mut gzx) = (0f64, 0f64, 0f64);
                for l in 0..8 {
                    let [ox, oy, oz] = NODE_OFFSETS[l];
                    let [sx, sy, sz] = NODE_SIGNS[l];
                    let n = ((cz + oz) * my + (cy + oy)) * mx + (cx + ox);
                    let (ux, uy, uz) = (u[3 * n] as f64, u[3 * n + 1] as f64, u[3 * n + 2] as f64);
                    exx += sx * ux;
                    eyy += sy * uy;
                    ezz += sz * uz;
                    gxy += sy * ux + sx * uy;
                    gyz += sz * uy + sy * uz;
                    gzx += sx * uz + sz * ux;
                }
                let (exx, eyy, ezz) =
                    (exx * inv4h - eigen[0], eyy * inv4h - eigen[1], ezz * inv4h - eigen[2]);
                let (gxy, gyz, gzx) = (gxy * inv4h, gyz * inv4h, gzx * inv4h);
                let e = e0 * eps[ci] as f64;
                let lam = e * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
                let mu = e / (2.0 * (1.0 + nu));
                let tr = exx + eyy + ezz;
                let (sxx, syy, szz) =
                    (lam * tr + 2.0 * mu * exx, lam * tr + 2.0 * mu * eyy, lam * tr + 2.0 * mu * ezz);
                let (sxy, syz, szx) = (mu * gxy, mu * gyz, mu * gzx);
                let tx = sxx * a + sxy * b + szx * c;
                let ty = sxy * a + syy * b + syz * c;
                let tz = szx * a + syz * b + szz * c;
                let snn = tx * a + ty * b + tz * c;
                let tau2 = (tx * tx + ty * ty + tz * tz - snn * snn).max(0.0);
                let factor = eps[ci] as f64;
                let qt = snn.max(0.0) / (strength_t * factor).max(1e-9);
                let qs2 = tau2 / (strength_s * factor).max(1e-9).powi(2);
                let q = qt * qt + qs2;
                out[ci] = if q <= 1e-18 { sf_cap } else { (1.0 / q.sqrt()).min(sf_cap as f64) as f32 };
            }
        }
    }
    out
}

/// Accumulates results (load steps) into one [`SweepField`].
pub struct SweepBuilder {
    scored: Vec<([f32; 6], f32, f32)>,
    ringed: Vec<([f32; 6], f32, f32)>,
    sf_cap: f32,
    cells_seen: usize,
}

impl SweepBuilder {
    pub fn new(sf_cap: f32) -> Self {
        Self { scored: Vec::new(), ringed: Vec::new(), sf_cap, cells_seen: 0 }
    }

    /// Fold one result's stress state in. `u` is the padded nodal displacement
    /// (3/node), `eps` the per-cell stiffness factors the solve used
    /// (allowables scale by the SAME factor — Gibson–Ashby, exactly as the SF
    /// plots do), `eigen` a uniform eigenstrain ([0.0; 3] for structural
    /// solves), `ring` the constraint mask from [`constraint_ring_mask`].
    /// Cells whose best-possible 1/SF² (principal-stress upper bound) stays
    /// below 1/sf_cap² at every orientation are dropped — they can never move
    /// a pixel off the cap the sweep initializes at.
    #[allow(clippy::too_many_arguments)]
    pub fn add_result(
        &mut self,
        grid: &VoxelGrid,
        u: &[f32],
        e0: f64,
        nu: f64,
        eps: &[f32],
        eigen: [f64; 3],
        strength_t: f64,
        strength_s: f64,
        ring: &[bool],
    ) {
        let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
        let (mx, my) = (nx + 1, ny + 1);
        let inv4h = 1.0 / (4.0 * grid.h);
        let q_cap = 1.0 / (self.sf_cap as f64 * self.sf_cap as f64);
        for cz in 0..nz {
            for cy in 0..ny {
                for cx in 0..nx {
                    let ci = (cz * ny + cy) * nx + cx;
                    if eps[ci] <= 0.0 {
                        continue;
                    }
                    self.cells_seen += 1;
                    // Strain at the cell center (same stencil as stress.rs).
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
                    let (exx, eyy, ezz) = (
                        exx * inv4h - eigen[0],
                        eyy * inv4h - eigen[1],
                        ezz * inv4h - eigen[2],
                    );
                    let (gxy, gyz, gzx) = (gxy * inv4h, gyz * inv4h, gzx * inv4h);
                    let e = e0 * eps[ci] as f64;
                    let lam = e * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
                    let mu = e / (2.0 * (1.0 + nu));
                    let tr = exx + eyy + ezz;
                    let s = [
                        (lam * tr + 2.0 * mu * exx) as f32,
                        (lam * tr + 2.0 * mu * eyy) as f32,
                        (lam * tr + 2.0 * mu * ezz) as f32,
                        (mu * gxy) as f32,
                        (mu * gyz) as f32,
                        (mu * gzx) as f32,
                    ];
                    let factor = eps[ci] as f64;
                    let inv_t = 1.0 / (strength_t * factor).max(1e-9);
                    let inv_s = 1.0 / (strength_s * factor).max(1e-9);
                    // Prune: best possible q over ALL n. σₙₙ ≤ σ₁ and
                    // τ ≤ (σ₁−σ₃)/2, so q ≤ (⟨σ₁⟩₊·inv_t)² + (τmax·inv_s)².
                    let (s1, s3) = principal_range(s);
                    let qt = s1.max(0.0) * inv_t;
                    let qs = 0.5 * (s1 - s3) * inv_s;
                    if qt * qt + qs * qs <= q_cap {
                        continue;
                    }
                    let rec = (s, inv_t as f32, inv_s as f32);
                    if ring[ci] {
                        self.ringed.push(rec);
                    } else {
                        self.scored.push(rec);
                    }
                }
            }
        }
    }

    /// Freeze into the SoA sweep layout (scored cells first, ring cells after).
    pub fn finish(self) -> SweepField {
        let n_scored = self.scored.len();
        let total = n_scored + self.ringed.len();
        let mut s: [Vec<f32>; 6] = Default::default();
        for v in &mut s {
            v.reserve(total);
        }
        let mut inv_t = Vec::with_capacity(total);
        let mut inv_s = Vec::with_capacity(total);
        for (tensor, it, is) in self.scored.into_iter().chain(self.ringed) {
            for (k, v) in s.iter_mut().enumerate() {
                v.push(tensor[k]);
            }
            inv_t.push(it);
            inv_s.push(is);
        }
        SweepField { s, inv_t, inv_s, n_scored, sf_cap: self.sf_cap, cells_seen: self.cells_seen }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: f32 = 10.0;

    /// A SweepField holding exactly the given tensors (allowables 1.0/1.0,
    /// no ring, no pruning surprises — tensors are chosen well above cap).
    fn field(tensors: &[[f32; 6]], strength_t: f32, strength_s: f32) -> SweepField {
        let n = tensors.len();
        let mut s: [Vec<f32>; 6] = Default::default();
        for t in tensors {
            for (k, v) in s.iter_mut().enumerate() {
                v.push(t[k]);
            }
        }
        SweepField {
            s,
            inv_t: vec![1.0 / strength_t; n],
            inv_s: vec![1.0 / strength_s; n],
            n_scored: n,
            sf_cap: CAP,
            cells_seen: n,
        }
    }

    fn sf_at(f: &SweepField, pitch: f64, roll: f64) -> f32 {
        let d = layer_normal(pitch, roll);
        f.sweep(&[[d[0] as f32, d[1] as f32, d[2] as f32]])[0].0
    }

    #[test]
    fn uniaxial_tension_matches_hand_calc() {
        // σ = diag(4, 0, 0), Sᵗᶻ = 2, Sˢᶻ = 1.
        let f = field(&[[4.0, 0.0, 0.0, 0.0, 0.0, 0.0]], 2.0, 1.0);
        // Layers ⊥ X (n = x̂ at roll 90°): pure normal tension, SF = 2/4.
        assert!((sf_at(&f, 0.0, 90.0) - 0.5).abs() < 1e-5);
        // Layers ⊥ Z (n = ẑ): traction-free plane → cap.
        assert!((sf_at(&f, 0.0, 0.0) - CAP).abs() < 1e-5);
        // 45° in the xz-plane: σₙₙ = τ = 2 → q = (2/2)² + (2/1)² = 5.
        let sf = sf_at(&f, 0.0, 45.0);
        assert!((sf - 1.0 / 5f32.sqrt()).abs() < 1e-5, "sf {sf}");
    }

    #[test]
    fn compression_cannot_delaminate() {
        let f = field(&[[-4.0, 0.0, 0.0, 0.0, 0.0, 0.0]], 2.0, 1.0);
        // n = x̂: pure normal COMPRESSION, zero shear → cap.
        assert!((sf_at(&f, 0.0, 90.0) - CAP).abs() < 1e-5);
        // 45°: the tension term drops (σₙₙ = −2 → ⟨·⟩₊ = 0) but the shear
        // traction is still there: q = (2/1)² → SF = 0.5.
        assert!((sf_at(&f, 0.0, 45.0) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn pure_shear_matches_hand_calc() {
        // σxy = 3, Sˢᶻ = 1. n = x̂: t = (0,3,0), σₙₙ = 0, τ = 3 → SF = 1/3.
        let f = field(&[[0.0, 0.0, 0.0, 3.0, 0.0, 0.0]], 1.0, 1.0);
        let sf = sf_at(&f, 0.0, 90.0);
        assert!((sf - 1.0 / 3.0).abs() < 1e-5, "sf {sf}");
        // n = ẑ: the xy-shear plane carries no traction through ẑ → cap.
        assert!((sf_at(&f, 0.0, 0.0) - CAP).abs() < 1e-5);
    }

    #[test]
    fn flip_symmetry_on_grid_edges() {
        // Roll −90° and +90° are n = −x̂ / +x̂ — the criterion is quadratic in
        // n, so whole grid columns must agree exactly.
        let f = field(&[[3.0, -1.0, 2.0, 1.5, -0.5, 0.75]], 2.0, 1.0);
        let (n, dirs) = hemisphere_grid(15.0);
        let out = f.sweep(&dirs);
        for ip in 0..n {
            let l = out[ip * n];
            let r = out[ip * n + n - 1];
            assert!((l.0 - r.0).abs() < 1e-5 && (l.1 - r.1).abs() < 1e-5, "row {ip}: {l:?} {r:?}");
        }
        assert_eq!(n, 13);
        assert_eq!(dirs.len(), 13 * 13);
    }

    #[test]
    fn principal_range_agrees_with_diag_and_shear() {
        let (s1, s3) = principal_range([5.0, -2.0, 1.0, 0.0, 0.0, 0.0]);
        assert!((s1 - 5.0).abs() < 1e-9 && (s3 + 2.0).abs() < 1e-9);
        // Pure shear σxy = τ: principals ±τ.
        let (s1, s3) = principal_range([0.0, 0.0, 0.0, 3.0, 0.0, 0.0]);
        assert!((s1 - 3.0).abs() < 1e-6 && (s3 + 3.0).abs() < 1e-6, "{s1} {s3}");
    }

    #[test]
    fn worst_orientation_bounded_by_principal_prune_bound() {
        // For any tensor, min over the grid SF ≥ the prune bound's SF (the
        // bound is an upper bound on q). Coarse grid, generic tensor.
        let t = [3.0f32, -1.0, 2.0, 1.5, -0.5, 0.75];
        let f = field(&[t], 2.0, 1.0);
        let (_, dirs) = hemisphere_grid(5.0);
        let worst = f.sweep(&dirs).iter().map(|v| v.0).fold(f32::MAX, f32::min);
        let (s1, s3) = principal_range(t);
        let q_ub = (s1.max(0.0) / 2.0).powi(2) + (0.5 * (s1 - s3)).powi(2);
        let sf_lb = (1.0 / q_ub.sqrt()) as f32;
        assert!(worst >= sf_lb - 1e-5, "worst {worst} < bound {sf_lb}");
    }
}
