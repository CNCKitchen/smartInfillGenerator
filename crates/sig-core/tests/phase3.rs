//! Phase-3/4 tests: SIMP optimization quality, bin clustering, watertight
//! region extraction, 3MF export/import roundtrips (incl. the reference
//! Cube.3mf sample from OrcaSlicer/Bambu Studio).

use sig_core::attach::{assemble, BcKind, BcSpec};
use sig_core::bins::{
    assign_bins, cleanup_small_regions, cluster_densities, extract_region, taubin_smooth,
    RegionMesh,
};
use sig_core::mesh::primitives;
use sig_core::simp::{classify_cells, evaluate, optimize, OptimizeParams};
use sig_core::solve::SolveSettings;
use sig_core::threemf::{export_orca_3mf, export_stl_zip, import_3mf, weld, IndexedMesh};
use sig_core::zip::{read_zip, ZipWriter};
use sig_core::{pad_for_levels, VoxelGrid};
use std::collections::HashMap;

fn face_tris(face: usize) -> Vec<u32> {
    vec![2 * face as u32, 2 * face as u32 + 1]
}

/// Cantilever fixture: optimize, bin, and return everything needed to assert.
struct OptFixture {
    grid: VoxelGrid,
    levels: usize,
    problem: sig_core::NodeProblem,
    settings: SolveSettings,
    skin: Vec<u32>,
    design: Vec<u32>,
    x: Vec<f64>,
    u: Vec<f64>,
}

fn run_cantilever_optimization() -> OptFixture {
    let beam = primitives::boxx([0.0; 3], [60.0, 10.0, 10.0]);
    let grid0 = VoxelGrid::voxelize(&beam, 1.0);
    let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
    let bcs = vec![
        BcSpec { kind: BcKind::Fixed, tris: face_tris(0) },
        BcSpec { kind: BcKind::Force([0.0, 0.0, -30.0]), tris: face_tris(1) },
    ];
    let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
    // Budget must leave the interior headroom: the 1 mm solid skin alone is
    // ~38% of this beam's mass, so 0.6 puts the interior mean near 0.35.
    let params = OptimizeParams {
        budget: 0.6,
        exponent: 1.5,
        wall_mm: 1.0,
        max_iter: 30,
        ..Default::default()
    };
    let result = optimize(&grid, levels, &asm.problem, &settings, &params, |_p, _x, _c| {})
        .expect("optimize");
    OptFixture {
        grid,
        levels,
        problem: asm.problem,
        settings,
        skin: result.skin_cells,
        design: result.design_cells,
        x: result.x,
        u: result.u,
    }
}

#[test]
fn optimized_bins_beat_uniform_infill_at_equal_mass() {
    let f = run_cantilever_optimization();

    // Bin the optimized field.
    let centers = cluster_densities(&f.x, 3);
    assert!(centers.len() >= 2, "expected at least 2 distinct bins, got {centers:?}");
    let mut bins = assign_bins(&f.x, &centers);
    cleanup_small_regions(&f.grid, &f.design, &mut bins, centers.len(), 30);
    let x_binned: Vec<f64> = bins.iter().map(|&b| centers[b as usize]).collect();

    // Uniform field at the SAME interior mass.
    let mean = x_binned.iter().sum::<f64>() / x_binned.len() as f64;
    let x_uniform = vec![mean; x_binned.len()];

    let (c_binned, maxd_binned, _) = evaluate(
        &f.grid, f.levels, &f.problem, &f.settings, &f.skin, &f.design, &x_binned, 1.5, 1.0,
        Some(&f.u),
    )
    .expect("binned eval");
    let (c_uniform, _, _) = evaluate(
        &f.grid, f.levels, &f.problem, &f.settings, &f.skin, &f.design, &x_uniform, 1.5, 1.0,
        Some(&f.u),
    )
    .expect("uniform eval");

    let gain = c_uniform / c_binned;
    assert!(
        gain > 1.03,
        "optimized layout should beat uniform at equal mass: C_uni/C_opt = {gain:.3} (binned {c_binned:.4}, uniform {c_uniform:.4})"
    );
    assert!(maxd_binned > 0.0 && maxd_binned < 50.0, "sane deflection {maxd_binned}");

    // Sanity on the density distribution: spread, not constant.
    let lo = f.x.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = f.x.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    assert!(hi - lo > 0.2, "optimization should differentiate densities (lo {lo:.2} hi {hi:.2})");
}

