// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! 3MF input/output.
//!
//! Export targets the Bambu/Orca project flavor reverse-engineered from the
//! reference Cube.3mf: production-extension geometry split into
//! 3D/Objects/object_1.model, with per-part settings (modifier_part +
//! sparse_infill_density) carried in Metadata/model_settings.config.
//! Deviation from the sample (two rounds of real-Orca testing): modifiers
//! override ONLY sparse_infill_density — never wall keys. The sample's
//! wall_loops=0 strips perimeters where a modifier touches the surface, so
//! every region must inherit the part's own perimeter settings. The PART
//! (object level) does carry wall_loops = the perimeter count the user set
//! in the app, so the print matches the solid skin the analysis assumed.
//! No project_settings.config is written on purpose: the user's own printer/
//! filament/process presets stay active when the project opens.

use crate::bins::RegionMesh;
use crate::mesh::TriMesh;
use crate::zip::{read_zip, ZipError, ZipWriter};
use std::collections::HashMap;

pub struct IndexedMesh {
    pub vertices: Vec<[f32; 3]>,
    pub triangles: Vec<[u32; 3]>,
}

/// Weld a triangle soup into an indexed mesh (quantized by bbox*1e-6).
pub fn weld(mesh: &TriMesh) -> IndexedMesh {
    let (lo, hi) = mesh.bounds().unwrap_or(([0.0; 3], [1.0; 3]));
    let diag = ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2) + (hi[2] - lo[2]).powi(2)).sqrt();
    let q = (diag * 1e-6).max(1e-9);
    let mut ids: HashMap<(i64, i64, i64), u32> = HashMap::new();
    let mut vertices: Vec<[f32; 3]> = Vec::new();
    let mut triangles: Vec<[u32; 3]> = Vec::with_capacity(mesh.tris.len());
    for t in &mesh.tris {
        let mut tri = [0u32; 3];
        for v in 0..3 {
            let p = [t[3 * v], t[3 * v + 1], t[3 * v + 2]];
            let key = (
                (p[0] as f64 / q).round() as i64,
                (p[1] as f64 / q).round() as i64,
                (p[2] as f64 / q).round() as i64,
            );
            tri[v] = *ids.entry(key).or_insert_with(|| {
                vertices.push(p);
                (vertices.len() - 1) as u32
            });
        }
        if tri[0] != tri[1] && tri[1] != tri[2] && tri[0] != tri[2] {
            triangles.push(tri);
        }
    }
    IndexedMesh { vertices, triangles }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn mesh_xml(m: &IndexedMesh) -> String {
    let mut s = String::with_capacity(m.vertices.len() * 40 + m.triangles.len() * 40);
    s.push_str("   <mesh>\n    <vertices>\n");
    for v in &m.vertices {
        s.push_str(&format!("     <vertex x=\"{}\" y=\"{}\" z=\"{}\"/>\n", v[0], v[1], v[2]));
    }
    s.push_str("    </vertices>\n    <triangles>\n");
    for t in &m.triangles {
        s.push_str(&format!("     <triangle v1=\"{}\" v2=\"{}\" v3=\"{}\"/>\n", t[0], t[1], t[2]));
    }
    s.push_str("    </triangles>\n   </mesh>\n");
    s
}

fn region_to_indexed(r: &RegionMesh) -> IndexedMesh {
    IndexedMesh {
        vertices: r.positions.chunks(3).map(|c| [c[0], c[1], c[2]]).collect(),
        triangles: r.indices.chunks(3).map(|c| [c[0], c[1], c[2]]).collect(),
    }
}

/// Build the Orca/Bambu project 3MF: the part plus one nested modifier mesh
/// per density bin above the base. `base_density` (0..1) and `wall_loops`
/// (the perimeter count the analysis assumed) are written as object-level
/// overrides so the print matches the simulation without touching the user's
/// process preset. `solid_pattern` (e.g. "rectilinear" / "concentric"), when
/// given, sets sparse_infill_pattern ON EACH MODIFIER — used by the binary
/// (hollow/solid) mode where the dense regions slice as solid fill. It is
/// deliberately NOT written as object-level internal_solid_infill_pattern:
/// newer Bambu Studio renamed that key's "rectilinear" value to "zig-zag"
/// and pops a "values have been replaced" dialog on every load, while
/// "rectilinear"/"concentric" remain valid sparse-pattern values everywhere.
/// Modifiers otherwise override ONLY the infill density — walls/shells
/// inherit from the part (a modifier wall key strips/changes perimeters
/// wherever it touches the surface). Regions must be sorted ascending by
/// density (slicer modifier order resolves the nesting).
/// Minimal valid 1×1 PNG used as the plate thumbnail. Bambu Studio / OrcaSlicer
/// only treat a 3MF as one of THEIR projects (and therefore load
/// `model_settings.config` — our modifiers) when `_rels/.rels` carries a
/// `schemas.bambulab.com/.../cover-thumbnail-*` relationship; that relationship
/// must point at a real image, so we ship this tiny placeholder. Without it the
/// loader warns "The 3mf is not from Bambu Lab, load geometry data only" and
/// drops the modifiers.
const THUMB_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 252, 207, 192, 80, 15, 0, 4,
    133, 1, 128, 132, 169, 140, 33, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// Bambu-flavored `slice_info.config` (the `X-BBL-Client` header is part of how
/// the loader recognizes its own files).
const SLICE_INFO: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<config>\n  <header>\n    <header_item key=\"X-BBL-Client-Type\" value=\"slicer\"/>\n    <header_item key=\"X-BBL-Client-Version\" value=\"02.07.00.00\"/>\n  </header>\n</config>\n";

