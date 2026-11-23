// SPDX-License-Identifier: MPL-2.0
//! The cook of gameplay data tables end to end (spec 51 § 4.4, spec 39, ADR-2012): real
//! `cook_project` runs over a temporary project, the pak they pack, and a load from that pak with
//! the source files gone, as in a shipped game.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_ai::smart_object_def::{SMART_OBJECT_DEF_TYPE_PATH, SmartObjectDef};
use regia_ai::smart_object_load::SmartObjectDefs;
use regia_ai::{SmartObject, SmartObjectIndex, SmartObjectPlugin, SmartObjectSlot};
use regia_app::prelude::*;
use regia_asset::registry::{AssetRegistry, cache_registry_path};
use regia_asset::{
    AssetId, AssetManifest, AssetRef, AssetServer, AssetServerSettings, CookedHeader, SourceAssetChanged,
    cooked_cache_dir, cooked_file_name, current_platform, decode_cooked_file,
};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project, pack_cooked_directory};
use regia_framework::chooser::{
    CHOOSER_TYPE_PATH, ChooserRegistrations, ChooserTable, Choosers, load_registered_choosers,
};
use regia_framework::curve_table::{
    CURVE_TABLE_TYPE_PATH, CurveTable, CurveTableRegistrations, CurveTables, TableInterp,
    load_registered_curve_tables,
};
use regia_test_utils::unique_temp_dir;

const CURVES: &str = r#"(format_version: 1, interp: Cubic, curves: {
    "falloff": (post: Linear, keys: [(0.0, 1.0), (10.0, 0.6), (30.0, 0.0)]),
    "xp": (keys: [(x: 1.0, y: 0.0, interp: Step), (x: 2.0, y: 100.0, out_tangent: 160.0)]),
})"#;

const CSV: &str = "name,0,1,2\nfalloff,1,0.5,0\nramp,,1,2\n";

const NESTED: &str = r#"(format_version: 1, result: Asset("any"),
    rows: [(id: "only", cells: {}, result: Asset("anim/only.montage.ron"))])"#;

const MONTAGES: &str = r#"(format_version: 1, result: Asset("montage"),
    rows: [(id: "only", cells: {}, result: Asset("anim/only.montage.ron"))])"#;

const CHOOSER: &str = r#"(
    format_version: 1,
    mode: Best,
    result: Asset("any"),
    inputs: { "angle": Float, "held": Asset },
    outputs: { "skin": Asset },
    columns: [
        (id: "angle", input: "angle", kind: FloatDistance(max_distance: 180.0)),
        (id: "held", input: "held", kind: Asset),
        (id: "skin", output: "skin", kind: SetAsset),
    ],
    rows: [
        (id: "front", cells: { "angle": 0.0, "held": Is("props/crate.material.ron") },
         result: Asset("anim/front.montage.ron")),
        (id: "nested", cells: { "angle": 90.0, "skin": "props/other.material.ron" },
         result: Chooser("anim/nested.chooser.ron")),
    ],
    catch_all: Some((result: Asset("tables/falloff.curves.ron"))),
)"#;

const SMART: &str = r#"SmartObjectDef(
    format_version: 1,
    activity: ["Activity.Sit"],
    slots: [(name: "left"), (name: "right")],
)"#;

const MATERIAL: &str = r#"(
    format_version: 3,
    name: "M_Crate",
    shader: Standard,
    base: (base_color: (1.0, 0.5, 0.5, 1.0), metallic: 0.0, roughness: 0.6),
)"#;

/// A project with `files` under `assets/`.
fn project(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = unique_temp_dir(tag);
    for (rel, text) in files {
        write(&root, rel, text);
    }
    root
}

fn write(root: &Path, rel: &str, text: &str) {
    let file = root.join("assets").join(rel);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("dir");
    std::fs::write(file, text).expect("write");
}

fn cook(root: &Path) -> CookReport {
    cook_project(CookOptions::for_host(root.to_path_buf())).expect("cook")
}

fn cooked(root: &Path, path: &str) -> (CookedHeader, Vec<u8>, String) {
    let cache = cooked_cache_dir(root, &current_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("a cook wrote one");
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("{path} is not in the manifest"));
    let file = std::fs::read(cache.join(cooked_file_name(entry.id))).expect("cooked file");
    let (header, payload) = decode_cooked_file(&file).expect("decode");
    (header, payload.to_vec(), entry.type_path.clone())
}

fn the_project(tag: &str) -> PathBuf {
    project(
        tag,
        &[
            ("tables/falloff.curves.ron", CURVES),
            ("tables/ramp.curves.csv", CSV),
            ("anim/hit.chooser.ron", CHOOSER),
            ("anim/nested.chooser.ron", NESTED),
            ("anim/montages.chooser.ron", MONTAGES),
            ("anim/front.montage.ron", "(not: \"cooked\")"),
            ("props/crate.material.ron", MATERIAL),
            ("props/other.material.ron", MATERIAL),
            ("props/chair.smartobject.ron", SMART),
        ],
    )
}

