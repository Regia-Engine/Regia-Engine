// SPDX-License-Identifier: MPL-2.0
//! Cook order and the runtime loader of the ability formats (spec 53 § 4.1, ADR-9370): costs and
//! cooldowns load before the abilities naming them, a missing reference fails a strict order with
//! `abilities_reference_missing`, a loop fails every asset on it with `abilities_reference_cycle`,
//! the set cook leaves run-state type checks to the game, and the startup loader reads a project's
//! loose RON by asset path.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings, current_platform};
use regia_framework::abilities::*;
use regia_test_utils::unique_temp_dir;

const MANA: &str = r#"(format_version: 1, name: "mana_10", lifetime: Instant,
 base_edits: [(attribute: "mana", op: AddPre, amount: Const(-10.0))])"#;

const COOLDOWN: &str = r#"(format_version: 1, name: "cd", lifetime: Timed(Const(2.0)),
 status_tags: ["Cooldown.Fire"])"#;

const BOLT: &str = r#"(format_version: 1, name: "bolt", cost: Some("z/mana.effect.ron"),
 cooldown: Some("z/cd.effect.ron"), run_state: ScratchOnly)"#;

fn parsed(files: &[(&str, &str)]) -> BTreeMap<String, CookedAbilityAsset> {
    files
        .iter()
        .map(|(path, text)| {
            let asset = match AssetKind::of(path).expect("an ability format") {
                AssetKind::Attributes => CookedAbilityAsset::Attributes(
                    parse_ron(text, ATTRIBUTES_FORMAT_VERSION).expect("parses"),
                ),
                AssetKind::Effect => {
                    CookedAbilityAsset::Effect(parse_ron(text, EFFECT_FORMAT_VERSION).expect("parses"))
                }
                AssetKind::Ability => {
                    CookedAbilityAsset::Ability(parse_ron(text, ABILITY_FORMAT_VERSION).expect("parses"))
                }
            };
            ((*path).to_owned(), asset)
        })
        .collect()
}

fn texts(files: &[(&str, &str)]) -> BTreeMap<String, String> {
    files
        .iter()
        .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
        .collect()
}

#[test]
fn costs_and_cooldowns_load_before_the_ability_naming_them() {
    let set = parsed(&[
        ("a/bolt.ability.ron", BOLT),
        ("z/mana.effect.ron", MANA),
        ("z/cd.effect.ron", COOLDOWN),
    ]);
    let order = load_order(&set, true).expect("orders");
    assert_eq!(
        order,
        vec!["z/cd.effect.ron", "z/mana.effect.ron", "a/bolt.ability.ron"],
        "references first, then by path"
    );
    assert_eq!(
        load_order_references(&set["a/bolt.ability.ron"]),
        vec!["z/mana.effect.ron", "z/cd.effect.ron"]
    );
}

#[test]
fn a_missing_reference_fails_a_strict_order_and_not_a_loose_one() {
    let set = parsed(&[("a/bolt.ability.ron", BOLT), ("z/mana.effect.ron", MANA)]);
    let errors = load_order(&set, true).expect_err("the cooldown is missing");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].0, "a/bolt.ability.ron");
    assert_eq!(errors[0].1.code, "abilities_reference_missing");
    // At run time the reference may be loaded already: the order leaves it to the registry.
    assert_eq!(
        load_order(&set, false).expect("orders"),
        vec!["z/mana.effect.ron", "a/bolt.ability.ron"]
    );
}

#[test]
fn a_loop_of_references_fails_every_asset_on_it() {
    let a = r#"(format_version: 1, name: "a", cost: Some("b.ability.ron"))"#;
    let b = r#"(format_version: 1, name: "b", cooldown: Some("c.ability.ron"))"#;
    let c = r#"(format_version: 1, name: "c", cost: Some("a.ability.ron"))"#;
    let set = parsed(&[
        ("a.ability.ron", a),
        ("b.ability.ron", b),
        ("c.ability.ron", c),
        ("z/mana.effect.ron", MANA),
    ]);
    let errors = load_order(&set, true).expect_err("a loop");
    let paths: Vec<&str> = errors.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, vec!["a.ability.ron", "b.ability.ron", "c.ability.ron"]);
    assert!(errors.iter().all(|(_, d)| d.code == "abilities_reference_cycle"));
    assert!(
        errors[0]
            .1
            .message
            .contains("a.ability.ron -> b.ability.ron -> c.ability.ron -> a.ability.ron"),
        "{}",
        errors[0].1.message
    );
    let cooked = cook_ability_set(
        &texts(&[("a.ability.ron", a), ("b.ability.ron", b), ("c.ability.ron", c)]),
        None,
    )
    .expect_err("the cook fails");
    assert_eq!(cooked.len(), 3);
    assert!(cooked.iter().all(|(_, d)| d.code == "abilities_reference_cycle"));
}

