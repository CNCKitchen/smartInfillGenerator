// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Density bins and modifier-region extraction (DESIGN.md decisions #10/#3):
//! - level placement: bottom level pinned at the printability floor, upper
//!   levels by strain-energy-weighted k-means in stiffness space (convex
//!   E(ρ) makes dense infill more efficient per gram, so load levels land
//!   high — measured +15.2% vs uniform on the cantilever fixture, vs +13.9%
//!   for plain density-space k-means),
//! - assignment: anchored at the optimizer's per-cell choice, with a
//!   bisected mass multiplier so the binned design still meets the budget,
//! - regions are NESTED indicators (bin >= k) so exported modifiers overlap
//!   and the slicer's modifier order (low -> high density) resolves them,
//! - isosurfacing via marching tetrahedra on a node lattice with a void
//!   margin: closed, crack-free, watertight by construction,
//! - Taubin smoothing (no shrink) takes the staircase off.

use crate::voxel::VoxelGrid;
use std::collections::HashMap;

/// 1-D k-means on density values (equal cell volumes). Returns ascending centers.
pub fn cluster_densities(values: &[f64], k: usize) -> Vec<f64> {
    assert!(k >= 1);
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &v in values {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if !lo.is_finite() || hi - lo < 1e-9 {
        return vec![values.first().copied().unwrap_or(0.2); k.min(1).max(1)];
    }
    // Quantile init.
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut centers: Vec<f64> = (0..k)
        .map(|i| sorted[((i as f64 + 0.5) / k as f64 * (sorted.len() - 1) as f64) as usize])
        .collect();
    let mut assign = vec![0usize; values.len()];
    for _ in 0..60 {
        let mut changed = false;
        for (i, &v) in values.iter().enumerate() {
            let mut best = 0usize;
            let mut bd = f64::INFINITY;
            for (c, &cv) in centers.iter().enumerate() {
                let d = (v - cv).abs();
                if d < bd {
                    bd = d;
                    best = c;
                }
            }
            if assign[i] != best {
                assign[i] = best;
                changed = true;
            }
        }
        let mut sum = vec![0f64; k];
        let mut cnt = vec![0usize; k];
        for (i, &v) in values.iter().enumerate() {
            sum[assign[i]] += v;
            cnt[assign[i]] += 1;
        }
        for c in 0..k {
            if cnt[c] > 0 {
                centers[c] = sum[c] / cnt[c] as f64;
            }
        }
        if !changed {
            break;
        }
    }
    centers.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // Round to whole percent for the slicer; keep distinct.
    for c in centers.iter_mut() {
        *c = (*c * 100.0).round() / 100.0;
    }
    centers.dedup_by(|a, b| (*a - *b).abs() < 0.005);
    centers
}

/// Place `n` discrete infill levels for an optimized continuous field.
///
/// Level 0 is PINNED at `floor`: its job is printability (the structurally
/// idle bulk prints and supports its top surfaces), not load bearing. The
/// remaining levels minimize the COMPLIANCE error of quantization rather
/// than the mass error: weighted 1-D k-means in relative-stiffness space
/// y = coeff·x^exponent, weighted by per-cell strain energy. Because the
/// infill law is convex (exponent > 1), stiffness per gram grows with
/// density — the classic argument that makes intermediate densities
/// inefficient — so the energy weighting pushes the upper levels toward the
/// cap instead of averaging the structurally idle transition band ("one low
/// level so it prints, the rest rather high").
pub fn cluster_levels(
    x: &[f64],
    se: &[f64],
    n: usize,
    exponent: f64,
    coeff: f64,
    floor: f64,
    cap: f64,
) -> Vec<f64> {
    assert_eq!(x.len(), se.len());
    assert!(n >= 1);
    let mut levels = vec![floor];
    if n == 1 {
        return levels;
    }
    // Candidates for the load-bearing levels: cells meaningfully above floor.
    let mut ys: Vec<f64> = Vec::new();
    let mut ws: Vec<f64> = Vec::new();
    let mut w_total = 0.0;
    for (i, &xi) in x.iter().enumerate() {
        if xi > floor + 0.02 {
            ys.push(coeff * xi.powf(exponent));
            let w = se[i].max(0.0);
            ws.push(w);
            w_total += w;
        }
    }
    if ys.is_empty() {
        return levels; // whole interior sits at the floor — nothing to grade
    }
    if w_total <= 0.0 {
        ws.iter_mut().for_each(|w| *w = 1.0); // degenerate: volume weighting
    }
    let k = (n - 1).min(ys.len());
    for c in weighted_kmeans_1d(&ys, &ws, k) {
        levels.push((c / coeff).powf(1.0 / exponent).clamp(floor, cap));
    }
    levels.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // Round to whole percent for the slicer; merge near-duplicates.
    for l in levels.iter_mut() {
        *l = (*l * 100.0).round() / 100.0;
    }
    levels.dedup_by(|a, b| (*a - *b).abs() < 0.015);
    levels
}

/// Weighted 1-D k-means (Lloyd), weighted-quantile init, ascending centers.
fn weighted_kmeans_1d(v: &[f64], w: &[f64], k: usize) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|&a, &b| v[a].partial_cmp(&v[b]).unwrap());
    let total: f64 = w.iter().sum();
    let mut centers: Vec<f64> = Vec::with_capacity(k);
    let mut acc = 0.0;
    let mut next_target = 0.5 / k as f64 * total;
    for &i in &idx {
        acc += w[i];
        while centers.len() < k && acc >= next_target {
            centers.push(v[i]);
            next_target = (centers.len() as f64 + 0.5) / k as f64 * total;
        }
    }
    while centers.len() < k {
        centers.push(v[idx[idx.len() - 1]]);
    }
    let mut assign = vec![0usize; v.len()];
    for _ in 0..60 {
        let mut changed = false;
        for (i, &vi) in v.iter().enumerate() {
            let mut best = 0usize;
            let mut bd = f64::INFINITY;
            for (c, &cv) in centers.iter().enumerate() {
                let d = (vi - cv).abs();
                if d < bd {
                    bd = d;
                    best = c;
                }
            }
            if assign[i] != best {
                assign[i] = best;
                changed = true;
            }
        }
        let mut sum = vec![0f64; k];
        let mut wsum = vec![0f64; k];
        for (i, &vi) in v.iter().enumerate() {
            sum[assign[i]] += w[i] * vi;
            wsum[assign[i]] += w[i];
        }
        for c in 0..k {
            if wsum[c] > 0.0 {
                centers[c] = sum[c] / wsum[c];
            }
        }
        if !changed {
            break;
        }
    }
    centers.sort_by(|a, b| a.partial_cmp(b).unwrap());
    centers
}

