// SPDX-License-Identifier: MPL-2.0
//! The game widgets end to end through the UI plugin (ADR-2958): the nodes they draw land where the
//! math says after a real layout, moving things cost no layout, buttons send what they say, and
//! `.ui.ron` authors all of them. No GPU and no golden image.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::glam::{Affine3A, Quat, Vec3};
use regia_render::Camera;
use regia_scene::GlobalTransform;
use regia_ui::agent_bridge::call;
use regia_ui::authoring::{WidgetDef, parse_ui_ron, spawn_ui, write_ui_ron};
use regia_ui::game_widgets::radar::MAX_BLIPS;
use regia_ui::game_widgets::scoreboard::MAX_SCORE_ROWS;
use regia_ui::game_widgets::*;
use regia_ui::layout::UiLayoutStats;
use regia_ui::prelude::*;
use regia_ui::scale::UiScales;
use regia_ui::widgets::VirtualButtons;
use regia_ui::world_target::{Billboard, WorldPanel};
use regia_ui::{BackgroundKind, UI_RON_VERSION, UiLibrary, UiMigrators, UiTarget};
use serde_json::json;

fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, UiPlugin::code_only()));
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(10)));
    // The first frame runs `Startup`, which sets the window size the tests read.
    app.update();
    app
}

fn root(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id()
}

fn sized(app: &mut App, parent: Entity, w: f32, h: f32, bundle: impl Bundle) -> Entity {
    let node = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(0.0),
                    top: Dimension::Points(0.0),
                    ..Edges::default()
                },
                size: [Dimension::Points(w), Dimension::Points(h)],
                ..Default::default()
            }),
            bundle,
        ))
        .id();
    app.world_mut().entity_mut(parent).add_child(node);
    node
}

