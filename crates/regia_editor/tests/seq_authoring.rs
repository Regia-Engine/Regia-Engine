// SPDX-License-Identifier: MPL-2.0
//! Sequence authoring through the real agent dispatcher (ADR-9710, `book/agents/seq-curve-panel.md`):
//! create a sequence, bind a spawned cube, key it at 0 s and 2 s around a move, auto-key a second
//! move as one undo entry with it, preview and stop with the level save unchanged, and play the
//! saved file headless: the cube at 1 s is the sampler's value. Requests come from an in-process
//! inbox, each answered within one frame (no wall-clock waits).

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::command::UndoStack;
use regia_editor::level_io;
use regia_editor::selection::Selection;
use regia_framework::sequencer::{
    Binding, ChannelValue, Sequence, SequenceBindings, SequencePlayer, SequencerPlugin,
};
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Agent {
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
        app.insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(project.to_path_buf()),
        });
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open);
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
}

fn code(err: &RpcError) -> &str {
    err.data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

#[test]
fn a_cube_keyed_around_a_move_plays_back_the_sampler_value_through_the_dispatcher() {
    let project = regia_test_utils::unique_temp_dir("seq-authoring-e2e");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);

    let made = agent.call(
        "editor.seq_new",
        json!({ "path": "assets/cine/cube.seq.ron", "length": 3.0, "fps": 30.0 }),
    );
    assert_eq!(made["length"], json!(3.0));
    let cube = agent.call("editor.spawn", json!({ "name": "Cube" }))["entity"]
        .as_u64()
        .expect("cube");
    let bound = agent.call(
        "editor.seq_binding_edit",
        json!({ "op": "add", "binding": "cube", "entity": cube }),
    );
    assert!(bound["bindings"][0]["guid"].is_string(), "bound by guid: {bound}");

    // Key at 0 s, move, key at 2 s.
    let key = agent.call(
        "editor.seq_key",
        json!({ "binding": "cube", "channel": "position" }),
    );
    assert_eq!(key["keys"][0]["created"], json!(true));
    agent.call(
        "editor.set_transform",
        json!({ "entity": cube, "translation": [4.0, 2.0, 0.0] }),
    );
    agent.call(
        "editor.seq_key",
        json!({ "binding": "cube", "channel": "position", "time": 2.0 }),
    );

    // Auto-key: a move at the 2 s playhead is keyed into the move's own undo entry.
    agent.call("editor.seq_preview", json!({ "on": true, "time": 2.0 }));
    agent.call("editor.seq_preview", json!({ "on": false }));
    agent.call("editor.seq_key", json!({ "auto": true }));
    let depth = agent.call("editor.history", json!({}))["undo_depth"].clone();
    agent.call(
        "editor.set_transform",
        json!({ "entity": cube, "translation": [6.0, 2.0, 0.0] }),
    );
    // The next editor.seq_* call runs the auto key.
    let state = agent.call(
        "editor.seq_track_edit",
        json!({ "op": "add", "kind": "marker", "name": "probe", "time": 0.5 }),
    );
    let tracks: Vec<&str> = state["tracks"]
        .as_array()
        .expect("tracks")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(
        tracks,
        ["cube.position", "cube.rotation", "cube.scale"],
        "auto key wrote the transform"
    );
    let history = agent.call("editor.history", json!({}));
    assert_eq!(
        history["undo_depth"].as_u64(),
        depth.as_u64().map(|d| d + 2),
        "the move with its key, then the marker: {history}"
    );
    // Undo the marker, then the move and its key in one step.
    agent.call("editor.undo", json!({}));
    agent.call("editor.undo", json!({}));
    let after = agent.call(
        "editor.seq_track_edit",
        json!({ "op": "add", "kind": "marker", "name": "probe", "time": 0.5 }),
    );
    let names: Vec<&str> = after["tracks"]
        .as_array()
        .expect("tracks")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, ["cube.position"], "one undo took the auto key back");
    agent.call("editor.undo", json!({}));
    agent.call("editor.seq_key", json!({ "auto": false }));

    // Removing the used binding without cascade is refused.
    let refused = agent
        .try_call(
            "editor.seq_binding_edit",
            json!({ "op": "remove", "binding": "cube" }),
        )
        .expect_err("used");
    assert_eq!(code(&refused), "seq_binding_used", "{refused:?}");

    // Preview, save, stop: the level file is byte-identical.
    agent.call("editor.save_level", json!({ "path": "assets/room.level.ron" }));
    let level_file = project.join("assets/room.level.ron");
    let level_before = std::fs::read(&level_file).expect("level");
    agent.call("editor.seq_preview", json!({ "on": true, "time": 1.0 }));
    agent.call("editor.save_level", json!({ "path": "assets/room.level.ron" }));
    assert_eq!(
        std::fs::read(&level_file).expect("level"),
        level_before,
        "saved during preview"
    );
    // The file watcher then delivers the editor's own write (review rev-claude-51-k finding 1):
    // the edit world, compared with the preview lifted, matches the file, so the level is neither
    // reloaded (which would clear the undo history and kill the cube's entity) nor prompted for.
    let depth_saved = agent.call("editor.history", json!({}))["undo_depth"].clone();
    regia_editor::asset_watch::on_level_changed(agent.app.world_mut(), &level_file);
    assert!(
        agent
            .app
            .world()
            .get_resource::<regia_editor::asset_watch::PendingLevelReload>()
            .is_none(),
        "no reload prompt for the editor's own save"
    );
    let cube_entity = Entity::try_from_bits(cube).expect("cube bits");
    assert!(
        agent.app.world().get_entity(cube_entity).is_ok(),
        "the level was not reloaded under the preview"
    );
    assert_eq!(
        agent.call("editor.history", json!({}))["undo_depth"],
        depth_saved,
        "the undo history survives"
    );
    agent.call("editor.seq_preview", json!({ "on": false }));
    agent.call("editor.save_level", json!({ "path": "assets/room.level.ron" }));
    assert_eq!(
        std::fs::read(&level_file).expect("level"),
        level_before,
        "saved after stop"
    );

    agent.call("editor.seq_curve_save", json!({}));
    let text = std::fs::read_to_string(project.join("assets/cine/cube.seq.ron")).expect("saved");
    let sequence = Sequence::from_ron(&text).expect("valid");
    let Some(ChannelValue::Vec3(expected)) = sequence.sample(1.0).values.first().map(|v| v.value) else {
        panic!("a position at 1 s in {text}");
    };

    // Play the saved file headless.
    let mut play = App::new();
    play.add_plugins((MinimalPlugins, SequencerPlugin));
    let target = play.world_mut().spawn(Transform::default()).id();
    let player = play
        .world_mut()
        .spawn((
            SequencePlayer::new(sequence),
            SequenceBindings::default().with("cube", Binding::Entity(target)),
        ))
        .id();
    {
        let mut p = play
            .world_mut()
            .get_mut::<SequencePlayer>(player)
            .expect("player");
        p.play();
        p.pause();
        p.seek(1.0);
    }
    play.update();
    let at = play
        .world()
        .get::<Transform>(target)
        .expect("t")
        .translation
        .to_array();
    assert_eq!(at, expected, "the cube at 1 s is the sampler's value");
    assert!(
        expected[0] > 0.0 && expected[0] < 4.0,
        "between the keys: {expected:?}"
    );
}

