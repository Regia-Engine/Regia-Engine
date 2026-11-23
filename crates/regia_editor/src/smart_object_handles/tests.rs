// SPDX-License-Identifier: MPL-2.0
//! Slot handle tests: names, the parent-space math, which entity shows them, and the agent drag (one
//! undo step).

use regia_scene::GlobalTransform;
use regia_scene::prelude::Transform;
use serde_json::json;

use super::*;
use crate::smart_object_def_panel::{default_slot, open_def};

const REL: &str = "assets/props/bench.smartobject.ron";

fn handler(name: &str) -> Handler {
    methods()
        .into_iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| h)
        .expect("registered")
}

fn bench() -> SmartObjectDef {
    let mut def = SmartObjectDef::new();
    let mut left = default_slot("left");
    left.offset.translation = (-0.4, 0.0, 0.0);
    left.entrances.push(regia_ai::smart_object_def::EntranceDef {
        offset: OffsetDef {
            translation: (0.0, 0.0, 1.0),
            ..OffsetDef::default()
        },
        kind: regia_ai::smart_object_def::EntranceKindDef::Both,
        rank: 0,
    });
    let mut right = default_slot("right");
    right.offset.translation = (0.4, 0.0, 0.0);
    def.slots = vec![left, right];
    def
}

/// A turned, scaled, moved object.
fn object_transform() -> Transform {
    Transform {
        translation: Vec3::new(3.0, 0.5, -2.0),
        rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        scale: Vec3::splat(2.0),
    }
}

fn world_with_open_bench() -> (World, Entity) {
    let root = regia_test_utils::unique_temp_dir("smart-object-handles");
    let file = root.join(REL);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&file, bench().to_ron().expect("ron")).expect("write");
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject { root: Some(root) });
    world.insert_resource(UndoStack::new());
    open_def(&mut world, REL).expect("open");
    let t = object_transform();
    let entity = world.spawn((t, GlobalTransform(t.compute_affine()))).id();
    (world, entity)
}

fn def_of(world: &World) -> SmartObjectDef {
    world
        .resource::<SmartObjectDefPanel>()
        .doc()
        .expect("doc")
        .def
        .clone()
}

fn close(a: (f32, f32, f32), b: Vec3) -> bool {
    (Vec3::new(a.0, a.1, a.2) - b).length() < 1e-4
}

#[test]
fn handle_names_round_trip() {
    for kind in [
        SlotHandleKind::Slot { slot: 0 },
        SlotHandleKind::SlotYaw { slot: 12 },
        SlotHandleKind::Entrance { slot: 3, entrance: 7 },
        SlotHandleKind::EntranceYaw { slot: 0, entrance: 1 },
    ] {
        assert_eq!(SlotHandleKind::parse(&kind.name()), Some(kind));
    }
    for bad in [
        "",
        "slot_",
        "slot_x",
        "entrance_1",
        "entrance_1_",
        "slot_-1",
        "SLOT_0",
        "slot_0_turn",
    ] {
        assert_eq!(SlotHandleKind::parse(bad), None, "{bad}");
    }
}

#[test]
fn handles_sit_at_the_slot_poses_points_first() {
    let def = bench();
    let object = object_transform().compute_affine();
    let grips = handles_for(&def, object);
    // Two slots, one entrance: three points, then three dials.
    assert_eq!(grips.len(), 6);
    assert!(
        grips[..3]
            .iter()
            .all(|g| matches!(g.handle, Handle::Point { .. }))
    );
    assert!(grips[3..].iter().all(|g| g.kind.is_yaw()));
    // The left slot (-0.4, 0, 0) in an object turned +90° about Y, scaled 2, at (3, 0.5, -2):
    // local -X turns onto world +Z, times 2.
    let left = grips[0].handle.anchor();
    assert!((left - Vec3::new(3.0, 0.5, -1.2)).length() < 1e-4, "{left}");
    // Its entrance one metre along the slot's +Z: world +X... times 2 from (3, .5, -1.2).
    let entrance = grips[1].handle.anchor();
    assert!(
        (entrance - Vec3::new(5.0, 0.5, -1.2)).length() < 1e-4,
        "{entrance}"
    );
}

/// Acceptance (2), the math: a world delta on a slot handle changes the offset by that delta in the
/// object's local space (rotation and scale undone); an entrance by its slot's.
#[test]
fn a_moved_handle_changes_the_offset_by_the_local_delta() {
    let def = bench();
    let object = object_transform().compute_affine();
    let delta = Vec3::new(0.6, -0.2, 1.0);
    let start = handles_for(&def, object)[0].handle.anchor();
    let next = moved(&def, object, SlotHandleKind::Slot { slot: 0 }, start + delta).expect("moved");
    let local = object.inverse().transform_vector3(delta);
    assert!(close(
        next.slots[0].offset.translation,
        Vec3::new(-0.4, 0.0, 0.0) + local
    ));
    assert_eq!(next.slots[1], def.slots[1], "the other slot stays");
    assert_eq!(next.slots[0].offset.rotation, def.slots[0].offset.rotation);

    let at = handles_for(&def, object)[1].handle.anchor();
    let next = moved(
        &def,
        object,
        SlotHandleKind::Entrance { slot: 0, entrance: 0 },
        at + delta,
    )
    .expect("moved");
    let slot = slot_frame(&def, object, 0).expect("frame");
    let local = slot.inverse().transform_vector3(delta);
    assert!(close(
        next.slots[0].entrances[0].offset.translation,
        Vec3::new(0.0, 0.0, 1.0) + local
    ));

    assert_eq!(moved(&def, object, SlotHandleKind::SlotYaw { slot: 0 }, at), None);
    assert_eq!(moved(&def, object, SlotHandleKind::Slot { slot: 9 }, at), None);
    assert_eq!(
        moved(&def, object, SlotHandleKind::Slot { slot: 0 }, Vec3::NAN),
        None
    );
}