fn frames(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

fn at(x: f32, z: f32) -> GlobalTransform {
    GlobalTransform(Affine3A::from_translation(Vec3::new(x, 0.0, z)))
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.51
}

fn center(app: &App, node: Entity) -> [f32; 2] {
    let c = app.world().get::<ComputedNode>(node).expect("laid out");
    [c.rect[0] + c.rect[2] * 0.5, c.rect[1] + c.rect[3] * 0.5]
}

fn texts_under(app: &App, node: Entity, out: &mut Vec<String>) {
    let world = app.world();
    let shown = world
        .get::<NodeStyle>(node)
        .is_none_or(|s| s.0.display != Display::None);
    if !shown {
        return;
    }
    if let Some(text) = world.get::<Text>(node) {
        let s: String = text
            .spans
            .iter()
            .filter_map(|span| match &span.content {
                regia_ui::text::TextContent::Lit(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        if !s.is_empty() {
            out.push(s);
        }
    }
    if let Some(children) = world.get::<Children>(node) {
        for child in children.iter() {
            texts_under(app, child, out);
        }
    }
}

fn texts(app: &App, node: Entity) -> Vec<String> {
    let mut out = Vec::new();
    texts_under(app, node, &mut out);
    out
}

// ---- Radar ------------------------------------------------------------------------------------

#[test]
fn radar_blips_land_where_the_world_says_with_north_up() {
    let mut app = ui_app();
    let r = root(&mut app);
    let radar = sized(
        &mut app,
        r,
        200.0,
        200.0,
        Radar {
            center: [10.0, 20.0],
            ..Radar::with_range(50.0)
        },
    );
    let north = app.world_mut().spawn((RadarBlip::default(), at(10.0, -5.0))).id();
    let east = app.world_mut().spawn((RadarBlip::default(), at(35.0, 20.0))).id();
    let out = app
        .world_mut()
        .spawn((RadarBlip::default(), at(500.0, 20.0)))
        .id();
    let pinned = app
        .world_mut()
        .spawn((
            RadarBlip {
                clamp_to_edge: true,
                ..RadarBlip::default()
            },
            at(10.0, 500.0),
        ))
        .id();
    frames(&mut app, 3);
    let state = app.world().get::<RadarState>(radar).expect("state").clone();
    assert_eq!(state.size, [200.0, 200.0]);
    assert_eq!(state.blips.len(), 3, "out of range and not pinned is not shown");
    assert!(state.blips.iter().all(|b| b.source != out));
    let of = |e: Entity| state.blips.iter().find(|b| b.source == e).expect("shown");
    let n = center(&app, of(north).node);
    assert!(close(n[0], 100.0) && close(n[1], 50.0), "north, drawn at {n:?}");
    let e = center(&app, of(east).node);
    assert!(close(e[0], 150.0) && close(e[1], 100.0), "east, drawn at {e:?}");
    let p = of(pinned);
    assert!(!p.inside);
    let drawn = center(&app, p.node);
    assert!(close(drawn[1], 200.0), "south rim, drawn at {drawn:?}");
}

#[test]
fn a_radar_follows_the_hud_focus_and_turns_with_it() {
    let mut app = ui_app();
    let r = root(&mut app);
    let radar = sized(
        &mut app,
        r,
        200.0,
        200.0,
        Radar {
            rotate: true,
            ..Radar::with_range(50.0)
        },
    );
    // Facing east (a −90° turn about +Y takes −Z to +X), standing at x = 100.
    let facing_east = Affine3A::from_rotation_translation(
        Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2),
        Vec3::new(100.0, 0.0, 0.0),
    );
    app.world_mut().spawn((HudFocus, GlobalTransform(facing_east)));
    let ahead = app.world_mut().spawn((RadarBlip::default(), at(125.0, 0.0))).id();
    frames(&mut app, 3);
    let state = app.world().get::<RadarState>(radar).expect("state").clone();
    assert_eq!(state.center, [100.0, 0.0]);
    assert!((state.heading - std::f32::consts::FRAC_PI_2).abs() < 1e-4);
    let blip = state.blips.iter().find(|b| b.source == ahead).expect("shown");
    let drawn = center(&app, blip.node);
    assert!(
        close(drawn[0], 100.0) && close(drawn[1], 50.0),
        "ahead is up: {drawn:?}"
    );
}

#[test]
fn a_minimap_shows_its_whole_area_and_its_map_image() {
    let mut app = ui_app();
    let r = root(&mut app);
    let map = sized(
        &mut app,
        r,
        272.0,
        272.0,
        Radar::minimap([0.0, 0.0, 1.0, 1.0]).with_map(0xABC, [0.0; 4]),
    );
    let dot = app
        .world_mut()
        .spawn((
            RadarBlip {
                size_px: 7.0,
                ..RadarBlip::default()
            },
            at(0.30, 0.62),
        ))
        .id();
    frames(&mut app, 3);
    let state = app.world().get::<RadarState>(map).expect("state").clone();
    let blip = state.blips.iter().find(|b| b.source == dot).expect("shown");
    let drawn = center(&app, blip.node);
    assert!(
        close(drawn[0], 0.30 * 272.0) && close(drawn[1], 0.62 * 272.0),
        "{drawn:?}"
    );
    // The first child draws the whole image, stretched.
    let children = app.world().get::<Children>(map).expect("children");
    let image = children
        .iter()
        .find_map(|c| app.world().get::<UiImage>(c).copied())
        .expect("map image node");
    assert_eq!(image.id, 0xABC);
    assert_eq!(image.uv, [0.0, 0.0, 1.0, 1.0]);
    assert!(!image.preserve_aspect);
}

#[test]
fn a_radar_map_is_a_window_that_scrolls_with_the_center() {
    let mut app = ui_app();
    let r = root(&mut app);
    let radar = sized(
        &mut app,
        r,
        100.0,
        100.0,
        Radar {
            center: [50.0, 50.0],
            ..Radar::with_range(25.0).with_map(1, [0.0, 0.0, 100.0, 100.0])
        },
    );
    frames(&mut app, 2);
    let image_of = |app: &App| {
        app.world()
            .get::<Children>(radar)
            .expect("children")
            .iter()
            .find_map(|c| app.world().get::<UiImage>(c).copied())
            .expect("map")
    };
    assert_eq!(image_of(&app).uv, [0.25, 0.25, 0.75, 0.75]);
    app.world_mut().get_mut::<Radar>(radar).expect("radar").center = [75.0, 50.0];
    frames(&mut app, 1);
    assert_eq!(image_of(&app).uv, [0.5, 0.25, 1.0, 0.75]);
}

/// The entity at a radar's center has no blip of its own, however it was chosen; a minimap's center
/// is its rect, so the same entity is on it like any other.
#[test]
fn the_followed_entity_has_no_blip_on_a_radar_and_has_one_on_a_minimap() {
    let mut app = ui_app();
    let r = root(&mut app);
    let named = app.world_mut().spawn((RadarBlip::default(), at(5.0, 0.0))).id();
    let focus = app
        .world_mut()
        .spawn((RadarBlip::default(), HudFocus, at(1.0, 0.0)))
        .id();
    let by_name = sized(
        &mut app,
        r,
        100.0,
        100.0,
        Radar {
            follow: Some(named),
            ..Radar::with_range(50.0)
        },
    );
    let by_focus = sized(&mut app, r, 100.0, 100.0, Radar::with_range(50.0));
    let map = sized(
        &mut app,
        r,
        100.0,
        100.0,
        Radar {
            follow: Some(named),
            ..Radar::minimap([-50.0, -50.0, 50.0, 50.0])
        },
    );
    frames(&mut app, 3);
    let shown = |app: &App, n: Entity| -> Vec<Entity> {
        let state = app.world().get::<RadarState>(n).expect("state");
        state.blips.iter().map(|b| b.source).collect()
    };
    assert_eq!(shown(&app, by_name), [focus]);
    assert_eq!(shown(&app, by_focus), [named]);
    let by_bits = |mut v: Vec<Entity>| {
        v.sort_by_key(|e| e.to_bits());
        v
    };
    assert_eq!(by_bits(shown(&app, map)), by_bits(vec![named, focus]));
}

/// Two blips of different sizes pass each other: each keeps its node, so no layout runs.
#[test]
fn blips_of_mixed_sizes_passing_each_other_run_no_layout() {
    let mut app = ui_app();
    let r = root(&mut app);
    sized(
        &mut app,
        r,
        200.0,
        200.0,
        Radar {
            center: [0.0, 0.0],
            ..Radar::with_range(50.0)
        },
    );
    let small = app
        .world_mut()
        .spawn((
            RadarBlip {
                size_px: 4.0,
                ..RadarBlip::default()
            },
            at(5.0, 0.0),
        ))
        .id();
    let big = app
        .world_mut()
        .spawn((
            RadarBlip {
                size_px: 12.0,
                ..RadarBlip::default()
            },
            at(10.0, 0.0),
        ))
        .id();
    frames(&mut app, 4);
    let before = *app.world().resource::<UiLayoutStats>();
    *app.world_mut().get_mut::<GlobalTransform>(small).expect("small") = at(20.0, 0.0);
    *app.world_mut().get_mut::<GlobalTransform>(big).expect("big") = at(1.0, 0.0);
    frames(&mut app, 3);
    let after = *app.world().resource::<UiLayoutStats>();
    assert_eq!(
        (after.full_runs, after.incremental_runs),
        (before.full_runs, before.incremental_runs),
    );
}

#[test]
fn moving_blips_run_no_layout_and_the_pool_does_not_grow() {
    let mut app = ui_app();
    let r = root(&mut app);
    let radar = sized(
        &mut app,
        r,
        200.0,
        200.0,
        Radar {
            center: [0.0, 0.0],
            ..Radar::with_range(50.0)
        },
    );
    let blips: Vec<Entity> = (0..50)
        .map(|i| {
            app.world_mut()
                .spawn((RadarBlip::default(), at(i as f32 * 0.5, 0.0)))
                .id()
        })
        .collect();
    frames(&mut app, 4);
    let before = *app.world().resource::<UiLayoutStats>();
    let nodes_before = app.world().get::<Children>(radar).expect("children").len();
    for frame in 1..=20 {
        for b in &blips {
            let mut g = app.world_mut().get_mut::<GlobalTransform>(*b).expect("blip");
            g.0.translation.z = frame as f32 * 0.25;
        }
        frames(&mut app, 1);
    }
    let after = *app.world().resource::<UiLayoutStats>();
    assert_eq!(
        after.full_runs, before.full_runs,
        "moving blips rebuild no layout"
    );
    assert_eq!(
        after.incremental_runs, before.incremental_runs,
        "moving blips reconcile no node: {before:?} {after:?}"
    );
    assert_eq!(
        app.world().get::<Children>(radar).expect("children").len(),
        nodes_before
    );
    // They did move on screen.
    let state = app.world().get::<RadarState>(radar).expect("state");
    assert!(state.blips.iter().all(|b| b.world[1] == 5.0));
}

#[test]
fn the_nearest_blips_win_a_crowded_radar_and_layers_filter() {
    let mut app = ui_app();
    let r = root(&mut app);
    let radar = sized(
        &mut app,
        r,
        200.0,
        200.0,
        Radar {
            max_blips: 3,
            ..Radar::with_range(1000.0)
        },
    );
    let ids: Vec<Entity> = (0..10)
        .map(|i| {
            app.world_mut()
                .spawn((RadarBlip::default(), at(10.0 + i as f32, 0.0)))
                .id()
        })
        .collect();
    let hidden_layer = app
        .world_mut()
        .spawn((
            RadarBlip {
                layer: 2,
                ..RadarBlip::default()
            },
            at(0.5, 0.0),
        ))
        .id();
    app.world_mut().get_mut::<Radar>(radar).expect("radar").layer_mask = 1;
    frames(&mut app, 3);
    let state = app.world().get::<RadarState>(radar).expect("state").clone();
    let mut shown: Vec<Entity> = state.blips.iter().map(|b| b.source).collect();
    shown.sort_by_key(|e| e.to_bits());
    let mut nearest = ids[..3].to_vec();
    nearest.sort_by_key(|e| e.to_bits());
    assert_eq!(shown, nearest);
    assert!(!shown.contains(&hidden_layer));
    assert_eq!(state.dropped, 7);
    // Everything leaving hides the pool without despawning it.
    for e in ids {
        app.world_mut().despawn(e);
    }
    frames(&mut app, 2);
    let state = app.world().get::<RadarState>(radar).expect("state").clone();
    assert!(state.blips.is_empty());
    assert_eq!(state.dropped, 0);
    // The hard cap holds whatever a caller asks for.
    app.world_mut().get_mut::<Radar>(radar).expect("radar").max_blips = usize::MAX;
    for i in 0..(MAX_BLIPS + 5) {
        app.world_mut()
            .spawn((RadarBlip::default(), at(i as f32 * 0.001, 0.0)));
    }
    frames(&mut app, 2);
    let state = app.world().get::<RadarState>(radar).expect("state");
    assert_eq!(state.blips.len(), MAX_BLIPS);
    assert_eq!(state.dropped, 5);
}

#[test]
fn a_blip_or_center_that_is_not_finite_never_reaches_a_node() {
    let mut app = ui_app();
    let r = root(&mut app);
    let radar = sized(
        &mut app,
        r,
        200.0,
        200.0,
        Radar {
            center: [f32::NAN, 0.0],
            ..Radar::with_range(50.0)
        },
    );
    app.world_mut().spawn((
        RadarBlip::default(),
        GlobalTransform(Affine3A::from_translation(Vec3::splat(f32::NAN))),
    ));
    frames(&mut app, 3);
    let state = app.world().get::<RadarState>(radar).expect("state");
    assert!(state.blips.is_empty());
    // A zero or NaN range falls back to a usable one instead of dividing by it.
    app.world_mut().get_mut::<Radar>(radar).expect("radar").mode = RadarMode::Radar { range: f32::NAN };
    app.world_mut().get_mut::<Radar>(radar).expect("radar").center = [0.0, 0.0];
    app.world_mut().spawn((RadarBlip::default(), at(0.5, 0.0)));
    frames(&mut app, 2);
    assert_eq!(
        app.world().get::<RadarState>(radar).expect("state").blips.len(),
        1
    );
}

// ---- World markers and brackets ---------------------------------------------------------------

/// A camera at the origin looking down −Z, and a full-window layer. Returns `(layer, size)`.
fn marker_scene(app: &mut App) -> (Entity, [f32; 2]) {
    let r = root(app);
    app.world_mut()
        .spawn((Camera::default(), GlobalTransform(Affine3A::IDENTITY)));
    let window = app.world().resource::<UiScales>().window_size;
    let layer = sized(app, r, window[0], window[1], MarkerLayer::default());
    (layer, window)
}

fn at3(x: f32, y: f32, z: f32) -> GlobalTransform {
    GlobalTransform(Affine3A::from_translation(Vec3::new(x, y, z)))
}

#[test]
fn a_marker_ahead_sits_on_the_screen_center_and_follows_the_world() {
    let mut app = ui_app();
    let (layer, size) = marker_scene(&mut app);
    let target = app
        .world_mut()
        .spawn((
            WorldMarker {
                label: "Base".into(),
                show_distance: true,
                ..WorldMarker::default()
            },
            at3(0.0, 0.0, -10.0),
        ))
        .id();
    frames(&mut app, 3);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let m = state.markers.iter().find(|m| m.source == target).expect("marker");
    assert!(m.on_screen && m.visible && !m.clamped);
    assert!(
        close(m.pos[0], size[0] * 0.5) && close(m.pos[1], size[1] * 0.5),
        "{m:?}"
    );
    assert!((m.distance - 10.0).abs() < 1e-3);
    // The label reads with the distance; the node is centered on its dot.
    assert_eq!(texts(&app, m.node), ["Base  10 m"]);
    let rect = app.world().get::<ComputedNode>(m.node).expect("laid out").rect;
    assert!(close(rect[0] + rect[2] * 0.5, size[0] * 0.5), "{rect:?}");
    assert!(close(rect[1] + 6.0, size[1] * 0.5), "{rect:?}");
    // It moves without a layout pass (its text stays the same at the same distance).
    let before = *app.world().resource::<UiLayoutStats>();
    app.world_mut()
        .get_mut::<GlobalTransform>(target)
        .expect("t")
        .0
        .translation
        .x = 0.5;
    app.world_mut()
        .get_mut::<GlobalTransform>(target)
        .expect("t")
        .0
        .translation
        .z = -9.98;
    frames(&mut app, 1);
    let after = *app.world().resource::<UiLayoutStats>();
    assert_eq!(
        (after.full_runs, after.incremental_runs),
        (before.full_runs, before.incremental_runs)
    );
    let moved = app.world().get::<MarkerLayerState>(layer).expect("state").markers[0].pos;
    assert!(moved[0] > size[0] * 0.5 + 1.0, "{moved:?}");
}

#[test]
fn off_screen_markers_pin_to_the_edge_or_hide() {
    let mut app = ui_app();
    let (layer, size) = marker_scene(&mut app);
    let pinned = app
        .world_mut()
        .spawn((
            WorldMarker {
                clamp_to_edge: true,
                ..WorldMarker::default()
            },
            at3(100.0, 0.0, -1.0),
        ))
        .id();
    let behind = app
        .world_mut()
        .spawn((
            WorldMarker {
                clamp_to_edge: true,
                ..WorldMarker::default()
            },
            at3(1.0, 0.0, 10.0),
        ))
        .id();
    let plain = app
        .world_mut()
        .spawn((WorldMarker::default(), at3(100.0, 0.0, -1.0)))
        .id();
    frames(&mut app, 3);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let of = |e| state.markers.iter().find(|m| m.source == e).expect("readout");
    let p = of(pinned);
    assert!(!p.on_screen && p.clamped && p.visible);
    assert!(close(p.pos[0], size[0] - 24.0), "{p:?}");
    let b = of(behind);
    assert!(b.behind && b.visible && b.clamped, "{b:?}");
    assert!(b.pos[0] > size[0] * 0.5);
    let n = of(plain);
    assert!(!n.visible && !n.clamped);
    let style = app.world().get::<NodeStyle>(n.node).expect("style");
    assert_eq!(style.0.display, Display::None);
}

#[test]
fn a_marker_fades_with_distance_and_goes_with_its_entity() {
    let mut app = ui_app();
    let (layer, _) = marker_scene(&mut app);
    let far = app
        .world_mut()
        .spawn((
            WorldMarker {
                fade: Some([10.0, 30.0]),
                ..WorldMarker::default()
            },
            at3(0.0, 0.0, -20.0),
        ))
        .id();
    // Frame 1 sizes the new layer, frame 2 lays out the marker (still unseen: it has no width to
    // center by), frame 3 shows it at its faded alpha.
    frames(&mut app, 2);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let m = &state.markers[0];
    assert!((m.alpha - 0.5).abs() < 1e-3, "{m:?}");
    assert_eq!(
        app.world().get::<Opacity>(m.node),
        Some(&Opacity(0.0)),
        "a new marker waits unseen rather than flash beside its place"
    );
    frames(&mut app, 1);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let m = &state.markers[0];
    assert_eq!(app.world().get::<Opacity>(m.node), Some(&Opacity(m.alpha)));
    // A marker with no label is its dot, at the size it asked for and centered on its point.
    let dot = app
        .world()
        .get::<Children>(m.node)
        .and_then(|c| c.iter().next())
        .expect("dot");
    let dot_rect = app.world().get::<ComputedNode>(dot).expect("laid out").rect;
    let px = WorldMarker::default().size_px;
    assert!(close(dot_rect[2], px) && close(dot_rect[3], px), "{dot_rect:?}");
    let at = center(&app, dot);
    assert!(close(at[0], m.pos[0]) && close(at[1], m.pos[1]), "{at:?} {m:?}");
    let node = m.node;
    app.world_mut().despawn(far);
    frames(&mut app, 1);
    assert!(
        app.world().get_entity(node).is_err(),
        "the node goes with the entity"
    );
    assert!(
        app.world()
            .get::<MarkerLayerState>(layer)
            .expect("state")
            .markers
            .is_empty()
    );
}

#[test]
fn a_layer_without_a_camera_draws_nothing_and_does_not_panic() {
    let mut app = ui_app();
    let r = root(&mut app);
    let layer = sized(&mut app, r, 400.0, 300.0, MarkerLayer::default());
    app.world_mut().spawn((
        WorldMarker {
            clamp_to_edge: true,
            ..WorldMarker::default()
        },
        at3(0.0, 0.0, -5.0),
    ));
    app.world_mut()
        .spawn((TargetLock::default(), at3(0.0, 0.0, -5.0)));
    frames(&mut app, 3);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state");
    assert!(state.camera.is_none());
    assert!(state.markers.iter().all(|m| !m.visible));
    assert!(state.brackets.iter().all(|b| !b.on_screen));
}

#[test]
fn the_bracket_frames_the_targets_sphere_and_shrinks_with_distance() {
    let mut app = ui_app();
    let (layer, size) = marker_scene(&mut app);
    let near = app
        .world_mut()
        .spawn((
            TargetLock {
                radius_m: 2.0,
                min_px: 1.0,
                label: "Scout".into(),
                ..TargetLock::default()
            },
            at3(0.0, 0.0, -10.0),
        ))
        .id();
    frames(&mut app, 3);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let b = state.brackets.iter().find(|b| b.target == near).expect("bracket");
    // fov 45°: 2 m at 10 m depth, half the height is focal * 0.2 of it.
    let focal = 1.0 / (std::f32::consts::FRAC_PI_4 * 0.5).tan();
    let expect = focal * 0.2 * size[1] * 0.5;
    assert!((b.half_px - expect).abs() < 0.01, "{} vs {expect}", b.half_px);
    assert!(b.on_screen && b.time_to_intercept.is_none() && !b.no_solution);
    // The draw pass skips what is under a node with no area, so the frame's node needs one.
    let frame = app.world().get::<ComputedNode>(b.node).expect("laid out").rect;
    assert!(frame[2] > 0.0 && frame[3] > 0.0, "{frame:?}");
    // The four corners, drawn, make the frame's corners.
    let world = app.world();
    let corner = |name: &str| {
        let wanted = format!("bracket.{}.{name}", near.to_bits());
        let mut q = world
            .try_query::<(&regia_ui::NodeName, &ComputedNode)>()
            .expect("query");
        q.iter(world)
            .find(|(n, _)| n.0 == wanted)
            .map(|(_, c)| c.rect)
            .expect(&wanted)
    };
    let (cx, cy) = (size[0] * 0.5, size[1] * 0.5);
    let tl = corner("tl");
    assert!(close(tl[0], cx - expect) && close(tl[1], cy - expect), "{tl:?}");
    let br = corner("br");
    assert!(
        close(br[0] + br[2], cx + expect) && close(br[1] + br[3], cy + expect),
        "{br:?}"
    );
    assert_eq!(texts(&app, b.node), ["Scout  10 m"]);
    // Farther away it is smaller, and the smallest size holds a distant target framed.
    {
        let mut lock = app.world_mut().get_mut::<TargetLock>(near).expect("lock");
        lock.min_px = 14.0;
    }
    app.world_mut()
        .get_mut::<GlobalTransform>(near)
        .expect("t")
        .0
        .translation
        .z = -100_000.0;
    frames(&mut app, 2);
    let far = app
        .world()
        .get::<MarkerLayerState>(layer)
        .expect("state")
        .brackets[0]
        .clone();
    assert_eq!(far.half_px, 14.0);
    // Off screen the bracket hides.
    app.world_mut()
        .get_mut::<GlobalTransform>(near)
        .expect("t")
        .0
        .translation = Vec3::new(0.0, 0.0, 10.0).into();
    frames(&mut app, 1);
    let off = app
        .world()
        .get::<MarkerLayerState>(layer)
        .expect("state")
        .brackets[0]
        .clone();
    assert!(!off.on_screen);
    assert_eq!(
        app.world().get::<NodeStyle>(off.node).expect("style").0.display,
        Display::None
    );
}

#[test]
fn the_lead_pip_shows_where_to_aim_with_a_given_velocity() {
    let mut app = ui_app();
    let (layer, size) = marker_scene(&mut app);
    // Crossing to the right at 10 m/s, 100 m out, 100 m/s projectile.
    let target = app
        .world_mut()
        .spawn((
            TargetLock {
                projectile_speed: Some(100.0),
                velocity: Some([10.0, 0.0, 0.0]),
                ..TargetLock::default()
            },
            at3(0.0, 0.0, -100.0),
        ))
        .id();
    frames(&mut app, 3);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let b = state
        .brackets
        .iter()
        .find(|b| b.target == target)
        .expect("bracket");
    let time = b.time_to_intercept.expect("a solution");
    assert!((time - 1.005).abs() < 0.01, "{time}");
    let aim = b.lead_world.expect("aim");
    assert!((aim[0] - 10.0 * time).abs() < 1e-3, "{aim:?}");
    let pip = b.lead_screen.expect("the pip is on screen");
    assert!(
        pip[0] > size[0] * 0.5 + 1.0,
        "ahead of the target's motion: {pip:?}"
    );
    assert!(close(pip[1], size[1] * 0.5));
    // The drawn pip is where the readout says, and an impossible shot hides it and says so.
    let pip_node = {
        let world = app.world();
        let wanted = format!("bracket.{}.pip", target.to_bits());
        let mut q = world.try_query::<(Entity, &regia_ui::NodeName)>().expect("query");
        q.iter(world)
            .find(|(_, n)| n.0 == wanted)
            .map(|(e, _)| e)
            .expect("pip")
    };
    let drawn = center(&app, pip_node);
    assert!(
        close(drawn[0], pip[0]) && close(drawn[1], pip[1]),
        "{drawn:?} {pip:?}"
    );
    app.world_mut()
        .get_mut::<TargetLock>(target)
        .expect("lock")
        .velocity = Some([0.0, 0.0, -200.0]);
    frames(&mut app, 1);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let b = &state.brackets[0];
    assert!(b.no_solution && b.lead_screen.is_none() && b.time_to_intercept.is_none());
    assert_eq!(
        app.world().get::<NodeStyle>(pip_node).expect("style").0.display,
        Display::None
    );
}

#[test]
fn a_targets_velocity_is_estimated_from_its_motion() {
    let mut app = ui_app();
    let (layer, _) = marker_scene(&mut app);
    let target = app
        .world_mut()
        .spawn((
            TargetLock {
                projectile_speed: Some(300.0),
                ..TargetLock::default()
            },
            at3(0.0, 0.0, -50.0),
        ))
        .id();
    frames(&mut app, 3);
    let dt = 0.01;
    let mut x = 0.0;
    for _ in 0..120 {
        x += 6.0 * dt;
        app.world_mut()
            .get_mut::<GlobalTransform>(target)
            .expect("t")
            .0
            .translation
            .x = x;
        frames(&mut app, 1);
    }
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let v = state.brackets[0].velocity;
    assert!((v[0] - 6.0).abs() < 0.05, "{v:?}");
    assert!(v[1].abs() < 1e-3 && v[2].abs() < 1e-3, "{v:?}");
}

/// A target just off the right edge coming back: no frame, but the pip shows where to aim.
#[test]
fn the_pip_shows_while_the_target_itself_is_off_screen() {
    let mut app = ui_app();
    let (layer, size) = marker_scene(&mut app);
    let target = app
        .world_mut()
        .spawn((
            TargetLock {
                projectile_speed: Some(100.0),
                velocity: Some([-100.0, 0.0, 0.0]),
                ..TargetLock::default()
            },
            at3(120.0, 0.0, -100.0),
        ))
        .id();
    frames(&mut app, 4);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let b = state
        .brackets
        .iter()
        .find(|b| b.target == target)
        .expect("bracket");
    assert!(!b.on_screen, "{b:?}");
    let pip = b.lead_screen.expect("the aim point is on screen");
    assert!(pip[0] > 0.0 && pip[0] < size[0], "{pip:?}");
    let world = app.world();
    let named = |wanted: String| {
        let mut q = world.try_query::<(Entity, &regia_ui::NodeName)>().expect("query");
        q.iter(world)
            .find(|(_, n)| n.0 == wanted)
            .map(|(e, _)| e)
            .expect("node")
    };
    let display = |node: Entity| world.get::<NodeStyle>(node).expect("style").0.display;
    let id = target.to_bits();
    assert_eq!(display(b.node), Display::Flex);
    assert_eq!(display(named(format!("bracket.{id}.pip"))), Display::Flex);
    assert_eq!(display(named(format!("bracket.{id}.tl"))), Display::None);
    let drawn = center(&app, named(format!("bracket.{id}.pip")));
    assert!(
        close(drawn[0], pip[0]) && close(drawn[1], pip[1]),
        "{drawn:?} {pip:?}"
    );
}

#[test]
fn the_lead_is_relative_to_the_shooter_not_the_camera() {
    let mut app = ui_app();
    let (layer, _) = marker_scene(&mut app);
    // The shooter is the focus entity 20 m in front of the camera; the target stands still.
    app.world_mut().spawn((HudFocus, at3(0.0, 0.0, -20.0)));
    let target = app
        .world_mut()
        .spawn((
            TargetLock {
                projectile_speed: Some(40.0),
                velocity: Some([0.0; 3]),
                shooter_velocity: Some([0.0; 3]),
                ..TargetLock::default()
            },
            at3(0.0, 0.0, -100.0),
        ))
        .id();
    frames(&mut app, 3);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let b = state
        .brackets
        .iter()
        .find(|b| b.target == target)
        .expect("bracket");
    assert!(
        (b.time_to_intercept.expect("t") - 2.0).abs() < 1e-4,
        "80 m at 40 m/s: {b:?}"
    );
}

#[test]
fn a_plate_is_a_billboarded_world_root_that_follows_its_entity() {
    let mut app = ui_app();
    let plate = WorldPlate {
        size: [200.0, 50.0],
        ppm: 100.0,
        fade: Some([5.0, 9.0]),
        depth_test: true,
    };
    let source = app
        .world_mut()
        .spawn((
            WorldMarker {
                label: "Ally".into(),
                offset: [0.0, 2.0, 0.0],
                plate: Some(plate),
                ..WorldMarker::default()
            },
            at3(3.0, 0.0, -4.0),
        ))
        .id();
    frames(&mut app, 2);
    let world = app.world_mut();
    let (root, link) = world
        .query::<(Entity, &WorldPlateRoot)>()
        .iter(world)
        .next()
        .map(|(e, l)| (e, *l))
        .expect("a world root exists");
    assert_eq!(link.source, source);
    let target = world.get::<UiRoot>(root).expect("root").target;
    assert!(matches!(target, UiTarget::WorldSpace { size, ppm } if size == [200.0, 50.0] && ppm == 100.0));
    let panel = world.get::<WorldPanel>(root).expect("panel");
    assert_eq!(panel.billboard, Billboard::Full);
    assert_eq!(panel.fade, Some([5.0, 9.0]));
    assert!(panel.depth_test);
    let g = world.get::<GlobalTransform>(root).expect("placed");
    assert_eq!(Vec3::from(g.0.translation), Vec3::new(3.0, 2.0, -4.0));
    assert_eq!(texts(&app, link.text), ["Ally"]);
    // It follows, and it goes when the marker does.
    app.world_mut()
        .get_mut::<GlobalTransform>(source)
        .expect("t")
        .0
        .translation
        .x = 8.0;
    frames(&mut app, 1);
    let g = app.world().get::<GlobalTransform>(root).expect("placed");
    assert_eq!(g.0.translation.x, 8.0);
    app.world_mut().entity_mut(source).remove::<WorldMarker>();
    frames(&mut app, 1);
    assert!(app.world().get_entity(root).is_err());
}

#[test]
fn a_changed_plate_applies_in_place() {
    let mut app = ui_app();
    let source = app
        .world_mut()
        .spawn((
            WorldMarker {
                plate: Some(WorldPlate::default()),
                ..WorldMarker::default()
            },
            at3(0.0, 0.0, -5.0),
        ))
        .id();
    frames(&mut app, 2);
    let world = app.world_mut();
    let root = world
        .query_filtered::<Entity, With<WorldPlateRoot>>()
        .iter(world)
        .next()
        .expect("root");
    let wider = WorldPlate {
        size: [512.0, 64.0],
        ppm: 64.0,
        fade: None,
        depth_test: false,
    };
    app.world_mut()
        .get_mut::<WorldMarker>(source)
        .expect("marker")
        .plate = Some(wider);
    frames(&mut app, 1);
    let target = app.world().get::<UiRoot>(root).expect("the same root").target;
    assert!(matches!(target, UiTarget::WorldSpace { size, ppm } if size == [512.0, 64.0] && ppm == 64.0));
    let panel = app.world().get::<WorldPanel>(root).expect("panel");
    assert_eq!((panel.fade, panel.depth_test), (None, false));
}

/// A marker and a blip land on the same physical pixel at any UI scale: layout px and the visual
/// transform's px must agree.
#[test]
fn placement_is_right_at_a_ui_scale_that_is_not_one() {
    let mut app = ui_app();
    let window = app.world().resource::<UiScales>().window_size;
    let r = app
        .world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ScaleWithScreen {
                    reference: [window[0] * 0.5, window[1] * 0.5],
                    match_width: 0.5,
                },
                ..Default::default()
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    app.world_mut()
        .spawn((Camera::default(), GlobalTransform(Affine3A::IDENTITY)));
    let layer = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
            MarkerLayer::default(),
        ))
        .id();
    app.world_mut().entity_mut(r).add_child(layer);
    let radar = sized(
        &mut app,
        r,
        100.0,
        100.0,
        Radar {
            center: [0.0, 0.0],
            ..Radar::with_range(10.0)
        },
    );
    let target = app
        .world_mut()
        .spawn((WorldMarker::default(), at3(0.0, 0.0, -10.0)))
        .id();
    let blip = app.world_mut().spawn((RadarBlip::default(), at(5.0, 0.0))).id();
    frames(&mut app, 4);
    let scale = app.world().get::<ComputedNode>(r).expect("root").scale;
    assert!((scale - 2.0).abs() < 1e-3, "{scale}");
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    let marker = state.markers.iter().find(|m| m.source == target).expect("marker");
    // In UI px on the layer, half the window.
    assert!(close(state.size[0], window[0] * 0.5), "{:?}", state.size);
    assert!(close(marker.pos[0], window[0] * 0.25), "{marker:?}");
    // Drawn at the window's center, in physical px.
    let rect = app
        .world()
        .get::<ComputedNode>(marker.node)
        .expect("laid out")
        .rect;
    assert!(close(rect[0] + rect[2] * 0.5, window[0] * 0.5), "{rect:?}");
    assert!(close(rect[1] + 6.0 * scale, window[1] * 0.5), "{rect:?}");
    // The blip: 5 m of 10 m east of the center of a 100 UI px radar.
    let radar_state = app.world().get::<RadarState>(radar).expect("state").clone();
    assert_eq!(radar_state.size, [100.0, 100.0]);
    let shown = radar_state.blips.iter().find(|b| b.source == blip).expect("blip");
    assert!(close(shown.pos[0], 75.0), "{shown:?}");
    let drawn = center(&app, shown.node);
    assert!(
        close(drawn[0], 150.0) && close(drawn[1], 100.0),
        "physical px: {drawn:?}"
    );
}

/// A layer under a split-screen camera's root projects through that camera, onto its viewport.
#[test]
fn a_layer_under_a_camera_root_follows_that_camera() {
    let mut app = ui_app();
    let window = app.world().resource::<UiScales>().window_size;
    app.world_mut()
        .spawn((Camera::default(), GlobalTransform(Affine3A::IDENTITY)));
    // The right half of the window, looking along +X.
    let split = app
        .world_mut()
        .spawn((
            Camera {
                viewport: [0.5, 0.0, 0.5, 1.0],
                order: 1,
                ..Camera::default()
            },
            GlobalTransform(Affine3A::from_rotation_y(-std::f32::consts::FRAC_PI_2)),
        ))
        .id();
    let root = app
        .world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..UiRoot::camera(split)
            },
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    let layer = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
            MarkerLayer::default(),
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(layer);
    let ahead = app
        .world_mut()
        .spawn((WorldMarker::default(), at3(10.0, 0.0, 0.0)))
        .id();
    frames(&mut app, 3);
    let state = app.world().get::<MarkerLayerState>(layer).expect("state").clone();
    assert_eq!(state.camera, Some(split));
    assert!(close(state.size[0], window[0] * 0.5), "{:?}", state.size);
    let m = state.markers.iter().find(|m| m.source == ahead).expect("marker");
    assert!(m.on_screen, "ahead of this camera: {m:?}");
    assert!(close(m.pos[0], window[0] * 0.25), "{m:?}");
}