pub fn export_orca_3mf(
    part_name: &str,
    part: &IndexedMesh,
    regions: &[RegionMesh],
    base_density: f64,
    wall_loops: u32,
    top_bottom_layers: u32,
    solid_pattern: Option<&str>,
    thumbnail: Option<&[u8]>,
) -> Vec<u8> {
    let n_objects = 1 + regions.len();

    // Plate placement: center x/y on a 256 bed, drop z to the plate.
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for v in &part.vertices {
        for d in 0..3 {
            lo[d] = lo[d].min(v[d]);
            hi[d] = hi[d].max(v[d]);
        }
    }
    let tx = 128.0 - (lo[0] + hi[0]) / 2.0;
    let ty = 128.0 - (lo[1] + hi[1]) / 2.0;
    let tz = -lo[2];
    let place = format!("1 0 0 0 1 0 0 0 1 {tx} {ty} {tz}");

    let uuid = |n: usize| format!("{:08x}-89ab-cdef-0123-456789abcdef", n + 1);
    let assembly_id = n_objects + 1;

    // ---- 3D/Objects/object_1.model: all meshes ----
    let mut obj = String::new();
    obj.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    obj.push_str("<model unit=\"millimeter\" xml:lang=\"en-US\" xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\" xmlns:BambuStudio=\"http://schemas.bambulab.com/package/2021\" xmlns:p=\"http://schemas.microsoft.com/3dmanufacturing/production/2015/06\" requiredextensions=\"p\">\n");
    obj.push_str(" <metadata name=\"BambuStudio:3mfVersion\">1</metadata>\n <resources>\n");
    obj.push_str(&format!("  <object id=\"1\" p:UUID=\"{}\" type=\"model\">\n", uuid(1)));
    obj.push_str(&mesh_xml(part));
    obj.push_str("  </object>\n");
    for (k, r) in regions.iter().enumerate() {
        let id = k + 2;
        obj.push_str(&format!("  <object id=\"{id}\" p:UUID=\"{}\" type=\"model\">\n", uuid(id)));
        obj.push_str(&mesh_xml(&region_to_indexed(r)));
        obj.push_str("  </object>\n");
    }
    obj.push_str(" </resources>\n <build/>\n</model>\n");

    // ---- 3D/3dmodel.model: assembly of components ----
    let mut root = String::new();
    root.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    root.push_str("<model unit=\"millimeter\" xml:lang=\"en-US\" xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\" xmlns:BambuStudio=\"http://schemas.bambulab.com/package/2021\" xmlns:p=\"http://schemas.microsoft.com/3dmanufacturing/production/2015/06\" requiredextensions=\"p\">\n");
    // Vendor string the loader recognizes as one of its own projects. (Real
    // OrcaSlicer exports also write "BambuStudio-…"; ours is filaSim under the
    // hood — the Title metadata carries the part name.)
    root.push_str(" <metadata name=\"Application\">BambuStudio-02.07.00.00</metadata>\n");
    root.push_str(" <metadata name=\"BambuStudio:3mfVersion\">1</metadata>\n");
    root.push_str(&format!(" <metadata name=\"Title\">{}</metadata>\n", xml_escape(part_name)));
    root.push_str(" <resources>\n");
    root.push_str(&format!("  <object id=\"{assembly_id}\" p:UUID=\"{}\" type=\"model\">\n   <components>\n", uuid(assembly_id)));
    for id in 1..=n_objects {
        root.push_str(&format!(
            "    <component p:path=\"/3D/Objects/object_1.model\" objectid=\"{id}\" p:UUID=\"{}\" transform=\"1 0 0 0 1 0 0 0 1 0 0 0\"/>\n",
            uuid(100 + id)
        ));
    }
    root.push_str("   </components>\n  </object>\n </resources>\n");
    root.push_str(&format!(
        " <build p:UUID=\"{}\">\n  <item objectid=\"{assembly_id}\" p:UUID=\"{}\" transform=\"{place}\" printable=\"1\"/>\n </build>\n</model>\n",
        uuid(200),
        uuid(201)
    ));

    // ---- Metadata/model_settings.config ----
    let mut cfg = String::new();
    cfg.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<config>\n");
    cfg.push_str(&format!("  <object id=\"{assembly_id}\">\n"));
    cfg.push_str(&format!("    <metadata key=\"name\" value=\"{}\"/>\n", xml_escape(part_name)));
    cfg.push_str("    <metadata key=\"extruder\" value=\"1\"/>\n");
    cfg.push_str(&format!(
        "    <metadata key=\"sparse_infill_density\" value=\"{}%\"/>\n",
        (base_density * 100.0).round() as u32
    ));
    // Binary mode: set the OBJECT-level (general) infill pattern too, so the
    // base/sparse infill prints in the chosen pattern, not just the modifiers.
    if let Some(p) = solid_pattern {
        cfg.push_str(&format!(
            "    <metadata key=\"sparse_infill_pattern\" value=\"{}\"/>\n",
            xml_escape(p)
        ));
    }
    cfg.push_str(&format!("    <metadata key=\"wall_loops\" value=\"{wall_loops}\"/>\n"));
    // Top/bottom shells the analysis assumed (0 = open infill showpieces).
    cfg.push_str(&format!(
        "    <metadata key=\"top_shell_layers\" value=\"{top_bottom_layers}\"/>\n"
    ));
    cfg.push_str(&format!(
        "    <metadata key=\"bottom_shell_layers\" value=\"{top_bottom_layers}\"/>\n"
    ));
    cfg.push_str("    <part id=\"1\" subtype=\"normal_part\">\n");
    cfg.push_str(&format!("      <metadata key=\"name\" value=\"{}\"/>\n", xml_escape(part_name)));
    cfg.push_str("      <metadata key=\"matrix\" value=\"1 0 0 0 0 1 0 0 0 0 1 0 0 0 0 1\"/>\n");
    cfg.push_str("      <mesh_stat edges_fixed=\"0\" degenerate_facets=\"0\" facets_removed=\"0\" facets_reversed=\"0\" backwards_edges=\"0\"/>\n");
    cfg.push_str("    </part>\n");
    for (k, r) in regions.iter().enumerate() {
        let id = k + 2;
        let pct = (r.density * 100.0).round() as u32;
        cfg.push_str(&format!("    <part id=\"{id}\" subtype=\"modifier_part\">\n"));
        cfg.push_str(&format!("      <metadata key=\"name\" value=\"infill {pct}%\"/>\n"));
        cfg.push_str("      <metadata key=\"matrix\" value=\"1 0 0 0 0 1 0 0 0 0 1 0 0 0 0 1\"/>\n");
        cfg.push_str("      <metadata key=\"extruder\" value=\"0\"/>\n");
        cfg.push_str(&format!(
            "      <metadata key=\"sparse_infill_density\" value=\"{pct}%\"/>\n"
        ));
        if let Some(p) = solid_pattern {
            cfg.push_str(&format!(
                "      <metadata key=\"sparse_infill_pattern\" value=\"{}\"/>\n",
                xml_escape(p)
            ));
        }
        cfg.push_str("      <mesh_stat edges_fixed=\"0\" degenerate_facets=\"0\" facets_removed=\"0\" facets_reversed=\"0\" backwards_edges=\"0\"/>\n");
        cfg.push_str("    </part>\n");
    }
    cfg.push_str("  </object>\n  <plate>\n");
    cfg.push_str("    <metadata key=\"plater_id\" value=\"1\"/>\n");
    cfg.push_str("    <metadata key=\"plater_name\" value=\"\"/>\n");
    cfg.push_str("    <metadata key=\"locked\" value=\"false\"/>\n");
    cfg.push_str("    <metadata key=\"thumbnail_file\" value=\"Metadata/plate_1.png\"/>\n");
    cfg.push_str("    <model_instance>\n");
    cfg.push_str(&format!("      <metadata key=\"object_id\" value=\"{assembly_id}\"/>\n"));
    cfg.push_str("      <metadata key=\"instance_id\" value=\"0\"/>\n");
    cfg.push_str("      <metadata key=\"identify_id\" value=\"463\"/>\n");
    cfg.push_str("    </model_instance>\n  </plate>\n");
    cfg.push_str(&format!(
        "  <assemble>\n   <assemble_item object_id=\"{assembly_id}\" instance_id=\"0\" transform=\"{place}\" offset=\"0 0 0\" />\n  </assemble>\n"
    ));
    cfg.push_str("</config>\n");

    // ---- container plumbing ----
    let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\n <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\n <Default Extension=\"model\" ContentType=\"application/vnd.ms-package.3dmanufacturing-3dmodel+xml\"/>\n <Default Extension=\"png\" ContentType=\"image/png\"/>\n</Types>\n";
    // The bambulab.com cover-thumbnail relationships are what flips the loader's
    // `is_bbl_3mf` flag — without them Bambu/Orca treat the file as a foreign
    // 3MF and drop the modifiers. The plain `metadata/thumbnail` relationship is
    // the generic OPC preview.
    let rels = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n <Relationship Target=\"/3D/3dmodel.model\" Id=\"rel-1\" Type=\"http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel\"/>\n <Relationship Target=\"/Metadata/plate_1.png\" Id=\"rel-2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail\"/>\n <Relationship Target=\"/Metadata/plate_1.png\" Id=\"rel-4\" Type=\"http://schemas.bambulab.com/package/2021/cover-thumbnail-middle\"/>\n <Relationship Target=\"/Metadata/plate_1_small.png\" Id=\"rel-5\" Type=\"http://schemas.bambulab.com/package/2021/cover-thumbnail-small\"/>\n</Relationships>\n";
    let model_rels = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n <Relationship Target=\"/3D/Objects/object_1.model\" Id=\"rel-1\" Type=\"http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel\"/>\n</Relationships>\n";

    let mut zip = ZipWriter::new();
    zip.add("[Content_Types].xml", content_types.as_bytes());
    zip.add("_rels/.rels", rels.as_bytes());
    zip.add("3D/3dmodel.model", root.as_bytes());
    zip.add("3D/_rels/3dmodel.model.rels", model_rels.as_bytes());
    zip.add("3D/Objects/object_1.model", obj.as_bytes());
    zip.add("Metadata/model_settings.config", cfg.as_bytes());
    // Bambu-project markers (see THUMB_PNG / SLICE_INFO): the cover thumbnails
    // the relationships point at, plus the slicer header. The plate thumbnail is
    // a snapshot of the optimized part when the caller supplies one, else the
    // 1×1 placeholder (still enough to flip the is_bbl_3mf flag).
    let thumb = thumbnail.filter(|t| !t.is_empty()).unwrap_or(THUMB_PNG);
    zip.add("Metadata/slice_info.config", SLICE_INFO.as_bytes());
    zip.add("Metadata/plate_1.png", thumb);
    zip.add("Metadata/plate_1_small.png", thumb);
    zip.finish()
}

fn tri_normal(v: &[[f32; 3]], a: u32, b: u32, c: u32) -> [f32; 3] {
    let (pa, pb, pc) = (v[a as usize], v[b as usize], v[c as usize]);
    let e1 = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
    let e2 = [pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]];
    [
        e1[1] * e2[2] - e1[2] * e2[1],
        e1[2] * e2[0] - e1[0] * e2[2],
        e1[0] * e2[1] - e1[1] * e2[0],
    ]
}

/// Append triangle (a,b,c), flipping it if its winding disagrees with the parent
/// normal `n` — midpoint sub-triangles are coplanar with the parent, so this
/// keeps every child's outward orientation correct regardless of template order.
fn push_oriented(out: &mut Vec<[u32; 3]>, v: &[[f32; 3]], n: &[f32; 3], a: u32, b: u32, c: u32) {
    let m = tri_normal(v, a, b, c);
    if m[0] * n[0] + m[1] * n[1] + m[2] * n[2] >= 0.0 {
        out.push([a, b, c]);
    } else {
        out.push([a, c, b]);
    }
}