/// `report` with the non-cookable files taken out.
fn real_errors(report: &CookReport) -> Vec<&(String, String)> {
    report
        .errors
        .iter()
        .filter(|(_, e)| !e.starts_with("unsupported source extension"))
        .collect()
}

#[test]
fn tables_cook_to_their_own_forms_and_decode_to_their_sources() {
    let root = the_project("data-cook-forms");
    let report = cook(&root);
    assert!(real_errors(&report).is_empty(), "{:?}", report.errors);

    let (_, payload, type_path) = cooked(&root, "assets/tables/falloff.curves.ron");
    assert_eq!(type_path, CURVE_TABLE_TYPE_PATH);
    assert_eq!(
        CurveTable::from_cooked(&payload).expect("load"),
        CurveTable::from_ron_str(CURVES).expect("source")
    );

    let (_, payload, type_path) = cooked(&root, "assets/tables/ramp.curves.csv");
    assert_eq!(type_path, CURVE_TABLE_TYPE_PATH);
    assert_eq!(
        CurveTable::from_cooked(&payload).expect("load"),
        CurveTable::from_csv_str(CSV, TableInterp::Linear).expect("source")
    );

    let (_, payload, type_path) = cooked(&root, "assets/anim/hit.chooser.ron");
    assert_eq!(type_path, CHOOSER_TYPE_PATH);
    assert_eq!(
        ChooserTable::from_cooked(&payload).expect("load"),
        ChooserTable::from_ron_str(CHOOSER).expect("source")
    );

    let (_, payload, type_path) = cooked(&root, "assets/props/chair.smartobject.ron");
    assert_eq!(type_path, SMART_OBJECT_DEF_TYPE_PATH);
    let text = String::from_utf8(payload).expect("text");
    assert_eq!(
        SmartObjectDef::from_ron(&text).expect("load"),
        SmartObjectDef::from_ron(SMART).expect("source")
    );
}

/// The ids of a first cook: the chooser cooks last, so assets later in path order have theirs.
#[test]
fn a_chooser_depends_on_what_it_names_and_not_on_raw_files() {
    let root = the_project("data-cook-deps");
    assert!(real_errors(&cook(&root)).is_empty());
    let id_of = |path: &str| cooked(&root, path).0.asset_id;
    let (header, _, _) = cooked(&root, "assets/anim/hit.chooser.ron");
    let mut wanted = vec![
        id_of("assets/anim/nested.chooser.ron"),
        id_of("assets/props/crate.material.ron"),
        id_of("assets/props/other.material.ron"),
        id_of("assets/tables/falloff.curves.ron"),
    ];
    wanted.sort();
    assert_eq!(
        header.dependencies, wanted,
        "`front.montage.ron` is raw: no pak entry"
    );
}

#[test]
fn the_registry_records_kinds_results_and_dependencies() {
    let root = the_project("data-cook-registry");
    assert!(real_errors(&cook(&root)).is_empty());
    let cache = cooked_cache_dir(&root, &current_platform());
    let registry = AssetRegistry::read_from(&cache_registry_path(&cache))
        .expect("read")
        .expect("a cook wrote one");
    let tags: BTreeMap<&str, (Option<String>, Option<String>)> = registry
        .entries()
        .map(|e| {
            (
                e.path.as_str(),
                (e.tags.get("kind").cloned(), e.tags.get("result").cloned()),
            )
        })
        .collect();
    let kind = |path: &str| tags.get(path).and_then(|t| t.0.as_deref());
    assert_eq!(kind("assets/tables/falloff.curves.ron"), Some("curve_table"));
    assert_eq!(kind("assets/tables/ramp.curves.csv"), Some("curve_table"));
    assert_eq!(kind("assets/props/chair.smartobject.ron"), Some("smart_object"));
    assert_eq!(kind("assets/anim/hit.chooser.ron"), Some("chooser"));
    assert_eq!(kind("assets/props/crate.material.ron"), None);
    assert_eq!(
        tags["assets/anim/hit.chooser.ron"].1.as_deref(),
        Some("any"),
        "result="
    );
    assert_eq!(
        tags["assets/anim/montages.chooser.ron"].1.as_deref(),
        Some("montage")
    );
    let hit = registry
        .entries()
        .find(|e| e.path == "assets/anim/hit.chooser.ron")
        .expect("entry");
    assert_eq!(hit.dependencies.len(), 4);
    assert!(hit.dependencies.iter().all(|d| registry.by_id(*d).is_some()));
}

