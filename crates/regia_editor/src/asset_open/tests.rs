// SPDX-License-Identifier: MPL-2.0
//! Tests of the asset-open dispatcher (ADR-7725): one file of every kind opens in its panel by a
//! Content Browser double click and by `editor.open_asset`, and the panel's state reports the path.

use std::path::{Path, PathBuf};

use regia_app::prelude::{Reflect, ReflectDefault};

use super::*;
use crate::content::{ContentBrowserState, NewAssetKind, create_asset};

/// A game-style data asset type (registered as one, so `.data.ron` files of it open).
#[derive(Reflect, Default, Clone, PartialEq, Debug)]
#[reflect(Default)]
struct OpenLoot {
    rolls: u32,
}
impl regia_asset::data_asset::Asset for OpenLoot {}

/// The path of one fixture of every kind, by kind name.
const FIXTURES: &[(&str, &str)] = &[
    ("level", "assets/maps/arena.level.ron"),
    ("prefab", "assets/props/crate.prefab.ron"),
    ("material", "assets/paint/blue.material.ron"),
    ("material_graph", "assets/mat/rock.matgraph.ron"),
    ("material_function", "assets/mat/pass.matfunc.ron"),
    ("material_instance", "assets/mat/rock_wet.matinst.ron"),
    ("anim_graph", "assets/anim/hero.animgraph.ron"),
    ("procgen_graph", "assets/pcg/forest.pgraph.ron"),
    ("ability", "assets/abilities/dash.ability.ron"),
    ("behavior_tree", "assets/ai/guard.bt.ron"),
    ("state_tree", "assets/ai/guard.statetree.ron"),
    ("montage", "assets/anim/wave.montage.ron"),
    ("sequence", "assets/cine/intro.seq.ron"),
    ("shot_list", "assets/cine/intro.shots.ron"),
    ("ui_layout", "assets/ui/hud.ui.ron"),
    ("ui_animation", "assets/ui/hud.uianim.ron"),
    ("sound_cue", "assets/sfx/step.cue.ron"),
    ("data_asset", "assets/data/loot.data.ron"),
    ("data_table", "assets/data/weapons.table.ron"),
    ("curve_table", "assets/data/xp.curves.ron"),
    ("chooser", "assets/data/pick.chooser.ron"),
    ("conversation", "assets/conversations/intro.conv.ron"),
    ("vfx_system", "assets/vfx/sparks.vfx.ron"),
    ("video", "assets/movies/logo.webm"),
    ("cloth", "assets/physics/flag.cloth.ron"),
    ("soft_body", "assets/physics/blob.soft.ron"),
    ("motion_database", "assets/anim/hero.motiondb.ron"),
    ("biome", "assets/biomes/meadow.biome.ron"),
    ("logic_graph", "assets/logic/gate.logic.ron"),
    ("viewer", "assets/textures/rock.png"),
];