/// The set cook validates in order (the ability against its cost and cooldown) and leaves the
/// game's run-state types to the game's load; the bytes load back to the RON's values.
#[test]
fn the_set_cook_validates_in_order_and_leaves_run_state_types_to_the_game() {
    let keep = r#"(format_version: 1, name: "keep", cost: Some("z/mana.effect.ron"),
        run_state: KeepAcrossRuns, run_type: "the_games_type")"#;
    let files = texts(&[
        ("a/bolt.ability.ron", BOLT),
        ("a/keep.ability.ron", keep),
        ("z/mana.effect.ron", MANA),
        ("z/cd.effect.ron", COOLDOWN),
    ]);
    let cooked = cook_ability_set(&files, None).expect("cooks");
    assert_eq!(cooked.len(), 4);
    let mut text = AbilityAssets::default();
    let mut bytes = AbilityAssets::default();
    for path in ["z/cd.effect.ron", "z/mana.effect.ron", "a/bolt.ability.ron"] {
        assert!(
            !has_errors(&text.load_text(path, &files[path], None, None)),
            "{path}"
        );
        assert!(
            !has_errors(&bytes.load_cooked(path, &cooked[path], None, None)),
            "{path}"
        );
    }
    assert_eq!(
        text.ability("a/bolt.ability.ron"),
        bytes.ability("a/bolt.ability.ron")
    );
    // The game's load still checks the run-state type.
    let refused = bytes.load_cooked("a/keep.ability.ron", &cooked["a/keep.ability.ron"], None, None);
    assert!(
        refused.iter().any(|d| d.code == "unknown_run_state"),
        "{refused:?}"
    );
    // A cost that is not instant fails the set cook with the text path's code.
    let mut bad = files.clone();
    bad.insert("z/mana.effect.ron".into(), COOLDOWN.into());
    let errors = cook_ability_set(&bad, None).expect_err("bad cost");
    assert!(
        errors
            .iter()
            .any(|(p, d)| p == "a/bolt.ability.ron" && d.code == "ability_bad_cost"),
        "{errors:?}"
    );
}

/// The startup loader reads registered paths from the project's `assets/` (a dev tree) in load
/// order, whatever order they were registered in.
#[test]
fn the_startup_loader_reads_registered_paths_from_a_dev_tree() {
    let root = unique_temp_dir("abilities-load-dev");
    for (rel, text) in [
        ("a/bolt.ability.ron", BOLT),
        ("z/mana.effect.ron", MANA),
        ("z/cd.effect.ron", COOLDOWN),
    ] {
        let file = root.join("assets").join(rel);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
        std::fs::write(file, text).expect("write");
    }
    let server = AssetServer::open(AssetServerSettings {
        project_root: root,
        platform: current_platform(),
        content_dir: None,
    })
    .expect("open");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(server);
    app.load_ability_assets_at_startup(["a/bolt.ability.ron", "z/mana.effect.ron"])
        .load_ability_assets_at_startup(["z/cd.effect.ron", "missing/x.effect.ron"]);
    app.update();
    let assets = app.world().resource::<AbilityAssets>();
    assert!(assets.ability("a/bolt.ability.ron").is_some());
    assert!(assets.effect("z/cd.effect.ron").is_some());
    let registrations = app.world().resource::<AbilityAssetRegistrations>();
    assert_eq!(registrations.paths().count(), 4);
    let missing = &registrations.loads["missing/x.effect.ron"];
    assert!(!missing.loaded());
    assert_eq!(missing.diagnostics[0].code, "asset_not_found");
    assert!(registrations.loads["a/bolt.ability.ron"].loaded());
    assert!(!registrations.loads["a/bolt.ability.ron"].cooked);
}
