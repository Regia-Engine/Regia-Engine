// SPDX-License-Identifier: MPL-2.0
//! Level round trips through prefab instances (ADR-876): load → snapshot → load must keep what
//! the level authored on an instance — overrides, added and removed components, added children
//! (wherever they hang in the instance subtree) — and an instance whose template is still
//! loading must keep its record until it resolves.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::reflect::AppTypeRegistry;
use regia_scene::components::{EntityGuid, Name, Tags};
use regia_scene::cooked::CookedScene;
use regia_scene::prefab::{
    PendingPrefab, PrefabInstance, PrefabLibrary, PrefabLink, PrefabTemplate, resolve_pending_prefabs,
};
use regia_scene::prelude::*;
use regia_scene::spawn::{SceneSpawnError, SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};

/// Stand-in for a game component with a field to override (type path `round_trips::Glow`).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Glow {
    /// Candela.
    pub intensity: f32,
}

const GLOW: &str = "round_trips::Glow";
const TAGS: &str = "regia_scene::components::Tags";
const TRANSFORM: &str = "regia_scene::transform::Transform";

fn test_world() -> World {
    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        regia_scene::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
        registry.register::<EntityGuid>();
        registry.register::<Tags>();
        registry.register::<smallvec::SmallVec<[Name; 4]>>();
        registry.register::<Glow>();
    }
    world.init_resource::<PrefabLibrary>();
    world
}

fn bake(world: &mut World, asset: Guid, text: &str) {
    let file = SceneFile::from_ron_str(text).expect("parse prefab");
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
    world.resource_mut::<PrefabLibrary>().insert(template);
}

/// The local ids of the lamp prefab.
#[derive(Clone, Copy)]
struct Lamp {
    root: Guid,
    light: Guid,
    knob: Guid,
}

impl Lamp {
    fn new() -> Self {
        Self {
            root: Guid::new_random(),
            light: Guid::new_random(),
            knob: Guid::new_random(),
        }
    }

    /// Root (1,0,0) → light (0,2,0, tagged, glowing) → knob.
    fn text(&self) -> String {
        let Self { root, light, knob } = self;
        format!(
            r#"(format_version: 10, entities: [
    (guid: "{root}", name: "Lamp", components: {{ "{TRANSFORM}": (translation: (1.0, 0.0, 0.0)) }}),
    (guid: "{light}", name: "Light", parent: "{root}", components: {{
        "{TRANSFORM}": (translation: (0.0, 2.0, 0.0)),
        "{TAGS}": (["lamp"]),
        "{GLOW}": (intensity: 100.0),
    }}),
    (guid: "{knob}", name: "Knob", parent: "{light}", components: {{ "{TRANSFORM}": () }}),
])"#
        )
    }
}

/// `file` with its entity records (and every instance's added children) sorted by guid.
fn sorted(mut file: SceneFile) -> SceneFile {
    fn sort(entities: &mut [SceneEntity]) {
        entities.sort_by_key(|entity| entity.guid);
        for entity in entities {
            if let Some(instance) = entity.prefab_instance.as_mut() {
                sort(&mut instance.added_children);
            }
        }
    }
    sort(&mut file.entities);
    file
}

fn entity_by_guid(world: &mut World, guid: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(entity, _)| entity)
}

fn entity_by_local(world: &mut World, local: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &PrefabLink)>()
        .iter(world)
        .find(|(_, link)| link.guid() == local)
        .map(|(entity, _)| entity)
}

fn world_translation(world: &World, entity: Entity) -> Vec3 {
    world
        .get::<GlobalTransform>(entity)
        .expect("global transform")
        .translation()
}

fn parent_of(world: &World, entity: Entity) -> Option<Entity> {
    world.get::<ChildOf>(entity).map(|link| link.parent())
}

