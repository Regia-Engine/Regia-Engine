// SPDX-License-Identifier: MPL-2.0
//! Viewport and panel tools (ADR-2696, `book/agents/viewport-tools.md`): vertex snap, drop to floor
//! with align to normal, collider handles, World Settings through save and load, Output Log links and
//! the rebindable fly and outliner keys. Everything runs on editor state with no display and no GPU;
//! the panels are drawn through a headless egui context.

use std::path::Path;
use std::sync::Arc;

use regia::physics::{Collider, ColliderShape, ShapeAxis};
use regia_agent::extension::AgentExtension;
use regia_app::prelude::*;
use regia_core::math::{Isometry, Mat4, Quat, Vec2, Vec3};
use regia_editor::collider_handles::{self, ColliderGrip, ColliderHandleKind, ColliderHandleTool};
use regia_editor::command::{UndoStack, redo_world, undo_world};
use regia_editor::command_palette::{CommandKeymap, HOLD_COMMANDS, KeyChord};
use regia_editor::editor_layout::LayoutRequests;
use regia_editor::editor_prefs::{EditorPreferences, OWNER_ONLY_PREFS, PrefValue};
use regia_editor::handles::{DragPhase, HandleState, PointerInput, SnapSet};
use regia_editor::level_io::{self, OpenLevel};
use regia_editor::log_links::{self, AssetAction, LinkTarget, asset_action, expand_command, parse_links};
use regia_editor::selection::{SelectMode, Selection};
use regia_editor::shell::{EditorShell, Panel};
use regia_editor::surface_tools::{self, PlacementOptions, drop_to_floor, vertex_snap_at};
use regia_editor::views::ViewCamera;
use regia_editor::world_settings::SetLevelSettings;
use regia_render::{CpuMesh, LabMaterial, MeshRenderer};
use regia_scene::GlobalTransform;
use regia_scene::level::{AtmospherePreset, LevelSettings, current_settings};
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<regia_editor::viewport::EditorCamera>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

/// Calls an editor agent method by name and returns the error code on failure.
fn call(world: &mut World, method: &str, params: Value) -> Result<Value, String> {
    if world.get_resource::<AgentExtension>().is_none() {
        regia_editor::agent_ext::install(world);
    }
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params).map_err(|e| {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason").or_else(|| d.get("code")))
            .and_then(Value::as_str)
            .map_or_else(|| e.message.clone(), str::to_owned)
    })
}

fn spawn_mesh(world: &mut World, half: [f32; 3], transform: Transform) -> Entity {
    let (vertices, indices) = regia_render::box_mesh(half);
    world
        .spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(LabMaterial::default()),
            ),
        ))
        .id()
}

fn at(x: f32, y: f32, z: f32) -> Transform {
    Transform {
        translation: Vec3::new(x, y, z),
        ..Transform::IDENTITY
    }
}

/// World positions of a mesh's vertices (after `GlobalTransform` propagated).
fn world_vertices(world: &World, entity: Entity) -> Vec<Vec3> {
    let model = Mat4::from(world.get::<GlobalTransform>(entity).expect("global").0);
    world
        .get::<MeshRenderer>(entity)
        .expect("mesh")
        .mesh
        .vertices
        .iter()
        .map(|v| model.transform_point3(Vec3::from_array(v.position)))
        .collect()
}

fn lowest_y(world: &World, entity: Entity) -> f32 {
    world_vertices(world, entity)
        .iter()
        .map(|p| p.y)
        .fold(f32::MAX, f32::min)
}

fn select(world: &mut World, entities: &[Entity]) {
    let mut selection = Selection::default();
    for entity in entities {
        selection.select(*entity, SelectMode::Add);
    }
    world.insert_resource(selection);
}

/// A camera looking along -Z at the origin, whose pixels map simply.
fn camera() -> ViewCamera {
    ViewCamera {
        eye: Vec3::new(0.0, 0.0, 10.0),
        target: Vec3::ZERO,
        fov_y: 0.8,
        near: 0.1,
        far: 100.0,
        ortho: false,
    }
}

const SIZE: Vec2 = Vec2::new(800.0, 600.0);

/// The fixed scene of the floor tests: a floor whose top is at y = 1.25.
fn floor_scene() -> (World, Entity) {
    let mut world = World::new();
    world.init_resource::<UndoStack>();
    let floor = spawn_mesh(&mut world, [20.0, 0.25, 20.0], at(0.0, 1.0, 0.0));
    (world, floor)
}

// ---------------------------------------------------------------------------------------------
// Vertex snap
// ---------------------------------------------------------------------------------------------

#[test]
fn vertex_snap_goes_to_the_nearest_vertex_of_the_triangle_under_the_cursor() {
    let mut world = World::new();
    let moved = spawn_mesh(&mut world, [0.1, 0.1, 0.1], at(-3.0, 0.0, 0.0));
    let target = spawn_mesh(&mut world, [1.0, 1.0, 1.0], at(0.0, 0.0, 0.0));
    let view = camera();
    // The pixel just inside the corner (+1, +1, +1) of the box's front face.
    let corner = view
        .project(Vec3::new(0.9, 0.9, 1.0), SIZE)
        .expect("in front of the camera");
    let snap = vertex_snap_at(&world, &view, SIZE, corner, &[moved]).expect("the box is under the cursor");
    assert_eq!(snap.hit.entity, target);
    assert_eq!(snap.position, Vec3::new(1.0, 1.0, 1.0));
    // The vertex belongs to the hit triangle.
    assert!(snap.hit.indices.contains(&snap.vertex));
    // The opposite corner of the face gives the opposite vertex.
    let other = view.project(Vec3::new(-0.9, -0.9, 1.0), SIZE).expect("visible");
    let snap = vertex_snap_at(&world, &view, SIZE, other, &[moved]).expect("hit");
    assert_eq!(snap.position, Vec3::new(-1.0, -1.0, 1.0));
    // Empty space snaps to nothing.
    assert_eq!(
        vertex_snap_at(&world, &view, SIZE, Vec2::new(2.0, 2.0), &[moved]),
        None
    );
}

#[test]
fn vertex_snap_looks_through_the_entity_being_moved() {
    let mut world = World::new();
    // The moved box sits right in front of the target.
    let moved = spawn_mesh(&mut world, [2.0, 2.0, 0.1], at(0.0, 0.0, 5.0));
    let target = spawn_mesh(&mut world, [1.0, 1.0, 1.0], at(0.0, 0.0, 0.0));
    let view = camera();
    let pixel = view.project(Vec3::new(0.9, 0.9, 1.0), SIZE).expect("visible");
    let snap = vertex_snap_at(&world, &view, SIZE, pixel, &[moved]).expect("seen through");
    assert_eq!(snap.hit.entity, target);
    // Without leaving it out, the ray stops on it.
    let own = vertex_snap_at(&world, &view, SIZE, pixel, &[]).expect("hit");
    assert_eq!(own.hit.entity, moved);
}

#[test]
fn vertex_snap_agent_moves_the_pivot_in_the_parents_space_as_one_undo_step() {
    let mut app = edit_app();
    let world = app.world_mut();
    let parent = world
        .spawn((
            Transform {
                translation: Vec3::new(0.0, 5.0, 0.0),
                scale: Vec3::splat(2.0),
                ..Transform::IDENTITY
            },
            GlobalTransform(
                Transform {
                    translation: Vec3::new(0.0, 5.0, 0.0),
                    scale: Vec3::splat(2.0),
                    ..Transform::IDENTITY
                }
                .compute_affine(),
            ),
        ))
        .id();
    let moved = world.spawn((Transform::IDENTITY, ChildOf(parent))).id();
    spawn_mesh(world, [1.0, 1.0, 1.0], at(0.0, 0.0, 0.0));
    // The default camera looks at the origin: the middle pixel is on the box.
    let dry = call(
        world,
        "editor.vertex_snap",
        json!({ "entity": moved.to_bits(), "x": 320, "y": 240, "apply": false }),
    )
    .expect("dry");
    assert_eq!(dry["snapped"], json!(true));
    assert_eq!(dry["applied"], json!(false));
    assert_eq!(world.get::<Transform>(moved), Some(&Transform::IDENTITY));
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    let done = call(
        world,
        "editor.vertex_snap",
        json!({ "entity": moved.to_bits(), "x": 320, "y": 240 }),
    )
    .expect("snap");
    assert_eq!(done["applied"], json!(true));
    let vertex =
        Vec3::from_array(serde_json::from_value::<[f32; 3]>(done["position"].clone()).expect("position"));
    // Vertices of a unit box are at ±1 on each axis.
    assert_eq!(vertex.abs(), Vec3::ONE, "{vertex}");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    regia_scene::propagate::propagate_transforms(world);
    let global = world.get::<GlobalTransform>(moved).expect("global").0;
    let pivot: Vec3 = global.translation.into();
    assert!((pivot - vertex).length() < 1e-4, "{pivot} vs {vertex}");
    assert!(undo_world(world));
    assert_eq!(world.get::<Transform>(moved), Some(&Transform::IDENTITY));
    // Nothing under the pixel: no error, no move.
    let miss = call(
        world,
        "editor.vertex_snap",
        json!({ "entity": moved.to_bits(), "x": 1, "y": 1 }),
    )
    .expect("miss");
    assert_eq!(miss["snapped"], json!(false));
    assert_eq!(world.get::<Transform>(moved), Some(&Transform::IDENTITY));
    assert_eq!(
        call(world, "editor.vertex_snap", json!({ "x": 1, "y": 1 })).expect_err("refused"),
        "vertex_snap_no_entity"
    );
}

// ---------------------------------------------------------------------------------------------
// Drop to floor
// ---------------------------------------------------------------------------------------------

#[test]
fn a_box_lands_on_the_floor_within_a_millimeter() {
    let (mut world, floor) = floor_scene();
    let cube = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(2.0, 9.0, -3.0));
    let report = drop_to_floor(&mut world, &[cube], false);
    assert_eq!(report.placed.len(), 1);
    assert_eq!(report.placed[0].surface, floor);
    assert!(report.undoable);
    let gap = lowest_y(&world, cube) - 1.25;
    assert!(gap.abs() < 1.0e-3, "gap {gap}");
    let t = world.get::<Transform>(cube).expect("transform");
    assert_eq!((t.translation.x, t.translation.z), (2.0, -3.0), "only y moves");
    // One undo step puts it back, a redo lands it again.
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    assert!(undo_world(&mut world));
    assert_eq!(world.get::<Transform>(cube).map(|t| t.translation.y), Some(9.0));
    assert!(redo_world(&mut world));
    assert!((world.get::<Transform>(cube).expect("t").translation.y - 1.75).abs() < 1e-3);
}

#[test]
fn a_rotated_scaled_child_lands_by_its_real_lowest_point() {
    let (mut world, _) = floor_scene();
    let parent_pose = Transform {
        translation: Vec3::new(0.0, 8.0, 0.0),
        scale: Vec3::splat(2.0),
        ..Transform::IDENTITY
    };
    let parent = world
        .spawn((parent_pose, GlobalTransform(parent_pose.compute_affine())))
        .id();
    let pose = Transform {
        translation: Vec3::new(0.0, 1.0, 0.0),
        rotation: Quat::from_rotation_z(0.6) * Quat::from_rotation_x(0.4),
        scale: Vec3::new(1.0, 0.5, 1.5),
    };
    let child = spawn_mesh(&mut world, [0.5, 0.5, 0.5], pose);
    world.entity_mut(child).insert(ChildOf(parent));
    regia_scene::propagate::propagate_transforms(&mut world);
    drop_to_floor(&mut world, &[child], false);
    let gap = lowest_y(&world, child) - 1.25;
    assert!(gap.abs() < 1.0e-3, "gap {gap}");
    // The parent stays; the child's local rotation and scale are unchanged.
    assert_eq!(world.get::<Transform>(parent), Some(&parent_pose));
    let t = world.get::<Transform>(child).expect("t");
    assert!(t.rotation.angle_between(pose.rotation) < 1e-5);
    assert_eq!(t.scale, pose.scale);
}

#[test]
fn the_first_surface_under_the_footprint_stops_it_not_the_one_under_its_center() {
    let (mut world, _) = floor_scene();
    // A ledge under one side of a wide box: the center's column would fall to the floor.
    let ledge = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(2.5, 2.5, 0.0));
    let wide = spawn_mesh(&mut world, [3.0, 0.25, 3.0], at(0.0, 10.0, 0.0));
    let report = drop_to_floor(&mut world, &[wide], false);
    assert_eq!(report.placed[0].surface, ledge);
    let gap = lowest_y(&world, wide) - 3.0;
    assert!(gap.abs() < 1.0e-3, "gap {gap}");
}

#[test]
fn a_thin_post_under_a_flat_plate_stops_it_though_no_vertex_is_above_it() {
    let (mut world, _) = floor_scene();
    // The plate's 8 vertices are all past the post; only the grid of columns finds it.
    let post = spawn_mesh(&mut world, [0.2, 1.0, 0.2], at(0.0, 2.25, 0.0));
    let plate = spawn_mesh(&mut world, [4.0, 0.1, 4.0], at(0.0, 9.0, 0.0));
    let report = drop_to_floor(&mut world, &[plate], false);
    assert_eq!(report.placed[0].surface, post);
    let gap = lowest_y(&world, plate) - 3.25;
    assert!(gap.abs() < 1.0e-3, "gap {gap}");
}

#[test]
fn a_sunk_entity_comes_back_up_and_a_rested_one_is_no_undo_step() {
    let (mut world, _) = floor_scene();
    let cube = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(0.0, 1.5, 0.0));
    drop_to_floor(&mut world, &[cube], false);
    let gap = lowest_y(&world, cube) - 1.25;
    assert!(gap.abs() < 1.0e-3, "gap {gap}");
    let depth = world.resource::<UndoStack>().undo_depth();
    let again = drop_to_floor(&mut world, &[cube], false);
    assert_eq!(again.placed.len(), 1);
    assert!(!again.undoable);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth);
}

#[test]
fn a_miss_stays_put_with_no_undo_step_and_the_agent_says_so() {
    let mut app = edit_app();
    let world = app.world_mut();
    let cube = spawn_mesh(world, [0.5, 0.5, 0.5], at(0.0, 5.0, 0.0));
    let report = drop_to_floor(world, &[cube], false);
    assert_eq!(report.missed, [cube]);
    assert!(!report.undoable);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    assert_eq!(world.get::<Transform>(cube).map(|t| t.translation.y), Some(5.0));
    assert_eq!(
        call(
            world,
            "editor.drop_to_floor",
            json!({ "entities": [cube.to_bits()] })
        )
        .expect_err("refused"),
        "drop_no_surface"
    );
    assert_eq!(
        call(world, "editor.drop_to_floor", json!({})).expect_err("refused"),
        "drop_no_selection"
    );
}

#[test]
fn a_multi_selection_is_one_undo_step_and_selected_entities_are_not_surfaces() {
    let (mut world, _) = floor_scene();
    let lower = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(0.0, 3.0, 0.0));
    let upper = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(0.0, 7.0, 0.0));
    select(&mut world, &[lower, upper]);
    let status = surface_tools::drop_selection(&mut world);
    assert_eq!(status, "Dropped 2 to the floor");
    // Both reach the floor: the upper does not stop on the lower.
    for entity in [lower, upper] {
        let gap = lowest_y(&world, entity) - 1.25;
        assert!(gap.abs() < 1.0e-3, "gap {gap}");
    }
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    assert!(undo_world(&mut world));
    assert_eq!(world.get::<Transform>(upper).map(|t| t.translation.y), Some(7.0));
    assert_eq!(world.get::<Transform>(lower).map(|t| t.translation.y), Some(3.0));
}

#[test]
fn a_drop_does_not_merge_into_the_gizmo_drag_before_it() {
    let (mut world, _) = floor_scene();
    let cube = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(0.0, 3.0, 0.0));
    let drag = regia_editor::command::SetTransform::capture(&world, cube, at(0.0, 6.0, 0.0));
    let mut drag = drag;
    regia_editor::command::EditorCommand::apply(&mut drag, &mut world);
    world.resource_mut::<UndoStack>().push(Box::new(drag));
    drop_to_floor(&mut world, &[cube], false);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
    assert!(undo_world(&mut world));
    assert_eq!(world.get::<Transform>(cube).map(|t| t.translation.y), Some(6.0));
}

/// A ramp: a thin wide box turned about Z, with the world up of its top face as the normal.
fn ramp_scene(angle: f32) -> (World, Vec3) {
    let mut world = World::new();
    world.init_resource::<UndoStack>();
    let rotation = Quat::from_rotation_z(angle);
    spawn_mesh(
        &mut world,
        [20.0, 0.25, 20.0],
        Transform {
            rotation,
            ..Transform::IDENTITY
        },
    );
    (world, rotation * Vec3::Y)
}

#[test]
fn align_turns_up_onto_the_normal_and_rests_on_the_slope_within_a_millimeter() {
    let (mut world, normal) = ramp_scene(0.5);
    let cube = spawn_mesh(&mut world, [0.5, 1.0, 0.5], at(1.0, 12.0, 2.0));
    let report = drop_to_floor(&mut world, &[cube], true);
    assert_eq!(report.placed.len(), 1);
    let placed = report.placed[0];
    assert!((placed.normal - normal).length() < 1e-4, "{}", placed.normal);
    let t = world.get::<Transform>(cube).expect("t");
    assert!((t.rotation * Vec3::Y - normal).length() < 1e-4);
    // The lowest point along the normal lies in the ramp's top plane (it passes 0.25 along the normal
    // from the origin).
    let support = world_vertices(&world, cube)
        .iter()
        .map(|p| p.dot(normal))
        .fold(f32::MAX, f32::min);
    assert!((support - 0.25).abs() < 1.0e-3, "support {support}");
    assert_eq!(
        (t.translation.x, t.translation.z),
        (1.0, 2.0),
        "slides straight down"
    );
    // Undo restores the rotation too.
    assert!(undo_world(&mut world));
    assert_eq!(world.get::<Transform>(cube), Some(&at(1.0, 12.0, 2.0)));
}

#[test]
fn without_align_a_slope_leaves_the_rotation_alone_and_does_not_sink_in() {
    let (mut world, normal) = ramp_scene(0.5);
    let cube = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(0.0, 12.0, 0.0));
    drop_to_floor(&mut world, &[cube], false);
    let t = world.get::<Transform>(cube).expect("t");
    assert_eq!(t.rotation, Quat::IDENTITY);
    let support = world_vertices(&world, cube)
        .iter()
        .map(|p| p.dot(normal))
        .fold(f32::MAX, f32::min);
    // Every vertex above the plane, the lowest one touching it.
    assert!((support - 0.25).abs() < 1.0e-3, "support {support}");
}

#[test]
fn the_placement_option_decides_for_the_key_and_the_agent() {
    let (mut world, normal) = ramp_scene(0.3);
    let cube = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(0.0, 12.0, 0.0));
    let options = call(
        &mut world,
        "editor.placement_options",
        json!({ "align_to_normal": true }),
    )
    .expect("options");
    assert_eq!(options["align_to_normal"], json!(true));
    select(&mut world, &[cube]);
    let done = call(&mut world, "editor.drop_to_floor", json!({})).expect("drop");
    assert_eq!(done["align"], json!(true));
    let t = world.get::<Transform>(cube).expect("t");
    assert!((t.rotation * Vec3::Y - normal).length() < 1e-4);
    assert!(undo_world(&mut world));
    let plain = call(&mut world, "editor.drop_to_floor", json!({ "align": false })).expect("drop");
    assert_eq!(plain["align"], json!(false));
    assert_eq!(world.get::<Transform>(cube).expect("t").rotation, Quat::IDENTITY);
    assert!(world.resource::<PlacementOptions>().align_to_normal);
}