/// Two layers show the same marker independently, and a layer mask keeps one out.
#[test]
fn layers_are_independent_and_masks_filter() {
    let mut app = ui_app();
    let (first, _) = marker_scene(&mut app);
    let r = root(&mut app);
    let second = sized(
        &mut app,
        r,
        300.0,
        300.0,
        MarkerLayer {
            layer_mask: 2,
            ..MarkerLayer::default()
        },
    );
    let both = app
        .world_mut()
        .spawn((
            WorldMarker {
                layer: 3,
                ..WorldMarker::default()
            },
            at3(0.0, 0.0, -5.0),
        ))
        .id();
    let only_first = app
        .world_mut()
        .spawn((WorldMarker::default(), at3(1.0, 0.0, -5.0)))
        .id();
    frames(&mut app, 3);
    let a = app.world().get::<MarkerLayerState>(first).expect("state");
    assert_eq!(a.markers.len(), 2);
    let b = app.world().get::<MarkerLayerState>(second).expect("state");
    assert_eq!(b.markers.len(), 1);
    assert_eq!(b.markers[0].source, both);
    assert!(b.markers.iter().all(|m| m.source != only_first));
    assert_ne!(
        a.markers.iter().find(|m| m.source == both).expect("first").node,
        b.markers[0].node,
    );
}