/// A level with one lamp instance that edits everything an instance record can: a field
/// override, an added and a removed component, two added children chained under a prefab-internal
/// entity, and one added child directly under the instance root.
fn edited_level_text(asset: Guid, lamp: Lamp, ids: [Guid; 4]) -> String {
    let [instance, badge, pin, sign] = ids;
    let Lamp { light, knob, .. } = lamp;
    format!(
        r#"(format_version: 10, entities: [
    (guid: "{instance}", name: "Lamp A", components: {{ "{TRANSFORM}": (translation: (10.0, 0.0, 0.0)) }},
     prefab_instance: (
        prefab: "asset:{asset}",
        overrides: [(target: "{light}", component: "{GLOW}", path: ".intensity", value: 800.0)],
        added_components: [(target: "{knob}", component: "{TAGS}", value: (["chromed"]))],
        removed_components: [(target: "{light}", component: "{TAGS}")],
        added_children: [
            (guid: "{badge}", name: "Badge", parent: "{light}", components: {{ "{TRANSFORM}": (translation: (0.0, 0.0, 1.0)) }}),
            (guid: "{pin}", name: "Badge Pin", parent: "{badge}", components: {{ "{TRANSFORM}": (translation: (0.0, 0.0, 1.0)) }}),
            (guid: "{sign}", name: "Sign", components: {{ "{TRANSFORM}": (translation: (0.0, 5.0, 0.0)) }}),
        ],
     )),
])"#
    )
}

/// Checks the spawned world of [`edited_level_text`]: every edit landed where it was authored.
fn assert_edits_landed(world: &mut World, lamp: Lamp, ids: [Guid; 4]) {
    let [instance, badge, pin, sign] = ids;
    let light = entity_by_local(world, lamp.light).expect("light");
    let knob = entity_by_local(world, lamp.knob).expect("knob");
    assert_eq!(world.get::<Glow>(light).map(|g| g.intensity), Some(800.0));
    assert!(
        world.get::<Tags>(light).is_none(),
        "removed component stays removed"
    );
    assert!(world.get::<Tags>(knob).is_some_and(|t| t.contains("chromed")));

    let instance = entity_by_guid(world, instance).expect("instance root");
    let badge = entity_by_guid(world, badge).expect("badge");
    let pin = entity_by_guid(world, pin).expect("pin");
    let sign = entity_by_guid(world, sign).expect("sign");
    assert_eq!(
        parent_of(world, badge),
        Some(light),
        "badge hangs under the light"
    );
    assert_eq!(parent_of(world, pin), Some(badge), "pin hangs under the badge");
    assert_eq!(
        parent_of(world, sign),
        Some(instance),
        "sign hangs under the instance root"
    );

    regia_scene::propagate::propagate_transforms(world);
    // Instance (10,0,0) + lamp root (1,0,0) + light (0,2,0) + badge (0,0,1) + pin (0,0,1).
    assert!((world_translation(world, pin) - Vec3::new(11.0, 2.0, 2.0)).length() < 1e-5);
    assert!((world_translation(world, sign) - Vec3::new(10.0, 5.0, 0.0)).length() < 1e-5);
}

