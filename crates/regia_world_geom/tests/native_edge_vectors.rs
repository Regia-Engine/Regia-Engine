// SPDX-License-Identifier: MPL-2.0
//! The explicitly permitted ownership replacement; original T3 vectors are immutable.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]
pub mod native_support;
use native_support::*;
use regia_world_geom::{SpanBuf, SurfaceKind};
use serde_json::Value;
#[test]
fn regenerated_edge_rows_have_regia_provenance_and_stay_within_one_mm() {
    let replacement: Value = serde_json::from_str(include_str!(
        "fixtures/regia/t3-edge-vectors/cinder_steps_mesh.json"
    ))
    .unwrap();
    assert_eq!(replacement["format"], "do27-t3-vectors/1");
    assert_eq!(replacement["provenance"], "regia");
    assert_eq!(
        replacement["query_bits_version"],
        regia_world_geom::QUERY_BITS_VERSION
    );
    let original = read("cinder_steps_mesh");
    assert_eq!(replacement["triangles"], original["triangles"]);
    let geom = native(&original, "cinder_steps_mesh", 16.0);
    let state = geom.new_state();
    assert_eq!(replacement["regia_changes"].as_array().unwrap().len(), 1);
    for (row, change) in replacement["columns"]
        .as_array()
        .unwrap()
        .iter()
        .zip(replacement["regia_changes"].as_array().unwrap())
    {
        let old = &original["columns"][change["original_column"].as_u64().unwrap() as usize];
        assert_eq!(row["x"], old["x"]);
        assert_eq!(row["z"], old["z"]);
        let (x, z) = (hex(&row["x"]), hex(&row["z"]));
        let mut edges = std::collections::BTreeMap::new();
        for tri in geom.triangles() {
            for i in 0..3 {
                let a = tri.v[i].map(regia_world_geom::m_to_mm);
                let b = tri.v[(i + 1) % 3].map(regia_world_geom::m_to_mm);
                let key = if a < b { (a, b) } else { (b, a) };
                *edges.entry(key).or_insert(0usize) += 1;
            }
        }
        assert!(
            edges.into_iter().any(|((a, b), n)| {
                let (a, b) = (a.map(|v| v as f32 / 1000.0), b.map(|v| v as f32 / 1000.0));
                let dx = b[0] - a[0];
                let dz = b[2] - a[2];
                n >= 2
                    && (dx != 0.0 || dz != 0.0)
                    && dx * (z - a[2]) - dz * (x - a[0]) == 0.0
                    && (x - a[0]) * (x - b[0]) <= 0.0
                    && (z - a[2]) * (z - b[2]) <= 0.0
            }),
            "replacement must lie exactly on a shared edge"
        );
        let mut out = SpanBuf::default();
        geom.floors_at(&state, x, z, &mut out);
        let expected = row["spans"].as_array().unwrap();
        let prior = old["spans"].as_array().unwrap();
        assert_eq!(out.spans().len(), expected.len());
        assert_eq!(expected.len(), prior.len());
        for ((got, want), old) in out.spans().iter().zip(expected).zip(prior) {
            assert_eq!(want[2], old[2]);
            assert!((hex(&want[0]) - hex(&old[0])).abs() <= 0.001);
            assert_eq!(got.floor.y.to_bits(), hex(&want[0]).to_bits());
            assert_eq!(
                got.ceiling.map(|l| l.y.to_bits()),
                want[1].as_str().map(|_| hex(&want[1]).to_bits())
            );
            if !want[1].is_null() {
                assert!((hex(&want[1]) - hex(&old[1])).abs() <= 0.001);
            }
            if got.floor.kind == SurfaceKind::Ground {
                assert_eq!(want[0], old[0]);
            }
        }
        for at in row["at"].as_array().unwrap() {
            let got = geom.support(&state, x, z, hex(&at[0]));
            assert_eq!(
                out.spans().iter().position(|s| *s == got),
                Some(at[1].as_u64().unwrap() as usize)
            );
        }
    }
}
