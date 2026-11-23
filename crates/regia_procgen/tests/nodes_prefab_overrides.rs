// SPDX-License-Identifier: MPL-2.0
//! `spawn.prefab` attribute overrides (spec 63 § 13.2, ADR-2724): each spawned instance carries its
//! point's attribute values, and applying them drives a reflected component field per instance.

mod common;

use common::*;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::reflect::AppTypeRegistry;
use regia_procgen::AttrValue;
use regia_procgen::exec::PrefabFieldOverride;
use regia_procgen::prefab_overrides::{
    ProcPrefabOverrides, apply_pending_prefab_overrides, apply_prefab_overrides,
};
use regia_scene::prefab::PendingPrefab;

/// An `i64` field. Values past 2^53 must not round through `f64`.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Counter {
    n: i64,
}

/// A `u64` field. `i64::MAX` must stay `i64::MAX`, not `i64::MAX + 1`.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct CounterU {
    n: u64,
}

/// A game component standing in for what a prefab carries.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Glow {
    /// A float field.
    pub intensity: f32,
    /// A vector field.
    pub offset: Vec3,
    /// An integer field.
    pub level: i32,
    /// A flag.
    pub on: bool,
    /// A text field.
    pub label: String,
}

/// Fields addressed by index and a one-field newtype: the apply side must agree with the
/// compile check (`check_compiled_overrides`) on both.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Panel {
    /// Three numbers by index.
    pub tint: [f32; 3],
    /// A one-field newtype.
    pub wrap: Wrap,
}

/// A one-field newtype (spec 62 § 3.5: only 2-4 component attributes write into structs).
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq)]
pub struct Wrap {
    /// The single field.
    pub value: f32,
}

const OVERRIDES: &str = "[(attr: \"position:x\", path: \"Glow.intensity\"), (attr: \"position\", path: \"Glow.offset\"), \
     (attr: \"position:x\", path: \"Glow.level\"), (attr: \"position:x\", path: \"Glow.on\"), (attr: \"kind\", path: \"Glow.label\")]";

/// Three points at x = 0, 2, 4 with a name attribute `kind`, spawned as prefabs with `overrides`.
fn prefab_graph(overrides: Option<&str>) -> regia_procgen::ProcGraph {
    let mut spawn = vec![("prefab", "\"props/lamp.prefab.ron\"")];
    if let Some(o) = overrides {
        spawn.push(("overrides", o));
    }
    graph(
        vec![
            (
                1,
                node(
                    "gen.grid",
                    &[
                        ("count", "(3.0, 1.0, 1.0)"),
                        ("step_m", "(2.0, 1.0, 1.0)"),
                        ("centered", "false"),
                    ],
                ),
            ),
            (
                2,
                node("attr.set", &[("target", "\"kind\""), ("value", "\"lamp\"")]),
            ),
            (3, node("spawn.prefab", &spawn)),
        ],
        &[(1, "out", 2, "in"), (2, "out", 3, "in")],
    )
}

fn empty() -> std::sync::Arc<regia_procgen::StaticSnapshot> {
    snapshot(box_shape([0.0; 3], [50.0; 3], 0.0), None)
}

fn test_world() -> World {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut r = handle.write();
        r.register::<Glow>();
        r.register::<Panel>();
        r.register::<Wrap>();
        r.register::<[f32; 3]>();
        r.register::<Counter>();
        r.register::<CounterU>();
        r.register::<f32>();
        r.register::<i32>();
        r.register::<i64>();
        r.register::<u64>();
        r.register::<bool>();
        r.register::<String>();
        r.register::<Vec3>();
    }
    world
}

fn apply_field(
    world: &mut World,
    entity: Entity,
    component: &str,
    field: &str,
    value: AttrValue,
) -> Vec<regia_procgen::prefab_overrides::OverrideFailure> {
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    apply_prefab_overrides(
        world,
        entity,
        "props/lamp.prefab.ron",
        &[PrefabFieldOverride {
            component: component.to_string(),
            field: field.to_string(),
            value,
        }],
        &registry.read(),
    )
}

#[test]
fn an_attribute_drives_a_component_field_on_each_spawned_instance() {
    let r = run(&prefab_graph(Some(OVERRIDES)), empty(), 4);
    assert_eq!(r.output.prefabs.len(), 3);
    let mut world = test_world();
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    for (k, spawn) in r.output.prefabs.iter().enumerate() {
        let x = 2.0 * k as f32;
        assert_eq!(spawn.overrides.len(), 5, "one value per override row");
        assert_eq!(spawn.overrides[0].value, AttrValue::Float(f64::from(x)));
        // The instance as the prefab system would leave it.
        let e = world.spawn(Glow::default()).id();
        let failures =
            apply_prefab_overrides(&mut world, e, &spawn.prefab, &spawn.overrides, &registry.read());
        assert!(failures.is_empty(), "instance {k}: {failures:?}");
        let glow = world.get::<Glow>(e).cloned().unwrap_or_default();
        assert_eq!(
            glow,
            Glow {
                intensity: x,
                offset: Vec3::new(x, 0.0, 0.0),
                level: x as i32,
                on: x != 0.0,
                label: "lamp".to_string(),
            },
            "instance {k}"
        );
    }
}