#[test]
fn a_plate_marker_is_not_drawn_on_a_layer() {
    let mut app = ui_app();
    let (layer, _) = marker_scene(&mut app);
    app.world_mut().spawn((
        WorldMarker {
            plate: Some(WorldPlate::default()),
            ..WorldMarker::default()
        },
        at3(0.0, 0.0, -5.0),
    ));
    frames(&mut app, 2);
    assert!(
        app.world()
            .get::<MarkerLayerState>(layer)
            .expect("state")
            .markers
            .is_empty()
    );
}

// ---- Scoreboard --------------------------------------------------------------------------------

fn table(rows: usize) -> ScoreTable {
    ScoreTable {
        columns: vec![ScoreColumn::new("Tags"), ScoreColumn::new("Tagged")],
        rows: (0..rows)
            .map(|i| ScoreRow {
                id: i as u64,
                name: format!("P{i}"),
                team: None,
                local: i == 1,
                values: vec![ScoreValue::Int(i as i64), ScoreValue::Int(10 - i as i64)],
            })
            .collect(),
        teams: Vec::new(),
    }
}

fn board(app: &mut App, rows: usize) -> Entity {
    let r = root(app);
    sized(
        app,
        r,
        500.0,
        400.0,
        Scoreboard {
            title: "Scores".into(),
            table: table(rows),
            from_rules: false,
            ..Scoreboard::default()
        },
    )
}