/// w8 item 1 through the real dispatcher: open shot list A, `render.sequence` loads list B, Save:
/// A's bytes on disk are unchanged and `editor.shot_panel_state` reports no path. Then w8 item 2's
/// `editor.shot_panel_adopt` takes a sequence player's own list as an unsaved document.
#[test]
fn render_sequence_after_an_opened_shot_list_detaches_the_document() {
    let project = regia_test_utils::unique_temp_dir("seq-authoring-detach");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    let a = "assets/cine/a.shots.ron";
    agent.call("editor.create_asset", json!({ "kind": "shot_list", "path": a }));
    let a_bytes = std::fs::read(project.join(a)).expect("A");
    agent.call("editor.shot_panel_open", json!({ "path": a }));
    let mut b =
        regia_framework::shots::ShotSequence::from_ron(&String::from_utf8_lossy(&a_bytes)).expect("list");
    b.shots[0].name = "b".into();
    let b_text = b.to_ron().expect("ron");
    agent.call("render.sequence", json!({ "action": "load", "ron": b_text }));

    let state = agent.call("editor.shot_panel_state", json!({}));
    assert_eq!(state["path"], Value::Null, "{state}");
    assert_eq!(state["shots"], json!(["b"]), "{state}");
    let err = agent
        .try_call("editor.shot_panel_save", json!({}))
        .expect_err("detached");
    assert_eq!(code(&err), "shot_panel_detached", "{err:?}");
    assert_eq!(std::fs::read(project.join(a)).expect("A"), a_bytes, "A unchanged");

    // A sequence player's own shot list becomes the document.
    agent.call(
        "editor.seq_new",
        json!({ "path": "assets/cine/s.seq.ron", "length": 3.0 }),
    );
    let player = agent.call("editor.seq_curve_state", json!({}))["player"]
        .as_u64()
        .expect("player");
    let entity = Entity::try_from_bits(player).expect("bits");
    agent
        .app
        .world_mut()
        .get_mut::<SequencePlayer>(entity)
        .expect("player")
        .set_shots(Some(b));
    let adopted = agent.call(
        "editor.shot_panel_adopt",
        json!({ "player": player, "discard": true }),
    );
    assert_eq!(adopted["dirty"], true, "{adopted}");
    assert_eq!(adopted["path"], Value::Null, "{adopted}");
    let saved = agent.call(
        "editor.shot_panel_save",
        json!({ "path": "assets/cine/own.shots.ron" }),
    );
    assert_eq!(saved["dirty"], false, "{saved}");
    assert_eq!(std::fs::read(project.join(a)).expect("A"), a_bytes, "A unchanged");
}