// ---------------------------------------------------------------------------------------------
// Collider handles
// ---------------------------------------------------------------------------------------------

fn collider_entity(world: &mut World, shape: ColliderShape, pose: Transform) -> Entity {
    world
        .spawn((
            pose,
            GlobalTransform(pose.compute_affine()),
            Collider {
                shape,
                ..Collider::default()
            },
        ))
        .id()
}

fn grip(grips: &[ColliderGrip], kind: ColliderHandleKind) -> ColliderGrip {
    *grips
        .iter()
        .find(|g| g.kind == kind)
        .unwrap_or_else(|| panic!("{kind:?} in {grips:?}"))
}

/// Drags grip `index` with the real handle state machine, in `steps` pointer samples from where it
/// is on screen to `to`, through the same begin, update and end the viewport calls.
fn drag_with_pointer(
    world: &mut World,
    entity: Entity,
    kind: ColliderHandleKind,
    to: Vec2,
    steps: u32,
) -> Vec2 {
    let view = camera();
    let mut state = HandleState::default();
    let grips = collider_handles::handles_for(
        world.get::<Collider>(entity).expect("collider"),
        collider_handles::body_pose(world, entity),
    );
    let index = grips.iter().position(|g| g.kind == kind).expect("grip");
    let shapes: Vec<_> = grips.iter().map(|g| g.handle).collect();
    let from = view.project(grips[index].handle.anchor(), SIZE).expect("visible");
    let radius = regia_editor::handles::DEFAULT_PICK_RADIUS;
    let none = SnapSet::empty();
    let input = |pos: Vec2, pressed: bool, down: bool| PointerInput {
        pos: Some(pos),
        pressed,
        down,
    };
    let begin = state
        .update(&view, SIZE, input(from, true, true), &shapes, &none, radius)
        .expect("grabbed");
    assert_eq!(begin.phase, DragPhase::Begin);
    assert_eq!(begin.handle, index, "the grip under the pointer");
    assert!(collider_handles::begin_drag(world, entity, kind));
    for step in 1..=steps {
        let pointer = from.lerp(to, step as f32 / steps as f32);
        let event = state
            .update(&view, SIZE, input(pointer, false, true), &shapes, &none, radius)
            .expect("drag");
        assert_eq!(event.phase, DragPhase::Drag);
        collider_handles::update_drag(world, event.position);
    }
    let end = state
        .update(&view, SIZE, input(to, false, false), &shapes, &none, radius)
        .expect("end");
    assert_eq!(end.phase, DragPhase::End);
    collider_handles::update_drag(world, end.position);
    collider_handles::end_drag(world);
    from
}

#[test]
fn dragging_a_box_face_changes_the_extent_as_one_undo_step() {
    let mut world = World::new();
    world.init_resource::<UndoStack>();
    let start = Vec3::new(1.0, 2.0, 0.5);
    let entity = collider_entity(
        &mut world,
        ColliderShape::Box { half_extents: start },
        Transform::IDENTITY,
    );
    let kind = ColliderHandleKind::Extent {
        axis: 0,
        positive: true,
    };
    let view = camera();
    let to = view.project(Vec3::new(3.0, 0.0, 0.0), SIZE).expect("visible");
    drag_with_pointer(&mut world, entity, kind, to, 12);
    let collider = world.get::<Collider>(entity).expect("collider");
    let ColliderShape::Box { half_extents } = collider.shape else {
        panic!("box");
    };
    assert!((half_extents.x - 3.0).abs() < 0.02, "{half_extents}");
    assert_eq!((half_extents.y, half_extents.z), (2.0, 0.5));
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        1,
        "one drag, one step"
    );
    assert_eq!(
        world.resource::<UndoStack>().next_undo().as_deref(),
        Some("Collider handle")
    );
    assert!(undo_world(&mut world));
    assert_eq!(
        world.get::<Collider>(entity).expect("collider").shape,
        ColliderShape::Box { half_extents: start }
    );
    assert!(redo_world(&mut world));
    let ColliderShape::Box { half_extents } = world.get::<Collider>(entity).expect("c").shape else {
        panic!("box");
    };
    assert!((half_extents.x - 3.0).abs() < 0.02);
}

#[test]
fn every_shape_exposes_its_grips_and_each_edits_only_its_field() {
    let mut world = World::new();
    world.init_resource::<UndoStack>();
    let body = at(1.0, 0.0, 0.0);
    let pose = Isometry::new(body.rotation, body.translation);

    // Box: six faces and the offset.
    let boxed = collider_entity(
        &mut world,
        ColliderShape::Box {
            half_extents: Vec3::ONE,
        },
        body,
    );
    let collider = world.get::<Collider>(boxed).expect("c").clone();
    let grips = collider_handles::handles_for(&collider, pose);
    assert_eq!(grips.len(), 7);
    let down = grip(
        &grips,
        ColliderHandleKind::Extent {
            axis: 1,
            positive: false,
        },
    );
    assert_eq!(down.handle.anchor(), Vec3::new(1.0, -1.0, 0.0));
    let next =
        collider_handles::apply_handle(&collider, pose, down.kind, Vec3::new(1.0, -4.0, 0.0)).expect("edit");
    assert_eq!(
        next.shape,
        ColliderShape::Box {
            half_extents: Vec3::new(1.0, 4.0, 1.0)
        }
    );

    // Sphere: three radius grips.
    let sphere = Collider {
        shape: ColliderShape::Sphere { radius: 2.0 },
        ..Collider::default()
    };
    let grips = collider_handles::handles_for(&sphere, pose);
    assert_eq!(grips.len(), 4);
    for axis in 0..3 {
        let g = grip(&grips, ColliderHandleKind::Radius { axis });
        let moved = g.handle.anchor() + (g.handle.anchor() - Vec3::new(1.0, 0.0, 0.0)) * 0.5;
        let next = collider_handles::apply_handle(&sphere, pose, g.kind, moved).expect("edit");
        let ColliderShape::Sphere { radius } = next.shape else {
            panic!("sphere");
        };
        assert!((radius - 3.0).abs() < 1e-4, "axis {axis}: {radius}");
    }

    // Capsule: two ends, two radii.
    let capsule = Collider {
        shape: ColliderShape::Capsule {
            half_height: 1.0,
            radius: 0.5,
            axis: ShapeAxis::Y,
        },
        ..Collider::default()
    };
    let grips = collider_handles::handles_for(&capsule, pose);
    assert_eq!(grips.len(), 5);
    let tip = grip(&grips, ColliderHandleKind::HalfHeight { positive: false });
    assert_eq!(tip.handle.anchor(), Vec3::new(1.0, -1.5, 0.0));
    let next =
        collider_handles::apply_handle(&capsule, pose, tip.kind, Vec3::new(1.0, -2.5, 0.0)).expect("edit");
    assert_eq!(
        next.shape,
        ColliderShape::Capsule {
            half_height: 2.0,
            radius: 0.5,
            axis: ShapeAxis::Y
        }
    );
    let side = grip(&grips, ColliderHandleKind::Radius { axis: 2 });
    let next =
        collider_handles::apply_handle(&capsule, pose, side.kind, Vec3::new(1.0, 0.0, 0.75)).expect("edit");
    assert_eq!(
        next.shape,
        ColliderShape::Capsule {
            half_height: 1.0,
            radius: 0.75,
            axis: ShapeAxis::Y
        }
    );
    // A capsule has no radius grip along its long axis.
    assert_eq!(
        collider_handles::apply_handle(&capsule, pose, ColliderHandleKind::Radius { axis: 1 }, Vec3::ZERO),
        None
    );

    // Another shape: the offset grip only.
    let cone = Collider {
        shape: ColliderShape::Cone {
            half_height: 1.0,
            radius: 1.0,
            axis: ShapeAxis::Y,
        },
        ..Collider::default()
    };
    assert_eq!(collider_handles::handles_for(&cone, pose).len(), 1);
}

#[test]
fn the_offset_grip_moves_the_shape_in_body_space_and_the_sizes_follow_it() {
    let mut world = World::new();
    world.init_resource::<UndoStack>();
    let pose = Transform {
        translation: Vec3::new(0.0, 0.0, 0.0),
        rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        ..Transform::IDENTITY
    };
    let entity = collider_entity(&mut world, ColliderShape::Sphere { radius: 1.0 }, pose);
    let view = camera();
    let to = view.project(Vec3::new(2.0, 1.0, 0.0), SIZE).expect("visible");
    drag_with_pointer(&mut world, entity, ColliderHandleKind::Offset, to, 6);
    let collider = world.get::<Collider>(entity).expect("c").clone();
    // The body is turned 90 degrees about Y: world +X is its local +Z... and back again.
    let world_center = collider_handles::handles_for(&collider, collider_handles::body_pose(&world, entity))
        .iter()
        .find(|g| g.kind == ColliderHandleKind::Offset)
        .expect("offset")
        .handle
        .anchor();
    assert!(
        (world_center - Vec3::new(2.0, 1.0, 0.0)).length() < 0.02,
        "{world_center}"
    );
    assert_eq!(collider.shape, ColliderShape::Sphere { radius: 1.0 });
    assert!(undo_world(&mut world));
    assert_eq!(
        world.get::<Collider>(entity).expect("c").offset.translation,
        Vec3::ZERO
    );
}

#[test]
fn a_drag_that_ends_where_it_began_is_no_undo_step() {
    let mut world = World::new();
    world.init_resource::<UndoStack>();
    let entity = collider_entity(
        &mut world,
        ColliderShape::Sphere { radius: 1.0 },
        Transform::IDENTITY,
    );
    let kind = ColliderHandleKind::Radius { axis: 0 };
    assert!(collider_handles::begin_drag(&mut world, entity, kind));
    assert!(collider_handles::update_drag(
        &mut world,
        Vec3::new(2.0, 0.0, 0.0)
    ));
    assert!(collider_handles::update_drag(
        &mut world,
        Vec3::new(1.0, 0.0, 0.0)
    ));
    assert!(!collider_handles::end_drag(&mut world));
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    // A later update without a drag does nothing.
    assert!(!collider_handles::update_drag(
        &mut world,
        Vec3::new(5.0, 0.0, 0.0)
    ));
    assert!(!world.resource::<ColliderHandleTool>().dragging());
}

#[test]
fn the_collider_agent_lists_drags_and_refuses() {
    let mut app = edit_app();
    let world = app.world_mut();
    let entity = collider_entity(
        world,
        ColliderShape::Capsule {
            half_height: 1.0,
            radius: 0.5,
            axis: ShapeAxis::Y,
        },
        at(0.0, 1.0, 0.0),
    );
    let bare = world.spawn(Transform::IDENTITY).id();
    let listed = call(
        world,
        "editor.collider_handles",
        json!({ "entity": entity.to_bits(), "enabled": true }),
    )
    .expect("list");
    assert_eq!(listed["enabled"], json!(true));
    assert_eq!(listed["collider"]["shape"], json!("capsule"));
    let names: Vec<&str> = listed["handles"]
        .as_array()
        .expect("handles")
        .iter()
        .filter_map(|h| h["name"].as_str())
        .collect();
    assert_eq!(
        names,
        ["offset", "half_height+", "half_height-", "radius_x", "radius_z"]
    );
    let dragged = call(
        world,
        "editor.collider_handle_drag",
        json!({ "entity": entity.to_bits(), "handle": "half_height+", "to": [0.0, 4.5, 0.0], "steps": 30 }),
    )
    .expect("drag");
    assert_eq!(dragged["changed"], json!(true));
    let half = dragged["collider"]["half_height"].as_f64().expect("half");
    assert!((half - 3.0).abs() < 1e-4, "{half}");
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        1,
        "30 steps, one entry"
    );
    assert!(undo_world(world));
    for (params, code) in [
        (
            json!({ "entity": bare.to_bits(), "handle": "offset", "to": [0, 0, 0] }),
            "collider_missing",
        ),
        (
            json!({ "entity": entity.to_bits(), "handle": "extent_x+", "to": [0, 0, 0] }),
            "collider_handle_unknown",
        ),
        (
            json!({ "entity": entity.to_bits(), "handle": "nonsense", "to": [0, 0, 0] }),
            "collider_handle_unknown",
        ),
    ] {
        assert_eq!(
            call(world, "editor.collider_handle_drag", params.clone()).expect_err("refused"),
            code,
            "{params}"
        );
    }
    assert!(
        call(
            world,
            "editor.collider_handle_drag",
            json!({ "entity": entity.to_bits(), "handle": "offset" }),
        )
        .is_err(),
        "`to` is required"
    );
    assert!(collider_handles::tool_shows(world, entity));
    assert!(!collider_handles::tool_shows(world, bare));
}

#[test]
fn a_collider_with_no_mesh_drops_by_its_shape() {
    let (mut world, _) = floor_scene();
    let entity = collider_entity(
        &mut world,
        ColliderShape::Sphere { radius: 0.5 },
        at(0.0, 6.0, 0.0),
    );
    drop_to_floor(&mut world, &[entity], false);
    let y = world.get::<Transform>(entity).expect("t").translation.y;
    // The sphere's outline circles reach down to its radius.
    assert!((y - 1.75).abs() < 1.0e-3, "{y}");
}

// ---------------------------------------------------------------------------------------------
// World Settings
// ---------------------------------------------------------------------------------------------

fn save_and_reopen(world: &mut World, path: &Path) -> LevelSettings {
    let open = world.resource::<OpenLevel>().clone();
    level_io::save_level_file(world, &open, path).expect("save");
    let mut reopened = App::new();
    reopened.add_plugins(MinimalPlugins);
    reopened.add_plugins(regia_scene::ScenePlugin);
    let opened = level_io::open_level_file(reopened.world_mut(), path).expect("open");
    reopened.world_mut().insert_resource(opened);
    current_settings(reopened.world())
}

#[test]
fn world_settings_round_trip_through_save_and_load_and_undo() {
    let root = regia_test_utils::unique_temp_dir("world-settings");
    let mut app = edit_app();
    let world = app.world_mut();
    let before = current_settings(world);
    let done = call(
        world,
        "editor.world_settings_set",
        json!({ "settings": {
            "gravity": [0.0, -1.62, 0.0],
            "game_rules": "arena",
            "streaming_enabled": true,
            "atmosphere": { "preset": "Mars", "cloud_coverage": 0.25 },
            "look": { "exposure_bias_ev": 0.5 },
        } }),
    )
    .expect("set");
    assert_eq!(done["changed"], json!(true));
    assert_eq!(done["dirty"], json!(true));
    let settings = current_settings(world);
    assert_eq!(settings.gravity, Vec3::new(0.0, -1.62, 0.0));
    assert_eq!(settings.atmosphere.preset, AtmospherePreset::Mars);
    assert_eq!(settings.atmosphere.cloud_coverage, 0.25);
    // Fields the patch did not name stay as they were.
    assert_eq!(settings.look.contrast, before.look.contrast);
    assert_eq!(settings.ambient_audio, before.ambient_audio);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1, "one set, one step");

    // Save, reopen in a fresh world: the same settings, in the file form and the directory form.
    let file = root.join("arena.level.ron");
    assert_eq!(save_and_reopen(world, &file), settings);
    let text = std::fs::read_to_string(&file).expect("saved");
    assert!(text.contains("arena"), "{text}");
    assert_eq!(
        save_and_reopen(world, &root.join("arena-split.level.d")),
        settings
    );

    // Undo: the world is back, the next save writes the old values.
    assert!(undo_world(world));
    assert_eq!(current_settings(world), before);
    assert_eq!(save_and_reopen(world, &file), before);
    assert!(redo_world(world));
    assert_eq!(current_settings(world), settings);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn world_settings_refuse_nonsense_and_change_nothing() {
    let mut app = edit_app();
    let world = app.world_mut();
    let before = current_settings(world);
    for (params, code) in [
        (json!({}), "world_settings_bad_value"),
        (json!({ "settings": 3 }), "world_settings_bad_value"),
        (
            json!({ "settings": { "gravity": "down" } }),
            "world_settings_bad_value",
        ),
        (
            json!({ "settings": { "gravvity": [0, 0, 0] } }),
            "world_settings_unknown_field",
        ),
        (
            json!({ "settings": { "look": { "exposure_bias": 1.0 } } }),
            "world_settings_unknown_field",
        ),
        (
            json!({ "settings": { "gravity": [1e39, 0, 0] } }),
            "world_settings_bad_value",
        ),
        (
            json!({ "settings": { "atmosphere": { "cloud_coverage": 4.0 } } }),
            "world_settings_bad_value",
        ),
    ] {
        assert_eq!(
            call(world, "editor.world_settings_set", params.clone()).expect_err("refused"),
            code,
            "{params}"
        );
    }
    assert_eq!(current_settings(world), before);
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    // An optional field clears with null, a reset is undoable.
    call(
        world,
        "editor.world_settings_set",
        json!({ "settings": { "environment": "asset:abc" } }),
    )
    .expect("set");
    assert_eq!(current_settings(world).environment.as_deref(), Some("asset:abc"));
    call(
        world,
        "editor.world_settings_set",
        json!({ "settings": { "environment": null } }),
    )
    .expect("clear");
    assert_eq!(current_settings(world).environment, None);
    call(
        world,
        "editor.world_settings_set",
        json!({ "settings": { "gravity": [0, -3, 0] } }),
    )
    .expect("set");
    let reset = call(world, "editor.world_settings_set", json!({ "reset": true })).expect("reset");
    assert_eq!(reset["changed"], json!(true));
    assert_eq!(current_settings(world), LevelSettings::default());
    assert!(undo_world(world));
    assert_eq!(current_settings(world).gravity.y, -3.0);
    // With no level open there is nothing to edit.
    world.remove_resource::<OpenLevel>();
    assert_eq!(
        call(world, "editor.world_settings_set", json!({ "reset": true })).expect_err("refused"),
        "world_settings_no_level"
    );
    let got = call(world, "editor.world_settings_get", json!({})).expect("get");
    assert!(got["settings"]["gravity"].is_array());
}

#[test]
fn the_panel_command_is_undoable_and_keeps_the_field_name() {
    let mut world = World::new();
    let mut next = LevelSettings::default();
    next.gravity.y = -2.0;
    let mut command = SetLevelSettings::capture(&world, next.clone(), "gravity.y", true);
    regia_editor::command::EditorCommand::apply(&mut command, &mut world);
    assert_eq!(
        regia_editor::command::EditorCommand::description(&command),
        "World settings: gravity.y"
    );
    assert_eq!(current_settings(&world), next);
    regia_editor::command::EditorCommand::revert(&mut command, &mut world);
    assert_eq!(current_settings(&world), LevelSettings::default());
}

/// A shell on a real project (with none it shows the project browser), for `world`.
fn shell_in_project(world: &mut World) -> (EditorShell, std::path::PathBuf) {
    let root = regia_test_utils::unique_temp_dir("viewport-tools-shell");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = \"T\"\nid = \"com.example.t\"\n",
    )
    .expect("project file");
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    (EditorShell::new(None, Some(root.clone())), root)
}

/// Runs `frames` shell frames without a GPU, with `events` in the first.
fn frames_with(
    shell: &mut EditorShell,
    ctx: &egui::Context,
    world: &mut World,
    frames: u32,
    events: Vec<egui::Event>,
) {
    let mut pending = events;
    for _ in 0..frames {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            time: Some(ctx.input(|i| i.time) + 1.0 / 60.0),
            predicted_dt: 1.0 / 60.0,
            events: std::mem::take(&mut pending),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| shell.ui(ui, world, None));
        output.textures_delta.clear();
    }
}

fn key(key: egui::Key, pressed: bool) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