#[test]
fn a_board_lists_players_sorted_and_highlights_the_local_one() {
    let mut app = ui_app();
    let b = board(&mut app, 3);
    frames(&mut app, 3);
    assert_eq!(
        texts(&app, b),
        [
            "Scores",
            "Tags \u{25BC}",
            "Tagged",
            "P2",
            "2",
            "8",
            "P1",
            "1",
            "9",
            "P0",
            "0",
            "10"
        ]
    );
    let state = app.world().get::<ScoreboardState>(b).expect("state");
    assert!(state.shown);
    assert_eq!(state.lines.len(), 3);
    // The local player's row has its fill; another's is clear.
    let world = app.world();
    let fill = |name: &str| {
        let mut q = world
            .try_query::<(&regia_ui::NodeName, &Background)>()
            .expect("query");
        q.iter(world)
            .find(|(n, _)| n.0 == name)
            .map(|(_, b)| b.kind.clone())
    };
    let is_clear = |k: Option<BackgroundKind>| matches!(k, Some(BackgroundKind::Color(c)) if c.0[3] == 0.0);
    assert!(is_clear(fill("scoreboard.row.0")));
    assert!(
        !is_clear(fill("scoreboard.row.1")),
        "row 1 is P1, the local player"
    );
}

#[test]
fn clicking_a_heading_sorts_by_it_and_again_flips() {
    let mut app = ui_app();
    let b = board(&mut app, 3);
    frames(&mut app, 3);
    call(
        app.world_mut(),
        "ui.click",
        &json!({"name": "scoreboard.sort.tagged"}),
    )
    .expect("click");
    frames(&mut app, 3);
    {
        let board = app.world().get::<Scoreboard>(b).expect("board");
        assert_eq!((board.sort_column, board.descending), (Some(1), true));
    }
    let t = texts(&app, b);
    assert_eq!(&t[1..3], ["Tags", "Tagged \u{25BC}"]);
    assert_eq!(t[3], "P0", "{t:?}");
    call(
        app.world_mut(),
        "ui.click",
        &json!({"name": "scoreboard.sort.tagged"}),
    )
    .expect("click");
    frames(&mut app, 3);
    let t = texts(&app, b);
    assert_eq!(t[2], "Tagged \u{25B2}");
    assert_eq!(t[3], "P2", "{t:?}");
}