/// Before ADR-876 the snapshot dropped `added_components`/`removed_components` and wrote the
/// badge and pin as stray roots (their parent is prefab-internal), so a re-save lost the edits.
#[test]
fn instance_edits_and_added_children_survive_load_save_load() {
    let lamp = Lamp::new();
    let asset = Guid::new_random();
    let ids = [
        Guid::new_random(),
        Guid::new_random(),
        Guid::new_random(),
        Guid::new_random(),
    ];
    let [instance_id, badge_id, pin_id, sign_id] = ids;
    let level = SceneFile::from_ron_str(&edited_level_text(asset, lamp, ids)).expect("parse level");

    let mut world = test_world();
    bake(&mut world, asset, &lamp.text());
    let spawned = spawn_scene(&mut world, &level, &SpawnOptions::default()).expect("spawn");
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
    assert_edits_landed(&mut world, lamp, ids);

    let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
    let text = snapshot.to_ron_string().expect("write");
    // The instance root and the sign (a plain child of the root) are top-level; the badge and pin
    // live in the instance record, addressed through the light's local id.
    let top: Vec<Guid> = snapshot.entities.iter().map(|e| e.guid).collect();
    assert_eq!(top.len(), 2, "{text}");
    assert!(top.contains(&instance_id) && top.contains(&sign_id), "{text}");
    let record = snapshot
        .entities
        .iter()
        .find(|e| e.guid == instance_id)
        .and_then(|e| e.prefab_instance.as_ref())
        .expect("instance record");
    assert_eq!(record.overrides.len(), 1, "{text}");
    assert_eq!(record.added_components.len(), 1, "{text}");
    assert_eq!(record.added_components[0].target, lamp.knob);
    assert_eq!(record.removed_components.len(), 1, "{text}");
    assert_eq!(record.removed_components[0].target, lamp.light);
    let added: Vec<(Guid, Option<Guid>)> = record
        .added_children
        .iter()
        .map(|child| (child.guid, child.parent))
        .collect();
    assert_eq!(added.len(), 2, "{text}");
    assert!(added.contains(&(badge_id, Some(lamp.light))), "{text}");
    assert!(added.contains(&(pin_id, Some(badge_id))), "{text}");

    // Load the snapshot into a fresh world: the same edits, and the same file on re-save (record
    // order follows the world's query order, so compare canonically sorted files).
    let reparsed = SceneFile::from_ron_str(&text).expect("reparse");
    let mut world2 = test_world();
    bake(&mut world2, asset, &lamp.text());
    let spawned2 = spawn_scene(&mut world2, &reparsed, &SpawnOptions::default()).expect("respawn");
    assert!(spawned2.warnings.is_empty(), "{:?}", spawned2.warnings);
    assert_edits_landed(&mut world2, lamp, ids);
    let snapshot2 = snapshot_scene(&mut world2, &SnapshotFilter::default()).expect("snapshot 2");
    assert_eq!(
        sorted(snapshot2).to_ron_string().expect("write 2"),
        sorted(snapshot).to_ron_string().expect("write 1"),
        "the round trip is stable"
    );
}

/// The cooked path keeps the same record (it is cooked structurally) and spawns the same world.
#[test]
fn cooked_instance_edits_land_like_the_text_path() {
    let lamp = Lamp::new();
    let asset = Guid::new_random();
    let ids = [
        Guid::new_random(),
        Guid::new_random(),
        Guid::new_random(),
        Guid::new_random(),
    ];
    let level = SceneFile::from_ron_str(&edited_level_text(asset, lamp, ids)).expect("parse level");
    let mut world = test_world();
    bake(&mut world, asset, &lamp.text());
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let (cooked, _) = CookedScene::cook(&level, &registry.read()).expect("cook");
    let bytes = cooked.to_bytes().expect("bytes");
    let cooked = CookedScene::from_bytes(&bytes).expect("parse cooked");
    cooked
        .spawn(&mut world, &SpawnOptions::default())
        .expect("spawn cooked");
    assert_edits_landed(&mut world, lamp, ids);
}

