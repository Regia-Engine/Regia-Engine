// SPDX-License-Identifier: MPL-2.0
//! Runtime level loading accepts level variants (ADR-7823): `load_level_source` on a
//! `.levelvariant.ron` resolves its chain (bases found by id in the project's `.meta` index) into
//! a level in the `LevelLibrary`; a cooked (flattened) variant loads as the level it holds; a loop
//! is refused with `variant_loop`.

use std::path::Path;

use regia_asset::MetaSidecar;
use regia_core::id::Guid;
use regia_ecs::prelude::World;
use regia_framework::level_assets::{level_id_of, load_level_source};
use regia_scene::level::{LevelId, LevelLibrary};
use regia_scene::level_variant::LEVEL_VARIANT_META_KIND;
use regia_test_utils::unique_temp_dir;

const BASE: Guid = Guid::from_u128(0xb0);
const VARIANT: Guid = Guid::from_u128(0xb1);
const T: &str = "regia_scene::transform::Transform";

fn meta(path: &Path, kind: &str, id: Guid) {
    let sidecar = MetaSidecar::new(kind, regia_asset::AssetId(id));
    std::fs::write(
        regia_asset::meta_path_for(path),
        sidecar.to_ron_string().expect("meta"),
    )
    .expect("write meta");
}

fn base_text() -> String {
    let e = |n: u128, x: f32| {
        format!(
            r#"(guid: "{}", components: {{ "{T}": (translation: ({x:?}, 0.0, 0.0)) }}),"#,
            Guid::from_u128(n)
        )
    };
    format!(
        "SceneFile(format_version: {}, entities: [{}{}{}])",
        regia_scene::format::SCENE_FORMAT_VERSION,
        e(1, 0.0),
        e(2, 1.0),
        e(3, 2.0)
    )
}

fn variant_text(base: Guid) -> String {
    format!(
        r#"LevelVariant(format_version: 1, base: "asset:{base}", overrides: [(target: "{a}", component: "{T}", path: ".translation", value: (5.0, 0.0, 0.0))], removed_entities: ["{b}"], added_entities: [(guid: "{d}", components: {{}})])"#,
        a = Guid::from_u128(1),
        b = Guid::from_u128(2),
        d = Guid::from_u128(4),
    )
}

fn translation_of(world: &World, level: LevelId, entity: u128) -> Option<String> {
    let library = world.get_resource::<LevelLibrary>()?;
    let record = library
        .get(level)?
        .scene
        .entities
        .iter()
        .find(|e| e.guid == Guid::from_u128(entity))?
        .clone();
    record.components.get(T).map(|v| v.get_ron().to_owned())
}

#[test]
fn a_variant_loads_as_its_resolved_level() {
    let root = unique_temp_dir("level_variant_load");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let base = assets.join("arena.level.ron");
    std::fs::write(&base, base_text()).expect("base");
    meta(&base, "level", BASE);
    let variant = assets.join("arena_night.levelvariant.ron");
    std::fs::write(&variant, variant_text(BASE)).expect("variant");
    meta(&variant, LEVEL_VARIANT_META_KIND, VARIANT);

    let mut world = World::new();
    let report = load_level_source(&mut world, &variant).expect("loads");
    assert_eq!(report.level, Some(LevelId(VARIANT)));
    assert_eq!(level_id_of(&variant), LevelId(VARIANT));
    let level = LevelId(VARIANT);
    assert_eq!(
        translation_of(&world, level, 1).as_deref(),
        Some("(translation: (5.0, 0.0, 0.0))")
    );
    assert!(translation_of(&world, level, 2).is_none(), "removed");
    let guids: Vec<u128> = world
        .resource::<LevelLibrary>()
        .get(level)
        .expect("level")
        .scene
        .entities
        .iter()
        .map(|e| e.guid.as_u128())
        .collect();
    assert_eq!(guids, vec![1, 3, 4]);

    // A cooked (flattened) variant under the same name loads as the level it holds, with no base.
    let flat = root.join("packaged/arena_night.levelvariant.ron");
    std::fs::create_dir_all(flat.parent().expect("parent")).expect("dir");
    let resolved = world
        .resource::<LevelLibrary>()
        .get(level)
        .expect("level")
        .scene
        .to_ron_string()
        .expect("ron");
    std::fs::write(&flat, &resolved).expect("flat");
    let mut packaged = World::new();
    let report = load_level_source(&mut packaged, &flat).expect("flattened loads");
    let id = report.level.expect("id");
    assert_eq!(
        translation_of(&packaged, id, 1).as_deref(),
        Some("(translation: (5.0, 0.0, 0.0))")
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_variant_loop_and_a_missing_base_are_refused_with_their_codes() {
    let root = unique_temp_dir("level_variant_loop");
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).expect("assets");
    let one = assets.join("one.levelvariant.ron");
    let two = assets.join("two.levelvariant.ron");
    std::fs::write(&one, variant_text(Guid::from_u128(0xc2))).expect("one");
    meta(&one, LEVEL_VARIANT_META_KIND, Guid::from_u128(0xc1));
    std::fs::write(&two, variant_text(Guid::from_u128(0xc1))).expect("two");
    meta(&two, LEVEL_VARIANT_META_KIND, Guid::from_u128(0xc2));
    let mut world = World::new();
    let err = load_level_source(&mut world, &one).expect_err("loop");
    assert_eq!(err.reason(), "variant_loop", "{err}");

    let lonely = assets.join("lonely.levelvariant.ron");
    std::fs::write(&lonely, variant_text(Guid::from_u128(0xdead))).expect("lonely");
    let err = load_level_source(&mut world, &lonely).expect_err("missing");
    assert_eq!(err.reason(), "base_missing", "{err}");
    std::fs::remove_dir_all(&root).ok();
}