#[test]
fn a_held_board_shows_only_while_its_action_is_down() {
    let mut app = ui_app();
    let r = root(&mut app);
    let b = sized(
        &mut app,
        r,
        300.0,
        200.0,
        Scoreboard {
            table: table(1),
            from_rules: false,
            ..Scoreboard::held("Scores")
        },
    );
    // The widgets rewrite the virtual buttons each frame, so the hold is written after them.
    #[derive(Resource, Default)]
    struct Held(bool);
    fn hold(held: Res<Held>, mut buttons: ResMut<VirtualButtons>) {
        buttons.0.insert("Scores".to_owned(), held.0);
    }
    app.init_resource::<Held>()
        .add_systems(Update, hold.after(regia_ui::widgets::ui_widgets));
    frames(&mut app, 2);
    assert_eq!(
        app.world().get::<NodeStyle>(b).expect("style").0.display,
        Display::None
    );
    assert!(!app.world().get::<ScoreboardState>(b).expect("state").shown);
    app.world_mut().resource_mut::<Held>().0 = true;
    frames(&mut app, 2);
    assert_eq!(
        app.world().get::<NodeStyle>(b).expect("style").0.display,
        Display::Flex
    );
    assert!(app.world().get::<ScoreboardState>(b).expect("state").shown);
    app.world_mut().resource_mut::<Held>().0 = false;
    frames(&mut app, 2);
    assert!(!app.world().get::<ScoreboardState>(b).expect("state").shown);
}

#[test]
fn rows_and_columns_may_change_and_the_row_cap_holds() {
    let mut app = ui_app();
    let b = board(&mut app, 2);
    frames(&mut app, 3);
    // Fewer rows hide the rest.
    app.world_mut().get_mut::<Scoreboard>(b).expect("board").table = table(1);
    frames(&mut app, 2);
    assert_eq!(texts(&app, b).len(), 3 + 3);
    // A new column set rebuilds.
    {
        let mut board = app.world_mut().get_mut::<Scoreboard>(b).expect("board");
        board.table.columns = vec![ScoreColumn::new("Ping")];
        board.table.rows[0].values = vec![ScoreValue::Text("12 ms".into())];
        board.sort_column = None;
    }
    frames(&mut app, 2);
    assert_eq!(texts(&app, b), ["Scores", "Ping", "P0", "12 ms"]);
    // More than the cap draws the cap.
    let over = MAX_SCORE_ROWS + 25;
    app.world_mut().get_mut::<Scoreboard>(b).expect("board").table = table(over);
    frames(&mut app, 2);
    let state = app.world().get::<ScoreboardState>(b).expect("state");
    assert_eq!(state.lines.len(), MAX_SCORE_ROWS);
    assert_eq!(state.dropped, 25);
}

