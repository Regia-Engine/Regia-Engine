// SPDX-License-Identifier: MPL-2.0
//! Procedural viewport tools (spec 67 § 5.2 and § 2.4): mouse gestures through the scripted
//! session, spline handle drags, and graph-panel actions. Headless: synthetic rays and fixed
//! calls, no GPU and no wall clock.

use std::collections::BTreeMap;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_editor::command::{UndoStack, undo_world};
use regia_editor::procgen::presets::{TOOL_FOREST_AREA_PATH, TOOL_ROAD_PATH};
use regia_editor::procgen::tools::{self, PointerPhase, ToolEvent, ToolGesture, ToolKind};
use regia_editor::procgen::view::{self, HandleGrip};
use regia_procgen::world::{ProcGenPlugin, ProcGenSettings, ProcGraphSource};
use regia_scene::components::{EntityGuid, Generated, Name};
use regia_scene::prelude::{Transform, snapshot_scene};
use regia_scene::spawn::SnapshotFilter;
use regia_scene::spline::{Spline, SplinePoint};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(ProcGenSettings {
        inline: true,
        source: ProcGraphSource::Memory(BTreeMap::new()),
        ..ProcGenSettings::default()
    });
    app.add_plugins(ProcGenPlugin);
    app.init_resource::<UndoStack>();
    regia_editor::procgen::install(app.world_mut());
    app.finish_plugins();
    app.update();
    app
}

fn ground(app: &mut App) {
    let (vertices, indices) = regia_render::box_mesh([20.0, 0.5, 20.0]);
    let mesh = Arc::new(regia_render::CpuMesh { vertices, indices });
    let material = Arc::new(regia_render::LabMaterial::default());
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        regia_render::MeshRenderer::new(mesh, material),
        Name::new("ground"),
        EntityGuid(Guid(77)),
    ));
}

fn scene_text(app: &mut App) -> String {
    let filter = SnapshotFilter {
        level: None,
        resources: Vec::new(),
    };
    let mut file = snapshot_scene(app.world_mut(), &filter).expect("snapshot");
    file.entities.sort_by_key(|e| e.guid);
    file.to_ron_string().expect("ron")
}

fn ray(x: f32, z: f32) -> (Vec3, Vec3) {
    (Vec3::new(x, 15.0, z), Vec3::NEG_Y)
}

fn pointer(world: &mut World, phase: PointerPhase, x: f32, z: f32) {
    let (origin, direction) = ray(x, z);
    tools::handle_event(
        world,
        ToolEvent::Pointer {
            phase,
            origin,
            direction,
        },
    )
    .expect("pointer");
}

fn click(world: &mut World, x: f32, z: f32) {
    pointer(world, PointerPhase::Press, x, z);
    pointer(world, PointerPhase::Release, x, z);
}

fn drag(world: &mut World, from: (f32, f32), to: (f32, f32)) {
    pointer(world, PointerPhase::Press, from.0, from.1);
    pointer(world, PointerPhase::Move, to.0, to.1);
    pointer(world, PointerPhase::Release, to.0, to.1);
}

fn scripted(world: &mut World, kind: ToolKind, preset: &str, gesture: ToolGesture, apply: bool) {
    tools::run_tool(world, kind, preset, None, &gesture, apply).expect("scripted");
}

fn point(x: f32, z: f32) -> ToolGesture {
    ToolGesture {
        points: vec![Vec3::new(x, 5.0, z)],
        ..ToolGesture::default()
    }
}

fn named(world: &World, name: &str) -> Entity {
    world
        .iter_entities()
        .find(|entity| entity.get::<Name>().is_some_and(|n| n.as_str() == name))
        .map(|entity| entity.id())
        .unwrap_or_else(|| panic!("no entity named {name}"))
}

fn mouse_path(world: &mut World) {
    tools::arm(world, ToolKind::Path, TOOL_ROAD_PATH);
    click(world, 0.0, 0.0);
    click(world, 4.0, 0.0);
    click(world, 8.0, 0.0);
    drag(world, (4.0, 0.0), (4.0, 2.0));
    tools::handle_event(world, ToolEvent::Backspace).expect("backspace");
    tools::handle_event(world, ToolEvent::Close).expect("close");
    tools::handle_event(world, ToolEvent::Enter).expect("enter");
}

