// SPDX-License-Identifier: MPL-2.0
//! Full native query transcript over the unchanged T3 inputs (ADR-8246).
#![cfg(all(feature = "stage_b", feature = "stats"))]
#![allow(clippy::unwrap_used)]
pub mod native_support;
use native_support::*;
use regia_detsim::float::{det_max, det_min};
use regia_world_geom::{Band, FaceMask, QueryStats, SpanBuf, SurfaceHit};
use serde_json::Value;

fn hit(h: &mut blake3::Hasher, s: Option<SurfaceHit>) {
    h.update(&[u8::from(s.is_some())]);
    if let Some(s) = s {
        for f in [s.y, s.normal[0], s.normal[1], s.normal[2]] {
            h.update(&f.to_bits().to_le_bytes());
        }
        h.update(&[s.kind as u8]);
        h.update(&s.tri.map_or(u16::MAX, |t| t.layer).to_le_bytes());
        h.update(&s.tri.map_or(u32::MAX, |t| t.triangle).to_le_bytes());
        h.update(&s.section.map_or(u16::MAX, |s| s.0).to_le_bytes());
        h.update(&s.material.to_le_bytes());
    }
}
fn table(file: &Value, name: &str, cell: f32, pyramid: bool) -> String {
    let geom = native(file, name, cell);
    let state = geom.new_state();
    let mut hash = blake3::Hasher::new();
    // The original T3 input and expected rows remain part of the transcript, byte-for-byte in JSON values.
    hash.update(file.to_string().as_bytes());
    for row in file["columns"].as_array().unwrap() {
        let x = hex(&row["x"]);
        let z = hex(&row["z"]);
        let mut spans = SpanBuf::default();
        geom.floors_at(&state, x, z, &mut spans);
        hash.update(&(spans.spans().len() as u32).to_le_bytes());
        hash.update(&[u8::from(spans.overflowed)]);
        for s in spans.spans() {
            hit(&mut hash, Some(s.floor));
            hit(&mut hash, s.ceiling);
        }
        hit(&mut hash, Some(geom.place_floor(&state, x, z)));
        for at in row["at"].as_array().unwrap() {
            let y = hex(&at[0]);
            let s = geom.support(&state, x, z, y);
            hit(&mut hash, Some(s.floor));
            hit(&mut hash, s.ceiling);
            let raw = geom.surface_at(&state, x, z, y, 0.0);
            hit(&mut hash, raw.under);
            hit(&mut hash, raw.over);
            let p = geom.push_disc(
                &state,
                x,
                z,
                2.5,
                Band {
                    lo: s.floor.y + 0.5,
                    hi: y + 2.0,
                },
            );
            for f in p.xz {
                hash.update(&f.to_bits().to_le_bytes());
            }
            hash.update(&p.contacts.to_le_bytes());
            hash.update(&p.last.map_or(u32::MAX, |t| t.triangle).to_le_bytes());
            // ADR-8323 (W-30, QUERY_BITS_VERSION 8 per ADR-8531): the capsule push joins the table, a walker's band
            // from the selected floor.
            let (cp, cb) = geom.push_capsule(
                &state,
                x,
                z,
                0.35,
                Band {
                    lo: s.floor.y,
                    hi: s.floor.y + 1.85,
                },
            );
            for f in [cp.xz[0], cp.xz[1], cb.lo, cb.hi] {
                hash.update(&f.to_bits().to_le_bytes());
            }
            hash.update(&cp.contacts.to_le_bytes());
            let clamp = geom.clamp_under_ceiling_contact(&state, x, y, z, s.floor.y, 2.5);
            hash.update(&clamp.y.to_bits().to_le_bytes());
            hit(&mut hash, clamp.contact);
            let from = [x - 3.5, y + 1.0, z - 2.25];
            let to = [x + 12.0, y - 4.0, z + 11.5];
            let ray = if pyramid {
                geom.segment(&state, from, to, FaceMask::SOLID)
            } else {
                geom.segment_without_pyramid(&state, from, to, FaceMask::SOLID)
            };
            hash.update(&[u8::from(ray.is_some())]);
            if let Some(ray) = ray {
                for f in [ray.t, ray.point[0], ray.point[1], ray.point[2]] {
                    hash.update(&f.to_bits().to_le_bytes());
                }
                hash.update(&[ray.kind as u8]);
                hash.update(&ray.tri.map_or(u16::MAX, |t| t.layer).to_le_bytes());
                hash.update(&ray.tri.map_or(u32::MAX, |t| t.triangle).to_le_bytes());
            }
        }
    }
    hash.finalize().to_hex().to_string()
}
#[test]
fn query_table_is_the_same_at_4m_and_16m_cells() {
    for name in FIXTURES {
        let f = read(name);
        assert_eq!(table(&f, name, 4.0, true), table(&f, name, 16.0, true), "{name}");
    }
}
#[test]
fn query_table_is_the_same_with_and_without_the_pyramid() {
    for name in FIXTURES {
        let f = read(name);
        assert_eq!(
            table(&f, name, 16.0, true),
            table(&f, name, 16.0, false),
            "{name}"
        );
    }
}
#[test]
fn query_table_equal_on_every_arch() {
    // QUERY_BITS_VERSION 7 (ADR-8323, W-30): re-recorded on x86-64 Linux (the build server, 2026-11-17) because the
    // capsule push rows joined the transcript; every earlier query is unchanged (the 4 m / 16 m and pyramid
    // tests above still agree). The version-6 pins were f8b07dde, 6c7a06a3, 53c0c9c4 and 108e7296.
    // `switchbacks_canyon` re-recorded on x86-64 Linux (the test machine, lane tunnels-stage-b-correctness,
    // ADR-8898): its cook cuts the whole K8 skylight climbs, so the queries over them answer the cut
    // (intended; was f5f76977). The three mesh rows are unchanged.
    let pins = [
        "cbe1bbc2fd1f909a46a8e83ba4ae759e6cd8805d5e94cd672ccb3bdadbcb2d25",
        "784a71fb6fa2d46912aeda5669299d76cd7ad1555003be1402fe87e407d5aa31",
        "962477c5efa350bb49cf4a818acc1fca525e1972045a7ea95f82628e53a5844d",
        "d2bcba18fb9c8891ab7ae2ea91a8e961b7544e415cf9059ac3f357d08e51cf8f",
    ];
    let mut mismatches = Vec::new();
    for (name, pin) in FIXTURES.into_iter().zip(pins) {
        let f = read(name);
        let actual = table(&f, name, 16.0, true);
        eprintln!("NATIVE_TABLE {name} {actual}");
        if actual != pin {
            mismatches.push((name, actual, pin));
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:?}");
}
#[test]
fn switchbacks_column_tests_le_24_triangles_p99() {
    for name in ["switchbacks_mesh", "switchbacks_canyon"] {
        let f = read(name);
        let geom = native(&f, name, 16.0);
        let state = geom.new_state();
        let mut counts = Vec::new();
        let mut spans = SpanBuf::default();
        for row in f["columns"].as_array().unwrap() {
            let mut stats = QueryStats::default();
            geom.floors_at_with_stats(&state, hex(&row["x"]), hex(&row["z"]), &mut spans, &mut stats);
            counts.push(stats.triangle_tests);
        }
        counts.sort_unstable();
        let p99 = counts[(counts.len() - 1) * 99 / 100];
        eprintln!("NATIVE_STATS {name} p99={p99} max={}", counts[counts.len() - 1]);
        assert!(p99 <= 24, "{name}: p99 {p99} exceeds 24");
    }
}

#[test]
fn randomized_cell_and_pyramid_transcript_matches() {
    let mut seed = 0xC0FFEE_u64;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed
    };
    for name in FIXTURES {
        let file = read(name);
        let four = native(&file, name, 4.0);
        let sixteen = native(&file, name, 16.0);
        let (mut min_x, mut max_x, mut min_z, mut max_z) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for tri in sixteen.triangles() {
            for p in tri.v {
                min_x = det_min(min_x, p[0]);
                max_x = det_max(max_x, p[0]);
                min_z = det_min(min_z, p[2]);
                max_z = det_max(max_z, p[2]);
            }
        }
        let state4 = four.new_state();
        let state16 = sixteen.new_state();
        for _ in 0..32 {
            let u = (next() % 10_000) as f32 / 10_000.0;
            let v = (next() % 10_000) as f32 / 10_000.0;
            let x = min_x + (max_x - min_x) * u;
            let z = min_z + (max_z - min_z) * v;
            let band = regia_world_geom::Band { lo: -2.0, hi: 4.0 };
            assert_eq!(
                four.push_disc(&state4, x, z, 1.5, band),
                sixteen.push_disc(&state16, x, z, 1.5, band),
                "{name} push {x} {z}"
            );
            let from = [x, 2.0, z];
            let to = [x + 6.0, -4.0, z + 5.0];
            assert_eq!(
                sixteen.segment(&state16, from, to, FaceMask::SOLID),
                sixteen.segment_without_pyramid(&state16, from, to, FaceMask::SOLID),
                "{name} pyramid {x} {z}"
            );
            assert_eq!(
                four.segment(&state4, from, to, FaceMask::SOLID),
                sixteen.segment(&state16, from, to, FaceMask::SOLID),
                "{name} cell {x} {z}"
            );
        }
    }
}
#[test]
fn every_wall_inside_the_pad_is_visited() {
    let mut seed = 0xA11CE_u64;
    for name in FIXTURES {
        let file = read(name);
        let geom = native(&file, name, 16.0);
        let state = geom.new_state();
        let (mut min_x, mut max_x, mut min_z, mut max_z) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for tri in geom.triangles() {
            for p in tri.v {
                min_x = det_min(min_x, p[0]);
                max_x = det_max(max_x, p[0]);
                min_z = det_min(min_z, p[2]);
                max_z = det_max(max_z, p[2]);
            }
        }
        let mut missed = 0u32;
        for i in 0..4096 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let u = (seed % 10_000) as f32 / 10_000.0;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let v = (seed % 10_000) as f32 / 10_000.0;
            let x = min_x + (max_x - min_x) * u;
            let z = min_z + (max_z - min_z) * v;
            let from = [x, 8.0, z];
            let to = [x + ((i % 7) as f32 - 3.0), -12.0, z + ((i % 5) as f32 - 2.0)];
            let (visited, scan) = geom.wall_visit_and_scan(from, to);
            let queried = geom.segment(&state, from, to, FaceMask::WALL);
            assert_eq!(
                queried.map(|h| (h.t.to_bits(), h.tri)),
                scan.map(|h| (h.t.to_bits(), h.tri))
            );
            if let Some(hit) = scan
                && let Some(tri) = hit.tri
                && !visited.iter().any(|&id| geom.tri(id).tri == tri)
            {
                missed += 1;
            }
        }
        assert_eq!(missed, 0, "{name}");
    }
}
#[test]
fn query_bits_version_matches_the_query_table() {
    assert_eq!(
        regia_world_geom::QUERY_BITS_VERSION,
        8,
        "native transcript v6, unchanged at v7 (W-13: blocker plugs move answers only while active) and v8 (push_capsule joined, ADR-8323, ADR-8531); changes require a monotonic bit version and new reviewed pins"
    );
}