/// `editor.select_by_asset` (spec 19 § 3, w8-editor-smalls-z) through the real dispatcher: a
/// level with two entities using `a.glb` and one using `b.glb` — the method selects exactly the
/// two; an unknown path answers ok with an empty selection.
#[test]
fn select_by_asset_selects_exactly_the_entities_using_the_asset() {
    let project = regia_test_utils::unique_temp_dir("select-by-asset-e2e");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    use regia_scene::prelude::Name as SceneName;
    let world = agent.app.world_mut();
    let a1 = world
        .spawn((
            SceneName::new("Rock A"),
            regia_render::MeshSource::new("assets/props/a.glb", ""),
        ))
        .id();
    let a2 = world
        .spawn((
            SceneName::new("Rock B"),
            regia_render::MeshSource::new("assets/props/a.glb", ""),
        ))
        .id();
    world.spawn((
        SceneName::new("Rock C"),
        regia_render::MeshSource::new("assets/props/b.glb", ""),
    ));
    let mut want: Vec<u64> = [a1, a2].iter().map(|e| e.to_bits()).collect();
    want.sort_unstable();

    let picked = agent.call("editor.select_by_asset", json!({ "path": "assets/props/a.glb" }));
    let mut got: Vec<u64> = picked["entities"]
        .as_array()
        .expect("entities")
        .iter()
        .filter_map(|v| v["entity"].as_u64())
        .collect();
    got.sort_unstable();
    assert_eq!(got, want, "{picked}");
    let selection = agent.call("editor.get_selection", json!({}));
    assert_eq!(selection["entities"].as_array().map(Vec::len), Some(2));

    // An unknown path is not an error: it answers ok with count 0 and clears nothing.
    let none = agent.call(
        "editor.select_by_asset",
        json!({ "path": "assets/props/absent.glb" }),
    );
    assert_eq!(none["count"], json!(0), "{none}");
    assert!(none["entities"].as_array().is_some_and(Vec::is_empty));

    // A mode=add call adds to the selection instead of replacing it (editor.select's modes).
    let added = agent.call(
        "editor.select_by_asset",
        json!({ "path": "assets/props/a.glb", "mode": "add" }),
    );
    assert_eq!(added["count"], json!(2), "{added}");
    let _ = std::fs::remove_dir_all(&project);
}

