// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep (sprint-test-quality-sweep, ADR-2077): deterministic edge-case tests for
//! the least-covered public functions of `regia_ai` (ability gating, spatial-query parameter
//! values and score shapes, state-tree literals and includes, claim priorities, query regions,
//! smart-object definitions, slot enablement, the role index and materialization). A test that
//! failed on the old code is the regression test for the fix named next to it in `src`.

use regia_ai::smart_object::priority_color;
use regia_ai::smart_object_def::{
    MAX_DEF_SLOTS, smart_object_def_migration_sources, smart_object_def_schema_fingerprint,
};
use regia_ai::smart_object_v2::slot_transform;
use regia_ai::spatial_query::{Combine, Num, ParamValue, Shape, apply_shape, combine};
use regia_ai::state_tree::{IncludeDef, Literal};
use regia_ai::{
    Abilities, Ability, AbilityDef, AbilityError, BbValue, ClaimPriority, QueryRegion, SmartObject,
    SmartObjectDef, SmartObjectIndex, SmartObjectPlugin, SmartObjectSlot, StateTreeDef, effective_enabled,
    materialize_object,
};
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_scene::GlobalTransform;

/// Values that hostile callers pass: zero, signed zero, tiny, negative, huge, infinities and NaN.
const HOSTILE: [f32; 8] = [
    0.0,
    -0.0,
    -1.0,
    1e-30,
    f32::MAX,
    f32::INFINITY,
    f32::NEG_INFINITY,
    f32::NAN,
];

// ---------------------------------------------------------------------------------------------
// abilities.rs

#[test]
fn ability_def_effective_cooldown_and_cost_are_finite_and_non_negative() {
    let def = AbilityDef::new("fire", 2.5, 10.0);
    assert_eq!(def.effective_cooldown(), 2.5);
    assert_eq!(def.effective_cost(), 10.0);
    for v in HOSTILE {
        let def = AbilityDef::new("x", v, v);
        let (c, k) = (def.effective_cooldown(), def.effective_cost());
        assert!(c.is_finite() && c >= 0.0, "cooldown {v} -> {c}");
        assert!(k.is_finite() && k >= 0.0, "cost {v} -> {k}");
        if !(v.is_finite() && v > 0.0) {
            assert_eq!((c, k), (0.0, 0.0), "{v}");
        }
    }
}

#[test]
fn ability_remaining_secs_is_never_negative_or_non_finite() {
    for v in HOSTILE {
        let slot = Ability {
            remaining: v,
            ..Ability::new(AbilityDef::new("a", 1.0, 0.0))
        };
        let r = slot.remaining_secs();
        assert!(r.is_finite() && r >= 0.0, "remaining {v} -> {r}");
        assert_eq!(slot.is_ready(), r == 0.0, "{v}");
    }
}

#[test]
fn ability_can_activate_agrees_with_try_activate_and_never_mutates() {
    let pools = [0.0, 4.0, 5.0, 6.0, -3.0, f32::NAN, f32::INFINITY];
    for cooldown in [0.0, 1.0] {
        for remaining in [0.0, 0.5] {
            for have in pools {
                let slot = Ability {
                    remaining,
                    ..Ability::new(AbilityDef::new("bolt", cooldown, 5.0))
                };
                let before = slot.clone();
                let check = slot.can_activate(have);
                assert_eq!(slot, before, "can_activate mutated the slot");
                let mut fired = slot.clone();
                let mut pool = have;
                let result = fired.try_activate(&mut pool);
                assert_eq!(check, result, "remaining {remaining}, pool {have}");
                match result {
                    Ok(()) => {
                        assert_eq!(fired.activations, 1);
                        assert!(pool.is_finite() && pool >= 0.0);
                        assert_eq!(fired.remaining_secs(), cooldown);
                    }
                    Err(AbilityError::OnCooldown { remaining: r, .. }) => {
                        assert_eq!(r, remaining);
                        assert_eq!(fired, before);
                    }
                    Err(AbilityError::Insufficient { need, have: h, .. }) => {
                        assert_eq!(need, 5.0);
                        assert!(h.is_finite() && h < need);
                        assert_eq!(fired, before);
                        assert!(pool.to_bits() == have.to_bits(), "a rejection spent the pool");
                    }
                    Err(e) => panic!("unexpected {e}"),
                }
            }
        }
    }
    // A free ability fires against any pool, even a NaN one.
    let free = Ability::new(AbilityDef::new("wave", 0.0, 0.0));
    for have in HOSTILE {
        assert_eq!(free.can_activate(have), Ok(()), "{have}");
    }
}