#[test]
fn the_shell_opens_and_draws_the_world_settings_tab_only_on_request() {
    let mut app = edit_app();
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let (mut shell, root) = shell_in_project(world);
    frames_with(&mut shell, &ctx, world, 2, Vec::new());
    let world_settings = Panel::WorldSettings.id();
    assert!(
        !shell.layout().panel_ids().contains(&world_settings),
        "not in the default layout"
    );
    call(world, "editor.world_settings_panel", json!({})).expect("open");
    assert!(world.contains_resource::<LayoutRequests>());
    frames_with(&mut shell, &ctx, world, 3, Vec::new());
    assert!(shell.layout().panel_ids().contains(&world_settings));
    // The palette command does the same.
    assert_eq!(
        regia_editor::command_palette::run(world, "window.world_settings").as_deref(),
        Some("World settings")
    );
    // A panel id round trip for saved layouts.
    assert_eq!(
        Panel::from_id(Panel::WorldSettings.id()),
        Some(Panel::WorldSettings)
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Output Log links
// ---------------------------------------------------------------------------------------------

fn file_link(text: &str) -> (String, u32, Option<u32>, String) {
    let links = parse_links(text);
    assert_eq!(links.len(), 1, "{text}: {links:?}");
    let link = &links[0];
    let LinkTarget::File { line, col } = link.target else {
        panic!("{text}: {link:?}");
    };
    (
        link.path().to_owned(),
        line,
        col,
        text[link.start..link.end].to_owned(),
    )
}

#[test]
fn links_parse_lines_columns_and_the_text_around_them() {
    for (text, path, line, col, span) in [
        ("src/main.rs:12", "src/main.rs", 12, None, "src/main.rs:12"),
        ("src/main.rs:12:5", "src/main.rs", 12, Some(5), "src/main.rs:12:5"),
        (
            "thread 'main' panicked at crates/a/src/b.rs:7:9:",
            "crates/a/src/b.rs",
            7,
            Some(9),
            "crates/a/src/b.rs:7:9",
        ),
        (
            "  --> game/src/lib.rs:40:13",
            "game/src/lib.rs",
            40,
            Some(13),
            "game/src/lib.rs:40:13",
        ),
        ("(see main.rs:3)", "main.rs", 3, None, "main.rs:3"),
        (
            "file=shaders/a.wgsl:88",
            "shaders/a.wgsl",
            88,
            None,
            "shaders/a.wgsl:88",
        ),
        ("can't open ./a.rs:2", "./a.rs", 2, None, "./a.rs:2"),
    ] {
        assert_eq!(
            file_link(text),
            (path.to_owned(), line, col, span.to_owned()),
            "{text}"
        );
    }
}

#[test]
fn links_handle_paths_with_spaces() {
    assert_eq!(
        file_link("error at /home/me/my project/src/main.rs:12:5: oops"),
        (
            "/home/me/my project/src/main.rs".to_owned(),
            12,
            Some(5),
            "/home/me/my project/src/main.rs:12:5".to_owned()
        )
    );
    // Quotes hold a relative path with spaces together.
    assert_eq!(
        file_link("in \"my project/src/a b.rs:4\" now"),
        (
            "my project/src/a b.rs".to_owned(),
            4,
            None,
            "my project/src/a b.rs:4".to_owned()
        )
    );
    assert_eq!(
        file_link("in `my dir/c.rs:9:1`"),
        ("my dir/c.rs".to_owned(), 9, Some(1), "my dir/c.rs:9:1".to_owned())
    );
    // Ambiguous text carries both readings, longest first; a click takes the one that exists.
    let links = parse_links("copied /tmp/a to src/b.rs:3");
    assert_eq!(links[0].paths, ["/tmp/a to src/b.rs", "src/b.rs"]);
    // A complete earlier path is not the start of this one.
    let links = parse_links("copied /tmp/a.txt to src/b.rs:3");
    assert_eq!(links[0].paths, ["src/b.rs"]);
}

#[test]
fn links_handle_windows_drive_letters_and_unc_paths() {
    assert_eq!(
        file_link(r"C:\Users\me\src\main.rs:12:5"),
        (
            r"C:\Users\me\src\main.rs".to_owned(),
            12,
            Some(5),
            r"C:\Users\me\src\main.rs:12:5".to_owned()
        )
    );
    assert_eq!(file_link("D:/work/game/lib.rs:7").0, "D:/work/game/lib.rs");
    assert_eq!(
        file_link(r"error: C:\Program Files (x86)\My Game\src\a.rs:3:1: bad"),
        (
            r"C:\Program Files (x86)\My Game\src\a.rs".to_owned(),
            3,
            Some(1),
            r"C:\Program Files (x86)\My Game\src\a.rs:3:1".to_owned()
        )
    );
    assert_eq!(file_link(r"\\server\share\a.rs:8").0, r"\\server\share\a.rs");
    // The drive's own colon is never a line.
    assert_eq!(parse_links(r"C:\Users\me"), Vec::new());
    assert_eq!(file_link("at:C:/x/y.rs:5").0, "C:/x/y.rs");
}

#[test]
fn asset_paths_are_links_without_a_line_and_several_links_keep_their_order() {
    let links =
        parse_links("cannot load assets/mat/rusty iron.material.ron, using assets/mat/base.material.ron");
    assert_eq!(links.len(), 2, "{links:?}");
    assert_eq!(links[0].target, LinkTarget::Asset);
    assert_eq!(
        links[0].paths,
        ["assets/mat/rusty iron.material.ron", "iron.material.ron"],
        "a word naming a directory continues the path"
    );
    assert_eq!(links[1].path(), "assets/mat/base.material.ron");
    let links = parse_links("cannot load \"assets/mat/rusty iron.material.ron\"");
    assert_eq!(links[0].path(), "assets/mat/rusty iron.material.ron");
    // The editor's own warning: both the relative and the absolute path, each with spaces.
    let links = parse_links(
        "mesh assets/my models/broken one.glb: mesh file not found: /srv/p/assets/my models/broken one.glb",
    );
    assert_eq!(links.len(), 2, "{links:?}");
    assert_eq!(
        links[0].paths,
        [
            "assets/my models/broken one.glb",
            "models/broken one.glb",
            "one.glb"
        ]
    );
    assert_eq!(links[1].path(), "/srv/p/assets/my models/broken one.glb");
    let links = parse_links("/home/me/my game/assets/a.glb: no meshes");
    assert_eq!(links[0].path(), "/home/me/my game/assets/a.glb");
    // A line number makes it a file link in the code editor, once.
    let links = parse_links("assets/a.prefab.ron:12:3: bad field");
    assert_eq!(links.len(), 1);
    assert_eq!(
        links[0].target,
        LinkTarget::File {
            line: 12,
            col: Some(3)
        }
    );
    // Two kinds on one line.
    let links = parse_links("src/a.rs:1 wrote assets/b.png and src/c.rs:2:3");
    let kinds: Vec<&str> = links.iter().map(|l| l.path()).collect();
    assert_eq!(kinds, ["src/a.rs", "assets/b.png", "src/c.rs"]);
    // The longest suffix wins: one link, not two.
    assert_eq!(parse_links("x/y.level.ron").len(), 1);
    assert_eq!(asset_action("a/b.material.ron"), AssetAction::Editor);
    assert_eq!(asset_action("a/b.animgraph.ron"), AssetAction::Editor);
    assert_eq!(asset_action("a/b.prefab.ron"), AssetAction::Prefab);
    assert_eq!(asset_action("a/b.level.ron"), AssetAction::Level);
    assert_eq!(asset_action("a/b.glb"), AssetAction::Viewer);
    // Since ADR-7725 a UI layout opens in the UI Designer; a text asset with no panel still opens in
    // the code editor.
    assert_eq!(asset_action("a/b.ui.ron"), AssetAction::Editor);
    assert_eq!(asset_action("a/b.theme.ron"), AssetAction::CodeEditor);
}

fn project_with(file: &str, text: &str) -> std::path::PathBuf {
    let root = regia_test_utils::unique_temp_dir("log-links");
    let full = root.join(file);
    std::fs::create_dir_all(full.parent().expect("parent")).expect("dir");
    std::fs::write(&full, text).expect("file");
    root
}

fn world_in(root: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    regia_editor::agent_ext::install(&mut world);
    world
}

#[test]
fn opening_a_link_runs_the_preference_and_never_a_shell() {
    let root = project_with("src/a b.rs", "fn main() {}\n");
    let mut world = world_in(&root);
    // Nothing configured, an environment with no editor: says so.
    let link = parse_links("\"src/a b.rs:3:7\"").remove(0);
    let none = |_: &str| None;
    let err = log_links::open_link(&mut world, &link, true, &none).expect_err("refused");
    assert_eq!(
        err.data.map(|d| d["reason"].clone()).unwrap_or_default(),
        "open_in_editor_no_code_editor"
    );
    // The environment's editor works on a machine with nothing configured.
    let env = |name: &str| (name == "EDITOR").then(|| "nvim".to_owned());
    let reply = log_links::open_link(&mut world, &link, true, &env).expect("dry");
    let full = root.join("src/a b.rs").display().to_string();
    assert_eq!(reply["command"], json!(["nvim", "+3", full]));
    // The preference wins, and a path with spaces stays one argument.
    let mut prefs = EditorPreferences::default();
    prefs
        .set(log_links::CODE_EDITOR_PREF, &PrefValue::Text("code".to_owned()))
        .expect("set");
    world.insert_resource(prefs);
    let reply = log_links::open_link(&mut world, &link, true, &none).expect("dry");
    assert_eq!(reply["command"], json!(["code", "-g", format!("{full}:3:7")]));
    assert_eq!(reply["line"], json!(3));
    assert_eq!(reply["col"], json!(7));
    // A missing file is an error and starts nothing.
    let gone = parse_links("src/gone.rs:1").remove(0);
    assert!(log_links::open_link(&mut world, &gone, true, &none).is_err());
    // The real start of a program that exists (and of one that does not).
    #[cfg(unix)]
    {
        let mut prefs = EditorPreferences::default();
        prefs
            .set(
                log_links::CODE_EDITOR_PREF,
                &PrefValue::Text("true {file}".to_owned()),
            )
            .expect("set");
        world.insert_resource(prefs);
        assert!(log_links::open_link(&mut world, &link, false, &none).is_ok());
        let mut prefs = EditorPreferences::default();
        prefs
            .set(
                log_links::CODE_EDITOR_PREF,
                &PrefValue::Text("regia-no-such-program {file}".to_owned()),
            )
            .expect("set");
        world.insert_resource(prefs);
        assert!(log_links::open_link(&mut world, &link, false, &none).is_err());
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_click_takes_the_reading_that_exists_under_the_project_game_or_assets() {
    let root = project_with("src/b.rs", "\n");
    std::fs::create_dir_all(root.join("game/src")).expect("dir");
    std::fs::write(root.join("game/src/lib.rs"), "\n").expect("file");
    std::fs::create_dir_all(root.join("assets/mat")).expect("dir");
    std::fs::write(root.join("assets/mat/a.material.ron"), "()\n").expect("file");
    let resolve = |text: &str| {
        let link = parse_links(text).remove(0);
        log_links::resolve_link(&root, &link)
    };
    assert_eq!(
        resolve("copied /tmp/a to src/b.rs:3"),
        Some(root.join("src/b.rs"))
    );
    assert_eq!(resolve("src/lib.rs:1"), Some(root.join("game/src/lib.rs")));
    assert_eq!(
        resolve("mat/a.material.ron"),
        Some(root.join("assets/mat/a.material.ron"))
    );
    assert_eq!(resolve("src/nope.rs:1"), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn agents_stay_in_the_project_and_cannot_pick_the_program() {
    let root = project_with("src/a.rs", "\n");
    let outside = project_with("src/out.rs", "\n");
    let mut world = world_in(&root);
    // The preference that names the program is the owner's: all-or-nothing refused.
    assert!(OWNER_ONLY_PREFS.contains(&log_links::CODE_EDITOR_PREF));
    let err = call(
        &mut world,
        "editor.prefs_set",
        json!({ "values": { "code.editor_command": "sh -c evil", "camera.fly_speed": 9 } }),
    )
    .expect_err("refused");
    assert_eq!(err, "pref_owner_only");
    let prefs = world
        .get_resource::<EditorPreferences>()
        .cloned()
        .unwrap_or_default();
    assert_eq!(prefs.text(log_links::CODE_EDITOR_PREF), "");
    assert_ne!(prefs.float("camera.fly_speed"), 9.0, "a refused set changes none");
    assert_eq!(
        call(
            &mut world,
            "editor.log_open_link",
            json!({ "text": format!("{}:1", outside.join("src/out.rs").display()), "dry_run": true }),
        )
        .expect_err("refused"),
        "log_link_outside_project"
    );
    assert_eq!(
        call(
            &mut world,
            "editor.log_open_link",
            json!({ "text": "../secrets/x.rs:1", "dry_run": true }),
        )
        .expect_err("refused"),
        "log_link_outside_project"
    );
    assert_eq!(
        call(
            &mut world,
            "editor.log_open_link",
            json!({ "text": "src/a.rs:1", "index": 4, "dry_run": true }),
        )
        .expect_err("refused"),
        "log_link_missing"
    );
    let mut prefs = EditorPreferences::default();
    prefs
        .set(log_links::CODE_EDITOR_PREF, &PrefValue::Text("zed".to_owned()))
        .expect("the owner sets it");
    world.insert_resource(prefs);
    let inside = call(
        &mut world,
        "editor.log_open_link",
        json!({ "text": format!("at {}:5:2", root.join("src/a.rs").display()), "dry_run": true }),
    )
    .expect("a project file by its absolute path");
    assert_eq!(inside["opened"], json!("file"));
    assert_eq!(inside["command"][0], json!("zed"));
    let listed = call(
        &mut world,
        "editor.log_links",
        json!({ "text": "src/a.rs:5:2 and b/c.glb" }),
    )
    .expect("list");
    let links = listed["links"].as_array().expect("links");
    assert_eq!(links.len(), 2);
    assert_eq!(links[0]["kind"], json!("file"));
    assert_eq!(links[0]["line"], json!(5));
    assert_eq!(links[1]["kind"], json!("asset"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn an_asset_link_is_queued_for_the_shell_and_opens_like_the_content_browser() {
    let root = project_with("assets/mat/a.material.ron", "()\n");
    std::fs::write(root.join("assets/hero.glb"), "x").expect("file");
    std::fs::write(root.join("assets/note.theme.ron"), "()\n").expect("file");
    let mut world = world_in(&root);
    let reply = call(
        &mut world,
        "editor.log_open_link",
        json!({ "text": "mat/a.material.ron failed" }),
    )
    .expect("asset");
    assert_eq!(reply["action"], json!("editor"));
    assert_eq!(reply["path"], json!("assets/mat/a.material.ron"));
    call(
        &mut world,
        "editor.log_open_link",
        json!({ "text": "assets/hero.glb" }),
    )
    .expect("asset");
    let mut content = regia_editor::content::ContentBrowserState::default();
    assert_eq!(log_links::route_asset_requests(&mut world, &mut content), None);
    assert_eq!(content.open_request, Some(root.join("assets/mat/a.material.ron")));
    assert_eq!(content.view_request.as_deref(), Some("assets/hero.glb"));
    assert!(
        !world.contains_resource::<log_links::AssetOpenRequests>(),
        "drained"
    );
    // A dry run queues nothing.
    call(
        &mut world,
        "editor.log_open_link",
        json!({ "text": "assets/hero.glb", "dry_run": true }),
    )
    .expect("dry");
    assert!(!world.contains_resource::<log_links::AssetOpenRequests>());
    // A text asset with no panel opens in the code editor.
    let env_free = call(
        &mut world,
        "editor.log_open_link",
        json!({ "text": "assets/note.theme.ron", "dry_run": true }),
    );
    assert!(
        env_free.is_ok()
            || env_free
                .as_ref()
                .is_err_and(|c| c == "open_in_editor_no_code_editor")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_level_link_waits_for_the_save() {
    let root = regia_test_utils::unique_temp_dir("log-level");
    std::fs::create_dir_all(root.join("assets")).expect("dir");
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.clone()),
    });
    let open = world.resource::<OpenLevel>().clone();
    let path = root.join("assets/next.level.ron");
    level_io::save_level_file(world, &open, &path).expect("save");
    call(
        world,
        "editor.world_settings_set",
        json!({ "settings": { "gravity": [0, -2, 0] } }),
    )
    .expect("an unsaved edit");
    let mut content = regia_editor::content::ContentBrowserState::default();
    let queue = |world: &mut World| {
        world.insert_resource(log_links::AssetOpenRequests {
            paths: vec!["assets/next.level.ron".to_owned()],
        });
    };
    queue(world);
    let status = log_links::route_asset_requests(world, &mut content).expect("refused");
    assert!(status.contains("Save the level"), "{status}");
    assert_eq!(current_settings(world).gravity.y, -2.0, "the edit is still there");
    world.resource_mut::<UndoStack>().mark_saved();
    queue(world);
    assert_eq!(log_links::route_asset_requests(world, &mut content), None);
    assert_eq!(
        world.resource::<OpenLevel>().path.as_deref(),
        Some(path.as_path())
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_code_editor_preference_is_typed_text_with_a_default_of_nothing() {
    let prefs = EditorPreferences::default();
    assert_eq!(prefs.text(log_links::CODE_EDITOR_PREF), "");
    let mut prefs = EditorPreferences::default();
    assert_eq!(
        prefs.set(log_links::CODE_EDITOR_PREF, &PrefValue::Text("zed".to_owned())),
        Ok(true)
    );
    assert!(
        prefs
            .set(log_links::CODE_EDITOR_PREF, &PrefValue::Int(1))
            .is_err()
    );
    assert!(
        prefs
            .set(log_links::CODE_EDITOR_PREF, &PrefValue::Text("a\nb".to_owned()))
            .is_err()
    );
    assert!(
        prefs
            .set(log_links::CODE_EDITOR_PREF, &PrefValue::Text("x".repeat(2000)))
            .is_err()
    );
    // The file round trip keeps it.
    let (back, problems) = EditorPreferences::from_text(&prefs.to_text()).expect("reads");
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(back.text(log_links::CODE_EDITOR_PREF), "zed");
    let file = Path::new("/p/a.rs");
    assert_eq!(
        expand_command(&back.text(log_links::CODE_EDITOR_PREF), file, 2, None, &|_| None),
        ["zed", "/p/a.rs:2:1"]
    );
}

// ---------------------------------------------------------------------------------------------
// Key bindings
// ---------------------------------------------------------------------------------------------

/// Shell frames with the pointer resting over the viewport pane (no GPU).
fn pointer_over_viewport() -> egui::Event {
    egui::Event::PointerMoved(egui::pos2(640.0, 300.0))
}

fn drop_scene(world: &mut World) -> Entity {
    spawn_mesh(world, [20.0, 0.25, 20.0], at(0.0, 1.0, 0.0));
    let cube = spawn_mesh(world, [0.5, 0.5, 0.5], at(0.0, 6.0, 0.0));
    select(world, &[cube]);
    cube
}

#[test]
fn the_end_key_drops_the_selection_and_a_user_binding_replaces_it() {
    let mut app = edit_app();
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let (mut shell, root) = shell_in_project(world);
    let cube = drop_scene(world);
    frames_with(&mut shell, &ctx, world, 3, vec![pointer_over_viewport()]);
    frames_with(
        &mut shell,
        &ctx,
        world,
        2,
        vec![
            pointer_over_viewport(),
            key(egui::Key::End, true),
            key(egui::Key::End, false),
        ],
    );
    let y = world.get::<Transform>(cube).expect("t").translation.y;
    assert!((y - 1.75).abs() < 1.0e-3, "End dropped it: {y}");
    assert!(undo_world(world));
    assert_eq!(world.get::<Transform>(cube).expect("t").translation.y, 6.0);

    // Moved to J: End does nothing, J drops.
    call(
        world,
        "editor.keymap_bind",
        json!({ "bindings": { "selection.drop_to_floor": "J" } }),
    )
    .expect("bind");
    frames_with(
        &mut shell,
        &ctx,
        world,
        2,
        vec![
            pointer_over_viewport(),
            key(egui::Key::End, true),
            key(egui::Key::End, false),
        ],
    );
    assert_eq!(world.get::<Transform>(cube).expect("t").translation.y, 6.0);
    frames_with(
        &mut shell,
        &ctx,
        world,
        2,
        vec![
            pointer_over_viewport(),
            key(egui::Key::J, true),
            key(egui::Key::J, false),
        ],
    );
    let y = world.get::<Transform>(cube).expect("t").translation.y;
    assert!((y - 1.75).abs() < 1.0e-3, "J dropped it: {y}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn hold_commands_move_their_key_and_never_run_on_it() {
    let mut app = edit_app();
    let world = app.world_mut();
    for id in HOLD_COMMANDS {
        assert!(
            regia_editor::command_palette::find(id).is_some(),
            "{id} is a command"
        );
        assert!(
            regia_editor::command_palette::hold_chord(world, id).is_some(),
            "{id} has a built-in key"
        );
    }
    assert_eq!(
        regia_editor::command_palette::hold_chord(world, "camera.fly_forward"),
        KeyChord::parse("W")
    );
    call(
        world,
        "editor.keymap_bind",
        json!({ "bindings": { "camera.fly_forward": "I", "gizmo.vertex_snap": "Shift+B" } }),
    )
    .expect("bind");
    assert_eq!(
        regia_editor::command_palette::hold_chord(world, "camera.fly_forward"),
        KeyChord::parse("I")
    );
    // Holding the chord does not run the palette's toggle.
    let (mut shell, root) = shell_in_project(world);
    let ctx = egui::Context::default();
    let mut press = key(egui::Key::B, true);
    if let egui::Event::Key { modifiers, .. } = &mut press {
        *modifiers = egui::Modifiers::SHIFT;
    }
    frames_with(&mut shell, &ctx, world, 2, vec![press]);
    assert!(
        !world
            .get_resource::<PlacementOptions>()
            .is_some_and(|o| o.vertex_snap),
        "holding is not the toggle"
    );
    regia_editor::command_palette::run(world, "gizmo.vertex_snap");
    assert!(world.resource::<PlacementOptions>().vertex_snap);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_fly_and_outliner_keys_are_conflicts_of_commands_now() {
    let mut keymap = CommandKeymap::default();
    keymap
        .bind("view.toggle_grid", KeyChord::parse("Ctrl+G").expect("chord"))
        .expect("bind");
    let found = regia_editor::editor_keymap::conflicts(&keymap);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].shadows.contains("outliner.group"), "{found:?}");
    // Moving the outliner's command clears the conflict.
    keymap
        .bind("outliner.group", KeyChord::parse("Alt+G").expect("chord"))
        .expect("bind");
    assert!(regia_editor::editor_keymap::conflicts(&keymap).is_empty());
    let mut keymap = CommandKeymap::default();
    keymap
        .bind("view.toggle_grid", KeyChord::parse("A").expect("chord"))
        .expect("bind");
    let found = regia_editor::editor_keymap::conflicts(&keymap);
    assert!(found[0].shadows.contains("camera.fly_left"), "{found:?}");
}

fn spawn_named(world: &mut World, name: &str) -> Entity {
    let mut command = regia_editor::command::SpawnEntities::new_entity(
        world,
        regia_core::id::Guid::new_random(),
        name,
        Transform::IDENTITY,
    )
    .expect("spawn");
    regia_editor::command::EditorCommand::apply(&mut command, world);
    world.resource_mut::<UndoStack>().push(Box::new(command));
    world
        .iter_entities()
        .map(|e| e.id())
        .find(|e| regia_editor::outliner::display_name(world, *e) == name)
        .expect("spawned")
}

#[test]
fn the_outliner_commands_act_on_the_selection() {
    let mut app = edit_app();
    let world = app.world_mut();
    let thing = spawn_named(world, "Thing");
    select(world, &[thing]);
    assert_eq!(
        regia_editor::command_palette::run(world, "outliner.duplicate").as_deref(),
        Some("Duplicated 1")
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);
    assert!(undo_world(world));
    select(world, &[thing]);
    assert_eq!(
        regia_editor::command_palette::run(world, "outliner.delete").as_deref(),
        Some("Deleted 1")
    );
    assert!(world.get_entity(thing).is_err());
    world.insert_resource(Selection::default());
    assert_eq!(
        regia_editor::command_palette::run(world, "outliner.group").as_deref(),
        Some("Nothing selected")
    );
}

#[test]
fn a_user_shortcut_replaces_the_outliners_built_in_key() {
    let mut app = edit_app();
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let (mut shell, root) = shell_in_project(world);
    let over_outliner = || egui::Event::PointerMoved(egui::pos2(60.0, 300.0));
    let press = |k: egui::Key| vec![over_outliner(), key(k, true), key(k, false)];
    let first = spawn_named(world, "First");
    let second = spawn_named(world, "Second");
    frames_with(&mut shell, &ctx, world, 3, vec![over_outliner()]);

    // The built-in key works with the pointer there.
    select(world, &[first]);
    frames_with(&mut shell, &ctx, world, 2, press(egui::Key::Delete));
    assert!(world.get_entity(first).is_err(), "Delete deleted it");

    // Moved: Delete does nothing, the new key deletes.
    call(
        world,
        "editor.keymap_bind",
        json!({ "bindings": { "outliner.delete": "K" } }),
    )
    .expect("bind");
    select(world, &[second]);
    frames_with(&mut shell, &ctx, world, 2, press(egui::Key::Delete));
    assert!(world.get_entity(second).is_ok(), "the old key no longer deletes");
    frames_with(&mut shell, &ctx, world, 2, press(egui::Key::K));
    assert!(world.get_entity(second).is_err(), "K deleted it");
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Real pointer events through the panels
// ---------------------------------------------------------------------------------------------

/// Drives one panel in a headless egui context, with a clock that only moves forward.
struct PanelDriver {
    ctx: egui::Context,
    /// What the last frame drew: each text and where.
    texts: Vec<(String, egui::Rect)>,
    /// The rect the panel was given.
    rect: egui::Rect,
}

fn collect_text(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
    match shape {
        egui::Shape::Text(text) => out.push((
            text.galley.text().to_owned(),
            text.galley.rect.translate(text.pos.to_vec2()),
        )),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text(shape, out);
            }
        }
        _ => {}
    }
}

impl PanelDriver {
    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            texts: Vec::new(),
            rect: egui::Rect::NOTHING,
        }
    }

    fn frame(&mut self, events: Vec<egui::Event>, draw: &mut dyn FnMut(&mut egui::Ui)) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            time: Some(self.ctx.input(|i| i.time) + 1.0 / 60.0),
            predicted_dt: 1.0 / 60.0,
            events,
            ..Default::default()
        };
        let mut rect = egui::Rect::NOTHING;
        let mut output = self.ctx.run_ui(raw, |ui| {
            rect = ui.available_rect_before_wrap();
            draw(ui);
        });
        output.textures_delta.clear();
        self.rect = rect;
        self.texts.clear();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut self.texts);
        }
    }

    /// Where the text matching `find` was drawn.
    fn text_at(&self, find: &dyn Fn(&str) -> bool) -> Option<egui::Pos2> {
        self.texts
            .iter()
            .find(|(text, _)| find(text))
            .map(|(_, rect)| rect.center())
    }

    /// Moves the pointer to `at`, presses, releases: one click, with the frames egui needs.
    fn click(&mut self, at: egui::Pos2, draw: &mut dyn FnMut(&mut egui::Ui)) {
        self.frame(vec![egui::Event::PointerMoved(at)], draw);
        self.frame(vec![button(at, true)], draw);
        self.frame(vec![button(at, false)], draw);
        self.frame(Vec::new(), draw);
    }

    /// Presses at `from`, moves in `steps` to `to`, releases there.
    fn drag(&mut self, from: egui::Pos2, to: egui::Pos2, steps: u32, draw: &mut dyn FnMut(&mut egui::Ui)) {
        self.frame(vec![egui::Event::PointerMoved(from)], draw);
        self.frame(vec![button(from, true)], draw);
        for step in 1..=steps {
            let at = from + (to - from) * (step as f32 / steps as f32);
            self.frame(vec![egui::Event::PointerMoved(at)], draw);
        }
        self.frame(vec![button(to, false)], draw);
        self.frame(Vec::new(), draw);
    }
}

fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    }
}

/// A camera looking along -Z from z = 12, as the editor's orbit camera.
fn orbit_camera() -> regia_editor::viewport::EditorCamera {
    regia_editor::viewport::EditorCamera {
        target: Vec3::ZERO,
        distance: 12.0,
        yaw: 0.0,
        pitch: 0.0,
        ..Default::default()
    }
}

fn pane_view(world: &World) -> ViewCamera {
    ViewCamera::of(
        regia_editor::views::ViewKind::Perspective,
        world.resource::<regia_editor::viewport::EditorCamera>(),
    )
}

/// The screen point of a world point in a pane at `rect`.
fn on_screen(world: &World, rect: egui::Rect, point: Vec3) -> egui::Pos2 {
    let size = Vec2::new(rect.width(), rect.height());
    let at = pane_view(world)
        .project(point, size)
        .expect("in front of the camera");
    rect.min + egui::vec2(at.x, at.y)
}

/// Presses a translate gizmo's X arrow and drags it to `to`, with `extra` events on the first frame.
/// The arrow's exact length is the gizmo's business, so this tries points along it until one moves
/// the entity. Returns whether one did.
fn drag_gizmo_x(
    panel: &mut PanelDriver,
    world: &mut World,
    state: &mut regia_editor::viewport::ViewportPanelState,
    entity: Entity,
    to: egui::Pos2,
) -> bool {
    let start = *world.get::<Transform>(entity).expect("transform");
    let center = on_screen(world, panel.rect, start.translation);
    for along in (30..=90).step_by(6) {
        let grab = center + egui::vec2(along as f32, 0.0);
        let mut draw = |ui: &mut egui::Ui| regia_editor::viewport::viewport_panel(ui, world, None, state);
        panel.drag(grab, to, 6, &mut draw);
        if world.get::<Transform>(entity).expect("transform").translation != start.translation {
            return true;
        }
    }
    false
}

#[test]
fn holding_v_during_a_real_gizmo_drag_lands_the_pivot_on_the_vertex() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(orbit_camera());
    let start = at(-3.0, 0.0, 0.0);
    let moved = spawn_mesh(world, [0.1, 0.1, 0.1], start);
    spawn_mesh(world, [1.0, 1.0, 1.0], at(2.0, 0.5, 0.0));
    regia_scene::propagate::propagate_transforms(world);
    select(world, &[moved]);
    let mut state = regia_editor::viewport::ViewportPanelState::default();
    state.toolbar = false;
    let mut panel = PanelDriver::new();
    panel.frame(Vec::new(), &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });

    // Without V the X arrow moves along X only: the check that this drag reaches the gizmo at all.
    let target = on_screen(world, panel.rect, Vec3::new(2.9, 1.4, 1.0));
    assert!(
        drag_gizmo_x(&mut panel, world, &mut state, moved, target),
        "the drag reached the gizmo"
    );
    let free = world.get::<Transform>(moved).expect("t").translation;
    assert_eq!(
        (free.y, free.z),
        (0.0, 0.0),
        "the arrow keeps to its axis: {free}"
    );
    assert!(free.x > -3.0, "{free}");
    world.resource_mut::<UndoStack>().seal();
    let depth = world.resource::<UndoStack>().undo_depth();
    assert!(undo_world(world));
    assert_eq!(world.get::<Transform>(moved), Some(&start));

    // With V held the same drag ends on the corner of the box under the pointer.
    panel.frame(vec![key(egui::Key::V, true)], &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });
    assert!(
        drag_gizmo_x(&mut panel, world, &mut state, moved, target),
        "the drag reached the gizmo"
    );
    let snapped = world.get::<Transform>(moved).expect("t").translation;
    assert_eq!(snapped, Vec3::new(3.0, 1.5, 1.0), "the box's corner");
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth,
        "one drag, one step"
    );
    assert!(undo_world(world));
    assert_eq!(world.get::<Transform>(moved), Some(&start));
}

