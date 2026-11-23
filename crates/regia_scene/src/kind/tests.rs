// SPDX-License-Identifier: MPL-2.0
//! Kind registration, resolution and inheritance through prefab instances (spec 101 § 2).

use regia_core::id::Guid;
use regia_ecs::reflect::AppTypeRegistry;

use super::*;
use crate::components::EntityGuid;
use crate::format::{PrefabInstanceData, SceneAssetRef, SceneFile};
use crate::prefab::{PrefabLibrary, PrefabTemplate, instantiate_prefab};
use crate::spawn::SpawnOptions;
use crate::transform::Transform;

/// A health value the `Crate` kind requires.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Health(pub u32);

/// A code kind requiring `Transform` and `Health`.
#[derive(Component, Reflect, Clone, Debug, Default)]
#[reflect(Component, Default)]
#[require(Transform, Health)]
pub struct Crate;

crate::entity_kind!(
    Crate,
    "Crate",
    icon = "crate",
    color = [0xc0, 0x8a, 0x3e],
    description = "A breakable supply crate",
    category = "Props"
);

/// A second code kind, for conflicts.
#[derive(Component, Reflect, Clone, Debug, Default)]
#[reflect(Component, Default)]
pub struct Barrel;

crate::entity_kind!(
    Barrel,
    "Barrel",
    icon = "box",
    color = [0x80, 0x40, 0x20],
    description = "A barrel"
);

fn app() -> App {
    let mut app = App::new();
    app.register_type::<Transform>().register_type::<Health>();
    add_to_app(&mut app);
    app
}

#[test]
fn register_kind_records_metadata_and_the_required_components() {
    let mut app = app();
    app.register_kind::<Crate>();
    let registry = app.world().resource::<KindRegistry>();
    let info = registry.get("Crate").expect("registered");
    assert_eq!(info.icon, "crate");
    assert_eq!(info.color_hex(), "#c08a3e");
    assert_eq!(info.category_name(), "Props");
    assert_eq!(
        info.required,
        [
            "regia_scene::kind::tests::Health".to_string(),
            "regia_scene::transform::Transform".to_string()
        ]
    );
    assert_eq!(
        info.source,
        KindSource::Code {
            marker: "regia_scene::kind::tests::Crate".into()
        }
    );
    assert_eq!(std::mem::size_of::<Crate>(), 0, "a kind marker is zero-size");
}

#[test]
fn a_spawned_marker_brings_its_contract_and_resolves() {
    let mut app = app();
    app.register_kind::<Crate>();
    let entity = app.world_mut().spawn(Crate).id();
    let world = app.world();
    assert!(
        world.get::<Health>(entity).is_some(),
        "required components are inserted"
    );
    assert!(world.get::<Transform>(entity).is_some());
    let kind = kind_of(world, entity).expect("kinded");
    assert_eq!(kind.name, "Crate");
    assert!(kind.info.is_some());
    assert!(!kind.via_prefab);
    let plain = app.world_mut().spawn(Transform::default()).id();
    assert_eq!(kind_of(app.world(), plain), None);
}

#[test]
fn data_kinds_resolve_through_the_tag_and_unknown_tags_are_reported() {
    let mut app = app();
    let file = file::KindFile::from_ron_str(
        r##"(format_version: 1, name: "Pickup", icon: "pickup", color: "#30c050", description: "Something to pick up")"##,
    )
    .expect("parse");
    app.world_mut()
        .resource_mut::<KindRegistry>()
        .register_data(file.to_info("assets/kinds/pickup.kind.ron").expect("info"));
    let tagged = app.world_mut().spawn(KindTag("Pickup".into())).id();
    let unknown = app.world_mut().spawn(KindTag("Nope".into())).id();
    let world = app.world();
    assert_eq!(
        kind_of(world, tagged).and_then(|k| k.info).map(|i| i.icon),
        Some("pickup".into())
    );
    let unknown = kind_of(world, unknown).expect("tag present");
    assert_eq!(unknown.name, "Nope");
    assert_eq!(unknown.info, None, "kind_unknown");
}

#[test]
fn two_markers_are_a_conflict_and_the_first_by_name_wins() {
    let mut app = app();
    app.register_kind::<Crate>().register_kind::<Barrel>();
    let both = app.world_mut().spawn((Crate, Barrel)).id();
    let kind = kind_of(app.world(), both).expect("kinded");
    assert_eq!(kind.name, "Barrel");
    assert_eq!(kind.conflicts, ["Crate"]);
}