/// ONE conforming red–green refinement pass. `mark_edge(verts, a, b)` decides
/// whether the welded edge (a,b) is split; an edge is split iff EITHER incident
/// triangle asks for it, and gets ONE shared midpoint keyed by its welded vertex
/// pair — so the result is watertight wherever the input was (no T-junctions,
/// unlike `TriMesh::subdivided`/`capped_edges`). Every split pattern (1/2/3 edges)
/// is itself conforming: only interior diagonals differ between neighbours, and
/// winding is preserved per parent. Returns the refined mesh and whether anything
/// split (false ⇒ no edge was marked, a fixed point).
pub fn subdivide_pass<F: Fn(&[[f32; 3]], u32, u32) -> bool>(
    mesh: &IndexedMesh,
    mark_edge: F,
) -> (IndexedMesh, bool) {
    use std::collections::{HashMap, HashSet};
    let key = |a: u32, b: u32| if a < b { (a, b) } else { (b, a) };
    // 1. mark edges (shared key → both incident triangles agree).
    let mut marked: HashSet<(u32, u32)> = HashSet::new();
    for t in &mesh.triangles {
        for &(a, b) in &[(t[0], t[1]), (t[1], t[2]), (t[2], t[0])] {
            if mark_edge(&mesh.vertices, a, b) {
                marked.insert(key(a, b));
            }
        }
    }
    if marked.is_empty() {
        return (IndexedMesh { vertices: mesh.vertices.clone(), triangles: mesh.triangles.clone() }, false);
    }
    let mut verts = mesh.vertices.clone();
    // 2. one shared midpoint per marked edge.
    let mut mid: HashMap<(u32, u32), u32> = HashMap::with_capacity(marked.len());
    for &(a, b) in &marked {
        let (p, q) = (verts[a as usize], verts[b as usize]);
        verts.push([(p[0] + q[0]) * 0.5, (p[1] + q[1]) * 0.5, (p[2] + q[2]) * 0.5]);
        mid.insert((a, b), verts.len() as u32 - 1);
    }
    let dist2 = |v: &Vec<[f32; 3]>, a: u32, b: u32| {
        let (p, q) = (v[a as usize], v[b as usize]);
        (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)
    };
    // 3. rebuild each triangle by its split pattern.
    let mut nt: Vec<[u32; 3]> = Vec::with_capacity(mesh.triangles.len() * 2);
    for t in &mesh.triangles {
        let (a, b, c) = (t[0], t[1], t[2]);
        let n = tri_normal(&verts, a, b, c);
        let mab = mid.get(&key(a, b)).copied();
        let mbc = mid.get(&key(b, c)).copied();
        let mca = mid.get(&key(c, a)).copied();
        match (mab, mbc, mca) {
            (None, None, None) => nt.push([a, b, c]),
            // 1 edge → split to the opposite vertex.
            (Some(p), None, None) => {
                push_oriented(&mut nt, &verts, &n, a, p, c);
                push_oriented(&mut nt, &verts, &n, p, b, c);
            }
            (None, Some(p), None) => {
                push_oriented(&mut nt, &verts, &n, a, b, p);
                push_oriented(&mut nt, &verts, &n, a, p, c);
            }
            (None, None, Some(p)) => {
                push_oriented(&mut nt, &verts, &n, a, b, p);
                push_oriented(&mut nt, &verts, &n, p, b, c);
            }
            // 2 edges → corner triangle + quad (shorter interior diagonal).
            (Some(p), Some(q), None) => {
                push_oriented(&mut nt, &verts, &n, p, b, q); // corner b
                if dist2(&verts, a, q) <= dist2(&verts, p, c) {
                    push_oriented(&mut nt, &verts, &n, a, p, q);
                    push_oriented(&mut nt, &verts, &n, a, q, c);
                } else {
                    push_oriented(&mut nt, &verts, &n, a, p, c);
                    push_oriented(&mut nt, &verts, &n, p, q, c);
                }
            }
            (None, Some(q), Some(r)) => {
                push_oriented(&mut nt, &verts, &n, r, q, c); // corner c
                if dist2(&verts, b, r) <= dist2(&verts, a, q) {
                    push_oriented(&mut nt, &verts, &n, a, b, r);
                    push_oriented(&mut nt, &verts, &n, b, q, r);
                } else {
                    push_oriented(&mut nt, &verts, &n, a, b, q);
                    push_oriented(&mut nt, &verts, &n, a, q, r);
                }
            }
            (Some(p), None, Some(r)) => {
                push_oriented(&mut nt, &verts, &n, a, p, r); // corner a
                if dist2(&verts, p, c) <= dist2(&verts, b, r) {
                    push_oriented(&mut nt, &verts, &n, p, b, c);
                    push_oriented(&mut nt, &verts, &n, p, c, r);
                } else {
                    push_oriented(&mut nt, &verts, &n, p, b, r);
                    push_oriented(&mut nt, &verts, &n, b, c, r);
                }
            }
            // 3 edges → classic red 4-split.
            (Some(p), Some(q), Some(r)) => {
                push_oriented(&mut nt, &verts, &n, a, p, r);
                push_oriented(&mut nt, &verts, &n, p, b, q);
                push_oriented(&mut nt, &verts, &n, r, q, c);
                push_oriented(&mut nt, &verts, &n, p, q, r);
            }
        }
    }
    (IndexedMesh { vertices: verts, triangles: nt }, true)
}

/// Conformingly refine until every edge is `<= target` (driver over
/// [`subdivide_pass`]). The `max_tris` guard leaves headroom for a pass's
/// worst-case 4× growth (a full red-split), so the returned count never blows
/// far past the budget. Returns the mesh and `true` iff the target was actually
/// reached (vs. stopped early on the budget/pass cap — then edges may exceed
/// `target` and the caller may want to warn).
pub fn subdivide_to_edge_checked(
    mesh: &IndexedMesh,
    target: f32,
    max_tris: usize,
) -> (IndexedMesh, bool) {
    let t2 = target.max(1e-6).powi(2);
    let mut m = IndexedMesh { vertices: mesh.vertices.clone(), triangles: mesh.triangles.clone() };
    for _ in 0..24 {
        if m.triangles.len().saturating_mul(4) >= max_tris {
            return (m, false);
        }
        let (next, split) = subdivide_pass(&m, |v, a, b| {
            let (p, q) = (v[a as usize], v[b as usize]);
            (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2) > t2
        });
        if !split {
            return (m, true);
        }
        m = next;
    }
    (m, true)
}

/// Convenience wrapper: refine to `target`, discarding the "target met" flag.
pub fn subdivide_to_edge(mesh: &IndexedMesh, target: f32, max_tris: usize) -> IndexedMesh {
    subdivide_to_edge_checked(mesh, target, max_tris).0
}

/// Band index of a scalar value for `steps` equal bands over `[lo, hi]`. Band `b`
/// covers `[lo + b·step, lo + (b+1)·step)`; below `lo` → 0, at/above `hi` →
/// `steps-1`. Counts how many of the interior boundaries `lo + k·step` the value
/// reaches, comparing against the SAME f32 boundary values `isoband_cut` clips on
/// — so a vertex exactly on a boundary lands deterministically in the upper band
/// (no floor/round mislabel), keeping cut geometry and band labels in lockstep.
pub fn band_index(lo: f32, hi: f32, steps: u32, v: f32) -> u32 {
    let steps = steps.max(1);
    if hi <= lo {
        return 0;
    }
    let step = (hi - lo) / steps as f32;
    let mut b = 0u32;
    let mut k = 1u32;
    while k < steps {
        if v >= lo + k as f32 * step {
            b = k;
            k += 1;
        } else {
            break;
        }
    }
    b
}

/// Clip a scalar-tagged polygon (CCW list of (position, scalar)) to one side of
/// a scalar threshold via Sutherland–Hodgman. `keep_ge` keeps the `s >= thr`
/// half, else the `s <= thr` half. Crossing points interpolate position
/// linearly and carry scalar exactly `thr`.
fn clip_scalar_half(poly: &[([f32; 3], f32)], thr: f32, keep_ge: bool) -> Vec<([f32; 3], f32)> {
    let n = poly.len();
    let mut out: Vec<([f32; 3], f32)> = Vec::with_capacity(n + 2);
    let inside = |s: f32| if keep_ge { s >= thr } else { s <= thr };
    for i in 0..n {
        let (pp, ps) = poly[i];
        let (qp, qs) = poly[(i + 1) % n];
        let pin = inside(ps);
        let qin = inside(qs);
        if pin {
            out.push((pp, ps));
        }
        if pin != qin {
            // ps != qs here (opposite sides), so the divisor is non-zero.
            let t = (thr - ps) / (qs - ps);
            let ip = [
                pp[0] + (qp[0] - pp[0]) * t,
                pp[1] + (qp[1] - pp[1]) * t,
                pp[2] + (qp[2] - pp[2]) * t,
            ];
            out.push((ip, thr));
        }
    }
    out
}