#[test]
fn a_real_pointer_drag_on_a_grip_edits_the_collider_and_keeps_the_selection() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(orbit_camera());
    let start = Vec3::new(1.0, 1.0, 1.0);
    let entity = collider_entity(
        world,
        ColliderShape::Box { half_extents: start },
        Transform::IDENTITY,
    );
    select(world, &[entity]);
    regia_editor::command_palette::run(world, "collider.toggle_handles");
    let mut state = regia_editor::viewport::ViewportPanelState::default();
    state.toolbar = false;
    let mut panel = PanelDriver::new();
    let draw = |world: &mut World, state: &mut _, ui: &mut egui::Ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, state);
    };
    panel.frame(Vec::new(), &mut |ui| draw(world, &mut state, ui));
    let from = on_screen(world, panel.rect, Vec3::new(1.0, 0.0, 0.0));
    let to = on_screen(world, panel.rect, Vec3::new(3.0, 0.0, 0.0));

    // A click on the grip neither changes anything nor clears the selection.
    panel.click(from, &mut |ui| draw(world, &mut state, ui));
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    assert_eq!(world.resource::<Selection>().primary(), Some(entity));

    panel.drag(from, to, 8, &mut |ui| draw(world, &mut state, ui));
    let ColliderShape::Box { half_extents } = world.get::<Collider>(entity).expect("c").shape else {
        panic!("box");
    };
    assert!((half_extents.x - 3.0).abs() < 0.05, "{half_extents}");
    assert_eq!((half_extents.y, half_extents.z), (1.0, 1.0));
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        1,
        "one drag, one step"
    );
    assert_eq!(world.resource::<Selection>().primary(), Some(entity));
    // The transform gizmo is not under the grips: the entity did not move.
    assert_eq!(world.get::<Transform>(entity), Some(&Transform::IDENTITY));
    assert!(undo_world(world));
    assert_eq!(
        world.get::<Collider>(entity).expect("c").shape,
        ColliderShape::Box { half_extents: start }
    );
}

#[test]
fn the_tools_menu_and_the_c_key_reach_their_options() {
    let mut app = edit_app();
    let world = app.world_mut();
    let mut state = regia_editor::viewport::ViewportPanelState::default();
    let mut panel = PanelDriver::new();
    let draw = |world: &mut World, state: &mut _, ui: &mut egui::Ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, state);
    };
    panel.frame(Vec::new(), &mut |ui| draw(world, &mut state, ui));
    // Both sit in the Tools menu, so the toolbar keeps its width.
    let open_tools = |panel: &mut PanelDriver, world: &mut World, state: &mut _| {
        let tools = panel.text_at(&|t| t == "Tools").expect("the toolbar shows it");
        panel.click(tools, &mut |ui| draw(world, state, ui));
    };
    open_tools(&mut panel, world, &mut state);
    let align = panel
        .text_at(&|t| t == "Align to surface")
        .expect("the menu shows it");
    panel.click(align, &mut |ui| draw(world, &mut state, ui));
    assert!(world.resource::<PlacementOptions>().align_to_normal);
    if panel.text_at(&|t| t == "Collider handles (C)").is_none() {
        open_tools(&mut panel, world, &mut state);
    }
    let handles = panel
        .text_at(&|t| t == "Collider handles (C)")
        .expect("the menu shows it");
    panel.click(handles, &mut |ui| draw(world, &mut state, ui));
    assert!(world.resource::<ColliderHandleTool>().enabled);
    // Close the menu (a key goes to the viewport).
    let quiet = panel.rect.center();
    panel.click(quiet, &mut |ui| draw(world, &mut state, ui));
    // C flips it back, and a user binding takes the key away.
    let press = |k: egui::Key| vec![key(k, true), key(k, false)];
    panel.frame(press(egui::Key::C), &mut |ui| draw(world, &mut state, ui));
    assert!(!world.resource::<ColliderHandleTool>().enabled);
    call(
        world,
        "editor.keymap_bind",
        json!({ "bindings": { "collider.toggle_handles": "Alt+C" } }),
    )
    .expect("bind");
    panel.frame(press(egui::Key::C), &mut |ui| draw(world, &mut state, ui));
    assert!(!world.resource::<ColliderHandleTool>().enabled);
}

#[test]
fn the_world_settings_panel_draws_edits_and_resets_through_the_undo_stack() {
    let mut app = edit_app();
    let world = app.world_mut();
    let mut panel = PanelDriver::new();
    let draw =
        |world: &mut World, ui: &mut egui::Ui| regia_editor::world_settings::world_settings_panel(ui, world);
    call(
        world,
        "editor.world_settings_set",
        json!({ "settings": { "gravity": [0, -3, 0] } }),
    )
    .expect("set");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    panel.frame(Vec::new(), &mut |ui| draw(world, ui));
    panel.frame(Vec::new(), &mut |ui| draw(world, ui));
    for field in ["gravity", "atmosphere", "look", "streaming"] {
        assert!(
            panel.text_at(&|t| t.to_lowercase().contains(field)).is_some(),
            "{field} is drawn: {:?}",
            panel.texts.iter().map(|(t, _)| t).collect::<Vec<_>>()
        );
    }

    // Dragging the gravity's y field changes it as one undo step.
    let field = panel
        .text_at(&|t| t.trim().parse::<f32>().ok() == Some(-3.0))
        .expect("the y field shows -3");
    panel.drag(field, field + egui::vec2(40.0, 0.0), 8, &mut |ui| draw(world, ui));
    let edited = current_settings(world).gravity.y;
    assert_ne!(edited, -3.0, "the drag reached the field");
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        2,
        "one drag, one step"
    );
    assert!(undo_world(world));
    assert_eq!(current_settings(world).gravity.y, -3.0);
    panel.frame(Vec::new(), &mut |ui| draw(world, ui));

    // The button resets everything, undoably.
    let reset = panel.text_at(&|t| t == "Reset to defaults").expect("button");
    panel.click(reset, &mut |ui| draw(world, ui));
    assert_eq!(current_settings(world), LevelSettings::default());
    assert!(undo_world(world));
    assert_eq!(current_settings(world).gravity.y, -3.0);
    // With no level open the panel says so.
    world.remove_resource::<OpenLevel>();
    panel.frame(Vec::new(), &mut |ui| draw(world, ui));
    assert!(panel.text_at(&|t| t.contains("Open a level")).is_some());
}