#[test]
fn a_table_with_errors_fails_the_cook_and_its_warnings_do_not() {
    let root = project(
        "data-cook-errors",
        &[
            (
                "bad.curves.ron",
                r#"(format_version: 1, curves: { "a": (keys: []) })"#,
            ),
            ("bad.curves.csv", "name,1,1\na,0,0\n"),
            ("bad.chooser.ron", "(format_version: 1)"),
            ("new.chooser.ron", r#"(format_version: 9, result: OutputOnly)"#),
            (
                "bad.smartobject.ron",
                "SmartObjectDef(format_version: 1, slots: [(name: \"a b\")])",
            ),
            (
                // Warning: `b` can never be selected. Info: no catch-all elsewhere.
                "warned.chooser.ron",
                r#"(format_version: 1, result: OutputOnly, rows: [
                    (id: "a", cells: {}), (id: "b", cells: {})])"#,
            ),
            ("good.curves.ron", CURVES),
        ],
    );
    let report = cook(&root);
    let errors: BTreeMap<&str, &str> = report
        .errors
        .iter()
        .map(|(path, error)| (path.as_str(), error.as_str()))
        .collect();
    let error = |path: &str| errors.get(path).copied().unwrap_or("no error");
    assert!(
        error("assets/bad.curves.ron").contains("curve_empty"),
        "{errors:?}"
    );
    assert!(
        error("assets/bad.curves.csv").contains("curve_csv_ragged"),
        "{errors:?}"
    );
    assert!(
        error("assets/bad.chooser.ron").contains("chooser_bad_file"),
        "{errors:?}"
    );
    assert!(
        error("assets/new.chooser.ron").contains("chooser_unsupported_version"),
        "{errors:?}"
    );
    assert!(
        error("assets/bad.smartobject.ron").contains("data table error"),
        "{errors:?}"
    );
    assert_eq!(errors.len(), 5, "warnings and good tables cook: {errors:?}");
}

#[test]
fn a_chooser_whose_reference_does_not_resolve_fails_the_cook() {
    let root = project(
        "data-cook-refs",
        &[
            (
                "missing.chooser.ron",
                r#"(format_version: 1, result: Asset("any"), rows: [
                    (id: "a", cells: {}, result: Chooser("gone.chooser.ron"))])"#,
            ),
            (
                "a.chooser.ron",
                r#"(format_version: 1, result: Asset("any"), rows: [
                    (id: "a", cells: {}, result: Chooser("b.chooser.ron"))])"#,
            ),
            (
                "b.chooser.ron",
                r#"(format_version: 1, result: Asset("any"), rows: [
                    (id: "b", cells: {}, result: Chooser("a.chooser.ron"))])"#,
            ),
        ],
    );
    let report = cook(&root);
    let error = |path: &str| {
        report
            .errors
            .iter()
            .find(|(p, _)| p == path)
            .map_or("no error", |(_, e)| e.as_str())
    };
    assert!(
        error("assets/missing.chooser.ron").contains("chooser_ref_unresolved"),
        "{report:?}"
    );
    assert!(
        error("assets/a.chooser.ron").contains("chooser_cycle"),
        "{report:?}"
    );
    assert!(
        error("assets/b.chooser.ron").contains("chooser_cycle"),
        "{report:?}"
    );
}

#[test]
fn a_nested_chooser_going_bad_recooks_the_table_that_names_it() {
    let root = project(
        "data-cook-nested-edit",
        &[
            ("nested.chooser.ron", NESTED),
            (
                "top.chooser.ron",
                r#"(format_version: 1, result: Asset("any"), rows: [
                    (id: "a", cells: {}, result: Chooser("nested.chooser.ron"))])"#,
            ),
        ],
    );
    assert!(real_errors(&cook(&root)).is_empty());
    let again = cook(&root);
    assert_eq!(again.cooked, 0, "an unchanged project cooks nothing");
    // The new nested table names the top one back: the top file did not change, its check did.
    write(
        &root,
        "nested.chooser.ron",
        r#"(format_version: 1, result: Asset("any"), rows: [
            (id: "b", cells: {}, result: Chooser("top.chooser.ron"))])"#,
    );
    let report = cook(&root);
    assert!(
        report
            .errors
            .iter()
            .any(|(p, e)| p == "assets/top.chooser.ron" && e.contains("chooser_cycle")),
        "{report:?}"
    );
}

