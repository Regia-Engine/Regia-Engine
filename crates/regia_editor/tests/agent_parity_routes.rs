// SPDX-License-Identifier: MPL-2.0
//! Agent routes added for editor UI parity (ADR-7747), each driven through the real agent
//! dispatcher (requests from an in-process inbox, each answered within one frame; no wall-clock
//! waits):
//!
//! - `editor.level_reload_resolve`: the status bar's "Reload from disk" / "Keep mine" prompt. The
//!   open level changes on disk while it has unsaved edits, the prompt is pending, and the agent
//!   resolves it both ways. The change is delivered with `asset_watch::on_level_changed` (what the
//!   file watcher calls), so no watcher runs.
//! - `editor.seq_key { entity }`: key a bound entity's binding without selecting it.
//! - `editor.set_property` for each reflect-driven Inspector value-editor kind (drag value,
//!   slider, text edit, combo box, angle and color edit): the generic route the parity scan counts
//!   for every Inspector widget.
//! - The value-editor gaps this lane closed, one test each.

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_core::math::Quat;
use regia_ecs::attrs::{AssetFilter, Multiline};
use regia_editor::asset_watch::{self, PendingLevelReload};
use regia_editor::command::UndoStack;
use regia_editor::level_io;
use regia_editor::selection::Selection;
use regia_scene::prelude::{Name, Transform};
use serde_json::{Value, json};

struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Agent {
    fn new(project: &std::path::Path) -> Self {
        Self::with(project, |_| {})
    }

    /// An editor app; `setup` runs before the agent extension installs (to register types).
    fn with(project: &std::path::Path, setup: impl FnOnce(&mut App)) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        setup(&mut app);
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

    fn names(&mut self) -> Vec<String> {
        let mut q = self.app.world_mut().query::<&Name>();
        let mut out: Vec<String> = q.iter(self.app.world()).map(|n| n.as_str().to_string()).collect();
        out.sort();
        out
    }

    fn pending(&self) -> bool {
        self.app.world().contains_resource::<PendingLevelReload>()
    }
}