#[test]
fn a_click_on_a_link_in_the_output_log_panel_returns_it_and_opens_it() {
    let root = project_with("src/only_here_0451.rs", "\n");
    let mut world = world_in(&root);
    let message = "failed at src/only_here_0451.rs:5:2 with a plain tail";
    regia_core::log::ring::push("ERROR", "viewport_tools", message.to_owned());
    let mut state = regia_editor::log_panel::LogPanelState::default();
    state.contains = "only_here_0451".to_owned();
    let mut panel = PanelDriver::new();
    let mut clicked = None;
    let mut draw = |ui: &mut egui::Ui| {
        if let Some(link) = regia_editor::log_panel::output_log_panel(ui, &mut state) {
            clicked = Some(link);
        }
    };
    panel.frame(Vec::new(), &mut draw);
    panel.frame(Vec::new(), &mut draw);
    // The line draws as text, link, text.
    let link_at = panel
        .text_at(&|t| t == "src/only_here_0451.rs:5:2")
        .expect("the link is its own widget");
    assert!(panel.text_at(&|t| t.contains("plain tail")).is_some());
    assert!(panel.text_at(&|t| t.contains("failed at")).is_some());
    panel.click(link_at, &mut draw);
    let link = clicked.expect("the click returned the link");
    assert_eq!(link.path(), "src/only_here_0451.rs");
    assert_eq!(
        link.target,
        LinkTarget::File {
            line: 5,
            col: Some(2)
        }
    );
    // What the shell does with it: no editor configured reads as a status line, not a panic.
    let mut prefs = EditorPreferences::default();
    prefs
        .set(
            log_links::CODE_EDITOR_PREF,
            &PrefValue::Text("regia-no-such-program {file}".to_owned()),
        )
        .expect("set");
    world.insert_resource(prefs);
    let status = log_links::open_clicked(&mut world, &link);
    assert!(status.contains("regia-no-such-program"), "{status}");
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Through the editor's own registrations and level content
// ---------------------------------------------------------------------------------------------

#[test]
fn a_collider_added_in_the_editor_is_edited_saved_and_reloaded() {
    let root = regia_test_utils::unique_temp_dir("collider-level");
    let mut app = edit_app();
    let world = app.world_mut();
    let mut spawn = regia_editor::command::SpawnEntities::new_entity(
        world,
        regia_core::id::Guid::new_random(),
        "Crate",
        Transform::IDENTITY,
    )
    .expect("spawn");
    regia_editor::command::EditorCommand::apply(&mut spawn, world);
    world.resource_mut::<UndoStack>().push(Box::new(spawn));
    let entity = world
        .iter_entities()
        .map(|e| e.id())
        .find(|e| regia_editor::outliner::display_name(world, *e) == "Crate")
        .expect("spawned");
    call(
        world,
        "editor.add_component",
        json!({ "entity": entity.to_bits(), "component": "regia_physics::components::Collider" }),
    )
    .expect("the editor knows Collider with no physics plugin");
    assert!(world.get::<Collider>(entity).is_some());
    call(
        world,
        "editor.collider_handle_drag",
        json!({ "entity": entity.to_bits(), "handle": "extent_x+", "to": [2.5, 0, 0] }),
    )
    .expect("drag");
    let file = root.join("crate.level.ron");
    let open = world.resource::<OpenLevel>().clone();
    level_io::save_level_file(world, &open, &file).expect("save");

    let mut next = edit_app();
    let world = next.world_mut();
    let opened = level_io::open_level_file(world, &file).expect("open");
    world.insert_resource(opened);
    let found = world
        .iter_entities()
        .map(|e| e.id())
        .find(|e| regia_editor::outliner::display_name(world, *e) == "Crate")
        .expect("in the level");
    let ColliderShape::Box { half_extents } = world.get::<Collider>(found).expect("saved").shape else {
        panic!("box");
    };
    assert!((half_extents.x - 2.5).abs() < 1e-4, "{half_extents}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn greybox_shapes_drop_snap_and_count_as_surfaces_through_their_hidden_parts() {
    use regia_render::{Primitive, PrimitiveShape};
    let mut world = World::new();
    world.init_resource::<UndoStack>();
    let cuboid = |size: Vec3| Primitive::new(PrimitiveShape::Cuboid { size });
    let floor = world
        .spawn((at(0.0, 0.0, 0.0), cuboid(Vec3::new(20.0, 1.0, 20.0))))
        .id();
    let cube = world.spawn((at(1.0, 5.0, 0.0), cuboid(Vec3::ONE))).id();
    let report = drop_to_floor(&mut world, &[cube], false);
    assert_eq!(report.placed.len(), 1, "{report:?}");
    assert_eq!(report.placed[0].surface, floor, "the level entity, not its part");
    let y = world.get::<Transform>(cube).expect("t").translation.y;
    assert!((y - 1.0).abs() < 1.0e-3, "{y}");
    assert!(undo_world(&mut world));
    // A frame passes: transforms and hidden parts follow the undo.
    regia_scene::propagate::propagate_transforms(&mut world);
    regia_editor::mesh_source::resolve_mesh_sources(&mut world);

    // Vertex snap looks through the moved shape's hidden part too.
    let view = ViewCamera {
        eye: Vec3::new(1.0, 5.0, 12.0),
        target: Vec3::new(1.0, 5.0, 0.0),
        ..camera()
    };
    let pixel = view.project(Vec3::new(1.4, 5.4, 0.5), SIZE).expect("visible");
    assert!(
        vertex_snap_at(&world, &view, SIZE, pixel, &[cube]).is_none(),
        "only the moved shape is under the pointer"
    );
    let own = vertex_snap_at(&world, &view, SIZE, pixel, &[]).expect("hit");
    assert_eq!(own.hit.owner, cube);
    assert!(
        (own.position - Vec3::new(1.5, 5.5, 0.5)).length() < 1e-3,
        "{}",
        own.position
    );
}

#[test]
fn in_four_views_each_pane_drags_grips_through_its_own_camera() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(orbit_camera());
    world
        .get_resource_or_insert_with(regia_editor::views::ViewportLayout::default)
        .quad = true;
    let start = Vec3::ONE;
    let entity = collider_entity(
        world,
        ColliderShape::Box { half_extents: start },
        Transform::IDENTITY,
    );
    select(world, &[entity]);
    regia_editor::command_palette::run(world, "collider.toggle_handles");
    let mut state = regia_editor::viewport::ViewportPanelState::default();
    state.toolbar = false;
    let mut panel = PanelDriver::new();
    let draw = |world: &mut World, state: &mut _, ui: &mut egui::Ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, state);
    };
    panel.frame(Vec::new(), &mut |ui| draw(world, &mut state, ui));
    // The top-left pane of the quad is pane 0, the perspective one.
    let half = panel.rect.size() * 0.5 - egui::vec2(2.0, 2.0);
    let pane = egui::Rect::from_min_size(panel.rect.min, half);
    let from = on_screen(world, pane, Vec3::new(1.0, 0.0, 0.0));
    let to = on_screen(world, pane, Vec3::new(3.0, 0.0, 0.0));
    let extents = |world: &World| match world.get::<Collider>(entity).expect("c").shape {
        ColliderShape::Box { half_extents } => half_extents,
        _ => panic!("box"),
    };
    panel.frame(vec![egui::Event::PointerMoved(from)], &mut |ui| {
        draw(world, &mut state, ui);
    });
    panel.frame(vec![button(from, true)], &mut |ui| draw(world, &mut state, ui));
    // Mid-drag too: another pane reading this drag through its own camera shows only here, since
    // on release pane 0 draws first and ends the drag.
    for step in 1..=8 {
        let t = step as f32 / 8.0;
        let at = on_screen(world, pane, Vec3::new(1.0 + 2.0 * t, 0.0, 0.0));
        panel.frame(vec![egui::Event::PointerMoved(at)], &mut |ui| {
            draw(world, &mut state, ui);
        });
        let live = extents(world).x;
        assert!((live - (1.0 + 2.0 * t)).abs() < 0.1, "step {step}: {live}");
    }
    panel.frame(vec![button(to, false)], &mut |ui| draw(world, &mut state, ui));
    let half_extents = extents(world);
    assert!((half_extents.x - 3.0).abs() < 0.1, "{half_extents}");
    assert_eq!((half_extents.y, half_extents.z), (1.0, 1.0));
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        1,
        "one drag, one step"
    );
}

// ---------------------------------------------------------------------------------------------
// A plain drop keeps the local rotation under a sheared parent
// ---------------------------------------------------------------------------------------------

#[test]
fn a_plain_drop_under_a_non_uniformly_scaled_parent_keeps_the_local_rotation() {
    let (mut world, _) = floor_scene();
    let parent_pose = Transform {
        translation: Vec3::new(0.0, 8.0, 0.0),
        scale: Vec3::new(3.0, 1.0, 1.0),
        ..Transform::IDENTITY
    };
    let parent = world
        .spawn((parent_pose, GlobalTransform(parent_pose.compute_affine())))
        .id();
    // Turned about z under a stretch along x: the child's world matrix is sheared, so it has no exact
    // rotation of its own to read back.
    let pose = Transform {
        translation: Vec3::new(0.0, 1.0, 0.0),
        rotation: Quat::from_rotation_z(0.6),
        ..Transform::IDENTITY
    };
    let child = spawn_mesh(&mut world, [0.5, 0.5, 0.5], pose);
    world.entity_mut(child).insert(ChildOf(parent));
    regia_scene::propagate::propagate_transforms(&mut world);
    drop_to_floor(&mut world, &[child], false);
    let t = world.get::<Transform>(child).expect("t");
    assert_eq!(t.rotation, pose.rotation, "only the translation changes");
    assert_eq!(t.scale, pose.scale);
    assert_eq!(t.translation.x, pose.translation.x, "straight down");
    let gap = lowest_y(&world, child) - 1.25;
    assert!(gap.abs() < 1.0e-3, "gap {gap}");
}

// ---------------------------------------------------------------------------------------------
// Terrain as a surface
// ---------------------------------------------------------------------------------------------

/// A 256 m terrain of 1 m samples, centered on the origin, whose height is `0.2 * x`.
fn sloped_terrain(world: &mut World) -> Entity {
    let n = 257u32;
    let heights = (0..n * n).map(|i| ((i % n) as f32 - 128.0) * 0.2).collect();
    let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 256.0, 100.0, -50.0).expect("map");
    let heightfield = map.to_heightfield();
    let terrain = regia_terrain::Terrain::new(
        &heightfield,
        256.0,
        100.0,
        -50.0,
        regia_terrain::CdlodConfig::default(),
        4,
        regia_terrain::TerrainLayers::default(),
    )
    .expect("terrain");
    world.spawn(terrain).id()
}

fn terrain_scene() -> (World, Entity) {
    let mut world = World::new();
    world.init_resource::<UndoStack>();
    let terrain = sloped_terrain(&mut world);
    (world, terrain)
}

/// The terrain's height straight below `(x, z)`.
fn ground_y(world: &World, x: f32, z: f32) -> f32 {
    regia_terrain::pick_terrain(world, Vec3::new(x, 90.0, z), Vec3::NEG_Y, 500.0)
        .expect("over the terrain")
        .position
        .y
}

#[test]
fn a_box_lands_on_terrain_by_the_high_side_of_its_footprint() {
    let (mut world, terrain) = terrain_scene();
    let cube = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(10.0, 40.0, 5.0));
    let report = drop_to_floor(&mut world, &[cube], false);
    assert_eq!(report.placed.len(), 1, "{report:?}");
    assert_eq!(report.placed[0].surface, terrain);
    // The slope rises along +x: the corner at x = 10.5 touches first, nothing sinks in.
    let lowest = lowest_y(&world, cube);
    let contact = ground_y(&world, 10.5, 5.0);
    assert!((contact - 2.1).abs() < 1.0e-2, "the fixture's slope: {contact}");
    assert!((lowest - contact).abs() < 1.0e-3, "{lowest} vs {contact}");
    assert_eq!(world.get::<Transform>(cube).expect("t").rotation, Quat::IDENTITY);
    assert!(undo_world(&mut world));
    assert_eq!(world.get::<Transform>(cube), Some(&at(10.0, 40.0, 5.0)));
}

#[test]
fn align_reads_the_terrain_normal_and_rests_on_the_slope() {
    let (mut world, terrain) = terrain_scene();
    let cube = spawn_mesh(&mut world, [0.5, 1.0, 0.5], at(-20.0, 40.0, 7.0));
    let report = drop_to_floor(&mut world, &[cube], true);
    let placed = report.placed.first().copied().expect("placed");
    assert_eq!(placed.surface, terrain);
    // z is flat and x rises 0.2 per metre.
    let normal = Vec3::new(-0.2, 1.0, 0.0).normalize();
    assert!((placed.normal - normal).length() < 5.0e-3, "{}", placed.normal);
    let t = world.get::<Transform>(cube).expect("t");
    assert!((t.rotation * Vec3::Y - placed.normal).length() < 1e-4);
    // The ground's plane passes through the origin along that normal. The height samples are 16-bit
    // over 100 m (1.5 mm), so this is looser than the mesh case.
    let support = world_vertices(&world, cube)
        .iter()
        .map(|p| p.dot(normal))
        .fold(f32::MAX, f32::min);
    assert!(support.abs() < 5.0e-3, "support {support}");
}

#[test]
fn off_the_edge_of_a_terrain_the_normal_falls_back_to_up() {
    let (mut world, terrain) = terrain_scene();
    // The neighbor samples of the normal are 0.25 m away: east of here leaves the 128 m half width.
    let cube = spawn_mesh(&mut world, [0.05, 0.05, 0.05], at(127.9, 60.0, 0.0));
    let report = drop_to_floor(&mut world, &[cube], true);
    let placed = report.placed.first().copied().expect("placed");
    assert_eq!(placed.surface, terrain);
    assert_eq!(placed.normal, Vec3::Y);
    assert_eq!(world.get::<Transform>(cube).expect("t").rotation, Quat::IDENTITY);
}

#[test]
fn a_mesh_above_the_terrain_stops_a_drop_and_one_below_it_does_not() {
    let (mut world, terrain) = terrain_scene();
    let plate = spawn_mesh(&mut world, [2.0, 0.25, 2.0], at(10.0, 10.0, 0.0));
    let cube = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(10.0, 40.0, 0.0));
    let report = drop_to_floor(&mut world, &[cube], false);
    assert_eq!(report.placed[0].surface, plate);
    let gap = lowest_y(&world, cube) - 10.25;
    assert!(gap.abs() < 1.0e-3, "gap {gap}");
    // Buried below the ground, a mesh is no surface: the terrain stops the drop first.
    let buried = spawn_mesh(&mut world, [2.0, 0.25, 2.0], at(-30.0, -20.0, 0.0));
    let other = spawn_mesh(&mut world, [0.5, 0.5, 0.5], at(-30.0, 40.0, 0.0));
    let report = drop_to_floor(&mut world, &[other], false);
    assert_ne!(report.placed[0].surface, buried);
    assert_eq!(report.placed[0].surface, terrain);
}

#[test]
fn vertex_snap_finds_nothing_when_terrain_hides_the_mesh() {
    let (mut world, _) = terrain_scene();
    let view = ViewCamera {
        eye: Vec3::new(10.0, 60.0, 5.0),
        target: Vec3::new(10.0, 0.0, 0.0),
        fov_y: 0.8,
        near: 0.1,
        far: 500.0,
        ortho: false,
    };
    let above = spawn_mesh(&mut world, [1.0, 1.0, 1.0], at(10.0, 20.0, 0.0));
    let pixel = view.project(Vec3::new(10.9, 21.0, 0.9), SIZE).expect("visible");
    let snap = vertex_snap_at(&world, &view, SIZE, pixel, &[]).expect("above the ground");
    assert_eq!(snap.hit.entity, above);
    assert_eq!(snap.position, Vec3::new(11.0, 21.0, 1.0));
    // The same ray with that box buried: the ground is nearer than the mesh.
    world
        .entity_mut(above)
        .insert(GlobalTransform(at(10.0, -40.0, 0.0).compute_affine()));
    let buried = view.project(Vec3::new(10.9, -39.0, 0.9), SIZE).expect("in view");
    assert_eq!(vertex_snap_at(&world, &view, SIZE, buried, &[]), None);
}

// ---------------------------------------------------------------------------------------------
// Skinned meshes
// ---------------------------------------------------------------------------------------------

/// A box of half extents `half` bound to one joint, drawn where `palette` puts it.
fn spawn_skinned(world: &mut World, half: [f32; 3], palette: regia_render::JointMatrix) -> Entity {
    let (mut vertices, indices) = regia_render::box_mesh(half);
    for vertex in &mut vertices {
        vertex.joints = [0; 4];
        vertex.weights = [1.0, 0.0, 0.0, 0.0];
    }
    world
        .spawn((
            Transform::IDENTITY,
            GlobalTransform(Transform::IDENTITY.compute_affine()),
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(LabMaterial::default()),
            ),
            regia_render::SkinJoints {
                palette: vec![palette],
            },
        ))
        .id()
}

#[test]
fn vertex_snap_reads_a_skinned_mesh_where_its_palette_draws_it() {
    let mut world = World::new();
    let skinned = spawn_skinned(
        &mut world,
        [1.0, 1.0, 1.0],
        regia_render::JointMatrix::translation(3.0, 0.0, 0.0),
    );
    let view = camera();
    let pixel = view.project(Vec3::new(3.9, 0.9, 1.0), SIZE).expect("visible");
    let snap = vertex_snap_at(&world, &view, SIZE, pixel, &[]).expect("the posed box");
    assert_eq!(snap.hit.entity, skinned);
    assert_eq!(snap.position, Vec3::new(4.0, 1.0, 1.0));
    // Nothing is left at the bind pose.
    let bind = view.project(Vec3::new(0.0, 0.0, 1.0), SIZE).expect("visible");
    assert_eq!(vertex_snap_at(&world, &view, SIZE, bind, &[]), None);
}

#[test]
fn a_skinned_mesh_drops_from_its_pose_and_is_a_surface_where_its_pose_is() {
    // A skinned floor posed up so its top is at 1.25, with nothing at its bind pose.
    let mut world = World::new();
    world.init_resource::<UndoStack>();
    let floor = spawn_skinned(
        &mut world,
        [20.0, 0.25, 20.0],
        regia_render::JointMatrix::translation(0.0, 1.0, 0.0),
    );
    // A skinned box bound at the origin, drawn 4 m up.
    let body = spawn_skinned(
        &mut world,
        [0.5, 0.5, 0.5],
        regia_render::JointMatrix::translation(0.0, 4.0, 0.0),
    );
    let report = drop_to_floor(&mut world, &[body], false);
    assert_eq!(
        report.placed.first().map(|p| p.surface),
        Some(floor),
        "{report:?}"
    );
    // Drawn at pivot + 4, lowest vertex at pivot + 3.5, resting on 1.25.
    let y = world.get::<Transform>(body).expect("t").translation.y;
    assert!((y - (1.25 - 3.5)).abs() < 1.0e-3, "pivot y {y}");
}

// ---------------------------------------------------------------------------------------------
// `agent.open_in_editor` and the preference; prefab links
// ---------------------------------------------------------------------------------------------

/// Runs `agent.open_in_editor`'s checked request through the hook the editor installed, as a dry run.
fn open_in_editor_dry(world: &mut World, params: &Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let target = regia_agent::open_in_editor::parse_target(world, params)?;
    let hook = world
        .resource::<regia_agent::open_in_editor::OpenInEditorHook>()
        .0;
    hook(world, &target, true)
}