/// Assign each cell to a level under a mass constraint, ANCHORED at the
/// optimizer's own choice: per cell minimize
///   w_c · (E(l) − E(x_c))² + λ · (l − x_c)
/// in relative-stiffness space, with the multiplier λ bisected until the
/// mean assigned density meets `target_mean`. At λ = 0 this is plain
/// nearest-stiffness quantization — the continuous field already encodes
/// the optimal mass distribution (at an OC optimum all marginal values are
/// equal, so re-ranking cells by frozen-u sensitivity only adds noise) —
/// and λ repairs the quantization mass drift by moving the cells with the
/// least strain energy first (w_c = se_c + ε keeps dead cells cheap to move
/// but still anchored).
pub fn assign_bins_mass(
    x: &[f64],
    se: &[f64],
    levels: &[f64],
    exponent: f64,
    coeff: f64,
    target_mean: f64,
) -> Vec<u8> {
    assert_eq!(x.len(), se.len());
    if levels.len() <= 1 {
        return vec![0u8; x.len()];
    }
    let rel: Vec<f64> = levels.iter().map(|&l| coeff * l.powf(exponent)).collect();
    let ex: Vec<f64> = x.iter().map(|&xi| coeff * xi.powf(exponent)).collect();
    let mean_se = se.iter().map(|s| s.max(0.0)).sum::<f64>() / se.len().max(1) as f64;
    let anchor = (0.01 * mean_se).max(1e-300);
    let w: Vec<f64> = se.iter().map(|s| s.max(0.0) + anchor).collect();
    let assign_for = |lambda: f64| -> Vec<u8> {
        (0..x.len())
            .map(|i| {
                let mut best = 0u8;
                let mut bs = f64::INFINITY;
                for (c, (&l, &r)) in levels.iter().zip(&rel).enumerate() {
                    let de = r - ex[i];
                    let s = w[i] * de * de + lambda * (l - x[i]);
                    if s < bs {
                        bs = s;
                        best = c as u8;
                    }
                }
                best
            })
            .collect()
    };
    let mean_of = |bins: &[u8]| -> f64 {
        bins.iter().map(|&b| levels[b as usize]).sum::<f64>() / bins.len().max(1) as f64
    };
    let mut bins = assign_for(0.0);
    if (mean_of(&bins) - target_mean).abs() <= 0.002 {
        return bins;
    }
    // Mean density is monotone decreasing in λ; saturate both ends.
    let bound = 1e4 * w.iter().cloned().fold(0.0, f64::max).max(1e-300);
    let (mut lo, mut hi) = (-bound, bound);
    for _ in 0..80 {
        let lambda = 0.5 * (lo + hi);
        bins = assign_for(lambda);
        let m = mean_of(&bins);
        if (m - target_mean).abs() <= 0.002 {
            break;
        }
        if m > target_mean {
            lo = lambda; // too heavy: raise the price of mass
        } else {
            hi = lambda;
        }
    }
    bins
}

