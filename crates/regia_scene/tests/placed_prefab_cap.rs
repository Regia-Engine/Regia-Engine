// SPDX-License-Identifier: MPL-2.0
//! The placement cap counts what prefab instances spawn (finding scene-ecs 3, second half;
//! v0.5.1): spec 37 § 6.4's "total spawned entities per instance ≤ 100,000" must hold for a level
//! whose few records instantiate large prefabs, not only for one with many records.

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_scene::components::EntityGuid;
use regia_scene::format::{PrefabInstanceData, SceneAssetRef};
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use regia_scene::placed::{
    MAX_PLACED_ENTITIES, PlacedError, PlacedSpawned, check_placement, check_placement_with_prefabs,
};
use regia_scene::prefab::{NestedPrefab, PrefabLibrary, PrefabTemplate, TemplateEntity};
use regia_scene::prefab_variant::PrefabVariant;
use regia_scene::prelude::*;

const HOST: LevelId = LevelId(Guid::from_u128(0xb0));
const PIECE: LevelId = LevelId(Guid::from_u128(0xa0));
/// A prefab of 1,000 entities (a root and 999 children).
const LEAF: Guid = Guid::from_u128(0xfab_0001);
/// A prefab of 101 entities, each nesting `LEAF`: 101 + 101 × 1,000 = 101,101 entities.
const FAN: Guid = Guid::from_u128(0xfab_0002);
/// A prefab of 10 entities.
const SMALL: Guid = Guid::from_u128(0xfab_0003);
/// A variant of `FAN` with no edits.
const FAN_VARIANT: Guid = Guid::from_u128(0xfab_0004);

fn local(asset: Guid, i: u128) -> Guid {
    Guid::from_u128((asset.as_u128() << 32) + i + 1)
}

fn template_entity(asset: Guid, i: u128, nested: Option<Guid>) -> TemplateEntity {
    TemplateEntity {
        local_id: local(asset, i),
        name: None,
        parent: (i > 0).then_some(0),
        components: Vec::new(),
        nested: nested.map(|inner| NestedPrefab {
            asset: inner,
            instance: PrefabInstanceData::plain(SceneAssetRef::new(inner)),
        }),
    }
}

fn template(asset: Guid, count: u128, nested: Option<Guid>) -> PrefabTemplate {
    PrefabTemplate {
        asset,
        entities: (0..count).map(|i| template_entity(asset, i, nested)).collect(),
        warnings: Vec::new(),
    }
}

fn prefabs() -> PrefabLibrary {
    let mut library = PrefabLibrary::new();
    library.insert(template(LEAF, 1000, None));
    library.insert(template(FAN, 101, Some(LEAF)));
    library.insert(template(SMALL, 10, None));
    library.insert_variant(PrefabVariant {
        asset: FAN_VARIANT,
        base: FAN,
        edits: PrefabInstanceData::plain(SceneAssetRef::new(FAN)),
    });
    library
}

/// A level of one record that instantiates `prefab`, without the children `removed`.
fn piece(prefab: Guid, removed: &[Guid]) -> Level {
    let removed: Vec<String> = removed.iter().map(|g| format!("\"{g}\"")).collect();
    let text = format!(
        "SceneFile(format_version: {v}, entities: [(guid: \"{record}\", components: {{}}, prefab_instance: (prefab: \"asset:{prefab}\", removed_children: [{removed}]))], resources: {{}})",
        v = SCENE_FORMAT_VERSION,
        record = Guid::from_u128(0xf000),
        removed = removed.join(", "),
    );
    Level::from_ron_str(PIECE, &text).expect("level parses")
}

fn library_with(level: Level) -> LevelLibrary {
    let mut library = LevelLibrary::new();
    library.insert(level);
    library
}

