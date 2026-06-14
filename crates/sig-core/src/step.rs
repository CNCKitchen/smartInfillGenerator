// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! STEP (ISO 10303) import via the `truck` CAD kernel (Apache-2.0, vetted
//! 2026-06-13 — see crates/sig-core/Cargo.toml).
//!
//! STEP carries BREP geometry (trimmed NURBS faces). The downstream pipeline is
//! triangle-soup based (winding-number voxelization), so we tessellate the BREP
//! to a [`TriMesh`]. The value STEP adds over STL is *face topology*: we keep a
//! `face_of_tri` map so segmentation can seed patches from real CAD faces
//! instead of guessing them by dihedral angle (see [`crate::segment`]).
//!
//! ## Tessellation control
//! truck exposes ONE native knob — a surface-deviation (chord) tolerance fed to
//! its constrained-Delaunay triangulator (no separate normal-deviation /
//! max-edge / aspect-ratio controls like a commercial kernel's import dialog).
//! This module's job is only to produce a "fine enough to start with" BASE
//! tessellation, exactly analogous to a freshly imported STL: curved faces are
//! resolved by the deviation tolerance, and the engine then applies the SAME
//! edge-length refinement it runs on every imported STL (`Model::new` ->
//! [`TriMesh::subdivided_with_parents`]) for display vertex density.
//!
//! We deliberately do NOT remesh for triangle regularity here. truck's
//! parameter-space Delaunay is sliver-prone, but slivers are cosmetic for our
//! pipeline: voxelization is quality-agnostic, and result mapping depends on
//! vertex density (set by the shared downstream refinement), not triangle shape.

use crate::mesh::{triangle_area_vector, TriMesh};
use truck_meshalgo::tessellation::*;
use truck_stepio::r#in::Table;

/// Tessellation settings for [`import_step`].
#[derive(Clone, Debug)]
pub struct StepTessellation {
    /// Surface-deviation (chord) tolerance in model units (mm), passed straight
    /// to truck. Smaller = finer facets on curved faces. `None` = auto: a
    /// fraction (`auto_deviation_frac`) of the model's bounding-box diagonal.
    ///
    /// NOTE: this sets CURVE resolution at import time. The engine's downstream
    /// refinement only splits existing facets (it can't recover curvature), so
    /// this must be fine enough for the tightest curved feature that matters —
    /// hence a conservatively small auto default.
    pub surface_deviation: Option<f64>,
    /// Fraction of the bbox diagonal used when `surface_deviation` is `None`.
    pub auto_deviation_frac: f64,
}

impl Default for StepTessellation {
    fn default() -> Self {
        Self {
            surface_deviation: None,
            auto_deviation_frac: 0.00025, // 0.025% of the diagonal — fine base
        }
    }
}

/// Result of a STEP import.
#[derive(Clone, Debug)]
pub struct StepImport {
    /// Tessellated triangle soup, ready for voxelization.
    pub mesh: TriMesh,
    /// BREP face id per triangle (same length/order as `mesh.tris`). Triangles
    /// from the same CAD face share an id; ids are contiguous across all shells.
    pub face_of_tri: Vec<u32>,
    /// Number of BREP faces encountered (some may have contributed no triangles).
    pub face_count: usize,
    /// Number of distinct BREP shells (closed shells ≈ solid bodies).
    pub shell_count: usize,
    /// The surface-deviation tolerance actually used (mm).
    pub tolerance: f64,
}

/// Errors from [`import_step`].
#[derive(Debug)]
pub enum StepError {
    /// The STEP text could not be parsed (`ruststep`).
    Parse(String),
    /// The file parsed but held no DATA section.
    NoData,
    /// No BREP shell could be converted to geometry (unsupported entities, or
    /// truck failed on every shell).
    NoShells,
    /// Every shell converted but tessellation produced zero triangles.
    Empty,
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepError::Parse(s) => write!(f, "could not parse STEP file: {s}"),
            StepError::NoData => write!(f, "STEP file has no DATA section"),
            StepError::NoShells => write!(
                f,
                "no BREP shell could be read (unsupported STEP entities?) — \
                 try exporting STL/3MF from your CAD instead"
            ),
            StepError::Empty => write!(f, "tessellation produced no triangles"),
        }
    }
}

