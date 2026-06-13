// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Phase-3/4 tests: SIMP optimization quality, bin clustering, watertight
//! region extraction, 3MF export/import roundtrips (incl. the reference
//! Cube.3mf sample from OrcaSlicer/Bambu Studio).

use sig_core::attach::{assemble, check_problem, BcKind, BcSpec};
use sig_core::bins::{
    assign_bins, assign_bins_mass, cleanup_small_regions, cluster_densities, cluster_levels,
    extract_region, taubin_smooth, RegionMesh,
};
use sig_core::mesh::primitives;
use sig_core::simp::{build_mirror_pairs, classify_cells, evaluate, optimize, OptimizeParams};
use sig_core::solve::SolveSettings;
use sig_core::threemf::{
    export_orca_3mf, export_prusa_3mf, export_stl_zip, import_3mf, weld, IndexedMesh,
};
use sig_core::zip::{read_zip, ZipWriter};
use sig_core::{pad_for_levels, solve_static, BoxRegion, StaticProblem, VoxelGrid};
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
    skin_frac: Vec<f32>,
    x: Vec<f64>,
    u: Vec<f64>,
    se: Vec<f64>,
}

/// The app's binning pipeline: floor-pinned energy-weighted level placement
/// plus mass-constrained assignment (see bins.rs).
fn bin_fixture(f: &OptFixture, n: usize) -> (Vec<f64>, Vec<u8>) {
    let centers = cluster_levels(&f.x, &f.se, n, 1.5, 1.0, 0.10, 0.70);
    let target = f.x.iter().sum::<f64>() / f.x.len() as f64;
    let mut bins = assign_bins_mass(&f.x, &f.se, &centers, 1.5, 1.0, target);
    cleanup_small_regions(&f.grid, &f.design, &mut bins, centers.len(), 30);
    (centers, bins)
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
    // Budget = target mean INFILL density (interior only; skin is extra).
    // 35% leaves room to differentiate in both directions [0.10, 0.70].
    let params = OptimizeParams {
        budget: 0.35,
        exponent: 1.5,
        wall_mm: 1.0,
        max_iter: 30,
        ..Default::default()
    };
    let result = optimize(&grid, levels, &asm.problem, &settings, &params, None, None, |_p, _x, _c| {})
        .expect("optimize");
    OptFixture {
        grid,
        levels,
        problem: asm.problem,
        settings,
        skin: result.skin_cells,
        design: result.design_cells,
        skin_frac: result.skin_frac,
        x: result.x,
        u: result.u,
        se: result.se,
    }
}

