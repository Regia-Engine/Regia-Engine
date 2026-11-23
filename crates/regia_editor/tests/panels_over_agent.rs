// SPDX-License-Identifier: MPL-2.0
//! The Layers, Tests and asset panels over the agent protocol (rule 8): every call goes through the
//! real dispatcher (registration, parameter schema, permissions), fed from an in-process inbox so
//! each one is answered within one frame. The unit tests in the panel modules call the handlers
//! directly; this file proves the methods reach them from a client and that errors keep their codes.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_editor::command::UndoStack;
use regia_editor::level_io::{self, OpenLevel};
use regia_editor::selection::{SelectMode, Selection};
use regia_scene::layers::{LayerDefs, LayerDefsLibrary, LayerParked};
use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level};
use serde_json::{Value, json};

const DEFS_ID: Guid = Guid::from_u128(0xd0);
const LEVEL: LevelId = LevelId(Guid::from_u128(0x1e));
const MEMBER: Guid = Guid::from_u128(0x20);

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Client {
    /// The editor's edit world with the agent plugin and a level with one member of layer `night`.
    fn new(project: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.init_resource::<UndoStack>()
            .init_resource::<Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>();
        let defs = LayerDefs::from_ron_str(
            r#"LayerDefs(format_version: 1, layers: [
                (name: "night", kind: Runtime, debug_color: (0.0, 0.0, 0.0), parent: None),
            ])"#,
        )
        .expect("defs");
        app.world_mut()
            .resource_mut::<LayerDefsLibrary>()
            .insert(DEFS_ID, defs);
        let text = format!(
            r#"SceneFile(format_version: {v}, entities: [
                (guid: "{MEMBER}", components: {{
                    "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
                    "regia_scene::layers::LayerMembership": (layers: ["night"]),
                }}),
            ], resources: {{
                "regia_scene::layers::LevelLayers": (defs: ["asset:{DEFS_ID}"], entries: []),
            }})"#,
            v = regia_scene::format::SCENE_FORMAT_VERSION,
        );
        let level = Level::from_ron_str(LEVEL, &text).expect("level");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), LEVEL, LoadMode::Replace).expect("open");
        app.world_mut().insert_resource(OpenLevel {
            id: LEVEL,
            path: None,
        });
        level_io::reset_edit_state(app.world_mut());
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded::<Inbound>();
        app.world_mut().insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        app.world_mut().insert_resource(permissions);
        Self {
            app,
            inbox,
            next_id: 0,
        }
    }

    fn try_call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(self.next_id)),
            method: method.into(),
            params,
        };
        self.inbox.send(Inbound { request, reply }).expect("inbox open");
        self.app.update();
        let response = answer
            .try_recv()
            .expect("the dispatcher answers within the frame");
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            (None, None) => panic!("{method}: empty response"),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        match self.try_call(method, params.clone()) {
            Ok(value) => value,
            Err(err) => panic!("{method} {params}: {err:?}"),
        }
    }

    fn parked(&mut self) -> usize {
        let world = self.app.world_mut();
        world.query::<&LayerParked>().iter(world).count()
    }
}