#[test]
fn a_prefab_whose_expansion_passes_the_cap_is_refused() {
    let prefabs = prefabs();
    for prefab in [FAN, FAN_VARIANT] {
        let library = library_with(piece(prefab, &[]));
        let err = check_placement_with_prefabs(&library, &prefabs, PIECE, &[HOST])
            .expect_err("101,101 entities is over the cap");
        assert_eq!(err.reason(), "placed_limit", "{prefab}");
        assert!(
            err.to_string().contains(&MAX_PLACED_ENTITIES.to_string()),
            "{err}"
        );
        // The record-only count (the patch-compatible entry point) is unchanged.
        assert_eq!(check_placement(&library, PIECE, &[HOST]), Ok(1));
    }
}

#[test]
fn a_small_prefab_counts_its_template_entities() {
    let prefabs = prefabs();
    let library = library_with(piece(SMALL, &[]));
    assert_eq!(
        check_placement_with_prefabs(&library, &prefabs, PIECE, &[HOST]),
        Ok(1 + 10),
        "the record and the ten entities it instantiates"
    );
}

/// Removed children (and their nested prefabs) are not spawned, so they are not counted.
#[test]
fn removed_children_are_not_counted() {
    let prefabs = prefabs();
    let library = library_with(piece(FAN, &[local(FAN, 1), local(FAN, 2)]));
    assert_eq!(
        check_placement_with_prefabs(&library, &prefabs, PIECE, &[HOST]),
        Ok(1 + 99 + 99 * 1000)
    );
}

/// A prefab that is not loaded counts as zero, like a missing level (instantiation reports it).
#[test]
fn a_missing_prefab_counts_as_zero() {
    let library = library_with(piece(FAN, &[]));
    assert_eq!(
        check_placement_with_prefabs(&library, &PrefabLibrary::new(), PIECE, &[HOST]),
        Ok(1)
    );
}

fn host_placing_piece() -> Level {
    let text = format!(
        "SceneFile(format_version: {v}, entities: [(guid: \"{guid}\", components: {{ \"regia_scene::placed::PlacedLevel\": (level: \"asset:{piece}\", mode: Inline, filter: (layers: [])) }})], resources: {{}})",
        v = SCENE_FORMAT_VERSION,
        guid = Guid::from_u128(0x21),
        piece = PIECE.0,
    );
    Level::from_ron_str(HOST, &text).expect("host parses")
}

fn fill(prefab: Guid) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(prefabs());
    {
        let mut library = app.world_mut().resource_mut::<LevelLibrary>();
        library.insert(piece(prefab, &[]));
        library.insert(host_placing_piece());
    }
    open_level(app.world_mut(), HOST, LoadMode::Replace).expect("open");
    app.update();
    app.update();
    let world = app.world_mut();
    let mut query = world.query::<(Entity, &EntityGuid)>();
    let instance = query
        .iter(world)
        .find(|(_, g)| g.0 == Guid::from_u128(0x21))
        .map(|(e, _)| e)
        .expect("the instance exists");
    (app, instance)
}

/// End to end: the fill refuses the large prefab with `placed_limit` before anything spawns, and
/// still places the small one.
#[test]
fn the_fill_refuses_a_large_prefab_and_places_a_small_one() {
    let (mut app, instance) = fill(FAN);
    let world = app.world_mut();
    let error = world.get::<PlacedError>(instance).expect("refused");
    assert_eq!(error.reason, "placed_limit", "{}", error.detail);
    assert!(world.get::<PlacedSpawned>(instance).is_none());
    let spawned = world.query::<&EntityGuid>().iter(world).count();
    assert!(spawned < 10, "nothing of the prefab spawned: {spawned} entities");

    let (mut app, instance) = fill(SMALL);
    let world = app.world_mut();
    assert!(world.get::<PlacedError>(instance).is_none());
    assert!(world.get::<PlacedSpawned>(instance).is_some());
    let spawned = world.query::<&EntityGuid>().iter(world).count();
    assert!(
        spawned >= 1 + 1 + 10,
        "host instance, record and template: {spawned}"
    );
}