#[test]
fn optimized_bins_beat_uniform_infill_at_equal_mass() {
    let f = run_cantilever_optimization();

    // Bin the optimized field with the app's pipeline.
    let (centers, bins) = bin_fixture(&f, 3);
    assert!(centers.len() >= 2, "expected at least 2 distinct bins, got {centers:?}");
    // Level placement follows the convex-law theory: bottom pinned at the
    // printability floor, top driven toward the cap by the energy weighting.
    assert!((centers[0] - 0.10).abs() < 1e-9, "bottom level is the floor: {centers:?}");
    assert!(*centers.last().unwrap() > 0.45, "load level sits high: {centers:?}");
    let x_binned: Vec<f64> = bins.iter().map(|&b| centers[b as usize]).collect();

    // Mass-constrained assignment lands near the optimizer's mean.
    let target = f.x.iter().sum::<f64>() / f.x.len() as f64;
    let mean = x_binned.iter().sum::<f64>() / x_binned.len() as f64;
    assert!(
        (mean - target).abs() < 0.03,
        "binned mean {mean:.3} should track the continuous mean {target:.3}"
    );

    // Uniform field at the SAME interior mass.
    let x_uniform = vec![mean; x_binned.len()];

    let (c_binned, maxd_binned, _) = evaluate(
        &f.grid, f.levels, &f.problem, &f.settings, &f.skin, &f.design, &f.skin_frac, &x_binned,
        1.5, 1.0, Some(&f.u),
    )
    .expect("binned eval");
    let (c_uniform, _, _) = evaluate(
        &f.grid, f.levels, &f.problem, &f.settings, &f.skin, &f.design, &f.skin_frac, &x_uniform,
        1.5, 1.0, Some(&f.u),
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
    let (centers, bins) = bin_fixture(&f, 3);

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

/// Diagnostic: A/B the old binning (mass-error k-means on density, nearest
/// assignment) against the new one (floor-pinned energy-weighted levels,
/// mass-constrained assignment) on the cantilever fixture. Run with:
/// cargo test -p sig-core --test phase3 bin_placement_ab -- --ignored --nocapture
#[test]
#[ignore]
fn bin_placement_ab() {
    let f = run_cantilever_optimization();
    let eval_layout = |x_b: &[f64], label: &str, centers: &[f64]| {
        let (c, _, _) = evaluate(
            &f.grid, f.levels, &f.problem, &f.settings, &f.skin, &f.design, &f.skin_frac, x_b,
            1.5, 1.0, Some(&f.u),
        )
        .unwrap();
        let mean = x_b.iter().sum::<f64>() / x_b.len() as f64;
        let x_u = vec![mean; x_b.len()];
        let (cu, _, _) = evaluate(
            &f.grid, f.levels, &f.problem, &f.settings, &f.skin, &f.design, &f.skin_frac, &x_u,
            1.5, 1.0, Some(&f.u),
        )
        .unwrap();
        println!(
            "{label}: levels {:?} mean infill {:.1}% C {:.5} gain vs uniform {:+.2}%",
            centers.iter().map(|v| (v * 100.0).round()).collect::<Vec<_>>(),
            mean * 100.0,
            c,
            (cu / c - 1.0) * 100.0
        );
    };
    let target = f.x.iter().sum::<f64>() / f.x.len() as f64;
    let xb_of = |centers: &[f64], bins: &[u8]| -> Vec<f64> {
        bins.iter().map(|&b| centers[b as usize]).collect()
    };

    // A: old pipeline (mass-error k-means on rho, nearest-rho assignment).
    let centers_a = cluster_densities(&f.x, 3);
    let mut bins_a = assign_bins(&f.x, &centers_a);
    cleanup_small_regions(&f.grid, &f.design, &mut bins_a, centers_a.len(), 30);
    eval_layout(&xb_of(&centers_a, &bins_a), "A old rho-kmeans + nearest    ", &centers_a);

    // B: energy-weighted E-space levels + anchored mass assignment.
    let centers_b = cluster_levels(&f.x, &f.se, 3, 1.5, 1.0, 0.10, 0.70);
    let mut bins_b = assign_bins_mass(&f.x, &f.se, &centers_b, 1.5, 1.0, target);
    cleanup_small_regions(&f.grid, &f.design, &mut bins_b, centers_b.len(), 30);
    eval_layout(&xb_of(&centers_b, &bins_b), "B se-E levels + anchored mass ", &centers_b);

    // C: volume-weighted E-space levels + anchored mass assignment.
    let ones = vec![1.0; f.x.len()];
    let centers_c = cluster_levels(&f.x, &ones, 3, 1.5, 1.0, 0.10, 0.70);
    let mut bins_c = assign_bins_mass(&f.x, &f.se, &centers_c, 1.5, 1.0, target);
    cleanup_small_regions(&f.grid, &f.design, &mut bins_c, centers_c.len(), 30);
    eval_layout(&xb_of(&centers_c, &bins_c), "C vol-E levels + anchored mass", &centers_c);

    // D: old rho-kmeans levels + anchored mass assignment.
    let mut bins_d = assign_bins_mass(&f.x, &f.se, &centers_a, 1.5, 1.0, target);
    cleanup_small_regions(&f.grid, &f.design, &mut bins_d, centers_a.len(), 30);
    eval_layout(&xb_of(&centers_a, &bins_d), "D old levels + anchored mass  ", &centers_a);
}

#[test]
fn binary_mode_solid_or_floor_beats_uniform() {
    // Binary (hollow/solid) mode: the optimizer runs with SIMP penalization
    // p=3 and bounds [0.05, 1.0] so the field converges toward the extremes;
    // the result is quantized to exactly {floor, solid} with the
    // mass-constrained assignment and evaluated with the PHYSICAL pattern
    // law (n=1.5) — which is exact at both endpoints.
    let beam = primitives::boxx([0.0; 3], [60.0, 10.0, 10.0]);
    let grid0 = VoxelGrid::voxelize(&beam, 1.0);
    let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
    let bcs = vec![
        BcSpec { kind: BcKind::Fixed, tris: face_tris(0) },
        BcSpec { kind: BcKind::Force([0.0, 0.0, -30.0]), tris: face_tris(1) },
    ];
    let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
    let params = OptimizeParams {
        budget: 0.35,
        exponent: 3.0, // penalization — optimizer only
        coeff: 1.0,
        floor: 0.05,
        cap: 1.0,
        wall_mm: 1.0,
        max_iter: 30,
        ..Default::default()
    };
    let result = optimize(&grid, levels, &asm.problem, &settings, &params, None, None, |_p, _x, _c| {})
        .expect("optimize");

    let two = vec![0.05, 1.0];
    let target = result.x.iter().sum::<f64>() / result.x.len() as f64;
    let mut bins = assign_bins_mass(&result.x, &result.se, &two, 1.5, 1.0, target);
    cleanup_small_regions(&grid, &result.design_cells, &mut bins, two.len(), 30);
    let x_binned: Vec<f64> = bins.iter().map(|&b| two[b as usize]).collect();
    assert!(x_binned.iter().all(|&v| v == 0.05 || v == 1.0), "strictly two-level design");
    assert!(x_binned.iter().any(|&v| v == 1.0), "has solid cells");
    let mean = x_binned.iter().sum::<f64>() / x_binned.len() as f64;
    assert!((mean - target).abs() < 0.05, "binned mean {mean:.3} tracks target {target:.3}");

    let (c_binned, _, _) = evaluate(
        &grid, levels, &asm.problem, &settings, &result.skin_cells, &result.design_cells,
        &result.skin_frac, &x_binned, 1.5, 1.0, Some(&result.u),
    )
    .unwrap();
    let x_uniform = vec![mean; x_binned.len()];
    let (c_uniform, _, _) = evaluate(
        &grid, levels, &asm.problem, &settings, &result.skin_cells, &result.design_cells,
        &result.skin_frac, &x_uniform, 1.5, 1.0, Some(&result.u),
    )
    .unwrap();
    let gain = c_uniform / c_binned;
    assert!(gain > 1.10, "solid-or-hollow core should clearly beat uniform: {gain:.3}");
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

    let bytes = export_orca_3mf("bracket & arm", &part, &regions, 0.12, 3, 5, None);

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
    // The PART carries the perimeter count the analysis assumed; modifiers
    // override ONLY the infill density — a modifier wall key strips/changes
    // perimeters where it touches the surface (real-Orca finding).
    assert_eq!(cfg.matches("wall_loops").count(), 1, "wall_loops exactly once");
    assert!(cfg.contains("wall_loops\" value=\"3\""), "user perimeter count on the part");
    let object_level = &cfg[..cfg.find("<part").unwrap()];
    assert!(object_level.contains("wall_loops"), "wall_loops at object level, not in a part");
    assert!(!cfg.contains("sparse_infill_pattern"), "no pattern override unless requested");
    assert!(cfg.contains("bracket &amp; arm"));

    // Binary mode requests a solid-fill pattern — written as
    // sparse_infill_pattern ON EACH MODIFIER, never as object-level
    // internal_solid_infill_pattern (newer Bambu Studio renamed that key's
    // "rectilinear" value to "zig-zag" and warns on every project load).
    let bytes2 = export_orca_3mf("p", &part, &regions, 0.05, 2, 5, Some("concentric"));
    let entries2 = read_zip(&bytes2).unwrap();
    let cfg2 = entries2
        .iter()
        .find(|(n, _)| n == "Metadata/model_settings.config")
        .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
        .unwrap();
    assert!(!cfg2.contains("internal_solid_infill_pattern"), "deprecated key never written");
    assert_eq!(cfg2.matches("sparse_infill_pattern").count(), 2, "pattern on each modifier");
    assert!(cfg2.contains("sparse_infill_pattern\" value=\"concentric\""));
    let obj2 = &cfg2[..cfg2.find("<part").unwrap()];
    assert!(!obj2.contains("sparse_infill_pattern"), "pattern in modifiers, not object level");

    // PrusaSlicer flavor: ONE object, volumes as triangle ranges in
    // Slic3r_PE_model.config (part = ModelPart, regions = ParameterModifier
    // with fill_density; fill_pattern only when a solid pattern is chosen).
    let bytes3 = export_prusa_3mf("bracket & arm", &part, &regions, 0.12, 3, 5, Some("concentric"));
    let entries3 = read_zip(&bytes3).unwrap();
    let names3: Vec<&str> = entries3.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names3.contains(&"Metadata/Slic3r_PE_model.config"));
    let model3 = entries3
        .iter()
        .find(|(n, _)| n == "3D/3dmodel.model")
        .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
        .unwrap();
    assert!(model3.contains("slic3rpe:Version3mf"), "PrusaSlicer recognizes its own flavor");
    assert_eq!(model3.matches("<object ").count(), 1, "one object, volumes via config");
    let cfg3 = entries3
        .iter()
        .find(|(n, _)| n == "Metadata/Slic3r_PE_model.config")
        .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
        .unwrap();
    assert_eq!(cfg3.matches("<volume ").count(), 3, "part + 2 modifier volumes");
    assert_eq!(cfg3.matches("ParameterModifier").count(), 2);
    assert!(cfg3.contains("ModelPart"));
    // Part has 12 tris -> volume 0 is tris 0..=11; first modifier starts at 12.
    assert!(cfg3.contains("<volume firstid=\"0\" lastid=\"11\""));
    assert!(cfg3.contains("<volume firstid=\"12\""));
    assert!(cfg3.contains("fill_density\" value=\"25%\""));
    assert!(cfg3.contains("fill_density\" value=\"50%\""));
    assert!(cfg3.contains("fill_density\" value=\"12%\""), "base density on the object");
    assert!(cfg3.contains("perimeters\" value=\"3\""));
    assert_eq!(cfg3.matches("fill_pattern").count(), 2, "pattern per modifier");
    assert!(cfg3.contains("fill_pattern\" value=\"concentric\""));

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
        budget: 0.35,
        exponent: 1.5,
        wall_mm: 0.9,
        max_iter: 40,
        ..Default::default()
    };
    let mut prev_c = f64::INFINITY;
    let result = optimize(&grid, levels, &asm.problem, &settings, &params, None, None, |p, _x, _c| {
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

/// Diagnostic: MGCG convergence on the 3DBenchy at the app's resolution
/// presets. Run: cargo test -p sig-core --test phase3 benchy -- --ignored --nocapture
#[test]
#[ignore]
fn benchy_convergence() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../3dbenchy.stl");
    let bytes = std::fs::read(path).expect("3dbenchy.stl in repo root");
    let mesh = sig_core::TriMesh::from_stl(&bytes).expect("parse benchy");
    let (lo, hi) = mesh.bounds().unwrap();
    let vol = (hi[0] - lo[0]) * (hi[1] - lo[1]) * (hi[2] - lo[2]);
    println!(
        "benchy: {} tris, bbox {:.1}x{:.1}x{:.1}",
        mesh.len(), hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]
    );
    for (label, target) in
        [("preview", 100_000f64), ("normal", 300_000f64), ("fine", 1_000_000f64)]
    {
        let h = (vol / target).cbrt();
        let grid = VoxelGrid::voxelize(&mesh, h);
        println!(
            "{label}: h={h:.3} dims {}x{}x{} solid {}",
            grid.nx, grid.ny, grid.nz, grid.solid_count()
        );
        let problem = StaticProblem {
            grid,
            fixed: vec![BoxRegion::new(
                [lo[0] - 1.0, lo[1] - 1.0, lo[2] - 1.0],
                [hi[0] + 1.0, hi[1] + 1.0, lo[2] + 2.0],
            )],
            loads: vec![(
                BoxRegion::new(
                    [lo[0] - 1.0, lo[1] - 1.0, hi[2] - 6.0],
                    [hi[0] + 1.0, hi[1] + 1.0, hi[2] + 1.0],
                ),
                [5.0, 0.0, 0.0],
            )],
            settings: SolveSettings { max_iter: 600, ..Default::default() },
        };
        let t0 = std::time::Instant::now();
        match solve_static(&problem) {
            Ok(s) => println!(
                "  iters {} res {:.2e} maxu {:.4} mm ({:.1}s)",
                s.iterations, s.rel_residual, s.max_displacement(), t0.elapsed().as_secs_f64()
            ),
            Err(e) => println!("  FAILED after {:.1}s: {e}", t0.elapsed().as_secs_f64()),
        }
    }
}

#[test]
fn subdivision_preserves_area_and_respects_cap() {
    let m = primitives::boxx([0.0; 3], [40.0, 6.0, 6.0]);
    let area = |mm: &sig_core::TriMesh| -> f64 {
        mm.tris
            .iter()
            .map(|t| {
                let e1 = [(t[3] - t[0]) as f64, (t[4] - t[1]) as f64, (t[5] - t[2]) as f64];
                let e2 = [(t[6] - t[0]) as f64, (t[7] - t[1]) as f64, (t[8] - t[2]) as f64];
                let c = [
                    e1[1] * e2[2] - e1[2] * e2[1],
                    e1[2] * e2[0] - e1[0] * e2[2],
                    e1[0] * e2[1] - e1[1] * e2[0],
                ];
                0.5 * (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt()
            })
            .sum()
    };
    let s = m.subdivided(1.0, 1_000_000);
    assert!(s.len() > 1000, "40mm edges at 1mm target should refine ({} tris)", s.len());
    assert!((area(&s) - area(&m)).abs() / area(&m) < 1e-4, "area preserved");
    let capped = m.subdivided(0.1, 5_000);
    assert!(capped.len() <= 5_000, "budget respected ({} tris)", capped.len());
    // Already-fine mesh passes through unchanged.
    let fine = s.subdivided(10.0, 1_000_000);
    assert_eq!(fine.len(), s.len());
    // Parent map covers every child and references valid originals; every
    // original triangle has at least one child.
    let (s2, parents) = m.subdivided_with_parents(1.0, 1_000_000);
    assert_eq!(parents.len(), s2.len());
    assert!(parents.iter().all(|&p| (p as usize) < m.len()));
    let mut seen = vec![false; m.len()];
    for &p in &parents {
        seen[p as usize] = true;
    }
    assert!(seen.iter().all(|&b| b), "every original triangle has children");
}

#[test]
fn column_compression_stress_matches_nominal() {
    use sig_core::solve::{solve_nodes, NodeProblem};
    use sig_core::stress::{cell_field, FieldKind};
    // 8x8x16 mm column, E=2000, clamped bottom, 64 N total down on top:
    // nominal sigma_zz = -64/(8*8) = -1 MPa away from the ends.
    let grid0 = VoxelGrid::solid_box(8, 8, 16, 1.0);
    let settings = SolveSettings { e0: 2000.0, nu: 0.3, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
    let (mx, my) = (grid.nx + 1, grid.ny + 1);
    let mut np = NodeProblem::default();
    let mut top = Vec::new();
    for y in 0..=8usize {
        for x in 0..=8usize {
            np.fixed.push((y * mx + x) as u32);
            top.push(((16 * my + y) * mx + x) as u32);
        }
    }
    let per_node = -64.0 / top.len() as f64;
    for &n in &top {
        np.forces.push((n, [0.0, 0.0, per_node]));
    }
    let sol = solve_nodes(&grid, levels, &np, &settings).expect("solve");
    assert!(sol.converged);

    let eps = grid.scale.clone();
    let szz = cell_field(&grid, &sol.u, settings.e0, settings.nu, &eps, FieldKind::Szz);
    let vm = cell_field(&grid, &sol.u, settings.e0, settings.nu, &eps, FieldKind::VonMises);
    let ezz = cell_field(&grid, &sol.u, settings.e0, settings.nu, &eps, FieldKind::Ezz);
    let sxx = cell_field(&grid, &sol.u, settings.e0, settings.nu, &eps, FieldKind::Sxx);
    let ci = grid.cell_index(4, 4, 8); // center, mid-height (St-Venant zone)
    assert!((szz[ci] + 1.0).abs() < 0.06, "sigma_zz {} vs -1 MPa", szz[ci]);
    assert!((vm[ci] - 1.0).abs() < 0.06, "von Mises {} vs 1 MPa", vm[ci]);
    assert!(sxx[ci].abs() < 0.08, "sigma_xx {} ~ 0", sxx[ci]);
    // Uniaxial: eps_zz = sigma/E = -5e-4.
    assert!(
        (ezz[ci] as f64 + 5e-4).abs() < 3e-5,
        "eps_zz {} vs -5e-4",
        ezz[ci]
    );
}

#[test]
fn displacement_sampling_ignores_inactive_nodes() {
    use sig_core::solve::{active_nodes, Solution};
    // One solid cell at (1,1,1) in an otherwise empty 4^3 grid.
    let mut grid = VoxelGrid::solid_box(4, 4, 4, 1.0);
    grid.scale.iter_mut().for_each(|s| *s = 0.0);
    let ci = grid.cell_index(1, 1, 1);
    grid.scale[ci] = 1.0;
    let active = active_nodes(&grid);
    let (mx, my, mz) = (5usize, 5usize, 5usize);
    let mut u = vec![0f32; 3 * mx * my * mz];
    for n in 0..mx * my * mz {
        if active[n] {
            u[3 * n] = 2.0; // uniform x-displacement on the solid cell's nodes
        }
    }
    let sol = Solution {
        u,
        mx,
        my,
        mz,
        h: 1.0,
        origin: [0.0; 3],
        active,
        iterations: 1,
        rel_residual: 0.0,
        converged: true,
        residuals: Vec::new(),
    };
    // Inside the solid cell: exact either way.
    assert!((sol.sample_displacement([1.5, 1.5, 1.5])[0] - 2.0).abs() < 1e-9);
    // Just OUTSIDE the cell face: plain trilinear would dilute with the
    // void-side zeros (0.9*2.0 = 1.8); active-aware sampling stays exact.
    // This is the thin-wall "shredded stripes / stuck vertices" bug.
    assert!((sol.sample_displacement([1.5, 1.5, 2.1])[0] - 2.0).abs() < 1e-9);
    // Deep in the void: nearest-active fallback, not a stuck zero.
    assert!((sol.sample_displacement([3.9, 3.9, 3.9])[0] - 2.0).abs() < 1e-9);
}

#[test]
fn elastic_foundation_settles_by_sigma_over_k() {
    use sig_core::solve::solve_nodes;
    // 10x10x20 mm column standing on an elastic (Winkler) foundation, uniform
    // pressure on top: the base must settle by u = sigma/k and the top adds
    // the column's own elastic shortening sigma*L/E. Validates the area-
    // consistent spring assembly AND the springs-only (no Dirichlet) path.
    let col = primitives::boxx([0.0; 3], [10.0, 10.0, 20.0]);
    let grid0 = VoxelGrid::voxelize(&col, 1.0);
    // nu = 0 keeps the column uniaxial (no Poisson barreling) for a clean
    // analytic reference.
    let settings = SolveSettings { e0: 2000.0, nu: 0.0, tol: 1e-7, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
    let k = 10.0; // N/mm^3 bedding modulus
    let sigma = 0.5; // MPa on the top face, pushing down
    let bcs = vec![
        BcSpec { kind: BcKind::Elastic(k), tris: face_tris(4) }, // -z face
        BcSpec { kind: BcKind::Pressure(sigma), tris: face_tris(5) }, // +z face
    ];
    let asm = assemble(&col, &grid, &bcs, None, &settings).unwrap();
    let report = check_problem(&grid, &asm);
    assert!(report.ok, "elastic springs alone must constrain all rigid-body modes");
    let sol = solve_nodes(&grid, levels, &asm.problem, &settings).expect("solve");
    assert!(sol.converged);

    let settle = sigma / k; // 0.05 mm foundation compression
    let u_base = sol.sample_displacement([5.0, 5.0, 0.0]);
    assert!(
        (u_base[2] + settle).abs() < 0.08 * settle,
        "base settles by sigma/k: got {} expected {}",
        u_base[2],
        -settle
    );
    let shorten = sigma * 20.0 / settings.e0; // 0.005 mm elastic shortening
    let u_top = sol.sample_displacement([5.0, 5.0, 20.0]);
    assert!(
        (u_top[2] + settle + shorten).abs() < 0.08 * (settle + shorten),
        "top = settle + shortening: got {} expected {}",
        u_top[2],
        -(settle + shorten)
    );
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
    let s = classify_cells(&grid, 1.0, 1.0, 1.0, false);
    assert_eq!(s.skin.len() + s.design.len(), 1000);
    // 1-layer skin of a 10^3 box: 10^3 - 8^3 = 488.
    assert_eq!(s.skin.len(), 488, "one skin layer expected");
    assert!(s.skin_frac.iter().all(|&f| f == 0.0), "legacy mode: no fractions");
    let s2 = classify_cells(&grid, 2.0, 2.0, 2.0, false);
    assert_eq!(s2.skin.len(), 1000 - 6 * 6 * 6);
    assert_eq!(s2.design.len(), 216);

    // Composite mode at integer wall/h reproduces the legacy split exactly.
    let c2 = classify_cells(&grid, 2.0, 2.0, 2.0, true);
    assert_eq!(c2.skin, s2.skin);
    assert_eq!(c2.design, s2.design);
    assert!(c2.skin_frac.iter().all(|&f| f == 0.0));
}

#[test]
fn mirror_pairs_involutive_on_aligned_plane() {
    // 10^3 solid box, plane x = 5 (a cell boundary): every cell has an exact
    // mirror cell, the pairing is an involution, and cx maps to 9 - cx.
    let grid = VoxelGrid::solid_box(10, 10, 10, 1.0);
    let design: Vec<u32> = (0..1000).collect();
    let partner = build_mirror_pairs(&grid, &design, [1.0, 0.0, 0.0, 5.0]);
    for (k, &j) in partner.iter().enumerate() {
        assert_ne!(j, u32::MAX, "every cell of the box has a mirror");
        assert_eq!(partner[j as usize], k as u32, "involution");
        let cx = design[k] as usize % 10;
        let mx = design[j as usize] as usize % 10;
        assert_eq!(mx, 9 - cx, "x column mirrors");
    }
    // A plane outside the box pairs nothing.
    let none = build_mirror_pairs(&grid, &design, [1.0, 0.0, 0.0, 50.0]);
    assert!(none.iter().all(|&j| j == u32::MAX));
}

#[test]
fn symmetry_constraint_yields_mirror_density() {
    // Cantilever, symmetric about the beam's y mid-plane: with the
    // constraint on, the optimized field must be EXACTLY mirror-symmetric
    // for every paired cell (the final projection enforces it).
    let beam = primitives::boxx([0.0; 3], [60.0, 10.0, 10.0]);
    let grid0 = VoxelGrid::voxelize(&beam, 1.0);
    let settings = SolveSettings { e0: 2400.0, nu: 0.35, tol: 1e-5, ..Default::default() };
    let (grid, levels) = pad_for_levels(&grid0, settings.max_levels);
    let bcs = vec![
        BcSpec { kind: BcKind::Fixed, tris: face_tris(0) },
        BcSpec { kind: BcKind::Force([0.0, 0.0, -30.0]), tris: face_tris(1) },
    ];
    let asm = assemble(&beam, &grid, &bcs, None, &settings).unwrap();
    let plane = [0.0, 1.0, 0.0, 5.0]; // beam spans y in [0, 10]
    let params = OptimizeParams {
        budget: 0.35,
        exponent: 1.5,
        wall_mm: 1.0,
        symmetry: Some(plane),
        max_iter: 12,
        ..Default::default()
    };
    let result =
        optimize(&grid, levels, &asm.problem, &settings, &params, None, None, |_p, _x, _c| {})
            .expect("optimize");
    let partner = build_mirror_pairs(&grid, &result.design_cells, plane);
    let paired = partner.iter().filter(|&&j| j != u32::MAX).count();
    assert!(
        paired as f64 > 0.9 * partner.len() as f64,
        "most interior cells pair across the mid-plane ({paired}/{})",
        partner.len()
    );
    for (k, &j) in partner.iter().enumerate() {
        if j == u32::MAX {
            continue;
        }
        let d = (result.x[k] - result.x[j as usize]).abs();
        assert!(d < 1e-9, "mirror cells share their density (Δ = {d:.2e})");
    }
}

#[test]
fn nodal_recovery_averages_adjacent_cells() {
    use sig_core::stress::recover_nodal;
    // 2x1x1 solid cells with values 1 and 3: shared face nodes average to 2,
    // outer nodes keep their cell's value, and a padded void region (the
    // grid is 4 wide) yields NaN on nodes touching no solid cell.
    let mut grid = VoxelGrid::solid_box(4, 1, 1, 1.0);
    grid.scale[2] = 0.0;
    grid.scale[3] = 0.0;
    let values = vec![1.0f32, 3.0, 0.0, 0.0];
    let nodal = recover_nodal(&grid, &values);
    let (mx, my) = (5, 2);
    let n = |x: usize, y: usize, z: usize| (z * my + y) * mx + x;
    for y in 0..2 {
        for z in 0..2 {
            assert_eq!(nodal[n(0, y, z)], 1.0, "outer nodes of cell 0");
            assert_eq!(nodal[n(1, y, z)], 2.0, "shared nodes average 1 and 3");
            assert_eq!(nodal[n(2, y, z)], 3.0, "outer nodes of cell 1");
            assert!(nodal[n(4, y, z)].is_nan(), "void-only nodes are NaN");
        }
    }
}

#[test]
fn voxelize_cut_cells_carry_occupancy() {
    // A 9.5 mm box on a 1 mm grid: the grid centers on the bounds, so every
    // outer cell is only 75% inside. The 3×3×3 supersample (local 1/6, 1/2,
    // 5/6) sees 2 of 3 stations inside along each cut axis — exact fractions
    // 18/27 (face), 12/27 (edge), 8/27 (corner), 1.0 interior.
    let b = primitives::boxx([0.0; 3], [9.5, 9.5, 9.5]);
    let grid = VoxelGrid::voxelize(&b, 1.0);
    assert_eq!((grid.nx, grid.ny, grid.nz), (10, 10, 10));
    let mut counts = [0usize; 4]; // face, edge, corner, interior
    for cz in 0..10 {
        for cy in 0..10 {
            for cx in 0..10 {
                let s = grid.scale[(cz * 10 + cy) * 10 + cx];
                assert!(s > 0.0, "center-inside cells stay solid");
                let cut = [cx, cy, cz].iter().filter(|&&c| c == 0 || c == 9).count();
                let expect = [18.0 / 27.0, 12.0 / 27.0, 8.0 / 27.0, 1.0][if cut == 0 {
                    3
                } else {
                    cut - 1
                }];
                assert!(
                    (s - expect as f32).abs() < 1e-6,
                    "cell ({cx},{cy},{cz}) occupancy {s} vs {expect}"
                );
                counts[if cut == 0 { 3 } else { cut - 1 }] += 1;
            }
        }
    }
    assert_eq!(counts, [6 * 8 * 8, 12 * 8, 8, 8 * 8 * 8]);
    // Occupancy-weighted volume approaches the true 9.5³. The 3-station
    // quantization reads 0.75-covered cells as 2/3, so this worst-case
    // alignment lands ~5% low — far better than the +19% of counting cut
    // cells as full.
    let vol = grid.solid_volume();
    assert!(
        (vol / 9.5f64.powi(3) - 1.0).abs() < 0.07,
        "occupancy volume {vol:.1} vs true {:.1}",
        9.5f64.powi(3)
    );
}

#[test]
fn classify_cells_directional_shells() {
    // 10^3 box, h = 1: walls follow each slice's outline, shells the columns.
    let grid0 = VoxelGrid::solid_box(10, 10, 10, 1.0);
    let (grid, _) = pad_for_levels(&grid0, 1);
    // 1-layer walls + 2-layer shells: side ring (36/slice × 10) ∪ top 2 +
    // bottom 2 of every column (4 × 100), overlap 36 × 4.
    let s = classify_cells(&grid, 1.0, 2.0, 2.0, false);
    assert_eq!(s.skin.len(), 360 + 400 - 144, "ring ∪ shells");
    // Shells off (open-top showpiece): only the side ring stays solid —
    // the infill runs right to the top/bottom surface.
    let open = classify_cells(&grid, 1.0, 0.0, 0.0, false);
    assert_eq!(open.skin.len(), 360, "walls only");
    assert_eq!(open.design.len(), 640);
    assert!(open.skin_frac.iter().all(|&f| f == 0.0));
}

#[test]
fn classify_cells_composite_fractions() {
    // Wall = half a cell: no cell is fully skin; surface cells carry the
    // overlapping-slab fraction of the 0.5-cell band.
    let grid0 = VoxelGrid::solid_box(10, 10, 10, 1.0);
    let (grid, _) = pad_for_levels(&grid0, 1);
    let c = classify_cells(&grid, 0.5, 0.5, 0.5, true);
    assert!(c.skin.is_empty(), "no cell is fully inside a half-cell band");
    assert_eq!(c.design.len(), 1000);
    // Face cells: one void side -> f = 0.5. Edge cells: two orthogonal void
    // sides -> 1 - 0.5^2 = 0.75. Corner cells: 1 - 0.5^3 = 0.875.
    let (mut n_half, mut n_edge, mut n_corner, mut n_interior) = (0, 0, 0, 0);
    for &f in &c.skin_frac {
        if (f - 0.5).abs() < 1e-6 {
            n_half += 1;
        } else if (f - 0.75).abs() < 1e-6 {
            n_edge += 1;
        } else if (f - 0.875).abs() < 1e-6 {
            n_corner += 1;
        } else if f == 0.0 {
            n_interior += 1;
        } else {
            panic!("unexpected skin fraction {f}");
        }
    }
    assert_eq!(n_half, 6 * 8 * 8, "face cells");
    assert_eq!(n_edge, 12 * 8, "edge cells");
    assert_eq!(n_corner, 8, "corner cells");
    assert_eq!(n_interior, 8 * 8 * 8, "interior cells");

    // A 1-cell-thick plate with a 0.4-cell wall: both faces exposed along
    // one axis -> two slabs, f = 0.8 in the plate's face cells.
    let plate0 = VoxelGrid::solid_box(10, 10, 1, 1.0);
    let (plate, _) = pad_for_levels(&plate0, 1);
    let p = classify_cells(&plate, 0.4, 0.4, 0.4, true);
    let n_two_sided = p.skin_frac.iter().filter(|&&f| (f - 0.8).abs() < 1e-6).count();
    assert_eq!(n_two_sided, 8 * 8, "plate face cells count both walls");
}

#[test]
fn weld_dedups_box() {
    let m = weld(&primitives::boxx([0.0; 3], [5.0, 5.0, 5.0]));
    assert_eq!(m.vertices.len(), 8);
    assert_eq!(m.triangles.len(), 12);
    let _ = IndexedMesh { vertices: m.vertices.clone(), triangles: m.triangles.clone() };
}