#[test]
fn the_component_may_live_on_a_child_of_the_instance_root() {
    let r = run(&prefab_graph(Some(OVERRIDES)), empty(), 4);
    let mut world = test_world();
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let spawn = &r.output.prefabs[2];
    let root = world.spawn_empty().id();
    let child = world.spawn((Glow::default(), ChildOf(root))).id();
    let failures = apply_prefab_overrides(
        &mut world,
        root,
        &spawn.prefab,
        &spawn.overrides,
        &registry.read(),
    );
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(world.get::<Glow>(child).map(|g| g.intensity), Some(4.0));
}

#[test]
fn pending_instances_get_their_overrides_once_the_prefab_resolves() {
    let r = run(&prefab_graph(Some(OVERRIDES)), empty(), 4);
    let mut world = test_world();
    let spawn = &r.output.prefabs[1];
    let e = world
        .spawn((
            Glow::default(),
            PendingPrefab::default(),
            ProcPrefabOverrides {
                prefab: spawn.prefab.clone(),
                overrides: spawn.overrides.clone(),
            },
        ))
        .id();
    apply_pending_prefab_overrides(&mut world);
    assert_eq!(
        world.get::<Glow>(e).map(|g| g.intensity),
        Some(0.0),
        "still pending"
    );
    assert!(world.get::<ProcPrefabOverrides>(e).is_some());
    world.entity_mut(e).remove::<PendingPrefab>();
    apply_pending_prefab_overrides(&mut world);
    assert_eq!(
        world.get::<Glow>(e).map(|g| g.intensity),
        Some(2.0),
        "applied after resolution"
    );
    assert!(world.get::<ProcPrefabOverrides>(e).is_none(), "applied once");
}

#[test]
fn an_array_element_path_applies_by_reflect_index() {
    // The pair of `array_elements_are_addressable_by_index` (the compile check): what the
    // check accepts, the apply path resolves — bevy_reflect's `tint[1]` syntax.
    let mut world = test_world();
    let e = world.spawn(Panel::default()).id();
    let failures = apply_field(&mut world, e, "Panel", "tint[1]", AttrValue::Float(0.25));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(world.get::<Panel>(e).map(|p| p.tint[1]), Some(0.25));
    assert_eq!(world.get::<Panel>(e).map(|p| p.tint[0]), Some(0.0));
}

#[test]
fn a_scalar_override_refuses_a_one_field_newtype() {
    // The compile check refuses a scalar into a 1-field struct (spec 62 § 3.5); the apply
    // side must refuse it too, not convert where the check would not.
    let mut world = test_world();
    let e = world.spawn(Panel::default()).id();
    let failures = apply_field(&mut world, e, "Panel", "wrap", AttrValue::Float(0.25));
    assert_eq!(
        failures.iter().map(|f| f.code).collect::<Vec<_>>(),
        ["override_type_mismatch"],
        "{failures:?}"
    );
    assert_eq!(world.get::<Panel>(e).map(|p| p.wrap.value), Some(0.0));
}

#[test]
fn bad_overrides_report_stable_codes_and_the_others_still_apply() {
    let rows = "[(attr: \"position:x\", path: \"Nope.intensity\"), (attr: \"position:x\", path: \"Glow.nothing\"), \
         (attr: \"position\", path: \"Glow.intensity\"), (attr: \"kind\", path: \"Glow.level\"), (attr: \"position:x\", path: \"Glow.intensity\")]";
    let r = run(&prefab_graph(Some(rows)), empty(), 4);
    let mut world = test_world();
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let spawn = &r.output.prefabs[1];
    let e = world.spawn(Glow::default()).id();
    let codes: Vec<&str> =
        apply_prefab_overrides(&mut world, e, &spawn.prefab, &spawn.overrides, &registry.read())
            .iter()
            .map(|f| f.code)
            .collect();
    assert_eq!(
        codes,
        [
            "override_path_unknown",
            "override_path_unknown",
            "override_type_mismatch",
            "override_type_mismatch"
        ]
    );
    assert_eq!(
        world.get::<Glow>(e).map(|g| g.intensity),
        Some(2.0),
        "the valid row applied"
    );
    // Messages name the prefab.
    let msg = apply_prefab_overrides(
        &mut world,
        e,
        &spawn.prefab,
        &spawn.overrides[..1],
        &registry.read(),
    );
    assert!(
        msg.first()
            .is_some_and(|f| f.message.contains("props/lamp.prefab.ron")),
        "{msg:?}"
    );
}