/// The error's data code.
fn code(err: &RpcError) -> &str {
    err.data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[test]
fn the_layers_panel_parks_a_member_from_a_client() {
    let project = regia_test_utils::unique_temp_dir("panels_over_agent_layers");
    let mut client = Client::new(&project);
    let state = client.call("editor.layers_panel_state", json!({}));
    assert_eq!(state["layers"][0]["name"], "night");
    assert_eq!(state["layers"][0]["members"], 1);
    assert_eq!(client.parked(), 0);

    client.call(
        "editor.layers_panel_set",
        json!({ "layer": "night", "visible": false }),
    );
    client.app.update();
    assert_eq!(client.parked(), 1, "hiding parks the member");

    client.call(
        "editor.layers_panel_set",
        json!({ "layer": "night", "visible": true }),
    );
    client.app.update();
    assert_eq!(client.parked(), 0, "showing brings it back");

    let err = client
        .try_call(
            "editor.layers_panel_set",
            json!({ "layer": "ghost", "visible": true }),
        )
        .expect_err("unknown layer");
    assert_eq!(code(&err), "layer_unknown", "{err:?}");
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn the_placed_edit_method_checks_its_action_before_acting() {
    let project = regia_test_utils::unique_temp_dir("panels_over_agent_placed");
    let mut client = Client::new(&project);
    let err = client
        .try_call("editor.placed_edit", json!({ "action": "melt" }))
        .expect_err("bad action");
    assert!(err.message.contains("begin, commit or discard"), "{err:?}");
    let err = client
        .try_call("editor.placed_edit", json!({ "action": "begin" }))
        .expect_err("no entity");
    assert!(err.message.contains("`entity`"), "{err:?}");
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn the_asset_panels_answer_a_client_before_any_run() {
    let project = regia_test_utils::unique_temp_dir("panels_over_agent_assets");
    let mut client = Client::new(&project);
    let state = client.call("editor.asset_panels_state", json!({ "limit": 10 }));
    assert_eq!(state["source"], "none");
    assert_eq!(state["problems"]["ran"], false);
    assert_eq!(state["graph"]["can_back"], false);
    let err = client
        .try_call("editor.problems_panel", json!({ "run": true }))
        .expect_err("no project set to validate");
    assert_eq!(code(&err), "registry_not_loaded", "{err:?}");
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn the_tests_panel_lists_nothing_and_refuses_an_unknown_test() {
    let project = regia_test_utils::unique_temp_dir("panels_over_agent_tests");
    let mut client = Client::new(&project);
    let state = client.call("editor.tests_panel_state", json!({}));
    assert_eq!(state["tests"], json!([]));
    let err = client
        .try_call("editor.tests_panel_open", json!({ "test": "ghost" }))
        .expect_err("unknown test");
    assert_eq!(code(&err), "test_not_found", "{err:?}");
    let err = client
        .try_call("editor.tests_panel_open", json!({}))
        .expect_err("`test` is a required parameter");
    assert!(err.message.contains("test"), "{err:?}");
    let _ = std::fs::remove_dir_all(&project);
}

/// ADR-3285: a read-only client cannot open a test row, and the selection stays.
#[test]
fn a_read_only_client_cannot_open_a_test_row() {
    let project = regia_test_utils::unique_temp_dir("panels_over_agent_tests_read_only");
    let mut client = Client::new(&project);
    let entity = client.app.world_mut().spawn_empty().id();
    client
        .app
        .world_mut()
        .resource_mut::<Selection>()
        .select(entity, SelectMode::Replace);
    client
        .app
        .world_mut()
        .insert_resource(regia_agent::AgentPermissions {
            config: regia_agent::AgentConfig::fail_closed(),
            source: None,
        });
    let err = client
        .try_call("editor.tests_panel_open", json!({ "test": "ghost" }))
        .expect_err("read-only");
    assert_eq!(code(&err), "F0012", "{err:?}");
    let selection = client.app.world().resource::<Selection>();
    assert_eq!(selection.entities(), &[entity]);
    assert_eq!(selection.primary(), Some(entity));
    let _ = std::fs::remove_dir_all(&project);
}

/// w11-biome-procgen-tooling item 1 through the real dispatcher: `editor.create_asset { kind:
/// "biome" }` (Content ▸ New ▸ Biome) writes a biome, the Biome editor opens it, edits it by field
/// path, refuses a value with its code, undoes, saves, and the runtime loader reads the file.
#[test]
fn the_biome_editor_creates_edits_and_saves_from_a_client() {
    let project = regia_test_utils::unique_temp_dir("panels_over_agent_biome");
    let mut client = Client::new(&project);
    client
        .app
        .world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.clone()),
        });
    let made = client.call(
        "editor.create_asset",
        json!({ "kind": "biome", "path": "assets/biomes/fen.biome.ron" }),
    );
    assert_eq!(made["path"], json!("assets/biomes/fen.biome.ron"));
    let state = client.call(
        "editor.biome_panel_open",
        json!({ "path": "assets/biomes/fen.biome.ron" }),
    );
    assert_eq!(state["def"]["name"], json!("fen"));
    assert_eq!(state["dirty"], json!(false));
    client.call(
        "editor.biome_panel_edit",
        json!({ "op": "add", "path": "layers", "value": { "name": "reeds" } }),
    );
    client.call(
        "editor.biome_panel_edit",
        json!({ "op": "add", "path": "layers/reeds/entries", "value": { "name": "reed", "spawn": { "Mesh": "meshes/reed.glb" } } }),
    );
    client.call(
        "editor.biome_panel_edit",
        json!({ "op": "set", "path": "layers/reeds/entries/reed/filters", "value": [{ "WaterDistance": { "max_m": 4.0, "feather": 1.0 } }] }),
    );
    let refused = client
        .try_call(
            "editor.biome_panel_edit",
            json!({ "op": "set", "path": "layers/reeds/max_child_depth", "value": -1 }),
        )
        .expect_err("a u32 refuses -1");
    let reason = refused
        .data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(reason, "biome_panel_bad_value", "{refused:?}");
    let state = client.call("editor.biome_panel_get", json!({}));
    assert_eq!(state["undo_depth"], json!(3), "{state}");
    assert_eq!(state["dirty"], json!(true));
    let saved = client.call("editor.biome_panel_save", json!({}));
    assert_eq!(saved["written"], json!("assets/biomes/fen.biome.ron"));
    let text = std::fs::read_to_string(project.join("assets/biomes/fen.biome.ron")).expect("file");
    let def = regia_procgen::biome::def::BiomeDef::from_ron(&text).expect("the runtime loader reads it");
    assert_eq!(def.layers[0].entries[0].filters.len(), 1);
    assert_eq!(text, def.to_ron());
    let _ = std::fs::remove_dir_all(&project);
}

/// Review B3 through the real dispatcher: `editor.biome_panel_open` (and `new: true`) over unsaved
/// edits is refused with `biome_panel_unsaved` and the edits survive; `discard: true` drops them.
#[test]
fn the_biome_editor_refuses_to_drop_unsaved_edits_from_a_client() {
    let project = regia_test_utils::unique_temp_dir("panels_over_agent_biome_unsaved");
    let mut client = Client::new(&project);
    client
        .app
        .world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.clone()),
        });
    for path in ["assets/biomes/a.biome.ron", "assets/biomes/b.biome.ron"] {
        client.call("editor.create_asset", json!({ "kind": "biome", "path": path }));
    }
    client.call(
        "editor.biome_panel_open",
        json!({ "path": "assets/biomes/a.biome.ron" }),
    );
    client.call(
        "editor.biome_panel_edit",
        json!({ "op": "set", "path": "priority", "value": 42 }),
    );
    for params in [
        json!({ "path": "assets/biomes/b.biome.ron" }),
        json!({ "path": "assets/biomes/c.biome.ron", "new": true }),
    ] {
        let refused = client
            .try_call("editor.biome_panel_open", params.clone())
            .expect_err("unsaved edits");
        let reason = refused
            .data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_owned();
        assert_eq!(reason, "biome_panel_unsaved", "{params}: {refused:?}");
        let state = client.call("editor.biome_panel_get", json!({}));
        assert_eq!(state["path"], json!("assets/biomes/a.biome.ron"), "{params}");
        assert_eq!(state["def"]["priority"], json!(42), "{params}: the edit survives");
        assert_eq!(state["dirty"], json!(true));
    }
    let state = client.call(
        "editor.biome_panel_open",
        json!({ "path": "assets/biomes/b.biome.ron", "discard": true }),
    );
    assert_eq!(state["path"], json!("assets/biomes/b.biome.ron"));
    assert_eq!(state["dirty"], json!(false));
    let _ = std::fs::remove_dir_all(&project);
}