/// As a shipped game has them: the pak holds the tables and no source text exists.
#[test]
fn a_packed_table_loads_from_the_pak_with_the_sources_gone() {
    let root = the_project("data-cook-pak");
    assert!(real_errors(&cook(&root)).is_empty());
    let cache = cooked_cache_dir(&root, &current_platform());
    let content = root.join("content");
    pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pack");
    // A second root with no `assets/` and no cooked cache: only the pak can answer.
    let shipped = unique_temp_dir("data-cook-pak-game");
    let server = AssetServer::open(AssetServerSettings {
        project_root: shipped.clone(),
        platform: current_platform(),
        content_dir: Some(content),
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    world.init_resource::<CurveTables>();
    world.init_resource::<CurveTableRegistrations>();
    world.init_resource::<Choosers>();
    world.init_resource::<ChooserRegistrations>();
    world
        .resource_mut::<CurveTableRegistrations>()
        .add("tables/falloff.curves.ron");
    world
        .resource_mut::<ChooserRegistrations>()
        .add("anim/hit.chooser.ron");
    load_registered_curve_tables(&mut world);
    load_registered_choosers(&mut world);

    let tables = world.resource::<CurveTables>();
    let slot = tables.slot("tables/falloff.curves.ron").expect("slot");
    assert!(slot.last_issues.is_empty(), "{:?}", slot.last_issues);
    assert!(slot.cooked, "there is no text to read");
    assert_eq!(slot.table, CurveTable::from_ron_str(CURVES).expect("source"));

    let choosers = world.resource::<Choosers>();
    let slot = choosers.slot("anim/hit.chooser.ron").expect("slot");
    assert!(slot.cooked);
    assert_eq!(slot.table, ChooserTable::from_ron_str(CHOOSER).expect("source"));
}

/// A dev tree always runs what is on disk, whatever the last cook wrote.
#[test]
fn source_text_wins_over_a_stale_cook() {
    let root = the_project("data-cook-stale");
    assert!(real_errors(&cook(&root)).is_empty());
    write(
        &root,
        "tables/falloff.curves.ron",
        r#"(format_version: 1, curves: { "edited": (keys: [(0.0, 1.0)]) })"#,
    );
    let server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: current_platform(),
        content_dir: None,
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    world.init_resource::<CurveTables>();
    world.init_resource::<CurveTableRegistrations>();
    world
        .resource_mut::<CurveTableRegistrations>()
        .add("tables/falloff.curves.ron");
    load_registered_curve_tables(&mut world);
    let slot = world
        .resource::<CurveTables>()
        .slot("tables/falloff.curves.ron")
        .expect("slot");
    assert!(!slot.cooked);
    assert!(slot.table.curve("edited").is_some());
}

/// A manifest entry of the wrong type is an error, not a misread payload.
#[test]
fn a_cooked_asset_of_another_type_is_refused() {
    let root = the_project("data-cook-wrong-type");
    assert!(real_errors(&cook(&root)).is_empty());
    let cache = cooked_cache_dir(&root, &current_platform());
    let content = root.join("content");
    pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pack");
    let server = AssetServer::open(AssetServerSettings {
        project_root: unique_temp_dir("data-cook-wrong-type-game"),
        platform: current_platform(),
        content_dir: Some(content),
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    world.init_resource::<CurveTables>();
    world.init_resource::<CurveTableRegistrations>();
    // Registered under the path of a material: the payload must not be decoded as a curve table.
    world
        .resource_mut::<CurveTableRegistrations>()
        .add("props/crate.material.ron");
    load_registered_curve_tables(&mut world);
    let slot = world
        .resource::<CurveTables>()
        .slot("props/crate.material.ron")
        .expect("slot");
    assert_eq!(slot.generation, 0);
    assert_eq!(slot.last_issues[0].code, "curve_bad_file");
    assert!(
        slot.last_issues[0].message.contains("is not a"),
        "{:?}",
        slot.last_issues
    );
}

fn smart_object_app(server: AssetServer) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SmartObjectPlugin));
    app.insert_resource(server);
    app
}

fn slot_roles(app: &mut App, object: Entity) -> Vec<(String, Entity)> {
    let mut query = app
        .world_mut()
        .query::<(Entity, &SmartObjectSlot, &regia_ecs::prelude::ChildOf)>();
    let mut roles: Vec<(String, Entity)> = query
        .iter(app.world())
        .filter(|(_, _, parent)| parent.parent() == object)
        .map(|(entity, slot, _)| (slot.role.clone(), entity))
        .collect();
    roles.sort();
    roles
}

/// As a shipped game has it: the scene's id resolves through the pak manifest, and no text exists.
#[test]
fn a_smart_object_reference_loads_from_the_pak() {
    let root = the_project("data-cook-smart-pak");
    assert!(real_errors(&cook(&root)).is_empty());
    let cache = cooked_cache_dir(&root, &current_platform());
    let content = root.join("content");
    pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pack");
    let id = cooked(&root, "assets/props/chair.smartobject.ron").0.asset_id;
    let server = AssetServer::open(AssetServerSettings {
        project_root: unique_temp_dir("data-cook-smart-pak-game"),
        platform: current_platform(),
        content_dir: Some(content),
    })
    .expect("open");
    let mut app = smart_object_app(server);
    let object = app
        .world_mut()
        .spawn(SmartObject {
            definition: Some(AssetRef::new(id)),
            ..SmartObject::default()
        })
        .id();
    app.update();
    app.update();
    let roles: Vec<String> = slot_roles(&mut app, object).into_iter().map(|r| r.0).collect();
    assert_eq!(roles, ["left", "right"]);
    let defs = app.world().resource::<SmartObjectDefs>();
    let slot = defs.slot(id).expect("slot");
    assert!(slot.cooked, "there is no text to read");
    assert_eq!(slot.error, None);
    assert_eq!(slot.path.as_deref(), Some("props/chair.smartobject.ron"));
}

#[test]
fn objects_naming_one_definition_share_one_load() {
    let root = the_project("data-cook-smart-share");
    assert!(real_errors(&cook(&root)).is_empty());
    let id = cooked(&root, "assets/props/chair.smartobject.ron").0.asset_id;
    let server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: current_platform(),
        content_dir: None,
    })
    .expect("open");
    let mut app = smart_object_app(server);
    for _ in 0..3 {
        app.world_mut().spawn(SmartObject {
            definition: Some(AssetRef::new(id)),
            ..SmartObject::default()
        });
    }
    app.update();
    app.update();
    app.update();
    assert_eq!(app.world().resource::<SmartObjectIndex>().slots().count(), 6);
    let defs = app.world().resource::<SmartObjectDefs>();
    assert_eq!(
        defs.slot(id).expect("slot").generation,
        1,
        "read once for three objects"
    );
}