fn code(err: &RpcError) -> &str {
    err.data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Rewrites the level file as another program would: `from` renamed to `to`, atomically.
fn change_on_disk(path: &std::path::Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).expect("level");
    assert!(text.contains(from), "{from} in {text}");
    let tmp = path.with_extension("ron.tmp");
    std::fs::write(&tmp, text.replace(from, to)).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

#[test]
fn an_external_change_with_unsaved_edits_is_resolved_both_ways_by_the_agent() {
    let project = regia_test_utils::unique_temp_dir("level-reload-resolve");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    agent.call("editor.spawn", json!({ "name": "Origin" }));
    agent.call("editor.save_level", json!({ "path": "assets/room.level.ron" }));
    let level_file = project.join("assets/room.level.ron");

    // No prompt: the status form says so, and a choice is refused without acting.
    assert_eq!(
        agent.call("editor.level_reload_resolve", json!({})),
        json!({ "pending": false })
    );
    let err = agent
        .try_call("editor.level_reload_resolve", json!({ "choice": "reload" }))
        .expect_err("nothing pending");
    assert_eq!(code(&err), "no_pending_reload", "{err:?}");
    let err = agent
        .try_call("editor.level_reload_resolve", json!({ "choice": "later" }))
        .expect_err("bad choice");
    assert_eq!(code(&err), "bad_choice", "{err:?}");

    // 1. Unsaved edit, then an external change: the prompt waits; keep_mine keeps the edits.
    agent.call("editor.spawn", json!({ "name": "Unsaved" }));
    assert!(agent.app.world().resource::<UndoStack>().is_dirty());
    change_on_disk(&level_file, "\"Origin\"", "\"Gantry\"");
    asset_watch::on_level_changed(agent.app.world_mut(), &level_file);
    assert!(
        agent.pending(),
        "an external change over unsaved edits asks first"
    );
    let status = agent.call("editor.level_reload_resolve", json!({}));
    assert_eq!(status["pending"], json!(true), "{status}");
    assert!(
        status["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("room.level.ron")),
        "{status}"
    );
    assert!(agent.pending(), "the status form does not answer the prompt");
    let kept = agent.call("editor.level_reload_resolve", json!({ "choice": "keep_mine" }));
    assert_eq!(kept["choice"], json!("keep_mine"), "{kept}");
    assert_eq!(kept["reloaded"], json!(false), "{kept}");
    assert!(!agent.pending(), "the prompt is answered");
    assert_eq!(agent.names(), ["Origin", "Unsaved"], "the edits are kept");
    assert!(agent.app.world().resource::<UndoStack>().is_dirty());

    // 2. Another external change: reload discards the edits and loads the file.
    change_on_disk(&level_file, "\"Gantry\"", "\"Beacon\"");
    asset_watch::on_level_changed(agent.app.world_mut(), &level_file);
    assert!(agent.pending());
    let reloaded = agent.call("editor.level_reload_resolve", json!({ "choice": "reload" }));
    assert_eq!(reloaded["choice"], json!("reload"), "{reloaded}");
    assert_eq!(reloaded["reloaded"], json!(true), "{reloaded}");
    assert!(
        reloaded["status"]
            .as_str()
            .is_some_and(|line| line.starts_with("reloaded ")),
        "{reloaded}"
    );
    assert!(!agent.pending());
    assert_eq!(agent.names(), ["Beacon"], "the file's level, without the edits");
    let history = agent.call("editor.history", json!({}));
    assert_eq!(history["dirty"], json!(false), "{history}");
    assert_eq!(history["undo_depth"], json!(0), "{history}");

    // Answered: a second choice is refused.
    let err = agent
        .try_call("editor.level_reload_resolve", json!({ "choice": "keep_mine" }))
        .expect_err("answered");
    assert_eq!(code(&err), "no_pending_reload", "{err:?}");
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn seq_key_keys_the_binding_of_a_named_entity_without_a_selection() {
    let project = regia_test_utils::unique_temp_dir("seq-key-entity");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    agent.call(
        "editor.seq_new",
        json!({ "path": "assets/cine/door.seq.ron", "length": 2.0, "fps": 30.0 }),
    );
    let door = agent.call("editor.spawn", json!({ "name": "Door" }))["entity"]
        .as_u64()
        .expect("door");
    let other = agent.call("editor.spawn", json!({ "name": "Other" }))["entity"]
        .as_u64()
        .expect("other");
    agent.call(
        "editor.seq_binding_edit",
        json!({ "op": "add", "binding": "door", "entity": door }),
    );
    agent.call("editor.select", json!({ "entities": [] }));

    // Without a selection the old form is refused; `entity` names the binding instead.
    let err = agent
        .try_call("editor.seq_key", json!({ "channel": "position" }))
        .expect_err("no selection");
    assert_eq!(code(&err), "seq_key_no_selection", "{err:?}");
    let keyed = agent.call(
        "editor.seq_key",
        json!({ "entity": door, "channel": "position", "time": 0.5 }),
    );
    assert_eq!(keyed["binding"], json!("door"), "{keyed}");
    assert_eq!(keyed["keys"][0]["track"], json!("door.position"), "{keyed}");
    assert_eq!(keyed["keys"][0]["created"], json!(true), "{keyed}");

    // With auto, `entity` keys too (like `binding`).
    let auto = agent.call(
        "editor.seq_key",
        json!({ "entity": door, "channel": "position", "time": 1.0, "auto": false }),
    );
    assert_eq!(auto["binding"], json!("door"), "{auto}");

    for (params, want) in [
        (
            json!({ "entity": door, "binding": "door" }),
            "seq_key_binding_and_entity",
        ),
        (json!({ "entity": other }), "seq_key_unbound"),
        (json!({ "entity": 999_999_999u64 }), "seq_key_no_entity"),
    ] {
        let err = agent
            .try_call("editor.seq_key", params.clone())
            .expect_err("refused");
        assert_eq!(code(&err), want, "{params}: {err:?}");
    }
    let err = agent
        .try_call("editor.seq_key", json!({ "entity": "door" }))
        .expect_err("not a number");
    assert!(err.message.contains("entity"), "{err:?}");
    let _ = std::fs::remove_dir_all(&project);
}

// --- Inspector value editors: `editor.set_property` per widget kind ----------------------------
//
// The reflect-driven Inspector draws a field by its type (crates/regia_editor/src/inspector):
// numbers with a `DragValue` (or a `Slider`), strings with `text_edit_singleline`, `@Multiline`
// strings with `text_edit_multiline`, `@AssetFilter` strings with the asset picker's
// `TextEdit::singleline`, enums with a `ComboBox` of variants, rotations with `drag_angle` / Euler
// `DragValue`s, and colour-named `[f32; 3]` fields with `color_edit_button` plus channel
// `DragValue`s. The parity scan counts each of those sites once, as a `method` row via
// `editor.set_property`; these tests prove that route writes each kind of value.

/// What a gadget does (an enum the Inspector draws as a combo box).
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Default)]
enum GadgetMode {
    #[default]
    Idle,
    Spin(f32),
}

/// A component with one field per Inspector value-editor kind.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
struct Gadget {
    speed: f32,
    count: u32,
    label: String,
    #[reflect(@Multiline)]
    notes: String,
    #[reflect(@AssetFilter("*.ogg"))]
    voice: String,
    mode: GadgetMode,
    base_color: [f32; 3],
}

fn gadget_agent() -> (Agent, u64, std::path::PathBuf) {
    let project = regia_test_utils::unique_temp_dir("inspector-value-routes");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::with(&project, |app| {
        app.register_type::<Gadget>();
    });
    let entity = agent
        .app
        .world_mut()
        .spawn((
            Name::new("Gadget"),
            Transform::default(),
            Gadget {
                mode: GadgetMode::Spin(2.0),
                ..Gadget::default()
            },
        ))
        .id();
    agent.app.update();
    (agent, entity.to_bits(), project)
}

fn gadget(agent: &Agent, bits: u64) -> Gadget {
    let entity = Entity::try_from_bits(bits).expect("bits");
    agent.app.world().get::<Gadget>(entity).cloned().expect("gadget")
}

/// Sets `path` of `component` through the dispatcher; one undo entry each.
fn set_field(agent: &mut Agent, bits: u64, component: &str, path: &str, value: Value) {
    let depth = agent.call("editor.history", json!({}))["undo_depth"].as_u64();
    let result = agent.call(
        "editor.set_property",
        json!({ "entity": bits, "component": component, "path": path, "value": value }),
    );
    assert_eq!(result["applied"], json!(true), "{path}: {result}");
    assert_eq!(
        agent.call("editor.history", json!({}))["undo_depth"].as_u64(),
        depth.map(|d| d + 1),
        "{path}: one undo entry"
    );
}

/// Two rotations are the same (or opposite, the same orientation) quaternion. A dot product, not
/// `angle_between`, whose `acos` near 1 is coarse in `f32`.
fn same_rotation(a: Quat, b: Quat) -> bool {
    a.dot(b).abs() > 1.0 - 1e-5
}

/// `DragValue` and `Slider` (numbers, float and integral).
#[test]
fn inspector_drag_value_and_slider_fields_are_set_by_set_property() {
    let (mut agent, bits, project) = gadget_agent();
    set_field(&mut agent, bits, "Gadget", "speed", json!(3.5));
    set_field(&mut agent, bits, "Gadget", "count", json!(7));
    let g = gadget(&agent, bits);
    assert_eq!((g.speed, g.count), (3.5, 7));
    let _ = std::fs::remove_dir_all(&project);
}

/// `text_edit_singleline`, `text_edit_multiline` and the asset picker's `TextEdit::singleline`.
#[test]
fn inspector_text_edit_fields_are_set_by_set_property() {
    let (mut agent, bits, project) = gadget_agent();
    set_field(&mut agent, bits, "Gadget", "label", json!("north gate"));
    set_field(&mut agent, bits, "Gadget", "notes", json!("line one\nline two"));
    set_field(&mut agent, bits, "Gadget", "voice", json!("assets/sfx/hum.ogg"));
    let g = gadget(&agent, bits);
    assert_eq!(g.label, "north gate");
    assert_eq!(g.notes, "line one\nline two");
    assert_eq!(g.voice, "assets/sfx/hum.ogg");
    let _ = std::fs::remove_dir_all(&project);
}

/// The enum `ComboBox` (its `selectable_value` rows pick the variant).
#[test]
fn inspector_combo_box_variant_is_set_by_set_property() {
    let (mut agent, bits, project) = gadget_agent();
    assert_eq!(gadget(&agent, bits).mode, GadgetMode::Spin(2.0));
    set_field(&mut agent, bits, "Gadget", "mode", json!("Idle"));
    assert_eq!(gadget(&agent, bits).mode, GadgetMode::Idle);
    let _ = std::fs::remove_dir_all(&project);
}

/// `drag_angle` and the Euler `DragValue`s (a rotation, in degrees per axis).
#[test]
fn inspector_angle_fields_are_set_by_set_property() {
    let (mut agent, bits, project) = gadget_agent();
    let entity = Entity::try_from_bits(bits).expect("bits");
    let rotation = |agent: &Agent| {
        agent
            .app
            .world()
            .get::<Transform>(entity)
            .expect("transform")
            .rotation
    };
    // An Euler lane (the yaw/pitch/roll drags): degrees, through the Inspector's selection path.
    let out = agent.call(
        "editor.set_property",
        json!({ "entities": [bits], "component": "Transform", "path": "rotation.ry", "value": 90.0 }),
    );
    assert!(
        same_rotation(rotation(&agent), Quat::from_rotation_y(90f32.to_radians())),
        "{out}"
    );
    // The whole quaternion (the axis-angle and raw displays write the field).
    let want = Quat::from_rotation_x(30f32.to_radians());
    set_field(
        &mut agent,
        bits,
        "Transform",
        "rotation",
        json!([want.x, want.y, want.z, want.w]),
    );
    assert!(same_rotation(rotation(&agent), want), "{:?}", rotation(&agent));
    let _ = std::fs::remove_dir_all(&project);
}

/// `color_edit_button` (the whole colour) and the channel `DragValue`s.
#[test]
fn inspector_color_fields_are_set_by_set_property() {
    let (mut agent, bits, project) = gadget_agent();
    set_field(&mut agent, bits, "Gadget", "base_color", json!([0.25, 0.5, 0.75]));
    assert_eq!(gadget(&agent, bits).base_color, [0.25, 0.5, 0.75]);
    set_field(&mut agent, bits, "Gadget", "base_color[0]", json!(1.0));
    assert_eq!(gadget(&agent, bits).base_color, [1.0, 0.5, 0.75]);
    let _ = std::fs::remove_dir_all(&project);
}

// --- Closed value-editor gaps ------------------------------------------------------------------

/// Material panel: the emissive colour button, the emissive brightness boost and the alpha-mode
/// combo (`material_editor.rs`) are set by `editor.edit_material { emissive, alpha_mode }`.
#[test]
fn edit_material_sets_the_emissive_color_and_the_alpha_mode() {
    let project = regia_test_utils::unique_temp_dir("parity-edit-material");
    std::fs::create_dir_all(project.join("assets/materials")).expect("assets dir");
    let file = project.join("assets/materials/lamp.material.ron");
    let original = regia_asset::MaterialAssetRon::default_standard()
        .to_ron_string()
        .expect("ron");
    std::fs::write(&file, &original).expect("material");
    let mut agent = Agent::new(&project);
    let path = "assets/materials/lamp.material.ron";
    for params in [
        json!({ "path": path, "emissive": [1.0, 65.0, 0.0] }),
        json!({ "path": path, "emissive": [1.0, -0.5, 0.0] }),
        json!({ "path": path, "emissive": [1.0, 1.0, 1.0, 1.0] }),
        json!({ "path": path, "alpha_mode": "cutout" }),
        json!({ "path": path, "alpha_mode": 2 }),
    ] {
        assert!(
            agent.try_call("editor.edit_material", params.clone()).is_err(),
            "{params}"
        );
    }
    assert_eq!(std::fs::read_to_string(&file).expect("read"), original);
    let out = agent.call(
        "editor.edit_material",
        json!({ "path": path, "emissive": [4.0, 2.0, 0.5], "alpha_mode": "mask" }),
    );
    assert_eq!(out["emissive"], json!([4.0, 2.0, 0.5]), "{out}");
    assert_eq!(out["alpha_mode"], json!("mask"), "{out}");
    let saved = regia_asset::MaterialAssetRon::from_ron(&std::fs::read(&file).expect("read")).expect("parse");
    assert_eq!(saved.base.emissive, [4.0, 2.0, 0.5]);
    assert_eq!(saved.render_state.alpha_mode, regia_asset::AlphaMode::Mask);
    // Omitted channels and fields stay.
    let out = agent.call(
        "editor.edit_material",
        json!({ "path": path, "emissive": [8.0], "alpha_mode": "blend" }),
    );
    assert_eq!(out["emissive"], json!([8.0, 2.0, 0.5]), "{out}");
    assert_eq!(out["alpha_mode"], json!("blend"), "{out}");
    let _ = std::fs::remove_dir_all(&project);
}

/// Project Settings → XR: the Dominant hand combo and the Retry slider (`xr_settings.rs`) are set
/// by `editor.xr_settings { set: { dominant_hand, retry_frames } }`.
#[test]
fn xr_settings_sets_the_dominant_hand_and_the_retry_steps() {
    let project = regia_test_utils::unique_temp_dir("parity-xr-settings");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    let before = agent.call("editor.xr_settings", json!({}));
    assert_eq!(before["config"]["dominant_hand"], json!("right"), "{before}");
    assert_eq!(before["config"]["retry_frames"], json!(120), "{before}");
    for set in [
        json!({ "dominant_hand": "middle" }),
        json!({ "dominant_hand": 1 }),
        json!({ "retry_frames": 0 }),
        json!({ "retry_frames": 7201 }),
        json!({ "retry_frames": 1.5 }),
    ] {
        let err = agent
            .try_call("editor.xr_settings", json!({ "set": set }))
            .expect_err("refused");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str),
            Some("xr_invalid_value"),
            "{set}: {err:?}"
        );
    }
    assert!(
        !project.join(regia_xr::XR_CONFIG_PATH).exists(),
        "a refused call writes nothing"
    );
    let out = agent.call(
        "editor.xr_settings",
        json!({ "set": { "dominant_hand": "left", "retry_frames": 240 } }),
    );
    assert_eq!(out["written"], json!(true), "{out}");
    assert_eq!(out["config"]["dominant_hand"], json!("left"), "{out}");
    assert_eq!(out["config"]["retry_frames"], json!(240), "{out}");
    let saved = agent.call("editor.xr_settings", json!({}));
    assert_eq!(
        saved["config"]["dominant_hand"],
        json!("left"),
        "read back from the file: {saved}"
    );
    assert_eq!(saved["config"]["retry_frames"], json!(240), "{saved}");
    let _ = std::fs::remove_dir_all(&project);
}