/// Added children authored inside a prefab (a nested instance's record) belong to the template:
/// they are prefab-internal (linked, fresh `EntityGuid` per instance), addressable by the level's
/// overrides, and never written into the level. Before ADR-876 two instances of such a prefab
/// shared one `EntityGuid`, and the snapshot failed validation with duplicate guids.
#[test]
fn template_added_children_are_prefab_internal() {
    let lamp = Lamp::new();
    let lamp_asset = Guid::new_random();
    let room_asset = Guid::new_random();
    let room_root = Guid::new_random();
    let shade = Guid::new_random();
    let room_text = format!(
        r#"(format_version: 10, entities: [
    (guid: "{room_root}", name: "Room", components: {{ "{TRANSFORM}": () }}, prefab_instance: (
        prefab: "asset:{lamp_asset}",
        added_children: [
            (guid: "{shade}", name: "Shade", parent: "{light}", components: {{ "{TRANSFORM}": (translation: (0.0, 1.0, 0.0)) }}),
        ],
    )),
])"#,
        light = lamp.light,
    );
    let (yard_a, yard_b) = (Guid::new_random(), Guid::new_random());
    let level = SceneFile::from_ron_str(&format!(
        r#"(format_version: 10, entities: [
    (guid: "{yard_a}", name: "Yard A", prefab_instance: (
        prefab: "asset:{room_asset}",
        overrides: [(target: "{shade}", component: "{TAGS}", path: ".", value: (["tinted"]))],
    )),
    (guid: "{yard_b}", name: "Yard B", prefab_instance: (prefab: "asset:{room_asset}")),
])"#
    ))
    .expect("parse level");

    let mut world = test_world();
    bake(&mut world, lamp_asset, &lamp.text());
    bake(&mut world, room_asset, &room_text);
    let spawned = spawn_scene(&mut world, &level, &SpawnOptions::default()).expect("spawn");
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);

    let shades: Vec<(Entity, Guid)> = world
        .query::<(Entity, &PrefabLink, &EntityGuid)>()
        .iter(&world)
        .filter(|(_, link, _)| link.guid() == shade)
        .map(|(entity, _, guid)| (entity, guid.0))
        .collect();
    assert_eq!(shades.len(), 2, "one shade per instance, both prefab-internal");
    assert_ne!(shades[0].1, shades[1].1, "each instance has its own EntityGuid");
    let tinted = shades
        .iter()
        .filter(|(entity, _)| world.get::<Tags>(*entity).is_some_and(|t| t.contains("tinted")))
        .count();
    assert_eq!(tinted, 1, "the level's override reached yard A's shade only");

    let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
    let guids: Vec<Guid> = snapshot.entities.iter().map(|e| e.guid).collect();
    assert_eq!(guids.len(), 2, "only the two yards are level entities: {guids:?}");
    assert!(
        snapshot.entities.iter().all(|e| e
            .prefab_instance
            .as_ref()
            .is_some_and(|i| i.added_children.is_empty())),
        "template children are not written into the level"
    );
}

/// A level spawned before its prefab finished loading: the pending instance keeps the authored
/// record, a snapshot taken meanwhile writes it back unchanged, and the resolved instance has the
/// overrides. Before ADR-876 the pending entity kept only the prefab id: the overrides were lost
/// on resolve and the snapshot wrote no `prefab_instance` at all.
#[test]
fn pending_instances_keep_their_record() {
    let lamp = Lamp::new();
    let asset = Guid::new_random();
    let ids = [
        Guid::new_random(),
        Guid::new_random(),
        Guid::new_random(),
        Guid::new_random(),
    ];
    let level = SceneFile::from_ron_str(&edited_level_text(asset, lamp, ids)).expect("parse level");

    let mut world = test_world();
    let spawned = spawn_scene(&mut world, &level, &SpawnOptions::default()).expect("spawn");
    assert!(
        spawned.warnings.iter().any(|w| w.contains("pending")),
        "{:?}",
        spawned.warnings
    );
    let instance = entity_by_guid(&mut world, ids[0]).expect("instance root");
    let pending = world.get::<PendingPrefab>(instance).expect("pending").clone();
    assert_eq!(pending.prefab, asset.as_u128());
    assert_eq!(
        pending.instance_data().expect("record parses"),
        level.entities[0]
            .prefab_instance
            .clone()
            .expect("authored record")
    );

    // Snapshot while pending: the record is written back as authored.
    let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
    assert_eq!(snapshot.entities.len(), 1);
    assert_eq!(
        snapshot.entities[0].prefab_instance,
        level.entities[0].prefab_instance
    );

    // Not loaded yet: resolving is a no-op.
    resolve_pending_prefabs(&mut world);
    assert!(world.get::<PendingPrefab>(instance).is_some());

    bake(&mut world, asset, &lamp.text());
    resolve_pending_prefabs(&mut world);
    assert!(world.get::<PendingPrefab>(instance).is_none());
    assert!(world.get::<PrefabInstance>(instance).is_some());
    assert_edits_landed(&mut world, lamp, ids);
}

