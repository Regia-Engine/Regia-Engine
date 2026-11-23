// SPDX-License-Identifier: MPL-2.0
//! The pak cook of the ability formats (spec 53 § 4.1, ADR-7216 (8), ADR-9370): `cook_project`
//! cooks `.attrs.ron`, `.effect.ron` and `.ability.ron` to `abilities-cooked` in load order, a
//! packaged project's pak lists them, and the runtime loader reads them from the pak (with the
//! sources gone) to the same values as the RON. A missing or looping cost or cooldown reference fails
//! the cook with its code. A dev tree loads the loose RON.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings, PakReader, cooked_cache_dir, current_platform};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, pack_cooked_directory};
use regia_core::id::TypePathHash;
use regia_framework::abilities::{
    ABILITIES_COOKED_TYPE_PATH, AbilityAssets, AbilityLoad, has_errors, load_ability_assets,
};
use regia_test_utils::unique_temp_dir;

const ATTRS: &str = r#"(format_version: 1, description: "Hero stats", attributes: [
    (name: "health_max", base: 100.0, min: 1.0, max: 9999.0),
    (name: "health", base: 100.0, min: 0.0, max: 9999.0, max_from: Some("health_max")),
    (name: "mana", base: 50.0, min: 0.0, max: 100.0),
])"#;

const MANA: &str = r#"(format_version: 1, name: "mana_10", lifetime: Instant,
 base_edits: [(attribute: "mana", op: AddPre, amount: Const(-10.0))])"#;

const COOLDOWN: &str = r#"(format_version: 1, name: "cd", lifetime: Timed(Const(2.0)),
 status_tags: ["Cooldown.Fire"])"#;

const POISON: &str = r#"(format_version: 1, name: "poison",
 lifetime: Timed(Const(6.0)), pulse: Some((every: 1.0, on_apply: false)),
 base_edits: [(attribute: "health", op: AddPre, amount: Curve((table: "", name: "poison")))],
 status_tags: ["Status.Poisoned"],
 curves: [(name: "poison", points: [(1.0, -2.0), (10.0, -9.0)])])"#;

/// Named before its cost and cooldown alphabetically: the cook must order by reference, not name.
const BOLT: &str = r#"(format_version: 1, name: "fire_bolt", kind_tags: ["Ability.Spell.Fire"],
 cost: Some("effects/mana_10.effect.ron"), cooldown: Some("effects/cd.effect.ron"),
 charge: OnStart, run_state: ScratchOnly, max_run_ticks: Some(120))"#;

const FILES: [(&str, &str); 5] = [
    ("abilities/a_bolt.ability.ron", BOLT),
    ("effects/cd.effect.ron", COOLDOWN),
    ("effects/mana_10.effect.ron", MANA),
    ("effects/poison.effect.ron", POISON),
    ("stats/hero.attrs.ron", ATTRS),
];

fn project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = unique_temp_dir(tag);
    for (rel, text) in files {
        let file = root.join("assets").join(rel);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
        std::fs::write(file, text).expect("write");
    }
    root
}

fn cook(root: &Path) -> CookReport {
    cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook")
}

fn paths() -> Vec<String> {
    FILES.iter().map(|(p, _)| (*p).to_owned()).collect()
}

/// The registry the RON gives through the text path.
fn from_ron() -> AbilityAssets {
    let mut assets = AbilityAssets::default();
    // Effects first: the ability is validated against its cost and cooldown.
    for (path, text) in FILES.iter().rev() {
        let d = assets.load_text(path, text, None, None);
        assert!(!has_errors(&d), "{path}: {d:?}");
    }
    assets
}

fn assert_same(loaded: &AbilityAssets, ron: &AbilityAssets) {
    assert_eq!(
        loaded.attributes("stats/hero.attrs.ron"),
        ron.attributes("stats/hero.attrs.ron")
    );
    assert!(loaded.attributes("stats/hero.attrs.ron").is_some());
    for path in [
        "effects/cd.effect.ron",
        "effects/mana_10.effect.ron",
        "effects/poison.effect.ron",
    ] {
        assert!(loaded.effect(path).is_some(), "{path}");
        assert_eq!(loaded.effect(path), ron.effect(path), "{path}");
    }
    assert!(loaded.ability("abilities/a_bolt.ability.ron").is_some());
    assert_eq!(
        loaded.ability("abilities/a_bolt.ability.ron"),
        ron.ability("abilities/a_bolt.ability.ron")
    );
}

