// SPDX-License-Identifier: MPL-2.0
//! Road decal and ribbon geometry (spec 26 § 1): the point-to-segment test against closed-form distances, and the
//! ribbon mesh against the area and offsets a strip of known width must have. CPU only, no golden.
//!
//! Reference: the clamped point-to-segment distance (any computational-geometry text); the area of a strip of
//! half-width w along a polyline of straight pieces of total length L is 2·w·L when the pieces do not overlap.

use regia_terrain::sampler::TerrainMapError;
use regia_terrain::{RoadDecal, RoadRibbon, TerrainMap, on_segment};

fn flat_map(world: f32) -> TerrainMap {
    TerrainMap::from_heights(2, 2, vec![0.0; 4], world, 1.0, 0.0).expect("flat map")
}

/// Area of the triangle list projected on XZ, summing absolute triangle areas.
fn xz_area(vertices: &[[f32; 4]], indices: &[u32]) -> f32 {
    indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let [a, b, c] = [t[0], t[1], t[2]].map(|i| vertices[i as usize]);
            0.5 * ((b[0] - a[0]) * (c[2] - a[2]) - (b[2] - a[2]) * (c[0] - a[0])).abs()
        })
        .sum()
}

#[test]
fn on_segment_matches_the_point_to_segment_distance_everywhere() {
    let (a, b) = ([2.0, -1.0], [8.0, 7.0]);
    let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dz * dz;
    let half = 1.3;
    for ix in -4..16 {
        for iz in -6..14 {
            let (x, z) = (ix as f32 * 0.77 + 0.13, iz as f32 * 0.69 - 0.21);
            let t = (((x - a[0]) * dx + (z - a[1]) * dz) / len2).clamp(0.0, 1.0);
            let d = ((x - (a[0] + t * dx)).powi(2) + (z - (a[1] + t * dz)).powi(2)).sqrt();
            // Stay clear of the boundary, where f32 rounding may legitimately flip the answer.
            if (d - half).abs() < 1e-3 {
                continue;
            }
            assert_eq!(on_segment(a, b, half, x, z), d < half, "({x}, {z}) distance {d}");
        }
    }
}

#[test]
fn on_segment_boundary_is_open_and_degenerate_segments_are_discs() {
    // The test is strict: a point exactly half_width away is outside.
    assert!(!on_segment([0.0, 0.0], [10.0, 0.0], 1.0, 5.0, 1.0));
    assert!(on_segment([0.0, 0.0], [10.0, 0.0], 1.0, 5.0, 0.999));
    // A zero-length segment is a disc around its point.
    assert!(on_segment([3.0, 3.0], [3.0, 3.0], 1.0, 3.5, 3.5));
    assert!(!on_segment([3.0, 3.0], [3.0, 3.0], 1.0, 4.0, 4.0));
    // Zero and negative widths disable the decal, even on the line itself.
    assert!(!on_segment([0.0, 0.0], [10.0, 0.0], 0.0, 5.0, 0.0));
    assert!(!on_segment([0.0, 0.0], [10.0, 0.0], -1.0, 5.0, 0.0));
    // The segment is directed neither way.
    assert_eq!(
        on_segment([0.0, 0.0], [10.0, 4.0], 1.0, 6.0, 2.0),
        on_segment([10.0, 4.0], [0.0, 0.0], 1.0, 6.0, 2.0)
    );
}

#[test]
fn decal_constructor_stores_its_fields() {
    let d = RoadDecal::new([1.0, 2.0], [3.0, 4.0], 0.5, [0.1, 0.2, 0.3]);
    assert_eq!(
        (d.a, d.b, d.half_width, d.color),
        ([1.0, 2.0], [3.0, 4.0], 0.5, [0.1, 0.2, 0.3])
    );
    assert_eq!(d, d);
}

#[test]
fn a_straight_ribbon_has_the_area_and_edges_of_a_strip() {
    let map = flat_map(200.0);
    let line = [[-20.0, 0.0], [0.0, 0.0], [20.0, 0.0]];
    let ribbon = RoadRibbon::conform(&map, &line, 3.0, 0.05).expect("ribbon");
    assert_eq!(ribbon.vertices.len(), 6);
    assert_eq!(ribbon.indices.len(), 12);
    assert_eq!(ribbon.lift_m, 0.05);
    // Every vertex is 3 m to one side of the centreline and `lift` above the (flat) ground.
    for pair in ribbon.vertices.chunks(2) {
        assert!((pair[0][2] - 3.0).abs() < 1e-5 || (pair[0][2] + 3.0).abs() < 1e-5);
        assert!(
            (pair[0][2] + pair[1][2]).abs() < 1e-5,
            "the pair straddles the centreline"
        );
        assert!((pair[0][1] - 0.05).abs() < 1e-5 && pair[0][3] == 1.0);
    }
    // Strip area 2·w·L = 2 · 3 · 40.
    let area = xz_area(&ribbon.vertices, &ribbon.indices);
    assert!((area - 240.0).abs() < 1e-2, "area {area}");
    assert!(
        ribbon
            .indices
            .iter()
            .all(|&i| (i as usize) < ribbon.vertices.len())
    );
    // All triangles wind the same way, so the strip faces one side.
    let signs: Vec<f32> = ribbon
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let [a, b, c] = [t[0], t[1], t[2]].map(|i| ribbon.vertices[i as usize]);
            (b[0] - a[0]) * (c[2] - a[2]) - (b[2] - a[2]) * (c[0] - a[0])
        })
        .collect();
    assert!(
        signs
            .iter()
            .all(|s| s.signum() == signs[0].signum() && s.abs() > 1e-3),
        "{signs:?}"
    );
}