/// Look-dev panel: the Output dir field (`lookdev_panel.rs`) is set by `editor.lookdev_trace
/// { out }`: a relative path lands under the project and the panel shows it. The trace's own
/// result (traced on a GPU, or `failed` without one) does not matter here; only where it writes.
#[test]
fn lookdev_trace_takes_the_output_directory() {
    let project = regia_test_utils::unique_temp_dir("parity-lookdev-out");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    let err = agent
        .try_call("editor.lookdev_trace", json!({ "scene": "cornell", "out": 3 }))
        .expect_err("not a string");
    assert!(err.message.contains("out"), "{err:?}");
    // Review sprint-w11-rev-e-k #2: `out` follows the output-path rule; an absolute path outside
    // every allowed root and a `..` path are refused and nothing is written there.
    for outside in ["/etc/regia-lookdev-parity-out", "../regia-lookdev-parity-out"] {
        let err = agent
            .try_call(
                "editor.lookdev_trace",
                json!({ "scene": "cornell", "width": 8, "height": 8, "spp": 1, "max_bounces": 1, "wait": true, "out": outside }),
            )
            .expect_err("outside the project");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str),
            Some("path_outside_project"),
            "{outside}: {err:?}"
        );
        let resolved = if outside.starts_with('/') {
            std::path::PathBuf::from(outside)
        } else {
            project.join(outside)
        };
        assert!(!resolved.exists(), "{outside}: nothing written");
    }
    let status = agent.call(
        "editor.lookdev_trace",
        json!({ "scene": "cornell", "width": 8, "height": 8, "spp": 1, "max_bounces": 1, "wait": true, "out": "renders/look1" }),
    );
    let want = project.join("renders/look1");
    assert_eq!(
        status["out_dir"].as_str().map(std::path::PathBuf::from),
        Some(want.clone()),
        "{status}"
    );
    assert_eq!(
        agent
            .app
            .world()
            .resource::<regia_editor::lookdev_panel::LookDevPanel>()
            .out,
        want.display().to_string(),
        "the panel's Output dir shows the agent's directory"
    );
    // Blank keeps the default directory.
    let status = agent.call(
        "editor.lookdev_trace",
        json!({ "scene": "cornell", "width": 8, "height": 8, "spp": 1, "max_bounces": 1, "wait": true, "out": " " }),
    );
    assert!(
        status["out_dir"]
            .as_str()
            .is_some_and(|dir| dir.contains(".regia") && dir.contains("lookdev")),
        "{status}"
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// Project Settings → `[content]`: the Chunk host field (`project_settings.rs`) is set by
/// `editor.project_settings_set { content: { base_url } }`; `""` removes the key; unknown keys of
/// `Regia.toml` stay.
#[test]
fn project_settings_set_writes_the_content_chunk_host() {
    let project = regia_test_utils::unique_temp_dir("parity-content-base-url");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    std::fs::write(
        project.join("Regia.toml"),
        "[project]\nname = \"Chunk Yard\"\nid = \"com.example.chunks\"\nversion = \"1.0.0\"\n\n[mystery]\nkeep = 1\n",
    )
    .expect("manifest");
    let mut agent = Agent::new(&project);
    let got = agent.call("editor.project_settings_get", json!({}));
    assert_eq!(got["content"]["base_url"], json!(""), "{got}");
    for content in [
        json!("https://x"),
        json!({ "base_url": 3 }),
        json!({ "host": "x" }),
    ] {
        let err = agent
            .try_call("editor.project_settings_set", json!({ "content": content }))
            .expect_err("refused");
        assert_eq!(code(&err), "bad_settings", "{content}: {err:?}");
    }
    let set = agent.call(
        "editor.project_settings_set",
        json!({ "content": { "base_url": "https://cdn.example.com/game" } }),
    );
    assert_eq!(set["saved"], json!(true), "{set}");
    assert_eq!(
        set["content"]["base_url"],
        json!("https://cdn.example.com/game"),
        "{set}"
    );
    let text = std::fs::read_to_string(project.join("Regia.toml")).expect("manifest");
    let table: toml::Table = text.parse().expect("toml");
    assert_eq!(
        table["content"]["base_url"].as_str(),
        Some("https://cdn.example.com/game"),
        "{text}"
    );
    assert!(table.contains_key("mystery"), "unknown keys kept: {text}");
    let cleared = agent.call(
        "editor.project_settings_set",
        json!({ "content": { "base_url": "" } }),
    );
    assert_eq!(cleared["content"]["base_url"], json!(""), "{cleared}");
    let text = std::fs::read_to_string(project.join("Regia.toml")).expect("manifest");
    let table: toml::Table = text.parse().expect("toml");
    assert!(
        table.get("content").and_then(|c| c.get("base_url")).is_none(),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// Animation skeleton panel: a socket's Rotation° fields (`anim/skeleton_panel.rs`) are set by
/// `editor.anim_skeleton { op: "add_socket", rotation: [yaw, pitch, roll] }` (degrees, Euler YXZ).
#[test]
fn anim_skeleton_sets_a_socket_rotation() {
    let project = regia_test_utils::unique_temp_dir("parity-socket-rotation");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    for rotation in [json!([90.0, 0.0]), json!([90.0, "x", 0.0]), json!(45.0)] {
        assert!(
            agent
                .try_call(
                    "editor.anim_skeleton",
                    json!({ "op": "add_socket", "name": "weapon", "joint": "hand_r", "rotation": rotation }),
                )
                .is_err(),
            "{rotation}"
        );
    }
    let out = agent.call(
        "editor.anim_skeleton",
        json!({ "op": "add_socket", "name": "weapon", "joint": "hand_r", "offset": [0.0, -0.05, 0.0], "rotation": [90.0, 30.0, 0.0] }),
    );
    let socket = out["sockets"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == "weapon"))
        .cloned()
        .expect("the socket");
    let rotation: Quat = serde_json::from_value(socket["offset"]["rotation"].clone()).expect("a quaternion");
    let want = Quat::from_euler(
        regia_core::math::EulerRot::YXZ,
        90f32.to_radians(),
        30f32.to_radians(),
        0.0,
    );
    assert!(same_rotation(rotation, want), "{socket}");
    // Without rotation the socket keeps the identity (the old behavior).
    let out = agent.call(
        "editor.anim_skeleton",
        json!({ "op": "add_socket", "name": "hip", "joint": "hand_r" }),
    );
    let socket = out["sockets"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == "hip"))
        .cloned()
        .expect("the socket");
    let rotation: Quat = serde_json::from_value(socket["offset"]["rotation"].clone()).expect("a quaternion");
    assert!(same_rotation(rotation, Quat::IDENTITY), "{socket}");
    let _ = std::fs::remove_dir_all(&project);
}

/// Animation graph panel: the preview's speed slider (`anim/graph_panel.rs`) is set by
/// `editor.anim_preview { speed }` (it scales every preview step, an agent's `advance` too).
#[test]
fn anim_preview_sets_the_preview_speed() {
    let project = regia_test_utils::unique_temp_dir("parity-anim-speed");
    std::fs::create_dir_all(project.join("assets")).expect("assets dir");
    let mut agent = Agent::new(&project);
    let before = agent.call("editor.anim_preview", json!({}));
    assert_eq!(before["speed"], json!(1.0), "{before}");
    for speed in [json!(-0.5), json!(2.5), json!("fast")] {
        assert!(
            agent
                .try_call("editor.anim_preview", json!({ "speed": speed }))
                .is_err(),
            "{speed}"
        );
    }
    let out = agent.call("editor.anim_preview", json!({ "speed": 0.5, "advance": 0.1 }));
    assert_eq!(out["speed"], json!(0.5), "{out}");
    let again = agent.call("editor.anim_preview", json!({}));
    assert_eq!(
        again["speed"],
        json!(0.5),
        "the slider keeps the agent's speed: {again}"
    );
    let _ = std::fs::remove_dir_all(&project);
}