/// Split a per-vertex-scalar triangle soup (`positions` 9 floats/tri, `scalars`
/// 3/tri) into sub-triangles that each lie wholly inside one contour band, and
/// tag each with its band index. Band `b` covers `[lo + b·step, lo + (b+1)·step)`
/// with `step = (hi-lo)/steps`; band 0 is unbounded below, band `steps-1`
/// unbounded above. The cut runs along the field's exact iso-lines, so band
/// edges are razor-sharp and — because a shared mesh edge's crossing depends
/// only on its two endpoint scalars (identical from both incident triangles) —
/// the result stays watertight. Triangles that don't straddle a boundary pass
/// through unsplit. Returns (positions 9/tri, band per tri).
pub fn isoband_cut(positions: &[f32], scalars: &[f32], lo: f32, hi: f32, steps: u32) -> (Vec<f32>, Vec<u32>) {
    let steps = steps.max(1);
    let ntri = positions.len() / 9;
    let mut out_pos: Vec<f32> = Vec::with_capacity(positions.len());
    let mut out_band: Vec<u32> = Vec::with_capacity(ntri);
    let step = (hi - lo) / steps as f32;
    let band_of = |v: f32| band_index(lo, hi, steps, v);
    let mut push_tri = |a: [f32; 3], b: [f32; 3], c: [f32; 3], band: u32| {
        // Drop near-degenerate slivers from clipping (zero-area, no color shown).
        let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let cx = e1[1] * e2[2] - e1[2] * e2[1];
        let cy = e1[2] * e2[0] - e1[0] * e2[2];
        let cz = e1[0] * e2[1] - e1[1] * e2[0];
        if cx * cx + cy * cy + cz * cz <= 1e-20 {
            return;
        }
        out_pos.extend_from_slice(&[a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]);
        out_band.push(band);
    };
    for t in 0..ntri {
        let p = [
            [positions[t * 9], positions[t * 9 + 1], positions[t * 9 + 2]],
            [positions[t * 9 + 3], positions[t * 9 + 4], positions[t * 9 + 5]],
            [positions[t * 9 + 6], positions[t * 9 + 7], positions[t * 9 + 8]],
        ];
        let s = [scalars[t * 3], scalars[t * 3 + 1], scalars[t * 3 + 2]];
        let smin = s[0].min(s[1]).min(s[2]);
        let smax = s[0].max(s[1]).max(s[2]);
        let bmin = band_of(smin);
        let bmax = band_of(smax);
        if bmin == bmax {
            // Wholly inside one band — pass through unsplit.
            push_tri(p[0], p[1], p[2], bmin);
            continue;
        }
        for b in bmin..=bmax {
            let mut poly = vec![(p[0], s[0]), (p[1], s[1]), (p[2], s[2])];
            if b > 0 {
                poly = clip_scalar_half(&poly, lo + b as f32 * step, true);
            }
            if b < steps - 1 {
                poly = clip_scalar_half(&poly, lo + (b + 1) as f32 * step, false);
            }
            if poly.len() < 3 {
                continue;
            }
            for k in 1..poly.len() - 1 {
                push_tri(poly[0].0, poly[k].0, poly[k + 1].0, b);
            }
        }
    }
    (out_pos, out_band)
}

/// Iso-band cut on an INDEXED mesh with per-VERTEX scalars. Identical bands to
/// [`isoband_cut`], but EXACTLY watertight: every band-boundary crossing is
/// computed ONCE per shared edge from its canonical (min,max) endpoint order and
/// cached, so the two triangles across an edge get bit-identical crossing points
/// — no float-mismatch micro-cracks (the per-triangle soup version can leave a
/// few). All iso-lines of a linear field on a triangle are edge-to-edge chords,
/// so every cut vertex lies on a mesh edge and is therefore shared. `scalars` is
/// one value per vertex (shared at shared vertices → consistent). Returns
/// (positions 9/tri, band per tri).
pub fn isoband_cut_indexed(
    verts: &[[f32; 3]],
    tris: &[[u32; 3]],
    scalars: &[f32],
    lo: f32,
    hi: f32,
    steps: u32,
) -> (Vec<f32>, Vec<u32>) {
    use std::collections::HashMap;
    let steps = steps.max(1);
    let step = (hi - lo) / steps as f32;
    let mut out_pos: Vec<f32> = Vec::new();
    let mut out_band: Vec<u32> = Vec::new();
    if step <= 0.0 {
        // Degenerate range → everything band 0, geometry unchanged.
        for tri in tris {
            let p = [verts[tri[0] as usize], verts[tri[1] as usize], verts[tri[2] as usize]];
            out_pos.extend_from_slice(&[
                p[0][0], p[0][1], p[0][2], p[1][0], p[1][1], p[1][2], p[2][0], p[2][1], p[2][2],
            ]);
            out_band.push(0);
        }
        return (out_pos, out_band);
    }
    // Crossing point on edge {a,b} at boundary k (threshold lo+k·step), cached by
    // canonical (min,max,k) so both incident triangles read the SAME f32 point.
    let mut cache: HashMap<(u32, u32, u32), [f32; 3]> = HashMap::new();
    let mut crossing = |a: u32, b: u32, k: u32, cache: &mut HashMap<(u32, u32, u32), [f32; 3]>| {
        let (u, w) = if a < b { (a, b) } else { (b, a) };
        if let Some(&p) = cache.get(&(u, w, k)) {
            return p;
        }
        let thr = lo + k as f32 * step;
        let (su, sw) = (scalars[u as usize], scalars[w as usize]);
        let (pu, pw) = (verts[u as usize], verts[w as usize]);
        let denom = sw - su;
        let t = if denom != 0.0 { ((thr - su) / denom).clamp(0.0, 1.0) } else { 0.0 };
        let p = [pu[0] + (pw[0] - pu[0]) * t, pu[1] + (pw[1] - pu[1]) * t, pu[2] + (pw[2] - pu[2]) * t];
        cache.insert((u, w, k), p);
        p
    };
    let mut push_tri =
        |a: [f32; 3], b: [f32; 3], c: [f32; 3], band: u32, op: &mut Vec<f32>, ob: &mut Vec<u32>| {
            let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let cx = e1[1] * e2[2] - e1[2] * e2[1];
            let cy = e1[2] * e2[0] - e1[0] * e2[2];
            let cz = e1[0] * e2[1] - e1[1] * e2[0];
            if cx * cx + cy * cy + cz * cz <= 1e-20 {
                return;
            }
            op.extend_from_slice(&[a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]);
            ob.push(band);
        };
    for tri in tris {
        let vid = [tri[0], tri[1], tri[2]];
        let p = [verts[vid[0] as usize], verts[vid[1] as usize], verts[vid[2] as usize]];
        let s = [scalars[vid[0] as usize], scalars[vid[1] as usize], scalars[vid[2] as usize]];
        let bmin = band_index(lo, hi, steps, s[0].min(s[1]).min(s[2]));
        let bmax = band_index(lo, hi, steps, s[0].max(s[1]).max(s[2]));
        if bmin == bmax {
            push_tri(p[0], p[1], p[2], bmin, &mut out_pos, &mut out_band);
            continue;
        }
        for b in bmin..=bmax {
            // Walk the 3 directed edges; collect this band's polygon as in-band
            // corners + shared boundary crossings, in boundary order. (Standard
            // slab clip, but crossings come from the cache → watertight.)
            let mut poly: Vec<[f32; 3]> = Vec::with_capacity(6);
            for e in 0..3 {
                let (i, j) = (e, (e + 1) % 3);
                let (si, sj) = (s[i], s[j]);
                let in_lo = b == 0 || si >= lo + b as f32 * step;
                let in_hi = b == steps - 1 || si <= lo + (b + 1) as f32 * step;
                if in_lo && in_hi {
                    poly.push(p[i]);
                }
                let mut xs: Vec<(f32, [f32; 3])> = Vec::new();
                for &k in &[b, b + 1] {
                    if k == 0 || k >= steps {
                        continue; // unbounded (−∞ / +∞) side, no crossing
                    }
                    let thr = lo + k as f32 * step;
                    if (si < thr) != (sj < thr) {
                        let t = (thr - si) / (sj - si); // param from i, for ordering only
                        xs.push((t, crossing(vid[i], vid[j], k, &mut cache)));
                    }
                }
                xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                for (_, pt) in xs {
                    poly.push(pt);
                }
            }
            for k in 1..poly.len().saturating_sub(1) {
                push_tri(poly[0], poly[k], poly[k + 1], b, &mut out_pos, &mut out_band);
            }
        }
    }
    (out_pos, out_band)
}