/// A layer or a radar authored with no size still draws: everything in them is positioned by hand,
/// so an automatic size would lay out at zero and show nothing, silently.
#[test]
fn a_marker_layer_and_a_radar_authored_without_a_size_show_their_contents() {
    let mut app = ui_app();
    let src = format!(
        r#"(version: {UI_RON_VERSION}, root: (children: [
            (name: "markers", widget: marker_layer(())),
            (name: "radar", widget: radar((range: 50.0))),
        ]))"#
    );
    let file = parse_ui_ron(&src, "unsized", &UiMigrators::default()).expect("parse");
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert("unsized".to_owned(), file);
    spawn_ui(app.world_mut(), "unsized").expect("spawns");
    app.world_mut()
        .spawn((Camera::default(), GlobalTransform(Affine3A::IDENTITY)));
    let target = app
        .world_mut()
        .spawn((WorldMarker::default(), at3(0.0, 0.0, -10.0)))
        .id();
    let blip = app.world_mut().spawn((RadarBlip::default(), at(10.0, 0.0))).id();
    frames(&mut app, 4);
    let window = app.world().resource::<UiScales>().window_size;
    let world = app.world_mut();
    let (layer_state, layer_px) = world
        .query::<(&MarkerLayerState, &ComputedNode)>()
        .iter(world)
        .next()
        .map(|(s, c)| (s.clone(), [c.rect[2], c.rect[3]]))
        .expect("layer");
    assert!(
        close(layer_px[0], window[0]) && close(layer_px[1], window[1]),
        "the layer fills its root: {layer_px:?} {window:?}"
    );
    let marker = layer_state
        .markers
        .iter()
        .find(|m| m.source == target)
        .expect("marker");
    assert!(marker.on_screen && marker.visible, "{marker:?}");
    let radar_state = world
        .query::<&RadarState>()
        .iter(world)
        .next()
        .cloned()
        .expect("radar");
    assert!(
        close(radar_state.size[0], 200.0) && close(radar_state.size[1], 200.0),
        "{:?}",
        radar_state.size
    );
    assert!(
        radar_state.blips.iter().any(|b| b.source == blip),
        "{radar_state:?}"
    );
}

/// A new width reaches the nodes of the rows and headings already built.
#[test]
fn a_column_width_change_reaches_the_board() {
    let mut app = ui_app();
    let n = board(&mut app, 2);
    frames(&mut app, 3);
    let widths = |app: &mut App| -> Vec<f32> {
        let world = app.world_mut();
        let mut q = world.query::<(&regia_ui::NodeName, &ComputedNode)>();
        q.iter(world)
            .filter(|(name, _)| name.0.starts_with("scoreboard.sort."))
            .map(|(_, c)| c.rect[2])
            .collect()
    };
    let before = widths(&mut app);
    assert!(!before.is_empty());
    {
        let mut board = app.world_mut().get_mut::<Scoreboard>(n).expect("board");
        for column in &mut board.table.columns {
            column.width = 200.0;
        }
    }
    frames(&mut app, 3);
    let after = widths(&mut app);
    assert!(after.iter().all(|w| close(*w, 200.0)), "{before:?} -> {after:?}");
}

/// Names come from players; only words the game authored may be localization keys.
#[test]
fn a_player_or_lobby_named_like_a_localization_key_shows_as_that() {
    let mut app = ui_app();
    let n = board(&mut app, 1);
    app.world_mut()
        .get_mut::<Scoreboard>(n)
        .expect("board")
        .table
        .rows[0]
        .name = "loc:menu-start".into();
    frames(&mut app, 3);
    assert!(
        texts(&app, n).contains(&"loc:menu-start".to_owned()),
        "{:?}",
        texts(&app, n)
    );
    let l = list(&mut app);
    app.world_mut().get_mut::<LobbyList>(l).expect("list").rows[0].name = "loc:menu-start".into();
    frames(&mut app, 3);
    assert!(
        texts(&app, l).contains(&"loc:menu-start".to_owned()),
        "{:?}",
        texts(&app, l)
    );
}

#[test]
fn an_idle_board_runs_no_layout() {
    let mut app = ui_app();
    board(&mut app, 8);
    frames(&mut app, 6);
    let before = *app.world().resource::<UiLayoutStats>();
    frames(&mut app, 10);
    let after = *app.world().resource::<UiLayoutStats>();
    assert_eq!(
        (after.full_runs, after.incremental_runs),
        (before.full_runs, before.incremental_runs),
    );
}

// ---- Lobby list ---------------------------------------------------------------------------------

fn lobby_rows() -> Vec<LobbyRow> {
    vec![
        LobbyRow {
            id: 41,
            name: "Friday night".into(),
            detail: "arena".into(),
            players: 3,
            max_players: 8,
            joinable: true,
        },
        LobbyRow {
            id: 42,
            name: String::new(),
            detail: String::new(),
            players: 8,
            max_players: 8,
            joinable: false,
        },
    ]
}

/// Every request a list sent. Messages live two frames, so a system keeps them for the test.
#[derive(Resource, Default)]
struct Requests(Vec<LobbyListRequest>);

fn keep_requests(mut seen: ResMut<Requests>, mut requests: MessageReader<LobbyListRequest>) {
    seen.0.extend(requests.read().copied());
}

fn list(app: &mut App) -> Entity {
    app.init_resource::<Requests>().add_systems(Update, keep_requests);
    let r = root(app);
    sized(
        app,
        r,
        600.0,
        400.0,
        LobbyList {
            rows: lobby_rows(),
            manual: true,
            ..LobbyList::default()
        },
    )
}

fn requests(app: &mut App) -> Vec<LobbyListRequest> {
    std::mem::take(&mut app.world_mut().resource_mut::<Requests>().0)
}

#[test]
fn the_list_draws_its_rows_and_a_row_click_asks_to_join() {
    let mut app = ui_app();
    let l = list(&mut app);
    frames(&mut app, 3);
    assert_eq!(
        texts(&app, l),
        [
            "Lobbies",
            "Refresh",
            "Create",
            "Friday night",
            "arena",
            "3/8",
            "Lobby 42",
            "8/8"
        ]
    );
    call(app.world_mut(), "ui.click", &json!({"name": "lobby_list.row.0"})).expect("click");
    frames(&mut app, 3);
    let got = requests(&mut app);
    assert_eq!(
        got,
        [LobbyListRequest {
            list: l,
            action: LobbyListAction::Join(41)
        }]
    );
    call(
        app.world_mut(),
        "ui.click",
        &json!({"name": "lobby_list.refresh"}),
    )
    .expect("click");
    call(app.world_mut(), "ui.click", &json!({"name": "lobby_list.create"})).expect("click");
    // Each queued click takes two frames, then the request's frame.
    frames(&mut app, 5);
    let got: Vec<LobbyListAction> = requests(&mut app).into_iter().map(|r| r.action).collect();
    assert_eq!(got, [LobbyListAction::Refresh, LobbyListAction::Create]);
}

#[test]
fn a_full_lobby_row_is_disabled_and_clicks_on_it_ask_nothing() {
    let mut app = ui_app();
    list(&mut app);
    frames(&mut app, 3);
    let world = app.world_mut();
    let full = world
        .query::<(Entity, &regia_ui::NodeName)>()
        .iter(world)
        .find(|(_, n)| n.0 == "lobby_list.row.1")
        .map(|(e, _)| e)
        .expect("row");
    assert!(app.world().get::<Disabled>(full).is_some());
    let _ = call(app.world_mut(), "ui.click", &json!({"name": "lobby_list.row.1"}));
    frames(&mut app, 3);
    assert!(requests(&mut app).is_empty());
    // A row that opens up is enabled again.
    let world = app.world_mut();
    let list_node = world
        .query_filtered::<Entity, With<LobbyList>>()
        .iter(world)
        .next()
        .expect("list");
    world.get_mut::<LobbyList>(list_node).expect("list").rows[1].joinable = true;
    frames(&mut app, 2);
    assert!(app.world().get::<Disabled>(full).is_none());
}

