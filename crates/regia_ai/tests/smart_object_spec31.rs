// SPDX-License-Identifier: MPL-2.0
//! Spec 31 acceptance items the unit tests in `smart_object.rs` did not pin (conformance audit,
//! ADR-5632): the index facing of a rotated slot through a middle transform node, the
//! `effective_enabled` helper on its own, and a level of 1,000 slots whose index matches a brute
//! force scan. Fixed worlds, no clocks.

use regia_ai::{
    SmartObject, SmartObjectIndex, SmartObjectPlugin, SmartObjectSlot, claim_slot, effective_enabled,
    smart_object_ancestor,
};
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Quat, Vec3};
use regia_ecs::prelude::ChildOf;
use regia_scene::{GlobalTransform, Transform};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SmartObjectPlugin));
    app
}

fn close(a: Vec3, b: Vec3) -> bool {
    (a - b).length() < 1e-5
}

/// Spec 31 § 1 and § 6: the slot's world pose composes every local transform up the `ChildOf`
/// chain, its facing is its local −Z in world space, and the owning object resolves through a
/// middle transform node that is not itself a smart object.
#[test]
fn a_rotated_object_turns_its_slot_facing_through_a_middle_node() {
    let mut app = app();
    let object = app
        .world_mut()
        .spawn((
            SmartObject::default(),
            Transform {
                translation: Vec3::new(10.0, 0.0, 0.0),
                rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                ..Transform::default()
            },
        ))
        .id();
    let middle = app
        .world_mut()
        .spawn((
            Transform {
                translation: Vec3::new(0.0, 0.0, -2.0),
                ..Transform::default()
            },
            ChildOf(object),
        ))
        .id();
    let slot = app
        .world_mut()
        .spawn((
            SmartObjectSlot::new("sit"),
            Transform {
                translation: Vec3::new(0.0, 1.0, 0.0),
                ..Transform::default()
            },
            GlobalTransform(Affine3A::IDENTITY),
            ChildOf(middle),
        ))
        .id();
    app.update();
    let index = app.world().resource::<SmartObjectIndex>();
    let info = index.slot(slot).expect("indexed");
    assert_eq!(info.object, object, "resolved through the middle node");
    // (0, 1, 0) in the middle node is (0, 1, -2) in the object; a quarter turn about +Y maps it to
    // (-2, 1, 0), and the object sits at x = 10.
    assert!(
        close(info.position, Vec3::new(8.0, 1.0, 0.0)),
        "{:?}",
        info.position
    );
    // Local −Z turned a quarter about +Y is world −X.
    assert!(close(info.forward, Vec3::NEG_X), "{:?}", info.forward);
    assert_eq!(index.object_of(slot), Some(object));
    assert_eq!(smart_object_ancestor(app.world(), slot), Some(object));
}

/// Spec 31 § 2: `effective_enabled` is true only for an enabled slot under an enabled object (or
/// with no object), and false for anything that is not a slot.
#[test]
fn effective_enabled_follows_the_slot_and_its_object() {
    let mut app = app();
    let object = app.world_mut().spawn(SmartObject::default()).id();
    let slot = app
        .world_mut()
        .spawn((SmartObjectSlot::new("sit"), ChildOf(object)))
        .id();
    let orphan = app.world_mut().spawn(SmartObjectSlot::new("sit")).id();
    let plain = app.world_mut().spawn_empty().id();
    assert!(effective_enabled(app.world(), slot));
    assert!(
        effective_enabled(app.world(), orphan),
        "a slot without an object is valid"
    );
    assert!(!effective_enabled(app.world(), plain), "not a slot");
    assert!(
        !effective_enabled(app.world(), object),
        "the object itself is not a slot"
    );

    app.world_mut()
        .get_mut::<SmartObject>(object)
        .expect("object")
        .enabled = false;
    assert!(
        !effective_enabled(app.world(), slot),
        "a disabled object disables the slot"
    );
    assert!(effective_enabled(app.world(), orphan));

    app.world_mut()
        .get_mut::<SmartObject>(object)
        .expect("object")
        .enabled = true;
    app.world_mut()
        .get_mut::<SmartObjectSlot>(slot)
        .expect("slot")
        .enabled = false;
    assert!(!effective_enabled(app.world(), slot), "a disabled slot");
}

/// The nearest available slot of `role` by a scan of every indexed slot: the reference
/// `nearest_available` must agree with (squared distance; ties keep the first in index order).
fn brute_nearest(index: &SmartObjectIndex, at: Vec3, role: &str) -> Option<(Entity, f32)> {
    let mut best: Option<(Entity, f32)> = None;
    for slot in index.slots() {
        if slot.role != role || !slot.available() {
            continue;
        }
        let d = (slot.position - at).length_squared();
        if best.is_none_or(|(_, b)| d < b) {
            best = Some((slot.entity, d));
        }
    }
    best
}

