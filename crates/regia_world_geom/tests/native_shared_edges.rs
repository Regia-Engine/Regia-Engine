// SPDX-License-Identifier: MPL-2.0
//! Canonical ownership under reversal of source triangle order in every T3 fixture.
#![cfg(feature = "stage_b")]
#![allow(clippy::unwrap_used)]
pub mod native_support;
use native_support::*;
use regia_world_geom::{SpanBuf, SurfaceHit, WorldGeom};
use std::collections::BTreeMap;
fn physical(
    h: SurfaceHit,
) -> (
    [u32; 4],
    regia_world_geom::SurfaceKind,
    Option<regia_world_geom::SectionId>,
    u16,
) {
    (
        [
            h.y.to_bits(),
            h.normal[0].to_bits(),
            h.normal[1].to_bits(),
            h.normal[2].to_bits(),
        ],
        h.kind,
        h.section,
        h.material,
    )
}
#[test]
fn every_fixture_shared_edge_is_independent_of_triangle_order() {
    let mut total = 0;
    let mut differences = 0;
    for name in FIXTURES {
        let file = read(name);
        let first = native(&file, name, 16.0);
        let mut cooked = first.cooked().clone();
        let mut maxima = BTreeMap::<u16, u32>::new();
        for tri in cooked.tiles.iter().flat_map(|t| &t.triangles) {
            maxima
                .entry(tri.layer)
                .and_modify(|m| *m = (*m).max(tri.index))
                .or_insert(tri.index);
        }
        for tri in cooked.tiles.iter_mut().flat_map(|t| &mut t.triangles) {
            tri.index = maxima[&tri.layer] - tri.index;
        }
        for tile in &mut cooked.tiles {
            tile.triangles.sort_by_key(|t| (t.layer, t.index));
        }
        let second =
            WorldGeom::from_canonical_bytes(&regia_world_geom::cooked::encode_canonical(&cooked).unwrap())
                .unwrap();
        let states = (first.new_state(), second.new_state());
        let mut edges = BTreeMap::new();
        for tri in first.triangles() {
            if !matches!(
                tri.kind,
                regia_world_geom::cooked::TriKind::Floor | regia_world_geom::cooked::TriKind::Roof
            ) {
                continue;
            }
            for i in 0..3 {
                let a = tri.v[i].map(regia_world_geom::m_to_mm);
                let b = tri.v[(i + 1) % 3].map(regia_world_geom::m_to_mm);
                let key = if a < b {
                    (tri.tri.layer, a, b)
                } else {
                    (tri.tri.layer, b, a)
                };
                *edges.entry(key).or_insert(0usize) += 1;
            }
        }
        let mut probes = 0;
        let mut misses = 0;
        for ((_, a, b), count) in edges {
            if count < 2 {
                continue;
            }
            let a = a.map(|v| v as f32 / 1000.0);
            let b = b.map(|v| v as f32 / 1000.0);
            if a[0] == b[0] && a[2] == b[2] {
                continue;
            }
            for t in [0.0, 0.5, 1.0] {
                let x = a[0] + (b[0] - a[0]) * t;
                let z = a[2] + (b[2] - a[2]) * t;
                let y = a[1] + (b[1] - a[1]) * t + 0.9;
                let mut left = SpanBuf::default();
                let mut right = SpanBuf::default();
                first.floors_at(&states.0, x, z, &mut left);
                second.floors_at(&states.1, x, z, &mut right);
                let project = |s: &regia_world_geom::Span| (physical(s.floor), s.ceiling.map(physical));
                // The reversed cook stores `max - index`. Every TriRef, not only the selected floor,
                // maps back through that permutation (ADR-8246 R4).
                let maps =
                    |a: Option<regia_world_geom::TriRef>, b: Option<regia_world_geom::TriRef>| match (a, b) {
                        (Some(l), Some(r)) => {
                            l.layer == r.layer
                                && maxima
                                    .get(&l.layer)
                                    .is_some_and(|m| l.triangle + r.triangle == *m)
                        }
                        (None, None) => true,
                        _ => false,
                    };
                let pair = |l: Option<SurfaceHit>, r: Option<SurfaceHit>| match (l, r) {
                    (Some(l), Some(r)) => maps(l.tri, r.tri),
                    (None, None) => true,
                    _ => false,
                };
                let support_l = first.support(&states.0, x, z, y);
                let support_r = second.support(&states.1, x, z, y);
                let refs_map = left.spans().len() == right.spans().len()
                    && left
                        .spans()
                        .iter()
                        .zip(right.spans())
                        .all(|(l, r)| maps(l.floor.tri, r.floor.tri) && pair(l.ceiling, r.ceiling))
                    && maps(support_l.floor.tri, support_r.floor.tri)
                    && pair(support_l.ceiling, support_r.ceiling);
                let same = left
                    .spans()
                    .iter()
                    .map(project)
                    .eq(right.spans().iter().map(project))
                    && project(&support_l) == project(&support_r)
                    && refs_map;
                probes += 1;
                if !same {
                    misses += 1;
                    if misses == 1 {
                        eprintln!(
                            "EDGE_ORDER_WITNESS {name} x={x} z={z} y={y} left={left:?} right={right:?}"
                        );
                    }
                }
            }
        }
        eprintln!("SHARED_EDGES {name} probes={probes} differences={misses}");
        total += probes;
        differences += misses;
    }
    assert!(total > 1000);
    assert_eq!(differences, 0);
}
