// SPDX-License-Identifier: MPL-2.0
//! The Create menu's actor entries and their viewport helpers (`docs/specs/19-editor.md` § 2
//! Create menu, § 3 helpers; ADR-7468). CPU only: the overlays are rasterized in-test.
//!
//! - Every actor entry of the menu's action registry is `editor.create_entity { kind }`, and the
//!   method makes what the entry names: the component, the label as the name, the viewport's
//!   focus point, the selection, one undo step.
//! - A player start draws a flag and a floor arrow, a trigger volume (sensor collider) a dashed
//!   outline, with the icons on and nothing with them off.

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_editor::command::UndoStack;
use regia_editor::selection::Selection;
use regia_editor::views::{ShowFlags, ViewCamera, ViewKind};
use regia_scene::prelude::Transform;
use serde_json::json;

/// The player start's helper color (`overlays::ICON_PLAYER_START`).
const PLAYER_START_RGB: (u8, u8, u8) = (150, 255, 40);
/// The trigger volume's helper color (`overlays::TRIGGER_VOLUME`).
const TRIGGER_RGB: (u8, u8, u8) = (255, 60, 200);

fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    app.world_mut()
        .insert_resource(regia_editor::viewport::EditorCamera {
            target: Vec3::new(1.5, 0.0, -2.0),
            ..Default::default()
        });
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

#[test]
fn every_create_actor_entry_is_create_entity_and_makes_what_it_names() {
    let mut app = edit_app();
    let (_, handler) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.create_entity")
        .expect("editor.create_entity is registered");
    let entries = [
        ("player_start", "Player start"),
        ("trigger_volume", "Trigger volume"),
        ("point_light", "Point light"),
        ("spot_light", "Spot light"),
        ("directional_light", "Directional light"),
        ("camera", "Camera"),
        ("audio_emitter", "Audio emitter"),
        ("rect_light", "Rect light"),
        ("tube_light", "Tube light"),
        ("decal", "Decal"),
        ("post_process_volume", "Post-process volume"),
        ("spline", "Spline"),
        ("nav_link", "Nav link"),
        ("smart_object", "Smart object"),
    ];
    for (kind, label) in entries {
        let action = regia_editor::agent_parity::find(&format!("create.{kind}"))
            .unwrap_or_else(|| panic!("the Create menu has no {kind} entry"));
        assert_eq!(action.label, label);
        assert_eq!(action.panel, "shell.create");
        assert_eq!(
            action.path,
            regia_editor::agent_parity::ActionPath::Method("editor.create_entity")
        );
        let depth = app.world().resource::<UndoStack>().undo_depth();
        let reply = handler(app.world_mut(), &json!({ "kind": kind })).expect("created");
        assert_eq!(reply["name"], label);
        assert_eq!(reply["undo_depth"], depth + 1, "{kind}: one undo step");
        let entity = Entity::try_from_bits(reply["entity"].as_u64().expect("id")).expect("entity");
        let world = app.world();
        assert_eq!(
            world.get::<Transform>(entity).map(|t| t.translation),
            Some(Vec3::new(1.5, 0.0, -2.0)),
            "{kind}: at the focus point"
        );
        assert_eq!(
            world.resource::<Selection>().primary(),
            Some(entity),
            "{kind}: selected"
        );
        let has = match kind {
            "player_start" => world
                .get::<regia_framework::player::PlayerStart>(entity)
                .is_some(),
            "trigger_volume" => world
                .get::<regia_physics::Collider>(entity)
                .is_some_and(|c| c.sensor),
            "point_light" => world.get::<regia_render::PointLight>(entity).is_some(),
            "spot_light" => world.get::<regia_render::SpotLight>(entity).is_some(),
            "directional_light" => world.get::<regia_render::DirectionalLight>(entity).is_some(),
            "camera" => world.get::<regia_render::Camera>(entity).is_some(),
            "audio_emitter" => world.get::<regia_audio::AudioEmitter>(entity).is_some(),
            "rect_light" => world.get::<regia_render::RectLight>(entity).is_some(),
            "tube_light" => world.get::<regia_render::TubeLight>(entity).is_some(),
            "decal" => world.get::<regia_vfx::StaticDecal>(entity).is_some(),
            "post_process_volume" => world.get::<regia_render::PostVolume>(entity).is_some(),
            "spline" => world
                .get::<regia_scene::spline::Spline>(entity)
                .is_some_and(|s| s.points.len() == 2),
            "nav_link" => world.get::<regia_nav::NavLinkAnchor>(entity).is_some(),
            "smart_object" => world.get::<regia_ai::SmartObject>(entity).is_some(),
            other => panic!("no component check for {other}"),
        };
        assert!(has, "{kind}: the component");
    }
    // Explicit placement and refusals.
    let reply = handler(
        app.world_mut(),
        &json!({ "kind": "player_start", "translation": [4.0, 0.5, 1.0] }),
    )
    .expect("placed");
    let entity = Entity::try_from_bits(reply["entity"].as_u64().expect("id")).expect("entity");
    assert_eq!(
        app.world().get::<Transform>(entity).map(|t| t.translation),
        Some(Vec3::new(4.0, 0.5, 1.0))
    );
    let err = handler(app.world_mut(), &json!({ "kind": "reflection_probe" })).expect_err("no such kind");
    assert_eq!(
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.as_str()),
        Some("create_unknown_kind")
    );
    // Undo removes the last one.
    assert!(regia_editor::command::undo_world(app.world_mut()));
    assert!(app.world().get_entity(entity).is_err());
}