#[test]
fn agent_open_in_editor_reads_the_preference_before_the_project_config() {
    let root = project_with("src/a.rs", "\n");
    let mut world = world_in(&root);
    // Native separators: on Windows the resolved file is `...\src\a.rs` (ADR-7566).
    let file = root.join("src").join("a.rs").display().to_string();
    let params = json!({ "file": "src/a.rs", "line": 4 });
    // Nothing configured: refused, with no fallback to the environment for an agent's request.
    let none = open_in_editor_dry(&mut world, &params).expect_err("unconfigured");
    assert_eq!(
        none.data.map(|d| d["reason"].clone()).unwrap_or_default(),
        "open_in_editor_no_code_editor"
    );
    // The project config answers.
    let state = regia_core::brand::project_state_dir(&root);
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(
        state.join("agent.toml"),
        "[editor]\ncode_editor = [\"from-config\", \"{file}:{line}\"]\n",
    )
    .expect("config");
    let reply = open_in_editor_dry(&mut world, &params).expect("configured");
    assert_eq!(reply["command"], json!(["from-config", format!("{file}:4")]));
    // The owner's preference wins over it.
    let mut prefs = EditorPreferences::default();
    prefs
        .set(log_links::CODE_EDITOR_PREF, &PrefValue::Text("zed".to_owned()))
        .expect("set");
    world.insert_resource(prefs);
    let reply = open_in_editor_dry(&mut world, &params).expect("preference");
    assert_eq!(reply["command"], json!(["zed", format!("{file}:4:1")]));
    // A blank preference is no preference.
    let mut prefs = EditorPreferences::default();
    prefs
        .set(log_links::CODE_EDITOR_PREF, &PrefValue::Text("   ".to_owned()))
        .expect("set");
    world.insert_resource(prefs);
    let reply = open_in_editor_dry(&mut world, &params).expect("back to the config");
    assert_eq!(reply["command"], json!(["from-config", format!("{file}:4")]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_prefab_link_enters_prefab_editing_and_a_bad_one_says_why() {
    let root = regia_test_utils::unique_temp_dir("log-prefab");
    let rel = regia_editor::content::create_asset(
        &root,
        regia_editor::content::NewAssetKind::Prefab,
        "props/crate.prefab.ron",
    )
    .expect("prefab");
    std::fs::write(root.join("assets/bad.prefab.ron"), "not ron at all (").expect("file");
    let mut app = edit_app();
    let world = app.world_mut();
    world.init_resource::<regia_editor::prefab_edit::PrefabEditMode>();
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.clone()),
    });
    let mut content = regia_editor::content::ContentBrowserState::default();
    // A broken file reports the parse failure and stays out of prefab mode.
    let reply = call(
        world,
        "editor.log_open_link",
        json!({ "text": "assets/bad.prefab.ron failed" }),
    )
    .expect("queued");
    assert_eq!(reply["action"], json!("prefab"));
    let status = log_links::route_asset_requests(world, &mut content).expect("refused");
    assert!(status.contains("cannot parse"), "{status}");
    let mode = world.resource::<regia_editor::prefab_edit::PrefabEditMode>();
    assert_eq!(mode.editing(), None);
    // A good one opens, routed through the agent method as a click is.
    let reply = call(
        world,
        "editor.log_open_link",
        json!({ "text": format!("spawned {rel} ok") }),
    )
    .expect("queued");
    assert_eq!(reply["action"], json!("prefab"));
    assert_eq!(log_links::route_asset_requests(world, &mut content), None);
    let mode = world.resource::<regia_editor::prefab_edit::PrefabEditMode>();
    assert_eq!(mode.editing(), Some(root.join(&rel).as_path()));
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Collider grips beside other viewport tools; a level link over a prefab or a running game
// ---------------------------------------------------------------------------------------------

/// A unit box collider, selected, with the tool on, and a drawn pane. Returns the entity and the panel
/// state.
fn grip_panel(
    world: &mut World,
    panel: &mut PanelDriver,
) -> (Entity, regia_editor::viewport::ViewportPanelState) {
    world.insert_resource(orbit_camera());
    let entity = collider_entity(
        world,
        ColliderShape::Box {
            half_extents: Vec3::ONE,
        },
        Transform::IDENTITY,
    );
    select(world, &[entity]);
    regia_editor::command_palette::run(world, "collider.toggle_handles");
    let mut state = regia_editor::viewport::ViewportPanelState::default();
    state.toolbar = false;
    panel.frame(Vec::new(), &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });
    (entity, state)
}

fn half_x(world: &World, entity: Entity) -> f32 {
    match world.get::<Collider>(entity).expect("collider").shape {
        ColliderShape::Box { half_extents } => half_extents.x,
        _ => panic!("box"),
    }
}

#[test]
fn collider_grips_stand_aside_for_game_view_and_the_terrain_tool() {
    let mut app = edit_app();
    let world = app.world_mut();
    let mut panel = PanelDriver::new();
    let (entity, mut state) = grip_panel(world, &mut panel);
    let from = on_screen(world, panel.rect, Vec3::new(1.0, 0.0, 0.0));
    let to = on_screen(world, panel.rect, Vec3::new(3.0, 0.0, 0.0));
    let draw = |world: &mut World, state: &mut _, ui: &mut egui::Ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, state);
    };
    // Game view: no gizmo, so no grips either.
    panel.frame(
        vec![key(egui::Key::G, true), key(egui::Key::G, false)],
        &mut |ui| {
            draw(world, &mut state, ui);
        },
    );
    panel.drag(from, to, 8, &mut |ui| draw(world, &mut state, ui));
    assert_eq!(half_x(world, entity), 1.0, "game view");
    panel.frame(
        vec![key(egui::Key::G, true), key(egui::Key::G, false)],
        &mut |ui| {
            draw(world, &mut state, ui);
        },
    );
    // A drag in game view box-selects, as it always has; the grips need their selection back.
    select(world, &[entity]);
    // The terrain tool owns the left button while it is on.
    let mut sculpt = regia_editor::terrain_tool::TerrainTool::default();
    sculpt.active = true;
    world.insert_resource(sculpt);
    panel.drag(from, to, 8, &mut |ui| draw(world, &mut state, ui));
    assert_eq!(half_x(world, entity), 1.0, "terrain tool");
    assert_eq!(world.resource::<UndoStack>().undo_depth(), 0);
    assert_eq!(world.resource::<Selection>().primary(), Some(entity));
    // With neither, the same drag reaches the grip: the check that the two above could have.
    world
        .resource_mut::<regia_editor::terrain_tool::TerrainTool>()
        .active = false;
    panel.drag(from, to, 8, &mut |ui| draw(world, &mut state, ui));
    assert!((half_x(world, entity) - 3.0).abs() < 0.05, "the grip");
}

#[test]
fn a_drag_cut_short_when_the_grips_go_away_is_still_one_undo_step() {
    for cut in ["tool", "selection"] {
        let mut app = edit_app();
        let world = app.world_mut();
        let mut panel = PanelDriver::new();
        let (entity, mut state) = grip_panel(world, &mut panel);
        let from = on_screen(world, panel.rect, Vec3::new(1.0, 0.0, 0.0));
        let to = on_screen(world, panel.rect, Vec3::new(2.0, 0.0, 0.0));
        let mut draw = |world: &mut World, events: Vec<egui::Event>| {
            panel.frame(events, &mut |ui| {
                regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
            });
        };
        draw(world, vec![egui::Event::PointerMoved(from)]);
        draw(world, vec![button(from, true)]);
        draw(world, vec![egui::Event::PointerMoved(to)]);
        draw(world, vec![egui::Event::PointerMoved(to)]);
        assert!(world.resource::<ColliderHandleTool>().dragging(), "{cut}");
        assert!(half_x(world, entity) > 1.5, "{cut}: the preview is on the entity");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 0, "{cut}");
        match cut {
            "tool" => {
                regia_editor::command_palette::run(world, "collider.toggle_handles");
            }
            _ => world.insert_resource(Selection::default()),
        }
        draw(world, vec![egui::Event::PointerMoved(to)]);
        assert!(!world.resource::<ColliderHandleTool>().dragging(), "{cut}");
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1, "{cut}");
        assert!(undo_world(world), "{cut}");
        assert_eq!(half_x(world, entity), 1.0, "{cut}");
    }
}

#[test]
fn ctrl_c_does_not_toggle_the_collider_tool() {
    let mut app = edit_app();
    let world = app.world_mut();
    let mut state = regia_editor::viewport::ViewportPanelState::default();
    state.toolbar = false;
    let mut panel = PanelDriver::new();
    let press = vec![key(egui::Key::C, true), key(egui::Key::C, false)];
    panel.frame(Vec::new(), &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });
    let with_held = |modifiers: egui::Modifiers| {
        let mut events = vec![egui::Event::ModifiersChanged(modifiers)];
        events.extend(press.clone());
        events
    };
    panel.frame(with_held(egui::Modifiers::CTRL), &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });
    let enabled = |world: &World| {
        world
            .get_resource::<ColliderHandleTool>()
            .is_some_and(|tool| tool.enabled)
    };
    assert!(!enabled(world), "Ctrl+C");
    panel.frame(with_held(egui::Modifiers::NONE), &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });
    assert!(enabled(world), "bare C");
}

#[test]
fn a_level_link_does_not_open_over_a_prefab_or_a_running_game() {
    let root = regia_test_utils::unique_temp_dir("log-level-guard");
    let prefab = regia_editor::content::create_asset(
        &root,
        regia_editor::content::NewAssetKind::Prefab,
        "props/crate.prefab.ron",
    )
    .expect("prefab");
    let mut app = edit_app();
    let world = app.world_mut();
    world.init_resource::<regia_editor::prefab_edit::PrefabEditMode>();
    world.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.clone()),
    });
    let open = world.resource::<OpenLevel>().clone();
    let level = root.join("assets/next.level.ron");
    level_io::save_level_file(world, &open, &level).expect("save");
    world.resource_mut::<UndoStack>().mark_saved();
    let before = world.resource::<OpenLevel>().path.clone();
    let mut content = regia_editor::content::ContentBrowserState::default();
    let queue = |world: &mut World| {
        world.insert_resource(log_links::AssetOpenRequests {
            paths: vec!["assets/next.level.ron".to_owned()],
        });
    };
    // A running game.
    let mut pie = regia_editor::pie::PieStatus::default();
    pie.mode = Some(regia_editor::pie::PlayMode::Playing);
    world.insert_resource(pie);
    queue(world);
    let status = log_links::route_asset_requests(world, &mut content).expect("refused");
    assert!(status.contains("Stop play-in-editor"), "{status}");
    assert_eq!(world.resource::<OpenLevel>().path, before);
    world.insert_resource(regia_editor::pie::PieStatus::default());
    // A prefab being edited.
    regia_editor::prefab_edit::enter_prefab_edit(world, &root.join(&prefab)).expect("enter");
    queue(world);
    let status = log_links::route_asset_requests(world, &mut content).expect("refused");
    assert!(status.contains("Close the prefab"), "{status}");
    assert_eq!(
        world
            .resource::<regia_editor::prefab_edit::PrefabEditMode>()
            .editing(),
        Some(root.join(&prefab).as_path())
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `specs/19` § 3 and § 4 keys over the viewport: `R` scale, `E` rotate and `W` translate set the gizmo mode;
/// `G` toggles the game view and `F8` possess/eject, each press flipping it (ADR-5615 audit).
#[test]
fn w_e_r_pick_the_gizmo_mode_and_g_and_f8_toggle_game_view_and_possession() {
    use regia_editor::viewport::{GizmoMode, GizmoState};
    use regia_editor::views::ViewportLayout;

    let mut app = edit_app();
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let (mut shell, root) = shell_in_project(world);
    frames_with(&mut shell, &ctx, world, 3, vec![pointer_over_viewport()]);
    let tap = |shell: &mut EditorShell, world: &mut World, k: egui::Key| {
        frames_with(
            shell,
            &ctx,
            world,
            2,
            vec![pointer_over_viewport(), key(k, true), key(k, false)],
        );
    };
    let mode = |world: &World| world.resource::<GizmoState>().mode;
    assert_eq!(mode(world), GizmoMode::Translate, "translate by default");
    tap(&mut shell, world, egui::Key::R);
    assert_eq!(mode(world), GizmoMode::Scale);
    tap(&mut shell, world, egui::Key::E);
    assert_eq!(mode(world), GizmoMode::Rotate);
    tap(&mut shell, world, egui::Key::W);
    assert_eq!(mode(world), GizmoMode::Translate);

    let layout = |world: &World| {
        let layout = world.resource::<ViewportLayout>();
        (layout.game_view, layout.possessed)
    };
    let (game_view, possessed) = layout(world);
    tap(&mut shell, world, egui::Key::G);
    assert_eq!(
        layout(world),
        (!game_view, possessed),
        "G toggles the game view only"
    );
    tap(&mut shell, world, egui::Key::G);
    tap(&mut shell, world, egui::Key::F8);
    assert_eq!(
        layout(world),
        (game_view, !possessed),
        "F8 toggles possession only"
    );
    tap(&mut shell, world, egui::Key::F8);
    assert_eq!(layout(world), (game_view, possessed));
    let _ = std::fs::remove_dir_all(&root);
}

/// ADR-7306: the dock times every panel it draws; `editor.panel_timings` reads the Outliner's and
/// the Inspector's draws after headless shell frames, and `editor.outliner_rows` lists what the
/// panel lists.
#[test]
fn the_shell_records_panel_draw_times_for_panel_timings() {
    let mut app = edit_app();
    let ctx = egui::Context::default();
    let world = app.world_mut();
    for i in 0..200 {
        world.spawn((
            regia_scene::prelude::Name::new(format!("Crate {i:03}")),
            Transform::IDENTITY,
        ));
    }
    let (mut shell, root) = shell_in_project(world);
    frames_with(&mut shell, &ctx, world, 6, Vec::new());
    let reply = call(world, "editor.panel_timings", json!({ "last": 240 })).expect("timings");
    let draws = |panel: &str| {
        reply["panels"]
            .as_array()
            .and_then(|p| p.iter().find(|p| p["panel"] == panel))
            .and_then(|p| p["draws"].as_u64())
            .unwrap_or(0)
    };
    assert!(
        draws("Outliner") >= 6,
        "the Outliner was timed each frame: {reply}"
    );
    // At least: the ring is process-wide and other tests in this binary draw shells too.
    assert!(draws("Inspector") >= 6, "the Inspector too: {reply}");
    let rows = call(world, "editor.outliner_rows", json!({ "limit": 1 })).expect("rows");
    assert!(rows["listed"].as_u64().is_some_and(|n| n >= 200), "{rows}");
    assert_eq!(
        world
            .resource::<regia_editor::outliner::OutlinerRows>()
            .order()
            .len() as u64,
        rows["listed"].as_u64().unwrap_or(0),
        "the panel and the method list the same rows"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------------------------
// Gizmo reference space, select tool, bounds pivot and offset, snap steps, surface snap, fixed-step
// rotate (ADR-7756)
// ---------------------------------------------------------------------------------------------

fn close(a: Vec3, b: Vec3, tolerance: f32) -> bool {
    (a - b).abs().max_element() <= tolerance
}

fn gizmo_state(world: &World) -> regia_editor::viewport::GizmoState {
    world.resource::<regia_editor::viewport::GizmoState>().clone()
}

/// `editor.set_gizmo { space }` sets the reference space and `editor.gizmo_drag { space }` moves
/// along that space's axes: the entity's own (local), its parent's (parent), the world's and the
/// camera's (view).
#[test]
fn each_reference_space_moves_along_its_own_axes_with_exact_numbers() {
    let mut app = edit_app();
    let world = app.world_mut();
    let turned = world
        .spawn(Transform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            ..Transform::IDENTITY
        })
        .id();
    select(world, &[turned]);

    let out = call(world, "editor.set_gizmo", json!({ "space": "local" })).expect("set");
    assert_eq!(out["space"], "local", "{out}");
    assert_eq!(
        gizmo_state(world).space,
        regia_editor::viewport::GizmoSpace::Local
    );
    assert_eq!(
        call(world, "editor.set_gizmo", json!({ "space": "sideways" })).expect_err("refused"),
        "gizmo_space_unknown"
    );

    // Local: the turned entity's own X is the world's +Y.
    call(
        world,
        "editor.gizmo_drag",
        json!({ "entities": [turned.to_bits()], "translate": [2.0, 0.0, 0.0], "space": "local" }),
    )
    .expect("local drag");
    let t = world.get::<Transform>(turned).expect("t").translation;
    assert!(close(t, Vec3::new(1.0, 4.0, 3.0), 1e-5), "{t}");

    // Parent, world and local for a child of a parent turned 90 degrees about Y. The child has its
    // own quarter turn about Z, so its local X (the parent's +Y), the parent's X and the world's X
    // (the parent's +Z) are three different axes and each space's drag lands somewhere else
    // (review sprint-v06-rev-late-k #27: with an unturned child, Parent and Local coincide). The
    // gizmo's own space is still Local here, so a drag that ignored its `space` param would move
    // the child along +Y and fail the Parent check.
    let parent = world
        .spawn(Transform {
            translation: Vec3::new(10.0, 0.0, 0.0),
            rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            ..Transform::IDENTITY
        })
        .id();
    let child = world
        .spawn((
            Transform {
                translation: Vec3::new(0.0, 1.0, 0.0),
                rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
                ..Transform::IDENTITY
            },
            ChildOf(parent),
        ))
        .id();
    select(world, &[child]);
    call(
        world,
        "editor.gizmo_drag",
        json!({ "entities": [child.to_bits()], "translate": [1.0, 0.0, 0.0], "space": "parent" }),
    )
    .expect("parent drag");
    let t = world.get::<Transform>(child).expect("t").translation;
    assert!(
        close(t, Vec3::new(1.0, 1.0, 0.0), 1e-5),
        "along the parent's X: {t}"
    );
    call(
        world,
        "editor.gizmo_drag",
        json!({ "entities": [child.to_bits()], "translate": [1.0, 0.0, 0.0], "space": "world" }),
    )
    .expect("world drag");
    let t = world.get::<Transform>(child).expect("t").translation;
    // World +X is the parent's +Z after its quarter turn about Y.
    assert!(
        close(t, Vec3::new(1.0, 1.0, 1.0), 1e-5),
        "along the world's X: {t}"
    );
    call(
        world,
        "editor.gizmo_drag",
        json!({ "entities": [child.to_bits()], "translate": [1.0, 0.0, 0.0], "space": "local" }),
    )
    .expect("local drag of the child");
    let t = world.get::<Transform>(child).expect("t").translation;
    // The child's own X is the parent's +Y after the child's quarter turn about Z (not the
    // parent's X, which would give (2, 1, 1), nor the world's, (1, 1, 2)).
    assert!(
        close(t, Vec3::new(1.0, 2.0, 1.0), 1e-5),
        "along the child's own X: {t}"
    );

    // View: along the camera's right.
    let camera = regia_editor::viewport::EditorCamera {
        target: Vec3::ZERO,
        distance: 10.0,
        yaw: 0.7,
        pitch: 0.3,
        ..Default::default()
    };
    let right = camera.right();
    world.insert_resource(camera);
    let free = world.spawn(Transform::IDENTITY).id();
    select(world, &[free]);
    call(
        world,
        "editor.gizmo_drag",
        json!({ "entities": [free.to_bits()], "translate": [1.0, 0.0, 0.0], "space": "view" }),
    )
    .expect("view drag");
    let t = world.get::<Transform>(free).expect("t").translation;
    assert!(close(t, right, 1e-5), "along the camera's right {right}: {t}");
}

/// Ctrl+` cycles the space and Q picks the select tool through the shell, from the keyboard; W
/// brings the translate gizmo back.
#[test]
fn ctrl_backtick_cycles_the_space_and_q_picks_the_select_tool() {
    use regia_editor::viewport::{GizmoMode, GizmoSpace};
    let mut app = edit_app();
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let (mut shell, root) = shell_in_project(world);
    frames_with(&mut shell, &ctx, world, 3, vec![pointer_over_viewport()]);
    let chord = |k: egui::Key, modifiers: egui::Modifiers, pressed: bool| egui::Event::Key {
        key: k,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers,
    };
    let mut seen = Vec::new();
    for _ in 0..4 {
        frames_with(
            &mut shell,
            &ctx,
            world,
            2,
            vec![
                pointer_over_viewport(),
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                chord(egui::Key::Backtick, egui::Modifiers::COMMAND, true),
                chord(egui::Key::Backtick, egui::Modifiers::COMMAND, false),
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
            ],
        );
        seen.push(gizmo_state(world).space);
    }
    assert_eq!(
        seen,
        vec![
            GizmoSpace::Local,
            GizmoSpace::Parent,
            GizmoSpace::View,
            GizmoSpace::World
        ],
        "one press, one step"
    );
    let tap = |shell: &mut EditorShell, world: &mut World, k: egui::Key| {
        frames_with(
            shell,
            &ctx,
            world,
            2,
            vec![pointer_over_viewport(), key(k, true), key(k, false)],
        );
    };
    tap(&mut shell, world, egui::Key::Q);
    assert_eq!(gizmo_state(world).mode, GizmoMode::Select);
    tap(&mut shell, world, egui::Key::W);
    assert_eq!(gizmo_state(world).mode, GizmoMode::Translate);
    let _ = std::fs::remove_dir_all(&root);
}

/// Tries each `(grab, to)` drag in turn, with `entity` selected again before each (a drag that
/// misses the gizmo is a box select, which clears the selection), until one changes the entity's
/// transform. Returns whether one did.
fn probe_drags(
    panel: &mut PanelDriver,
    world: &mut World,
    state: &mut regia_editor::viewport::ViewportPanelState,
    entity: Entity,
    drags: &[(egui::Pos2, egui::Pos2)],
) -> bool {
    let start = *world.get::<Transform>(entity).expect("transform");
    for (grab, to) in drags {
        select(world, &[entity]);
        let mut draw = |ui: &mut egui::Ui| regia_editor::viewport::viewport_panel(ui, world, None, state);
        panel.frame(Vec::new(), &mut draw);
        panel.drag(*grab, *to, 6, &mut draw);
        if *world.get::<Transform>(entity).expect("transform") != start {
            return true;
        }
    }
    false
}

/// Presses a translate gizmo's arrow that points along `direction` on screen from the entity's
/// pivot and drags it to `to`, trying points along the arrow until one moves the entity.
fn drag_gizmo_along(
    panel: &mut PanelDriver,
    world: &mut World,
    state: &mut regia_editor::viewport::ViewportPanelState,
    entity: Entity,
    direction: egui::Vec2,
    to: egui::Pos2,
) -> bool {
    let start = *world.get::<Transform>(entity).expect("transform");
    let center = on_screen(world, panel.rect, start.translation);
    let direction = direction.normalized();
    let drags: Vec<_> = (30..=90)
        .step_by(6)
        .map(|along| (center + direction * along as f32, to))
        .collect();
    probe_drags(panel, world, state, entity, &drags)
}

fn gizmo_panel(world: &mut World) -> (PanelDriver, regia_editor::viewport::ViewportPanelState) {
    let mut state = regia_editor::viewport::ViewportPanelState::default();
    state.toolbar = false;
    let mut panel = PanelDriver::new();
    panel.frame(Vec::new(), &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });
    (panel, state)
}

/// In local space the X arrow of an entity turned 45 degrees about Z points up and right on
/// screen; a drag to a point straight right of the grab moves the entity along its own X only.
#[test]
fn a_real_drag_on_the_local_x_arrow_moves_a_turned_entity_along_its_own_x() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(orbit_camera());
    let start = Transform {
        translation: Vec3::new(-3.0, 0.0, 0.0),
        rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_4),
        ..Transform::IDENTITY
    };
    let turned = spawn_mesh(world, [0.1, 0.1, 0.1], start);
    regia_scene::propagate::propagate_transforms(world);
    select(world, &[turned]);
    call(
        world,
        "editor.set_gizmo",
        json!({ "space": "local", "mode": "translate" }),
    )
    .expect("set");
    let (mut panel, mut state) = gizmo_panel(world);
    let center = on_screen(world, panel.rect, start.translation);
    let to = center + egui::vec2(140.0, 0.0);
    assert!(
        drag_gizmo_along(&mut panel, world, &mut state, turned, egui::vec2(1.0, -1.0), to),
        "the drag reached the local X arrow"
    );
    let t = world.get::<Transform>(turned).expect("t");
    let moved = t.translation - start.translation;
    let own_x = start.rotation * Vec3::X;
    assert!(moved.length() > 0.1, "{moved}");
    assert!(
        moved.normalize().dot(own_x) > 0.9999,
        "along its own X {own_x}, not the pointer: {moved}"
    );
    assert_eq!(t.rotation, start.rotation);
}