/// Spec 31 § 6: a level with 1,000 slots builds its index in one rebuild, every slot indexed with
/// its own position, and `nearest_available` agrees with a brute-force scan, before and after
/// claims. (The frame budget itself is a profiling target under the `smart_object_index` span,
/// not a test assert.)
#[test]
fn a_thousand_slot_level_indexes_every_slot_and_answers_like_a_scan() {
    let mut app = app();
    let object = app.world_mut().spawn(SmartObject::default()).id();
    let mut spawned = Vec::new();
    for i in 0..1_000_u32 {
        let (x, y, z) = (i % 10, (i / 10) % 10, i / 100);
        let position = Vec3::new(x as f32 * 3.0, y as f32 * 3.0, z as f32 * 3.0);
        let role = if i % 2 == 0 { "sit" } else { "cover" };
        let slot = app
            .world_mut()
            .spawn((
                SmartObjectSlot::new(role),
                GlobalTransform(Affine3A::from_translation(position)),
                ChildOf(object),
            ))
            .id();
        spawned.push((slot, position, role));
    }
    let before = app.world().resource::<SmartObjectIndex>().rebuilds();
    app.update();
    let index = app.world().resource::<SmartObjectIndex>();
    assert_eq!(index.len(), 1_000);
    assert!(index.rebuilds() > before);
    assert_eq!(index.for_role("sit").len(), 500);
    assert_eq!(index.available("cover").len(), 500);
    for (slot, position, role) in &spawned {
        let info = index.slot(*slot).expect("every slot indexed");
        assert_eq!(info.position, *position);
        assert_eq!(info.role, *role);
        assert_eq!(info.object, object);
    }
    let probes = [
        Vec3::new(1.3, 2.9, 4.1),
        Vec3::new(-5.0, 13.7, 26.2),
        Vec3::new(27.1, 0.4, 13.3),
        Vec3::new(14.2, 14.9, 14.6),
    ];
    let check = |index: &SmartObjectIndex| {
        for at in probes {
            for role in ["sit", "cover"] {
                let got = index.nearest_available(at, role).expect("a slot");
                let (want, want_d) = brute_nearest(index, at, role).expect("a slot");
                assert_eq!((got.position - at).length_squared(), want_d, "{at:?} {role}");
                assert_eq!(got.entity, want, "{at:?} {role}");
            }
        }
    };
    check(index);

    // Claim the nearest "sit" slot of every probe; the answers move to the next nearest.
    let mut claimed: Vec<Entity> = probes
        .iter()
        .filter_map(|at| index.nearest_available(*at, "sit").map(|s| s.entity))
        .collect();
    claimed.sort_unstable();
    claimed.dedup();
    let agent = app.world_mut().spawn_empty().id();
    for slot in &claimed {
        claim_slot(app.world_mut(), *slot, agent).expect("claims");
    }
    app.update();
    let index = app.world().resource::<SmartObjectIndex>();
    assert_eq!(index.available("sit").len(), 500 - claimed.len());
    for at in probes {
        let got = index.nearest_available(at, "sit").expect("a slot");
        assert!(!claimed.contains(&got.entity), "a claimed slot is never nearest");
    }
    check(index);
}

/// Spec 31 § 1: `claimed_by` is transient runtime state, `#[reflect(ignore)]`: the reflection path a
/// scene file uses writes the role and the enabled flag but never the claim, and a slot read back
/// is unclaimed.
#[test]
fn a_claim_is_never_persisted_through_reflection() {
    use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
    use regia_ecs::reflect::{AppTypeRegistry, FromReflect};
    use serde::de::DeserializeSeed;

    let mut app = app();
    let holder = app.world_mut().spawn_empty().id();
    let slot = SmartObjectSlot {
        claimed_by: Some(holder),
        enabled: false,
        ..SmartObjectSlot::new("operate")
    };
    let registry = app.world().resource::<AppTypeRegistry>().read();
    let text = ron::to_string(&TypedReflectSerializer::new(&slot, &registry)).expect("serialize");
    assert!(text.contains("operate"), "{text}");
    assert!(!text.contains("claimed_by"), "{text}");
    let registration = registry
        .get(std::any::TypeId::of::<SmartObjectSlot>())
        .expect("registered by the plugin");
    let mut deserializer = ron::Deserializer::from_str(&text).expect("text");
    let dynamic = TypedReflectDeserializer::new(registration, &registry)
        .deserialize(&mut deserializer)
        .expect("deserialize");
    let back = SmartObjectSlot::from_reflect(dynamic.as_ref()).expect("from reflect");
    assert_eq!(back.role, "operate");
    assert!(!back.enabled);
    assert_eq!(back.claimed_by, None);
}
