// SPDX-License-Identifier: MPL-2.0
//! Stable entity references in saves (ADR-3239, spec 91 § 10): acceptance tests.
//!
//! Every test saves a small world to a slot under `unique_temp_dir`, loads it into a fresh app
//! and checks that each reference points at the right *new* entity, found by its `Tag`.

use regia_app::prelude::*;
use regia_ecs::ecs::entity::{EntityMapper, MapEntities};
use regia_ecs::ecs::hierarchy::ChildOf;
use regia_ecs::reflect::{ReflectMapEntities, ReflectMut};
use regia_framework::save_entity_refs::{
    SAVE_ENTITY_IDS_INVALID, SAVE_ENTITY_REF_CLEARED, SAVE_ENTITY_REF_MISSING, SAVE_ENTITY_REF_OUT_OF_RANGE,
    SAVE_ENTITY_REF_UNMAPPED, SAVE_ENTITY_REF_UNSAVED, SaveEntityId, SaveEntityRefOp, SaveEntityRefReport,
    inspect_refs,
};
use regia_framework::save_game::{
    SaveError, SaveFileRaw, SaveGame, SavePlugin, SaveSlot, SaveSpec, Saveable,
};
use regia_scene::components::EntityGuid;
use regia_test_utils::unique_temp_dir;

/// A name to find an entity again after a load.
#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct Tag(String);

/// A pawn's AI target (plain reference).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, MapEntities)]
struct AiTarget {
    target: Entity,
}

impl MapEntities for AiTarget {
    fn map_entities<M: EntityMapper>(&mut self, mapper: &mut M) {
        self.target = mapper.get_mapped(self.target);
    }
}

/// An item and who carries it (optional reference).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, MapEntities)]
struct Carried {
    by: Option<Entity>,
}

impl MapEntities for Carried {
    fn map_entities<M: EntityMapper>(&mut self, mapper: &mut M) {
        if let Some(by) = &mut self.by {
            *by = mapper.get_mapped(*by);
        }
    }
}

/// A seat of a vehicle.
#[derive(Reflect, Clone, Debug, PartialEq)]
struct Seat {
    name: String,
    rider: Option<Entity>,
}

/// A vehicle with its seats (references in a list of nested structs) and its owner.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, MapEntities)]
struct Vehicle {
    seats: Vec<Seat>,
    owner: Entity,
    mirrors: [Option<Entity>; 2],
}

impl MapEntities for Vehicle {
    fn map_entities<M: EntityMapper>(&mut self, mapper: &mut M) {
        for seat in &mut self.seats {
            if let Some(rider) = &mut seat.rider {
                *rider = mapper.get_mapped(*rider);
            }
        }
        self.owner = mapper.get_mapped(self.owner);
    }
}

/// A link in a chain (tuple struct).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, MapEntities)]
struct Follows(Entity);

impl MapEntities for Follows {
    fn map_entities<M: EntityMapper>(&mut self, mapper: &mut M) {
        self.0 = mapper.get_mapped(self.0);
    }
}

/// Holds an entity but does not register `MapEntities`: never saved.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
struct Unmapped {
    other: Entity,
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SavePlugin::default()))
        .register_type::<Tag>()
        .register_type::<AiTarget>()
        .register_type::<Carried>()
        .register_type::<Vehicle>()
        .register_type::<Follows>()
        .register_type::<Unmapped>()
        .register_type::<EntityGuid>()
        .register_type::<ChildOf>();
    app
}

fn slot(dir: &std::path::Path, name: &str) -> SaveSlot {
    SaveSlot {
        name: name.into(),
        path: dir.join(format!("{name}.save")),
        header: None,
    }
}

fn tagged(world: &mut World, tag: &str) -> Entity {
    let mut query = world.query::<(Entity, &Tag)>();
    let found: Vec<Entity> = query
        .iter(world)
        .filter(|(_, t)| t.0 == tag)
        .map(|(entity, _)| entity)
        .collect();
    assert_eq!(found.len(), 1, "exactly one entity tagged {tag:?}: {found:?}");
    found[0]
}

fn tag(s: &str) -> Tag {
    Tag(s.to_string())
}

fn report(world: &World) -> SaveEntityRefReport {
    world
        .get_resource::<SaveEntityRefReport>()
        .cloned()
        .expect("every save and load writes a report")
}