/// Encode a Bambu/Orca `paint_color` whole-triangle (leaf) code for a 1-based
/// filament index. Clean-room reimplementation of the slicer's
/// `TriangleSelector` leaf serialization, reverse-engineered and verified
/// against `hook5_painted.3mf` (state 2 -> "8", 3 -> "0C", 4 -> "1C"; see the
/// `paint_color_roundtrip` test). Bit layout, per node: 2-bit split-sides
/// (0 = leaf — all our triangles are leaves), then the state: 2 bits when < 3,
/// else the marker `0b11` followed by 3-bit groups each trailed by a
/// continuation bit, decoded value = 3 + sum(group << 3*k). Bits are packed
/// LSB-first into nibbles and the nibble order is reversed in the final string.
fn paint_color_code(state: u32) -> String {
    let mut bits: Vec<u8> = Vec::new();
    let mut push = |v: u32, n: u32| {
        for i in 0..n {
            bits.push(((v >> i) & 1) as u8);
        }
    };
    push(0, 2); // split-sides = 0 -> leaf
    if state < 3 {
        push(state, 2);
    } else {
        push(3, 2); // extended marker
        let mut e = state - 3;
        loop {
            push(e & 7, 3);
            e >>= 3;
            push(if e != 0 { 1 } else { 0 }, 1);
            if e == 0 {
                break;
            }
        }
    }
    while bits.len() % 4 != 0 {
        bits.push(0);
    }
    let mut nibbles: Vec<u8> = bits
        .chunks_exact(4)
        .map(|c| c[0] | c[1] << 1 | c[2] << 2 | c[3] << 3)
        .collect();
    nibbles.reverse();
    nibbles.iter().map(|&v| char::from_digit(v as u32, 16).unwrap().to_ascii_uppercase()).collect()
}

/// Weld a triangle soup (9 floats/tri) into an indexed mesh, carrying a
/// per-triangle band index through in lockstep (degenerate triangles are
/// dropped from both, so `bands` stays aligned to `triangles`).
fn weld_with_bands(positions: &[f32], tri_band: &[u32]) -> (IndexedMesh, Vec<u32>) {
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for p in positions.chunks_exact(3) {
        for d in 0..3 {
            lo[d] = lo[d].min(p[d]);
            hi[d] = hi[d].max(p[d]);
        }
    }
    let diag = ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2) + (hi[2] - lo[2]).powi(2)).sqrt();
    let q = (diag as f64 * 1e-6).max(1e-9);
    let mut ids: HashMap<(i64, i64, i64), u32> = HashMap::new();
    let mut vertices: Vec<[f32; 3]> = Vec::new();
    let mut triangles: Vec<[u32; 3]> = Vec::new();
    let mut bands: Vec<u32> = Vec::new();
    for (ti, t) in positions.chunks_exact(9).enumerate() {
        let mut tri = [0u32; 3];
        for v in 0..3 {
            let p = [t[3 * v], t[3 * v + 1], t[3 * v + 2]];
            let key = (
                (p[0] as f64 / q).round() as i64,
                (p[1] as f64 / q).round() as i64,
                (p[2] as f64 / q).round() as i64,
            );
            tri[v] = *ids.entry(key).or_insert_with(|| {
                vertices.push(p);
                (vertices.len() - 1) as u32
            });
        }
        if tri[0] != tri[1] && tri[1] != tri[2] && tri[0] != tri[2] {
            triangles.push(tri);
            bands.push(tri_band.get(ti).copied().unwrap_or(0));
        }
    }
    (IndexedMesh { vertices, triangles }, bands)
}

/// Mesh XML with a `paint_color` attribute per painted triangle. Band 0 maps to
/// the object's base filament (extruder 1) and is left unpainted (the slicer
/// convention); band b > 0 paints with filament index b + 1.
fn mesh_xml_painted(m: &IndexedMesh, bands: &[u32]) -> String {
    let mut s = String::with_capacity(m.vertices.len() * 40 + m.triangles.len() * 48);
    s.push_str("   <mesh>\n    <vertices>\n");
    for v in &m.vertices {
        s.push_str(&format!("     <vertex x=\"{}\" y=\"{}\" z=\"{}\"/>\n", v[0], v[1], v[2]));
    }
    s.push_str("    </vertices>\n    <triangles>\n");
    for (t, &b) in m.triangles.iter().zip(bands) {
        if b == 0 {
            s.push_str(&format!("     <triangle v1=\"{}\" v2=\"{}\" v3=\"{}\"/>\n", t[0], t[1], t[2]));
        } else {
            s.push_str(&format!(
                "     <triangle v1=\"{}\" v2=\"{}\" v3=\"{}\" paint_color=\"{}\"/>\n",
                t[0],
                t[1],
                t[2],
                paint_color_code(b + 1)
            ));
        }
    }
    s.push_str("    </triangles>\n   </mesh>\n");
    s
}

/// Clean-room `project_settings.config` defining one filament per color band so
/// Bambu/Orca renders the painted model in the contour colors on open. Only the
/// filament arrays the painting needs are written (all sized to N) plus a
/// generic single-extruder-multi-material Bambu printer identity; everything
/// else falls back to the user's active preset. Authored here — no third-party
/// print profile is embedded.
fn color_project_settings(colors: &[String]) -> String {
    let n = colors.len();
    let rep = |val: &str| -> String {
        let items: Vec<String> = (0..n).map(|_| format!("\"{}\"", val)).collect();
        format!("[{}]", items.join(", "))
    };
    let colour_arr = {
        let items: Vec<String> =
            colors.iter().map(|c| format!("\"{}\"", xml_escape(c))).collect();
        format!("[{}]", items.join(", "))
    };
    let mut e: Vec<(String, String)> = Vec::new();
    let mut kv = |k: &str, v: String| e.push((k.to_string(), v));
    kv("version", "\"2.1.0.0\"".into());
    kv("printer_technology", "\"FFF\"".into());
    kv("printer_model", "\"Bambu Lab X1 Carbon\"".into());
    kv("printer_settings_id", "\"Bambu Lab X1 Carbon 0.4 nozzle\"".into());
    kv("printer_variant", "\"0.4\"".into());
    kv("nozzle_diameter", "[\"0.4\"]".into());
    kv("printable_area", "[\"0x0\", \"256x0\", \"256x256\", \"0x256\"]".into());
    kv("single_extruder_multi_material", "\"1\"".into());
    kv("filament_colour", colour_arr.clone());
    kv("filament_multi_colour", colour_arr.clone());
    kv("default_filament_colour", colour_arr);
    kv("filament_colour_type", rep("1"));
    kv("filament_type", rep("PLA"));
    kv("filament_settings_id", rep("Generic PLA"));
    kv("filament_ids", rep("GFA00"));
    kv("filament_vendor", rep("Generic"));
    kv("filament_is_support", rep("0"));
    kv("filament_soluble", rep("0"));
    kv("filament_diameter", rep("1.75"));
    kv("filament_density", rep("1.24"));
    kv("filament_map", rep("1"));
    kv("filament_max_volumetric_speed", rep("12"));
    kv("filament_flow_ratio", rep("0.98"));
    kv("nozzle_temperature", rep("220"));
    kv("nozzle_temperature_initial_layer", rep("220"));
    kv("hot_plate_temp", rep("60"));
    kv("hot_plate_temp_initial_layer", rep("60"));
    let body: Vec<String> = e.iter().map(|(k, v)| format!("    \"{}\": {}", k, v)).collect();
    format!("{{\n{}\n}}\n", body.join(",\n"))
}