/// Spec 39 test 5 through the file path: an edit reconciles by `def_slot`, survivors keep their
/// entity, and a bad edit keeps what is in use and says why.
#[test]
fn a_changed_definition_file_reloads_and_a_bad_one_is_refused() {
    let root = the_project("data-cook-smart-reload");
    assert!(real_errors(&cook(&root)).is_empty());
    let id = cooked(&root, "assets/props/chair.smartobject.ron").0.asset_id;
    let server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: current_platform(),
        content_dir: None,
    })
    .expect("open");
    let mut app = smart_object_app(server);
    let object = app
        .world_mut()
        .spawn(SmartObject {
            definition: Some(AssetRef::new(id)),
            ..SmartObject::default()
        })
        .id();
    app.update();
    app.update();
    let before = slot_roles(&mut app, object);
    assert_eq!(before.len(), 2);

    let file = root.join("assets/props/chair.smartobject.ron");
    write(
        &root,
        "props/chair.smartobject.ron",
        r#"SmartObjectDef(format_version: 1, slots: [(name: "left"), (name: "right"), (name: "back")])"#,
    );
    app.world_mut()
        .write_message(SourceAssetChanged::new(file.clone()));
    app.update();
    app.update();
    let after = slot_roles(&mut app, object);
    assert_eq!(after.len(), 3);
    let keeps = |role: &str| {
        let was = before.iter().find(|r| r.0 == role).expect("before");
        after.iter().any(|r| r == was)
    };
    assert!(keeps("left") && keeps("right"), "survivors keep their entities");
    let slot = app
        .world()
        .resource::<SmartObjectDefs>()
        .slot(id)
        .expect("slot")
        .clone();
    assert_eq!(slot.generation, 2);

    write(
        &root,
        "props/chair.smartobject.ron",
        "SmartObjectDef(format_version: 1, slots: [",
    );
    app.world_mut().write_message(SourceAssetChanged::new(file));
    app.update();
    app.update();
    assert_eq!(
        slot_roles(&mut app, object),
        after,
        "a bad file keeps the slots in use"
    );
    let slot = app
        .world()
        .resource::<SmartObjectDefs>()
        .slot(id)
        .expect("slot")
        .clone();
    assert_eq!(slot.generation, 2);
    assert_eq!(slot.error.map(|e| e.reason), Some("bad_definition"));
    assert!(slot.definition.is_some());
}

#[test]
fn a_reference_nothing_resolves_is_reported_once_and_changes_nothing() {
    let root = the_project("data-cook-smart-missing");
    assert!(real_errors(&cook(&root)).is_empty());
    let server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: current_platform(),
        content_dir: None,
    })
    .expect("open");
    let mut app = smart_object_app(server);
    let id = AssetId::new_random();
    let object = app
        .world_mut()
        .spawn(SmartObject {
            definition: Some(AssetRef::new(id)),
            ..SmartObject::default()
        })
        .id();
    for _ in 0..3 {
        app.update();
    }
    assert!(slot_roles(&mut app, object).is_empty());
    let defs = app.world().resource::<SmartObjectDefs>();
    assert_eq!(
        defs.slot(id).and_then(|s| s.error.clone()).map(|e| e.reason),
        Some("definition_not_found")
    );
}

const CAMERA: &str = r#"(format_version: 1, name: "player",
    rigs: [(name: "main", root: [Fov(degrees: Literal(60.0))])],
    director: Hook("the_games_hook"))"#;