#[test]
fn a_code_kind_replaces_its_data_declaration_and_notes_drift() {
    let mut app = app();
    let declaration = KindInfo {
        name: "Crate".into(),
        icon: "box".into(),
        color: [0xc0, 0x8a, 0x3e],
        description: "A breakable supply crate".into(),
        category: Some("Props".into()),
        required: Vec::new(),
        source: KindSource::Data {
            path: "assets/kinds/crate.kind.ron".into(),
            marker: Some("regia_scene::kind::tests::Crate".into()),
        },
    };
    app.world_mut()
        .resource_mut::<KindRegistry>()
        .register_data(declaration);
    app.register_kind::<Crate>();
    let registry = app.world().resource::<KindRegistry>();
    assert!(matches!(
        registry.get("Crate").map(|k| &k.source),
        Some(KindSource::Code { .. })
    ));
    assert_eq!(registry.kinds().len(), 1);
    assert!(
        registry
            .notes()
            .iter()
            .any(|n| n.contains("icon") && n.contains("required")),
        "{:?}",
        registry.notes()
    );
}

#[test]
fn categories_list_named_groups_then_uncategorized() {
    let mut app = app();
    app.register_kind::<Crate>().register_kind::<Barrel>();
    let registry = app.world().resource::<KindRegistry>();
    let groups: Vec<(String, Vec<String>)> = registry
        .by_category()
        .into_iter()
        .map(|(c, kinds)| (c, kinds.iter().map(|k| k.name.clone()).collect()))
        .collect();
    assert_eq!(
        groups,
        [
            ("Props".to_string(), vec!["Crate".to_string()]),
            (UNCATEGORIZED.to_string(), vec!["Barrel".to_string()])
        ]
    );
}

#[test]
fn kind_names_are_checked() {
    assert!(validate_kind_name("Enemy_Spawner2").is_ok());
    assert!(validate_kind_name("").is_err());
    assert!(validate_kind_name("two words").is_err());
    assert!(validate_kind_name(&"x".repeat(65)).is_err());
}

fn crate_prefab(root: Guid, child: Guid) -> String {
    format!(
        r#"(
    format_version: 1,
    entities: [
        (guid: "{root}", name: "Crate", components: {{
            "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0)),
            "regia_scene::kind::KindTag": ("Crate"),
        }}),
        (guid: "{child}", name: "Lid", parent: Some("{root}"), components: {{}}),
    ],
)"#
    )
}

#[test]
fn prefab_instances_inherit_the_root_kind() {
    let mut app = app();
    app.register_type::<EntityGuid>();
    app.world_mut().init_resource::<PrefabLibrary>();
    app.world_mut()
        .resource_mut::<KindRegistry>()
        .register_data(KindInfo {
            name: "Crate".into(),
            icon: "crate".into(),
            color: [1, 2, 3],
            description: String::new(),
            category: None,
            required: Vec::new(),
            source: KindSource::Data {
                path: "k.kind.ron".into(),
                marker: None,
            },
        });
    let file = SceneFile::from_ron_str(&crate_prefab(Guid::from_u128(1), Guid::from_u128(2))).expect("parse");
    let asset = Guid::from_u128(77);
    let registry = app.world().resource::<AppTypeRegistry>().0.clone();
    let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
    assert!(template.warnings.is_empty(), "{:?}", template.warnings);
    app.world_mut().resource_mut::<PrefabLibrary>().insert(template);

    let mut roots = Vec::new();
    for n in 0..2u128 {
        let world = app.world_mut();
        let root = world.spawn(EntityGuid(Guid::from_u128(100 + n))).id();
        let instance = PrefabInstanceData::plain(SceneAssetRef(asset));
        let result =
            instantiate_prefab(world, &instance, root, &SpawnOptions::default(), 0).expect("instantiate");
        let template_root = result.locals[&Guid::from_u128(1)];
        let lid = result.locals[&Guid::from_u128(2)];
        let world = app.world();
        assert_eq!(
            world.get::<KindTag>(template_root),
            Some(&KindTag("Crate".into()))
        );
        let template_kind = kind_of(world, template_root).expect("template root kinded");
        assert!(!template_kind.via_prefab);
        assert_eq!(kind_of(world, lid), None, "a part is not itself kinded");
        roots.push(root);
    }
    for root in roots {
        let kind = kind_of(app.world(), root).expect("instance root inherits");
        assert_eq!(kind.name, "Crate");
        assert!(kind.via_prefab);
        assert!(kind.info.is_some());
    }
}

