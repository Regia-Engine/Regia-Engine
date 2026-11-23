// SPDX-License-Identifier: MPL-2.0
//! Terrain parity end to end (ADR-873): after sculpting (strokes that run off the map edge),
//! craters and undo, the drawn CDLOD selection stays crack-free from every eye (camera and
//! shadow-cascade selections), the collider built from the map is the finest drawn grid, the
//! min/max pyramid matches a rebuild, and undo restores the heights bit for bit. CPU only.

use glam::Vec2;
use regia_core::math::{Frustum, Rng};
use regia_terrain::{
    Brush, BrushMode, BrushShape, CdlodConfig, Crater, HeightMips, MinMaxPyramid, StrokeUndo, Terrain,
    TerrainEdits, TerrainLayers, TerrainMap, apply_crater, apply_stroke, lod_seam_report, select_nodes,
    select_nodes_min_level, terrain_surface_height,
};

const ROOT_LEVEL: u32 = 5;

/// 257 × 257 samples over 256 m (1 m spacing), rolling ground around 100 m.
fn terrain() -> Terrain {
    let n = 257u32;
    let heights = (0..n * n)
        .map(|i| {
            let (x, z) = ((i % n) as f32, (i / n) as f32);
            100.0 + 7.0 * (x * 0.043).sin() * (z * 0.031).cos() + 2.0 * ((x - z) * 0.17).sin()
        })
        .collect();
    let map = TerrainMap::from_heights(n, n, heights, 256.0, 400.0, 0.0).expect("map");
    Terrain::new(
        &map.to_heightfield(),
        256.0,
        400.0,
        0.0,
        CdlodConfig::with_grid_size(16),
        ROOT_LEVEL,
        TerrainLayers::default(),
    )
    .expect("terrain")
}

fn brush(shape: BrushShape, mode: BrushMode) -> Brush {
    Brush {
        shape,
        mode,
        hardness: 0.5,
        ..Brush::default()
    }
}

/// Sculpts a corner bump that runs off the map, a trench, a flatten and a smooth.
fn sculpt(t: &mut Terrain, edits: &mut TerrainEdits) -> Vec<StrokeUndo> {
    let strokes = [
        (
            brush(
                BrushShape::Circle { radius: 18.0 },
                BrushMode::Raise { amount: 35.0 },
            ),
            vec![Vec2::new(-118.0, -121.0), Vec2::new(-150.0, -140.0)],
        ),
        (
            brush(
                BrushShape::Rectangle {
                    width: 6.0,
                    length: 40.0,
                },
                BrushMode::Lower { amount: 9.0 },
            ),
            vec![Vec2::new(20.0, -30.0), Vec2::new(24.0, 40.0)],
        ),
        (
            brush(BrushShape::Circle { radius: 10.0 }, BrushMode::Flatten),
            vec![Vec2::new(-40.0, 60.0)],
        ),
        (
            brush(
                BrushShape::Circle { radius: 12.0 },
                BrushMode::Smooth { kernel_radius: 2 },
            ),
            vec![Vec2::new(22.0, 0.0), Vec2::new(22.0, 20.0)],
        ),
        (
            // Off the map entirely: a no-op, not an error.
            brush(
                BrushShape::Circle { radius: 5.0 },
                BrushMode::Raise { amount: 5.0 },
            ),
            vec![Vec2::new(500.0, 500.0)],
        ),
    ];
    strokes
        .iter()
        .map(|(b, path)| apply_stroke(t, None, edits, b, path).expect("stroke"))
        .collect()
}

fn assert_minmax_matches_rebuild(t: &Terrain) {
    let rebuilt = MinMaxPyramid::build(&t.map, t.root_level).expect("pyramid");
    assert_eq!(
        format!("{:?}", t.minmax),
        format!("{rebuilt:?}"),
        "incremental min/max equals a rebuild"
    );
}

/// Every eye in `eyes`: the camera selection and the shadow selections (coarsest level 1
/// and 2) are crack-free.
fn assert_crack_free(t: &Terrain, eyes: &[[f32; 2]]) {
    let mips = HeightMips::build(&t.map);
    let mut level_changes = 0;
    for &eye in eyes {
        let camera = select_nodes(&t.map, &t.minmax, &t.cdlod, t.root_level, eye, None);
        let report = lod_seam_report(&t.map, &mips, &t.cdlod, t.root_level, &camera, eye);
        assert!(report.is_crack_free(1e-4), "camera eye {eye:?}: {report:?}");
        assert_eq!(report.coarse_edge_morphs, 0, "eye {eye:?}: {report:?}");
        level_changes += report.level_change_edges;
        for min_level in [1, 2] {
            let cascade =
                select_nodes_min_level(&t.map, &t.minmax, &t.cdlod, t.root_level, eye, None, min_level);
            let report = lod_seam_report(&t.map, &mips, &t.cdlod, t.root_level, &cascade, eye);
            assert!(
                report.is_crack_free(1e-4),
                "cascade (min level {min_level}) eye {eye:?}: {report:?}"
            );
        }
    }
    assert!(level_changes > 0, "the eyes must exercise level changes");
}