/// A camera asset cooks to its canonical text, the file route reads that in a packaged game with no
/// source, and director hooks (the game's code) are not the cook's to check (ADR-2030).
#[test]
fn a_camera_asset_cooks_and_loads_without_its_source() {
    use regia_framework::camera::asset::{CAMERA_ASSET_TYPE_PATH, CameraAsset, CameraBuildStatus};
    use regia_framework::camera::director::CameraHooks;
    use regia_framework::camera::driver::CameraAssets;
    use regia_framework::camera::load::{CameraFiles, load_camera_file};

    let root = project("data-cook-camera", &[("camera/player.camera.ron", CAMERA)]);
    let report = cook(&root);
    assert!(real_errors(&report).is_empty(), "{:?}", report.errors);
    let (_, payload, type_path) = cooked(&root, "assets/camera/player.camera.ron");
    assert_eq!(type_path, CAMERA_ASSET_TYPE_PATH);
    let text = String::from_utf8(payload).expect("text");
    assert_eq!(
        CameraAsset::from_ron(&text).expect("cooked text parses"),
        CameraAsset::from_ron(CAMERA).expect("source")
    );

    // The shipped layout: a pak, and a root with no source file.
    let cache = cooked_cache_dir(&root, &current_platform());
    let content = root.join("content");
    pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pack");
    let server = AssetServer::open(AssetServerSettings {
        project_root: unique_temp_dir("data-cook-camera-game"),
        platform: current_platform(),
        content_dir: Some(content),
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    let mut hooks = CameraHooks::default();
    hooks.accept_any_name();
    world.insert_resource(hooks);
    let status = load_camera_file(&mut world, "camera/player.camera.ron").expect("reads the pak");
    assert_eq!(status, CameraBuildStatus::Ok);
    assert!(world.resource::<CameraAssets>().active("player").is_some());
    let files = world.resource::<CameraFiles>();
    assert_eq!(
        files.get("camera/player.camera.ron").map(|f| f.cooked),
        Some(true)
    );
}

#[test]
fn a_camera_asset_with_errors_fails_the_cook() {
    let root = project(
        "data-cook-camera-bad",
        &[(
            "camera/bad.camera.ron",
            r#"(format_version: 1, name: "bad", rigs: [(name: "a", root: [])], director: Fixed("gone"))"#,
        )],
    );
    let report = cook(&root);
    assert!(
        report
            .errors
            .iter()
            .any(|(path, error)| path.contains("bad.camera.ron") && error.contains("camera_unknown_rig")),
        "{:?}",
        report.errors
    );
}

// ------------------------------------------------------------------ typed data tables (ADR-2132)

/// A row of the typed data tables below.
#[derive(Reflect, serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
struct LootRow {
    weight: u32,
    label: String,
    #[serde(default)]
    tier: Option<u8>,
}

const LOOT: &str = r#"(
    format_version: 1,
    row_type: "LootRow",
    rows: {
        "coin": (weight: 50, label: "Coin", tier: Some(1)),
        "gem": (weight: 5, label: "Gem, cut"),
    },
)"#;

const LOOT_CSV: &str = "key,weight,label,tier\r\ncoin,50,Coin,Some(1)\r\nnote,2,42,\r\n";

/// `.table.ron` and `.table.csv` cook to `data-table-cooked`, the registry records their kind and row
/// type, and a packaged game with the sources gone loads them from the pak to the same typed rows.
#[test]
fn typed_data_tables_cook_and_load_from_the_pak_with_the_sources_gone() {
    use regia_asset::data_table::{
        DATA_TABLE_COOKED_TYPE_PATH, DataTable, DataTableRegistrations, DataTables,
        load_registered_data_tables,
    };
    let root = project(
        "data-cook-typed",
        &[
            ("tables/loot.table.ron", LOOT),
            ("tables/notes.table.csv", LOOT_CSV),
        ],
    );
    assert!(real_errors(&cook(&root)).is_empty());
    let (_, _, type_path) = cooked(&root, "assets/tables/loot.table.ron");
    assert_eq!(type_path, DATA_TABLE_COOKED_TYPE_PATH);
    let (_, _, type_path) = cooked(&root, "assets/tables/notes.table.csv");
    assert_eq!(type_path, DATA_TABLE_COOKED_TYPE_PATH);

    let cache = cooked_cache_dir(&root, &current_platform());
    let registry = AssetRegistry::read_from(&cache_registry_path(&cache))
        .expect("read")
        .expect("a cook wrote one");
    let loot = registry
        .entries()
        .find(|e| e.path == "assets/tables/loot.table.ron")
        .expect("entry");
    assert_eq!(loot.tags.get("kind").map(String::as_str), Some("data_table"));
    assert_eq!(loot.tags.get("row_type").map(String::as_str), Some("LootRow"));

    let content = root.join("content");
    pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pack");
    let shipped = unique_temp_dir("data-cook-typed-game");
    let server = AssetServer::open(AssetServerSettings {
        project_root: shipped.clone(),
        platform: current_platform(),
        content_dir: Some(content),
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    world.init_resource::<DataTables>();
    let mut registrations = DataTableRegistrations::default();
    registrations.add::<LootRow>("tables/loot.table.ron");
    registrations.add::<LootRow>("tables/notes.table.csv");
    world.insert_resource(registrations);
    load_registered_data_tables(&mut world);

    let tables = world.resource::<DataTables>();
    let slot = tables.slot("tables/loot.table.ron").expect("slot");
    assert!(slot.cooked, "there is no text to read");
    assert!(slot.last_issues.is_empty(), "{:?}", slot.last_issues);
    let typed = tables.get::<LootRow>("tables/loot.table.ron").expect("typed");
    let source = DataTable::<LootRow>::from_ron_str(LOOT).expect("source");
    assert_eq!(
        typed.keys().collect::<Vec<_>>(),
        source.keys().collect::<Vec<_>>()
    );
    for (key, row) in source.iter() {
        assert_eq!(typed.get(key), Some(row), "{key}");
    }
    let notes = tables
        .get::<LootRow>("tables/notes.table.csv")
        .expect("csv typed");
    assert!(tables.slot("tables/notes.table.csv").expect("slot").cooked);
    assert_eq!(
        notes.get("note").expect("note").label,
        "42",
        "a String cell stays text"
    );
    assert_eq!(notes.get("note").expect("note").tier, None);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&shipped);
}

/// A dev tree runs what is on disk: the text wins over the cook. A table with structural errors is not
/// cooked, with a warning that names it and the error; it is not a cook error, because the table ships raw
/// and its load in the game reports the same issue.
#[test]
fn typed_data_table_text_wins_and_structural_errors_are_left_uncooked() {
    use regia_asset::data_table::{DataTableRegistrations, DataTables, load_registered_data_tables};
    let root = project(
        "data-cook-typed-errors",
        &[
            ("tables/loot.table.ron", LOOT),
            ("tables/twice.table.ron", &LOOT.replace("\"gem\":", "\"coin\":")),
        ],
    );
    let report = cook(&root);
    assert!(real_errors(&report).is_empty(), "{:?}", report.errors);
    let warning = report
        .warnings
        .iter()
        .find(|w| w.contains("twice.table.ron"))
        .unwrap_or_else(|| panic!("no warning names the table: {:?}", report.warnings));
    assert!(warning.contains("table_duplicate_key"), "{warning}");
    let cache = cooked_cache_dir(&root, &current_platform());
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .expect("a cook wrote one");
    assert!(
        manifest
            .entries
            .iter()
            .any(|e| e.path == "assets/tables/loot.table.ron")
    );
    assert!(
        !manifest
            .entries
            .iter()
            .any(|e| e.path == "assets/tables/twice.table.ron"),
        "a table that did not cook is not in the manifest"
    );

    write(
        &root,
        "tables/loot.table.ron",
        &LOOT.replace("weight: 50", "weight: 51"),
    );
    let server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: current_platform(),
        content_dir: None,
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    world.init_resource::<DataTables>();
    let mut registrations = DataTableRegistrations::default();
    registrations.add::<LootRow>("tables/loot.table.ron");
    world.insert_resource(registrations);
    load_registered_data_tables(&mut world);
    let tables = world.resource::<DataTables>();
    assert!(!tables.slot("tables/loot.table.ron").expect("slot").cooked);
    let typed = tables.get::<LootRow>("tables/loot.table.ron").expect("typed");
    assert_eq!(typed.get("coin").expect("coin").weight, 51);
    let _ = std::fs::remove_dir_all(&root);
}

const UIANIM: &str = r#"(format_version: 1, animations: {
    "hud::pop": (tracks: [
        (property: "opacity", keys: [(time: 0.0, value: 0.0), (time: 0.2, value: 1.0, ease: "quad_out")]),
        (property: "scale", keys: [(time: 0.0, value: 0.9), (time: 0.2, value: 1.0)]),
    ]),
})"#;