/// Writes the fixture for `kind` at `rel` under `root`.
fn write_fixture(root: &Path, kind: &str, rel: &str, registry: &regia_ecs::reflect::TypeRegistry) {
    let full = root.join(rel);
    std::fs::create_dir_all(full.parent().expect("parent")).expect("dir");
    let new = |kind: NewAssetKind| {
        create_asset(root, kind, rel).unwrap_or_else(|e| panic!("{rel}: {e}"));
    };
    let text = |body: &str| std::fs::write(&full, body).expect("write");
    match kind {
        "level" => new(NewAssetKind::Level),
        "prefab" => new(NewAssetKind::Prefab),
        "material" => new(NewAssetKind::Material),
        "material_graph" => new(NewAssetKind::MaterialGraph),
        "material_function" => new(NewAssetKind::MaterialFunction),
        "material_instance" => {
            let parent = "assets/mat/rock_parent.matgraph.ron";
            create_asset(root, NewAssetKind::MaterialGraph, parent).expect("parent");
            crate::content::create_material_instance(root, rel, parent).expect("instance");
        }
        "anim_graph" => new(NewAssetKind::AnimGraph),
        "procgen_graph" => new(NewAssetKind::ProcgenGraph),
        "ability" => new(NewAssetKind::Ability),
        "behavior_tree" => text("(root: 0, nodes: [])"),
        "state_tree" => text(r#"(format_version: 2, root: "Fight", states: [(name: "Fight")])"#),
        "montage" => {
            text(r#"(format_version: 2, name: "wave", tracks: [(slot: "full", segments: [(clip: "w")])])"#)
        }
        "sequence" => new(NewAssetKind::Sequence),
        "shot_list" => new(NewAssetKind::ShotList),
        "ui_layout" => new(NewAssetKind::UiLayout),
        "ui_animation" => text("(format_version: 1, animations: {})"),
        "sound_cue" => new(NewAssetKind::SoundCue),
        "data_asset" => {
            crate::data_asset::create_data_asset(root, rel, "OpenLoot", registry).expect("data asset");
        }
        "data_table" => new(NewAssetKind::DataTable),
        "curve_table" => new(NewAssetKind::CurveTable),
        "chooser" => new(NewAssetKind::Chooser),
        "conversation" => new(NewAssetKind::Conversation),
        "vfx_system" => new(NewAssetKind::VfxSystem),
        "video" => {
            std::fs::write(&full, regia_media::mux::PatternClip::new(16, 16, 8, 25).to_webm()).expect("clip")
        }
        "cloth" => new(NewAssetKind::Cloth),
        "soft_body" => new(NewAssetKind::SoftBody),
        "motion_database" => new(NewAssetKind::MotionDatabase),
        "biome" => new(NewAssetKind::Biome),
        "logic_graph" => new(NewAssetKind::LogicGraph),
        "viewer" => std::fs::write(&full, b"not decoded until the viewer draws").expect("png"),
        other => panic!("no fixture for {other}"),
    }
}

fn registry() -> regia_ecs::reflect::TypeRegistry {
    let mut registry = regia_ecs::reflect::TypeRegistry::default();
    registry.register::<OpenLoot>();
    registry.register_type_data::<OpenLoot, regia_asset::data_asset::ReflectAsset>();
    registry.register::<u32>();
    registry
}

/// A project with every fixture.
fn project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    std::fs::write(root.join("Regia.toml"), "[project]\nname = \"g\"\n").expect("manifest");
    let registry = registry();
    for (kind, rel) in FIXTURES {
        write_fixture(&root, kind, rel, &registry);
    }
    root
}

/// An edit world on `root`: the scene registry, an open level, every root resource the panels read.
fn world_at(root: &Path) -> World {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let mut world = std::mem::take(app.world_mut());
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root.to_path_buf(),
        platform: regia_asset_pipeline::host_platform().to_string(),
        content_dir: None,
    })
    .expect("asset server");
    world.insert_resource(server);
    world.insert_resource(regia_procgen::world::ProcGenSettings {
        editor: true,
        source: regia_procgen::world::ProcGraphSource::Dir(root.to_path_buf()),
        ..regia_procgen::world::ProcGenSettings::default()
    });
    world.insert_resource(crate::command::UndoStack::new());
    let open = crate::level_io::new_level(&mut world);
    world.insert_resource(open);
    crate::agent_ext::install(&mut world);
    {
        let types = world.resource::<regia_ecs::reflect::AppTypeRegistry>().clone();
        let mut types = types.write();
        types.register::<OpenLoot>();
        types.register_type_data::<OpenLoot, regia_asset::data_asset::ReflectAsset>();
        types.register::<u32>();
    }
    world
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    let handler = handler_of(world, method).unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params)
}