#[test]
fn abilities_cancel_resets_one_known_slot_only() {
    let mut set = Abilities::new();
    assert!(set.add(AbilityDef::new("a", 3.0, 0.0)));
    assert!(set.add(AbilityDef::new("b", 3.0, 0.0)));
    let mut pool = 0.0;
    set.try_activate("a", &mut pool).expect("a fires");
    set.try_activate("b", &mut pool).expect("b fires");
    assert!(set.cancel("a"));
    assert!(set.is_ready("a"));
    assert_eq!(set.remaining("b"), 3.0);
    assert!(!set.cancel("missing"));
    assert!(!set.contains("missing"), "cancel must not create a slot");
    // Cancelling a ready slot is harmless.
    assert!(set.cancel("a"));
    assert_eq!(set.get("a").map(|s| s.activations), Some(1));
}

// ---------------------------------------------------------------------------------------------
// spatial_query: ParamValue, Num, apply_shape, combine

#[test]
fn param_value_as_f32_reads_numbers_only() {
    assert_eq!(ParamValue::Int(-7).as_f32(), Some(-7.0));
    assert_eq!(ParamValue::Float(2.5).as_f32(), Some(2.5));
    assert_eq!(ParamValue::Int(i64::MAX).as_f32(), Some(i64::MAX as f32));
    assert_eq!(ParamValue::Bool(true).as_f32(), None);
    assert_eq!(ParamValue::Name("radius".into()).as_f32(), None);
    assert!(ParamValue::Float(f32::NAN).as_f32().is_some_and(f32::is_nan));
}

#[test]
fn num_param_names_only_parameter_references() {
    assert_eq!(Num::Lit(3.0).param(), None);
    assert_eq!(Num::from(1.5), Num::Lit(1.5));
    assert_eq!(Num::Param("radius".into()).param(), Some("radius"));
    assert_eq!(Num::Param(String::new()).param(), Some(""));
    // Both RON spellings of a reference read as a parameter, a number as a literal.
    let read = |text: &str| ron::from_str::<Num>(text).expect(text);
    assert_eq!(read("ParamF32(\"radius\")").param(), Some("radius"));
    assert_eq!(read("(\"radius\")").param(), Some("radius"));
    assert_eq!(read("8"), Num::Lit(8.0));
    assert_eq!(read("-2.5"), Num::Lit(-2.5));
    assert!(ron::from_str::<Num>("(\"a\", \"b\")").is_err());
    assert!(ron::from_str::<Num>("()").is_err());
    // A reference survives a write and a read.
    let text = ron::to_string(&Num::Param("k".into())).expect("writes");
    assert_eq!(read(&text), Num::Param("k".into()));
}

#[test]
fn apply_shape_maps_the_unit_interval_into_itself_and_combine_handles_edges() {
    let shapes = [
        Shape::Flat,
        Shape::Linear,
        Shape::Quadratic,
        Shape::Falling,
        Shape::Root,
    ];
    for shape in shapes {
        let mut last = None;
        for i in 0..=100 {
            let n = i as f32 / 100.0;
            let y = apply_shape(shape, n);
            assert!((0.0..=1.0).contains(&y), "{shape:?}({n}) = {y}");
            if let Some(prev) = last {
                // Monotonic: rising shapes rise, `Falling` falls, `Flat` stays.
                match shape {
                    Shape::Falling => assert!(y <= prev),
                    Shape::Flat => assert_eq!(y, prev),
                    _ => assert!(y >= prev),
                }
            }
            last = Some(y);
        }
    }
    assert_eq!(apply_shape(Shape::Root, 0.25), 0.5);
    assert_eq!(combine(Combine::Mean, &[]), None);
    assert!(combine(Combine::Mean, &[0.2, 0.4]).is_some_and(|m| (m - 0.3).abs() < 1e-6));
    assert_eq!(combine(Combine::Lowest, &[0.7, 0.2, 0.9]), Some(0.2));
    assert_eq!(combine(Combine::Highest, &[0.7, 0.2, 0.9]), Some(0.9));
    assert_eq!(combine(Combine::Product, &[0.5, 0.5]), Some(0.25));
    assert_eq!(combine(Combine::Lowest, &[0.4]), Some(0.4));
}

// ---------------------------------------------------------------------------------------------
// state_tree: Literal::to_value, StateTreeDef::has_includes