impl std::error::Error for StepError {}

/// Parse STEP `bytes`, tessellate the BREP, and return a triangle soup plus the
/// per-triangle CAD-face map.
pub fn import_step(bytes: &[u8], settings: &StepTessellation) -> Result<StepImport, StepError> {
    // STEP Part 21 is ASCII (occasionally Latin-1 in string fields). Lossy
    // decode keeps a stray byte in a label from killing the whole parse.
    let text = String::from_utf8_lossy(bytes);

    let exchange = truck_stepio::r#in::ruststep::parser::parse(&text)
        .map_err(|e| StepError::Parse(format!("{e:?}")))?;
    if exchange.data.is_empty() {
        return Err(StepError::NoData);
    }
    let table = Table::from_data_section(&exchange.data[0]);

    // Convert every BREP shell to truck's compressed form; skip ones truck can't
    // handle rather than failing the whole import.
    let cshells: Vec<_> = table
        .shell
        .values()
        .filter_map(|shell| table.to_compressed_shell(shell).ok())
        .collect();
    if cshells.is_empty() {
        return Err(StepError::NoShells);
    }

    // Resolve the chord tolerance. Auto = fraction of the BREP corner-vertex
    // bbox diagonal (cheap; underestimates very curved parts with few vertices,
    // which is why an explicit `surface_deviation` override exists).
    let tol = match settings.surface_deviation {
        Some(d) => d.max(1e-6),
        None => {
            let mut lo = [f64::INFINITY; 3];
            let mut hi = [f64::NEG_INFINITY; 3];
            for cs in &cshells {
                for v in &cs.vertices {
                    let p: [f64; 3] = (*v).into();
                    for d in 0..3 {
                        lo[d] = lo[d].min(p[d]);
                        hi[d] = hi[d].max(p[d]);
                    }
                }
            }
            let diag = ((hi[0] - lo[0]).powi(2)
                + (hi[1] - lo[1]).powi(2)
                + (hi[2] - lo[2]).powi(2))
            .sqrt();
            (diag * settings.auto_deviation_frac).max(1e-4)
        }
    };

    // Tessellate each shell; flatten per-face polygon meshes into one soup,
    // tagging triangles with a contiguous BREP-face id.
    let mut tris: Vec<[f32; 9]> = Vec::new();
    let mut face_of_tri: Vec<u32> = Vec::new();
    let mut face_id: u32 = 0;
    for cs in &cshells {
        let poly = cs.robust_triangulation(tol);
        for face in &poly.faces {
            if let Some(mesh) = &face.surface {
                let pos = mesh.positions();
                // face_iter yields each polygon as a vertex slice (tri/quad/n-gon);
                // fan-triangulate uniformly.
                for fv in mesh.faces().face_iter() {
                    if fv.len() < 3 {
                        continue;
                    }
                    for i in 1..fv.len() - 1 {
                        let a = pos[fv[0].pos];
                        let b = pos[fv[i].pos];
                        let c = pos[fv[i + 1].pos];
                        let t = [
                            a.x as f32, a.y as f32, a.z as f32,
                            b.x as f32, b.y as f32, b.z as f32,
                            c.x as f32, c.y as f32, c.z as f32,
                        ];
                        // Drop exact-degenerate triangles (the rest of the
                        // pipeline tolerates slivers; this just keeps stats honest).
                        if !t.iter().all(|x| x.is_finite()) {
                            continue;
                        }
                        if triangle_area_vector(&t) == [0.0, 0.0, 0.0] {
                            continue;
                        }
                        tris.push(t);
                        face_of_tri.push(face_id);
                    }
                }
            }
            face_id += 1;
        }
    }
    if tris.is_empty() {
        return Err(StepError::Empty);
    }

    // Return the BASE tessellation as-is — the caller refines it through the
    // same path as an imported STL (see module docs). `face_of_tri` is indexed
    // by base triangle and carried for segmentation seeding by the caller.
    Ok(StepImport {
        mesh: TriMesh::from_triangles(tris),
        face_of_tri,
        face_count: face_id as usize,
        shell_count: cshells.len(),
        tolerance: tol,
    })
}
