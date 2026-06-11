// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! 8-node hexahedral element for linear elasticity on a regular voxel grid.
//! One reference stiffness matrix KE per grid level (unit cube scaled by h and
//! E), scaled per cell by a relative stiffness factor — the standard
//! matrix-free topology-optimization formulation.

/// Local node order (isoparametric signs). Must stay consistent with
/// NODE_OFFSETS and everything that gathers/scatters cell DOFs.
pub const NODE_SIGNS: [[f64; 3]; 8] = [
    [-1.0, -1.0, -1.0],
    [1.0, -1.0, -1.0],
    [1.0, 1.0, -1.0],
    [-1.0, 1.0, -1.0],
    [-1.0, -1.0, 1.0],
    [1.0, -1.0, 1.0],
    [1.0, 1.0, 1.0],
    [-1.0, 1.0, 1.0],
];

/// Grid offset (dx,dy,dz) of local node l relative to the cell's min corner.
pub const NODE_OFFSETS: [[usize; 3]; 8] = [
    [0, 0, 0],
    [1, 0, 0],
    [1, 1, 0],
    [0, 1, 0],
    [0, 0, 1],
    [1, 0, 1],
    [1, 1, 1],
    [0, 1, 1],
];

/// Element stiffness matrix (24x24) for an h-sized cube, Young's modulus `e`,
/// Poisson ratio `nu`. Full 2x2x2 Gauss integration.
pub fn ke_hex(e: f64, nu: f64, h: f64) -> [[f64; 24]; 24] {
    let lam = e * nu / ((1.0 + nu) * (1.0 - 2.0 * nu));
    let mu = e / (2.0 * (1.0 + nu));
    let mut c = [[0.0f64; 6]; 6];
    for i in 0..3 {
        for j in 0..3 {
            c[i][j] = lam;
        }
        c[i][i] = lam + 2.0 * mu;
        c[i + 3][i + 3] = mu;
    }

    let g = 1.0 / 3.0f64.sqrt();
    let mut ke = [[0.0f64; 24]; 24];
    let detj_w = (h / 2.0).powi(3); // |J| * gauss weight (w=1)

    for gp in 0..8 {
        let (xi, eta, zeta) =
            (g * NODE_SIGNS[gp][0], g * NODE_SIGNS[gp][1], g * NODE_SIGNS[gp][2]);
        // dN/dx for each node (J = h/2 * I on a cube grid)
        let mut dndx = [[0.0f64; 3]; 8];
        for l in 0..8 {
            let [sx, sy, sz] = NODE_SIGNS[l];
            let f = 2.0 / h; // J^-1 factor
            dndx[l][0] = f * sx * (1.0 + eta * sy) * (1.0 + zeta * sz) / 8.0;
            dndx[l][1] = f * sy * (1.0 + xi * sx) * (1.0 + zeta * sz) / 8.0;
            dndx[l][2] = f * sz * (1.0 + xi * sx) * (1.0 + eta * sy) / 8.0;
        }
        // B matrix, engineering strain order: xx yy zz xy yz zx
        let mut b = [[0.0f64; 24]; 6];
        for l in 0..8 {
            let col = 3 * l;
            b[0][col] = dndx[l][0];
            b[1][col + 1] = dndx[l][1];
            b[2][col + 2] = dndx[l][2];
            b[3][col] = dndx[l][1];
            b[3][col + 1] = dndx[l][0];
            b[4][col + 1] = dndx[l][2];
            b[4][col + 2] = dndx[l][1];
            b[5][col] = dndx[l][2];
            b[5][col + 2] = dndx[l][0];
        }
        // ke += B^T C B * detj_w
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
        for i in 0..24 {
            for j in 0..24 {
                let mut s = 0.0;
                for k in 0..6 {
                    s += b[k][i] * cb[k][j];
                }
                ke[i][j] += detj_w * s;
            }
        }
    }
    ke
}

/// The 8 per-node 3x3 diagonal blocks of KE (for block-Jacobi smoothing).
pub fn ke_diag_blocks(ke: &[[f64; 24]; 24]) -> [[[f64; 3]; 3]; 8] {
    let mut blocks = [[[0.0f64; 3]; 3]; 8];
    for l in 0..8 {
        for r in 0..3 {
            for cc in 0..3 {
                blocks[l][r][cc] = ke[3 * l + r][3 * l + cc];
            }
        }
    }
    blocks
}

/// Invert a (well-conditioned SPD) 3x3 matrix; returns None when singular.
pub fn invert3(m: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-300 {
        return None;
    }
    let inv_det = 1.0 / det;
    let mut inv = [[0.0f64; 3]; 3];
    inv[0][0] = (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det;
    inv[0][1] = (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det;
    inv[0][2] = (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det;
    inv[1][0] = (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det;
    inv[1][1] = (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det;
    inv[1][2] = (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det;
    inv[2][0] = (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det;
    inv[2][1] = (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det;
    inv[2][2] = (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det;
    Some(inv)
}