#[test]
fn a_turned_dial_turns_about_the_own_up_axis() {
    let def = bench();
    let next = turned(
        &def,
        SlotHandleKind::SlotYaw { slot: 1 },
        std::f32::consts::FRAC_PI_2,
    )
    .expect("turned");
    let (x, y, z, w) = next.slots[1].offset.rotation;
    let q = Quat::from_xyzw(x, y, z, w);
    assert!(q.angle_between(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2)) < 1e-4);
    assert_eq!(next.slots[1].offset.translation, def.slots[1].offset.translation);
    assert_eq!(turned(&def, SlotHandleKind::Slot { slot: 1 }, 1.0), None);
}

#[test]
fn handles_show_on_the_preview_entity_only_while_on() {
    let (mut world, entity) = world_with_open_bench();
    assert!(
        !handles_show(&world, entity),
        "not the preview, names no definition"
    );
    world.resource_mut::<SmartObjectDefPanel>().preview = Some(entity);
    assert!(handles_show(&world, entity));
    world.resource_mut::<SmartObjectDefPanel>().handles_off = true;
    assert!(!handles_show(&world, entity), "the switch turns them off");
    world.resource_mut::<SmartObjectDefPanel>().handles_off = false;
    world
        .entity_mut(entity)
        .insert(regia_scene::prelude::FeedOwned { key: "feed".into() });
    assert!(!handles_show(&world, entity), "never on DCC feed data");
}

/// Acceptance (2) through the drag code: the agent drag moves slot 0 by a known world delta; the
/// offset changes by that delta in local space, the whole drag is one undo step, and undo restores it.
#[test]
fn the_agent_drag_is_one_undo_step_with_the_local_delta() {
    let (mut world, entity) = world_with_open_bench();
    let list = handler("editor.smart_object_slot_handles");
    let drag = handler("editor.smart_object_slot_handle_drag");
    let refused = list(&mut world, &json!({ "entity": entity.to_bits() })).expect_err("not shown");
    assert_eq!(
        refused
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str),
        Some("smart_object_handles_not_shown"),
        "{refused:?}"
    );
    let out = list(
        &mut world,
        &json!({ "preview": entity.to_bits(), "entity": entity.to_bits() }),
    )
    .expect("list");
    assert_eq!(out["preview"], entity.to_bits());
    let handles = out["handles"].as_array().expect("handles");
    assert_eq!(handles.len(), 6);
    assert_eq!(handles[0]["name"], "slot_0");
    let start = Vec3::new(
        handles[0]["position"][0].as_f64().expect("x") as f32,
        handles[0]["position"][1].as_f64().expect("y") as f32,
        handles[0]["position"][2].as_f64().expect("z") as f32,
    );
    let delta = Vec3::new(1.0, 0.25, -0.5);
    let to = start + delta;
    let out = drag(
        &mut world,
        &json!({ "entity": entity.to_bits(), "handle": "slot_0", "to": to.to_array(), "steps": 8 }),
    )
    .expect("drag");
    assert_eq!(out["changed"], true);
    assert_eq!(out["undo_depth"], 1, "one undo step for the whole drag");
    let local = object_transform()
        .compute_affine()
        .inverse()
        .transform_vector3(delta);
    assert!(close(
        def_of(&world).slots[0].offset.translation,
        Vec3::new(-0.4, 0.0, 0.0) + local
    ));
    let out = drag(
        &mut world,
        &json!({ "entity": entity.to_bits(), "handle": "slot_1_yaw", "angle": 0.5 }),
    )
    .expect("turn");
    assert_eq!(out["undo_depth"], 2);
    crate::command::undo_world(&mut world);
    crate::command::undo_world(&mut world);
    assert_eq!(def_of(&world), bench(), "undo restores the definition");
    let e = drag(
        &mut world,
        &json!({ "entity": entity.to_bits(), "handle": "slot_7", "to": [0, 0, 0] }),
    )
    .expect_err("unknown");
    assert_eq!(
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str),
        Some("smart_object_handle_unknown")
    );
    let e = drag(
        &mut world,
        &json!({ "entity": entity.to_bits(), "handle": "slot_0_yaw" }),
    )
    .expect_err("no angle");
    assert!(e.message.contains("angle"), "{}", e.message);
}
