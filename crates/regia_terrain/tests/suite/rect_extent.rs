// SPDX-License-Identifier: MPL-2.0
//! A terrain map whose world depth is not its world width (ADR-8369).
//!
//! The sample grid stays `cols × rows`. The host sets the Z extent with
//! `TerrainMap::with_world_depth` after load. A square map (no call) is unchanged.

use regia_terrain::sampler::TerrainMapError;
use regia_terrain::{CdlodConfig, DirtyRect, MinMaxPyramid, PatchTarget, QuadNode, TerrainMap, select_nodes};

/// 4100 × 1540 samples, 0.3125 m apart on both axes. Corner and one interior sample are set
/// so a bilinear read can prove the buffer was not padded out to a square.
fn rect_4100() -> TerrainMap {
    let (cols, rows, spacing) = (4100u32, 1540u32, 0.3125f32);
    let mut heights = vec![0.0; (cols * rows) as usize];
    let at = |c, r| (r * cols + c) as usize;
    heights[at(0, 0)] = 1.0;
    heights[at(cols - 1, 0)] = 2.0;
    heights[at(0, rows - 1)] = 3.0;
    heights[at(cols - 1, rows - 1)] = 4.0;
    heights[at(100, 50)] = 5.0;
    TerrainMap::from_heights(cols, rows, heights, (cols - 1) as f32 * spacing, 1.0, 0.0)
        .and_then(|m| m.with_world_depth((rows - 1) as f32 * spacing))
        .expect("map")
}

/// The acceptance case: a 4100 × 1540 heightfield keeps that sample count and a rectangular
/// world extent. A point in the band a square pad would have covered is outside the map,
/// and the collider reports no height there.
#[test]
fn _4100x1540_map_samples_without_padding() {
    let (cols, rows, spacing) = (4100u32, 1540u32, 0.3125f32);
    let map = rect_4100();
    let samples = (cols as usize) * (rows as usize);
    assert_eq!(map.heights().len(), samples);
    assert_ne!(map.heights().len(), (cols as usize) * (cols as usize));
    assert_eq!(map.world_width(), (cols - 1) as f32 * spacing);
    assert_eq!(map.world_depth(), (rows - 1) as f32 * spacing);
    assert_eq!(map.world_extent(), [1280.9375, 480.9375]);
    assert_eq!(map.sample_spacing_z(), map.sample_spacing());
    assert_eq!(map.sample_spacing(), spacing);
    assert_eq!(map.world_min(), [-640.46875, -240.46875]);
    assert_eq!(map.world_max(), [640.46875, 240.46875]);

    // Without the depth call the same grid is stretched over the square: sample (0, 0)
    // would sit at z = −world_width / 2.
    let stretched_z = -map.world_width() * 0.5;
    assert_eq!(stretched_z, -640.46875);
    assert_eq!(map.sample_to_world(0, 0), [-640.46875, -240.46875]);
    assert_eq!(map.sample_to_world(cols - 1, rows - 1), [640.46875, 240.46875]);
    assert_ne!(map.sample_to_world(0, 0)[1], stretched_z);

    let corners: [(u32, u32, f32); 4] = [
        (0, 0, 1.0),
        (cols - 1, 0, 2.0),
        (0, rows - 1, 3.0),
        (cols - 1, rows - 1, 4.0),
    ];
    for (c, r, h) in corners {
        let [x, z] = map.sample_to_world(c, r);
        assert_eq!(map.sample_bilinear(x, z).to_bits(), h.to_bits(), "({c}, {r})");
        assert_eq!(
            map.height_at_sample(i64::from(c), i64::from(r)).to_bits(),
            h.to_bits()
        );
    }
    assert_eq!(map.height_at_sample(100, 50).to_bits(), 5.0f32.to_bits());

    // z = −400 is inside a square of side (cols − 1) · spacing and outside this rectangle.
    let pad_z = -400.0f32;
    let square_half = map.world_width() * 0.5;
    assert!(pad_z > -square_half && pad_z < square_half);
    let [_, gz] = map.world_to_grid(0.0, pad_z);
    assert!(gz < 0.0, "pad-band row {gz} should be outside the grid");
    let [gx, _] = map.world_to_grid(0.0, 0.0);
    assert!((0.0..(cols - 1) as f32).contains(&gx));

    let collider = map.collision_heightfield(None);
    assert_eq!(collider.scale, [map.world_width(), 1.0, map.world_depth()]);
    assert_eq!(collider.heights.len(), samples);
    assert!(
        collider.height_at(0.0, pad_z).is_none(),
        "no collision in the pad band"
    );
    assert!(collider.height_at(0.0, 0.0).is_some());

    let (foot_min, _) = PatchTarget::of(&map).footprint(&DirtyRect {
        col_start: 0,
        col_end: 1,
        row_start: 0,
        row_end: 1,
    });
    assert_eq!(foot_min, map.world_min());
}

