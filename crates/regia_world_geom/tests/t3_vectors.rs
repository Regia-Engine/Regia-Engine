// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/tests/t3_vectors.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-431, mark: adapt (Stage A: unchanged). Only the check half is here: load a committed file,
// build the mesh from `(mm as f32) / 1000.0`, and compare every column and every `at` row. The
// generator and the `column_cost` timing are requirements (lane W-11 owns the cost bar).
// Data: spec/maps/t3-vectors/*.json at the same tag, as-is, under tests/fixtures/do27/t3-vectors/.

//! T3 reference vectors (format `do27-t3-vectors/1`) through `compat::surface::column` and
//! `Column::span_at` (plan row W-02 acceptance 2, spec 121 § 13.1).
//!
//! Each file holds the input mesh in integer millimetres, and per query XZ every free span bottom up
//! (`[floor, ceiling or null, kind]`) and `at` rows `[y, index]`: the index of the span `span_at(y)`
//! answers. Mesh-only fixtures have no terrain, so every height bit is pinned. The canyon fixture
//! stands the Switchbacks mesh in a flat canyon floor: its pin is the span count, the kinds and the
//! index answered (the handover's rule; a port's ground is not this heightfield). This crate's sampler
//! is the copied one, so the canyon heights are also compared, as a second, stronger check.

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use regia_world_geom::compat::mesh::{TunnelCollisionMesh, TunnelFaceKind, TunnelTriangle};
use regia_world_geom::compat::sampler::{FloorSurface, HeightGrid, TerrainSampler};
use regia_world_geom::compat::surface::column;
use serde_json::Value;

const FORMAT: &str = "do27-t3-vectors/1";

/// The four files and whether every height bit is pinned (mesh-only) or the span choice (canyon).
const FIXTURES: [(&str, bool); 4] = [
    ("switchbacks_mesh", true),
    ("cinder_steps_mesh", true),
    ("four_decks_mesh", true),
    ("switchbacks_canyon", false),
];

/// `$REGIA_WORLD_GEOM_FIXTURES` when set (a det-golden leg off the source tree, ADR-8838), else the
/// committed directory.
#[expect(
    clippy::disallowed_methods,
    reason = "test support: REGIA_WORLD_GEOM_FIXTURES names the vector directory, read before any query (ADR-8367)"
)]
fn dir() -> PathBuf {
    std::env::var_os("REGIA_WORLD_GEOM_FIXTURES")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/do27/t3-vectors"))
}

fn from_mm(v: &Value) -> f32 {
    let mm = v.as_i64().expect("integer millimetres");
    let mm = i32::try_from(mm).expect("millimetres fit i32");
    mm as f32 / 1000.0
}

fn hex(v: &Value) -> f32 {
    let text = v.as_str().expect("hex bit pattern");
    f32::from_bits(u32::from_str_radix(text, 16).expect("8 hex digits"))
}

/// The flat canyon floor the terrain fixture stands in: y = -256 m everywhere.
fn canyon() -> TerrainSampler {
    TerrainSampler::new(HeightGrid::square(4, vec![8192; 16]).expect("grid"), 1280.0).with_zero_counts(16384)
}

fn triangles(file: &Value) -> Vec<TunnelTriangle> {
    file["triangles"]
        .as_array()
        .expect("triangles")
        .iter()
        .map(|row| {
            let row = row.as_array().expect("triangle row");
            let kind = match row[0].as_str().expect("kind") {
                "F" => TunnelFaceKind::Floor,
                "W" => TunnelFaceKind::Wall,
                "C" => TunnelFaceKind::Ceiling,
                other => panic!("unknown kind {other}"),
            };
            let p = |i: usize| [from_mm(&row[i]), from_mm(&row[i + 1]), from_mm(&row[i + 2])];
            TunnelTriangle {
                a: p(1),
                b: p(4),
                c: p(7),
                kind,
                portal: false,
                pilot_only: false,
            }
        })
        .collect()
}

