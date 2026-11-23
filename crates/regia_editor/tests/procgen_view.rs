// SPDX-License-Identifier: MPL-2.0
//! The procedural viewport (specs 64 § 2.4, 65 § 7, 66 §§ 4.1 and 7, 67): generate around view,
//! the tiles overlay, Shape and Spline gizmos and the biome overlay. Headless: inline runs, counted
//! frames, no GPU.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_editor::procgen::agent;
use regia_editor::procgen::debug_view::ProcPreview;
use regia_editor::procgen::view::{self, CIRCLE_SEGMENTS};
use regia_editor::selection::{SelectMode, Selection};
use regia_editor::viewport::EditorCamera;
use regia_procgen::debug_view::state_color;
use regia_procgen::tiles::{
    AnchorSettings, ProcGenExtraAnchors, ProcGenTiles, TierReach, TileKey, TileSettings, TileState, ViewGate,
};
use regia_procgen::world::{
    ProcBounds, ProcGenMode, ProcGenPlugin, ProcGenSettings, ProcGenerator, ProcGraphSource,
};
use regia_procgen::{Edge, NodeEntry, ProcGraph};
use regia_scene::components::EntityGuid;
use regia_scene::shape::{Shape, ShapeKind};
use regia_scene::spline::{Spline, SplinePoint};
use regia_scene::{Transform, spline::TangentMode};
use serde_json::{Value, json};

fn node(ty: &str, settings: &[(&str, &str)]) -> NodeEntry {
    let mut n = NodeEntry::new(ty);
    for (k, v) in settings {
        n = n.with_ron(k, v);
    }
    n
}

/// `in.self -> sample.region -> spawn.mesh`.
fn scatter() -> String {
    let mut g = ProcGraph::default();
    g.nodes.insert(1, node("in.self", &[]));
    g.nodes.insert(
        2,
        node("sample.region", &[("spacing_m", "8.0"), ("jitter", "0.5")]),
    );
    g.nodes
        .insert(3, node("spawn.mesh", &[("entries", "[(mesh: \"rock.glb\")]")]));
    g.edges = vec![Edge::new(1, "out", 2, "region"), Edge::new(2, "out", 3, "in")];
    g.to_ron()
}

fn tiled_app() -> (App, Entity) {
    tiled_app_with(None)
}

fn tiled_app_with(view_gate: Option<ViewGate>) -> (App, Entity) {
    let mut files = BTreeMap::new();
    files.insert("s.pgraph.ron".to_string(), scatter());
    let mut app = App::new();
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(files),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.insert_resource(ProcPreview::default());
    app.insert_resource(Selection::default());
    app.insert_resource(EditorCamera::default());
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                mode: ProcGenMode::Anchored,
                bounds: ProcBounds::Box {
                    half_extents: Vec3::new(256.0, 10.0, 256.0),
                },
                seed: Some(11),
                tiles: Some(TileSettings {
                    cell_m: 64.0,
                    runtime: Some(AnchorSettings {
                        tiers: vec![TierReach {
                            cell_m: 64.0,
                            build_m: 80.0,
                            release_factor: 1.25,
                            view_bias: 0.5,
                            view_gate,
                        }],
                        max_new_per_frame: Some(10_000),
                        max_running: Some(10_000),
                        ..AnchorSettings::default()
                    }),
                    ..TileSettings::default()
                }),
                ..ProcGenerator::new("s.pgraph.ron")
            },
            EntityGuid(regia_core::id::Guid(0x61)),
            Transform::default(),
        ))
        .id();
    (app, e)
}

/// Puts the editor camera's eye at `(x, 5, z)`.
fn eye_at(app: &mut App, x: f32, z: f32) {
    let mut cam = app.world_mut().resource_mut::<EditorCamera>();
    cam.yaw = 0.0;
    cam.pitch = 0.0;
    cam.distance = 1.0;
    cam.target = Vec3::new(x, 5.0, z - 1.0);
}