/// Assign each value to the nearest center; returns bin index per value.
pub fn assign_bins(values: &[f64], centers: &[f64]) -> Vec<u8> {
    values
        .iter()
        .map(|&v| {
            let mut best = 0u8;
            let mut bd = f64::INFINITY;
            for (c, &cv) in centers.iter().enumerate() {
                let d = (v - cv).abs();
                if d < bd {
                    bd = d;
                    best = c as u8;
                }
            }
            best
        })
        .collect()
}

/// Demote connected components of `bin >= level` smaller than `min_cells`.
/// Operates highest level first so slivers cascade downward.
pub fn cleanup_small_regions(
    grid: &VoxelGrid,
    design_cells: &[u32],
    bins: &mut [u8],
    n_bins: usize,
    min_cells: usize,
) {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let mut bin_of_cell: HashMap<u32, usize> = HashMap::with_capacity(design_cells.len());
    for (i, &c) in design_cells.iter().enumerate() {
        bin_of_cell.insert(c, i);
    }
    for level in (1..n_bins).rev() {
        // Flood fill over cells with bin >= level.
        let mut seen = vec![false; design_cells.len()];
        for start in 0..design_cells.len() {
            if seen[start] || (bins[start] as usize) < level {
                continue;
            }
            let mut comp = vec![start as u32];
            let mut stack = vec![start as u32];
            seen[start] = true;
            while let Some(i) = stack.pop() {
                let c = design_cells[i as usize] as usize;
                let cx = c % nx;
                let cy = (c / nx) % ny;
                let cz = c / (nx * ny);
                let mut visit = |cc: usize| {
                    if let Some(&j) = bin_of_cell.get(&(cc as u32)) {
                        if !seen[j] && (bins[j] as usize) >= level {
                            seen[j] = true;
                            stack.push(j as u32);
                            comp.push(j as u32);
                        }
                    }
                };
                if cx > 0 {
                    visit(c - 1);
                }
                if cx + 1 < nx {
                    visit(c + 1);
                }
                if cy > 0 {
                    visit(c - nx);
                }
                if cy + 1 < ny {
                    visit(c + nx);
                }
                if cz > 0 {
                    visit(c - nx * ny);
                }
                if cz + 1 < nz {
                    visit(c + nx * ny);
                }
            }
            if comp.len() < min_cells {
                for &j in &comp {
                    bins[j as usize] = (level - 1) as u8;
                }
            }
        }
    }
}