#[test]
fn ribbon_edges_are_perpendicular_to_the_local_tangent() {
    let map = flat_map(200.0);
    // A diagonal run: at the ends the tangent is the one-sided segment, in the middle the central difference.
    let line = [[0.0, 0.0], [4.0, 4.0], [8.0, 8.0]];
    let ribbon = RoadRibbon::conform(&map, &line, 1.0, 0.02).expect("ribbon");
    for (k, pair) in ribbon.vertices.chunks(2).enumerate() {
        let (l, r) = (pair[0], pair[1]);
        let across = [l[0] - r[0], l[2] - r[2]];
        assert!(
            (across[0].hypot(across[1]) - 2.0).abs() < 1e-5,
            "vertex pair {k} spans the full width"
        );
        let along = [1.0 / 2f32.sqrt(), 1.0 / 2f32.sqrt()];
        assert!(
            (across[0] * along[0] + across[1] * along[1]).abs() < 1e-5,
            "pair {k} is not perpendicular"
        );
        let mid = [(l[0] + r[0]) * 0.5, (l[2] + r[2]) * 0.5];
        assert!((mid[0] - line[k][0]).abs() < 1e-5 && (mid[1] - line[k][1]).abs() < 1e-5);
    }
}

#[test]
fn ribbon_follows_a_slope() {
    // Heights [0, 2; 0, 2] over 2 m rise 2 m per 2 m in +X (scale 2 m).
    let map = TerrainMap::from_heights(2, 2, vec![0.0, 2.0, 0.0, 2.0], 2.0, 2.0, 0.0).expect("map");
    let line = [[-0.5, 0.0], [0.5, 0.0]];
    let ribbon = RoadRibbon::conform(&map, &line, 0.2, 0.1).expect("ribbon");
    for v in &ribbon.vertices {
        let y = map.sample_bilinear(v[0], v[2]) + 0.1;
        assert!((v[1] - y).abs() < 1e-4);
    }
    let (lo, hi) = (ribbon.vertices[0][1], ribbon.vertices[2][1]);
    assert!(hi > lo + 0.5, "the ribbon climbs with the ground ({lo} -> {hi})");
}

#[test]
fn conform_refuses_bad_centrelines_widths_and_lifts() {
    let map = flat_map(100.0);
    let line = [[0.0, 0.0], [5.0, 0.0]];
    let ok = RoadRibbon::conform(&map, &line, 1.0, 0.02);
    assert!(ok.is_ok());
    // The documented band for the lift is 0.01..=0.25, both ends inclusive.
    assert!(RoadRibbon::conform(&map, &line, 1.0, 0.01).is_ok());
    assert!(RoadRibbon::conform(&map, &line, 1.0, 0.25).is_ok());
    for lift in [0.0099, 0.2501, f32::NAN, f32::INFINITY, -0.1] {
        let err = RoadRibbon::conform(&map, &line, 1.0, lift).expect_err("lift outside the band");
        assert!(
            matches!(err, TerrainMapError::BadHeightParams { .. }),
            "lift {lift}: {err:?}"
        );
    }
    // Too short, non-finite points, bad widths.
    assert!(matches!(
        RoadRibbon::conform(&map, &line[..1], 1.0, 0.02),
        Err(TerrainMapError::BadWorldWidth(_))
    ));
    assert!(RoadRibbon::conform(&map, &[], 1.0, 0.02).is_err());
    assert!(RoadRibbon::conform(&map, &[[0.0, 0.0], [f32::NAN, 1.0]], 1.0, 0.02).is_err());
    assert!(RoadRibbon::conform(&map, &[[f32::INFINITY, 0.0], [1.0, 1.0]], 1.0, 0.02).is_err());
    for width in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert!(
            RoadRibbon::conform(&map, &line, width, 0.02).is_err(),
            "half-width {width}"
        );
    }
}

#[test]
fn a_repeated_centreline_point_does_not_produce_nan_vertices() {
    // The tangent of a zero-length piece is clamped, so the vertices stay finite.
    let map = flat_map(100.0);
    let ribbon = RoadRibbon::conform(&map, &[[1.0, 1.0], [1.0, 1.0]], 1.0, 0.02).expect("ribbon");
    assert!(ribbon.vertices.iter().all(|v| v.iter().all(|c| c.is_finite())));
}