fn keys(xs: &[i64], zs: &[i64]) -> BTreeSet<TileKey> {
    let mut out = BTreeSet::new();
    for &x in xs {
        for &z in zs {
            out.insert(TileKey {
                tier: 0,
                cell: regia_scene::stream_grid::CellCoord::new(x, z),
            });
        }
    }
    out
}

fn settle(app: &mut App) {
    for _ in 0..12 {
        view::tick(app.world_mut());
        app.update();
    }
}

#[test]
fn generate_around_view_requests_and_releases_cells_as_the_camera_crosses_a_boundary() {
    let (mut app, g) = tiled_app();
    eye_at(&mut app, 30.0, 30.0);
    // Off: the editor camera is no anchor, nothing is requested or resident.
    settle(&mut app);
    assert!(view::resident_cells(app.world(), g).is_empty());

    view::set_switches(app.world_mut(), Some(true), None, None, None);
    settle(&mut app);
    let first = keys(&[-1, 0, 1], &[-1, 0, 1]);
    let requested: BTreeSet<TileKey> = view::requested_cells(app.world_mut(), g).into_iter().collect();
    assert_eq!(requested, first);
    let resident: BTreeSet<TileKey> = view::resident_cells(app.world(), g).into_iter().collect();
    assert_eq!(resident, first);

    // Across the cell boundary at x = 64: the x = -1 column is beyond the release radius.
    eye_at(&mut app, 110.0, 30.0);
    settle(&mut app);
    let second = keys(&[0, 1, 2], &[-1, 0, 1]);
    let requested: BTreeSet<TileKey> = view::requested_cells(app.world_mut(), g).into_iter().collect();
    assert_eq!(requested, second);
    let resident: BTreeSet<TileKey> = view::resident_cells(app.world(), g).into_iter().collect();
    assert_eq!(resident, second);
    for gone in keys(&[-1], &[-1, 0, 1]) {
        assert!(!resident.contains(&gone), "{gone:?} was released");
        assert_eq!(app.world().resource::<ProcGenTiles>().state(g, gone), None);
    }

    // The anchor carries a frustum. `view_gate` is off, so the nine cells above are unchanged.
    assert!(
        app.world()
            .resource::<ProcGenExtraAnchors>()
            .0
            .first()
            .is_some_and(|a| a.frustum.is_some())
    );

    // Off again: the anchor goes away.
    view::set_switches(app.world_mut(), Some(false), None, None, None);
    assert!(
        app.world()
            .resource::<regia_procgen::tiles::ProcGenExtraAnchors>()
            .0
            .is_empty()
    );
}

#[test]
fn a_view_gate_drops_the_cell_behind_the_editor_camera() {
    let (mut app, g) = tiled_app_with(Some(ViewGate::default()));
    // Eye at (30, 5, 30), yaw 0 pitch 0: forward is −Z. Cell (0, −1) is ahead; (0, 1) is behind.
    eye_at(&mut app, 30.0, 30.0);
    view::set_switches(app.world_mut(), Some(true), None, None, None);
    settle(&mut app);
    let requested: BTreeSet<TileKey> = view::requested_cells(app.world_mut(), g).into_iter().collect();
    let ahead = TileKey {
        tier: 0,
        cell: regia_scene::stream_grid::CellCoord::new(0, -1),
    };
    let behind = TileKey {
        tier: 0,
        cell: regia_scene::stream_grid::CellCoord::new(0, 1),
    };
    assert!(
        requested.contains(&ahead),
        "the cell in front is requested: {requested:?}"
    );
    assert!(
        !requested.contains(&behind),
        "the cell behind the camera is not requested: {requested:?}"
    );
}