#[test]
fn override_rows_are_validated_and_absent_overrides_keep_the_output_hash() {
    let reg = regia_procgen::ProcNodeRegistry::with_builtins();
    let codes = |o: &str| -> Vec<String> {
        match prefab_graph(Some(o)).compile(&reg) {
            Ok(_) => Vec::new(),
            Err(e) => e.0.iter().map(|x| x.code.clone()).collect(),
        }
    };
    assert_eq!(
        codes("[(attr: \"a\", path: \"Glow\")]"),
        ["override_path_unknown"],
        "no field"
    );
    assert_eq!(
        codes("[(attr: \"a\", path: \"Glow.\")]"),
        ["override_path_unknown"]
    );
    assert_eq!(codes("[(path: \"Glow.intensity\")]"), ["setting_type"], "no attr");
    assert!(
        codes("[(attr: \"a\", path: \"my_game::fx::Glow.light.range\")]").is_empty(),
        "type paths"
    );
    assert!(codes("[]").is_empty());
    // An empty list and no setting give the same bytes; overrides change them.
    let plain = run(&prefab_graph(None), empty(), 4);
    let empty_list = run(&prefab_graph(Some("[]")), empty(), 4);
    let with = run(&prefab_graph(Some(OVERRIDES)), empty(), 4);
    assert_eq!(plain.output_hash, empty_list.output_hash);
    assert_ne!(plain.output_hash, with.output_hash);
    assert!(plain.output.prefabs.iter().all(|p| p.overrides.is_empty()));
    // A missing attribute fails the run.
    let g = prefab_graph(Some("[(attr: \"missing\", path: \"Glow.intensity\")]"));
    let e = run_with(&compile(&g), &regia_procgen::RunInputs::new(empty(), 4)).map(|r| r.output_hash);
    assert!(
        matches!(&e, Err(regia_procgen::RunError::Node { code, .. }) if code == "attr_missing"),
        "{e:?}"
    );
}

#[test]
fn integer_overrides_keep_every_bit_and_reject_non_finite_numbers() {
    let exact = (1_i64 << 53) + 1;
    let mut world = test_world();
    let e = world.spawn(Counter::default()).id();
    let failures = apply_field(&mut world, e, "Counter", "n", AttrValue::Int(exact));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(world.get::<Counter>(e).map(|c| c.n), Some(exact));

    let e = world.spawn(Counter::default()).id();
    let failures = apply_field(&mut world, e, "Counter", "n", AttrValue::Int(i64::MAX));
    assert!(failures.is_empty(), "i64::MAX dropped: {failures:?}");
    assert_eq!(world.get::<Counter>(e).map(|c| c.n), Some(i64::MAX));

    let e = world.spawn(Counter::default()).id();
    let failures = apply_field(&mut world, e, "Counter", "n", AttrValue::Int(i64::MIN));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(world.get::<Counter>(e).map(|c| c.n), Some(i64::MIN));

    let e = world.spawn(Counter::default()).id();
    let failures = apply_field(&mut world, e, "Counter", "n", AttrValue::Float(4.0));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(world.get::<Counter>(e).map(|c| c.n), Some(4));

    let e = world.spawn(CounterU::default()).id();
    let failures = apply_field(&mut world, e, "CounterU", "n", AttrValue::Int(i64::MAX));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        world.get::<CounterU>(e).map(|c| c.n),
        Some(i64::MAX as u64),
        "rounded up through f64"
    );

    let e = world.spawn(Counter::default()).id();
    let failures = apply_field(&mut world, e, "Counter", "n", AttrValue::Float(1.0e20));
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].code, "override_type_mismatch");
    assert_eq!(world.get::<Counter>(e).map(|c| c.n), Some(0));

    let e = world.spawn(Glow::default()).id();
    let failures = apply_field(&mut world, e, "Glow", "intensity", AttrValue::Float(f64::NAN));
    assert_eq!(failures.len(), 1, "NaN written: {failures:?}");
    assert_eq!(failures[0].code, "override_type_mismatch");
    assert_eq!(world.get::<Glow>(e).map(|g| g.intensity), Some(0.0));

    let e = world.spawn(Glow::default()).id();
    let failures = apply_field(
        &mut world,
        e,
        "Glow",
        "intensity",
        AttrValue::Float(f64::INFINITY),
    );
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].code, "override_type_mismatch");
    assert_eq!(world.get::<Glow>(e).map(|g| g.intensity), Some(0.0));

    let e = world.spawn(Glow::default()).id();
    let failures = apply_field(&mut world, e, "Glow", "on", AttrValue::Float(f64::NAN));
    assert_eq!(failures.len(), 1, "NaN became a bool: {failures:?}");
    assert_eq!(failures[0].code, "override_type_mismatch");
    assert_eq!(world.get::<Glow>(e).map(|g| g.on), Some(false));

    let e = world.spawn(Glow::default()).id();
    let failures = apply_field(&mut world, e, "Glow", "on", AttrValue::Float(2.0));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(world.get::<Glow>(e).map(|g| g.on), Some(true));
}