/// The rotate gizmo offers the view-axis ring (the library's `RotateView`, in `all_rotate`), and a
/// real drag on the outermost ring turns the entity about the camera's view direction.
#[test]
fn the_rotate_gizmo_has_a_view_ring_that_turns_about_the_view_direction() {
    assert!(
        transform_gizmo_egui::GizmoMode::all_rotate().contains(transform_gizmo_egui::GizmoMode::RotateView),
        "the rotate set includes the view ring"
    );
    let mut app = edit_app();
    let world = app.world_mut();
    let camera = regia_editor::viewport::EditorCamera {
        target: Vec3::ZERO,
        distance: 12.0,
        yaw: 0.6,
        pitch: 0.45,
        ..Default::default()
    };
    let forward = camera.forward();
    world.insert_resource(camera);
    let entity = spawn_mesh(world, [0.1, 0.1, 0.1], Transform::IDENTITY);
    regia_scene::propagate::propagate_transforms(world);
    select(world, &[entity]);
    call(
        world,
        "editor.set_gizmo",
        json!({ "mode": "rotate", "space": "world" }),
    )
    .expect("set");
    let (mut panel, mut state) = gizmo_panel(world);
    let center = on_screen(world, panel.rect, Vec3::ZERO);
    // From outside in: the first ring a probe grabs is the outermost, the view ring.
    let angle = 0.35f32;
    let drags: Vec<_> = (60..=130)
        .rev()
        .step_by(3)
        .map(|radius| {
            let r = radius as f32;
            (
                center + egui::vec2(angle.cos(), -angle.sin()) * r,
                center + egui::vec2((angle + 0.6).cos(), -(angle + 0.6).sin()) * r,
            )
        })
        .collect();
    assert!(
        probe_drags(&mut panel, world, &mut state, entity, &drags),
        "a probe reached the view ring"
    );
    let rotation = world.get::<Transform>(entity).expect("t").rotation;
    let (axis, angle) = rotation.to_axis_angle();
    assert!(angle > 0.05, "{angle}");
    assert!(
        axis.dot(forward).abs() > 0.999,
        "turns about the view direction {forward}, not {axis}"
    );
}

/// The select tool shows no gizmo: a drag that starts on the selection never moves it (it is a box
/// select), and `editor.set_tool` reads and sets the tool.
#[test]
fn in_the_select_tool_a_drag_on_the_selection_never_moves_it() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(orbit_camera());
    let start = at(-3.0, 0.0, 0.0);
    let entity = spawn_mesh(world, [0.5, 0.5, 0.5], start);
    regia_scene::propagate::propagate_transforms(world);
    select(world, &[entity]);
    let out = call(world, "editor.set_tool", json!({ "tool": "select" })).expect("tool");
    assert_eq!(out, json!({ "tool": "select", "gizmo": false }));
    assert_eq!(
        call(world, "editor.set_tool", json!({ "tool": "lasso" })).expect_err("refused"),
        "tool_unknown"
    );
    let (mut panel, mut state) = gizmo_panel(world);
    let depth = world.resource::<UndoStack>().undo_depth();
    let center = on_screen(world, panel.rect, start.translation);
    let to = center + egui::vec2(200.0, 40.0);
    // On the entity itself and where the X arrow would be.
    let drags: Vec<_> = std::iter::once((center, to))
        .chain(
            (30..=90)
                .step_by(6)
                .map(|along| (center + egui::vec2(along as f32, 0.0), to)),
        )
        .collect();
    assert!(
        !probe_drags(&mut panel, world, &mut state, entity, &drags),
        "nothing moves it"
    );
    assert_eq!(world.get::<Transform>(entity), Some(&start));
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth, "no edit");

    // Back on translate, the same arrow drags move it: they reached the pane all along.
    call(world, "editor.set_tool", json!({ "tool": "translate" })).expect("tool");
    assert!(probe_drags(&mut panel, world, &mut state, entity, &drags[1..]));
}

/// A box mesh whose vertices sit `offset` from the entity's origin.
fn spawn_off_center_mesh(world: &mut World, half: [f32; 3], offset: Vec3, transform: Transform) -> Entity {
    let (mut vertices, indices) = regia_render::box_mesh(half);
    for vertex in &mut vertices {
        vertex.position = (Vec3::from_array(vertex.position) + offset).to_array();
    }
    world
        .spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(LabMaterial::default()),
            ),
        ))
        .id()
}

/// Bounds center: a 90 degree turn about Y of a mesh drawn 2 m off its origin turns it about the
/// center of its box, not its origin; the median pivot (the control) turns it in place.
#[test]
fn rotating_90_degrees_about_the_bounds_center_of_an_off_center_mesh_lands_exactly() {
    let mut app = edit_app();
    let world = app.world_mut();
    let mesh = spawn_off_center_mesh(
        world,
        [0.5, 0.5, 0.5],
        Vec3::new(2.0, 0.0, 0.0),
        Transform::IDENTITY,
    );
    regia_scene::propagate::propagate_transforms(world);
    select(world, &[mesh]);
    let out = call(world, "editor.set_pivot_mode", json!({ "mode": "bounds" })).expect("mode");
    assert_eq!(out["mode"], "bounds", "{out}");
    let out = call(
        world,
        "editor.gizmo_drag",
        json!({ "rotate_deg": [0.0, 90.0, 0.0] }),
    )
    .expect("drag");
    let pivot = out["pivot"].as_array().expect("pivot").clone();
    assert!((pivot[0].as_f64().unwrap_or(0.0) - 2.0).abs() < 1e-5, "{out}");
    let t = *world.get::<Transform>(mesh).expect("t");
    assert!(
        close(t.translation, Vec3::new(2.0, 0.0, 2.0), 1e-5),
        "{}",
        t.translation
    );
    assert!(
        t.rotation
            .angle_between(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2))
            < 1e-5
    );
    assert!(undo_world(world));

    call(world, "editor.set_pivot_mode", json!({ "mode": "median" })).expect("mode");
    call(
        world,
        "editor.gizmo_drag",
        json!({ "rotate_deg": [0.0, 90.0, 0.0] }),
    )
    .expect("drag");
    let t = *world.get::<Transform>(mesh).expect("t");
    assert!(
        close(t.translation, Vec3::ZERO, 1e-6),
        "the control turns in place: {}",
        t.translation
    );
}

/// A temporary pivot offset moves the point a rotate turns about, and lapses once the selection
/// changes.
#[test]
fn a_pivot_offset_moves_the_turn_point_and_lapses_with_the_selection() {
    let mut app = edit_app();
    let world = app.world_mut();
    let a = world.spawn(Transform::IDENTITY).id();
    let b = world
        .spawn(Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)))
        .id();
    select(world, &[a]);
    let out = call(
        world,
        "editor.set_pivot_mode",
        json!({ "offset": [1.0, 0.0, 0.0] }),
    )
    .expect("offset");
    assert_eq!(out["offset"], json!([1.0, 0.0, 0.0]));
    call(
        world,
        "editor.gizmo_drag",
        json!({ "rotate_deg": [0.0, 90.0, 0.0] }),
    )
    .expect("drag");
    let t = world.get::<Transform>(a).expect("t").translation;
    assert!(
        close(t, Vec3::new(1.0, 0.0, 1.0), 1e-5),
        "about the offset point: {t}"
    );

    select(world, &[b]);
    let out = call(world, "editor.set_pivot_mode", json!({})).expect("read");
    assert_eq!(
        out["offset"],
        json!([0.0, 0.0, 0.0]),
        "another selection has no offset"
    );
    select(world, &[a]);
    let out = call(world, "editor.set_pivot_mode", json!({})).expect("read");
    assert_eq!(
        out["offset"],
        json!([0.0, 0.0, 0.0]),
        "it lapsed, it does not come back"
    );
    select(world, &[]);
    assert_eq!(
        call(
            world,
            "editor.set_pivot_mode",
            json!({ "offset": [1.0, 0.0, 0.0] })
        )
        .expect_err("refused"),
        "pivot_offset_no_selection"
    );
}

/// Holding P while the rotate gizmo shows turns it into a handle on the pivot: the drag moves the
/// pivot (no edit, no undo entry), and the next turn is about the moved point.
#[test]
fn holding_p_and_dragging_moves_the_pivot_not_the_selection() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(orbit_camera());
    let start = at(-3.0, 0.0, 0.0);
    let entity = spawn_mesh(world, [0.1, 0.1, 0.1], start);
    regia_scene::propagate::propagate_transforms(world);
    select(world, &[entity]);
    call(world, "editor.set_gizmo", json!({ "mode": "rotate" })).expect("set");
    let (mut panel, mut state) = gizmo_panel(world);
    let depth = world.resource::<UndoStack>().undo_depth();
    panel.frame(vec![key(egui::Key::P, true)], &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });
    let center = on_screen(world, panel.rect, start.translation);
    let to = on_screen(world, panel.rect, Vec3::new(-1.0, 0.0, 0.0));
    let mut moved = false;
    for along in (30..=90).step_by(6) {
        select(world, &[entity]);
        let grab = center + egui::vec2(along as f32, 0.0);
        let mut draw =
            |ui: &mut egui::Ui| regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
        panel.frame(Vec::new(), &mut draw);
        panel.drag(grab, to, 6, &mut draw);
        let out = call(world, "editor.set_pivot_mode", json!({})).expect("read");
        if out["offset"][0].as_f64().unwrap_or(0.0) > 0.1 {
            moved = true;
            break;
        }
    }
    assert!(moved, "a drag on the pivot handle moved the pivot");
    assert_eq!(
        world.get::<Transform>(entity),
        Some(&start),
        "the selection stays"
    );
    assert_eq!(world.resource::<UndoStack>().undo_depth(), depth, "no undo entry");
    panel.frame(vec![key(egui::Key::P, false)], &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });
    call(
        world,
        "editor.gizmo_drag",
        json!({ "rotate_deg": [0.0, 180.0, 0.0] }),
    )
    .expect("turn");
    let t = world.get::<Transform>(entity).expect("t").translation;
    assert!(t.x > -3.0 + 0.1, "turned about the moved pivot: {t}");
}

/// Review finding 12 of ADR-7756: the viewport lapses the temporary pivot offset when the
/// selection empties, not only when another entity is selected. Select A and offset its pivot, draw
/// with nothing selected, select A again: the offset is gone (it used to come back, because the GUI
/// cleared it only after the early returns that fire with no selection).
#[test]
fn a_pivot_offset_lapses_when_the_viewport_draws_an_empty_selection() {
    let mut app = edit_app();
    let world = app.world_mut();
    world.insert_resource(orbit_camera());
    let entity = spawn_mesh(world, [0.1, 0.1, 0.1], at(-3.0, 0.0, 0.0));
    regia_scene::propagate::propagate_transforms(world);
    select(world, &[entity]);
    call(world, "editor.set_gizmo", json!({ "mode": "rotate" })).expect("set");
    call(
        world,
        "editor.set_pivot_mode",
        json!({ "offset": [1.0, 0.0, 0.0] }),
    )
    .expect("offset");
    let (mut panel, mut state) = gizmo_panel(world);
    panel.frame(Vec::new(), &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });
    let out = call(world, "editor.set_pivot_mode", json!({})).expect("read");
    assert_eq!(
        out["offset"],
        json!([1.0, 0.0, 0.0]),
        "a frame with the same selection keeps it"
    );
    select(world, &[]);
    panel.frame(Vec::new(), &mut |ui| {
        regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    });
    select(world, &[entity]);
    let out = call(world, "editor.set_pivot_mode", json!({})).expect("read");
    assert_eq!(
        out["offset"],
        json!([0.0, 0.0, 0.0]),
        "deselecting lapsed it; reselecting does not bring it back"
    );
}