pub struct RegionMesh {
    /// Bin density this region enforces (0..1).
    pub density: f64,
    pub positions: Vec<f32>, // xyz per vertex
    pub indices: Vec<u32>,   // 3 per triangle
}

/// Extract one watertight region mesh for cells where `inside` is true.
/// Binary indicator + iso 0.4 => slight dilation; used for export regions
/// (bin membership is a set, not a smooth field).
pub fn extract_region(grid: &VoxelGrid, inside: &dyn Fn(usize) -> bool, iso: f64) -> RegionMesh {
    extract_iso(grid, &|ci| if inside(ci) { 1.0 } else { 0.0 }, iso)
}

/// Isosurface of an arbitrary per-cell scalar at `iso`. Node lattice with a
/// one-cell void margin; node value = mean of the 8 adjacent cell values.
/// For smooth fields (e.g. filtered densities) this yields the true smooth
/// level set — the binary variant rasterizes to cell granularity and grows
/// tent-shaped spikes wherever single cells cross the threshold.
pub fn extract_iso(grid: &VoxelGrid, cell_value: &dyn Fn(usize) -> f64, iso: f64) -> RegionMesh {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    // Lattice nodes: grid nodes plus a margin ring => (nx+3) per axis.
    let (lx, ly, lz) = (nx + 3, ny + 3, nz + 3);
    let node = |i: usize, j: usize, k: usize| -> f64 {
        // Lattice node (i,j,k) = grid node (i-1, j-1, k-1); adjacent cells
        // (i-2..i-1, ...) in grid-cell coordinates.
        let mut acc = 0f64;
        for dz in 0..2 {
            for dy in 0..2 {
                for dx in 0..2 {
                    let cx = (i + dx) as isize - 2;
                    let cy = (j + dy) as isize - 2;
                    let cz = (k + dz) as isize - 2;
                    if cx < 0 || cy < 0 || cz < 0 {
                        continue;
                    }
                    let (cx, cy, cz) = (cx as usize, cy as usize, cz as usize);
                    if cx >= nx || cy >= ny || cz >= nz {
                        continue;
                    }
                    let ci = (cz * ny + cy) * nx + cx;
                    acc += cell_value(ci);
                }
            }
        }
        acc / 8.0
    };
    let mut field = vec![0f32; lx * ly * lz];
    for k in 0..lz {
        for j in 0..ly {
            for i in 0..lx {
                field[(k * ly + j) * lx + i] = node(i, j, k) as f32;
            }
        }
    }
    let origin = [grid.origin[0] - grid.h, grid.origin[1] - grid.h, grid.origin[2] - grid.h];
    let (positions, indices) = marching_tets(&field, lx, ly, lz, grid.h, origin, iso);
    RegionMesh { density: 0.0, positions, indices }
}