#[test]
fn the_tiles_overlay_draws_one_outline_per_resident_cell_colored_by_state() {
    let (mut app, g) = tiled_app();
    eye_at(&mut app, 30.0, 30.0);
    view::set_switches(app.world_mut(), Some(true), Some(false), None, None);
    settle(&mut app);
    assert!(
        view::overlay_lines(app.world_mut()).is_empty(),
        "overlay off draws nothing"
    );
    view::set_switches(app.world_mut(), None, Some(true), None, None);
    let resident = view::resident_cells(app.world(), g).len();
    assert_eq!(resident, 9);
    let lines = view::overlay_lines(app.world_mut());
    let c = state_color(TileState::Resident);
    let cell_color = [c.r, c.g, c.b, 1.0];
    let outlines: Vec<_> = lines.iter().filter(|l| l.2 == cell_color).collect();
    assert_eq!(outlines.len(), resident * 4, "four edges per cell");
    // Every edge is 64 m long and horizontal.
    for (a, b, _) in &outlines {
        assert_eq!(a[1], b[1]);
        let len = ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
        assert!((len - 64.0).abs() < 1e-3, "{len}");
    }
    // The editor camera anchor draws its build and release circles (spec 66 debug view data).
    use regia_procgen::debug_view::{BUILD_COLOR, RELEASE_COLOR};
    let build_c = [BUILD_COLOR.r, BUILD_COLOR.g, BUILD_COLOR.b, 1.0];
    let release_c = [RELEASE_COLOR.r, RELEASE_COLOR.g, RELEASE_COLOR.b, 1.0];
    assert!(lines.iter().any(|l| l.2 == build_c), "build circle");
    assert!(lines.iter().any(|l| l.2 == release_c), "release circle");
}

#[test]
fn shape_volumes_draw_their_wireframes() {
    let tf = Transform::from_translation(Vec3::new(1.0, 2.0, 3.0));
    let boxed = Shape::new(ShapeKind::Box {
        half_extents: Vec3::new(1.0, 2.0, 3.0),
    });
    let lines = view::shape_lines(&boxed, &tf, [1.0; 4]);
    assert_eq!(lines.len(), 12);
    let xs: Vec<f32> = lines.iter().flat_map(|l| [l.0[0], l.1[0]]).collect();
    assert_eq!(xs.iter().cloned().fold(f32::INFINITY, f32::min), 0.0);
    assert_eq!(xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max), 2.0);
    let ball = Shape::new(ShapeKind::Sphere { radius: 2.0 });
    assert_eq!(view::shape_lines(&ball, &tf, [1.0; 4]).len(), 3 * CIRCLE_SEGMENTS);
    let soft = Shape { edge_m: 1.0, ..ball };
    assert_eq!(
        view::shape_lines(&soft, &tf, [1.0; 4]).len(),
        6 * CIRCLE_SEGMENTS,
        "an edge band adds the outer form"
    );
}

#[test]
fn a_spline_gizmo_has_the_expected_vertices_and_handles_on_the_authored_points() {
    let authored = [
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(10.0, 0.0, 0.0),
        Vec3::new(20.0, 0.0, 0.0),
        Vec3::new(30.0, 0.0, 0.0),
    ];
    let spline = Spline {
        points: authored
            .iter()
            .map(|p| SplinePoint::at(*p).with_mode(TangentMode::Linear))
            .collect(),
        ..Spline::through(authored)
    };
    let tf = Transform::from_translation(Vec3::new(5.0, 1.0, -2.0));
    let g = view::spline_gizmo(&spline, &tf);
    // Straight segments flatten to one chord each.
    assert_eq!(g.curve.len(), authored.len() - 1);
    let expect: Vec<[f32; 3]> = authored
        .iter()
        .map(|p| (*p + tf.translation).to_array())
        .collect();
    assert_eq!(g.points, expect, "handles sit on the authored control points");
    assert!(g.tangents.is_empty(), "Linear points derive their tangents");

    let free = Spline {
        points: vec![
            SplinePoint::free(Vec3::ZERO, Vec3::new(3.0, 0.0, 0.0), Vec3::new(6.0, 0.0, 0.0)),
            SplinePoint::free(
                Vec3::new(10.0, 0.0, 0.0),
                Vec3::new(3.0, 0.0, 0.0),
                Vec3::new(3.0, 0.0, 0.0),
            ),
        ],
        ..Spline::through([Vec3::ZERO, Vec3::X * 10.0])
    };
    let g = view::spline_gizmo(&free, &Transform::default());
    assert_eq!(g.tangents.len(), 4);
    let first_out = g
        .tangents
        .iter()
        .find(|h| h.point == 0 && !h.incoming)
        .map(|h| h.position);
    assert_eq!(first_out, Some([2.0, 0.0, 0.0]), "a third of tangent_out");
    let first_in = g
        .tangents
        .iter()
        .find(|h| h.point == 0 && h.incoming)
        .map(|h| h.position);
    assert_eq!(
        first_in,
        Some([-1.0, 0.0, 0.0]),
        "the incoming handle sits behind the point"
    );
}

