// SPDX-License-Identifier: MPL-2.0
//! Calls, by name, the editor extension methods that no other test called (spec 96, ADR-1704:
//! `cargo xtask book-coverage` requires a test call for every shipped extension method).
//!
//! Each call goes through the registered `AgentExtension` handler on an edit world, as dispatch
//! does, and checks the documented result shape or error code. Methods that render (`editor.pick`,
//! `editor.box_select`, `editor.thumbnails`) need a GPU device, so here they are checked up to
//! their parameter validation, which runs before any device is touched.

use regia_agent::extension::AgentExtension;
use regia_agent::protocol::RpcError;
use regia_app::prelude::*;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

/// An edit world with the editor's methods installed (no window, no renderer).
fn edit_world() -> World {
    let mut world = World::new();
    world.init_resource::<regia_editor::anim::AnimEditor>();
    world.init_resource::<regia_editor::command::UndoStack>();
    regia_editor::agent_ext::install(&mut world);
    world
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    let extension = world.resource::<AgentExtension>().clone();
    let (_, handler) = extension
        .method(method)
        .unwrap_or_else(|| panic!("{method} is not registered"));
    handler(world, &params)
}

/// The Regia error code (`F0002`, `theme_panel_no_draft`, ...) of a refused call.
fn code(result: Result<Value, RpcError>) -> String {
    match result {
        Ok(value) => panic!("expected an error, got {value}"),
        Err(error) => error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

#[test]
fn the_xr_page_and_simulator_answer_with_no_project_and_no_session() {
    let mut world = edit_world();
    assert_eq!(code(call(&mut world, "editor.xr_settings", json!({}))), "F0002");
    let answer = call(&mut world, "editor.xr_simulator", json!({ "enabled": true })).expect("toggle");
    assert_eq!(answer["enabled"], json!(true));
    assert_eq!(answer["active"], json!(false));
    assert_eq!(answer["queued"], json!(0));
    let bad = call(&mut world, "editor.xr_simulator", json!({ "left": [1.0] }));
    assert_eq!(code(bad), "F0002");
}

#[test]
fn play_in_editor_requests_are_queued() {
    let mut world = edit_world();
    for (method, params) in [
        ("editor.pause", json!({})),
        ("editor.resume", json!({})),
        ("editor.step", json!({ "frames": 3 })),
    ] {
        let out = call(&mut world, method, params).expect(method);
        assert_eq!(out["queued"], true, "{method}: {out}");
    }
    let queue = &world.resource::<regia_editor::pie::PieRequests>().queue;
    assert_eq!(
        queue,
        &[
            regia_editor::pie::PieRequest::Pause,
            regia_editor::pie::PieRequest::Resume,
            regia_editor::pie::PieRequest::Step(3),
        ]
    );
}

#[test]
fn gizmo_viewport_and_content_view_settings_round_trip() {
    let mut world = edit_world();
    let gizmo = call(
        &mut world,
        "editor.set_gizmo",
        json!({ "mode": "rotate", "snap": true }),
    )
    .expect("gizmo");
    assert_eq!(gizmo["mode"], "rotate");
    assert_eq!(gizmo["snap"], true);
    assert_eq!(
        code(call(&mut world, "editor.set_gizmo", json!({ "mode": "shear" }))),
        "F0002"
    );

    let view = call(
        &mut world,
        "editor.set_viewport",
        json!({ "grid": false, "layout": "quad" }),
    )
    .expect("viewport");
    assert_eq!(view["grid"], false);
    assert_eq!(view["layout"], "quad");
    assert_eq!(
        code(call(
            &mut world,
            "editor.set_viewport",
            json!({ "layout": "triple" })
        )),
        "F0002"
    );

    let content = call(
        &mut world,
        "editor.content_view",
        json!({ "grid": true, "tile": 96 }),
    )
    .expect("content");
    assert_eq!(content["grid"], true);
    assert_eq!(content["tile"], 96.0);
    assert!(call(&mut world, "editor.content_view", json!({ "tile": 10 })).is_err());

    let prefab = call(&mut world, "editor.prefab_mode", json!({})).expect("prefab mode");
    assert!(prefab["editing"].is_null(), "{prefab}");
}

#[test]
fn camera_bookmarks_and_focus() {
    let mut world = edit_world();
    let empty = call(&mut world, "editor.bookmark", json!({})).expect("list");
    assert_eq!(empty["bookmarks"], json!([]));
    let set = call(
        &mut world,
        "editor.bookmark",
        json!({ "action": "set", "slot": 2 }),
    )
    .expect("set");
    assert_eq!(set["slot"], 2);
    let recalled = call(
        &mut world,
        "editor.bookmark",
        json!({ "action": "recall", "slot": 2 }),
    )
    .expect("recall");
    assert_eq!(recalled["camera"], set["camera"]);
    assert_eq!(
        code(call(
            &mut world,
            "editor.bookmark",
            json!({ "action": "recall", "slot": 5 })
        )),
        "F0003"
    );
    let listed = call(&mut world, "editor.bookmark", json!({ "action": "list" })).expect("list");
    assert_eq!(listed["bookmarks"].as_array().map(Vec::len), Some(1));

    assert_eq!(
        code(call(&mut world, "editor.focus", json!({ "entities": [] }))),
        "F0002"
    );
    let entity = world.spawn(Transform::default()).id();
    let focus = call(
        &mut world,
        "editor.focus",
        json!({ "entities": [entity.to_bits()] }),
    )
    .expect("focus");
    for key in ["target", "distance", "yaw", "pitch", "eye"] {
        assert!(!focus[key].is_null(), "{key}: {focus}");
    }
}

#[test]
fn rendering_methods_validate_their_params_first() {
    let mut world = edit_world();
    assert_eq!(code(call(&mut world, "editor.pick", json!({ "y": 4 }))), "F0002");
    assert_eq!(
        code(call(
            &mut world,
            "editor.box_select",
            json!({ "x0": 0, "y0": 0, "x1": 8 })
        )),
        "F0002"
    );
    assert_eq!(
        code(call(&mut world, "editor.thumbnails", json!({ "jobs": [] }))),
        "F0002"
    );
    assert_eq!(
        code(call(&mut world, "editor.thumbnails", json!({ "jobs": [{}] }))),
        "F0002"
    );
}

#[test]
fn asset_loading_methods_name_missing_files() {
    let mut world = edit_world();
    let dir = regia_test_utils::unique_temp_dir("editor-ext-calls");
    let missing = dir.join("missing.glb");
    let missing = missing.to_string_lossy();
    assert_eq!(
        code(call(
            &mut world,
            "editor.spawn_gltf_hierarchy",
            json!({ "path": missing })
        )),
        "F0003"
    );
    assert_eq!(
        code(call(&mut world, "editor.spawn_gltf_hierarchy", json!({}))),
        "F0002"
    );
    let wav = dir.join("missing.wav");
    assert_eq!(
        code(call(
            &mut world,
            "editor.play_sound",
            json!({ "path": wav.to_string_lossy() })
        )),
        "F0003"
    );
    let not_wav = dir.join("not.wav");
    std::fs::write(&not_wav, b"not a wav file").expect("write");
    assert_eq!(
        code(call(
            &mut world,
            "editor.play_sound",
            json!({ "path": not_wav.to_string_lossy() })
        )),
        "F0002"
    );
    // w10-editor-agent-smalls (2): the other formats `regia_audio` decodes preview too. The Ogg Vorbis
    // fixture decodes (so the call is never refused as a bad parameter); a host with no audio output
    // answers an internal error from the playback step instead of `queued`.
    let ogg = dir.join("tone.ogg");
    std::fs::write(
        &ogg,
        include_bytes!("../../regia_audio/tests/fixtures/tone_1k.ogg"),
    )
    .expect("write");
    match call(
        &mut world,
        "editor.play_sound",
        json!({ "path": ogg.to_string_lossy() }),
    ) {
        Ok(answer) => assert_eq!(answer["queued"], json!(true)),
        Err(error) => assert_ne!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("F0002"),
            "an .ogg must decode: {error:?}"
        ),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shot_panel_without_a_sequence() {
    let mut world = edit_world();
    let state = call(&mut world, "editor.shot_panel_state", json!({})).expect("state");
    assert_eq!(state["loaded"], false);
    assert_eq!(state["shots"], json!([]));
    assert_eq!(
        code(call(&mut world, "editor.shot_panel_edit", json!({}))),
        "F0002"
    );
    assert_eq!(
        code(call(
            &mut world,
            "editor.shot_panel_edit",
            json!({ "op": "add", "time": 0.5, "value": 1.0 })
        )),
        "shot_panel_no_sequence"
    );
}

#[test]
fn theme_panel_open_rename_lint_and_save() {
    let mut world = edit_world();
    assert_eq!(
        code(call(
            &mut world,
            "editor.theme_panel_set",
            json!({ "rename": "x" })
        )),
        "theme_panel_no_draft"
    );
    let state = call(&mut world, "editor.theme_panel_state", json!({})).expect("state");
    assert_eq!(state["open"], false);
    let theme = state["themes"]
        .as_array()
        .and_then(|themes| themes.first())
        .and_then(Value::as_str)
        .expect("a built-in theme")
        .to_string();
    let opened = call(&mut world, "editor.theme_panel_open", json!({ "theme": theme })).expect("open");
    assert_eq!(opened["open"], true);
    assert_eq!(
        code(call(&mut world, "editor.theme_panel_open", json!({}))),
        "F0002"
    );
    let renamed = call(
        &mut world,
        "editor.theme_panel_set",
        json!({ "rename": "p6-book-theme" }),
    )
    .expect("rename");
    assert_eq!(renamed["name"], "p6-book-theme");
    let lint = call(&mut world, "editor.theme_panel_lint", json!({})).expect("lint");
    assert_eq!(lint["name"], "p6-book-theme");
    assert!(lint["findings"].is_array(), "{lint}");
    let dir = regia_test_utils::unique_temp_dir("editor-theme-save");
    let target = dir.join("p6-book.theme.ron");
    let saved = call(
        &mut world,
        "editor.theme_panel_save",
        json!({ "path": target.to_string_lossy() }),
    )
    .expect("save");
    assert_eq!(saved["saved"], target.display().to_string());
    assert!(target.is_file(), "the theme file was written");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn anim_graph_edits_disconnect_remove_and_save_edits() {
    let mut world = edit_world();
    call(&mut world, "editor.anim_open", json!({ "create": true })).expect("open");
    let blend =
        call(&mut world, "editor.anim_add_node", json!({ "kind": "blend" })).expect("blend")["node"].clone();
    let clip = call(
        &mut world,
        "editor.anim_add_node",
        json!({ "kind": "clip", "data": { "clip": "Run" } }),
    )
    .expect("clip")["node"]
        .clone();
    call(
        &mut world,
        "editor.anim_connect",
        json!({ "parent": blend, "child": clip, "slot": "a" }),
    )
    .expect("connect");
    let cut = call(
        &mut world,
        "editor.anim_disconnect",
        json!({ "parent": blend, "slot": "a" }),
    )
    .expect("disconnect");
    assert!(!cut["disconnected"].is_null(), "{cut}");
    let removed = call(&mut world, "editor.anim_remove_node", json!({ "node": clip })).expect("remove");
    assert!(!removed["removed"].is_null(), "{removed}");
    assert!(call(&mut world, "editor.anim_remove_node", json!({ "node": 99_999 })).is_err());
    // No preview rig is loaded, so there are no edits to save.
    assert!(call(&mut world, "editor.anim_save_edits", json!({})).is_err());
}

#[test]
fn pie_net_configures_runs_and_reports_clients() {
    let mut world = edit_world();
    assert_eq!(
        code(call(
            &mut world,
            "editor.pie_net_run",
            json!({ "action": "start" })
        )),
        "pie_net_offline"
    );
    assert_eq!(
        code(call(
            &mut world,
            "editor.pie_net_config",
            json!({ "clients": 17 })
        )),
        "pie_net_clients_limit"
    );
    let config = call(
        &mut world,
        "editor.pie_net_config",
        json!({ "mode": "dedicated", "clients": 3, "preset": "good", "seed": 9 }),
    )
    .expect("config");
    assert_eq!(config["mode"], "dedicated");
    call(&mut world, "editor.pie_net_run", json!({ "action": "start" })).expect("start");
    let run = call(
        &mut world,
        "editor.pie_net_run",
        json!({ "action": "step", "frames": 300 }),
    )
    .expect("step");
    assert_eq!(run["in_game"], 3);
    let state = call(&mut world, "editor.pie_net_state", json!({})).expect("state");
    assert_eq!(state["dedicated"], true);
    assert_eq!(state["stats"]["connections"].as_array().map(Vec::len), Some(3));
    assert!(state["traffic"]["total_in"].as_u64().is_some_and(|n| n > 0));
    call(&mut world, "editor.pie_net_run", json!({ "action": "stop" })).expect("stop");
    assert_eq!(
        code(call(
            &mut world,
            "editor.pie_net_run",
            json!({ "action": "step" })
        )),
        "pie_net_not_running"
    );
}

#[test]
fn the_scatter_brush_and_panel_culling_methods_answer() {
    let mut world = edit_world();
    let brush = call(&mut world, "editor.terrain_scatter_brush", json!({})).expect("read");
    assert_eq!(brush["classes"], json!([]));
    assert_eq!(
        code(call(
            &mut world,
            "editor.terrain_scatter_brush",
            json!({ "class": "trees" })
        )),
        "F0003"
    );
    assert_eq!(
        code(call(
            &mut world,
            "editor.terrain_scatter_brush",
            json!({ "density": -1.0 })
        )),
        "F0002"
    );
    let culling = call(
        &mut world,
        "editor.procgen_panel_culling",
        json!({ "enabled": false }),
    )
    .expect("off");
    assert_eq!(culling["enabled"], json!(false));
    assert_eq!(culling["nodes_culled"], json!(0));
    let culling = call(
        &mut world,
        "editor.procgen_panel_culling",
        json!({ "enabled": true }),
    )
    .expect("on");
    assert_eq!(culling["enabled"], json!(true));
    assert_eq!(
        code(call(
            &mut world,
            "editor.procgen_panel_culling",
            json!({ "enabled": 3 })
        )),
        "F0002"
    );
}

/// The viewport tools and menu items of ADR-7204 through the registered handlers: presets ease,
/// pivot and snap settings round-trip, a gizmo drag and a menu item act, the Outliner toggles
/// report, and refusals carry their codes.
#[test]
fn viewport_tools_and_menu_items_dispatch() {
    let mut world = edit_world();
    let preset = call(&mut world, "editor.view_preset", json!({ "preset": "side" })).expect("preset");
    let duration = preset["duration"].as_f64().expect("duration");
    let end = call(&mut world, "editor.camera_ease", json!({ "advance": duration })).expect("ease");
    assert_eq!(end["active"], false, "{end}");
    assert_eq!(end["camera"]["yaw"], preset["to"]["yaw"]);
    assert_eq!(
        code(call(
            &mut world,
            "editor.view_preset",
            json!({ "preset": "under" })
        )),
        "F0002"
    );
    assert_eq!(
        code(call(&mut world, "editor.camera_ease", json!({ "advance": -1.0 }))),
        "F0002"
    );

    let pivot = call(
        &mut world,
        "editor.set_pivot_mode",
        json!({ "mode": "individual" }),
    )
    .expect("pivot");
    assert_eq!(pivot["mode"], "individual");
    assert_eq!(
        call(&mut world, "editor.set_pivot_mode", json!({})).expect("read")["mode"],
        "individual"
    );

    let snap = call(
        &mut world,
        "editor.set_snap",
        json!({ "enabled": true, "distance": 0.25, "angle_deg": 5.0 }),
    )
    .expect("snap");
    assert_eq!(snap["enabled"], true);
    assert_eq!(snap["distance"], 0.25);
    assert_eq!(snap["angle_deg"], 5.0);
    assert_eq!(
        code(call(&mut world, "editor.set_snap", json!({ "scale": 0.0 }))),
        "F0002"
    );

    let entity = world.spawn(Transform::default()).id();
    let dragged = call(
        &mut world,
        "editor.gizmo_drag",
        json!({ "entities": [entity.to_bits()], "translate": [1.0, 2.0, 3.0] }),
    )
    .expect("drag");
    assert_eq!(
        dragged["moved"][0]["translation"],
        json!([1.0, 2.0, 3.0]),
        "{dragged}"
    );
    assert_eq!(
        code(call(
            &mut world,
            "editor.gizmo_drag",
            json!({ "entities": [], "translate": [1.0, 0.0, 0.0] })
        )),
        "F0002"
    );

    let set = call(
        &mut world,
        "editor.outliner_set",
        json!({ "entity": entity.to_bits(), "locked": true, "visible": false }),
    )
    .expect("outliner");
    assert_eq!(set["locked"], true);
    assert_eq!(set["visible"], false);
    assert_eq!(set["prefab"], false);

    let recent = call(&mut world, "editor.menu_invoke", json!({ "path": "File/Recent" })).expect("recent");
    assert_eq!(recent["recent"], json!([]));
    assert_eq!(
        code(call(
            &mut world,
            "editor.menu_invoke",
            json!({ "path": "Edit/Paste" })
        )),
        "F0003"
    );
    assert_eq!(
        code(call(
            &mut world,
            "editor.menu_invoke",
            json!({ "path": "Help/Nothing" })
        )),
        "F0002"
    );
}

/// The Vehicle Tuning panel's methods (ADR-9760) through the registered handlers: open the vehicle
/// lab's car, raise its final drive, refuse a bad value by its validate code, run, save, reload.
#[test]
fn vehicle_tuning_methods_answer_through_the_extension() {
    let dir = regia_test_utils::unique_temp_dir("vehicle-tuning-ext");
    std::fs::create_dir_all(dir.join("assets")).expect("assets");
    std::fs::write(
        dir.join("assets/car.vehicle.ron"),
        include_str!("../../../samples/vehicle_lab/assets/rwd_car.vehicle.ron"),
    )
    .expect("car");
    let mut world = edit_world();
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    let reason = |result: Result<Value, RpcError>| {
        result
            .expect_err("refused")
            .data
            .and_then(|d| d.get("reason").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default()
    };
    let open = call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": "assets/car.vehicle.ron" }),
    )
    .expect("open");
    assert_eq!(open["name"], "vehicle lab rwd");
    let base = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "script": "accel", "seconds": 15.0 }),
    )
    .expect("base run");
    let drive = open["config"]["drivetrain"]["Geared"]["gearbox"]["final_drive"]
        .as_f64()
        .expect("final drive");
    call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "field": "drivetrain.gearbox.final_drive", "value": drive * 1.2 }),
    )
    .expect("raise");
    assert_eq!(
        reason(call(
            &mut world,
            "editor.vehicle_tuning_set",
            json!({ "field": "drivetrain.gearbox.final_drive", "value": -1.0 }),
        )),
        "vehicle_invalid_config"
    );
    let tuned = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "script": "accel", "seconds": 15.0 }),
    )
    .expect("tuned run");
    let time = |run: &Value| run["zero_to_100_s"].as_f64().expect("reaches 100 km/h");
    assert!(time(&tuned) < time(&base), "{tuned} vs {base}");
    let state = call(&mut world, "editor.vehicle_tuning_state", json!({})).expect("state");
    assert_eq!(state["runs"].as_array().map(Vec::len), Some(2));
    assert_eq!(state["undo_depth"], 1);
    call(&mut world, "editor.vehicle_tuning_undo", json!({})).expect("undo");
    call(&mut world, "editor.vehicle_tuning_redo", json!({})).expect("redo");
    assert_eq!(
        reason(call(&mut world, "editor.vehicle_tuning_apply", json!({}))),
        "vehicle_tuning_not_playing"
    );
    let saved = call(&mut world, "editor.vehicle_tuning_save", json!({})).expect("save");
    assert_eq!(saved["written"], "assets/car.vehicle.ron");
    let text = std::fs::read_to_string(dir.join("assets/car.vehicle.ron")).expect("read");
    let reloaded = regia::physics::vehicle_asset::VehicleAsset::from_ron(&text).expect("reload");
    assert_eq!(
        format!("{:016x}", reloaded.config.digest()),
        state["config_digest"].as_str().unwrap_or_default()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `editor.view_state` (ADR-7275) through the registered handler: refused without a project;
/// with one, the tick records the open level's camera and a lock, and writes the file on a level
/// switch.
#[test]
fn view_state_reports_what_the_editor_keeps_per_project() {
    let mut world = edit_world();
    assert_eq!(
        code(call(&mut world, "editor.view_state", json!({}))),
        "view_state_no_project"
    );
    let root = regia_test_utils::unique_temp_dir("view-state-dispatch");
    std::fs::create_dir_all(root.join("assets/maps")).expect("maps");
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    regia_editor::view_state::tick(&mut world);
    let empty = call(&mut world, "editor.view_state", json!({})).expect("state");
    assert_eq!(empty["file"], ".regia/editor_view.ron");
    assert_eq!(empty["exists"], false, "{empty}");
    assert_eq!(empty["level"], Value::Null);

    // A level is open (its path, as a save or open sets it), the camera turned, one entity locked.
    world.insert_resource(regia_editor::level_io::OpenLevel {
        id: regia_framework::level_assets::level_id_of(std::path::Path::new("assets/maps/a.level.ron")),
        path: Some(root.join("assets/maps/a.level.ron")),
    });
    regia_editor::view_state::tick(&mut world);
    world.insert_resource(regia_editor::viewport::EditorCamera {
        yaw: 1.25,
        ..regia_editor::viewport::EditorCamera::default()
    });
    let guid = regia_core::id::Guid(0x1234);
    let locked = world.spawn(regia_scene::components::EntityGuid(guid)).id();
    regia_editor::viewport_ops::set_locked(&mut world, locked, true);
    regia_editor::view_state::tick(&mut world);
    let state = call(&mut world, "editor.view_state", json!({})).expect("state");
    assert_eq!(state["level"], "assets/maps/a.level.ron", "{state}");
    assert_eq!(state["recent"], json!(["assets/maps/a.level.ron"]), "{state}");
    let level = &state["levels"]["assets/maps/a.level.ron"];
    assert_eq!(level["camera"]["yaw"], 1.25, "{state}");
    assert_eq!(level["locked"], json!([guid.to_string()]), "{state}");
    assert_eq!(
        state["dirty"], true,
        "not written yet (the 60-frame rule): {state}"
    );

    // Another level opens: the file is written at once.
    world.insert_resource(regia_editor::level_io::OpenLevel {
        id: regia_framework::level_assets::level_id_of(std::path::Path::new("assets/maps/b.level.ron")),
        path: Some(root.join("assets/maps/b.level.ron")),
    });
    regia_editor::view_state::tick(&mut world);
    let state = call(&mut world, "editor.view_state", json!({})).expect("state");
    assert_eq!(state["exists"], true, "{state}");
    assert_eq!(state["writes"], 2, "on opening a, then on opening b: {state}");
    let text = std::fs::read_to_string(root.join(".regia/editor_view.ron")).expect("written");
    assert!(
        text.contains("assets/maps/a.level.ron") && text.contains(&guid.to_string()),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `editor.panel_timings` (ADR-7306) through the registered handler: the dock's timer records a
/// draw, the reply carries the newest times with the median and max, `last` trims, `panel`
/// filters, `clear` forgets, and a bad `last` is refused with `F0002`.
#[test]
fn panel_timings_report_the_last_draws_per_panel() {
    let mut world = edit_world();
    for _ in 0..3 {
        let _timer = regia_editor::panel_timings::PanelTimer::start("Extension Calls Panel");
    }
    regia_editor::panel_timings::record("Extension Calls Panel", std::time::Duration::from_millis(5));
    let reply = call(
        &mut world,
        "editor.panel_timings",
        json!({ "panel": "Extension Calls Panel", "last": 2 }),
    )
    .expect("timings");
    assert_eq!(reply["keep"], 240, "{reply}");
    // Nothing previews here: the preview thread has no job (ADR-8584).
    assert_eq!(reply["preview_jobs"], 0, "{reply}");
    let panels = reply["panels"].as_array().expect("panels");
    assert_eq!(panels.len(), 1, "{reply}");
    assert_eq!(panels[0]["panel"], "Extension Calls Panel");
    assert_eq!(panels[0]["draws"], 4, "{reply}");
    assert_eq!(panels[0]["last_ms"].as_array().map(Vec::len), Some(2), "{reply}");
    assert!(
        panels[0]["max_ms"].as_f64().is_some_and(|ms| ms >= 5.0),
        "{reply}"
    );
    assert_eq!(
        code(call(&mut world, "editor.panel_timings", json!({ "last": 0 }))),
        "F0002"
    );
    assert_eq!(
        code(call(&mut world, "editor.panel_timings", json!({ "panel": 3 }))),
        "F0002"
    );
    let all = call(&mut world, "editor.panel_timings", json!({ "clear": true })).expect("all");
    assert!(
        all["panels"]
            .as_array()
            .is_some_and(|p| p.iter().any(|p| p["panel"] == "Extension Calls Panel")),
        "{all}"
    );
    let after = call(
        &mut world,
        "editor.panel_timings",
        json!({ "panel": "Extension Calls Panel" }),
    )
    .expect("after clear");
    assert_eq!(after["panels"], json!([]), "{after}");
}

/// `editor.outliner_rows` (ADR-7306): the listed rows in order without a panel draw, closing a row
/// hides its children, `reveal` opens the parents again, windows by `offset`/`limit`, and bad
/// params are refused with `F0002`.
#[test]
fn outliner_rows_list_open_close_and_reveal() {
    let mut world = edit_world();
    let root = world
        .spawn((regia_scene::prelude::Name::new("Root"), Transform::default()))
        .id();
    let child = world
        .spawn((
            regia_scene::prelude::Name::new("Child"),
            Transform::default(),
            ChildOf(root),
        ))
        .id();
    world.spawn((regia_scene::prelude::Name::new("Other"), Transform::default()));
    regia_editor::visibility::mark_internal_entities(&mut world);
    let reply = call(&mut world, "editor.outliner_rows", json!({})).expect("rows");
    assert_eq!(reply["listed"], 3, "{reply}");
    let labels: Vec<&str> = reply["rows"]
        .as_array()
        .map(|rows| rows.iter().filter_map(|r| r["label"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(labels, ["Other", "Root", "Child"], "{reply}");
    assert_eq!(reply["rows"][2]["depth"], 1, "{reply}");

    let closed = call(
        &mut world,
        "editor.outliner_rows",
        json!({ "close": [root.to_bits()] }),
    )
    .expect("close");
    assert_eq!(closed["listed"], 2, "{closed}");
    assert_eq!(closed["rows"][1]["open"], false, "{closed}");
    assert_eq!(
        closed["tree_builds"], reply["tree_builds"],
        "closing keeps the tree: {closed}"
    );

    let revealed = call(
        &mut world,
        "editor.outliner_rows",
        json!({ "reveal": child.to_bits(), "offset": 2, "limit": 1 }),
    )
    .expect("reveal");
    assert_eq!(revealed["revealed"], true, "{revealed}");
    assert_eq!(revealed["reveal_index"], 2, "{revealed}");
    assert_eq!(revealed["rows"].as_array().map(Vec::len), Some(1), "{revealed}");
    assert_eq!(revealed["rows"][0]["entity"], child.to_bits(), "{revealed}");

    assert_eq!(
        code(call(&mut world, "editor.outliner_rows", json!({ "limit": 5000 }))),
        "F0002"
    );
    assert_eq!(
        code(call(
            &mut world,
            "editor.outliner_rows",
            json!({ "open": "Root" })
        )),
        "F0002"
    );
}

/// `editor.a11y_contrast` (ADR-9810): the editor theme's WCAG table, by name or for the active
/// theme, and `unknown_theme` for a name the editor does not ship.
#[test]
fn the_contrast_report_names_its_theme_and_refuses_unknown_ones() {
    let mut world = edit_world();
    let active = call(&mut world, "editor.a11y_contrast", json!({})).expect("active theme");
    assert_eq!(active["theme"], "active");
    let rows = active["rows"].as_array().expect("rows");
    assert!(rows.iter().any(|r| r["pair"] == "text on panel"), "{active}");
    assert!(
        rows.iter()
            .all(|r| r["ratio"].as_f64().is_some_and(|x| (1.0..=21.0).contains(&x)))
    );
    let dark = call(
        &mut world,
        "editor.a11y_contrast",
        json!({ "theme": "regia-dark" }),
    )
    .expect("dark");
    assert_eq!(dark["theme"], "regia-dark");
    assert_eq!(
        code(call(
            &mut world,
            "editor.a11y_contrast",
            json!({ "theme": "neon" })
        )),
        "unknown_theme"
    );
}

/// ADR-7745 through the registered handlers: the trailer and aircraft documents, a bump-strip run
/// that tows the trailer, a flight, and the Fracture test break's debris preview.
#[test]
fn vehicle_and_fracture_tooling_methods_answer_through_the_extension() {
    let dir = regia_test_utils::unique_temp_dir("w11-vft-ext");
    std::fs::create_dir_all(dir.join("assets")).expect("assets");
    std::fs::write(
        dir.join("assets/car.vehicle.ron"),
        include_str!("../../../samples/vehicle_lab/assets/rwd_car.vehicle.ron"),
    )
    .expect("car");
    let mut world = edit_world();
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(dir.clone()),
    });
    call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": "assets/car.vehicle.ron" }),
    )
    .expect("car");
    let trailer = call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": "assets/box.trailer.ron", "create": true }),
    )
    .expect("trailer");
    assert_eq!(trailer["opened"], "trailer");
    call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "trailer", "field": "chassis.chassis_mass", "value": 900.0 }),
    )
    .expect("trailer mass");
    let run = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "script": "accel", "seconds": 4.0, "track": "bump_strip", "tow": true }),
    )
    .expect("run");
    assert_eq!(run["track"], "bump_strip");
    assert_eq!(run["trailer"]["chassis_mass"], 900.0);
    call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": "assets/trainer.aircraft.ron", "create": true }),
    )
    .expect("aircraft");
    let flight = call(&mut world, "editor.vehicle_tuning_fly", json!({ "seconds": 2.0 })).expect("fly");
    assert_eq!(flight["mass"], 1000.0);
    let saved = call(
        &mut world,
        "editor.vehicle_tuning_save",
        json!({ "asset": "trailer" }),
    )
    .expect("save");
    assert_eq!(saved["written"], "assets/box.trailer.ron");

    call(
        &mut world,
        "editor.fracture_panel_set",
        json!({ "box": [1.5, 1.5, 0.25], "grid": [3, 3, 1] }),
    )
    .expect("box");
    call(&mut world, "editor.fracture_panel_run", json!({ "wait": true })).expect("run");
    // Held at its two bottom corners over a floor 1 m below its base: the knocked-out bottom centre
    // brick falls.
    call(
        &mut world,
        "editor.fracture_panel_set",
        json!({ "supports": [0, 2] }),
    )
    .expect("supports");
    call(
        &mut world,
        "editor.fracture_test_hit",
        json!({ "point": [0.0, -1.0, 0.0], "impulse": 9000.0, "radius": 0.5 }),
    )
    .expect("hit");
    let debris = call(
        &mut world,
        "editor.fracture_test_debris",
        json!({ "steps": 60, "floor_y": -2.5 }),
    )
    .expect("debris");
    assert!(debris["moved"].as_u64().is_some_and(|n| n >= 1), "{debris}");
    assert_eq!(debris["above_floor"], true, "{debris}");
    let _ = std::fs::remove_dir_all(&dir);
}