/// Marching tetrahedra over a scalar lattice (Kuhn 6-tet split: crack-free).
fn marching_tets(
    field: &[f32],
    lx: usize,
    ly: usize,
    lz: usize,
    h: f64,
    origin: [f64; 3],
    iso: f64,
) -> (Vec<f32>, Vec<u32>) {
    // Monotone lattice paths 000 -> 111.
    const TETS: [[[usize; 3]; 4]; 6] = [
        [[0, 0, 0], [1, 0, 0], [1, 1, 0], [1, 1, 1]],
        [[0, 0, 0], [1, 0, 0], [1, 0, 1], [1, 1, 1]],
        [[0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 1, 1]],
        [[0, 0, 0], [0, 1, 0], [0, 1, 1], [1, 1, 1]],
        [[0, 0, 0], [0, 0, 1], [1, 0, 1], [1, 1, 1]],
        [[0, 0, 0], [0, 0, 1], [0, 1, 1], [1, 1, 1]],
    ];
    let idx = |i: usize, j: usize, k: usize| (k * ly + j) * lx + i;
    let mut positions: Vec<f32> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut edge_vertex: HashMap<(u32, u32), u32> = HashMap::new();

    #[allow(clippy::too_many_arguments)]
    fn vertex_on_edge(
        a: u32,
        b: u32,
        field: &[f32],
        iso: f64,
        lx: usize,
        ly: usize,
        origin: [f64; 3],
        h: f64,
        positions: &mut Vec<f32>,
        edge_vertex: &mut HashMap<(u32, u32), u32>,
    ) -> u32 {
        let (a, b) = if a < b { (a, b) } else { (b, a) };
        if let Some(&v) = edge_vertex.get(&(a, b)) {
            return v;
        }
        let va = field[a as usize] as f64;
        let vb = field[b as usize] as f64;
        let t = ((iso - va) / (vb - va)).clamp(0.0, 1.0);
        let (ai, aj, ak) = ((a as usize) % lx, (a as usize / lx) % ly, (a as usize) / (lx * ly));
        let (bi, bj, bk) = ((b as usize) % lx, (b as usize / lx) % ly, (b as usize) / (lx * ly));
        let p = [
            origin[0] + (ai as f64 + t * (bi as f64 - ai as f64)) * h,
            origin[1] + (aj as f64 + t * (bj as f64 - aj as f64)) * h,
            origin[2] + (ak as f64 + t * (bk as f64 - ak as f64)) * h,
        ];
        let v = (positions.len() / 3) as u32;
        positions.extend_from_slice(&[p[0] as f32, p[1] as f32, p[2] as f32]);
        edge_vertex.insert((a, b), v);
        v
    }
    macro_rules! voe {
        ($a:expr, $b:expr) => {
            vertex_on_edge($a, $b, field, iso, lx, ly, origin, h, &mut positions, &mut edge_vertex)
        };
    }

    for k in 0..lz - 1 {
        for j in 0..ly - 1 {
            for i in 0..lx - 1 {
                // Skip cubes entirely on one side.
                let mut any_in = false;
                let mut any_out = false;
                for dz in 0..2 {
                    for dy in 0..2 {
                        for dx in 0..2 {
                            if field[idx(i + dx, j + dy, k + dz)] as f64 > iso {
                                any_in = true;
                            } else {
                                any_out = true;
                            }
                        }
                    }
                }
                if !any_in || !any_out {
                    continue;
                }
                for tet in &TETS {
                    let n: Vec<u32> = tet
                        .iter()
                        .map(|d| idx(i + d[0], j + d[1], k + d[2]) as u32)
                        .collect();
                    let inside: Vec<bool> =
                        n.iter().map(|&v| field[v as usize] as f64 > iso).collect();
                    let in_count = inside.iter().filter(|&&b| b).count();
                    if in_count == 0 || in_count == 4 {
                        continue;
                    }
                    let ins: Vec<u32> =
                        (0..4).filter(|&q| inside[q]).map(|q| n[q]).collect();
                    let outs: Vec<u32> =
                        (0..4).filter(|&q| !inside[q]).map(|q| n[q]).collect();
                    let mut tris: Vec<[u32; 3]> = Vec::new();
                    if in_count == 1 {
                        let v = [voe!(ins[0], outs[0]), voe!(ins[0], outs[1]), voe!(ins[0], outs[2])];
                        tris.push(v);
                    } else if in_count == 3 {
                        let v = [voe!(outs[0], ins[0]), voe!(outs[0], ins[1]), voe!(outs[0], ins[2])];
                        tris.push(v);
                    } else {
                        // 2-2: quad from the four crossing edges.
                        let v00 = voe!(ins[0], outs[0]);
                        let v01 = voe!(ins[0], outs[1]);
                        let v10 = voe!(ins[1], outs[0]);
                        let v11 = voe!(ins[1], outs[1]);
                        tris.push([v00, v01, v11]);
                        tris.push([v00, v11, v10]);
                    }
                    // Orient: normal must point from inside (>iso) to outside.
                    let centroid = |set: &[u32]| -> [f64; 3] {
                        let mut c = [0f64; 3];
                        for &v in set {
                            let (vi, vj, vk) = (
                                (v as usize) % lx,
                                (v as usize / lx) % ly,
                                (v as usize) / (lx * ly),
                            );
                            c[0] += origin[0] + vi as f64 * h;
                            c[1] += origin[1] + vj as f64 * h;
                            c[2] += origin[2] + vk as f64 * h;
                        }
                        for d in 0..3 {
                            c[d] /= set.len() as f64;
                        }
                        c
                    };
                    let ci = centroid(&ins);
                    let co = centroid(&outs);
                    let dir = [co[0] - ci[0], co[1] - ci[1], co[2] - ci[2]];
                    for t in &mut tris {
                        let p = |v: u32| {
                            let v = v as usize;
                            [
                                positions[3 * v] as f64,
                                positions[3 * v + 1] as f64,
                                positions[3 * v + 2] as f64,
                            ]
                        };
                        let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
                        let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                        let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
                        let nrm = [
                            e1[1] * e2[2] - e1[2] * e2[1],
                            e1[2] * e2[0] - e1[0] * e2[2],
                            e1[0] * e2[1] - e1[1] * e2[0],
                        ];
                        if nrm[0] * dir[0] + nrm[1] * dir[1] + nrm[2] * dir[2] < 0.0 {
                            t.swap(1, 2);
                        }
                        // Drop degenerate (duplicate-vertex) triangles.
                        if t[0] != t[1] && t[1] != t[2] && t[0] != t[2] {
                            indices.extend_from_slice(t);
                        }
                    }
                }
            }
        }
    }
    (positions, indices)
}