/// Build a standalone colored 3MF: the original part painted per-triangle into
/// N filament bands (Bambu/Orca `paint_color`), with N filaments defined in
/// `project_settings.config` so it opens showing the active contour colors.
/// `positions` is the original undeformed surface as a triangle soup (9
/// floats/tri); `tri_band[i]` is the 0-based band of triangle i (0 = lowest
/// value); `colors[b]` is the `#RRGGBB` color of band b. Pure visualization —
/// no infill modifiers.
pub fn export_color_3mf(
    part_name: &str,
    positions: &[f32],
    tri_band: &[u32],
    colors: &[String],
    thumbnail: Option<&[u8]>,
) -> Vec<u8> {
    let (part, bands) = weld_with_bands(positions, tri_band);

    // Plate placement: center x/y on a 256 bed, drop z to the plate.
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for v in &part.vertices {
        for d in 0..3 {
            lo[d] = lo[d].min(v[d]);
            hi[d] = hi[d].max(v[d]);
        }
    }
    let tx = 128.0 - (lo[0] + hi[0]) / 2.0;
    let ty = 128.0 - (lo[1] + hi[1]) / 2.0;
    let tz = -lo[2];
    let place = format!("1 0 0 0 1 0 0 0 1 {tx} {ty} {tz}");
    let uuid = |n: usize| format!("{:08x}-89ab-cdef-0123-456789abcdef", n + 1);

    // ---- 3D/Objects/object_1.model: the painted part ----
    let mut obj = String::new();
    obj.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    obj.push_str("<model unit=\"millimeter\" xml:lang=\"en-US\" xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\" xmlns:BambuStudio=\"http://schemas.bambulab.com/package/2021\" xmlns:p=\"http://schemas.microsoft.com/3dmanufacturing/production/2015/06\" requiredextensions=\"p\">\n");
    obj.push_str(" <metadata name=\"BambuStudio:3mfVersion\">1</metadata>\n <resources>\n");
    obj.push_str(&format!("  <object id=\"1\" p:UUID=\"{}\" type=\"model\">\n", uuid(1)));
    obj.push_str(&mesh_xml_painted(&part, &bands));
    obj.push_str("  </object>\n");
    obj.push_str(" </resources>\n <build/>\n</model>\n");

    // ---- 3D/3dmodel.model: assembly referencing the object ----
    let assembly_id = 2usize;
    let mut root = String::new();
    root.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    root.push_str("<model unit=\"millimeter\" xml:lang=\"en-US\" xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\" xmlns:BambuStudio=\"http://schemas.bambulab.com/package/2021\" xmlns:p=\"http://schemas.microsoft.com/3dmanufacturing/production/2015/06\" requiredextensions=\"p\">\n");
    root.push_str(" <metadata name=\"Application\">BambuStudio-02.07.00.00</metadata>\n");
    root.push_str(" <metadata name=\"BambuStudio:3mfVersion\">1</metadata>\n");
    root.push_str(&format!(" <metadata name=\"Title\">{}</metadata>\n", xml_escape(part_name)));
    root.push_str(" <resources>\n");
    root.push_str(&format!("  <object id=\"{assembly_id}\" p:UUID=\"{}\" type=\"model\">\n   <components>\n", uuid(assembly_id)));
    root.push_str(&format!(
        "    <component p:path=\"/3D/Objects/object_1.model\" objectid=\"1\" p:UUID=\"{}\" transform=\"1 0 0 0 1 0 0 0 1 0 0 0\"/>\n",
        uuid(101)
    ));
    root.push_str("   </components>\n  </object>\n </resources>\n");
    root.push_str(&format!(
        " <build p:UUID=\"{}\">\n  <item objectid=\"{assembly_id}\" p:UUID=\"{}\" transform=\"{place}\" printable=\"1\"/>\n </build>\n</model>\n",
        uuid(200),
        uuid(201)
    ));

    // ---- Metadata/model_settings.config: single part, base extruder 1 ----
    let mut cfg = String::new();
    cfg.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<config>\n");
    cfg.push_str(&format!("  <object id=\"{assembly_id}\">\n"));
    cfg.push_str(&format!("    <metadata key=\"name\" value=\"{}\"/>\n", xml_escape(part_name)));
    cfg.push_str("    <metadata key=\"extruder\" value=\"1\"/>\n");
    cfg.push_str("    <part id=\"1\" subtype=\"normal_part\">\n");
    cfg.push_str(&format!("      <metadata key=\"name\" value=\"{}\"/>\n", xml_escape(part_name)));
    cfg.push_str("      <metadata key=\"matrix\" value=\"1 0 0 0 0 1 0 0 0 0 1 0 0 0 0 1\"/>\n");
    cfg.push_str("      <mesh_stat edges_fixed=\"0\" degenerate_facets=\"0\" facets_removed=\"0\" facets_reversed=\"0\" backwards_edges=\"0\"/>\n");
    cfg.push_str("    </part>\n");
    cfg.push_str("  </object>\n  <plate>\n");
    cfg.push_str("    <metadata key=\"plater_id\" value=\"1\"/>\n");
    cfg.push_str("    <metadata key=\"plater_name\" value=\"\"/>\n");
    cfg.push_str("    <metadata key=\"locked\" value=\"false\"/>\n");
    cfg.push_str("    <metadata key=\"thumbnail_file\" value=\"Metadata/plate_1.png\"/>\n");
    cfg.push_str("    <model_instance>\n");
    cfg.push_str(&format!("      <metadata key=\"object_id\" value=\"{assembly_id}\"/>\n"));
    cfg.push_str("      <metadata key=\"instance_id\" value=\"0\"/>\n");
    cfg.push_str("      <metadata key=\"identify_id\" value=\"463\"/>\n");
    cfg.push_str("    </model_instance>\n  </plate>\n");
    cfg.push_str(&format!(
        "  <assemble>\n   <assemble_item object_id=\"{assembly_id}\" instance_id=\"0\" transform=\"{place}\" offset=\"0 0 0\" />\n  </assemble>\n"
    ));
    cfg.push_str("</config>\n");

    // ---- container plumbing (mirrors export_orca_3mf for is_bbl_3mf) ----
    let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\n <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\n <Default Extension=\"model\" ContentType=\"application/vnd.ms-package.3dmanufacturing-3dmodel+xml\"/>\n <Default Extension=\"png\" ContentType=\"image/png\"/>\n</Types>\n";
    let rels = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n <Relationship Target=\"/3D/3dmodel.model\" Id=\"rel-1\" Type=\"http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel\"/>\n <Relationship Target=\"/Metadata/plate_1.png\" Id=\"rel-2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/thumbnail\"/>\n <Relationship Target=\"/Metadata/plate_1.png\" Id=\"rel-4\" Type=\"http://schemas.bambulab.com/package/2021/cover-thumbnail-middle\"/>\n <Relationship Target=\"/Metadata/plate_1_small.png\" Id=\"rel-5\" Type=\"http://schemas.bambulab.com/package/2021/cover-thumbnail-small\"/>\n</Relationships>\n";
    let model_rels = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n <Relationship Target=\"/3D/Objects/object_1.model\" Id=\"rel-1\" Type=\"http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel\"/>\n</Relationships>\n";

    let thumb = thumbnail.filter(|t| !t.is_empty()).unwrap_or(THUMB_PNG);
    let mut zip = ZipWriter::new();
    zip.add("[Content_Types].xml", content_types.as_bytes());
    zip.add("_rels/.rels", rels.as_bytes());
    zip.add("3D/3dmodel.model", root.as_bytes());
    zip.add("3D/_rels/3dmodel.model.rels", model_rels.as_bytes());
    // The painted model XML is the bulk (hundreds of thousands of triangles after
    // iso-cut + refinement) and is highly repetitive — deflate it (and the other
    // text parts) ~10×. PNG thumbnails are already compressed, so store them.
    zip.add_deflated("3D/Objects/object_1.model", obj.as_bytes());
    zip.add_deflated("Metadata/model_settings.config", cfg.as_bytes());
    zip.add_deflated("Metadata/project_settings.config", color_project_settings(colors).as_bytes());
    zip.add("Metadata/slice_info.config", SLICE_INFO.as_bytes());
    zip.add("Metadata/plate_1.png", thumb);
    zip.add("Metadata/plate_1_small.png", thumb);
    zip.finish()
}

