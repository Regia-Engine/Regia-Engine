// SPDX-License-Identifier: MPL-2.0
//! Custom data assets (`#[derive(Asset)]`, `.data.ron`, spec 10 § 2.4) and net profiles
//! (`.netprofile.ron`, spec 72 § 8) through the cook, a pak and the asset server, with the sources
//! gone as in a shipped game (ADR-7232).
//!
//! `cargo test -p regia_asset_pipeline --test data_assets_and_net_profiles`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::data_asset::{
    DATA_ASSET_TYPE_PATH, DataAssetFile, DataAssets, ReflectAsset, RegisterDataAssetExt, data_asset_types,
};
use regia_asset::engine_source::EngineAssets;
use regia_asset::registry::{AssetRegistry, cache_registry_path};
use regia_asset::{AssetManifest, AssetServer, AssetServerSettings, cooked_cache_dir, current_platform};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, pack_cooked_directory};
use regia_net::profile_assets::{
    NET_PROFILE_TYPE_PATH, NetProfileAssets, apply_net_profile_assets, apply_profile_assets,
    builtin_profile_path, load_net_profile_assets, read_profile_assets, register_builtin_profile_files,
};
use regia_net::{NetProfile, NetProfiles, RelevanceShape};
use regia_test_utils::unique_temp_dir;

/// A game's data asset type.
#[derive(Reflect, Default, Debug, Clone, PartialEq, regia_macros::Asset)]
#[reflect(Asset, Default)]
struct WeaponStats {
    name: String,
    damage: u32,
    range_m: f32,
    tags: Vec<String>,
    falloff: Option<f32>,
}

const RIFLE: &str = r#"DataAsset(
    format_version: 1,
    type: "data_assets_and_net_profiles::WeaponStats",
    value: (
        name: "rifle",
        damage: 34,
        range_m: 120.5,
        tags: ["long", "loud"],
    ),
)"#;

const SNIPER: &str = r#"NetProfile(
    format_version: 1,
    priority: 2.5,
    relevance: Spatial(radius: 900.0, hysteresis: 0.1),
    position_step_m: Some(0.01),
)"#;

fn write(root: &Path, rel: &str, text: &str) {
    let file = root.join("assets").join(rel);
    std::fs::create_dir_all(file.parent().unwrap_or(&file)).unwrap_or_else(|e| panic!("dir: {e}"));
    std::fs::write(file, text).unwrap_or_else(|e| panic!("write: {e}"));
}

fn project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = unique_temp_dir(tag);
    for (rel, text) in files {
        write(&root, rel, text);
    }
    root
}

fn cook(root: &Path) -> CookReport {
    cook_project(CookOptions::for_host(root.to_path_buf())).unwrap_or_else(|e| panic!("cook: {e}"))
}

/// The manifest of `root`'s cook: path -> type path.
fn manifest(root: &Path) -> BTreeMap<String, String> {
    let cache = cooked_cache_dir(root, &current_platform());
    AssetManifest::read_from_cache(&cache)
        .ok()
        .flatten()
        .map(|m| m.entries.into_iter().map(|e| (e.path, e.type_path)).collect())
        .unwrap_or_default()
}

/// Packs `root`'s cook and opens a server over a second root with only the pak.
fn shipped_server(root: &Path, tag: &str) -> AssetServer {
    let cache = cooked_cache_dir(root, &current_platform());
    let content = root.join("content");
    pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).unwrap_or_else(|e| panic!("pack: {e}"));
    let shipped = unique_temp_dir(tag);
    AssetServer::open(AssetServerSettings {
        project_root: shipped,
        platform: current_platform(),
        content_dir: Some(content),
    })
    .unwrap_or_else(|e| panic!("open: {e}"))
}