#[test]
fn in_a_lobby_it_shows_the_members_and_the_ready_button_tracks_the_local_flag() {
    let mut app = ui_app();
    let l = list(&mut app);
    {
        let mut widget = app.world_mut().get_mut::<LobbyList>(l).expect("list");
        widget.current = Some(CurrentLobby {
            id: 7,
            name: "Mine".into(),
            max_players: 4,
            members: vec![
                LobbyMemberRow {
                    id: 1,
                    name: "Ann".into(),
                    ready: false,
                    owner: true,
                    local: true,
                },
                LobbyMemberRow {
                    id: 2,
                    name: String::new(),
                    ready: true,
                    owner: false,
                    local: false,
                },
            ],
            is_owner: true,
            can_launch: false,
            launched: false,
        });
    }
    frames(&mut app, 3);
    let t = texts(&app, l);
    assert_eq!(
        t,
        [
            "Lobbies",
            "Mine  2/4",
            "Ready",
            "Start match",
            "Leave",
            "Ann",
            "host",
            "Player 2",
            "ready"
        ],
    );
    // Launch is disabled until the owner can launch.
    let _ = call(app.world_mut(), "ui.click", &json!({"name": "lobby_list.launch"}));
    frames(&mut app, 2);
    assert!(requests(&mut app).is_empty());
    call(app.world_mut(), "ui.click", &json!({"name": "lobby_list.ready"})).expect("click");
    frames(&mut app, 3);
    let got: Vec<LobbyListAction> = requests(&mut app).into_iter().map(|r| r.action).collect();
    assert_eq!(got, [LobbyListAction::SetReady(true)]);
    {
        let mut widget = app.world_mut().get_mut::<LobbyList>(l).expect("list");
        let lobby = widget.current.as_mut().expect("in");
        lobby.members[0].ready = true;
        lobby.can_launch = true;
    }
    frames(&mut app, 3);
    assert!(texts(&app, l).contains(&"Not ready".to_owned()));
    call(app.world_mut(), "ui.click", &json!({"name": "lobby_list.launch"})).expect("click");
    frames(&mut app, 3);
    call(app.world_mut(), "ui.click", &json!({"name": "lobby_list.ready"})).expect("click");
    frames(&mut app, 3);
    let got: Vec<LobbyListAction> = requests(&mut app).into_iter().map(|r| r.action).collect();
    assert_eq!(got, [LobbyListAction::Launch, LobbyListAction::SetReady(false)]);
}

#[test]
fn the_status_line_says_empty_busy_or_the_error_code() {
    let mut app = ui_app();
    let l = list(&mut app);
    {
        let mut widget = app.world_mut().get_mut::<LobbyList>(l).expect("list");
        widget.rows.clear();
    }
    frames(&mut app, 2);
    assert!(texts(&app, l).contains(&"No lobbies found".to_owned()));
    app.world_mut().get_mut::<LobbyList>(l).expect("list").status = LobbyListStatus::Busy;
    frames(&mut app, 2);
    assert!(texts(&app, l).contains(&"Searching...".to_owned()));
    app.world_mut().get_mut::<LobbyList>(l).expect("list").status =
        LobbyListStatus::Error("lobby_full".into());
    frames(&mut app, 2);
    assert!(texts(&app, l).contains(&"lobby_full".to_owned()));
}

// ---- Authoring -----------------------------------------------------------------------------------

const WIDGETS_RON: &str = r#"(
    version: 6,
    root: (
        style: (size: (Percent(100.0), Percent(100.0))),
        children: [
            (
                name: "lobbies",
                style: (size: (Points(600.0), Points(400.0))),
                widget: lobby_list((title: "Matches", refresh: "loc:refresh")),
            ),
            (
                name: "scores",
                widget: scoreboard((
                    title: "Scores",
                    columns: [(key: "tags", title: "Tags", width: 64.0)],
                    hold_action: "Scores",
                )),
            ),
            (
                name: "radar",
                style: (size: (Points(200.0), Points(200.0))),
                widget: radar((range: 75.0, rotate: true, max_blips: 16)),
            ),
            (
                name: "minimap",
                style: (size: (Points(200.0), Points(200.0))),
                widget: radar((
                    world_rect: (-50.0, -50.0, 50.0, 50.0),
                    map_image: 77,
                    show_north: false,
                )),
            ),
            (
                name: "markers",
                widget: marker_layer((edge_margin: 40.0, layer_mask: 5, max_markers: 9)),
            ),
        ],
    ),
)"#;

fn named(app: &mut App, name: &str) -> Entity {
    let world = app.world_mut();
    world
        .query::<(Entity, &regia_ui::NodeName)>()
        .iter(world)
        .find(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no node {name}"))
}

#[test]
fn ui_ron_authors_every_game_widget() {
    let file = parse_ui_ron(WIDGETS_RON, "widgets.ui.ron", &UiMigrators::default()).expect("parses");
    assert_eq!(file.version, UI_RON_VERSION);
    assert!(matches!(
        file.root.children[0].widget,
        Some(WidgetDef::LobbyList(_))
    ));
    // The writer's text reads back as the same tree.
    let written = write_ui_ron(&file).expect("writes");
    assert_eq!(
        parse_ui_ron(&written, "written", &UiMigrators::default()).expect("reads back"),
        file,
        "{written}"
    );

    let mut app = ui_app();
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert("widgets".to_owned(), file);
    let _root = spawn_ui(app.world_mut(), "widgets").expect("spawns");
    frames(&mut app, 3);

    let n = named(&mut app, "lobbies");
    let list = app.world().get::<LobbyList>(n).expect("list");
    assert_eq!(list.labels.title, "Matches");
    assert_eq!(list.labels.refresh, "loc:refresh");
    assert_eq!(list.labels.create, "Create", "an empty word keeps the default");

    let n = named(&mut app, "scores");
    let board = app.world().get::<Scoreboard>(n).expect("board");
    assert_eq!(board.title, "Scores");
    assert_eq!(board.table.columns[0].key, "tags");
    assert_eq!(board.hold_action, "Scores");
    assert!(!board.visible, "a board with a hold action starts hidden");
    assert!(board.from_rules);

    let n = named(&mut app, "radar");
    let radar = app.world().get::<Radar>(n).expect("radar");
    assert_eq!(radar.mode, RadarMode::Radar { range: 75.0 });
    assert!(radar.rotate);
    assert_eq!(radar.max_blips, 16);
    assert_eq!(radar.shape, RadarShape::Circle);

    let n = named(&mut app, "minimap");
    let map = app.world().get::<Radar>(n).expect("minimap");
    assert_eq!(
        map.mode,
        RadarMode::Minimap {
            world_rect: [-50.0, -50.0, 50.0, 50.0]
        }
    );
    assert_eq!(map.shape, RadarShape::Rect);
    assert_eq!(map.map.expect("map").image, 77);
    assert_eq!(map.map.expect("map").world_rect, [-50.0, -50.0, 50.0, 50.0]);

    let n = named(&mut app, "markers");
    let layer = app.world().get::<MarkerLayer>(n).expect("layer");
    assert_eq!(
        (layer.edge_margin, layer.layer_mask, layer.max_markers),
        (40.0, 5, 9)
    );
    assert!(
        app.world().get::<PickThrough>(n).is_some(),
        "the overlay takes no clicks"
    );
}

#[test]
fn preview_samples_fill_empty_widgets_and_leave_full_ones_alone() {
    let mut app = ui_app();
    let file = parse_ui_ron(WIDGETS_RON, "widgets.ui.ron", &UiMigrators::default()).expect("parses");
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert("widgets".to_owned(), file);
    spawn_ui(app.world_mut(), "widgets").expect("spawns");
    fill_preview_samples(app.world_mut());
    frames(&mut app, 3);
    let n = named(&mut app, "scores");
    let board = app.world().get::<Scoreboard>(n).expect("board");
    assert_eq!(board.table.rows.len(), 4);
    assert_eq!(
        board.table.rows[0].values.len(),
        1,
        "one value per authored column"
    );
    let n = named(&mut app, "lobbies");
    assert_eq!(app.world().get::<LobbyList>(n).expect("list").rows.len(), 3);
    let n = named(&mut app, "minimap");
    assert_eq!(app.world().get::<RadarState>(n).expect("state").blips.len(), 4);
    // A second fill changes nothing.
    fill_preview_samples(app.world_mut());
    let world = app.world_mut();
    assert_eq!(world.query::<&RadarBlip>().iter(world).count(), 4);
}