/// `editor.seq_section_edit { op: "sub_binding" }` (w8-editor-smalls-z) through the dispatcher:
/// set, save, reload — the section's map holds the pair and the other sub section is untouched;
/// one undo restores the saved bytes.
#[test]
fn sub_sequence_target_renames_edit_one_section_and_undo_restores_the_bytes() {
    let project = regia_test_utils::unique_temp_dir("sub-binding-e2e");
    std::fs::create_dir_all(project.join("assets/cine")).expect("cine dir");
    let mut agent = Agent::new(&project);
    agent.call(
        "editor.seq_new",
        json!({ "path": "assets/cine/bindings.seq.ron", "length": 4.0, "fps": 30.0 }),
    );
    agent.call(
        "editor.seq_track_edit",
        json!({ "op": "add", "kind": "sub", "asset": "assets/cine/fx.seq.ron", "name": "fx" }),
    );
    agent.call(
        "editor.seq_track_edit",
        json!({ "op": "add", "kind": "sub", "asset": "assets/cine/fx2.seq.ron", "name": "fx2" }),
    );
    agent.call("editor.seq_curve_save", json!({}));
    let file = project.join("assets/cine/bindings.seq.ron");
    let baseline = std::fs::read(&file).expect("baseline save");

    let set = agent.call(
        "editor.seq_section_edit",
        json!({ "track": "fx", "op": "sub_binding", "index": 0, "from": "lamp", "to": "hero" }),
    );
    assert_eq!(set["applied"], json!(true), "{set}");
    agent.call("editor.seq_curve_save", json!({}));
    let saved = std::fs::read_to_string(&file).expect("saved");
    let sequence = Sequence::from_ron(&saved).expect("valid");
    let sub_bindings = |name: &str| {
        sequence
            .tracks
            .iter()
            .find(|t| t.name == name)
            .and_then(|t| match &t.keys {
                regia_framework::sequencer::Channel::Sub(list) => Some(list[0].bindings.clone()),
                _ => None,
            })
            .unwrap_or_default()
    };
    assert_eq!(
        sub_bindings("fx").get("lamp").map(String::as_str),
        Some("hero"),
        "the edited section's map holds the pair"
    );
    assert!(sub_bindings("fx2").is_empty(), "the other section is untouched");

    // One undo restores the bytes the file had before the set.
    agent.call("editor.undo", json!({}));
    agent.call("editor.seq_curve_save", json!({}));
    assert_eq!(
        std::fs::read(&file).expect("re-saved"),
        baseline,
        "undo restores the bytes"
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// New ▸ Material instance (spec 80 § 11.5, w8-editor-smalls-z): `editor.create_asset` with kind
/// `material_instance` writes the bytes `MaterialInstance::to_ron` writes for the same parent,
/// and the file loads back.
#[test]
fn a_material_instance_created_over_the_protocol_writes_to_ron_bytes() {
    let project = regia_test_utils::unique_temp_dir("create-instance-e2e");
    std::fs::create_dir_all(project.join("assets/materials")).expect("dir");
    let graph_path = project.join("assets/materials/rock.matgraph.ron");
    std::fs::write(
        &graph_path,
        regia_matgraph::MaterialGraph::new("rock")
            .to_ron()
            .expect("graph"),
    )
    .expect("graph");
    let mut agent = Agent::new(&project);

    let made = agent.call(
        "editor.create_asset",
        json!({
            "kind": "material_instance",
            "path": "assets/materials/rock_polish.matinst.ron",
            "parent": "assets/materials/rock.matgraph.ron"
        }),
    );
    assert_eq!(made["path"], json!("assets/materials/rock_polish.matinst.ron"));
    let bytes = std::fs::read(project.join("assets/materials/rock_polish.matinst.ron")).expect("file");
    let expected = regia_matgraph::MaterialInstance::new("assets/materials/rock.matgraph.ron")
        .to_ron()
        .expect("instance ron");
    assert_eq!(bytes, expected.as_bytes(), "the same bytes as to_ron");
    let instance = regia_matgraph::MaterialInstance::from_ron(std::str::from_utf8(&bytes).expect("utf8"))
        .expect("loads back");
    assert_eq!(instance.parent, "assets/materials/rock.matgraph.ron");

    // A missing parent refuses and writes nothing.
    let refused = agent
        .try_call(
            "editor.create_asset",
            json!({
                "kind": "material_instance",
                "path": "assets/materials/ghost.matinst.ron",
                "parent": "assets/materials/absent.matgraph.ron"
            }),
        )
        .expect_err("parent must exist");
    assert!(refused.message.contains("absent.matgraph.ron"), "{refused:?}");
    assert!(!project.join("assets/materials/ghost.matinst.ron").exists());
    let _ = std::fs::remove_dir_all(&project);
}

/// `editor.matgraph_find` (w8-editor-smalls-z): with `edit_scene` withheld (the fail-closed
/// read-only profile), the find answers the listing, `editor.matgraph_search { add }` is still
/// refused, and both return identical lists for the same query.
#[test]
fn a_read_only_agent_finds_material_graph_nodes_where_search_cannot_add() {
    let project = regia_test_utils::unique_temp_dir("matgraph-find-e2e");
    std::fs::create_dir_all(project.join("assets/materials")).expect("dir");
    let graph = project.join("assets/materials/find.matgraph.ron");
    std::fs::write(
        &graph,
        regia_matgraph::MaterialGraph::new("find")
            .to_ron()
            .expect("graph"),
    )
    .expect("graph");
    let mut agent = Agent::new(&project);
    agent.call(
        "editor.matgraph_open",
        json!({ "path": "assets/materials/find.matgraph.ron" }),
    );
    let full = agent.call("editor.matgraph_find", json!({ "query": "mul" }));

    // Withhold every mutating permission: the fail-closed profile is read-only.
    agent
        .app
        .world_mut()
        .insert_resource(regia_agent::AgentPermissions {
            config: regia_agent::AgentConfig::fail_closed(),
            source: None,
        });
    let readonly = agent
        .try_call("editor.matgraph_find", json!({ "query": "mul" }))
        .expect("find is read-only");
    assert_eq!(readonly, full, "the same listing");
    assert!(
        readonly["kinds"].as_array().is_some_and(|k| !k.is_empty()),
        "the fixture's search has results: {readonly}"
    );
    let refused = agent
        .try_call(
            "editor.matgraph_search",
            json!({ "query": "mul", "add": "Mul", "pos": [0.0, 0.0] }),
        )
        .expect_err("search needs edit_scene");
    assert_eq!(code(&refused), "F0012", "{refused:?}");
    // The mutating method is refused wholesale for a read-only agent: the listing is find's job.
    let _ = std::fs::remove_dir_all(&project);
}
