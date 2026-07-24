// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Stefan Hermann (CNC Kitchen) <stefan@cnckitchen.com>

//! Golden-file pin for the 3MF writers. DESIGN.md §5 calls 3MF-compatibility
//! drift a top risk, yet the writer is ~90 interleaved `push_str`/`format!`
//! calls and the other test only checks substring presence — blind to
//! structural regressions: moved blocks, broken UUID cross-refs, or the
//! attribute-order changes Bambu's loader is sensitive to. Here we export a
//! fixed part + two modifiers and assert every XML part byte-for-byte against a
//! committed fixture, so any change to the wire format shows up as a diff.
//!
//! Intentional format changes: regenerate with
//!   SIG_UPDATE_GOLDEN=1 cargo test -p filasim-core --test threemf_golden
//! then review and commit the updated fixtures under tests/golden/.

use filasim_core::bins::RegionMesh;
use filasim_core::mesh::primitives;
use filasim_core::threemf::{export_orca_3mf, export_prusa_3mf, weld};
use filasim_core::zip::read_zip;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A small but non-trivial part + two nested modifier regions. Fixed geometry
/// → fully deterministic output (UUIDs and the bed-placement transform are
/// derived from this, nothing random).
fn fixture_inputs() -> (filasim_core::threemf::IndexedMesh, Vec<RegionMesh>) {
    let part = weld(&primitives::boxx([0.0; 3], [30.0, 20.0, 10.0]));
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
    (part, regions)
}

/// Unzip and join every XML part (names sorted) into one snapshot string. PNGs
/// and other binaries are excluded — the spec lives in the XML.
fn xml_snapshot(zipped: &[u8]) -> String {
    let parts: BTreeMap<String, String> = read_zip(zipped)
        .expect("export produced a readable zip")
        .into_iter()
        .filter(|(n, _)| {
            n.ends_with(".model") || n.ends_with(".config") || n.ends_with(".rels") || n.ends_with(".xml")
        })
        .map(|(n, d)| (n, String::from_utf8(d).expect("XML parts are UTF-8")))
        .collect();
    let mut out = String::new();
    for (name, body) in &parts {
        out.push_str(&format!("===== {name} =====\n"));
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

fn assert_golden(flavor: &str, snapshot: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{flavor}.3mf.txt"));
    if std::env::var("SIG_UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, snapshot).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| {
            panic!("missing golden {path:?} — run `SIG_UPDATE_GOLDEN=1 cargo test` to create it")
        })
        // The writer emits '\n' only; tolerate CRLF the fixture may pick up from
        // git autocrlf on checkout so the pin stays portable (Windows / CI).
        .replace("\r\n", "\n");
    assert_eq!(
        snapshot,
        want.as_str(),
        "{flavor} 3MF output drifted from {path:?}; if intentional, regenerate with SIG_UPDATE_GOLDEN=1"
    );
}

#[test]
fn orca_3mf_matches_golden() {
    let (part, regions) = fixture_inputs();
    // solid_pattern set so the modifier + object-level pattern keys are covered.
    let zip =
        export_orca_3mf("bracket & arm", &part, &regions, 0.12, 3, 5, Some("concentric"), None, None);
    assert_golden("orca", &xml_snapshot(&zip));
}

#[test]
fn prusa_3mf_matches_golden() {
    let (part, regions) = fixture_inputs();
    let zip =
        export_prusa_3mf("bracket & arm", &part, &regions, 0.12, 3, 5, Some("concentric"), None);
    assert_golden("prusa", &xml_snapshot(&zip));
}