/// Build the PrusaSlicer project 3MF (reverse-engineered from a reference
/// export, `testhook_prusaslicer.3mf`): ONE object whose mesh concatenates
/// the part and all modifier meshes; volumes are triangle ranges declared in
/// Metadata/Slic3r_PE_model.config (`ModelPart` / `ParameterModifier`).
/// Object-level config carries `fill_density` (base) and `perimeters`;
/// modifiers override `fill_density` (+ `fill_pattern` in binary mode —
/// "rectilinear"/"concentric" are valid PrusaSlicer values). Geometry is
/// centered on the bbox like PrusaSlicer's own exports, with the build item
/// placing it at bed center, bottom on the plate. No print profile is
/// embedded: the user's printer/filament/print presets stay active.
pub fn export_prusa_3mf(
    part_name: &str,
    part: &IndexedMesh,
    regions: &[RegionMesh],
    base_density: f64,
    perimeters: u32,
    top_bottom_layers: u32,
    solid_pattern: Option<&str>,
) -> Vec<u8> {
    // ---- concatenate part + regions into one mesh, tracking tri ranges ----
    let mut vertices: Vec<[f32; 3]> = part.vertices.clone();
    let mut triangles: Vec<[u32; 3]> = part.triangles.clone();
    // (first_tri, last_tri) inclusive, per volume; part is volume 0.
    let mut ranges: Vec<(usize, usize)> = vec![(0, triangles.len().saturating_sub(1))];
    for r in regions {
        let m = region_to_indexed(r);
        let v0 = vertices.len() as u32;
        let t0 = triangles.len();
        vertices.extend_from_slice(&m.vertices);
        triangles.extend(m.triangles.iter().map(|t| [t[0] + v0, t[1] + v0, t[2] + v0]));
        ranges.push((t0, triangles.len().saturating_sub(1)));
    }

    // ---- center on the combined bbox (PrusaSlicer convention); the build
    // item then drops it at bed center with the bottom on the plate ----
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for v in &vertices {
        for d in 0..3 {
            lo[d] = lo[d].min(v[d]);
            hi[d] = hi[d].max(v[d]);
        }
    }
    let c = [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0];
    for v in vertices.iter_mut() {
        for d in 0..3 {
            v[d] -= c[d];
        }
    }
    let tz = (hi[2] - lo[2]) / 2.0;
    let mesh = IndexedMesh { vertices, triangles };

    // ---- 3D/3dmodel.model ----
    let mut model = String::new();
    model.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    model.push_str("<model unit=\"millimeter\" xml:lang=\"en-US\" xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\" xmlns:slic3rpe=\"http://schemas.slic3r.org/3mf/2017/06\">\n");
    model.push_str(" <metadata name=\"slic3rpe:Version3mf\">1</metadata>\n");
    model.push_str(&format!(" <metadata name=\"Title\">{}</metadata>\n", xml_escape(part_name)));
    model.push_str(" <metadata name=\"Application\">filaSim-0.1.0</metadata>\n");
    model.push_str(" <resources>\n  <object id=\"1\" type=\"model\">\n");
    model.push_str(&mesh_xml(&mesh));
    model.push_str("  </object>\n </resources>\n <build>\n");
    model.push_str(&format!(
        "  <item objectid=\"1\" transform=\"1 0 0 0 1 0 0 0 1 125 105 {tz}\" printable=\"1\"/>\n"
    ));
    model.push_str(" </build>\n</model>\n");

    // ---- Metadata/Slic3r_PE_model.config ----
    let mut cfg = String::new();
    cfg.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<config>\n");
    cfg.push_str(" <object id=\"1\" instances_count=\"1\">\n");
    cfg.push_str(&format!(
        "  <metadata type=\"object\" key=\"name\" value=\"{}\"/>\n",
        xml_escape(part_name)
    ));
    cfg.push_str(&format!(
        "  <metadata type=\"object\" key=\"fill_density\" value=\"{}%\"/>\n",
        (base_density * 100.0).round() as u32
    ));
    // Binary mode: set the general object-level fill pattern too.
    if let Some(p) = solid_pattern {
        cfg.push_str(&format!(
            "  <metadata type=\"object\" key=\"fill_pattern\" value=\"{}\"/>\n",
            xml_escape(p)
        ));
    }
    cfg.push_str(&format!(
        "  <metadata type=\"object\" key=\"perimeters\" value=\"{perimeters}\"/>\n"
    ));
    // Top/bottom shells the analysis assumed (0 = open infill showpieces).
    cfg.push_str(&format!(
        "  <metadata type=\"object\" key=\"top_solid_layers\" value=\"{top_bottom_layers}\"/>\n"
    ));
    cfg.push_str(&format!(
        "  <metadata type=\"object\" key=\"bottom_solid_layers\" value=\"{top_bottom_layers}\"/>\n"
    ));
    for (k, (first, last)) in ranges.iter().enumerate() {
        cfg.push_str(&format!("  <volume firstid=\"{first}\" lastid=\"{last}\">\n"));
        if k == 0 {
            cfg.push_str(&format!(
                "   <metadata type=\"volume\" key=\"name\" value=\"{}\"/>\n",
                xml_escape(part_name)
            ));
            cfg.push_str("   <metadata type=\"volume\" key=\"volume_type\" value=\"ModelPart\"/>\n");
        } else {
            let pct = (regions[k - 1].density * 100.0).round() as u32;
            cfg.push_str(&format!(
                "   <metadata type=\"volume\" key=\"name\" value=\"infill {pct}%\"/>\n"
            ));
            cfg.push_str("   <metadata type=\"volume\" key=\"modifier\" value=\"1\"/>\n");
            cfg.push_str(
                "   <metadata type=\"volume\" key=\"volume_type\" value=\"ParameterModifier\"/>\n",
            );
            cfg.push_str(&format!(
                "   <metadata type=\"volume\" key=\"fill_density\" value=\"{pct}%\"/>\n"
            ));
            if let Some(p) = solid_pattern {
                cfg.push_str(&format!(
                    "   <metadata type=\"volume\" key=\"fill_pattern\" value=\"{}\"/>\n",
                    xml_escape(p)
                ));
            }
        }
        cfg.push_str(
            "   <metadata type=\"volume\" key=\"matrix\" value=\"1 0 0 0 0 1 0 0 0 0 1 0 0 0 0 1\"/>\n",
        );
        cfg.push_str("   <mesh edges_fixed=\"0\" degenerate_facets=\"0\" facets_removed=\"0\" facets_reversed=\"0\" backwards_edges=\"0\"/>\n");
        cfg.push_str("  </volume>\n");
    }
    cfg.push_str(" </object>\n</config>\n");

    // ---- container plumbing ----
    let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\n <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\n <Default Extension=\"model\" ContentType=\"application/vnd.ms-package.3dmanufacturing-3dmodel+xml\"/>\n</Types>\n";
    let rels = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n <Relationship Target=\"/3D/3dmodel.model\" Id=\"rel-1\" Type=\"http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel\"/>\n</Relationships>\n";

    let mut zip = ZipWriter::new();
    zip.add("[Content_Types].xml", content_types.as_bytes());
    zip.add("_rels/.rels", rels.as_bytes());
    zip.add("3D/3dmodel.model", model.as_bytes());
    zip.add("Metadata/Slic3r_PE_model.config", cfg.as_bytes());
    zip.finish()
}

