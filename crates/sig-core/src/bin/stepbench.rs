// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Dev harness for the STEP importer: tessellate a .step at several settings,
//! write each result as an .stl for visual inspection, and print mesh-regularity
//! stats next to the reference CAD-exported .stl.
//!
//! Usage (from workspace root):
//!   cargo run -p sig-core --bin stepbench -- "hook5 v3.step" "hook5 v3.stl"

use sig_core::mesh::TriMesh;
use sig_core::step::{import_step, StepTessellation};

struct Stats {
    tris: usize,
    diag: f64,
    edge_mean: f64,
    edge_cov: f64, // std/mean — lower = more uniform edge lengths
    edge_min: f64,
    edge_max: f64,
    ang_p5: f64,    // 5th-percentile minimum-angle (deg) — low = slivers present
    ang_median: f64,
    sliver_frac: f64, // fraction of triangles with a min angle < 15 deg
}

fn stats(mesh: &TriMesh) -> Stats {
    let (lo, hi) = mesh.bounds().unwrap_or(([0.0; 3], [0.0; 3]));
    let diag = ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2) + (hi[2] - lo[2]).powi(2)).sqrt();

    let mut edges: Vec<f64> = Vec::with_capacity(mesh.tris.len() * 3);
    let mut min_angles: Vec<f64> = Vec::with_capacity(mesh.tris.len());
    for t in &mesh.tris {
        let p = |k: usize| [t[3 * k] as f64, t[3 * k + 1] as f64, t[3 * k + 2] as f64];
        let (a, b, c) = (p(0), p(1), p(2));
        let d = |u: [f64; 3], v: [f64; 3]| {
            ((u[0] - v[0]).powi(2) + (u[1] - v[1]).powi(2) + (u[2] - v[2]).powi(2)).sqrt()
        };
        let (la, lb, lc) = (d(b, c), d(a, c), d(a, b)); // side opposite each vertex
        edges.push(la);
        edges.push(lb);
        edges.push(lc);
        // interior angles via law of cosines; guard degenerate
        let ang = |opp: f64, x: f64, y: f64| -> f64 {
            if x < 1e-12 || y < 1e-12 {
                return 0.0;
            }
            let ct = ((x * x + y * y - opp * opp) / (2.0 * x * y)).clamp(-1.0, 1.0);
            ct.acos().to_degrees()
        };
        let aa = ang(la, lb, lc);
        let bb = ang(lb, la, lc);
        let cc = ang(lc, la, lb);
        min_angles.push(aa.min(bb).min(cc));
    }

    let n = edges.len().max(1) as f64;
    let emean = edges.iter().sum::<f64>() / n;
    let evar = edges.iter().map(|e| (e - emean).powi(2)).sum::<f64>() / n;
    let ecov = if emean > 0.0 { evar.sqrt() / emean } else { 0.0 };

    let mut sa = edges.clone();
    sa.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let emin = *sa.first().unwrap_or(&0.0);
    let emax = *sa.last().unwrap_or(&0.0);

    min_angles.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let pct = |v: &[f64], p: f64| -> f64 {
        if v.is_empty() {
            return 0.0;
        }
        v[((v.len() as f64 - 1.0) * p).round() as usize]
    };
    let ang_p5 = pct(&min_angles, 0.05);
    let ang_median = pct(&min_angles, 0.5);
    let sliver = min_angles.iter().filter(|&&a| a < 15.0).count() as f64
        / min_angles.len().max(1) as f64;

    Stats {
        tris: mesh.tris.len(),
        diag,
        edge_mean: emean,
        edge_cov: ecov,
        edge_min: emin,
        edge_max: emax,
        ang_p5,
        ang_median,
        sliver_frac: sliver,
    }
}

fn print_row(label: &str, s: &Stats) {
    println!(
        "{:<28} {:>8} {:>9.3} {:>7.2} {:>7.3} {:>8.2} {:>8.1} {:>9.1} {:>9.1}%",
        label,
        s.tris,
        s.edge_mean,
        s.edge_cov,
        s.edge_min,
        s.edge_max,
        s.ang_p5,
        s.ang_median,
        s.sliver_frac * 100.0,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let step_path = args.get(1).cloned().unwrap_or_else(|| "hook5 v3.step".into());
    let stl_path = args.get(2).cloned().unwrap_or_else(|| "hook5 v3.stl".into());

    let bytes = std::fs::read(&step_path).expect("read step file");
    println!("STEP file: {step_path} ({} KB)\n", bytes.len() / 1024);

    println!(
        "{:<28} {:>8} {:>9} {:>7} {:>7} {:>8} {:>8} {:>9} {:>10}",
        "case", "tris", "edge_mean", "CoV", "edge_min", "edge_max", "ang_p5", "ang_med", "slivers"
    );

    // Baseline (auto deviation, no subdivision) — establishes model scale.
    let base = import_step(&bytes, &StepTessellation::default()).expect("import baseline");
    let bs = stats(&base.mesh);
    println!(
        "  [shells={} faces={} tol={:.4} mm]",
        base.shell_count, base.face_count, base.tolerance
    );
    print_row("auto-dev, no-subdiv", &bs);
    std::fs::write("hook_truck_auto.stl", base.mesh.to_stl_binary()).ok();

    // Sweep the one truck knob (surface deviation). This reports the BASE
    // tessellation only; in the product, Model::new refines it like an STL.
    let cases: &[(&str, f64)] = &[
        ("dev=0.05 mm", 0.05),
        ("dev=0.02 mm", 0.02),
        ("dev=0.01 mm", 0.01),
        ("dev=0.005 mm", 0.005),
    ];
    for (label, dev) in cases {
        let settings = StepTessellation {
            surface_deviation: Some(*dev),
            ..Default::default()
        };
        match import_step(&bytes, &settings) {
            Ok(imp) => {
                let s = stats(&imp.mesh);
                print_row(label, &s);
                let fname = format!("hook_truck_{}.stl", label.replace([' ', '.', '='], "_"));
                std::fs::write(fname, imp.mesh.to_stl_binary()).ok();
            }
            Err(e) => println!("{label:<28} ERROR: {e}"),
        }
    }

    // Reference: the CAD-exported STL.
    if let Ok(stl_bytes) = std::fs::read(&stl_path) {
        if let Ok(stl) = TriMesh::from_stl(&stl_bytes) {
            println!("\n-- reference CAD STL ({stl_path}) --");
            print_row("reference.stl", &stats(&stl));
        }
    }

    println!(
        "\nLegend: edge_* in mm. CoV = stddev/mean of edge length (lower = more uniform).\n\
         ang_p5/ang_med = 5th-pct / median of per-triangle MIN angle in deg (60 = equilateral).\n\
         slivers = % of triangles with a min angle < 15 deg. Wrote hook_truck_*.stl for inspection."
    );
}