#[test]
fn a_data_asset_cooks_ships_in_a_pak_and_loads_as_its_type() {
    let root = project(
        "data-asset-cook",
        &[
            ("weapons/rifle.data.ron", RIFLE),
            (
                "weapons/broken.data.ron",
                "DataAsset(format_version: 1, value: ())",
            ),
            (
                "weapons/future.data.ron",
                "DataAsset(format_version: 9, type: \"x\", value: ())",
            ),
        ],
    );
    let report = cook(&root);
    let errors: BTreeMap<String, String> = report
        .errors
        .iter()
        .map(|(p, e)| (p.replace('\\', "/"), e.clone()))
        .collect();
    assert!(
        !errors.keys().any(|p| p.ends_with("weapons/rifle.data.ron")),
        "the rifle cooks: {errors:?}"
    );
    for bad in ["weapons/broken.data.ron", "weapons/future.data.ron"] {
        assert!(
            errors.keys().any(|p| p.ends_with(bad)),
            "{bad} fails the cook: {errors:?}"
        );
    }
    let cooked = manifest(&root);
    assert_eq!(
        cooked.get("assets/weapons/rifle.data.ron").map(String::as_str),
        Some(DATA_ASSET_TYPE_PATH),
        "{cooked:?}"
    );

    // The asset registry knows the kind and the value's type without the game's code.
    let cache = cooked_cache_dir(&root, &current_platform());
    let registry = AssetRegistry::read_from(&cache_registry_path(&cache))
        .unwrap_or_else(|e| panic!("registry: {e}"))
        .unwrap_or_else(|| panic!("a cook wrote one"));
    let entry = registry
        .by_path("assets/weapons/rifle.data.ron")
        .unwrap_or_else(|| panic!("no registry entry"));
    assert_eq!(entry.tags.get("kind").map(String::as_str), Some("data_asset"));
    assert_eq!(
        entry.tags.get("type").map(String::as_str),
        Some("data_assets_and_net_profiles::WeaponStats")
    );

    // A shipped game: the pak only.
    let mut app = App::new();
    app.register_data_asset::<WeaponStats>();
    let types = data_asset_types(&app.type_registry().read());
    assert_eq!(types, vec!["data_assets_and_net_profiles::WeaponStats"]);
    let mut server = shipped_server(&root, "data-asset-game");
    let mut assets = DataAssets::default();
    let registry = app.type_registry().read();
    let id = assets
        .load_path(&mut server, "assets/weapons/rifle.data.ron", &registry)
        .unwrap_or_else(|e| panic!("load: {e}"));
    let rifle = assets
        .get::<WeaponStats>(id)
        .unwrap_or_else(|| panic!("a WeaponStats"));
    assert_eq!(
        rifle,
        &WeaponStats {
            name: "rifle".into(),
            damage: 34,
            range_m: 120.5,
            tags: vec!["long".into(), "loud".into()],
            falloff: None,
        }
    );
    // ReflectAsset builds a default value for a tool that knows only the type path.
    let reflect_asset = registry
        .get_with_type_path("data_assets_and_net_profiles::WeaponStats")
        .and_then(|r| r.data::<ReflectAsset>())
        .unwrap_or_else(|| panic!("ReflectAsset registered"));
    let fresh = reflect_asset.default_value();
    let text = DataAssetFile::to_ron(fresh.as_partial_reflect(), &registry)
        .unwrap_or_else(|e| panic!("to_ron: {e}"));
    let back = DataAssetFile::parse(&text, &registry).unwrap_or_else(|e| panic!("parse: {e}"));
    assert_eq!(
        back.value.downcast_ref::<WeaponStats>(),
        Some(&WeaponStats::default())
    );
    // A path the manifest lacks is a load error, not a panic.
    let err = assets
        .load_path(&mut server, "assets/weapons/missing.data.ron", &registry)
        .map(|_| ())
        .expect_err("a path the manifest lacks is an error");
    assert_eq!(err.code(), "data_asset_load");
}

#[test]
fn net_profiles_come_from_engine_files_and_the_projects_pak() {
    let root = project(
        "netprofile-cook",
        &[
            ("net/profiles/sniper.netprofile.ron", SNIPER),
            ("net/profiles/bad.netprofile.ron", "NetProfile(priority: 0.0)"),
        ],
    );
    let report = cook(&root);
    assert!(
        report.errors.iter().any(
            |(p, e)| p.replace('\\', "/").ends_with("net/profiles/bad.netprofile.ron")
                && e.contains("priority")
        ),
        "an invalid profile fails the cook: {:?}",
        report.errors
    );
    let cooked = manifest(&root);
    assert_eq!(
        cooked
            .get("assets/net/profiles/sniper.netprofile.ron")
            .map(String::as_str),
        Some(NET_PROFILE_TYPE_PATH),
        "{cooked:?}"
    );

    let mut engine = EngineAssets::default();
    register_builtin_profile_files(&mut engine);
    assert!(engine.get(&builtin_profile_path("player")).is_some());
    let mut server = shipped_server(&root, "netprofile-game");
    let assets = read_profile_assets(&engine, Some(&mut server));
    assert!(assets.unreadable.is_empty(), "{:?}", assets.unreadable);
    let sources: Vec<(&str, &str)> = assets.files.iter().map(|f| (f.path.as_str(), f.source)).collect();
    assert!(
        sources.contains(&("engine://net/profiles/player.netprofile.ron", "engine")),
        "{sources:?}"
    );
    assert!(
        sources.contains(&("assets/net/profiles/sniper.netprofile.ron", "pak")),
        "{sources:?}"
    );

    // Built-ins equal to the code's change nothing; the project profile arrives.
    let mut profiles = NetProfiles::default();
    let generation = profiles.generation();
    let changed = apply_profile_assets(&assets, &mut profiles);
    assert_eq!(changed, 1, "only the project profile is new");
    assert!(profiles.generation() > generation);
    let sniper = profiles
        .get("assets/net/profiles/sniper.netprofile.ron")
        .unwrap_or_else(|| panic!("sniper loaded"));
    assert_eq!(sniper.priority, 2.5);
    assert_eq!(
        sniper.relevance,
        RelevanceShape::Spatial {
            radius: 900.0,
            hysteresis: 0.1
        }
    );
    assert_eq!(sniper.position_step_m, Some(0.01));
    assert_eq!(
        profiles.get("player"),
        regia_net::profile::builtin_profiles()
            .into_iter()
            .find(|(n, _)| *n == "player")
            .map(|(_, p)| p)
            .as_ref()
    );

    // The systems: Startup reads, PreUpdate applies once NetProfiles exists.
    let mut world = World::new();
    world.insert_resource(engine);
    world.insert_resource(server);
    load_net_profile_assets(&mut world);
    apply_net_profile_assets(&mut world);
    assert!(
        !world.resource::<NetProfileAssets>().applied,
        "no NetProfiles yet"
    );
    world.insert_resource(NetProfiles::default());
    apply_net_profile_assets(&mut world);
    assert!(world.resource::<NetProfileAssets>().applied);
    assert_eq!(
        world
            .resource::<NetProfiles>()
            .get("assets/net/profiles/sniper.netprofile.ron")
            .map(|p: &NetProfile| p.priority),
        Some(2.5)
    );
}