/// The fixed-step rotate turns the selection by the angle step about the world up, one undo step a
/// press, from the agent and from the U key.
#[test]
fn the_rotate_step_turns_by_the_angle_step_one_undo_step_each() {
    let mut app = edit_app();
    let world = app.world_mut();
    let entity = world.spawn(Transform::IDENTITY).id();
    select(world, &[entity]);
    call(world, "editor.set_snap", json!({ "angle_deg": 30.0 })).expect("step");
    let depth = world.resource::<UndoStack>().undo_depth();
    let out = call(world, "editor.rotate_step", json!({})).expect("step");
    assert_eq!(out["degrees"], json!(30.0));
    call(world, "editor.rotate_step", json!({})).expect("step");
    let rotation = world.get::<Transform>(entity).expect("t").rotation;
    assert!(rotation.angle_between(Quat::from_rotation_y(60f32.to_radians())) < 1e-5);
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 2,
        "two presses, two steps"
    );
    assert!(undo_world(world));
    let rotation = world.get::<Transform>(entity).expect("t").rotation;
    assert!(rotation.angle_between(Quat::from_rotation_y(30f32.to_radians())) < 1e-5);
    call(world, "editor.rotate_step", json!({ "back": true })).expect("back");
    let rotation = world.get::<Transform>(entity).expect("t").rotation;
    assert!(rotation.angle_between(Quat::IDENTITY) < 1e-5);

    // The U key over the viewport does the same.
    let ctx = egui::Context::default();
    let (mut shell, root) = shell_in_project(world);
    frames_with(&mut shell, &ctx, world, 3, vec![pointer_over_viewport()]);
    frames_with(
        &mut shell,
        &ctx,
        world,
        2,
        vec![
            pointer_over_viewport(),
            key(egui::Key::U, true),
            key(egui::Key::U, false),
        ],
    );
    let rotation = world.get::<Transform>(entity).expect("t").rotation;
    assert!(
        rotation.angle_between(Quat::from_rotation_y(30f32.to_radians())) < 1e-5,
        "U turned it by the step"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The snap steps persist per project: `editor.set_snap` writes `.regia/viewport_snap.ron`, and a
/// fresh editor on the same project reads them back; a bad step is refused.
#[test]
fn snap_steps_are_saved_with_the_project_and_read_back() {
    let mut app = edit_app();
    let world = app.world_mut();
    let (_shell, root) = shell_in_project(world);
    let out = call(
        world,
        "editor.set_snap",
        json!({ "distance": 0.125, "angle_deg": 45.0, "scale": 0.1 }),
    )
    .expect("set");
    let saved = out["saved"].as_str().expect("saved path").to_owned();
    assert!(saved.ends_with("viewport_snap.ron"), "{out}");
    assert!(root.join(regia_editor::viewport_ops::SNAP_STEPS_FILE).is_file());
    assert!(call(world, "editor.set_snap", json!({ "distance": -1.0 })).is_err());

    let mut other = edit_app();
    let fresh = other.world_mut();
    fresh.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    let out = call(fresh, "editor.set_snap", json!({})).expect("read");
    assert_eq!(
        (
            out["distance"].clone(),
            out["angle_deg"].clone(),
            out["scale"].clone()
        ),
        (json!(0.125), json!(45.0), json!(0.1f32)),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The toolbar's step menus: opening the Grid menu and picking a preset sets the step and saves it.
#[test]
fn picking_a_grid_step_in_the_toolbar_sets_and_saves_it() {
    let mut app = edit_app();
    let world = app.world_mut();
    let (_shell, root) = shell_in_project(world);
    let mut state = regia_editor::viewport::ViewportPanelState::default();
    let mut panel = PanelDriver::new();
    let mut draw = |ui: &mut egui::Ui| regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
    panel.frame(Vec::new(), &mut draw);
    let menu = panel
        .text_at(&|t| t.starts_with("Grid: "))
        .expect("the Grid step menu");
    panel.click(menu, &mut draw);
    let preset = panel.text_at(&|t| t == "0.25 m").expect("the 0.25 m preset");
    panel.click(preset, &mut draw);
    drop(draw);
    assert_eq!(gizmo_state(world).snap_distance, 0.25);
    let text =
        std::fs::read_to_string(root.join(regia_editor::viewport_ops::SNAP_STEPS_FILE)).expect("saved");
    assert!(text.contains("0.25"), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A slope for the surface snap drags: a wide thin box turned about Z, seen from above.
fn slope_scene(world: &mut World) -> Vec3 {
    let rotation = Quat::from_rotation_z(0.4);
    spawn_mesh(
        world,
        [20.0, 0.25, 20.0],
        Transform {
            rotation,
            ..Transform::IDENTITY
        },
    );
    world.insert_resource(regia_editor::viewport::EditorCamera {
        target: Vec3::ZERO,
        distance: 14.0,
        yaw: 0.0,
        pitch: 0.6,
        ..Default::default()
    });
    rotation * Vec3::Y
}

/// Surface snap: a translate drag over a slope ends with the pivot on the slope within a
/// millimeter, and with align on the up axis matches the normal; one drag is one undo step.
#[test]
fn a_surface_snap_drag_over_a_slope_ends_on_it_and_align_matches_the_normal() {
    let mut app = edit_app();
    let world = app.world_mut();
    let normal = slope_scene(world);
    let start = at(-3.0, 4.0, 0.0);
    let cube = spawn_mesh(world, [0.2, 0.2, 0.2], start);
    regia_scene::propagate::propagate_transforms(world);
    select(world, &[cube]);
    let out = call(
        world,
        "editor.placement_options",
        json!({ "surface_snap": true, "surface_align": true }),
    )
    .expect("options");
    assert_eq!(out["surface_snap"], true, "{out}");
    let (mut panel, mut state) = gizmo_panel(world);
    world.resource_mut::<UndoStack>().seal();
    let depth = world.resource::<UndoStack>().undo_depth();
    // The top face passes 0.25 along the normal from the origin: aim at its point above x = 2.
    let aim = Vec3::new(2.0, (0.25 + 2.0 * (-normal.x)) / normal.y, 0.0);
    assert!((aim.dot(normal) - 0.25).abs() < 1e-5);
    let to = on_screen(world, panel.rect, aim);
    let x = egui::vec2(1.0, 0.0);
    assert!(
        drag_gizmo_along(&mut panel, world, &mut state, cube, x, to),
        "the drag reached the gizmo"
    );
    let t = *world.get::<Transform>(cube).expect("t");
    assert!(
        (t.translation.dot(normal) - 0.25).abs() < 1.0e-3,
        "on the slope: {} (plane distance {})",
        t.translation,
        t.translation.dot(normal) - 0.25
    );
    assert!(
        close(t.translation, aim, 0.05),
        "under the pointer: {} vs {aim}",
        t.translation
    );
    assert!(
        (t.rotation * Vec3::Y - normal).length() < 1.0e-3,
        "up {} matches the normal {normal}",
        t.rotation * Vec3::Y
    );
    world.resource_mut::<UndoStack>().seal();
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one drag, one step"
    );
    assert!(undo_world(world));
    assert_eq!(world.get::<Transform>(cube), Some(&start));

    // An offset lifts it along the normal; without align the rotation stays.
    call(
        world,
        "editor.placement_options",
        json!({ "surface_align": false, "surface_offset": 0.5 }),
    )
    .expect("options");
    assert!(
        drag_gizmo_along(&mut panel, world, &mut state, cube, x, to),
        "the drag reached the gizmo"
    );
    let t = *world.get::<Transform>(cube).expect("t");
    assert!(
        (t.translation.dot(normal) - 0.75).abs() < 1.0e-3,
        "{}",
        t.translation
    );
    assert_eq!(t.rotation, start.rotation);
}

/// The keyboard path to the step menus: Tab reaches the Grid menu, Enter opens it, Tab moves into
/// its presets and Enter picks one, with no pointer event.
#[test]
fn the_grid_step_menu_works_from_the_keyboard() {
    let mut app = edit_app();
    let world = app.world_mut();
    let (_shell, root) = shell_in_project(world);
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut state = regia_editor::viewport::ViewportPanelState::default();
    // The label of the focused widget after one frame with `events`.
    let mut frame = |world: &mut World, events: Vec<egui::Event>| -> String {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1600.0, 800.0),
            )),
            time: Some(ctx.input(|i| i.time) + 1.0 / 60.0),
            predicted_dt: 1.0 / 60.0,
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            regia_editor::viewport::viewport_panel(ui, world, None, &mut state);
        });
        output.textures_delta.clear();
        let Some(update) = output.platform_output.accesskit_update else {
            return String::new();
        };
        update
            .nodes
            .iter()
            .find(|(id, _)| *id == update.focus)
            .and_then(|(_, node)| node.label().map(str::to_owned))
            .unwrap_or_default()
    };
    frame(world, Vec::new());
    let tab = || vec![key(egui::Key::Tab, true), key(egui::Key::Tab, false)];
    let enter = || vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)];
    let mut seen = Vec::new();
    for _ in 0..40 {
        seen.push(frame(world, tab()));
        if seen.last().is_some_and(|f| f == "Grid snap step") {
            break;
        }
    }
    assert_eq!(
        seen.last().map(String::as_str),
        Some("Grid snap step"),
        "Tab reaches the Grid menu: {seen:?}"
    );
    let mut focused = String::new();
    frame(world, enter());
    for _ in 0..20 {
        focused = frame(world, tab());
        if focused == "0.25 m" {
            break;
        }
    }
    assert_eq!(focused, "0.25 m", "Tab moves through the presets");
    frame(world, enter());
    frame(world, Vec::new());
    assert_eq!(gizmo_state(world).snap_distance, 0.25);
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// Pilot, look through, camera preview, guides (ADR-7768, `book/agents/viewport-tools.md`)
// ---------------------------------------------------------------------------

fn spawn_light(world: &mut World, transform: Transform) -> Entity {
    world
        .spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            regia_scene::prelude::Name::new("Key light"),
            regia_render::PointLight::default(),
        ))
        .id()
}

fn spawn_scene_camera(world: &mut World, name: &str, transform: Transform, fov_deg: f32) -> Entity {
    world
        .spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            regia_scene::prelude::Name::new(name.to_owned()),
            regia_render::Camera {
                fov_y: fov_deg.to_radians(),
                ..Default::default()
            },
        ))
        .id()
}

fn vec3_of(value: &Value) -> Vec3 {
    let n = |i: usize| value[i].as_f64().unwrap_or(f64::NAN) as f32;
    Vec3::new(n(0), n(1), n(2))
}

/// The brief's acceptance through the agent: pilot a light, fly 10 fixed steps, eject: the light
/// moved by exactly the camera's motion, and one undo restores it.
/// Review sprint-w12-rev-g1 #1: egui's second pass of a frame repeats a held fly key; the piloted
/// entity moves once per painted frame and catches up with the camera on the next frame.
#[test]
fn a_second_egui_pass_does_not_move_the_piloted_entity_twice() {
    use regia_editor::viewport_pilot::follow_in_frame;
    let mut app = edit_app();
    let world = app.world_mut();
    let start = Transform {
        translation: Vec3::new(3.0, 4.0, -2.0),
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };
    let light = spawn_light(world, start);
    call(world, "editor.pilot", json!({ "entity": light.to_bits() })).expect("pilot");
    let mut camera = world.resource::<regia_editor::viewport::EditorCamera>().clone();
    let step = Vec3::new(1.0, 0.0, 0.0);
    // Pass one of frame 7: the held key flew the camera one step.
    camera.target += step;
    assert!(follow_in_frame(world, &mut camera, 7));
    let once = world.get::<Transform>(light).expect("transform").translation;
    assert!((once - start.translation - step).length() < 1e-4, "{once:?}");
    // Pass two of frame 7 flies the camera again: the light stays.
    camera.target += step;
    assert!(follow_in_frame(world, &mut camera, 7));
    let again = world.get::<Transform>(light).expect("transform").translation;
    assert!(
        (again - once).length() < 1e-6,
        "one move per painted frame: {again:?}"
    );
    // Frame 8 carries the camera's motion since the last step.
    assert!(follow_in_frame(world, &mut camera, 8));
    let caught = world.get::<Transform>(light).expect("transform").translation;
    assert!(
        (caught - start.translation - step * 2.0).length() < 1e-4,
        "{caught:?}"
    );
}

#[test]
fn pilot_flies_a_light_by_the_camera_motion_and_one_undo_restores_it() {
    let mut app = edit_app();
    let world = app.world_mut();
    let start = Transform {
        translation: Vec3::new(3.0, 4.0, -2.0),
        rotation: Quat::from_rotation_y(0.4),
        scale: Vec3::ONE,
    };
    let light = spawn_light(world, start);
    let depth = world.resource::<UndoStack>().undo_depth();
    let begun = call(world, "editor.pilot", json!({ "entity": light.to_bits() })).expect("pilot");
    assert_eq!(begun["mode"], "pilot");
    assert_eq!(begun["name"], "Key light");
    let eye_start = vec3_of(&begun["eye"]);
    assert!(
        (eye_start - start.translation).length() < 1e-3,
        "the camera sits at the light: {eye_start:?}"
    );
    let flown = call(
        world,
        "editor.pilot",
        json!({ "fly": { "forward": 1.0, "right": 0.5, "up": 0.25, "seconds": 0.05, "steps": 10 } }),
    )
    .expect("fly");
    let eye_end = vec3_of(&flown["eye"]);
    let ended = call(world, "editor.pilot", json!({ "entity": null })).expect("eject");
    assert_eq!(ended["mode"], Value::Null);
    let after = *world.get::<Transform>(light).expect("transform");
    let camera_motion = eye_end - eye_start;
    assert!(camera_motion.length() > 1.0, "the camera flew: {camera_motion:?}");
    assert!(
        (after.translation - start.translation - camera_motion).length() < 1e-3,
        "light moved {:?}, camera {camera_motion:?}",
        after.translation - start.translation
    );
    assert_eq!(after.rotation, start.rotation, "flying does not turn it");
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth + 1,
        "one undo step per session"
    );
    assert!(undo_world(world));
    assert_eq!(*world.get::<Transform>(light).expect("transform"), start);
    assert_eq!(
        call(world, "editor.pilot", json!({ "fly": { "forward": 1.0 } })),
        Err("pilot_not_active".to_owned())
    );
    assert_eq!(
        call(world, "editor.pilot", json!({ "entity": 999_999_999u64 })),
        Err("pilot_not_found".to_owned())
    );
}

/// Ctrl+Shift+L over the viewport pilots the selection and shows the banner; again ejects. The
/// palette commands are the same keyboard path.
#[test]
fn ctrl_shift_l_pilots_the_selection_and_again_ejects() {
    let mut app = edit_app();
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let (mut shell, root) = shell_in_project(world);
    let light = spawn_light(world, at(1.0, 2.0, 3.0));
    select(world, &[light]);
    frames_with(&mut shell, &ctx, world, 3, vec![pointer_over_viewport()]);
    let mods = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
    let chord = |pressed: bool| egui::Event::Key {
        key: egui::Key::L,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: mods,
    };
    let tap = |shell: &mut EditorShell, world: &mut World| {
        frames_with(
            shell,
            &ctx,
            world,
            2,
            vec![
                pointer_over_viewport(),
                egui::Event::ModifiersChanged(mods),
                chord(true),
                chord(false),
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
            ],
        );
    };
    let piloting = |world: &mut World| {
        call(world, "editor.cameras", json!({})).expect("cameras")["session"]["mode"].clone()
    };
    let before = *world.get::<Transform>(light).expect("transform");
    let depth = world.resource::<UndoStack>().undo_depth();
    tap(&mut shell, world);
    assert_eq!(piloting(world), "pilot", "Ctrl+Shift+L pilots the selection");
    let still = |world: &World| {
        let now = *world.get::<Transform>(light).expect("transform");
        (now.translation - before.translation).length() < 1e-4
            && now.rotation.angle_between(before.rotation) < 1e-4
    };
    assert!(still(world), "starting a pilot does not move the entity");
    frames_with(&mut shell, &ctx, world, 3, vec![pointer_over_viewport()]);
    assert!(still(world), "frames without camera input do not move it");
    tap(&mut shell, world);
    assert_eq!(piloting(world), Value::Null, "again ejects");
    assert!(still(world));
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth,
        "no move, no undo step"
    );
    // The palette route (keyboard: Ctrl+Shift+P, type, Enter) runs the same commands.
    let message = regia_editor::command_palette::run(world, "view.pilot").expect("view.pilot is a command");
    assert_eq!(message, "Piloting Key light");
    assert_eq!(piloting(world), "pilot");
    let message = regia_editor::command_palette::run(world, "view.eject").expect("view.eject is a command");
    assert_eq!(message, "Ejected from Key light");
    let _ = std::fs::remove_dir_all(&root);
}

/// The Camera menu's list and look-through: every scene camera, searchable; looking through one
/// takes its pose and field of view without editing it; `null` returns to the editor camera.
#[test]
fn cameras_list_and_look_through_returns_to_the_editor_camera() {
    let mut app = edit_app();
    let world = app.world_mut();
    let wide = spawn_scene_camera(world, "Wide shot", at(0.0, 2.0, 12.0), 50.0);
    let close = spawn_scene_camera(world, "Close up", at(1.0, 1.5, 3.0), 25.0);
    let listed = call(world, "editor.cameras", json!({})).expect("cameras");
    let names: Vec<&str> = listed["cameras"]
        .as_array()
        .expect("rows")
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert_eq!(names, ["Close up", "Wide shot"], "sorted by name");
    let found = call(world, "editor.cameras", json!({ "query": "WIDE" })).expect("search");
    assert_eq!(found["cameras"][0]["entity"].as_u64(), Some(wide.to_bits()));
    assert_eq!(found["cameras"].as_array().map(Vec::len), Some(1));
    let before = world.resource::<regia_editor::viewport::EditorCamera>().clone();
    let depth = world.resource::<UndoStack>().undo_depth();
    let viewing = call(world, "editor.look_through", json!({ "entity": close.to_bits() })).expect("look");
    assert_eq!(viewing["mode"], "look_through");
    assert!((vec3_of(&viewing["eye"]) - Vec3::new(1.0, 1.5, 3.0)).length() < 1e-3);
    assert!(
        (viewing["fov_deg"].as_f64().unwrap_or(0.0) - 25.0).abs() < 1e-3,
        "exact framing"
    );
    assert_eq!(
        world.resource::<UndoStack>().undo_depth(),
        depth,
        "nothing edited"
    );
    let back = call(world, "editor.look_through", json!({ "entity": null })).expect("editor camera");
    assert_eq!(back["mode"], Value::Null);
    assert_eq!(*world.resource::<regia_editor::viewport::EditorCamera>(), before);
    let light = spawn_light(world, at(0.0, 0.0, 0.0));
    assert_eq!(
        call(world, "editor.look_through", json!({ "entity": light.to_bits() })),
        Err("look_through_not_camera".to_owned())
    );
    // The palette walks the cameras (the keyboard route of the menu's list).
    let message = regia_editor::command_palette::run(world, "view.look_through_next").expect("command");
    assert_eq!(message, "Looking through Close up");
    let message = regia_editor::command_palette::run(world, "view.look_through_next").expect("command");
    assert_eq!(message, "Looking through Wide shot");
    let message = regia_editor::command_palette::run(world, "view.editor_camera").expect("command");
    assert_eq!(message, "Back to the editor camera");
}

/// The picture-in-picture preview is requested only while a scene camera is selected: shell
/// frames with nothing (or a light) selected make no request.
#[test]
fn the_camera_preview_renders_only_while_a_camera_is_selected() {
    let mut app = edit_app();
    let ctx = egui::Context::default();
    let world = app.world_mut();
    let (mut shell, root) = shell_in_project(world);
    let camera = spawn_scene_camera(world, "Wide shot", at(0.0, 2.0, 12.0), 50.0);
    let light = spawn_light(world, at(0.0, 0.0, 0.0));
    let requests = |world: &mut World| {
        call(world, "editor.camera_preview", json!({})).expect("preview")["requests"]
            .as_u64()
            .unwrap_or(u64::MAX)
    };
    frames_with(&mut shell, &ctx, world, 4, vec![pointer_over_viewport()]);
    assert_eq!(requests(world), 0, "nothing selected: no preview render");
    select(world, &[light]);
    frames_with(&mut shell, &ctx, world, 4, vec![pointer_over_viewport()]);
    assert_eq!(requests(world), 0, "a light: no preview render");
    select(world, &[camera]);
    frames_with(&mut shell, &ctx, world, 4, vec![pointer_over_viewport()]);
    let shown = requests(world);
    assert!(
        (1..=4).contains(&shown),
        "one request per frame while a camera is selected: {shown}"
    );
    let off = call(world, "editor.camera_preview", json!({ "size": "off" })).expect("off");
    assert_eq!(off["camera"], Value::Null);
    frames_with(&mut shell, &ctx, world, 3, vec![pointer_over_viewport()]);
    assert_eq!(requests(world), shown, "off: no more requests");
    assert_eq!(
        call(world, "editor.camera_preview", json!({ "size": "huge" })),
        Err("camera_preview_bad_size".to_owned())
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `editor.set_viewport { guides }` sets the composition guides; keys left out keep their value.
#[test]
fn set_viewport_sets_the_composition_guides() {
    let mut app = edit_app();
    let world = app.world_mut();
    let out = call(
        world,
        "editor.set_viewport",
        json!({ "guides": { "thirds": true, "letterbox": "2.39:1" } }),
    )
    .expect("guides");
    assert_eq!(out["guides"]["thirds"], true);
    assert_eq!(out["guides"]["center"], false);
    assert!((out["guides"]["letterbox"].as_f64().unwrap_or(0.0) - 2.39).abs() < 1e-4);
    let out = call(
        world,
        "editor.set_viewport",
        json!({ "guides": { "safe_areas": true, "letterbox": null } }),
    )
    .expect("guides");
    assert_eq!(out["guides"]["thirds"], true, "kept");
    assert_eq!(out["guides"]["safe_areas"], true);
    assert_eq!(out["guides"]["letterbox"], Value::Null);
    assert!(
        call(
            world,
            "editor.set_viewport",
            json!({ "guides": { "letterbox": "cinema" } })
        )
        .is_err()
    );
}

/// File ▸ Screenshot… is a command (the palette's keyboard route) that opens the dialog and is in
/// the menu tree; `editor.screenshot` refuses bad sizes before it renders.
#[test]
fn the_screenshot_command_opens_the_dialog_and_bad_requests_are_refused() {
    let mut app = edit_app();
    let world = app.world_mut();
    let message = regia_editor::command_palette::run(world, "file.screenshot").expect("command");
    assert_eq!(message, "Screenshot…");
    let dialog = call(world, "editor.menu_list", json!({})).expect("menus");
    assert!(
        dialog.to_string().contains("File/Screenshot…"),
        "the menu tree lists File ▸ Screenshot…"
    );
    assert_eq!(
        call(world, "editor.screenshot", json!({ "multiplier": 9 })),
        Err("screenshot_bad_multiplier".to_owned())
    );
    assert_eq!(
        call(
            world,
            "editor.screenshot",
            json!({ "width": 20000, "height": 100 })
        ),
        Err("screenshot_bad_size".to_owned())
    );
    assert!(
        call(
            world,
            "editor.screenshot",
            json!({ "multiplier": 2, "width": 10, "height": 10 })
        )
        .is_err()
    );
    assert_eq!(
        call(world, "editor.screenshot", json!({ "size": [16, 99_999] })),
        Err("screenshot_bad_size".to_owned()),
        "size is width and height"
    );
}