#[test]
fn extracted_regions_are_watertight_and_oriented() {
    let f = run_cantilever_optimization();
    let centers = cluster_densities(&f.x, 3);
    let mut bins = assign_bins(&f.x, &centers);
    cleanup_small_regions(&f.grid, &f.design, &mut bins, centers.len(), 30);

    let mut bin_of_cell: HashMap<u32, u8> = HashMap::new();
    for (i, &c) in f.design.iter().enumerate() {
        bin_of_cell.insert(c, bins[i]);
    }
    for level in 1..centers.len() {
        let inside = |ci: usize| -> bool {
            bin_of_cell.get(&(ci as u32)).map_or(false, |&b| b as usize >= level)
        };
        let mut region = extract_region(&f.grid, &inside, 0.4);
        if region.indices.is_empty() {
            continue; // a level can be empty after cleanup; fine
        }
        // Watertight: every edge shared by exactly two triangles.
        let mut edge_count: HashMap<(u32, u32), u32> = HashMap::new();
        for t in region.indices.chunks(3) {
            for e in 0..3 {
                let (a, b) = (t[e], t[(e + 1) % 3]);
                let key = (a.min(b), a.max(b));
                *edge_count.entry(key).or_insert(0) += 1;
            }
        }
        for (&e, &c) in &edge_count {
            assert!(c == 2, "edge {e:?} shared by {c} triangles (level {level})");
        }
        // Signed volume positive => outward orientation.
        let vol_before = signed_volume(&region.positions, &region.indices);
        assert!(vol_before > 0.0, "region volume should be positive, got {vol_before}");

        // Smoothing keeps topology and roughly the volume.
        taubin_smooth(&mut region.positions, &region.indices, 10);
        let vol_after = signed_volume(&region.positions, &region.indices);
        assert!(
            (vol_after / vol_before) > 0.7 && (vol_after / vol_before) < 1.3,
            "taubin volume drift: {vol_before} -> {vol_after}"
        );
        assert!(region.positions.iter().all(|v| v.is_finite()));
    }
}

fn signed_volume(positions: &[f32], indices: &[u32]) -> f64 {
    let mut vol = 0f64;
    for t in indices.chunks(3) {
        let p = |i: u32| {
            let i = i as usize;
            [positions[3 * i] as f64, positions[3 * i + 1] as f64, positions[3 * i + 2] as f64]
        };
        let (a, b, c) = (p(t[0]), p(t[1]), p(t[2]));
        vol += (a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
            + a[2] * (b[0] * c[1] - b[1] * c[0]))
            / 6.0;
    }
    vol
}

#[test]
fn clustering_recovers_separated_levels() {
    let mut values = Vec::new();
    values.extend(std::iter::repeat(0.12).take(100));
    values.extend(std::iter::repeat(0.38).take(80));
    values.extend(std::iter::repeat(0.66).take(50));
    let centers = cluster_densities(&values, 3);
    assert_eq!(centers.len(), 3);
    assert!((centers[0] - 0.12).abs() < 0.02, "{centers:?}");
    assert!((centers[1] - 0.38).abs() < 0.02, "{centers:?}");
    assert!((centers[2] - 0.66).abs() < 0.02, "{centers:?}");
}

#[test]
fn orca_3mf_roundtrips_through_own_zip_and_import() {
    let part_soup = primitives::boxx([0.0; 3], [30.0, 20.0, 10.0]);
    let part = weld(&part_soup);
    assert_eq!(part.vertices.len(), 8);
    assert_eq!(part.triangles.len(), 12);

    // Fake nested modifier regions: two boxes.
    let region = |lo: [f32; 3], hi: [f32; 3], density: f64| -> RegionMesh {
        let m = weld(&primitives::boxx(lo, hi));
        RegionMesh {
            density,
            positions: m.vertices.iter().flat_map(|v| v.iter().copied()).collect(),
            indices: m.triangles.iter().flat_map(|t| t.iter().copied()).collect(),
        }
    };
    let regions = vec![
        region([2.0; 3], [20.0, 15.0, 8.0], 0.25),
        region([3.0; 3], [10.0, 10.0, 7.0], 0.50),
    ];

    let bytes = export_orca_3mf("bracket & arm", &part, &regions, 0.12);

    // Container structure.
    let entries = read_zip(&bytes).expect("read back own zip");
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    for required in [
        "[Content_Types].xml",
        "_rels/.rels",
        "3D/3dmodel.model",
        "3D/_rels/3dmodel.model.rels",
        "3D/Objects/object_1.model",
        "Metadata/model_settings.config",
    ] {
        assert!(names.contains(&required), "missing {required}");
    }
    let cfg = entries
        .iter()
        .find(|(n, _)| n == "Metadata/model_settings.config")
        .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
        .unwrap();
    assert!(cfg.contains("subtype=\"normal_part\""));
    assert_eq!(cfg.matches("subtype=\"modifier_part\"").count(), 2);
    assert!(cfg.contains("sparse_infill_density\" value=\"25%\""));
    assert!(cfg.contains("sparse_infill_density\" value=\"50%\""));
    assert!(cfg.contains("sparse_infill_density\" value=\"12%\""), "base density on the object");
    // Modifiers override ONLY the infill density. No wall_loops anywhere:
    // 0 strips perimeters where a modifier touches the surface, and a pinned
    // count would override the user's process profile (real-Orca finding).
    assert!(!cfg.contains("wall_loops"), "walls must inherit from the part profile");
    assert!(cfg.contains("bracket &amp; arm"));

    // Geometry comes back via the import path (largest bbox = the part).
    let (mesh, count) = import_3mf(&bytes).expect("import own 3mf");
    assert_eq!(count, 3, "part + 2 modifiers");
    assert_eq!(mesh.len(), 12, "part mesh wins by bbox volume");
    let (lo, hi) = mesh.bounds().unwrap();
    assert_eq!((lo, hi), ([0.0, 0.0, 0.0], [30.0, 20.0, 10.0]));

    // STL zip fallback.
    let stl_zip = export_stl_zip(&regions);
    let stl_entries = read_zip(&stl_zip).unwrap();
    assert_eq!(stl_entries.len(), 2);
    assert!(stl_entries.iter().any(|(n, _)| n == "modifier_25pct.stl"));
    let (_, stl_bytes) = &stl_entries[0];
    let parsed = sig_core::TriMesh::from_stl(stl_bytes).unwrap();
    assert_eq!(parsed.len(), 12);
}

