// SPDX-License-Identifier: MPL-2.0
use super::*;

use crate::command::{EditorCommand as _, SpawnEntities};
use regia_core::id::Guid;
use regia_scene::prelude::Transform;

/// A scratch project folder, removed at the end of the test.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(dir.join("assets")).expect("assets");
        std::fs::create_dir_all(dir.join("generators")).expect("generators");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An edit-world-like app on `root`, marked headless without an adapter.
fn headless_app(root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>();
    app.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    prepare(app.world_mut(), false);
    app
}

/// Spawns a named entity through the undo stack, so the level is dirty.
fn spawn_dirty(world: &mut World, name: &str) {
    let mut command =
        SpawnEntities::new_entity(world, Guid::new_random(), name, Transform::default()).expect("spawn");
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
}

fn code(err: &RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[test]
fn install_registers_every_method_once_with_the_capabilities() {
    let mut app = App::new();
    install(app.world_mut());
    let extension = app.world().resource::<regia_agent::extension::AgentExtension>();
    let listed: Vec<String> = extension
        .schema_entries()
        .iter()
        .filter_map(|m| m.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    for (method, _) in methods() {
        assert_eq!(
            listed.iter().filter(|n| *n == method.name).count(),
            1,
            "{}",
            method.name
        );
    }
    let caps: Vec<&str> = extension.capabilities().iter().map(|(n, _)| *n).collect();
    assert!(
        caps.contains(&"generators") && caps.contains(&"editor_headless"),
        "{caps:?}"
    );
}

#[test]
fn view_methods_answer_editor_headless_without_an_adapter() {
    let scratch = Scratch::new("headless-no-adapter");
    let mut app = headless_app(&scratch.0);
    let err = no_adapter_error(app.world()).expect("no adapter");
    assert_eq!(code(&err), EDITOR_HEADLESS);
    let Err(err) = crate::agent_ext::viewport_device(app.world_mut()) else {
        panic!("a headless editor without an adapter opened a device");
    };
    assert_eq!(code(&err), EDITOR_HEADLESS);
    // The agent's captures and render-out refuse the same way (ADR-5180).
    let disabled = app
        .world()
        .get_resource::<regia_agent::GpuDisabled>()
        .expect("GpuDisabled without an adapter");
    assert_eq!(code(&disabled.error()), EDITOR_HEADLESS);
    assert!(disabled.reason.contains("--no-gpu"), "{}", disabled.reason);
    // A windowed world (no HeadlessEditor) never answers editor_headless.
    let windowed = App::new();
    assert!(no_adapter_error(windowed.world()).is_none());
}

fn hint(err: &RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("hint"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The agent's `GpuDisabled` refusal is the view methods' `no_adapter_error`, word for word, and
/// the hint only tells the caller to drop `--no-gpu` when the editor was started with it; an editor
/// whose adapter request failed is told to install a driver instead.
#[test]
fn the_capture_refusal_is_the_view_refusal_and_the_hint_matches_the_cause() {
    let scratch = Scratch::new("headless-refusal-text");
    let app = headless_app(&scratch.0);
    let view = no_adapter_error(app.world()).expect("no adapter");
    let disabled = app
        .world()
        .get_resource::<regia_agent::GpuDisabled>()
        .expect("GpuDisabled without an adapter")
        .error();
    assert_eq!(disabled, view, "the same refusal");
    assert!(hint(&view).contains("without --no-gpu"), "{view:?}");

    let mut found_none = App::new();
    found_none.insert_resource(HeadlessEditor {
        no_adapter_reason: Some("no suitable adapter found".to_string()),
        ..HeadlessEditor::default()
    });
    let view = no_adapter_error(found_none.world()).expect("no adapter");
    assert_eq!(code(&view), EDITOR_HEADLESS);
    assert!(view.message.contains("no suitable adapter found"), "{view:?}");
    assert!(
        !hint(&view).contains("--no-gpu"),
        "the editor was not started with --no-gpu: {view:?}"
    );
    assert!(hint(&view).contains("lavapipe"), "{view:?}");
}

#[test]
fn headless_status_reports_the_mode_and_the_dirty_level() {
    let scratch = Scratch::new("headless-status");
    let mut app = headless_app(&scratch.0);
    let status = headless_status(app.world_mut(), &json!({})).expect("status");
    assert_eq!(status["headless"], true);
    assert!(status["adapter"].is_null());
    assert_eq!(status["no_adapter_reason"], "started with --no-gpu");
    assert_eq!(status["dirty"], false);
    spawn_dirty(app.world_mut(), "Crate");
    let status = headless_status(app.world_mut(), &json!({})).expect("status");
    assert_eq!(status["dirty"], true);
}

#[test]
fn shutdown_saves_an_untitled_dirty_level_into_assets() {
    let scratch = Scratch::new("headless-shutdown");
    let mut app = headless_app(&scratch.0);
    spawn_dirty(app.world_mut(), "Barrel");
    let out = shutdown(app.world_mut(), &json!({})).expect("shutdown");
    assert_eq!(out["exiting"], true);
    assert_eq!(out["headless"], true);
    assert_eq!(out["saved"], json!(["assets/unsaved.level.ron"]));
    let text = std::fs::read_to_string(scratch.0.join("assets/unsaved.level.ron")).expect("saved level");
    assert!(text.contains("Barrel"), "{text}");
    assert!(!app.world().resource::<UndoStack>().is_dirty());
    assert!(app.world().resource::<HeadlessEditor>().shutdown_requested);
    // Nothing dirty: a second save writes nothing.
    assert_eq!(save_dirty_state(app.world_mut()), SaveSummary::default());
}

#[test]
fn shutdown_keeps_the_session_dirty_when_the_tilemap_cannot_be_written() {
    let scratch = Scratch::new("headless-shutdown-tilemap");
    let mut app = headless_app(&scratch.0);
    spawn_dirty(app.world_mut(), "Barrel");
    let map = regia_2d::tilemap::TileMapAsset::from_ron(
        r#"(version: 1, width: 2, height: 1, tile_width: 8, tile_height: 8,
            tilesets: [(name: "t", first_gid: 1, image: "t.png", image_width: 16, image_height: 8,
                        tile_width: 8, tile_height: 8, columns: 2, tile_count: 2)],
            layers: [Tiles((name: "ground", data: [1, 2]))])"#,
    )
    .expect("map");
    // The map's folder does not exist yet, so its write fails.
    let folder = scratch.0.join("assets/maps");
    let target = folder.join("level.tilemap.ron");
    let mut mode = crate::mode2d::Mode2d::default();
    mode.doc = Some(crate::mode2d::TileDocument {
        path: target.clone(),
        map,
        dirty: true,
        images: Vec::new(),
    });
    app.world_mut().insert_resource(mode);

    let err = shutdown(app.world_mut(), &json!({})).expect_err("the tile map is not saved");
    assert_eq!(code(&err), "shutdown_save_failed", "{err:?}");
    assert!(err.message.contains("tile map"), "{}", err.message);
    assert!(
        app.world().resource::<UndoStack>().is_dirty(),
        "the strokes are still unsaved"
    );
    assert!(!app.world().resource::<HeadlessEditor>().shutdown_requested);

    // Once the folder exists, a second shutdown writes both documents and exits.
    std::fs::create_dir_all(&folder).expect("folder");
    let out = shutdown(app.world_mut(), &json!({})).expect("shutdown");
    assert_eq!(out["exiting"], true);
    assert!(target.exists(), "the tile map was written");
    assert!(!app.world().resource::<UndoStack>().is_dirty());
}

#[test]
fn shutdown_keeps_running_when_a_dirty_tilemap_cannot_be_saved() {
    let scratch = Scratch::new("headless-tilemap-fail");
    let mut app = headless_app(&scratch.0);
    spawn_dirty(app.world_mut(), "Barrel");
    let map = regia_2d::tilemap::TileMapAsset::from_ron(
        r#"(version: 1, width: 1, height: 1, tile_width: 8, tile_height: 8, tilesets: [], layers: [Tiles((name: "g", data: [0]))])"#,
    )
    .expect("map");
    let mut mode = crate::mode2d::Mode2d::default();
    mode.doc = Some(crate::mode2d::TileDocument {
        path: scratch.0.join("assets/maps/floor.txt"),
        map,
        dirty: true,
        images: Vec::new(),
    });
    app.world_mut().insert_resource(mode);
    let err = shutdown(app.world_mut(), &json!({})).expect_err("tilemap");
    assert_eq!(code(&err), "shutdown_save_failed");
    assert!(err.message.contains("tile map"), "{}", err.message);
    assert!(!app.world().resource::<HeadlessEditor>().shutdown_requested);
    assert!(app.world().resource::<UndoStack>().is_dirty());
    assert!(!scratch.0.join("assets/maps/floor.txt").exists());
    let again = save_dirty_state(app.world_mut());
    assert!(
        again.failed.iter().any(|line| line.contains("tile map")),
        "{again:?}"
    );
    assert!(app.world().resource::<UndoStack>().is_dirty());
}

#[test]
fn shutdown_without_saving_drops_the_edits() {
    let scratch = Scratch::new("headless-shutdown-nosave");
    let mut app = headless_app(&scratch.0);
    spawn_dirty(app.world_mut(), "Barrel");
    let out = shutdown(app.world_mut(), &json!({ "save": false })).expect("shutdown");
    assert_eq!(out["saved"], json!([]));
    assert!(!scratch.0.join("assets/unsaved.level.ron").exists());
}

#[test]
fn windowed_shutdown_hands_the_shell_documents_to_the_event_loop() {
    let scratch = Scratch::new("headless-windowed-shutdown");
    let mut app = headless_app(&scratch.0);
    app.world_mut().remove_resource::<HeadlessEditor>();
    spawn_dirty(app.world_mut(), "Barrel");
    let out = shutdown(app.world_mut(), &json!({})).expect("shutdown");
    assert_eq!(out["headless"], false);
    assert_eq!(out["saved"], json!(["assets/unsaved.level.ron"]));
    assert!(
        app.world().contains_resource::<ShutdownRequest>(),
        "main.rs saves the shell's documents next"
    );
}

#[test]
fn a_headless_shutdown_records_what_it_saved_for_the_exit_line() {
    let scratch = Scratch::new("headless-shutdown-record");
    let mut app = headless_app(&scratch.0);
    spawn_dirty(app.world_mut(), "Barrel");
    shutdown(app.world_mut(), &json!({})).expect("shutdown");
    assert_eq!(
        app.world().resource::<HeadlessEditor>().shutdown_saved,
        ["assets/unsaved.level.ron"]
    );
    assert!(!app.world().contains_resource::<ShutdownRequest>());
}

#[test]
fn open_level_opens_a_saved_level_and_guards_unsaved_edits() {
    let scratch = Scratch::new("headless-open-level");
    let mut app = headless_app(&scratch.0);
    spawn_dirty(app.world_mut(), "Lamp");
    let saved = save_dirty_state(app.world_mut());
    assert_eq!(saved.saved, ["assets/unsaved.level.ron"]);
    std::fs::rename(
        scratch.0.join("assets/unsaved.level.ron"),
        scratch.0.join("assets/room.level.ron"),
    )
    .expect("rename");
    let fresh = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(fresh);
    spawn_dirty(app.world_mut(), "Other");

    let err = open_level(app.world_mut(), &json!({ "path": "assets/room.level.ron" })).expect_err("dirty");
    assert_eq!(code(&err), "level_dirty");
    let out = open_level(
        app.world_mut(),
        &json!({ "path": "assets/room.level.ron", "discard": true }),
    )
    .expect("open");
    assert_eq!(out["path"], "assets/room.level.ron");
    assert!(out["entities"].as_u64().unwrap_or(0) >= 1, "{out}");
    assert!(!app.world().resource::<UndoStack>().is_dirty());
    let status = headless_status(app.world_mut(), &json!({})).expect("status");
    assert_eq!(status["level"], "assets/room.level.ron");

    let err = open_level(app.world_mut(), &json!({ "path": "assets/none.level.ron" })).expect_err("missing");
    assert_eq!(code(&err), "level_not_found");
    assert!(open_level(app.world_mut(), &json!({ "path": "../outside.level.ron" })).is_err());
    assert!(open_level(app.world_mut(), &json!({})).is_err());
}

const GROUND: &str = r#"
format_version = 1

[generator]
kind = "heightfield"
seed = 3
outputs = ["assets/gen/ground.r16"]

[generator.heightfield]
width = 16
height = 16
"#;

#[test]
fn gen_methods_list_run_and_verify_through_the_handlers() {
    let scratch = Scratch::new("headless-gen");
    std::fs::write(scratch.0.join("generators/ground.toml"), GROUND).expect("declare");
    let mut app = headless_app(&scratch.0);
    let world = app.world_mut();

    let listed = gen_list(world, &json!({})).expect("list");
    assert_eq!(listed["generators"][0]["name"], "ground");
    assert_eq!(listed["generators"][0]["kind"], "heightfield");
    assert_eq!(listed["generators"][0]["seed"], 3);

    let ran = gen_run(world, &json!({ "name": "ground" })).expect("run");
    assert_eq!(ran["outputs"][0]["path"], "assets/gen/ground.r16");
    assert_eq!(ran["outputs"][0]["written"], true);
    let verified = gen_verify(world, &json!({})).expect("verify");
    assert_eq!(verified["ok"], true, "{verified}");

    let output = scratch.0.join("assets/gen/ground.r16");
    let mut bytes = std::fs::read(&output).expect("read");
    bytes[0] ^= 1;
    std::fs::write(&output, bytes).expect("edit");
    let verified = gen_verify(world, &json!({ "names": ["ground"] })).expect("verify");
    assert_eq!(verified["ok"], false);
    assert_eq!(verified["diffs"][0]["kind"], "changed");
    assert_eq!(verified["diffs"][0]["first_difference"], 0);

    let err = gen_run(world, &json!({ "name": "nope" })).expect_err("unknown");
    assert_eq!(code(&err), "gen_not_found");
    assert!(gen_verify(world, &json!({ "names": "ground" })).is_err());
    assert!(gen_run(world, &json!({})).is_err());
}

#[test]
fn gen_methods_need_a_project() {
    let mut app = App::new();
    let err = gen_list(app.world_mut(), &json!({})).expect_err("no project");
    assert_eq!(code(&err), "no_project");
}

// A long generator's job, the short path and the golden output guard (ADR-2821). The real binary's
// test (`tests/headless_editor.rs`) drives the job through `agent.job_status`.

#[test]
fn gen_run_answers_the_report_on_the_short_path_with_any_wait_that_lets_it_finish() {
    let scratch = Scratch::new("headless-gen-short");
    std::fs::write(
        scratch.0.join("generators/ground.toml"),
        "format_version = 1\n\n[generator]\nkind = \"heightfield\"\nseed = 1\noutputs = [\"assets/gen/ground.r16\"]\n\n[generator.heightfield]\nwidth = 8\nheight = 8\n",
    )
    .expect("generator");
    let mut app = headless_app(&scratch.0);
    let report = gen_run(
        app.world_mut(),
        &json!({ "name": "ground", "wait_ms": MAX_GEN_WAIT_MS }),
    )
    .expect("report");
    assert_eq!(report["outputs"][0]["path"], "assets/gen/ground.r16", "{report}");
    let verified = gen_verify(app.world_mut(), &json!({ "wait_ms": MAX_GEN_WAIT_MS })).expect("verify");
    assert_eq!(verified["ok"], true, "{verified}");
    // Errors keep their code on the short path.
    let missing = gen_run(
        app.world_mut(),
        &json!({ "name": "nope", "wait_ms": MAX_GEN_WAIT_MS }),
    )
    .expect_err("missing");
    assert_eq!(code(&missing), "gen_not_found");
    for wait in [json!(MAX_GEN_WAIT_MS + 1), json!(-1), json!("5"), json!(1.5)] {
        let refused = gen_run(app.world_mut(), &json!({ "name": "ground", "wait_ms": wait }));
        assert_eq!(refused.err().map(|e| e.code), Some(protocol::INVALID_PARAMS));
    }
}

#[test]
fn gen_run_refuses_a_declared_output_under_goldens() {
    let scratch = Scratch::new("headless-gen-golden");
    std::fs::write(
        scratch.0.join("generators/aimed.toml"),
        "format_version = 1\n\n[generator]\nkind = \"heightfield\"\nseed = 1\noutputs = [\"crates/x/GOLDENS/a.r16\"]\n\n[generator.heightfield]\nwidth = 8\nheight = 8\n",
    )
    .expect("generator");
    let mut app = headless_app(&scratch.0);
    let refused = gen_run(app.world_mut(), &json!({ "name": "aimed" })).expect_err("refused");
    let reason = refused
        .data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_string();
    assert_eq!(reason, "golden_path_denied");
    assert!(!scratch.0.join("crates").exists(), "nothing was written");
}

/// rev-claude-57 seq-shot finding 3 (ADR-7495): a headless frame runs the auto key the windowed
/// shell runs once per frame, so an agent's move is keyed at the next editor frame (as
/// `seq-curve-panel.md` says), once.
#[test]
fn a_headless_frame_keys_a_move_with_auto_key_on() {
    use regia_framework::sequencer::SequencePlayer;
    use regia_scene::prelude::EntityGuid;

    let scratch = Scratch::new("headless-auto-key");
    let mut app = headless_app(&scratch.0);
    let call = |world: &mut World, method: &str, params: Value| {
        let (_, handler) = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == method)
            .unwrap_or_else(|| panic!("{method} is installed"));
        handler(world, &params).unwrap_or_else(|e| panic!("{method}: {}", e.message))
    };
    let keys_of = |world: &World, player: Entity| -> usize {
        world
            .get::<SequencePlayer>(player)
            .map_or(0, |p| p.sequence().tracks.iter().map(|t| t.keys.len()).sum())
    };
    let world = app.world_mut();
    let sequence = regia_framework::sequencer::edit::tracks::new_sequence("intro", 5.0, 30.0).expect("new");
    let player = world.spawn(SequencePlayer::new(sequence)).id();
    let cube = world
        .spawn((Transform::default(), EntityGuid(Guid::new_random())))
        .id();
    call(
        world,
        "editor.seq_binding_edit",
        serde_json::json!({ "op": "add", "binding": "cube", "entity": cube.to_bits() }),
    );
    call(world, "editor.seq_key", serde_json::json!({ "auto": true }));
    assert_eq!(keys_of(world, player), 0);
    let mut moved = world.get::<Transform>(cube).copied().unwrap_or_default();
    moved.translation.x = 1.0;
    let mut command = crate::command::SetTransform::capture(world, cube, moved);
    command.apply(world);
    let mut undo = world.get_resource_or_insert_with(UndoStack::new);
    undo.push(Box::new(command));
    undo.seal();
    let window = app.world_mut().spawn_empty().id();
    let mut editor = Loop {
        app,
        window,
        host: None,
        pie: None,
    };
    editor.frame();
    assert_eq!(
        keys_of(editor.app.world(), player),
        3,
        "position, rotation and scale keyed at the frame"
    );
    editor.frame();
    assert_eq!(keys_of(editor.app.world(), player), 3, "keyed once");
}

#[test]
fn open_level_refuses_while_a_prefab_is_being_edited() {
    let scratch = Scratch::new("headless-open-level-prefab");
    let mut app = headless_app(&scratch.0);
    std::fs::create_dir_all(scratch.0.join("assets/props")).expect("props dir");
    std::fs::write(
        scratch.0.join("assets/props/crate.prefab.ron"),
        r#"( format_version: 1, entities: [
            ( guid: "8b6f2a11-0000-4000-8000-000000000001", name: "Root",
              components: { "regia_scene::transform::Transform": () } ),
        ] )"#,
    )
    .expect("prefab");
    app.world_mut()
        .insert_resource(crate::prefab_edit::PrefabEditMode::default());
    crate::prefab_edit::enter_prefab_edit(app.world_mut(), &scratch.0.join("assets/props/crate.prefab.ron"))
        .expect("enter prefab edit");

    std::fs::write(
        scratch.0.join("assets/room.level.ron"),
        r#"( format_version: 1, entities: [] )"#,
    )
    .expect("level");
    let level = app.world().resource::<OpenLevel>().id;
    let err = open_level(
        app.world_mut(),
        &json!({ "path": "assets/room.level.ron", "discard": true }),
    )
    .expect_err("prefab mode refuses");
    assert_eq!(code(&err), "prefab_open");
    assert_eq!(
        app.world().resource::<OpenLevel>().id,
        level,
        "the same level stays open"
    );
}