#[test]
fn literal_to_value_keeps_the_type_and_the_value() {
    assert_eq!(Literal::Bool(true).to_value(), BbValue::Bool(true));
    assert_eq!(Literal::Int(i64::MIN).to_value(), BbValue::Int(i64::MIN));
    assert_eq!(Literal::Float(-0.5).to_value(), BbValue::Float(-0.5));
    assert_eq!(
        Literal::String("ü".into()).to_value(),
        BbValue::String("ü".into())
    );
    // Signed zero is preserved bit for bit.
    match Literal::Float(-0.0).to_value() {
        BbValue::Float(v) => assert_eq!(v.to_bits(), (-0.0_f32).to_bits()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn has_includes_follows_the_include_list_and_expansion_clears_it() {
    let mut def = StateTreeDef::new("root");
    assert!(!def.has_includes());
    def.includes.push(IncludeDef {
        prefix: "combat".into(),
        path: "trees/combat.statetree.ron".into(),
    });
    assert!(def.has_includes());
    // A missing include fails; nothing is half-expanded.
    assert!(def.expand_includes(&|_| None).is_err());
    assert!(def.has_includes());
    def.includes.clear();
    let expanded = def.expand_includes(&|_| None).expect("nothing to include");
    assert!(!expanded.has_includes());
}

// ---------------------------------------------------------------------------------------------
// smart_object_v2: ClaimPriority, QueryRegion, slot_transform; smart_object: priority_color

#[test]
fn claim_priority_names_round_trip_case_insensitively() {
    let all = [
        ClaimPriority::Background,
        ClaimPriority::Low,
        ClaimPriority::Normal,
        ClaimPriority::High,
        ClaimPriority::Urgent,
    ];
    for p in all {
        assert_eq!(ClaimPriority::from_name(p.name()), Some(p));
        assert_eq!(ClaimPriority::from_name(&p.name().to_ascii_uppercase()), Some(p));
        assert!(p.name().bytes().all(|b| b.is_ascii_lowercase()));
    }
    assert!(all.windows(2).all(|w| w[0] < w[1]), "priorities are ordered");
    for bad in ["", " normal", "normal ", "norm", "urgentt", "ＨＩＧＨ"] {
        assert_eq!(ClaimPriority::from_name(bad), None, "{bad:?}");
    }
    // Every priority has its own overlay color.
    let colors: Vec<String> = all.iter().map(|p| format!("{:?}", priority_color(*p))).collect();
    for (i, a) in colors.iter().enumerate() {
        for b in &colors[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn query_region_is_valid_rejects_non_finite_and_inverted_shapes() {
    let sphere = |center: Vec3, radius: f32| QueryRegion::Sphere { center, radius };
    let aabb = |min: Vec3, max: Vec3| QueryRegion::Aabb { min, max };
    assert!(sphere(Vec3::ZERO, 0.0).is_valid());
    assert!(sphere(Vec3::ONE, 5.0).is_valid());
    assert!(!sphere(Vec3::ZERO, -0.1).is_valid());
    assert!(!sphere(Vec3::ZERO, f32::NAN).is_valid());
    assert!(!sphere(Vec3::ZERO, f32::INFINITY).is_valid());
    assert!(!sphere(Vec3::NAN, 1.0).is_valid());
    assert!(aabb(Vec3::ZERO, Vec3::ZERO).is_valid());
    assert!(aabb(-Vec3::ONE, Vec3::ONE).is_valid());
    assert!(!aabb(Vec3::ONE, -Vec3::ONE).is_valid());
    assert!(!aabb(Vec3::new(0.0, 2.0, 0.0), Vec3::new(1.0, 1.0, 1.0)).is_valid());
    assert!(!aabb(Vec3::ZERO, Vec3::new(f32::INFINITY, 1.0, 1.0)).is_valid());
    assert!(!aabb(Vec3::NAN, Vec3::ONE).is_valid());
    // A valid region contains its reference point; no region contains a NaN point.
    for r in [sphere(Vec3::ONE, 0.0), aabb(-Vec3::ONE, Vec3::ONE)] {
        assert!(r.contains(r.reference()));
        assert!(!r.contains(Vec3::NAN));
    }
}

#[test]
fn slot_transform_reads_the_propagated_global_transform_only() {
    let mut world = World::new();
    let bare = world.spawn(SmartObjectSlot::new("sit")).id();
    assert_eq!(slot_transform(&world, bare), None);
    let at = Affine3A::from_translation(Vec3::new(1.0, 2.0, 3.0));
    let placed = world
        .spawn((SmartObjectSlot::new("sit"), GlobalTransform(at)))
        .id();
    assert_eq!(slot_transform(&world, placed), Some(GlobalTransform(at)));
    world.despawn(placed);
    assert_eq!(slot_transform(&world, placed), None);
}

// ---------------------------------------------------------------------------------------------
// smart_object: effective_enabled, SmartObjectIndex::for_role

#[test]
fn effective_enabled_needs_an_enabled_slot_and_an_enabled_object_chain() {
    let mut world = World::new();
    let nothing = world.spawn_empty().id();
    assert!(!effective_enabled(&world, nothing), "no slot");

    let loose = world.spawn(SmartObjectSlot::new("sit")).id();
    assert!(effective_enabled(&world, loose), "a slot without an object");

    let object = world.spawn(SmartObject::default()).id();
    let group = world.spawn(ChildOf(object)).id();
    let deep = world.spawn((SmartObjectSlot::new("sit"), ChildOf(group))).id();
    assert!(
        effective_enabled(&world, deep),
        "the object two levels up is enabled"
    );

    world.get_mut::<SmartObject>(object).expect("object").enabled = false;
    assert!(
        !effective_enabled(&world, deep),
        "a disabled object disables its slots"
    );
    world.get_mut::<SmartObject>(object).expect("object").enabled = true;
    world
        .get_mut::<SmartObject>(object)
        .expect("object")
        .off_reasons
        .insert("quest".into());
    assert!(
        !effective_enabled(&world, deep),
        "an object off reason disables its slots"
    );
    world
        .get_mut::<SmartObject>(object)
        .expect("object")
        .off_reasons
        .clear();

    world
        .get_mut::<SmartObjectSlot>(deep)
        .expect("slot")
        .off_reasons
        .insert("broken".into());
    assert!(!effective_enabled(&world, deep), "a slot off reason");
    world
        .get_mut::<SmartObjectSlot>(deep)
        .expect("slot")
        .off_reasons
        .clear();
    world.get_mut::<SmartObjectSlot>(deep).expect("slot").enabled = false;
    assert!(!effective_enabled(&world, deep), "a disabled slot");
}

#[test]
fn for_role_lists_every_slot_of_a_role_in_a_stable_order_unlike_available() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SmartObjectPlugin));
    let object = app.world_mut().spawn(SmartObject::default()).id();
    let mut spawn = |role: &str, x: f32| {
        app.world_mut()
            .spawn((
                SmartObjectSlot::new(role),
                GlobalTransform(Affine3A::from_translation(Vec3::new(x, 0.0, 0.0))),
                ChildOf(object),
            ))
            .id()
    };
    let a = spawn("sit", 0.0);
    let b = spawn("sit", 1.0);
    let c = spawn("lean", 2.0);
    let off = spawn("sit", 3.0);
    app.world_mut()
        .get_mut::<SmartObjectSlot>(off)
        .expect("slot")
        .enabled = false;
    app.update();
    let index = app.world().resource::<SmartObjectIndex>();
    let sit: Vec<Entity> = index.for_role("sit").iter().map(|s| s.entity).collect();
    assert_eq!(sit.len(), 3, "{sit:?}");
    assert!(sit.contains(&a) && sit.contains(&b) && sit.contains(&off));
    assert!(!sit.contains(&c));
    let again: Vec<Entity> = index.for_role("sit").iter().map(|s| s.entity).collect();
    assert_eq!(sit, again);
    let available: Vec<Entity> = index.available("sit").iter().map(|s| s.entity).collect();
    assert!(
        !available.contains(&off),
        "a disabled slot is listed but not available"
    );
    assert!(index.for_role("").is_empty());
    assert!(index.for_role("Sit").is_empty(), "roles are case-sensitive");
    for info in index.for_role("lean") {
        assert_eq!(info.role, "lean");
        assert_eq!(info.object, object);
    }
}

// ---------------------------------------------------------------------------------------------
// smart_object_def: validate, materialize_object, migration/fingerprint

fn def_with_rotation(rotation: &str) -> String {
    format!("(format_version: 1, slots: [(name: \"a\", offset: (rotation: {rotation}))])")
}

#[test]
fn smart_object_def_validate_rejects_rotations_that_cannot_be_normalized() {
    // A unit quaternion and a scaled one both normalize.
    for ok in [
        "(0.0, 0.0, 0.0, 1.0)",
        "(0.0, 0.0, 0.0, 3.0)",
        "(0.0, 0.7071, 0.0, 0.7071)",
    ] {
        assert!(SmartObjectDef::from_ron(&def_with_rotation(ok)).is_ok(), "{ok}");
    }
    // Zero, NaN and a quaternion whose squared length overflows `f32` would all make a
    // degenerate slot rotation.
    for bad in [
        "(0.0, 0.0, 0.0, 0.0)",
        "(1e20, 0.0, 0.0, 0.0)",
        "(0.0, 0.0, 3e19, 3e19)",
    ] {
        let err = SmartObjectDef::from_ron(&def_with_rotation(bad)).expect_err(bad);
        assert_eq!(err.reason(), "bad_definition", "{bad}");
    }
    let mut def = SmartObjectDef::from_ron(&def_with_rotation("(0.0, 0.0, 0.0, 1.0)")).expect("valid");
    def.slots[0].offset.translation.1 = f32::NAN;
    assert!(def.validate().is_err(), "NaN translation");
    def.slots[0].offset.translation.1 = f32::INFINITY;
    assert!(def.validate().is_err(), "infinite translation");
}

#[test]
fn smart_object_def_validate_counts_and_names() {
    let slots = |n: usize| {
        let s: Vec<String> = (0..n).map(|i| format!("(name: \"s{i}\")")).collect();
        format!("(format_version: 1, slots: [{}])", s.join(","))
    };
    assert!(SmartObjectDef::from_ron(&slots(MAX_DEF_SLOTS)).is_ok());
    assert!(SmartObjectDef::from_ron(&slots(0)).is_ok());
    let name = |n: &str| SmartObjectDef::from_ron(&format!("(format_version: 1, slots: [(name: \"{n}\")])"));
    assert!(name(&"a".repeat(32)).is_ok());
    assert!(name("Seat_01").is_ok());
    for bad in ["sièce", "a-b", "a.b", "a/b", "日本"] {
        assert!(name(bad).is_err(), "{bad}");
    }
    // Names differing only in case are different slots.
    assert!(SmartObjectDef::from_ron("(format_version: 1, slots: [(name: \"a\"), (name: \"A\")])").is_ok());
}

#[test]
fn smart_object_def_format_metadata_is_stable() {
    assert!(smart_object_def_migration_sources().is_empty());
    let a = smart_object_def_schema_fingerprint();
    assert!(!a.is_empty());
    assert_eq!(a, smart_object_def_schema_fingerprint());
}

fn slot_children(world: &World, object: Entity) -> Vec<(Entity, String, Option<u16>)> {
    let mut out: Vec<(Entity, String, Option<u16>)> = world
        .get::<Children>(object)
        .map(|c| c.iter().collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| {
            world
                .get::<SmartObjectSlot>(e)
                .map(|s| (e, s.role.clone(), s.def_slot))
        })
        .collect();
    out.sort_by_key(|(_, _, i)| *i);
    out
}

#[test]
fn materialize_object_is_idempotent_and_shrinks_and_keeps_hand_made_slots() {
    let three = SmartObjectDef::from_ron(
        "(format_version: 1, slots: [(name: \"a\"), (name: \"b\"), (name: \"c\", enabled: false)])",
    )
    .expect("valid");
    let one = SmartObjectDef::from_ron("(format_version: 1, slots: [(name: \"z\")])").expect("valid");
    let mut world = World::new();
    let object = world.spawn_empty().id();
    // A slot the author placed by hand (no definition index) is never touched.
    let manual = world
        .spawn((SmartObjectSlot::new("manual"), ChildOf(object)))
        .id();

    materialize_object(&mut world, object, &three);
    assert!(
        world.get::<SmartObject>(object).is_some(),
        "the object gets a SmartObject"
    );
    let first = slot_children(&world, object);
    let roles: Vec<&str> = first.iter().map(|(_, r, _)| r.as_str()).collect();
    assert_eq!(roles, ["manual", "a", "b", "c"]);
    let c = first[3].0;
    assert!(!world.get::<SmartObjectSlot>(c).expect("c").enabled);

    materialize_object(&mut world, object, &three);
    assert_eq!(
        slot_children(&world, object),
        first,
        "a second pass changes nothing"
    );

    materialize_object(&mut world, object, &one);
    let after = slot_children(&world, object);
    let roles: Vec<&str> = after.iter().map(|(_, r, _)| r.as_str()).collect();
    assert_eq!(roles, ["manual", "z"]);
    // Slot 0 is reused (renamed), the others are gone.
    assert_eq!(after[1].0, first[1].0);
    assert!(world.get_entity(first[2].0).is_err() && world.get_entity(c).is_err());
    assert!(world.get_entity(manual).is_ok());
}