/// Taubin lambda/mu smoothing (volume-preserving-ish), in place.
pub fn taubin_smooth(positions: &mut [f32], indices: &[u32], passes: usize) {
    let nv = positions.len() / 3;
    if nv == 0 {
        return;
    }
    // Vertex adjacency.
    let mut neighbors: Vec<Vec<u32>> = vec![Vec::new(); nv];
    let add = |a: u32, b: u32, neighbors: &mut Vec<Vec<u32>>| {
        if !neighbors[a as usize].contains(&b) {
            neighbors[a as usize].push(b);
        }
    };
    for t in indices.chunks(3) {
        add(t[0], t[1], &mut neighbors);
        add(t[1], t[0], &mut neighbors);
        add(t[1], t[2], &mut neighbors);
        add(t[2], t[1], &mut neighbors);
        add(t[2], t[0], &mut neighbors);
        add(t[0], t[2], &mut neighbors);
    }
    let mut tmp = vec![0f32; positions.len()];
    // Taubin λ/μ pair. The pass-band kPB = 1/λ − 1/|μ| sets how much detail
    // survives: a SMALLER band removes more (smoother). These (0.63/−0.65,
    // kPB ≈ 0.048) are deliberately more aggressive than the textbook
    // 0.5/−0.53 (kPB ≈ 0.11) so cranking the slider visibly melts the voxel
    // staircase; λ/μ stay volume-preserving (no net shrink).
    for pass in 0..passes * 2 {
        let factor = if pass % 2 == 0 { 0.63f32 } else { -0.65f32 };
        for v in 0..nv {
            let nb = &neighbors[v];
            if nb.is_empty() {
                tmp[3 * v] = positions[3 * v];
                tmp[3 * v + 1] = positions[3 * v + 1];
                tmp[3 * v + 2] = positions[3 * v + 2];
                continue;
            }
            let mut c = [0f32; 3];
            for &u in nb {
                c[0] += positions[3 * u as usize];
                c[1] += positions[3 * u as usize + 1];
                c[2] += positions[3 * u as usize + 2];
            }
            for d in 0..3 {
                c[d] /= nb.len() as f32;
                tmp[3 * v + d] = positions[3 * v + d] + factor * (c[d] - positions[3 * v + d]);
            }
        }
        positions.copy_from_slice(&tmp);
    }
}