fn eyes(seed: u64, count: usize) -> Vec<[f32; 2]> {
    let mut rng = Rng::new(seed);
    let mut out = vec![[-118.0, -121.0], [20.0, 0.0], [0.0, 0.0]];
    out.extend((0..count).map(|_| [rng.range(-200.0..200.0), rng.range(-200.0..200.0)]));
    out
}

#[test]
fn sculpted_terrain_stays_crack_free_collides_where_drawn_and_undoes_exactly() {
    let mut t = terrain();
    let original = t.map.heights().to_vec();
    let mut edits = TerrainEdits::default();
    let undos = sculpt(&mut t, &mut edits);
    assert!(
        undos[..4].iter().all(|u| !u.is_empty()),
        "the on-map strokes changed heights"
    );
    assert!(undos[4].is_empty(), "the off-map stroke changed nothing");
    assert_ne!(t.map.heights(), original.as_slice());
    assert_minmax_matches_rebuild(&t);
    assert_crack_free(&t, &eyes(41, 30));

    // The collider is the finest drawn grid, edits included.
    let collider = t.map.collision_heightfield(None);
    let mut rng = Rng::new(9);
    for _ in 0..2000 {
        let x = rng.range(-127.9..127.9);
        let z = rng.range(-127.9..127.9);
        let c = collider.height_at(x, z).expect("inside");
        let drawn = terrain_surface_height(&t.map, t.map.sample_spacing(), x, z);
        assert!((c - drawn).abs() < 1e-3, "({x}, {z}): collider {c} drawn {drawn}");
    }
    // In-range sculpting saves within half a 16-bit step.
    let drift = t.map.save_drift();
    assert!(drift.round_trips(), "{drift:?}");

    // Undo in reverse restores every height bit for bit, and the pyramid with it.
    for undo in undos.iter().rev() {
        undo.restore(&mut t, None, None, &mut edits, true);
    }
    assert!(
        t.map
            .heights()
            .iter()
            .zip(&original)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "undo is bit-exact"
    );
    assert_minmax_matches_rebuild(&t);
}

#[test]
fn craters_keep_the_seams_closed() {
    let mut t = terrain();
    for (center, radius, depth) in [
        ([0.0, 0.0], 9.0, 4.0),
        ([-127.0, 64.0], 14.0, 6.0),
        ([90.0, -90.0], 25.0, 10.0),
    ] {
        let rect = apply_crater(
            &mut t.map,
            &Crater {
                center,
                radius,
                depth,
                rim_height: depth * 0.3,
            },
        )
        .expect("crater");
        assert!(!rect.is_empty());
        t.minmax.update_region(&t.map, &rect);
    }
    assert_minmax_matches_rebuild(&t);
    assert_crack_free(&t, &eyes(77, 20));
}

#[test]
fn a_culled_selection_checks_only_the_drawn_neighbours() {
    let t = terrain();
    let mips = HeightMips::build(&t.map);
    let eye3 = glam::Vec3::new(-60.0, 140.0, -60.0);
    // The same camera helpers as `cdlod`'s frustum test.
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        eye3,
        glam::Vec3::new(40.0, 100.0, 40.0),
        glam::Vec3::Y,
    );
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(1.0f32, 1.6, 0.5, 2000.0);
    let frustum = Frustum::from_view_projection(&(proj * view));
    let eye = [eye3.x, eye3.z];
    let all = select_nodes(&t.map, &t.minmax, &t.cdlod, t.root_level, eye, None);
    let culled = select_nodes(&t.map, &t.minmax, &t.cdlod, t.root_level, eye, Some(&frustum));
    assert!(culled.nodes.len() < all.nodes.len());
    let full = lod_seam_report(&t.map, &mips, &t.cdlod, t.root_level, &all, eye);
    let part = lod_seam_report(&t.map, &mips, &t.cdlod, t.root_level, &culled, eye);
    assert!(part.is_crack_free(1e-4), "{part:?}");
    assert!(
        part.same_level_edges + part.level_change_edges < full.same_level_edges + full.level_change_edges
    );
}