#[test]
fn selected_volumes_draw_without_the_show_all_switch() {
    let (mut app, _g) = tiled_app();
    let shape = app
        .world_mut()
        .spawn((Shape::default(), Transform::default()))
        .id();
    let curve = app
        .world_mut()
        .spawn((
            Spline::through([Vec3::ZERO, Vec3::X * 8.0, Vec3::new(8.0, 0.0, 8.0)]),
            Transform::default(),
        ))
        .id();
    assert!(
        view::volume_lines(app.world_mut()).is_empty(),
        "nothing selected, show_volumes off"
    );
    app.world_mut()
        .resource_mut::<Selection>()
        .select(curve, SelectMode::Replace);
    let with_handles = view::volume_lines(app.world_mut()).len();
    assert!(with_handles > 0);
    view::set_switches(app.world_mut(), None, None, Some(true), None);
    let all = view::volume_lines(app.world_mut()).len();
    assert_eq!(all, with_handles + 12, "show_volumes adds the box");
    let _ = shape;
}

#[test]
fn biome_overlay_outlines_areas_and_marks_preview_alone() {
    use regia_procgen::biome::ecs::BiomeArea;
    let (mut app, _g) = tiled_app();
    let a = app
        .world_mut()
        .spawn((BiomeArea::new("a.biome.ron"), Transform::default()))
        .id();
    app.world_mut().spawn((
        BiomeArea::new("b.biome.ron"),
        Transform::from_translation(Vec3::new(50.0, 0.0, 0.0)),
    ));
    assert!(view::biome_lines(app.world_mut()).is_empty(), "off by default");
    view::set_switches(app.world_mut(), None, None, None, Some(true));
    let lines = view::biome_lines(app.world_mut());
    assert_eq!(lines.len(), 24, "a unit box per area");
    let colors: BTreeSet<[u32; 4]> = lines.iter().map(|l| l.2.map(f32::to_bits)).collect();
    assert_eq!(colors.len(), 1);
    if let Some(mut area) = app.world_mut().get_mut::<BiomeArea>(a) {
        area.preview_alone = true;
    }
    let lines = view::biome_lines(app.world_mut());
    let colors: BTreeSet<[u32; 4]> = lines.iter().map(|l| l.2.map(f32::to_bits)).collect();
    assert_eq!(colors.len(), 2, "the preview-alone area has its own color");
}

fn call(app: &mut App, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let handler = agent::methods()
        .into_iter()
        .find(|(m, _)| m.name == "editor.procgen_view_set")
        .map(|(_, h)| h)
        .expect("method registered");
    handler(app.world_mut(), &params)
}

#[test]
fn procgen_view_set_flips_the_switches_over_the_agent() {
    let (mut app, _g) = tiled_app();
    eye_at(&mut app, 30.0, 30.0);
    let read = call(&mut app, json!({})).expect("read");
    assert_eq!(read["generate_around_view"], false);
    assert_eq!(read["anchors"], 0);
    let set = call(
        &mut app,
        json!({ "generate_around_view": true, "tiles_overlay": true, "show_volumes": false }),
    )
    .expect("set");
    assert_eq!(set["generate_around_view"], true);
    assert_eq!(set["tiles_overlay"], true);
    assert_eq!(set["anchors"], 1);
    let kept = call(&mut app, json!({ "show_volumes": true })).expect("partial");
    assert_eq!(
        kept["generate_around_view"], true,
        "omitted flags keep their value"
    );
    assert_eq!(kept["show_volumes"], true);
    let err = call(&mut app, json!({ "tiles_overlay": "yes" })).expect_err("bad flag");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["reason"].as_str()),
        Some("procgen_bad_view")
    );
}