/// A UI animation library cooks to canonical text (ADR-3092), a pak carries it, and the loader reads it
/// back with the source gone; the sampled values are the ones the source's plan gives.
#[test]
fn a_ui_animation_library_cooks_and_loads_from_the_pak() {
    use regia_ui::anim_pack::{UIANIM_TYPE_PATH, load_uianim_file};
    use regia_ui::anim_timeline::UiAnimLibrary;

    let root = project("data-cook-uianim", &[("ui/hud.uianim.ron", UIANIM)]);
    let report = cook(&root);
    assert!(real_errors(&report).is_empty(), "{:?}", report.errors);
    let (_, payload, type_path) = cooked(&root, "assets/ui/hud.uianim.ron");
    assert_eq!(type_path, UIANIM_TYPE_PATH);
    let text = String::from_utf8(payload).expect("text");
    let source = UiAnimLibrary::from_ron(UIANIM).expect("source");
    assert_eq!(
        UiAnimLibrary::from_ron(&text).expect("cooked text parses"),
        source
    );

    let cache = cooked_cache_dir(&root, &current_platform());
    let content = root.join("content");
    pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pack");
    let server = AssetServer::open(AssetServerSettings {
        project_root: unique_temp_dir("data-cook-uianim-game"),
        platform: current_platform(),
        content_dir: Some(content),
    })
    .expect("open");
    let mut world = World::new();
    world.insert_resource(server);
    let names = load_uianim_file(&mut world, "ui/hud.uianim.ron").expect("reads the pak");
    assert_eq!(names, vec!["hud::pop".to_owned()]);
    assert_eq!(world.resource::<UiAnimLibrary>().animations, source.animations);
}