/// Counts of what was compared, for the report.
#[derive(Default, Debug)]
struct Tally {
    columns: usize,
    spans: usize,
    rows: usize,
    height_diffs: usize,
}

fn check(name: &str, every_bit: bool) -> Tally {
    let text = std::fs::read_to_string(dir().join(format!("{name}.json"))).expect("committed vector file");
    let file: Value = serde_json::from_str(&text).expect("vector JSON");
    assert_eq!(file["format"], FORMAT, "{name}");
    let terrain = (!every_bit).then(canyon);
    let mesh = TunnelCollisionMesh::new(triangles(&file)).welded(terrain.as_ref());
    assert_eq!(
        file["counts"]["support_floors"].as_u64(),
        Some(mesh.support_floors().len() as u64),
        "{name}: support floor count (trim and weld)"
    );
    let mut tally = Tally::default();
    for (i, want) in file["columns"].as_array().expect("columns").iter().enumerate() {
        let (x, z) = (hex(&want["x"]), hex(&want["z"]));
        let tag = want["tag"].as_str().unwrap_or("?");
        let col = column(terrain.as_ref(), Some(&mesh), x, z);
        let spans = col.spans();
        let want_spans = want["spans"].as_array().expect("spans");
        assert_eq!(
            spans.len(),
            want_spans.len(),
            "{name} column {i} ({tag}, {x}, {z}): span count {spans:?}"
        );
        for (k, (have, want)) in spans.iter().zip(want_spans).enumerate() {
            let kind = match have.floor.surface {
                FloorSurface::TunnelFloor => "mesh",
                FloorSurface::Heightfield => "ground",
            };
            assert_eq!(kind, want[2], "{name} column {i} ({tag}) span {k}: kind");
            let floor_same = have.floor.point[1].to_bits() == hex(&want[0]).to_bits();
            let lid_same = match (&want[1], have.ceiling) {
                (Value::Null, None) => true,
                (Value::Null, Some(_)) | (_, None) => false,
                (lid, Some(c)) => c.to_bits() == hex(lid).to_bits(),
            };
            if every_bit {
                assert!(
                    floor_same,
                    "{name} column {i} ({tag}) span {k}: floor {have:?} vs {want}"
                );
                assert!(
                    lid_same,
                    "{name} column {i} ({tag}) span {k}: ceiling {have:?} vs {want}"
                );
            } else {
                // The lid is mesh data, pinned on every fixture; only the ground height is reference.
                assert_eq!(
                    have.ceiling.is_some(),
                    !want[1].is_null(),
                    "{name} column {i} ({tag}) span {k}: covered"
                );
                tally.height_diffs += usize::from(!floor_same) + usize::from(!lid_same);
            }
            tally.spans += 1;
        }
        for row in want["at"].as_array().expect("at rows") {
            let y = hex(&row[0]);
            let chosen = col.span_at(y);
            let index = spans
                .iter()
                .position(|s| *s == chosen)
                .expect("the span answered is one of the column's");
            assert_eq!(
                Some(index as u64),
                row[1].as_u64(),
                "{name} column {i} ({tag}, {x}, {z}) at y {y}"
            );
            tally.rows += 1;
        }
        tally.columns += 1;
    }
    tally
}

/// Plan row W-02 acceptance 2: all four vector files through `compat::surface::column` and
/// `span_at`: the mesh-only fixtures every f32 bit, the canyon fixture span count, kinds and index.
#[test]
fn t3_vectors_match_the_compat_query() {
    for (name, every_bit) in FIXTURES {
        let tally = check(name, every_bit);
        eprintln!("T3VEC {name}: {tally:?}");
        assert!(
            tally.columns > 0 && tally.rows > tally.columns,
            "{name}: {tally:?}"
        );
        if !every_bit {
            // The copied sampler is the one the vectors were made with: the canyon heights match too.
            assert_eq!(
                tally.height_diffs, 0,
                "{name}: canyon heights differ in {} places",
                tally.height_diffs
            );
        }
    }
}