/// Rescue review item 10: the sync owns one slot in `ProcGenExtraAnchors`. Turning the switch on
/// adds the editor camera next to a host-supplied anchor, turning it off removes only the editor's
/// entry, and without an `EditorCamera` nothing is invented (a headless world anchors at the
/// origin otherwise).
#[test]
fn sync_view_anchor_owns_one_slot_and_invents_nothing_without_a_camera() {
    use regia_procgen::tiles::{AnchorView, ProcGenExtraAnchors};
    let (mut app, _g) = tiled_app();
    let host = AnchorView::at(Vec3::new(50.0, 0.0, 50.0));
    app.world_mut().insert_resource(ProcGenExtraAnchors(vec![host]));
    view::set_switches(app.world_mut(), Some(true), None, None, None);
    let extra = app.world_mut().resource::<ProcGenExtraAnchors>().0.clone();
    assert_eq!(extra.len(), 2, "host anchor kept, editor anchor added: {extra:?}");
    assert!(extra.contains(&host), "{extra:?}");
    view::set_switches(app.world_mut(), Some(false), None, None, None);
    let extra = app.world_mut().resource::<ProcGenExtraAnchors>().0.clone();
    assert_eq!(extra, vec![host], "only the editor's entry went away");
    // No editor camera: the switch comes on but no anchor appears.
    app.world_mut().remove_resource::<EditorCamera>();
    view::set_switches(app.world_mut(), Some(true), None, None, None);
    let extra = app.world_mut().resource::<ProcGenExtraAnchors>().0.clone();
    assert_eq!(extra, vec![host], "{extra:?}");
}

/// Rescue review item 12: `requested_cells` answers empty on a world without `ProcGenTiles`
/// (a generator-carrying world without the plugin) instead of panicking.
#[test]
fn requested_cells_is_empty_without_the_tile_plugin() {
    let mut app = App::new();
    let e = app
        .world_mut()
        .spawn((
            ProcGenerator {
                tiles: Some(TileSettings {
                    cell_m: 64.0,
                    runtime: Some(AnchorSettings::default()),
                    ..TileSettings::default()
                }),
                ..ProcGenerator::new("s.pgraph.ron")
            },
            Transform::default(),
        ))
        .id();
    assert!(view::requested_cells(app.world_mut(), e).is_empty());
    assert!(view::resident_cells(app.world(), e).is_empty());
}

/// Rescue review item 8: a selected volume beyond the first `MAX_VOLUMES` entities still draws
/// when `show_volumes` is on (selected first, uncapped; the budget fills with unselected ones).
#[test]
fn selected_volume_draws_past_the_unselected_budget() {
    let (mut app, _g) = tiled_app();
    // 256 unselected boxes sort before the selected sphere (entity order).
    for _ in 0..view::MAX_VOLUMES {
        app.world_mut().spawn((Shape::default(), Transform::default()));
    }
    let sphere = app
        .world_mut()
        .spawn((
            Shape {
                kind: ShapeKind::Sphere { radius: 2.0 },
                ..Shape::default()
            },
            Transform::default(),
        ))
        .id();
    app.world_mut()
        .resource_mut::<Selection>()
        .select(sphere, SelectMode::Replace);
    view::set_switches(app.world_mut(), None, None, Some(true), None);
    let lines = view::volume_lines(app.world_mut());
    // The selected sphere: 3 great circles of CIRCLE_SEGMENTS segments in the selected color.
    let selected_color = [1.0, 0.85, 0.0, 1.0];
    let selected = lines.iter().filter(|(_, _, c)| *c == selected_color).count();
    assert_eq!(
        selected,
        3 * CIRCLE_SEGMENTS,
        "the selected sphere draws past the budget"
    );
}