/// An instance of a prefab whose *nested* prefab is missing waits for the outer prefab (the one
/// the level names) and leaves nothing half-built. Before ADR-876 the entity waited for the
/// inner prefab instead (and would have instantiated it in place of the outer one), next to the
/// partially spawned outer subtree.
#[test]
fn a_missing_nested_prefab_keeps_the_outer_instance_pending() {
    let lamp = Lamp::new();
    let lamp_asset = Guid::new_random();
    let room_asset = Guid::new_random();
    let room_root = Guid::new_random();
    let room_text = format!(
        r#"(format_version: 10, entities: [
    (guid: "{room_root}", name: "Room", components: {{ "{TRANSFORM}": () }},
     prefab_instance: (prefab: "asset:{lamp_asset}")),
])"#
    );
    let yard = Guid::new_random();
    let level = SceneFile::from_ron_str(&format!(
        r#"(format_version: 10, entities: [
    (guid: "{yard}", name: "Yard", prefab_instance: (
        prefab: "asset:{room_asset}",
        overrides: [(target: "{light}", component: "{GLOW}", path: ".intensity", value: 5.0)],
    )),
])"#,
        light = lamp.light,
    ))
    .expect("parse level");

    let mut world = test_world();
    bake(&mut world, room_asset, &room_text);
    spawn_scene(&mut world, &level, &SpawnOptions::default()).expect("spawn");
    let yard_entity = entity_by_guid(&mut world, yard).expect("yard");
    let pending = world.get::<PendingPrefab>(yard_entity).expect("pending");
    assert_eq!(
        pending.prefab,
        room_asset.as_u128(),
        "waits for the prefab the level names"
    );
    let links = world.query::<&PrefabLink>().iter(&world).count();
    assert_eq!(links, 0, "nothing half-built");
    assert_eq!(world.get::<Children>(yard_entity).map(|c| c.len()), None);

    // The outer template is loaded but the inner is not: still pending, still nothing built.
    resolve_pending_prefabs(&mut world);
    assert!(world.get::<PendingPrefab>(yard_entity).is_some());
    assert_eq!(world.query::<&PrefabLink>().iter(&world).count(), 0);

    bake(&mut world, lamp_asset, &lamp.text());
    resolve_pending_prefabs(&mut world);
    assert!(world.get::<PendingPrefab>(yard_entity).is_none());
    let light = entity_by_local(&mut world, lamp.light).expect("light");
    assert_eq!(world.get::<Glow>(light).map(|g| g.intensity), Some(5.0));
    assert_eq!(
        world.query::<&PrefabLink>().iter(&world).count(),
        4,
        "room + lamp + light + knob"
    );
}

/// `PrefabLibrary::first_missing` names the first unregistered template of a nesting and
/// terminates on a nesting cycle.
#[test]
fn first_missing_walks_the_nesting() {
    let lamp = Lamp::new();
    let lamp_asset = Guid::new_random();
    let room_asset = Guid::new_random();
    let room_text = format!(
        r#"(format_version: 10, entities: [
    (guid: "{}", prefab_instance: (prefab: "asset:{lamp_asset}")),
])"#,
        Guid::new_random()
    );
    let mut world = test_world();
    bake(&mut world, room_asset, &room_text);
    let library = world.resource::<PrefabLibrary>();
    assert_eq!(library.first_missing(room_asset), Some(lamp_asset));
    assert_eq!(
        library.first_missing(Guid::from_u128(9)),
        Some(Guid::from_u128(9))
    );

    bake(&mut world, lamp_asset, &lamp.text());
    assert_eq!(world.resource::<PrefabLibrary>().first_missing(room_asset), None);

    // A prefab that (wrongly) nests itself: the walk terminates.
    let selfish = Guid::new_random();
    let selfish_text = format!(
        r#"(format_version: 10, entities: [
    (guid: "{}", prefab_instance: (prefab: "asset:{selfish}")),
])"#,
        Guid::new_random()
    );
    bake(&mut world, selfish, &selfish_text);
    assert_eq!(world.resource::<PrefabLibrary>().first_missing(selfish), None);
}