#[test]
fn imports_reference_orca_sample() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cube.3mf");
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            eprintln!("Cube.3mf sample not present; skipping");
            return;
        }
    };
    let (mesh, count) = import_3mf(&bytes).expect("import Cube.3mf (deflate entries)");
    assert!(count >= 2, "sample has part + modifier meshes, got {count}");
    let (lo, hi) = mesh.bounds().unwrap();
    let dims = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
    // The part is a 25 mm cube.
    for d in dims {
        assert!((d - 25.0).abs() < 1.0, "expected ~25mm cube, got {dims:?}");
    }
}

/// Diagnostic: print the convergence signals per iteration on the smoke-test
/// fixture. Run with: cargo test -p sig-core --test phase3 conv_trace -- --ignored --nocapture
#[test]
#[ignore]
fn conv_trace() {
    let beam = primitives::boxx([0.0; 3], [60.0, 12.0, 12.0]);
    // ~60k cells like the wasm smoke fixture.
    let h = (60.0f64 * 12.0 * 12.0 / 60_000.0).cbrt();
    let grid0 = VoxelGrid::voxelize(&beam, h);
    let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
    let bcs = vec![
        BcSpec { kind: BcKind::Fixed, tris: face_tris(0) },
        BcSpec { kind: BcKind::Force([0.0, 0.0, -40.0]), tris: face_tris(1) },
    ];
    let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
    let params = OptimizeParams {
        budget: 0.6,
        exponent: 1.5,
        wall_mm: 0.9,
        max_iter: 40,
        ..Default::default()
    };
    let mut prev_c = f64::INFINITY;
    let result = optimize(&grid, levels, &asm.problem, &settings, &params, |p, _x, _c| {
        let c_rel = if prev_c.is_finite() { (p.compliance - prev_c).abs() / p.compliance } else { f64::NAN };
        prev_c = p.compliance;
        println!(
            "it {:>2}  C {:.6e}  c_rel {:.2e}  max_dx {:.4}  mean_dx {:.5}",
            p.iteration, p.compliance, c_rel, p.change, p.mean_change
        );
    })
    .expect("optimize");
    println!("converged = {}, iterations = {}", result.converged, result.iterations);
}

#[test]
fn voxel_surface_mesh_counts() {
    let grid = VoxelGrid::solid_box(2, 2, 2, 1.0);
    let (tris, edges) = grid.surface_mesh();
    // 2x2x2 block: 6 block faces x 4 cell faces, 2 tris each, 9 floats per tri.
    assert_eq!(tris.len(), 24 * 2 * 9);
    // 48 unique surface edge segments, 2 endpoints x 3 floats each.
    assert_eq!(edges.len(), 48 * 6);
}

#[test]
fn zip_writer_reader_roundtrip() {
    let mut w = ZipWriter::new();
    w.add("a/b.txt", b"hello world");
    w.add("c.bin", &[0u8, 1, 2, 255, 254]);
    let bytes = w.finish();
    let entries = read_zip(&bytes).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "a/b.txt");
    assert_eq!(entries[0].1, b"hello world");
    assert_eq!(entries[1].1, vec![0u8, 1, 2, 255, 254]);
}

#[test]
fn classify_cells_skin_vs_interior() {
    let grid0 = VoxelGrid::solid_box(10, 10, 10, 1.0);
    let (grid, _) = pad_for_levels(&grid0, 1);
    let (skin, interior) = classify_cells(&grid, 1.0);
    assert_eq!(skin.len() + interior.len(), 1000);
    // 1-layer skin of a 10^3 box: 10^3 - 8^3 = 488.
    assert_eq!(skin.len(), 488, "one skin layer expected");
    let (skin2, interior2) = classify_cells(&grid, 2.0);
    assert_eq!(skin2.len(), 1000 - 6 * 6 * 6);
    assert_eq!(interior2.len(), 216);
}

#[test]
fn weld_dedups_box() {
    let m = weld(&primitives::boxx([0.0; 3], [5.0, 5.0, 5.0]));
    assert_eq!(m.vertices.len(), 8);
    assert_eq!(m.triangles.len(), 12);
    let _ = IndexedMesh { vertices: m.vertices.clone(), triangles: m.triangles.clone() };
}