/// As a shipped game has them: the pak lists every ability asset as `abilities-cooked`, no source
/// text exists, and the loader reads them to the values the RON gives.
#[test]
fn a_packaged_project_lists_the_cooked_ability_assets_and_loads_them_like_the_ron() {
    let root = project("abilities-cook-pak", &FILES);
    let report = cook(&root);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let cache = cooked_cache_dir(&root, &current_platform());
    let content = root.join("content");
    let paks = pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pack");
    let pak = PakReader::open(&paks[0]).expect("open the pak");
    for path in paths() {
        let entry = pak
            .manifest()
            .iter()
            .find(|e| e.path == format!("assets/{path}"))
            .unwrap_or_else(|| panic!("the pak lists {path}"));
        assert_eq!(entry.type_path, ABILITIES_COOKED_TYPE_PATH, "{path}");
        assert_eq!(
            pak.entry(entry.id).map(|e| e.type_tag),
            Some(TypePathHash::of(ABILITIES_COOKED_TYPE_PATH)),
            "{path}"
        );
    }

    // A second root with no `assets/` and no cooked cache: only the pak can answer.
    let shipped = unique_temp_dir("abilities-cook-pak-game");
    let server = AssetServer::open(AssetServerSettings {
        project_root: shipped,
        platform: current_platform(),
        content_dir: Some(content),
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    let loads = load_ability_assets(&mut world, &paths());
    assert_eq!(loads.len(), FILES.len());
    for (path, load) in &loads {
        assert!(load.loaded(), "{path}: {:?}", load.diagnostics);
        assert!(load.cooked, "{path}: there is no text to read");
    }
    assert_same(world.resource::<AbilityAssets>(), &from_ron());
}

/// A package ships the raw files beside its paks (for hosts that read them): with paks mounted the
/// loader still reads the cooked form, so a raw copy that differs from the cook does not win.
#[test]
fn a_packaged_game_reads_the_pak_before_raw_copies() {
    let root = project("abilities-cook-pak-raw", &FILES);
    assert!(cook(&root).errors.is_empty());
    let cache = cooked_cache_dir(&root, &current_platform());
    let content = root.join("content");
    pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pack");
    // The shipped root holds a raw copy that differs from what was cooked.
    let shipped = project(
        "abilities-cook-pak-raw-game",
        &[(
            "effects/mana_10.effect.ron",
            MANA.replace("-10.0", "-99.0").as_str(),
        )],
    );
    let server = AssetServer::open(AssetServerSettings {
        project_root: shipped,
        platform: current_platform(),
        content_dir: Some(content),
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    let loads = load_ability_assets(&mut world, &paths());
    assert!(loads.values().all(|l| l.loaded() && l.cooked), "{loads:?}");
    assert_same(world.resource::<AbilityAssets>(), &from_ron());
}

/// A dev tree runs what is on disk: the loose RON, whatever the last cook wrote.
#[test]
fn a_dev_tree_loads_the_loose_ron() {
    let root = project("abilities-cook-dev", &FILES);
    assert!(cook(&root).errors.is_empty());
    let server = AssetServer::open(AssetServerSettings {
        project_root: root,
        platform: current_platform(),
        content_dir: None,
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    let loads = load_ability_assets(&mut world, &paths());
    assert!(
        loads.values().all(|l: &AbilityLoad| l.loaded() && !l.cooked),
        "{loads:?}"
    );
    assert_same(world.resource::<AbilityAssets>(), &from_ron());
}

/// Two abilities naming each other as cost: the cook fails both with `abilities_reference_cycle`.
#[test]
fn a_cyclic_reference_fails_the_cook_with_its_code() {
    let a =
        r#"(format_version: 1, name: "a", cost: Some("abilities/b.ability.ron"), run_state: ScratchOnly)"#;
    let b = r#"(format_version: 1, name: "b", cooldown: Some("abilities/a.ability.ron"), run_state: ScratchOnly)"#;
    let root = project(
        "abilities-cook-cycle",
        &[("abilities/a.ability.ron", a), ("abilities/b.ability.ron", b)],
    );
    let report = cook(&root);
    for path in ["assets/abilities/a.ability.ron", "assets/abilities/b.ability.ron"] {
        assert!(
            report
                .errors
                .iter()
                .any(|(p, e)| p == path && e.contains("abilities_reference_cycle")),
            "{path}: {:?}",
            report.errors
        );
    }
}

/// A cost naming a file the project lacks fails the cook with `abilities_reference_missing`; the
/// rest of the project still cooks.
#[test]
fn a_missing_reference_fails_the_cook_with_its_code() {
    let bolt =
        r#"(format_version: 1, name: "bolt", cost: Some("effects/gone.effect.ron"), run_state: ScratchOnly)"#;
    let root = project(
        "abilities-cook-missing",
        &[
            ("abilities/bolt.ability.ron", bolt),
            ("effects/mana_10.effect.ron", MANA),
        ],
    );
    let report = cook(&root);
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    let (path, error) = &report.errors[0];
    assert_eq!(path, "assets/abilities/bolt.ability.ron");
    assert!(error.contains("abilities_reference_missing"), "{error}");
}

/// Editing a cost effect so it no longer validates re-checks the ability naming it, though the
/// ability's own file did not change.
#[test]
fn a_cost_going_bad_fails_the_ability_that_names_it() {
    let root = project("abilities-cook-edit", &FILES);
    assert!(cook(&root).errors.is_empty());
    // A cost must be an instant effect.
    std::fs::write(root.join("assets/effects/mana_10.effect.ron"), COOLDOWN).expect("write");
    let report = cook(&root);
    assert!(
        report
            .errors
            .iter()
            .any(|(p, e)| p == "assets/abilities/a_bolt.ability.ron" && e.contains("ability_bad_cost")),
        "{:?}",
        report.errors
    );
}