/// Spec 101 § 2.4: a nested prefab's root keeps its own kind — instantiating an outer prefab
/// whose root instantiates an inner one, the inner template root resolves to its own kind, not
/// the outer's.
#[test]
fn a_nested_prefab_root_keeps_its_own_kind() {
    let mut app = app();
    app.register_type::<EntityGuid>();
    app.world_mut().init_resource::<PrefabLibrary>();
    app.register_kind::<Crate>().register_kind::<Barrel>();

    // Inner prefab: a lamp whose root names the Crate kind, plus an unkinded part.
    let lamp_root = Guid::from_u128(1);
    let lamp_shade = Guid::from_u128(2);
    let inner_file = SceneFile::from_ron_str(&format!(
        r#"(
    format_version: 1,
    entities: [
        (guid: "{lamp_root}", name: "Lamp", components: {{
            "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0)),
            "regia_scene::kind::KindTag": ("Crate"),
        }}),
        (guid: "{lamp_shade}", name: "Shade", parent: Some("{lamp_root}"), components: {{}}),
    ],
)"#
    ))
    .expect("parse the inner prefab");
    let inner_asset = Guid::from_u128(70);
    let registry = app.world().resource::<AppTypeRegistry>().0.clone();
    let inner_template =
        PrefabTemplate::from_scene_file(inner_asset, &inner_file, &registry.read()).expect("bake");
    app.world_mut()
        .resource_mut::<PrefabLibrary>()
        .insert(inner_template);

    // Outer prefab: a room whose root names the Barrel kind and instantiates the lamp.
    let room_root = Guid::from_u128(3);
    let outer_file = SceneFile::from_ron_str(&format!(
        r#"(
    format_version: 1,
    entities: [
        (guid: "{room_root}", name: "Room", components: {{
            "regia_scene::transform::Transform": (translation: (10.0, 0.0, 0.0)),
            "regia_scene::kind::KindTag": ("Barrel"),
        }},
         prefab_instance: (prefab: "asset:{inner_asset}")),
    ],
)"#
    ))
    .expect("parse the outer prefab");
    let outer_asset = Guid::from_u128(71);
    let outer_template =
        PrefabTemplate::from_scene_file(outer_asset, &outer_file, &registry.read()).expect("bake");
    app.world_mut()
        .resource_mut::<PrefabLibrary>()
        .insert(outer_template);

    // Level: one instance of the room.
    let root = app.world_mut().spawn(EntityGuid(Guid::from_u128(100))).id();
    let instance = PrefabInstanceData::plain(SceneAssetRef(outer_asset));
    let result = instantiate_prefab(app.world_mut(), &instance, root, &SpawnOptions::default(), 0)
        .expect("instantiate");
    let room = result.locals[&room_root];
    let lamp = result.locals[&lamp_root];
    let shade = result.locals[&lamp_shade];
    let world = app.world();

    // The level instance root inherits the outer kind through its template root.
    let level_kind = kind_of(world, root).expect("the instance root inherits the room kind");
    assert_eq!(level_kind.name, "Barrel");
    assert!(level_kind.via_prefab);
    // The outer template root names its own kind; being the nested instance root as well does
    // not change that.
    let room_kind = kind_of(world, room).expect("the room root is kinded");
    assert_eq!(room_kind.name, "Barrel");
    assert!(!room_kind.via_prefab);
    // § 2.4: the nested prefab's root keeps its own kind (Crate), not the outer's (Barrel).
    let lamp_kind = kind_of(world, lamp).expect("the nested root keeps its own kind");
    assert_eq!(lamp_kind.name, "Crate");
    assert!(!lamp_kind.via_prefab);
    assert_eq!(kind_of(world, shade), None, "a part is not itself kinded");
}

