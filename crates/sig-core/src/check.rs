//! Pre-solve sanity checks (DESIGN.md decision #6):
//! - disconnected solid islands (each must be independently constrained),
//! - rigid-body-mode rank test of the constraint set per island,
//! - on failure, the offending rigid-body motion for the UI to animate.

use crate::voxel::VoxelGrid;

pub struct Islands {
    pub count: usize,
    /// Component id per cell; u32::MAX for void cells.
    pub cell_component: Vec<u32>,
}

/// 6-connected flood fill over solid cells.
pub fn islands(grid: &VoxelGrid) -> Islands {
    let (nx, ny, nz) = (grid.nx, grid.ny, grid.nz);
    let mut comp = vec![u32::MAX; nx * ny * nz];
    let mut count = 0u32;
    let mut stack: Vec<u32> = Vec::new();
    for start in 0..nx * ny * nz {
        if grid.scale[start] <= 0.0 || comp[start] != u32::MAX {
            continue;
        }
        comp[start] = count;
        stack.push(start as u32);
        while let Some(ci) = stack.pop() {
            let ci = ci as usize;
            let cx = ci % nx;
            let cy = (ci / nx) % ny;
            let cz = ci / (nx * ny);
            let mut visit = |c: usize| {
                if grid.scale[c] > 0.0 && comp[c] == u32::MAX {
                    comp[c] = count;
                    stack.push(c as u32);
                }
            };
            if cx > 0 {
                visit(ci - 1);
            }
            if cx + 1 < nx {
                visit(ci + 1);
            }
            if cy > 0 {
                visit(ci - nx);
            }
            if cy + 1 < ny {
                visit(ci + nx);
            }
            if cz > 0 {
                visit(ci - nx * ny);
            }
            if cz + 1 < nz {
                visit(ci + nx * ny);
            }
        }
        count += 1;
    }
    Islands { count: count as usize, cell_component: comp }
}

/// One scalar constraint: motion at `pos` along unit `dir` is blocked.
#[derive(Clone, Copy, Debug)]
pub struct ConstraintDir {
    pub pos: [f64; 3],
    pub dir: [f64; 3],
}

#[derive(Clone, Debug)]
pub struct RbmMode {
    /// Rigid translation component.
    pub t: [f64; 3],
    /// Rigid rotation vector (rad per unit amplitude); u(p) = t + r x (p - center).
    pub r: [f64; 3],
    pub center: [f64; 3],
}

#[derive(Clone, Debug)]
pub struct RbmResult {
    pub ok: bool,
    /// Smallest / largest eigenvalue of the constraint Gram matrix.
    pub lambda_ratio: f64,
    /// Present when under-constrained: the free rigid-body motion.
    pub mode: Option<RbmMode>,
}

/// Rank test: do the constraints kill all 6 rigid-body modes?
/// `center`/`scale` condition the rotation rows (use component centroid and
/// half bounding-box diagonal).
pub fn rbm_check(constraints: &[ConstraintDir], center: [f64; 3], scale: f64) -> RbmResult {
    let s = scale.max(1e-12);
    let mut m = [[0f64; 6]; 6];
    for c in constraints {
        let p = [
            (c.pos[0] - center[0]) / s,
            (c.pos[1] - center[1]) / s,
            (c.pos[2] - center[2]) / s,
        ];
        let d = c.dir;
        // g_k = d . u_k(p) for the 6 rigid modes (3 translations, 3 rotations e_k x p).
        let g = [
            d[0],
            d[1],
            d[2],
            d[1] * (-p[2]) + d[2] * p[1], // e_x x p = (0, -z, y)
            d[0] * p[2] + d[2] * (-p[0]), // e_y x p = (z, 0, -x)
            d[0] * (-p[1]) + d[1] * p[0], // e_z x p = (-y, x, 0)
        ];
        for i in 0..6 {
            for j in 0..6 {
                m[i][j] += g[i] * g[j];
            }
        }
    }
    let (vals, vecs) = jacobi6(&m);
    let lmax = vals.iter().cloned().fold(0.0f64, f64::max);
    let (kmin, &lmin) =
        vals.iter().enumerate().min_by(|a, b| a.1.partial_cmp(b.1).unwrap()).unwrap();
    let ratio = if lmax > 0.0 { lmin / lmax } else { 0.0 };
    let ok = lmax > 0.0 && ratio > 1e-7;
    let mode = if ok {
        None
    } else {
        let v = vecs[kmin];
        Some(RbmMode {
            t: [v[0], v[1], v[2]],
            // De-scale rotations: u = t + (r/s) x (p - center).
            r: [v[3] / s, v[4] / s, v[5] / s],
            center,
        })
    };
    RbmResult { ok, lambda_ratio: ratio, mode }
}

/// Cyclic Jacobi eigen-decomposition for a symmetric 6x6 matrix.
/// Returns (eigenvalues, eigenvectors as rows).
fn jacobi6(a_in: &[[f64; 6]; 6]) -> ([f64; 6], [[f64; 6]; 6]) {
    let mut a = *a_in;
    let mut v = [[0f64; 6]; 6];
    for i in 0..6 {
        v[i][i] = 1.0;
    }
    for _sweep in 0..50 {
        let mut off = 0f64;
        for i in 0..6 {
            for j in i + 1..6 {
                off += a[i][j] * a[i][j];
            }
        }
        if off < 1e-30 {
            break;
        }
        for p in 0..6 {
            for q in p + 1..6 {
                if a[p][q].abs() < 1e-300 {
                    continue;
                }
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for k in 0..6 {
                    let akp = a[k][p];
                    let akq = a[k][q];
                    a[k][p] = c * akp - s * akq;
                    a[k][q] = s * akp + c * akq;
                }
                for k in 0..6 {
                    let apk = a[p][k];
                    let aqk = a[q][k];
                    a[p][k] = c * apk - s * aqk;
                    a[q][k] = s * apk + c * aqk;
                }
                for k in 0..6 {
                    let vkp = v[p][k];
                    let vkq = v[q][k];
                    v[p][k] = c * vkp - s * vkq;
                    v[q][k] = s * vkp + c * vkq;
                }
            }
        }
    }
    let mut vals = [0f64; 6];
    for i in 0..6 {
        vals[i] = a[i][i];
    }
    (vals, v)
}
