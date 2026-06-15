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
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
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
    // OrcaSlicer exports also write "BambuStudio-…"; ours is InFEAll under the
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
    model.push_str(" <metadata name=\"Application\">InFEAll-0.1.0</metadata>\n");
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
