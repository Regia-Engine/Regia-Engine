// SPDX-License-Identifier: MPL-2.0
use regia_ecs::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;

use super::*;
use crate::components::{EntityGuid, Name, Tags};
use crate::format::{PrefabOverride, SceneFile};
use crate::prefab::{PrefabInstance, PrefabLink, PrefabTemplate, instantiate_prefab};
use crate::spawn::SpawnOptions;
use crate::transform::Transform;

/// Test component with one field to override.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Lamp {
    /// Brightness.
    pub intensity: f32,
}

const LAMP: &str = "regia_scene::prefab_variant::tests::Lamp";
const TAGS: &str = "regia_scene::components::Tags";

struct Ids {
    root: Guid,
    light: Guid,
    knob: Guid,
}

fn ids() -> Ids {
    Ids {
        root: Guid::from_u128(0x1001),
        light: Guid::from_u128(0x1002),
        knob: Guid::from_u128(0x1003),
    }
}

fn world() -> World {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        crate::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
        registry.register::<Tags>();
        registry.register::<smallvec::SmallVec<[Name; 4]>>();
        registry.register::<Lamp>();
    }
    world.init_resource::<PrefabLibrary>();
    world
}

/// The base: root → light (Lamp 100) → knob.
fn lamp_base(intensity: f32) -> SceneFile {
    let Ids { root, light, knob } = ids();
    let text = format!(
        r#"(
    format_version: 1,
    entities: [
        (guid: "{root}", name: "Lamp", components: {{ "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0)) }}),
        (guid: "{light}", name: "Light", parent: Some("{root}"), components: {{ "{LAMP}": (intensity: {intensity:?}) }}),
        (guid: "{knob}", name: "Knob", parent: Some("{light}"), components: {{}}),
    ],
)"#
    );
    SceneFile::from_ron_str(&text).expect("base parses")
}

fn register_base(world: &mut World, asset: Guid, intensity: f32) {
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let template =
        PrefabTemplate::from_scene_file(asset, &lamp_base(intensity), &registry.read()).expect("bake");
    world.resource_mut::<PrefabLibrary>().insert(template);
}

fn ov(target: Guid, component: &str, path: &str, value: &str) -> PrefabOverride {
    PrefabOverride {
        target,
        component: component.into(),
        path: path.into(),
        value: ron::value::RawValue::from_ron(value).expect("ron").to_owned(),
    }
}

fn register_variant(world: &mut World, asset: Guid, file: &PrefabVariantFile) {
    let variant = PrefabVariant::from_file(asset, file).expect("variant");
    world.resource_mut::<PrefabLibrary>().insert_variant(variant);
}

fn spawn(world: &mut World, asset: Guid, root_guid: Guid) -> crate::prefab::Instantiation {
    let root = world.spawn(EntityGuid(root_guid)).id();
    instantiate_prefab(
        world,
        &PrefabInstanceData::plain(SceneAssetRef(asset)),
        root,
        &SpawnOptions::default(),
        0,
    )
    .expect("instantiate")
}

fn intensity(world: &World, entity: Entity) -> f32 {
    world.get::<Lamp>(entity).expect("lamp").intensity
}

#[test]
fn the_file_round_trips_migrates_and_refuses_newer_versions() {
    let base = Guid::from_u128(0xb);
    let mut file = PrefabVariantFile::new(base);
    file.base
        .overrides
        .push(ov(ids().light, LAMP, ".intensity", "800.0"));
    file.base.removed_children.push(ids().knob);
    let text = file.to_ron_string().expect("write");
    assert!(text.starts_with("PrefabVariant("), "{text}");
    let back = PrefabVariantFile::from_ron_str(&text).expect("read");
    assert_eq!(back, file);
    assert_eq!(
        back.to_ron_string().expect("rewrite"),
        text,
        "parse then save is stable"
    );

    // Hand-written without a version: the v1 shape, migrated.
    let bare = format!(r#"(base: (prefab: "asset:{base}"))"#);
    let migrated = PrefabVariantFile::from_ron_str(&bare).expect("bare");
    assert_eq!(migrated.format_version, PREFAB_VARIANT_FORMAT_VERSION);
    assert_eq!(migrated.base_guid(), base);

    // Newer: refused by version even though the body does not match.
    let newer = r#"(format_version: 2, something_else: 3)"#;
    assert!(matches!(
        PrefabVariantFile::from_ron_str(newer),
        Err(PrefabVariantError::Newer { found: 2 })
    ));

    // Nil base and nil targets are structural errors.
    let nil = format!(r#"(format_version: 1, base: (prefab: "{}"))"#, Guid::NIL);
    assert!(matches!(
        PrefabVariantFile::from_ron_str(&nil),
        Err(PrefabVariantError::Invalid(_))
    ));
    let nil_target = format!(
        r#"(format_version: 1, base: (prefab: "{base}", removed_components: [(target: "{}", component: "{TAGS}")]))"#,
        Guid::NIL
    );
    assert!(matches!(
        PrefabVariantFile::from_ron_str(&nil_target),
        Err(PrefabVariantError::Invalid(_))
    ));
}

#[test]
fn a_variant_spawns_its_base_with_its_edits_and_follows_base_changes() {
    let mut world = world();
    let (base, red) = (Guid::from_u128(0xb), Guid::from_u128(0xc));
    register_base(&mut world, base, 100.0);
    let mut file = PrefabVariantFile::new(base);
    file.base
        .overrides
        .push(ov(ids().knob, TAGS, ".", r#"(["red"])"#));
    register_variant(&mut world, red, &file);

    let first = spawn(&mut world, red, Guid::from_u128(0xa1));
    assert_eq!(first.entities_spawned, 3, "{:?}", first.warnings);
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    assert_eq!(intensity(&world, first.locals[&ids().light]), 100.0);
    let tags = world.get::<Tags>(first.locals[&ids().knob]).expect("tags");
    assert!(tags.contains("red"), "{tags:?}");
    // The instance records the variant (what it instantiated), and the base's local ids.
    let record = world.get::<PrefabInstance>(first.root).expect("record");
    assert_eq!(record.prefab_guid(), red);
    assert!(
        record.overrides.is_empty(),
        "the variant's edits are not instance overrides"
    );
    assert_eq!(
        world
            .get::<PrefabLink>(first.locals[&ids().light])
            .expect("link")
            .guid(),
        ids().light
    );

    // The base changes: the variant follows (it never copied the base's values).
    register_base(&mut world, base, 50.0);
    let second = spawn(&mut world, red, Guid::from_u128(0xa2));
    assert_eq!(intensity(&world, second.locals[&ids().light]), 50.0);
    assert!(
        world
            .get::<Tags>(second.locals[&ids().knob])
            .expect("tags")
            .contains("red")
    );
}

#[test]
fn chain_steps_apply_base_most_first_and_the_instance_wins() {
    let mut world = world();
    let (base, a, b) = (Guid::from_u128(0xb), Guid::from_u128(0xa), Guid::from_u128(0xbb));
    register_base(&mut world, base, 100.0);
    let mut file_a = PrefabVariantFile::new(base);
    file_a
        .base
        .overrides
        .push(ov(ids().light, LAMP, ".intensity", "200.0"));
    file_a
        .base
        .overrides
        .push(ov(ids().root, TAGS, ".", r#"(["from_a"])"#));
    register_variant(&mut world, a, &file_a);
    let mut file_b = PrefabVariantFile::new(a);
    file_b
        .base
        .overrides
        .push(ov(ids().light, LAMP, ".intensity", "300.0"));
    file_b.base.removed_children.push(ids().knob);
    register_variant(&mut world, b, &file_b);

    let library = world.resource::<PrefabLibrary>();
    assert_eq!(library.variant_chain(b), vec![b, a, base]);
    let resolved = library.resolve(b).expect("resolve");
    assert_eq!(resolved.template.asset, base);
    let steps: Vec<Guid> = resolved.chain.iter().map(|step| step.asset).collect();
    assert_eq!(steps, vec![a, b], "base-most first");

    let spawned = spawn(&mut world, b, Guid::from_u128(0xa3));
    assert_eq!(intensity(&world, spawned.locals[&ids().light]), 300.0, "B over A");
    assert!(!spawned.locals.contains_key(&ids().knob), "B removed the knob");
    assert_eq!(spawned.entities_spawned, 2);
    assert!(
        world
            .get::<Tags>(spawned.locals[&ids().root])
            .expect("A's tag survives B")
            .contains("from_a")
    );

    // The instance's own override wins over every step.
    let root = world.spawn(EntityGuid(Guid::from_u128(0xa4))).id();
    let mut record = PrefabInstanceData::plain(SceneAssetRef(b));
    record
        .overrides
        .push(ov(ids().light, LAMP, ".intensity", "400.0"));
    let own = instantiate_prefab(&mut world, &record, root, &SpawnOptions::default(), 0).expect("instance");
    assert_eq!(intensity(&world, own.locals[&ids().light]), 400.0);
}

#[test]
fn variant_added_children_are_prefab_internal_and_deterministic() {
    let extra = Guid::from_u128(0xe1);
    let (base, v) = (Guid::from_u128(0xb), Guid::from_u128(0xd));
    let text = format!(
        r#"(format_version: 1, base: (prefab: "{base}", added_children: [
            (guid: "{extra}", name: "Extra", parent: Some("{light}"), components: {{ "{LAMP}": (intensity: 5.0) }}),
        ]))"#,
        light = ids().light
    );
    let file = PrefabVariantFile::from_ron_str(&text).expect("parse");
    let mut guids = Vec::new();
    for _ in 0..2 {
        let mut world = world();
        register_base(&mut world, base, 100.0);
        register_variant(&mut world, v, &file);
        let root = world.spawn(EntityGuid(Guid::from_u128(0xa5))).id();
        let mut record = PrefabInstanceData::plain(SceneAssetRef(v));
        // The instance addresses the variant's added child by its file guid.
        record.overrides.push(ov(extra, LAMP, ".intensity", "9.0"));
        let spawned =
            instantiate_prefab(&mut world, &record, root, &SpawnOptions::default(), 0).expect("spawn");
        assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
        assert_eq!(spawned.entities_spawned, 4);
        let child = spawned.locals[&extra];
        assert_eq!(intensity(&world, child), 9.0);
        assert_eq!(world.get::<PrefabLink>(child).expect("internal").guid(), extra);
        assert_eq!(
            world.get::<ChildOf>(child).expect("parented").parent(),
            spawned.locals[&ids().light]
        );
        guids.push(world.get::<EntityGuid>(child).expect("guid").0);
    }
    assert_eq!(guids[0], guids[1], "same root guid, same derived guid");
    assert_ne!(guids[0], extra, "every instance gets its own guid");
}

#[test]
fn cycles_missing_bases_and_self_bases_are_reported_and_leave_nothing_behind() {
    let mut world = world();
    let (a, b, missing) = (Guid::from_u128(0xa), Guid::from_u128(0xbb), Guid::from_u128(0xf));
    let self_file = PrefabVariantFile::new(a);
    assert!(matches!(
        PrefabVariant::from_file(a, &self_file),
        Err(SceneSpawnError::Invalid(_))
    ));

    register_variant(&mut world, a, &PrefabVariantFile::new(b));
    register_variant(&mut world, b, &PrefabVariantFile::new(a));
    assert_eq!(world.resource::<PrefabLibrary>().first_missing(a), None);
    let root = world.spawn_empty().id();
    let err = instantiate_prefab(
        &mut world,
        &PrefabInstanceData::plain(SceneAssetRef(a)),
        root,
        &SpawnOptions::default(),
        0,
    )
    .expect_err("cycle");
    assert!(
        matches!(&err, SceneSpawnError::Invalid(m) if m.contains("cycle")),
        "{err}"
    );
    assert!(world.get::<Children>(root).is_none(), "nothing spawned");
    assert!(world.get::<PrefabInstance>(root).is_none());

    let c = Guid::from_u128(0xcc);
    register_variant(&mut world, c, &PrefabVariantFile::new(missing));
    assert_eq!(world.resource::<PrefabLibrary>().first_missing(c), Some(missing));
    let err = instantiate_prefab(
        &mut world,
        &PrefabInstanceData::plain(SceneAssetRef(c)),
        root,
        &SpawnOptions::default(),
        0,
    )
    .expect_err("missing base");
    assert!(matches!(err, SceneSpawnError::PrefabNotLoaded(id) if id == missing));
}

#[test]
fn a_chain_longer_than_the_limit_is_refused() {
    let mut world = world();
    let base = Guid::from_u128(0xb);
    register_base(&mut world, base, 1.0);
    let mut previous = base;
    for step in 0..=MAX_VARIANT_CHAIN as u128 {
        let id = Guid::from_u128(0x100 + step);
        register_variant(&mut world, id, &PrefabVariantFile::new(previous));
        previous = id;
    }
    let library = world.resource::<PrefabLibrary>();
    assert!(matches!(library.resolve(previous), Err(SceneSpawnError::Invalid(m)) if m.contains("longer")));
    // One step shorter resolves.
    let shorter = Guid::from_u128(0x100 + MAX_VARIANT_CHAIN as u128 - 1);
    assert_eq!(
        library.resolve(shorter).expect("resolves").chain.len(),
        MAX_VARIANT_CHAIN
    );
}

#[test]
fn dependents_and_registration_replace_by_id() {
    let mut world = world();
    let (base, a, b, other) = (
        Guid::from_u128(0xb),
        Guid::from_u128(0xa),
        Guid::from_u128(0xbb),
        Guid::from_u128(0xcc),
    );
    register_base(&mut world, base, 1.0);
    register_base(&mut world, other, 1.0);
    register_variant(&mut world, b, &PrefabVariantFile::new(a));
    register_variant(&mut world, a, &PrefabVariantFile::new(base));
    let library = world.resource::<PrefabLibrary>();
    assert_eq!(library.dependent_variants(base), vec![a, b]);
    assert_eq!(library.dependent_variants(a), vec![b]);
    assert!(library.dependent_variants(other).is_empty());
    assert!(library.contains(a) && library.get(a).is_none() && library.variant(a).is_some());

    // A template registered under a variant's id replaces it, and back.
    register_base(&mut world, a, 1.0);
    let library = world.resource::<PrefabLibrary>();
    assert!(library.get(a).is_some() && library.variant(a).is_none());
    assert!(
        library.dependent_variants(base).is_empty(),
        "a is no longer a variant of base"
    );
    register_variant(&mut world, a, &PrefabVariantFile::new(base));
    assert!(world.resource::<PrefabLibrary>().get(a).is_none());
}

#[test]
fn a_variant_nested_inside_a_prefab_spawns_with_its_edits() {
    let mut world = world();
    let (base, v, outer) = (Guid::from_u128(0xb), Guid::from_u128(0xd), Guid::from_u128(0x0e));
    register_base(&mut world, base, 100.0);
    let mut file = PrefabVariantFile::new(base);
    file.base
        .overrides
        .push(ov(ids().light, LAMP, ".intensity", "700.0"));
    register_variant(&mut world, v, &file);
    let holder = Guid::from_u128(0x2001);
    let text = format!(
        r#"(format_version: 1, entities: [
            (guid: "{holder}", name: "Holder", prefab_instance: Some((prefab: "{v}"))),
        ])"#
    );
    let outer_file = SceneFile::from_ron_str(&text).expect("outer");
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let template = PrefabTemplate::from_scene_file(outer, &outer_file, &registry.read()).expect("bake");
    world.resource_mut::<PrefabLibrary>().insert(template);
    let spawned = spawn(&mut world, outer, Guid::from_u128(0xa6));
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
    assert_eq!(intensity(&world, spawned.locals[&ids().light]), 700.0);
}

#[test]
fn a_level_instance_of_a_variant_saves_and_reloads_the_same() {
    use crate::spawn::{SnapshotFilter, snapshot_scene, spawn_scene};
    let (base, v) = (Guid::from_u128(0xb), Guid::from_u128(0xd));
    let mut file = PrefabVariantFile::new(base);
    file.base
        .overrides
        .push(ov(ids().light, LAMP, ".intensity", "300.0"));
    let level_guid = Guid::from_u128(0x3001);
    let text = format!(
        r#"(format_version: {version}, entities: [
            (guid: "{level_guid}", name: "RedLamp", components: {{ "regia_scene::transform::Transform": () }},
             prefab_instance: Some((prefab: "{v}", overrides: [
                (target: "{knob}", component: "{TAGS}", path: ".", value: (["mine"])),
             ]))),
        ])"#,
        version = crate::format::SCENE_FORMAT_VERSION,
        knob = ids().knob,
    );
    let level = SceneFile::from_ron_str(&text).expect("level");
    let mut saved = Vec::new();
    for _ in 0..2 {
        let mut world = world();
        register_base(&mut world, base, 100.0);
        register_variant(&mut world, v, &file);
        let source = saved.last().cloned().unwrap_or_else(|| level.clone());
        let spawned = spawn_scene(&mut world, &source, &SpawnOptions::default()).expect("spawn");
        assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
        let mut lamps = world.query::<&Lamp>();
        let values: Vec<f32> = lamps.iter(&world).map(|lamp| lamp.intensity).collect();
        assert_eq!(values, vec![300.0], "the variant's override, once");
        let mut tagged = world.query::<&Tags>();
        assert_eq!(
            tagged.iter(&world).filter(|tags| tags.contains("mine")).count(),
            1
        );
        let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
        assert_eq!(
            snapshot.entities.len(),
            1,
            "prefab-internal entities are not saved"
        );
        assert_eq!(
            snapshot.entities[0].prefab_instance,
            level.entities[0].prefab_instance
        );
        saved.push(snapshot);
    }
    assert_eq!(
        saved[0].to_ron_string().expect("first"),
        saved[1].to_ron_string().expect("second"),
        "save, load, save is stable"
    );
}

/// Spec 83 § 2.1: a missing chain step is `PrefabNotLoaded`, so a level instance of a variant
/// whose base is not registered yet waits as a `PendingPrefab` and fills in, with the variant's
/// edits and the instance's own, once the base arrives.
#[test]
fn a_level_instance_waits_for_a_missing_base_and_resolves_with_the_variant_edits() {
    use crate::prefab::{PendingPrefab, resolve_pending_prefabs};
    use crate::spawn::spawn_scene;
    let (base, v) = (Guid::from_u128(0xb), Guid::from_u128(0xd));
    let mut file = PrefabVariantFile::new(base);
    file.base
        .overrides
        .push(ov(ids().light, LAMP, ".intensity", "300.0"));
    let level_guid = Guid::from_u128(0x3002);
    let text = format!(
        r#"(format_version: {version}, entities: [
            (guid: "{level_guid}", name: "RedLamp", components: {{ "regia_scene::transform::Transform": () }},
             prefab_instance: Some((prefab: "{v}", overrides: [
                (target: "{knob}", component: "{TAGS}", path: ".", value: (["mine"])),
             ]))),
        ])"#,
        version = crate::format::SCENE_FORMAT_VERSION,
        knob = ids().knob,
    );
    let level = SceneFile::from_ron_str(&text).expect("level");
    let mut world = world();
    register_variant(&mut world, v, &file);
    assert_eq!(world.resource::<PrefabLibrary>().first_missing(v), Some(base));
    spawn_scene(&mut world, &level, &SpawnOptions::default()).expect("spawn");
    let mut pending = world.query::<(Entity, &PendingPrefab)>();
    let waiting: Vec<(Entity, u128)> = pending.iter(&world).map(|(e, p)| (e, p.prefab)).collect();
    assert_eq!(waiting.len(), 1, "one pending instance");
    assert_eq!(
        waiting[0].1,
        v.as_u128(),
        "it waits for the variant, not the base"
    );
    assert_eq!(
        world.query::<&Lamp>().iter(&world).count(),
        0,
        "nothing spawned yet"
    );

    // Still missing: the resolver leaves it pending.
    resolve_pending_prefabs(&mut world);
    assert!(world.get::<PendingPrefab>(waiting[0].0).is_some());

    register_base(&mut world, base, 100.0);
    resolve_pending_prefabs(&mut world);
    let root = waiting[0].0;
    assert!(world.get::<PendingPrefab>(root).is_none());
    assert_eq!(
        world.get::<PrefabInstance>(root).map(PrefabInstance::prefab_guid),
        Some(v),
        "the instance records the variant"
    );
    let values: Vec<f32> = world.query::<&Lamp>().iter(&world).map(|l| l.intensity).collect();
    assert_eq!(values, vec![300.0], "the variant's override applied once");
    assert_eq!(
        world
            .query::<&Tags>()
            .iter(&world)
            .filter(|tags| tags.contains("mine"))
            .count(),
        1,
        "the instance's own override applied"
    );
}