/// A malformed library fails the cook with the library's own error and is left out of the pak.
#[test]
fn a_malformed_ui_animation_library_fails_the_cook() {
    let root = project(
        "data-cook-uianim-bad",
        &[(
            "ui/bad.uianim.ron",
            r#"(format_version: 1, animations: {"x": (tracks: [(property: "nope", keys: [(time: 0.0, value: 0.0)])])})"#,
        )],
    );
    let report = cook(&root);
    assert!(
        report
            .errors
            .iter()
            .any(|(path, e)| path.contains("bad.uianim.ron") && e.contains("unknown property `nope`")),
        "{:?}",
        report.errors
    );
}

/// The importer a sample data table's `.meta` must name, by the file's suffix (the cook's own map).
fn sample_table_importer(name: &str) -> &'static str {
    let name = name.to_ascii_lowercase();
    [
        (".curves.ron", "curve_table"),
        (".curves.csv", "curve_table"),
        (".chooser.ron", "chooser"),
        (".smartobject.ron", "smartobject"),
        (".camera.ron", "camera"),
        (".uianim.ron", "uianim"),
        (".table.ron", "data_table"),
        (".table.csv", "data_table"),
        // The abilities and net-profile importers (post05w3-montage-net-abilities-cook,
        // post05-core-assets-vfs) made these data tables too; `importer_of` in data_cook.rs.
        (".ability.ron", "abilities"),
        (".effect.ron", "abilities"),
        (".attrs.ron", "abilities"),
        (".netprofile.ron", "netprofile"),
        (".data.ron", "data_asset"),
        (".detutility.ron", "det_utility"),
    ]
    .iter()
    .find(|(suffix, _)| name.ends_with(suffix))
    .map_or("", |(_, importer)| importer)
}

/// Every data table a sample ships has its `.meta` in the tree. A missing sidecar is created by the
/// cook with a random id, in the source tree: each fresh `regia package` of the sample then minted a
/// new asset id and left an untracked file in the checkout (the arena's chooser, ADR-7344).
#[test]
fn every_sample_data_table_has_its_meta_in_the_tree() {
    let samples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples");
    let mut tables = 0;
    let mut missing = Vec::new();
    for entry in walkdir::WalkDir::new(&samples)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("target" | "build" | ".regia")))
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !entry.file_type().is_file() || !regia_asset_pipeline::data_cook::is_data_table(path) {
            continue;
        }
        tables += 1;
        let meta_path = regia_asset::meta_path_for(path);
        let Ok(bytes) = std::fs::read(&meta_path) else {
            missing.push(path.display().to_string());
            continue;
        };
        let meta = regia_asset::MetaSidecar::from_ron(&bytes).expect("a sample table's .meta parses");
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        assert_eq!(
            meta.importer,
            sample_table_importer(name),
            "{}",
            meta_path.display()
        );
    }
    assert!(
        tables > 0,
        "no sample data table found under {}",
        samples.display()
    );
    assert!(
        missing.is_empty(),
        "sample data tables without a .meta: {missing:?}"
    );
}

/// A deterministic utility catalogue (`.detutility.ron`, gap AI-7) cooks to its canonical text, which loads to the
/// same catalogue; a catalogue that fails validation fails the cook with its error code.
#[test]
fn a_det_utility_catalogue_cooks_to_its_canonical_text() {
    use regia_ai::det::{DET_UTILITY_TYPE_PATH, UtilityCatalogue};
    const CATALOGUE: &str = r#"(format_version: 1, inputs: [(name: "hull"), (name: "ammo")], options: [
        (name: "push", considerations: [(input: "ammo", curve: (kind: Linear, m: 1.0))]),
        (name: "retreat", considerations: [(input: "hull", curve: (kind: InverseLogistic, m: 8.0, k: 0.3))]),
    ])"#;
    let root = project(
        "data-cook-det-utility",
        &[
            ("ai/skirmish.detutility.ron", CATALOGUE),
            (
                "ai/bad.detutility.ron",
                r#"(format_version: 1, inputs: [], options: [(name: "x", considerations: [(input: "nope", curve: (kind: Quadratic))])])"#,
            ),
        ],
    );
    let report = cook(&root);
    assert!(
        report
            .errors
            .iter()
            .any(|(path, e)| path.contains("bad.detutility.ron") && e.contains("det_utility_unknown_input")),
        "{:?}",
        report.errors
    );
    let (_, payload, type_path) = cooked(&root, "assets/ai/skirmish.detutility.ron");
    assert_eq!(type_path, DET_UTILITY_TYPE_PATH);
    let text = String::from_utf8(payload).expect("text");
    assert_eq!(
        UtilityCatalogue::from_ron_str(&text).expect("load"),
        UtilityCatalogue::from_ron_str(CATALOGUE).expect("source")
    );
}