/// Pixels of `rgb` after rasterizing the pane's overlays.
fn helper_pixels(world: &World, show: ShowFlags, rgb: (u8, u8, u8)) -> (usize, Option<Vec2>) {
    let camera = regia_editor::viewport::EditorCamera::default();
    let view = ViewCamera::of(ViewKind::Perspective, &camera);
    let size = Vec2::new(320.0, 240.0);
    let shapes = regia_editor::overlays::build_overlays(world, &view, ViewKind::Perspective, size, show);
    let mut image = egui::ColorImage::new([320, 240], vec![egui::Color32::BLACK; 320 * 240]);
    regia_editor::overlays::paint_shapes(&mut image, &shapes);
    let want = egui::Color32::from_rgb(rgb.0, rgb.1, rgb.2);
    let mut count = 0;
    let mut sum = Vec2::ZERO;
    for (i, p) in image.pixels.iter().enumerate() {
        if *p == want {
            count += 1;
            sum += Vec2::new((i % 320) as f32, (i / 320) as f32);
        }
    }
    (count, (count > 0).then(|| sum / count as f32))
}

#[test]
fn player_start_and_trigger_volume_helpers_draw_with_the_icons() {
    let mut world = World::new();
    let start = Transform::from_translation(Vec3::new(-1.0, 0.0, 0.0));
    world.spawn((
        start,
        regia_scene::GlobalTransform(start.compute_affine()),
        regia_framework::player::PlayerStart,
    ));
    let trigger = Transform::from_translation(Vec3::new(1.5, 0.5, 0.0));
    world.spawn((
        trigger,
        regia_scene::GlobalTransform(trigger.compute_affine()),
        regia_physics::Collider {
            shape: regia_physics::ColliderShape::Box {
                half_extents: Vec3::splat(0.75),
            },
            sensor: true,
            ..regia_physics::Collider::default()
        },
    ));
    let on = ShowFlags {
        grid: false,
        ..ShowFlags::default()
    };
    let off = ShowFlags { icons: false, ..on };
    let (start_on, start_center) = helper_pixels(&world, on, PLAYER_START_RGB);
    let (trigger_on, trigger_center) = helper_pixels(&world, on, TRIGGER_RGB);
    assert!(start_on > 40, "the player start's flag and arrow: {start_on}");
    assert!(trigger_on > 40, "the trigger volume's dashed box: {trigger_on}");
    assert_eq!(helper_pixels(&world, off, PLAYER_START_RGB).0, 0, "icons off");
    assert_eq!(helper_pixels(&world, off, TRIGGER_RGB).0, 0, "icons off");
    // Each helper sits on its own entity: the player start left of the trigger volume.
    let (start_center, trigger_center) = (start_center.expect("start"), trigger_center.expect("trigger"));
    assert!(
        start_center.x < trigger_center.x,
        "{start_center} left of {trigger_center}"
    );
    // The Colliders flag draws sensors solid in its own color instead of the dashed helper.
    let colliders = ShowFlags {
        colliders: true,
        ..on
    };
    assert_eq!(helper_pixels(&world, colliders, TRIGGER_RGB).0, 0);
}

/// Show ▸ Navigation's agent path: `editor.set_viewport { navigation }` sets and reports the flag
/// (off by default); the pixels are checked in `create_helpers_capture` and the overlays unit tests.
#[test]
fn set_viewport_reports_and_sets_the_navigation_flag() {
    let mut app = edit_app();
    let (_, handler) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method("editor.set_viewport")
        .expect("registered");
    let read = handler(app.world_mut(), &json!({})).expect("read");
    assert_eq!(read["navigation"], false, "off by default: {read}");
    let set = handler(app.world_mut(), &json!({ "navigation": true })).expect("set");
    assert_eq!(set["navigation"], true);
    let action = regia_editor::agent_parity::find("viewport.show_navigation").expect("the Show menu entry");
    assert_eq!(
        action.path,
        regia_editor::agent_parity::ActionPath::Method("editor.set_viewport")
    );
}