/// Binary STL bytes from an indexed mesh.
pub fn indexed_to_stl(m: &IndexedMesh) -> Vec<u8> {
    let mut out = vec![0u8; 80];
    out.extend_from_slice(&(m.triangles.len() as u32).to_le_bytes());
    for t in &m.triangles {
        let a = m.vertices[t[0] as usize];
        let b = m.vertices[t[1] as usize];
        let c = m.vertices[t[2] as usize];
        let e1 = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let e2 = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let mut n = [
            e1[1] * e2[2] - e1[2] * e2[1],
            e1[2] * e2[0] - e1[0] * e2[2],
            e1[0] * e2[1] - e1[1] * e2[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len > 0.0 {
            n = [n[0] / len, n[1] / len, n[2] / len];
        }
        for v in n {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for p in [a, b, c] {
            for v in p {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out
}

/// Zip of one binary STL per modifier region (universal slicer fallback).
pub fn export_stl_zip(regions: &[RegionMesh]) -> Vec<u8> {
    let mut zip = ZipWriter::new();
    for r in regions {
        let pct = (r.density * 100.0).round() as u32;
        let stl = indexed_to_stl(&region_to_indexed(r));
        zip.add(&format!("modifier_{pct}pct.stl"), &stl);
    }
    zip.finish()
}

#[derive(Debug)]
pub enum ThreemfError {
    Zip(ZipError),
    NoModel,
    Xml(String),
    NoMesh,
}

impl std::fmt::Display for ThreemfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThreemfError::Zip(e) => write!(f, "{e}"),
            ThreemfError::NoModel => write!(f, "no 3D model file inside the 3MF archive"),
            ThreemfError::Xml(s) => write!(f, "3MF model parse error: {s}"),
            ThreemfError::NoMesh => write!(f, "3MF contains no triangle meshes"),
        }
    }
}

impl std::error::Error for ThreemfError {}

/// Import a 3MF: parse every .model entry, collect all mesh objects, return
/// the largest one (v1 analyzes a single body) plus the total mesh count.
pub fn import_3mf(bytes: &[u8]) -> Result<(TriMesh, usize), ThreemfError> {
    let entries = read_zip(bytes).map_err(ThreemfError::Zip)?;
    let mut meshes: Vec<Vec<[f32; 9]>> = Vec::new();
    let mut found_model = false;
    for (name, data) in &entries {
        if !name.to_lowercase().ends_with(".model") {
            continue;
        }
        found_model = true;
        parse_model_xml(data, &mut meshes).map_err(|e| ThreemfError::Xml(e))?;
    }
    if !found_model {
        return Err(ThreemfError::NoModel);
    }
    let count = meshes.len();
    // Pick the main body by bounding-box volume, not triangle count — a small
    // finely-tessellated modifier mesh must not beat a coarse big part.
    let best = meshes
        .into_iter()
        .max_by(|a, b| {
            bbox_volume(a).partial_cmp(&bbox_volume(b)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .ok_or(ThreemfError::NoMesh)?;
    if best.is_empty() {
        return Err(ThreemfError::NoMesh);
    }
    Ok((TriMesh::from_triangles(best), count))
}

fn bbox_volume(tris: &[[f32; 9]]) -> f64 {
    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for t in tris {
        for v in 0..3 {
            for d in 0..3 {
                let x = t[3 * v + d] as f64;
                lo[d] = lo[d].min(x);
                hi[d] = hi[d].max(x);
            }
        }
    }
    (hi[0] - lo[0]).max(0.0) * (hi[1] - lo[1]).max(0.0) * (hi[2] - lo[2]).max(0.0)
}

fn parse_model_xml(data: &[u8], meshes: &mut Vec<Vec<[f32; 9]>>) -> Result<(), String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut tris: Vec<[f32; 9]> = Vec::new();
    let mut in_mesh = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                if name == b"mesh" {
                    in_mesh = true;
                    verts.clear();
                    tris = Vec::new();
                } else if in_mesh && name == b"vertex" {
                    let mut v = [0f32; 3];
                    for attr in e.attributes().flatten() {
                        let val = String::from_utf8_lossy(&attr.value);
                        let val: f32 = val.trim().parse().unwrap_or(0.0);
                        match attr.key.local_name().as_ref() {
                            b"x" => v[0] = val,
                            b"y" => v[1] = val,
                            b"z" => v[2] = val,
                            _ => {}
                        }
                    }
                    verts.push(v);
                } else if in_mesh && name == b"triangle" {
                    let mut t = [0usize; 3];
                    for attr in e.attributes().flatten() {
                        let val = String::from_utf8_lossy(&attr.value);
                        let val: usize = val.trim().parse().unwrap_or(usize::MAX);
                        match attr.key.local_name().as_ref() {
                            b"v1" => t[0] = val,
                            b"v2" => t[1] = val,
                            b"v3" => t[2] = val,
                            _ => {}
                        }
                    }
                    if t.iter().all(|&i| i < verts.len()) {
                        let (a, b, c) = (verts[t[0]], verts[t[1]], verts[t[2]]);
                        tris.push([a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]]);
                    }
                }
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"mesh" && in_mesh {
                    in_mesh = false;
                    if !tris.is_empty() {
                        meshes.push(std::mem::take(&mut tris));
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

#[cfg(test)]
mod color_tests {
    use super::*;

    #[test]
    fn paint_color_roundtrip() {
        // Ground truth decoded from hook5_painted.3mf whole-triangle codes:
        // filament state s -> code. (state == 1-based filament index; state 0,
        // the base, is left unpainted by the exporter.)
        assert_eq!(paint_color_code(1), "4");
        assert_eq!(paint_color_code(2), "8");
        assert_eq!(paint_color_code(3), "0C");
        assert_eq!(paint_color_code(4), "1C");
        assert_eq!(paint_color_code(5), "2C");
        assert_eq!(paint_color_code(6), "3C");
        // Higher filament indices stay valid (extended continuation groups).
        assert_eq!(paint_color_code(16), paint_color_code(16)); // deterministic
        assert!(!paint_color_code(16).is_empty());
    }

    fn tri_area(p: &[f32]) -> f64 {
        let e1 = [(p[3] - p[0]) as f64, (p[4] - p[1]) as f64, (p[5] - p[2]) as f64];
        let e2 = [(p[6] - p[0]) as f64, (p[7] - p[1]) as f64, (p[8] - p[2]) as f64];
        let cx = e1[1] * e2[2] - e1[2] * e2[1];
        let cy = e1[2] * e2[0] - e1[0] * e2[2];
        let cz = e1[0] * e2[1] - e1[1] * e2[0];
        0.5 * (cx * cx + cy * cy + cz * cz).sqrt()
    }

    #[test]
    fn isoband_cut_conserves_area_and_covers_bands() {
        // One triangle whose scalar spans the full range across 3 bands.
        let pos = vec![0.0, 0.0, 0.0, 6.0, 0.0, 0.0, 0.0, 6.0, 0.0];
        let scalars = vec![0.0, 3.0, 6.0];
        let (out, bands) = isoband_cut(&pos, &scalars, 0.0, 6.0, 3);
        let area_in = tri_area(&pos);
        let area_out: f64 = out.chunks_exact(9).map(tri_area).sum();
        // Cutting redistributes area, never adds or drops it -> watertight.
        assert!((area_in - area_out).abs() / area_in < 1e-4, "area conserved");
        let mut present: Vec<u32> = bands.clone();
        present.sort();
        present.dedup();
        assert_eq!(present, vec![0, 1, 2], "all three bands represented");
    }

    #[test]
    fn subdivide_to_edge_is_conforming_bounded_and_wound() {
        // Closed, consistently-wound tetrahedron.
        let mesh = IndexedMesh {
            vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            triangles: vec![[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]],
        };
        let target = 0.2f32;
        let out = subdivide_to_edge(&mesh, target, 1_000_000);
        use std::collections::HashMap;
        let mut undirected: HashMap<(u32, u32), u32> = HashMap::new();
        let mut directed: HashMap<(u32, u32), u32> = HashMap::new();
        let mut maxe = 0.0f32;
        for tri in &out.triangles {
            for &(a, b) in &[(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
                *undirected.entry(if a < b { (a, b) } else { (b, a) }).or_insert(0) += 1;
                *directed.entry((a, b)).or_insert(0) += 1;
                let (p, q) = (out.vertices[a as usize], out.vertices[b as usize]);
                maxe = maxe.max(
                    ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt(),
                );
            }
        }
        // Watertight: every welded edge shared by exactly 2 triangles.
        assert!(undirected.values().all(|&c| c == 2), "conforming/watertight");
        // Consistent outward winding: each directed edge used exactly once.
        assert!(directed.values().all(|&c| c == 1), "consistent winding");
        // Refined to the requested edge length.
        assert!(maxe <= target * 1.001, "edges within target: {maxe} <= {target}");
        assert!(out.triangles.len() > 100, "actually refined ({} tris)", out.triangles.len());
    }

    #[test]
    fn isoband_cut_indexed_is_exactly_watertight() {
        // Closed tetrahedron, per-vertex scalars spanning several bands.
        let verts = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let tris = vec![[0u32, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]];
        let scalars = vec![0.3f32, 2.5, 5.0, 7.5];
        let (pos, band) = isoband_cut_indexed(&verts, &tris, &scalars, 0.0, 8.0, 5);
        // Area is conserved (the cut only partitions each face).
        let area_in: f64 = tris
            .iter()
            .map(|t| {
                let f = |i: u32| verts[i as usize];
                let (a, b, c) = (f(t[0]), f(t[1]), f(t[2]));
                let flat = [a[0], a[1], a[2], b[0], b[1], b[2], c[0], c[1], c[2]];
                tri_area(&flat)
            })
            .sum();
        let area_out: f64 = pos.chunks_exact(9).map(tri_area).sum();
        assert!((area_in - area_out).abs() / area_in < 1e-4, "area conserved");
        assert!(band.iter().collect::<std::collections::HashSet<_>>().len() >= 3, "multi-band");
        // EXACT watertightness: weld by bit-identical f32 coords (no tolerance) —
        // shared edge crossings are computed once, so this must close perfectly.
        use std::collections::HashMap;
        let mut ids: HashMap<[u32; 3], u32> = HashMap::new();
        let mut wid = |p: [f32; 3], ids: &mut HashMap<[u32; 3], u32>| {
            let k = [p[0].to_bits(), p[1].to_bits(), p[2].to_bits()];
            let n = ids.len() as u32;
            *ids.entry(k).or_insert(n)
        };
        let mut edges: HashMap<(u32, u32), i32> = HashMap::new();
        for t in pos.chunks_exact(9) {
            let a = wid([t[0], t[1], t[2]], &mut ids);
            let b = wid([t[3], t[4], t[5]], &mut ids);
            let c = wid([t[6], t[7], t[8]], &mut ids);
            for (u, v) in [(a, b), (b, c), (c, a)] {
                *edges.entry(if u < v { (u, v) } else { (v, u) }).or_insert(0) += 1;
            }
        }
        let boundary = edges.values().filter(|&&c| c == 1).count();
        assert_eq!(boundary, 0, "exactly watertight via bit-identical shared crossings");
    }

    #[test]
    fn band_index_exact_at_boundaries() {
        // 4 bands over [0,8] → interior boundaries at 2,4,6.
        assert_eq!(band_index(0.0, 8.0, 4, -1.0), 0);
        assert_eq!(band_index(0.0, 8.0, 4, 1.9), 0);
        assert_eq!(band_index(0.0, 8.0, 4, 2.0), 1); // exactly on a boundary → upper band
        assert_eq!(band_index(0.0, 8.0, 4, 5.9), 2);
        assert_eq!(band_index(0.0, 8.0, 4, 6.0), 3);
        assert_eq!(band_index(0.0, 8.0, 4, 100.0), 3); // above hi → top band
        assert_eq!(band_index(5.0, 5.0, 4, 5.0), 0); // degenerate range
    }

    #[test]
    fn isoband_cut_passes_single_band_unsplit() {
        let pos = vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        // All scalars in band 1 of [0,10] / 5 steps (step=2 -> band1=[2,4)).
        let scalars = vec![2.5, 3.0, 3.5];
        let (out, bands) = isoband_cut(&pos, &scalars, 0.0, 10.0, 5);
        assert_eq!(bands, vec![1], "unsplit single triangle");
        assert_eq!(out.len(), 9);
    }

    #[test]
    fn export_color_3mf_is_valid_zip_with_filaments() {
        // Two triangles, two bands -> two filaments, one painted (band 1).
        let positions: Vec<f32> = vec![
            0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, // tri 0 -> band 0 (base)
            0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0, 1.0, // tri 1 -> band 1
        ];
        let tri_band = vec![0u32, 1u32];
        let colors = vec!["#0000FF".to_string(), "#FF0000".to_string()];
        let bytes = export_color_3mf("part", &positions, &tri_band, &colors, None);
        // Valid zip (local file header magic).
        assert_eq!(&bytes[0..4], &[0x50, 0x4b, 0x03, 0x04]);
        // Entries are deflate-compressed now — read them back through read_zip.
        let entries = crate::zip::read_zip(&bytes).expect("valid zip");
        let get = |name: &str| {
            String::from_utf8(entries.iter().find(|(n, _)| n == name).unwrap().1.clone()).unwrap()
        };
        let obj = get("3D/Objects/object_1.model");
        let proj = get("Metadata/project_settings.config");
        assert!(obj.contains("paint_color="), "painted triangles present");
        assert!(proj.contains("#0000FF") && proj.contains("#FF0000"), "filament colors present");
    }
}