fn code(err: &RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The open path the kind's panel reports: its state method's field, or the shell's request for the
/// three shell kinds and the Data Asset window's document.
fn reported(world: &mut World, content: &ContentBrowserState, kind: &OpenKind) -> String {
    match kind.name {
        "material" => content
            .open_request
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        "viewer" => content.view_request.clone().unwrap_or_default(),
        "data_asset" => world
            .get_resource::<crate::data_asset::DataAssetEditor>()
            .and_then(|editor| editor.doc.as_ref().map(|doc| doc.path.clone()))
            .unwrap_or_default(),
        "level" => world
            .resource::<crate::level_io::OpenLevel>()
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        _ => {
            let (method, field) = kind
                .state
                .unwrap_or_else(|| panic!("{} has no state method", kind.name));
            let answer = call(world, method, json!({})).unwrap_or_else(|e| panic!("{method}: {}", e.message));
            let mut value = &answer;
            for part in field.split('.') {
                value = &value[part];
            }
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        }
    }
}

/// The reported path names the file (panels report it absolute, project-relative or under assets/).
fn assert_names(kind: &str, reported: &str, rel: &str) {
    let tail = rel.trim_start_matches("assets/");
    assert!(
        reported.replace('\\', "/").ends_with(tail),
        "{kind}: the panel reports {reported:?}, not {rel}"
    );
}

#[test]
fn every_open_kind_has_a_fixture_and_the_fixture_is_that_kind() {
    let names: Vec<&str> = FIXTURES.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(names, kind_names(), "one fixture per row, in table order");
    for (kind, rel) in FIXTURES {
        assert_eq!(kind_of(rel).map(|k| k.name), Some(*kind), "{rel}");
    }
}

#[test]
fn a_double_click_opens_every_kind_in_its_panel() {
    let root = project("asset-open-browser");
    for (name, rel) in FIXTURES {
        let mut world = world_at(&root);
        let kind = kind_of(rel).expect("kind");
        let mut state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..ContentBrowserState::default()
        };
        let mut level = world.resource::<crate::level_io::OpenLevel>().clone();
        let toast = open_from_browser(&mut world, &mut state, &mut level, rel);
        assert_eq!(toast, None, "{name}: {rel}");
        world.insert_resource(level);
        let path = reported(&mut world, &state, kind);
        assert_names(name, &path, rel);
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A browser state on `root` and the open level to hand `open_from_browser`.
fn browser(world: &World, root: &Path) -> (ContentBrowserState, crate::level_io::OpenLevel) {
    (
        ContentBrowserState {
            project_root: Some(root.to_path_buf()),
            ..ContentBrowserState::default()
        },
        world.resource::<crate::level_io::OpenLevel>().clone(),
    )
}

/// sprint-w13-ui-live-bugs (1): a double click on a level while the open one has unsaved edits
/// asks first (the Save all dialog, which opens the level after Save selected or Don't save); it
/// used to replace the level and drop the edits.
#[test]
fn a_browser_double_click_on_a_level_with_unsaved_edits_asks_first() {
    use crate::save_all_dialog::{LevelSwitch, SaveAllDialog, SavePurpose};
    let root = project("asset-open-browser-unsaved");
    let rel = "assets/maps/arena.level.ron";
    let mut world = world_at(&root);
    world.resource_mut::<crate::command::UndoStack>().mark_dirty();
    let (mut state, mut level) = browser(&world, &root);
    let before = level.path.clone();
    let toast = open_from_browser(&mut world, &mut state, &mut level, rel);
    assert_eq!(toast.as_deref(), Some("Unsaved changes: choose what to save"));
    assert_eq!(level.path, before, "the open level is untouched");
    assert!(
        world.resource::<crate::command::UndoStack>().is_dirty(),
        "the edits stay"
    );
    let dialog = world.resource::<SaveAllDialog>();
    assert_eq!(
        dialog.purpose,
        Some(SavePurpose::LevelSwitch(LevelSwitch::Open(root.join(rel)))),
        "the dialog opens the clicked level after the choice"
    );
    // With nothing unsaved the same click opens the level at once.
    let mut clean = world_at(&root);
    let (mut state, mut level) = browser(&clean, &root);
    assert_eq!(open_from_browser(&mut clean, &mut state, &mut level, rel), None);
    assert_eq!(level.path.as_deref(), Some(root.join(rel).as_path()));
    assert!(
        clean
            .get_resource::<SaveAllDialog>()
            .is_none_or(|d| d.purpose.is_none())
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// sprint-w13-ui-live-bugs (1): a double click on a level while a game runs is refused with the
/// agent route's code, and the level stays open.
#[test]
fn a_browser_double_click_during_play_is_refused() {
    let root = project("asset-open-browser-play");
    let rel = "assets/maps/arena.level.ron";
    let mut world = world_at(&root);
    world.insert_resource(crate::pie::PieStatus {
        mode: Some(crate::pie::PlayMode::Playing),
        ..Default::default()
    });
    let (mut state, mut level) = browser(&world, &root);
    let before = level.path.clone();
    let toast = open_from_browser(&mut world, &mut state, &mut level, rel).expect("refused");
    assert!(toast.contains("asset_open_play_running"), "{toast}");
    assert_eq!(level.path, before, "the open level is untouched");
    // The agent route answers the same code.
    let agent = open_level_guarded(&mut world, rel).expect_err("refused");
    assert_eq!(code(&agent), "asset_open_play_running");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn editor_open_asset_opens_every_kind_like_the_double_click() {
    let root = project("asset-open-agent");
    for (name, rel) in FIXTURES {
        // The double click.
        let mut clicked = world_at(&root);
        let kind = kind_of(rel).expect("kind");
        let mut state = ContentBrowserState {
            project_root: Some(root.clone()),
            ..ContentBrowserState::default()
        };
        let mut level = clicked.resource::<crate::level_io::OpenLevel>().clone();
        assert_eq!(open_from_browser(&mut clicked, &mut state, &mut level, rel), None);
        clicked.insert_resource(level);
        let by_click = reported(&mut clicked, &state, kind);

        // The agent method, then the shell's next frame for the queued kinds.
        let mut agent = world_at(&root);
        let answer = call(&mut agent, "editor.open_asset", json!({ "path": rel }))
            .unwrap_or_else(|e| panic!("{name}: {} ({})", e.message, code(&e)));
        assert_eq!(answer["kind"], json!(name));
        assert_eq!(answer["path"], json!(rel));
        let queued = matches!(
            kind.route,
            OpenRoute::Material | OpenRoute::Viewer | OpenRoute::Level
        );
        assert_eq!(answer["queued"], json!(queued), "{name}");
        let mut content = ContentBrowserState {
            project_root: Some(root.clone()),
            ..ContentBrowserState::default()
        };
        assert_eq!(
            crate::log_links::route_asset_requests(&mut agent, &mut content),
            None,
            "{name}"
        );
        let by_agent = reported(&mut agent, &content, kind);
        assert_eq!(by_agent, by_click, "{name}: the same panel state either way");
        if let Some(panel) = kind.panel
            && !queued
            && kind.route != OpenRoute::Prefab
        {
            let requests = agent
                .get_resource::<crate::editor_layout::LayoutRequests>()
                .map(|r| format!("{r:?}"))
                .unwrap_or_default();
            assert!(requests.contains(&format!("{panel:?}")), "{name}: {requests}");
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unknown_kind_and_a_missing_file_answer_their_codes() {
    let root = project("asset-open-codes");
    std::fs::write(root.join("assets/notes.txt"), "x").expect("txt");
    std::fs::write(root.join("assets/mix.audiopatch.ron"), "()").expect("patch");
    let mut world = world_at(&root);
    for path in ["assets/notes.txt", "assets/mix.audiopatch.ron", "notes.txt"] {
        let err = call(&mut world, "editor.open_asset", json!({ "path": path })).expect_err(path);
        assert_eq!(code(&err), "asset_kind_unknown", "{path}");
    }
    let err = call(
        &mut world,
        "editor.open_asset",
        json!({ "path": "assets/ai/missing.bt.ron" }),
    )
    .expect_err("missing");
    assert_eq!(code(&err), "F0003");
    let err = call(
        &mut world,
        "editor.open_asset",
        json!({ "path": "../outside.bt.ron" }),
    )
    .expect_err("outside");
    assert_eq!(code(&err), "F0002");
    assert!(call(&mut world, "editor.open_asset", json!({})).is_err());
    // A path without `assets/` is read under it.
    let answer = call(
        &mut world,
        "editor.open_asset",
        json!({ "path": "ai/guard.bt.ron" }),
    )
    .expect("bare");
    assert_eq!(answer["path"], json!("assets/ai/guard.bt.ron"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn asset_kinds_lists_the_open_table_and_the_new_menu() {
    let mut world = World::new();
    let answer = asset_kinds_method(&mut world, &json!({})).expect("kinds");
    let open: Vec<&str> = answer["open"]
        .as_array()
        .expect("open")
        .iter()
        .filter_map(|k| k["kind"].as_str())
        .collect();
    assert_eq!(open, kind_names());
    let create: Vec<&str> = answer["create"]
        .as_array()
        .expect("create")
        .iter()
        .filter_map(|k| k["kind"].as_str())
        .collect();
    let menu: Vec<&str> = NewAssetKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(create, menu, "the New menu's kinds, in its order");
}

#[test]
fn log_links_open_the_new_kinds_through_the_dispatcher() {
    use crate::log_links::{AssetAction, asset_action, parse_links};
    for (name, rel) in FIXTURES {
        let expected = match *name {
            "level" => AssetAction::Level,
            "prefab" => AssetAction::Prefab,
            "viewer" => AssetAction::Viewer,
            _ => AssetAction::Editor,
        };
        assert_eq!(asset_action(rel), expected, "{rel}");
        let links = parse_links(&format!("failed to load {rel}"));
        assert_eq!(links.len(), 1, "{rel}: {links:?}");
        assert_eq!(links[0].path(), *rel);
    }
    assert_eq!(asset_action("assets/x.theme.ron"), AssetAction::CodeEditor);
}

/// A data table row type for the typed load of a new `.table.ron`.
#[derive(Reflect, Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
struct OpenWeaponRow {
    damage: f32,
}

/// Loads `text` (a new `kind` file at `path`) through its runtime loader.
fn runtime_load(kind: NewAssetKind, path: &str, text: &str) -> Result<(), String> {
    match kind {
        NewAssetKind::ProcgenGraph => regia_procgen::graph::ProcGraph::from_ron(text)
            .map(|g| assert_eq!(g.format_version, regia_procgen::graph::PGRAPH_FORMAT_VERSION))
            .map_err(|e| e.to_string()),
        NewAssetKind::VfxSystem => regia_vfx::format::VfxSystemDef::from_ron(text)
            .map(|v| assert_eq!(v.version, regia_vfx::format::VFX_FORMAT_VERSION))
            .map_err(|e| e.to_string()),
        NewAssetKind::Conversation => regia_framework::conversation::asset::parse_conversation(path, text)
            .map(|c| {
                assert_eq!(
                    c.format_version,
                    regia_framework::conversation::asset::CONVERSATION_FORMAT_VERSION
                );
            })
            .map_err(|e| e.to_string()),
        NewAssetKind::CurveTable => regia_framework::curve_table::parse_curve_table(path, text)
            .map(|_| ())
            .map_err(|e| format!("{e:?}")),
        NewAssetKind::Chooser => regia_framework::chooser::ChooserTable::from_ron_str(text)
            .map(|_| ())
            .map_err(|e| format!("{e:?}")),
        NewAssetKind::DataTable => regia_asset::data_table::DataTable::<OpenWeaponRow>::from_ron_str(text)
            .map(|_| ())
            .map_err(|e| format!("{e:?}")),
        other => Err(format!("{} is not a new w11 kind", other.as_str())),
    }
}

/// The format version each new kind is written at.
fn current_version(kind: NewAssetKind) -> u32 {
    match kind {
        NewAssetKind::ProcgenGraph => regia_procgen::graph::PGRAPH_FORMAT_VERSION,
        NewAssetKind::VfxSystem => regia_vfx::format::VFX_FORMAT_VERSION,
        NewAssetKind::Conversation => regia_framework::conversation::asset::CONVERSATION_FORMAT_VERSION,
        NewAssetKind::CurveTable => regia_framework::curve_table::CURVE_TABLE_FORMAT_VERSION,
        NewAssetKind::Chooser => regia_framework::chooser::CHOOSER_FORMAT_VERSION,
        _ => regia_asset::data_table::DATA_TABLE_FORMAT_VERSION,
    }
}

const NEW_KINDS: [NewAssetKind; 6] = [
    NewAssetKind::ProcgenGraph,
    NewAssetKind::VfxSystem,
    NewAssetKind::Conversation,
    NewAssetKind::CurveTable,
    NewAssetKind::Chooser,
    NewAssetKind::DataTable,
];

#[test]
fn the_new_menu_and_create_asset_write_the_same_bytes_that_load_at_the_current_version() {
    let root = regia_test_utils::unique_temp_dir("asset-open-new");
    std::fs::create_dir_all(root.join("assets/menu")).expect("dir");
    let mut world = world_at(&root);
    let mut state = ContentBrowserState {
        project_root: Some(root.clone()),
        folder: "assets/menu".to_owned(),
        ..ContentBrowserState::default()
    };
    let listed = call(&mut world, "editor.asset_kinds", json!({})).expect("kinds");
    for kind in NEW_KINDS {
        assert!(NewAssetKind::ALL.contains(&kind), "{kind:?} is in the New menu");
        assert_eq!(NewAssetKind::parse(kind.as_str()), Ok(kind));
        assert!(
            listed["create"]
                .as_array()
                .is_some_and(|k| k.iter().any(|row| row["kind"] == json!(kind.as_str()))),
            "editor.asset_kinds lists {kind:?}"
        );
        state.selected = None;
        crate::content::new_asset_from_menu(&mut world, &mut state, kind, None);
        let created = state
            .selected
            .clone()
            .unwrap_or_else(|| panic!("{kind:?}: {:?}", state.toast));
        let menu_bytes = std::fs::read(root.join(&created)).expect("menu file");
        let name = created.rsplit('/').next().expect("name");
        let agent_path = format!("assets/agent/{name}");
        let answer = call(
            &mut world,
            "editor.create_asset",
            json!({ "kind": kind.as_str(), "path": agent_path }),
        )
        .unwrap_or_else(|e| panic!("{kind:?}: {}", e.message));
        assert_eq!(answer["path"], json!(agent_path));
        let agent_bytes = std::fs::read(root.join(&agent_path)).expect("agent file");
        assert_eq!(
            agent_bytes, menu_bytes,
            "{kind:?}: the menu and the agent write the same bytes"
        );
        let text = String::from_utf8(menu_bytes).expect("utf-8");
        assert!(
            text.contains(&format!("version: {}", current_version(kind))),
            "{kind:?} at its current version:\n{text}"
        );
        runtime_load(kind, &created, &text).unwrap_or_else(|e| panic!("{kind:?}: {e}\n{text}"));
        // The created file opens in its panel through the dispatcher.
        let opened = open(&mut world, &created).unwrap_or_else(|e| panic!("{kind:?}: {}", e.message));
        assert!(!opened.queued());
    }
    // A data table with a row type names it.
    let typed = call(
        &mut world,
        "editor.create_asset",
        json!({ "kind": "data_table", "path": "assets/agent/typed.table.ron", "type": "OpenWeaponRow" }),
    )
    .expect("typed");
    let text = std::fs::read_to_string(root.join(typed["path"].as_str().expect("path"))).expect("read");
    assert!(text.contains("row_type: \"OpenWeaponRow\""), "{text}");
    let _ = std::fs::remove_dir_all(&root);
}

/// An inline asset set: `props` holds a clean and a broken asset, `levels` a broken one.
const FOLDER_SET: &str = r##"#![enable(implicit_some)]
(
    format_version: 2,
    assets: [
        (path: "assets/props/crate.mesh", kind: mesh, cooked_bytes: 100),
        (path: "assets/props/barrel.scene", kind: scene, references: ["assets/props/crate.mesh"], cooked_bytes: 10),
        (path: "assets/props/bad.scene", kind: scene, references: ["assets/props/gone.png"], cooked_bytes: 10),
        (path: "assets/levels/broken.scene", kind: scene, references: ["assets/textures/missing.png"], cooked_bytes: 10),
    ],
)"##;

#[test]
fn folder_actions_are_scoped_to_the_folder() {
    use regia_asset_pipeline::asset_graph::AssetSet;
    let mut world = World::new();
    let set = AssetSet::from_ron(FOLDER_SET).expect("set");
    assert_eq!(
        crate::asset_panels::folder_assets(&set, "assets/props"),
        [
            "assets/props/crate.mesh",
            "assets/props/barrel.scene",
            "assets/props/bad.scene"
        ]
    );
    // A Content Browser folder matches a set whose paths have no `assets/` prefix too.
    let bare = AssetSet::from_ron(&FOLDER_SET.replace("assets/", "")).expect("bare set");
    assert_eq!(crate::asset_panels::folder_assets(&bare, "assets/props").len(), 3);

    // Validate folder: only the folder's problems; the broken asset outside is not listed.
    let problems = call(
        &mut world,
        "editor.problems_panel",
        json!({ "set": FOLDER_SET, "run": true, "folder": "assets/props" }),
    )
    .expect("validate folder");
    let rows = problems["rows"].to_string();
    assert!(rows.contains("assets/props/bad.scene"), "{problems}");
    assert!(!rows.contains("assets/levels/broken.scene"), "{problems}");
    // The whole project lists both (the control; a fresh panel, since a run with no scope repeats
    // the last one).
    let all = call(
        &mut World::new(),
        "editor.problems_panel",
        json!({ "set": FOLDER_SET, "run": true }),
    )
    .expect("all");
    assert!(
        all["rows"].to_string().contains("assets/levels/broken.scene"),
        "{all}"
    );

    // Reference graph and Size map start from the folder's assets.
    let graph = call(
        &mut world,
        "editor.reference_graph_panel",
        json!({ "set": FOLDER_SET, "action": "open", "folder": "assets/props" }),
    )
    .expect("graph");
    let roots = graph["query"]["roots"].to_string();
    assert!(
        roots.contains("assets/props/crate.mesh") && !roots.contains("levels"),
        "{graph}"
    );
    let map = call(
        &mut world,
        "editor.size_map_panel",
        json!({ "set": FOLDER_SET, "folder": "assets/props" }),
    )
    .expect("size map");
    let roots = map["query"]["roots"].to_string();
    assert!(
        roots.contains("assets/props/bad.scene") && !roots.contains("levels"),
        "{map}"
    );
    // An empty folder answers its code.
    let err = call(
        &mut world,
        "editor.size_map_panel",
        json!({ "set": FOLDER_SET, "folder": "assets/empty" }),
    )
    .expect_err("empty");
    assert_eq!(code(&err), "asset_folder_empty");

    // Audit: only the folder's rows.
    let audit = call(
        &mut world,
        "editor.asset_audit",
        json!({ "set": FOLDER_SET, "folder": "assets/props" }),
    )
    .expect("audit");
    assert_eq!(audit["rows"], json!(3), "{audit}");
}