/// Spec 83 § 2.1 / § 6: an over-long chain and a missing base fail through `instantiate_prefab`
/// as well as through `resolve`, and leave nothing behind on the root.
#[test]
fn failed_variant_instantiations_leave_nothing_behind() {
    let mut world = world();
    let base = Guid::from_u128(0xb);
    register_base(&mut world, base, 1.0);
    let mut previous = base;
    for step in 0..=MAX_VARIANT_CHAIN as u128 {
        let id = Guid::from_u128(0x200 + step);
        register_variant(&mut world, id, &PrefabVariantFile::new(previous));
        previous = id;
    }
    let missing = Guid::from_u128(0xf);
    let orphan = Guid::from_u128(0xcc);
    register_variant(&mut world, orphan, &PrefabVariantFile::new(missing));
    let count = |world: &mut World| world.query::<Entity>().iter(world).count();
    let entities_before = count(&mut world);
    for (asset, want_invalid) in [(previous, true), (orphan, false)] {
        let root = world.spawn(EntityGuid(Guid::from_u128(0x9000))).id();
        let err = instantiate_prefab(
            &mut world,
            &PrefabInstanceData::plain(SceneAssetRef(asset)),
            root,
            &SpawnOptions::default(),
            0,
        )
        .expect_err("refused");
        if want_invalid {
            assert!(
                matches!(&err, SceneSpawnError::Invalid(m) if m.contains("longer")),
                "{err}"
            );
        } else {
            assert!(
                matches!(err, SceneSpawnError::PrefabNotLoaded(id) if id == missing),
                "{err}"
            );
        }
        assert!(
            world.get::<Children>(root).is_none(),
            "no children under the root"
        );
        assert!(world.get::<PrefabInstance>(root).is_none(), "no instance record");
        assert!(world.despawn(root));
        assert_eq!(count(&mut world), entities_before, "no stray entities");
    }
    assert_eq!(world.query::<&Lamp>().iter(&world).count(), 0);
}