fn scripted_path(world: &mut World) {
    let preset = TOOL_ROAD_PATH;
    scripted(world, ToolKind::Path, preset, point(0.0, 0.0), false);
    scripted(world, ToolKind::Path, preset, point(4.0, 0.0), false);
    scripted(world, ToolKind::Path, preset, point(8.0, 0.0), false);
    scripted(
        world,
        ToolKind::Path,
        preset,
        ToolGesture {
            move_point: Some((1, Vec3::new(4.0, 5.0, 2.0))),
            ..ToolGesture::default()
        },
        false,
    );
    scripted(
        world,
        ToolKind::Path,
        preset,
        ToolGesture {
            remove_point: Some(2),
            ..ToolGesture::default()
        },
        false,
    );
    scripted(
        world,
        ToolKind::Path,
        preset,
        ToolGesture {
            closed: Some(true),
            ..ToolGesture::default()
        },
        false,
    );
    scripted(world, ToolKind::Path, preset, ToolGesture::default(), true);
}

#[test]
fn mouse_path_and_area_match_the_scripted_session() {
    let mut mouse = app();
    let mut script = app();
    ground(&mut mouse);
    ground(&mut script);
    mouse.update();
    script.update();
    mouse_path(mouse.world_mut());
    scripted_path(script.world_mut());
    let m = named(mouse.world(), "tool_road_01");
    let s = named(script.world(), "tool_road_01");
    let mh = tools::placement_hash(mouse.world(), m);
    let sh = tools::placement_hash(script.world(), s);
    assert_eq!(
        mh,
        sh,
        "path\nmouse:\n{}\nscript:\n{}",
        tools::placement_text(mouse.world(), m),
        tools::placement_text(script.world(), s)
    );
    assert!(
        mouse
            .world()
            .get::<Spline>(m)
            .is_some_and(|sp| sp.closed && sp.points.len() == 2)
    );

    tools::arm(mouse.world_mut(), ToolKind::Area, TOOL_FOREST_AREA_PATH);
    for (x, z) in [(-4.0, -4.0), (4.0, -4.0), (0.0, 4.0)] {
        click(mouse.world_mut(), x, z);
    }
    tools::handle_event(mouse.world_mut(), ToolEvent::Enter).expect("area enter");
    let preset = TOOL_FOREST_AREA_PATH;
    for (x, z) in [(-4.0, -4.0), (4.0, -4.0), (0.0, 4.0)] {
        scripted(script.world_mut(), ToolKind::Area, preset, point(x, z), false);
    }
    scripted(
        script.world_mut(),
        ToolKind::Area,
        preset,
        ToolGesture::default(),
        true,
    );
    let m = named(mouse.world(), "tool_forest_area_01");
    let s = named(script.world(), "tool_forest_area_01");
    assert_eq!(
        tools::placement_hash(mouse.world(), m),
        tools::placement_hash(script.world(), s),
        "area\nmouse:\n{}\nscript:\n{}",
        tools::placement_text(mouse.world(), m),
        tools::placement_text(script.world(), s)
    );
    let spline = mouse.world().get::<Spline>(m).expect("area spline");
    assert!(spline.closed && spline.points.len() == 3);
}

#[test]
fn mouse_path_undo_restores_the_scene_bit_for_bit() {
    let mut app = app();
    ground(&mut app);
    app.update();
    let before = scene_text(&mut app);
    mouse_path(app.world_mut());
    let placed = tools::placement_hash(app.world(), named(app.world(), "tool_road_01"));
    assert!(undo_world(app.world_mut()));
    assert_eq!(scene_text(&mut app), before);
    assert!(regia_editor::command::redo_world(app.world_mut()));
    assert_eq!(
        tools::placement_hash(app.world(), named(app.world(), "tool_road_01")),
        placed
    );
}