/// A map that never calls `with_world_depth` stays square, including a non-square sample grid
/// stretched over one `world_width`.
#[test]
fn default_world_depth_matches_world_width() {
    let square = TerrainMap::from_heights(5, 5, vec![0.0; 25], 8.0, 1.0, 0.0).expect("map");
    assert_eq!(square.world_depth(), square.world_width());
    assert!(!square.is_rectangular());
    assert_eq!(square.quadtree_width(), 8.0);
    assert_eq!(square.sample_to_world(4, 4), [4.0, 4.0]);

    let stretched = TerrainMap::from_heights(5, 3, vec![0.0; 15], 8.0, 1.0, 0.0).expect("map");
    assert_eq!(stretched.world_depth(), 8.0);
    assert_eq!(stretched.sample_to_world(4, 2), [4.0, 4.0]);
}

/// Non-finite and non-positive depths are refused. The map is unchanged on the error path
/// because the builder takes `self` by value only after the check passes.
#[test]
fn bad_world_depth_is_refused() {
    let map = TerrainMap::from_heights(4, 4, vec![0.0; 16], 8.0, 1.0, 0.0).expect("map");
    for depth in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let err = map.clone().with_world_depth(depth).expect_err("refused");
        assert!(
            matches!(err, TerrainMapError::BadWorldDepth(got) if got.to_bits() == depth.to_bits()),
            "{depth}: {err}"
        );
    }
    assert_eq!(map.world_depth(), 8.0);
}

/// Nodes wholly past the shorter side are not selected. A vertex of a node that straddles
/// that side is clamped onto the map edge.
#[test]
fn rectangular_selection_skips_the_overhang_and_clamps_straddling_vertices() {
    let map = TerrainMap::from_heights(17, 5, vec![1.0; 17 * 5], 16.0, 1.0, 0.0)
        .and_then(|m| m.with_world_depth(4.0))
        .expect("map");
    assert_eq!(map.quadtree_width(), 16.0);
    assert_eq!(map.world_min(), [-8.0, -2.0]);
    let root = 2u32;
    let config = CdlodConfig {
        grid_size: 4,
        range_factor: 64.0,
        morph_fraction: 0.25,
    };
    let pyramid = MinMaxPyramid::build(&map, root).expect("pyramid");
    let sel = select_nodes(&map, &pyramid, &config, root, [0.0, 0.0], None);
    assert_eq!(
        sel.nodes.len(),
        8,
        "four columns × the two rows that meet the map"
    );
    for draw in &sel.nodes {
        assert!(draw.node.overlaps_map(&map, root), "{:?}", draw.node);
        assert_eq!(draw.node.level, 0);
        assert!(draw.node.z == 1 || draw.node.z == 2, "pad row {:?}", draw.node);
        let (_x, z, size) = draw.node.map_rect(&map, root);
        assert!(z < map.world_max()[1] && z + size > map.world_min()[1]);
    }
    let outside = [
        QuadNode { level: 0, x: 0, z: 0 },
        QuadNode { level: 0, x: 3, z: 3 },
    ];
    for node in outside {
        assert!(!node.overlaps_map(&map, root), "{node:?}");
        assert!(sel.nodes.iter().all(|d| d.node != node));
    }
    // Node (x=2, z=1) spans z −4..0. Its −Z edge is past the map, so the vertex clamps to −2.
    let node = QuadNode { level: 0, x: 2, z: 1 };
    let [x, h, z] = regia_terrain::cdlod::eval_node_vertex(&map, &config, node, root, 0, 0, 0.0);
    assert_eq!(z, map.world_min()[1]);
    assert_eq!(x, 0.0);
    assert_eq!(h, 1.0);
}