/// Saves `source` to a fresh slot and loads it into a fresh app (after spawning a few unrelated
/// entities there, so new ids cannot equal the old ones by accident).
fn round_trip(source: &mut App, name: &str) -> App {
    let dir = unique_temp_dir(&format!("save-refs-{name}"));
    let slot = slot(&dir, name);
    SaveGame::save(&slot, source.world_mut(), &SaveSpec::default()).expect("save");
    let mut loaded = app();
    for _ in 0..7 {
        loaded.world_mut().spawn(Tag("unrelated".into()));
    }
    SaveGame::load(&slot, loaded.world_mut(), &SaveSpec::default()).expect("load");
    let _ = std::fs::remove_dir_all(&dir);
    loaded
}

#[test]
fn pawn_target_carried_item_vehicle_riders_and_a_chain_survive() {
    let mut source = app();
    let world = source.world_mut();
    let enemy = world.spawn((Saveable, tag("enemy"))).id();
    let _pawn = world
        .spawn((Saveable, tag("pawn"), AiTarget { target: enemy }))
        .id();
    let hero = world.spawn((Saveable, tag("hero"))).id();
    let _sword = world
        .spawn((Saveable, tag("sword"), Carried { by: Some(hero) }))
        .id();
    let rider_a = world.spawn((Saveable, tag("rider_a"))).id();
    let rider_b = world.spawn((Saveable, tag("rider_b"))).id();
    world.spawn((
        Saveable,
        tag("truck"),
        Vehicle {
            seats: vec![
                Seat {
                    name: "front".into(),
                    rider: Some(rider_a),
                },
                Seat {
                    name: "middle".into(),
                    rider: None,
                },
                Seat {
                    name: "back".into(),
                    rider: Some(rider_b),
                },
            ],
            owner: hero,
            mirrors: [Some(rider_a), None],
        },
    ));
    // a → b → c → d: three references.
    let d = world.spawn((Saveable, tag("d"))).id();
    let c = world.spawn((Saveable, tag("c"), Follows(d))).id();
    let b = world.spawn((Saveable, tag("b"), Follows(c))).id();
    world.spawn((Saveable, tag("a"), Follows(b)));

    let mut loaded = round_trip(&mut source, "links");
    let saved = report(source.world());
    assert_eq!(
        (saved.op, saved.notes.len()),
        (SaveEntityRefOp::Save, 0),
        "{saved:?}"
    );
    let world = loaded.world_mut();
    let enemy = tagged(world, "enemy");
    let pawn = tagged(world, "pawn");
    assert_eq!(world.get::<AiTarget>(pawn), Some(&AiTarget { target: enemy }));
    let hero = tagged(world, "hero");
    let sword = tagged(world, "sword");
    assert_eq!(world.get::<Carried>(sword), Some(&Carried { by: Some(hero) }));
    let (rider_a, rider_b) = (tagged(world, "rider_a"), tagged(world, "rider_b"));
    let truck = tagged(world, "truck");
    let vehicle = world.get::<Vehicle>(truck).expect("vehicle loads").clone();
    let riders: Vec<Option<Entity>> = vehicle.seats.iter().map(|s| s.rider).collect();
    assert_eq!(riders, vec![Some(rider_a), None, Some(rider_b)]);
    assert_eq!(vehicle.seats[1].name, "middle");
    assert_eq!(vehicle.owner, hero);
    assert_eq!(vehicle.mirrors, [Some(rider_a), None]);
    let (a, b, c, d) = (
        tagged(world, "a"),
        tagged(world, "b"),
        tagged(world, "c"),
        tagged(world, "d"),
    );
    assert_eq!(world.get::<Follows>(a), Some(&Follows(b)));
    assert_eq!(world.get::<Follows>(b), Some(&Follows(c)));
    assert_eq!(world.get::<Follows>(c), Some(&Follows(d)));
    let r = report(world);
    assert_eq!((r.op, r.notes.len()), (SaveEntityRefOp::Load, 0), "{r:?}");
}