/// The identity a prefab instantiation derives: the root's guid plus every prefab-internal
/// `(local id, guid)` sorted by local id — what two lockstep peers must agree on after running
/// the same spawn.
fn derived_identity(world: &mut World, root: Entity) -> (Option<Guid>, Vec<(Guid, Guid)>) {
    let root_guid = world.get::<EntityGuid>(root).map(|g| g.0);
    let mut members: Vec<(Guid, Guid)> = world
        .query::<(&PrefabLink, &EntityGuid)>()
        .iter(world)
        .map(|(link, guid)| (link.guid(), guid.0))
        .collect();
    members.sort();
    (root_guid, members)
}

/// Finding 1 (scene determinism audit, high): two peers that run the same `spawn_prefab` call
/// with the same caller-supplied root guid (a sim-RNG draw) on the same step derive the same
/// member guids. The root guid used to come from OS entropy, so the derived member guids (and
/// any snapshot keyed by them) differed byte for byte between the peers.
#[test]
fn runtime_spawn_prefab_is_deterministic_for_lockstep_peers() {
    let lamp = Lamp::new();
    let asset = Guid::from_u128(0xf1);
    let root_guid = Guid::from_u128(0x5eed);
    let mut a = test_world();
    bake(&mut a, asset, &lamp.text());
    let mut b = test_world();
    bake(&mut b, asset, &lamp.text());
    let opts = SpawnOptions::default();
    let root_a = a.spawn_prefab(asset, None, &opts, root_guid).expect("spawn a");
    let root_b = b.spawn_prefab(asset, None, &opts, root_guid).expect("spawn b");
    assert_eq!(
        derived_identity(&mut a, root_a),
        derived_identity(&mut b, root_b),
        "same inputs on the same step must derive the same guids on both peers"
    );

    // A different root guid is a different instance: its members derive different guids.
    let mut c = test_world();
    bake(&mut c, asset, &lamp.text());
    let root_c = c
        .spawn_prefab(asset, None, &opts, Guid::from_u128(0x5eee))
        .expect("spawn c");
    assert_ne!(
        derived_identity(&mut a, root_a),
        derived_identity(&mut c, root_c),
        "a different root guid must derive different member guids"
    );

    // The caller-owned guid is validated: nil and duplicate roots are refused.
    let err = a
        .spawn_prefab(asset, None, &opts, Guid::NIL)
        .expect_err("nil root guid is refused");
    assert!(matches!(err, SceneSpawnError::Invalid(_)), "unexpected: {err}");
    let err = a
        .spawn_prefab(asset, None, &opts, root_guid)
        .expect_err("a guid already in the world is refused");
    assert!(matches!(err, SceneSpawnError::Invalid(_)), "unexpected: {err}");
}

/// Finding 1 (scene determinism audit, high): the template loop and template-owned added
/// children used to mint OS-random guids when the instance root carried no `EntityGuid`. Both
/// paths share the root, so the refusal is enforced once, before anything spawns.
#[test]
fn instantiate_prefab_refuses_a_root_without_a_guid() {
    use regia_scene::prefab::instantiate_prefab;
    let lamp = Lamp::new();
    let asset = Guid::from_u128(0xf2);
    let mut world = test_world();
    bake(&mut world, asset, &lamp.text());
    let root = world.spawn_empty().id();
    let instance = PrefabInstanceData::plain(regia_scene::format::SceneAssetRef(asset));
    let err = instantiate_prefab(&mut world, &instance, root, &SpawnOptions::default(), 0)
        .expect_err("a root without EntityGuid is refused");
    assert!(
        matches!(err, SceneSpawnError::Invalid(_)),
        "unexpected error: {err}"
    );
    assert!(
        world.get::<PrefabInstance>(root).is_none(),
        "a failed instantiation leaves no PrefabInstance behind"
    );
    let children = world
        .get::<Children>(root)
        .map_or(0, |children| children.iter().count());
    assert_eq!(children, 0, "a failed instantiation leaves nothing behind");
}