#[test]
fn escape_cancels_and_a_short_enter_stays_open() {
    let mut app = app();
    ground(&mut app);
    app.update();
    let before = scene_text(&mut app);
    tools::arm(app.world_mut(), ToolKind::Path, TOOL_ROAD_PATH);
    click(app.world_mut(), 1.0, 1.0);
    let cancelled = tools::handle_event(app.world_mut(), ToolEvent::Escape).expect("esc");
    assert!(cancelled.cancelled);
    assert_eq!(scene_text(&mut app), before);

    tools::arm(app.world_mut(), ToolKind::Path, TOOL_ROAD_PATH);
    click(app.world_mut(), 1.0, 1.0);
    let err = tools::handle_event(app.world_mut(), ToolEvent::Enter).expect_err("short");
    assert_eq!(err.code, "procgen_tool_bad_gesture");
    assert!(tools::armed(app.world()));
    assert!(
        app.world()
            .get_resource::<tools::ProcToolSession>()
            .is_some_and(|s| s.0.is_some())
    );
    // A ray that misses the ground adds nothing.
    pointer(app.world_mut(), PointerPhase::Press, 80.0, 80.0);
    pointer(app.world_mut(), PointerPhase::Release, 80.0, 80.0);
    let n = app
        .world()
        .get::<Spline>(named(app.world(), "tool_road_01"))
        .map(|s| s.points.len());
    assert_eq!(n, Some(1));
}

#[test]
fn handle_drags_move_authored_and_generated_splines_and_undo() {
    let mut app = app();
    let authored = app
        .world_mut()
        .spawn((
            Name::new("authored"),
            Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
            Spline::through([Vec3::ZERO, Vec3::new(4.0, 0.0, 0.0)]),
            EntityGuid(Guid(11)),
        ))
        .id();
    let before_point = app.world().get::<Spline>(authored).expect("spline").clone();
    let (origin, dir) = ray(15.0, 0.0);
    view::apply_handle_drag(app.world_mut(), authored, HandleGrip::Point(0), origin, dir).expect("point");
    let moved = app.world().get::<Spline>(authored).expect("moved");
    assert!(
        (moved.points[0].position - Vec3::new(5.0, 0.0, 0.0)).length() < 1e-3,
        "{:?}",
        moved.points[0].position
    );

    let mut generated_spline = Spline::default();
    generated_spline.points.push(SplinePoint::free(
        Vec3::ZERO,
        Vec3::new(0.0, 0.0, -3.0),
        Vec3::new(3.0, 0.0, 0.0),
    ));
    generated_spline
        .points
        .push(SplinePoint::at(Vec3::new(4.0, 0.0, 0.0)));
    let before_tangent = generated_spline.clone();
    let generated = app
        .world_mut()
        .spawn((
            Name::new("generated"),
            Transform::IDENTITY,
            generated_spline,
            Generated {
                generator: Guid(3),
                run_id: 1,
            },
            EntityGuid(Guid(12)),
        ))
        .id();
    let (ox, oz) = (1.0, 0.0);
    assert!(view::handle_pointer(
        app.world_mut(),
        PointerPhase::Press,
        ray(ox, oz).0,
        ray(ox, oz).1
    ));
    assert!(view::handle_pointer(
        app.world_mut(),
        PointerPhase::Move,
        ray(2.0, 0.0).0,
        ray(2.0, 0.0).1
    ));
    assert!(view::handle_pointer(
        app.world_mut(),
        PointerPhase::Release,
        ray(2.0, 0.0).0,
        ray(2.0, 0.0).1
    ));
    let tangent = app.world().get::<Spline>(generated).expect("generated");
    assert!(
        (tangent.points[0].tangent_out - Vec3::new(6.0, 0.0, 0.0)).length() < 1e-3,
        "{:?}",
        tangent.points[0].tangent_out
    );

    assert!(undo_world(app.world_mut()));
    assert_eq!(
        app.world().get::<Spline>(generated).expect("restored tangent"),
        &before_tangent
    );
    assert!(undo_world(app.world_mut()));
    assert_eq!(
        app.world().get::<Spline>(authored).expect("restored point"),
        &before_point
    );

    let auto = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            Spline::through([Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0)]),
        ))
        .id();
    let miss = view::apply_handle_drag(
        app.world_mut(),
        auto,
        HandleGrip::Tangent {
            point: 0,
            incoming: false,
        },
        ray(0.0, 0.0).0,
        ray(0.0, 0.0).1,
    )
    .expect_err("auto");
    assert_eq!(miss.code, "procgen_handle_miss");
}