/// Writes `text` to `root/rel`, creating the folders.
fn write_file(root: &std::path::Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

const PICKUP_KIND: &str = r##"(format_version: 1, name: "Pickup", icon: "pickup", color: "#30c050", description: "Something to pick up")"##;

/// Spec 101 § 2.3 / ADR-5609: a project's `.kind.ron` files reach the live registry, so a
/// `KindTag` resolves with its declared icon instead of `kind_unknown`.
#[test]
fn project_data_kinds_load_into_the_live_registry() {
    let root = regia_test_utils::TempDir::new("kind_load_project");
    write_file(&root, "assets/kinds/pickup.kind.ron", PICKUP_KIND);
    write_file(
        &root,
        "assets/kinds/crate.kind.ron",
        r##"(format_version: 1, name: "Crate", icon: "box", color: "#c08a3e", description: "A breakable supply crate", category: Some("Props"))"##,
    );
    write_file(
        &root,
        "assets/kinds/future.kind.ron",
        r##"(format_version: 9, name: "Future", color: "#000000")"##,
    );
    write_file(&root, "assets/levels/a.level.ron", "not read by the kind loader");
    let mut app = app();
    app.register_kind::<Crate>();
    let tagged = app.world_mut().spawn(KindTag("Pickup".into())).id();
    assert_eq!(
        kind_of(app.world(), tagged).and_then(|k| k.info),
        None,
        "not declared before the project's kinds are loaded"
    );

    let load = load_project_kinds(app.world_mut(), &root);
    assert_eq!(load.loaded, 1, "Pickup; Crate stays the code kind");
    assert_eq!(
        load.errors
            .iter()
            .map(|(path, code, _)| (path.as_str(), *code))
            .collect::<Vec<_>>(),
        [("assets/kinds/future.kind.ron", "kind_version_newer")]
    );
    let world = app.world();
    let info = kind_of(world, tagged).and_then(|k| k.info).expect("declared now");
    assert_eq!(info.icon, "pickup");
    assert_eq!(
        info.source,
        KindSource::Data {
            path: "assets/kinds/pickup.kind.ron".into(),
            marker: None
        }
    );
    let registry = world.resource::<KindRegistry>();
    assert!(matches!(
        registry.get("Crate").map(|k| &k.source),
        Some(KindSource::Code { .. })
    ));
    let notes = registry.notes().to_vec();
    assert!(
        notes.iter().any(|n| n.contains("crate.kind.ron")
            && n.contains("differs from the code kind")
            && n.contains("icon")),
        "drift between the code kind and its declaration: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("future.kind.ron") && n.contains("kind_version_newer")),
        "{notes:?}"
    );

    // Loading again changes nothing and repeats no note.
    let again = load_project_kinds(app.world_mut(), &root);
    assert_eq!(again.loaded, 1);
    assert_eq!(app.world().resource::<KindRegistry>().notes(), notes.as_slice());
}

#[test]
fn opening_another_project_replaces_its_data_kinds_and_keeps_code_kinds() {
    let first = regia_test_utils::TempDir::new("kind_load_first");
    write_file(&first, "assets/pickup.kind.ron", PICKUP_KIND);
    write_file(&first, "assets/broken.kind.ron", "(");
    let second = regia_test_utils::TempDir::new("kind_load_second");
    write_file(
        &second,
        "assets/kinds/door.kind.ron",
        r##"(format_version: 1, name: "Door", icon: "door", color: "#806040")"##,
    );
    let mut app = app();
    app.register_kind::<Crate>().register_kind::<Crate>();
    let load = load_project_kinds(app.world_mut(), &first);
    assert_eq!(load.loaded, 1);
    assert_eq!(load.errors.len(), 1);
    assert_eq!(load.errors[0].1, "kind_parse");

    let load = load_project_kinds(app.world_mut(), &second);
    assert_eq!(load.loaded, 1);
    assert!(load.errors.is_empty());
    let registry = app.world().resource::<KindRegistry>();
    let names: Vec<&str> = registry.kinds().iter().map(|k| k.name.as_str()).collect();
    assert_eq!(names, ["Crate", "Door"], "Pickup left with the first project");
    assert_eq!(
        registry.notes(),
        ["kind `Crate` is registered twice by code; the first registration is kept".to_string()],
        "the first project's file notes are gone, the code note stays"
    );
}

#[test]
fn a_root_without_assets_loads_nothing() {
    let root = regia_test_utils::TempDir::new("kind_load_no_assets");
    write_file(&root, "pickup.kind.ron", PICKUP_KIND);
    let mut app = app();
    let load = load_project_kinds(app.world_mut(), &root);
    assert_eq!(load, KindLoad::default(), "only <root>/assets is read");
    assert!(app.world().resource::<KindRegistry>().is_empty());
}