#[test]
fn a_reference_to_an_unsaved_entity_becomes_none_or_is_reported() {
    let mut source = app();
    let world = source.world_mut();
    let camera = world.spawn(tag("camera_not_saved")).id();
    world.spawn((Saveable, tag("item"), Carried { by: Some(camera) }));
    world.spawn((Saveable, tag("pawn"), AiTarget { target: camera }));

    let dir = unique_temp_dir("save-refs-unsaved");
    let slot = slot(&dir, "unsaved");
    SaveGame::save(&slot, source.world_mut(), &SaveSpec::default()).expect("save");
    let notes = report(source.world()).notes;
    let codes: Vec<(&str, &str, &str)> = notes
        .iter()
        .map(|n| (n.code, n.component.as_str(), n.field.as_str()))
        .collect();
    assert!(
        codes.contains(&(SAVE_ENTITY_REF_CLEARED, "Carried", "by")),
        "{codes:?}"
    );
    assert!(
        codes.contains(&(SAVE_ENTITY_REF_UNSAVED, "AiTarget", "target")),
        "{codes:?}"
    );

    let mut loaded = app();
    SaveGame::load(&slot, loaded.world_mut(), &SaveSpec::default()).expect("load");
    let world = loaded.world_mut();
    let item = tagged(world, "item");
    assert_eq!(world.get::<Carried>(item), Some(&Carried { by: None }));
    let pawn = tagged(world, "pawn");
    assert!(
        world.get::<AiTarget>(pawn).is_none(),
        "the plain reference was not saved"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A level entity (with `EntityGuid`, not saved) is referenced by guid and found again in the
/// loaded level; when a content update removed it, the reference is reported and nothing dangles.
#[test]
fn level_entities_resolve_by_guid_and_a_missing_one_is_reported() {
    let door_guid = EntityGuid(regia_core::id::Guid(0x00d0_0d00));
    let mut source = app();
    let world = source.world_mut();
    let door = world.spawn((tag("door"), door_guid)).id();
    world.spawn((Saveable, tag("guard"), AiTarget { target: door }));
    world.spawn((Saveable, tag("key"), Carried { by: Some(door) }));

    let dir = unique_temp_dir("save-refs-level");
    let slot = slot(&dir, "level");
    SaveGame::save(&slot, source.world_mut(), &SaveSpec::default()).expect("save");
    let raw = SaveGame::load_raw(&slot, source.world_mut()).expect("raw");
    assert!(
        raw.entity_ids.contains(&SaveEntityId::Level {
            guid: 0x00d0_0d00,
            level: 0
        }),
        "the door is an external entry: {:?}",
        raw.entity_ids
    );

    // The level is loaded again (new entity ids), then the save.
    let mut loaded = app();
    for _ in 0..3 {
        loaded.world_mut().spawn(tag("filler"));
    }
    let new_door = loaded.world_mut().spawn((tag("door"), door_guid)).id();
    SaveGame::load(&slot, loaded.world_mut(), &SaveSpec::default()).expect("load");
    let world = loaded.world_mut();
    let guard = tagged(world, "guard");
    assert_eq!(world.get::<AiTarget>(guard), Some(&AiTarget { target: new_door }));
    let key = tagged(world, "key");
    assert_eq!(world.get::<Carried>(key), Some(&Carried { by: Some(new_door) }));

    // A content update removed the door.
    let mut updated = app();
    SaveGame::load(&slot, updated.world_mut(), &SaveSpec::default()).expect("load without the door");
    let world = updated.world_mut();
    let guard = tagged(world, "guard");
    assert!(world.get::<AiTarget>(guard).is_none(), "skipped, not dangling");
    let key = tagged(world, "key");
    assert_eq!(world.get::<Carried>(key), Some(&Carried { by: None }));
    let notes = report(world).notes;
    assert!(
        notes
            .iter()
            .any(|n| n.code == SAVE_ENTITY_REF_MISSING && n.component == "AiTarget" && n.field == "target"),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.code == SAVE_ENTITY_REF_CLEARED && n.component == "Carried")
    );
    // No live component anywhere points at a dead entity.
    let mut query = world.query::<&AiTarget>();
    let targets: Vec<Entity> = query.iter(world).map(|t| t.target).collect();
    for target in targets {
        assert!(world.get_entity(target).is_ok(), "dangling {target:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_type_without_map_entities_is_not_saved_and_is_reported() {
    let mut source = app();
    let world = source.world_mut();
    let other = world.spawn((Saveable, tag("other"))).id();
    world.spawn((Saveable, tag("holder"), Unmapped { other }));
    let mut loaded = round_trip(&mut source, "unmapped");
    let notes = report(source.world()).notes;
    assert!(
        notes
            .iter()
            .any(|n| n.code == SAVE_ENTITY_REF_UNMAPPED && n.component == "Unmapped"),
        "{notes:?}"
    );
    let holder = tagged(loaded.world_mut(), "holder");
    assert!(loaded.world().get::<Unmapped>(holder).is_none());
}

#[test]
fn a_hierarchy_survives_through_child_of() {
    let mut source = app();
    let world = source.world_mut();
    let parent = world.spawn((Saveable, tag("parent"))).id();
    world.spawn((Saveable, tag("child"), ChildOf(parent)));
    let mut loaded = round_trip(&mut source, "hierarchy");
    let world = loaded.world_mut();
    let (parent, child) = (tagged(world, "parent"), tagged(world, "child"));
    assert_eq!(world.get::<ChildOf>(child).map(ChildOf::parent), Some(parent));
    let children: Vec<Entity> = world
        .get::<regia_ecs::ecs::hierarchy::Children>(parent)
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    assert_eq!(children, vec![child], "Children is rebuilt from ChildOf, once");
}

fn build_world(app: &mut App) {
    let world = app.world_mut();
    let door = world
        .spawn((tag("door"), EntityGuid(regia_core::id::Guid(0x77))))
        .id();
    let level_crate = world
        .spawn((Saveable, tag("crate"), EntityGuid(regia_core::id::Guid(0x42))))
        .id();
    let hero = world
        .spawn((Saveable, tag("hero"), AiTarget { target: door }))
        .id();
    world.spawn((Saveable, tag("sword"), Carried { by: Some(hero) }));
    world.spawn((Saveable, tag("box"), Follows(level_crate)));
}

#[test]
fn two_saves_of_the_same_world_are_byte_identical() {
    let dir = unique_temp_dir("save-refs-bytes");
    let mut first = app();
    build_world(&mut first);
    let mut second = app();
    // Different unrelated history in the second world: same saveable state, other entity ids.
    for _ in 0..5 {
        second.world_mut().spawn(tag("noise"));
    }
    build_world(&mut second);
    let images = [
        normalized_image(&mut first, &dir, "a"),
        normalized_image(&mut first, &dir, "b"),
        normalized_image(&mut second, &dir, "c"),
    ];
    assert_eq!(images[0], images[1], "two saves of one world");
    assert_eq!(images[0], images[2], "the same saveable state in another world");
    let raw = SaveGame::load_raw(&slot(&dir, "a"), first.world_mut()).expect("raw");
    assert_eq!(
        raw.entity_ids[0],
        SaveEntityId::Level { guid: 0x42, level: 0 },
        "level entities come first"
    );
    assert_eq!(raw.entity_ids[1], SaveEntityId::Runtime(0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Saves `app` to slot `name`, then rewrites the file with the timestamp zeroed: the wall-clock
/// timestamp is the only field two saves of one world may differ in (spec 91 § 5).
fn normalized_image(app: &mut App, dir: &std::path::Path, name: &str) -> Vec<u8> {
    let saved = slot(dir, name);
    SaveGame::save(&saved, app.world_mut(), &SaveSpec::default()).expect("save");
    let mut raw = SaveGame::load_raw(&saved, app.world_mut()).expect("raw");
    raw.timestamp_secs = 0.0;
    let normalized = slot(dir, &format!("{name}-n"));
    SaveGame::save_raw(&normalized, &raw).expect("save raw");
    std::fs::read(&normalized.path).expect("read")
}

/// Builds a format 2 file by hand whose `AiTarget` names save id 9 in a two-entry table.
fn hostile_save(app: &mut App, dir: &std::path::Path) -> SaveSlot {
    let world = app.world_mut();
    let target = world.spawn((Saveable, tag("t"))).id();
    world.spawn((Saveable, tag("p"), AiTarget { target }));
    let good = slot(dir, "good");
    SaveGame::save(&good, world, &SaveSpec::default()).expect("save");
    let mut raw = SaveGame::load_raw(&good, world).expect("raw");
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    raw.map_component(&registry, "AiTarget", &mut |value| {
        if let ReflectMut::Struct(s) = value.reflect_mut()
            && let Some(target) = s.field_mut("target").and_then(|f| f.try_downcast_mut::<Entity>())
        {
            *target = Entity::from_raw_u32(9).expect("index 9");
        }
        Ok(())
    })
    .expect("rewrite");
    let bad = slot(dir, "bad");
    SaveGame::save_raw(&bad, &raw).expect("write hostile");
    bad
}

#[test]
fn a_hostile_out_of_range_id_is_refused_and_the_world_is_untouched() {
    let dir = unique_temp_dir("save-refs-hostile");
    let mut source = app();
    let bad = hostile_save(&mut source, &dir);
    let mut target = app();
    target.world_mut().spawn((Saveable, tag("live")));
    let err = SaveGame::load(&bad, target.world_mut(), &SaveSpec::default()).expect_err("refused");
    assert!(matches!(err, SaveError::Corrupt(_)), "{err:?}");
    assert!(err.to_string().contains(SAVE_ENTITY_REF_OUT_OF_RANGE), "{err}");
    let live = tagged(target.world_mut(), "live");
    assert!(
        target.world().get_entity(live).is_ok(),
        "a refused load changes nothing"
    );

    // A table shorter than the entity list is refused too.
    let mut raw = SaveGame::load_raw(&slot(&dir, "good"), source.world_mut()).expect("raw");
    raw.entity_ids.truncate(1);
    let short = slot(&dir, "short");
    SaveGame::save_raw(&short, &raw).expect("write");
    let err = SaveGame::load(&short, target.world_mut(), &SaveSpec::default()).expect_err("refused");
    assert!(err.to_string().contains(SAVE_ENTITY_IDS_INVALID), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A format 1 save (raw entity bits, the old documented limitation) loads through the converter:
/// the optional link is cleared, the plain one skips its component, plain data is unchanged.
#[test]
fn a_format_1_save_loads_through_the_converter() {
    let dir = unique_temp_dir("save-refs-v1");
    let mut source = app();
    let world = source.world_mut();
    let hero = world.spawn((Saveable, tag("hero"))).id();
    world.spawn((Saveable, tag("sword"), Carried { by: Some(hero) }));
    world.spawn((Saveable, tag("pawn"), AiTarget { target: hero }));
    let good = slot(&dir, "v2");
    SaveGame::save(&good, world, &SaveSpec::default()).expect("save");
    let mut raw: SaveFileRaw = SaveGame::load_raw(&good, world).expect("raw");
    // Make it what format 1 wrote: no table, raw entity bits of this process.
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    for path in ["Carried", "AiTarget"] {
        raw.map_component(&registry, path, &mut |value| {
            if let ReflectMut::Struct(s) = value.reflect_mut() {
                for i in 0..s.field_len() {
                    if let Some(f) = s.field_at_mut(i) {
                        if let Some(e) = f.try_downcast_mut::<Entity>() {
                            *e = hero;
                        }
                        if let Some(o) = f.try_downcast_mut::<Option<Entity>>() {
                            *o = Some(hero);
                        }
                    }
                }
            }
            Ok(())
        })
        .expect("rewrite");
    }
    drop(registry);
    raw.entity_ids.clear();
    raw.version = 1;
    let v1 = slot(&dir, "v1");
    SaveGame::save_raw(&v1, &raw).expect("write v1");
    assert_eq!(&std::fs::read(&v1.path).expect("read")[4..8], &1u32.to_le_bytes());

    let mut loaded = app();
    SaveGame::load(&v1, loaded.world_mut(), &SaveSpec::default()).expect("v1 loads");
    let world = loaded.world_mut();
    let sword = tagged(world, "sword");
    assert_eq!(world.get::<Carried>(sword), Some(&Carried { by: None }));
    let pawn = tagged(world, "pawn");
    assert!(world.get::<AiTarget>(pawn).is_none());
    assert!(
        report(world)
            .notes
            .iter()
            .any(|n| n.code == SAVE_ENTITY_REF_MISSING && n.component == "AiTarget")
    );
    tagged(world, "hero");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inspect_lists_ids_and_references_per_entity() {
    let dir = unique_temp_dir("save-refs-inspect");
    let mut source = app();
    build_world(&mut source);
    let slot = slot(&dir, "inspect");
    SaveGame::save(&slot, source.world_mut(), &SaveSpec::default()).expect("save");
    let raw = SaveGame::load_raw(&slot, source.world_mut()).expect("raw");
    let registry = source.world().resource::<AppTypeRegistry>().clone();
    let rows = inspect_refs(&raw, &registry.read());
    assert_eq!(rows.len(), 4, "crate, hero, sword, box");
    let hero = rows
        .iter()
        .find(|r| r.refs.iter().any(|x| x.component == "AiTarget"))
        .expect("hero row");
    let door_id = hero.refs[0].target.expect("door id") as usize;
    assert_eq!(
        raw.entity_ids[door_id],
        SaveEntityId::Level { guid: 0x77, level: 0 },
        "the door is the external entry"
    );
    let sword = rows
        .iter()
        .find(|r| r.refs.iter().any(|x| x.component == "Carried"))
        .expect("sword row");
    assert_eq!(sword.refs[0].target, Some(hero.id));
    let _ = std::fs::remove_dir_all(&dir);
}
