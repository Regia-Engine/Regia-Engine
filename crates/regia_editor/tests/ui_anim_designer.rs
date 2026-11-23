// SPDX-License-Identifier: MPL-2.0
//! The UI animation editor over the agent protocol (rule 8, ADR-3092): create an animation, save,
//! reload and compare; scrub equals `TweenPlan::sample`; undo restores byte-identical files; a track
//! naming a missing node is reported; a cooked pack carries the library and `ui.play_anim` plays it; a
//! malformed library is refused with the existing error. Every call goes through the real dispatcher;
//! nothing needs a window or a GPU and no golden is involved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings, cooked_cache_dir, current_platform};
use regia_asset_pipeline::{CookOptions, cook_project, pack_cooked_directory};
use regia_ui::anim_pack::load_uianim_file;
use regia_ui::anim_timeline::UiAnimLibrary;
use regia_ui::tween::{TweenProperty, TweenValue, UiTween, UiTweenPlugin};
use regia_ui::{Opacity, UiTransform};
use serde_json::{Value, json};

struct Client {
    app: App,
    inbox: Sender<Inbound>,
    next_id: u64,
}

impl Client {
    fn new(project: &Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        app.init_resource::<regia_editor::command::UndoStack>()
            .init_resource::<regia_editor::selection::Selection>()
            .init_resource::<regia_editor::viewport::EditorCamera>()
            .init_resource::<regia_editor::viewport::GizmoState>()
            .init_resource::<regia_editor::viewport::ViewportSettings>()
            .init_resource::<regia_editor::pie::PieRequests>()
            .init_resource::<regia_editor::pie::PieStatus>()
            .insert_resource(regia_editor::agent_ext::EditorProject {
                root: Some(project.to_path_buf()),
            });
        let open = regia_editor::level_io::new_level(app.world_mut());
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

    fn code(&mut self, method: &str, params: Value) -> String {
        let err = self.try_call(method, params).expect_err("refused");
        err.data
            .as_ref()
            .and_then(|data| data["code"].as_str().or_else(|| data["reason"].as_str()))
            .unwrap_or_default()
            .to_owned()
    }
}

fn project(tag: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets/ui")).expect("assets");
    dir
}

const UI: &str = r#"(
    version: 2,
    root: (
        children: [
            (name: "title", text: (spans: ["Hello"])),
            (name: "card", style: (size: (100, 40))),
        ],
    ),
)"#;

/// Creates "title::pop" with two tracks through the agent.
fn author(c: &mut Client, path: &str) {
    c.call("editor.ui_anim_open", json!({ "new": true, "path": path }));
    c.call(
        "editor.ui_anim_edit",
        json!({ "op": "add_animation", "name": "title::pop" }),
    );
    c.call(
        "editor.ui_anim_set_key",
        json!({ "property": "opacity", "time": 0.0, "value": 0.0 }),
    );
    c.call(
        "editor.ui_anim_set_key",
        json!({ "property": "opacity", "time": 0.5, "value": 1.0, "ease": "quad_out" }),
    );
    c.call(
        "editor.ui_anim_set_key",
        json!({ "property": "scale", "time": 0.0, "value": 0.8 }),
    );
    c.call(
        "editor.ui_anim_set_key",
        json!({ "property": "scale", "time": 0.5, "value": 1.0, "ease": "back_out" }),
    );
    c.call(
        "editor.ui_anim_edit",
        json!({ "op": "set_playback", "mode": "ping_pong", "loops": 2, "speed": 2.0 }),
    );
}

#[test]
fn an_animation_created_through_the_agent_saves_and_reloads_equal() {
    let dir = project("ui-anim-roundtrip");
    let path = "assets/ui/hud.uianim.ron";
    let mut c = Client::new(&dir);
    author(&mut c, path);
    let before = c.call("editor.ui_anim_list", json!({}));
    assert_eq!(before["dirty"], true);
    let saved = c.call("editor.ui_anim_save", json!({}));
    assert_eq!(saved["dirty"], false);
    let text = std::fs::read_to_string(dir.join(path)).expect("saved");

    let mut other = Client::new(&dir);
    let after = other.call("editor.ui_anim_open", json!({ "path": path }));
    assert_eq!(after["animations"], before["animations"]);
    let lib = UiAnimLibrary::from_ron(&text).expect("the saved file loads");
    let def = &lib.animations["title::pop"];
    assert_eq!(def.loops, 2);
    assert_eq!(def.speed, 2.0);
    // No temp file is left next to it.
    let leftovers: Vec<_> = std::fs::read_dir(dir.join("assets/ui"))
        .expect("dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, vec!["hud.uianim.ron".to_owned()]);
}

#[test]
fn scrub_equals_the_tween_plan_sample() {
    let dir = project("ui-anim-scrub");
    let mut c = Client::new(&dir);
    author(&mut c, "assets/ui/hud.uianim.ron");
    let revision = c.call("editor.ui_anim_list", json!({}))["revision"].clone();
    let lib = UiAnimLibrary::from_ron(&{
        c.call("editor.ui_anim_save", json!({}));
        std::fs::read_to_string(dir.join("assets/ui/hud.uianim.ron")).expect("saved")
    })
    .expect("loads");
    let def = &lib.animations["title::pop"];
    for reverse in [false, true] {
        let plan = def.compile(reverse, None).expect("compiles");
        for step in 0..=12 {
            let t = step as f32 * 0.07;
            let got = c.call(
                "editor.ui_anim_scrub",
                json!({ "animation": "title::pop", "time": t, "reverse": reverse }),
            );
            let mut raw = Vec::new();
            plan.sample(t, &mut raw);
            for property in TweenProperty::ALL {
                let want = raw.iter().rev().find(|(p, _)| *p == property).map(|(_, v)| *v);
                let have = got["values"].get(property.name());
                match (want, have) {
                    (None, None) => {}
                    (Some(TweenValue::Number(n)), Some(v)) => {
                        assert_eq!(
                            v.as_f64().map(|x| x as f32),
                            Some(n),
                            "{} at {t}",
                            property.name()
                        );
                    }
                    other => panic!("{} at {t}: {other:?}", property.name()),
                }
            }
        }
    }
    let after = c.call("editor.ui_anim_list", json!({}))["revision"].clone();
    assert_eq!(after, revision, "scrubbing changes nothing");
}

#[test]
fn undo_restores_a_byte_identical_file() {
    let dir = project("ui-anim-undo");
    let path = "assets/ui/hud.uianim.ron";
    let mut c = Client::new(&dir);
    author(&mut c, path);
    c.call("editor.ui_anim_save", json!({}));
    let original = std::fs::read(dir.join(path)).expect("saved");

    // Every kind of edit, each undone one by one.
    let edits = [
        json!({ "op": "add_animation", "name": "card::slide", "property": "translate_x" }),
        json!({ "op": "rename_animation", "animation": "card::slide", "to": "card::move" }),
        json!({ "op": "add_track", "animation": "title::pop", "property": "background" }),
        json!({ "op": "set_playback", "animation": "title::pop", "mode": "once", "speed": 1.0 }),
        json!({ "op": "remove_track", "animation": "title::pop", "property": "scale" }),
        json!({ "op": "remove_animation", "animation": "card::move" }),
    ];
    for edit in &edits {
        c.call("editor.ui_anim_edit", edit.clone());
    }
    c.call(
        "editor.ui_anim_set_key",
        json!({ "animation": "title::pop", "property": "opacity", "time": 0.25, "value": 0.4 }),
    );
    c.call(
        "editor.ui_anim_remove_key",
        json!({ "animation": "title::pop", "property": "opacity", "time": 0.25 }),
    );
    let steps = edits.len() + 2;
    let listed = c.call("editor.ui_anim_list", json!({}));
    assert_eq!(
        listed["undo"],
        json!(6 + steps),
        "six steps of author() and the edits"
    );
    for _ in 0..steps {
        c.call("editor.ui_anim_edit", json!({ "op": "undo" }));
    }
    assert_eq!(c.call("editor.ui_anim_list", json!({}))["redo"], json!(steps));
    c.call("editor.ui_anim_save", json!({}));
    assert_eq!(std::fs::read(dir.join(path)).expect("saved again"), original);
    assert_eq!(c.call("editor.ui_anim_list", json!({}))["dirty"], false);
    // Redo steps forward again, and the file is only touched by a save.
    c.call("editor.ui_anim_edit", json!({ "op": "redo" }));
    assert_eq!(std::fs::read(dir.join(path)).expect("unchanged"), original);
}

#[test]
fn a_track_naming_a_missing_node_is_reported() {
    let dir = project("ui-anim-validate");
    std::fs::write(dir.join("assets/ui/hud.ui.ron"), UI).expect("ui");
    let mut c = Client::new(&dir);
    c.call(
        "editor.ui_anim_open",
        json!({ "new": true, "path": "assets/ui/hud.uianim.ron" }),
    );
    c.call(
        "editor.ui_anim_edit",
        json!({ "op": "add_animation", "name": "ghost::pop" }),
    );
    c.call(
        "editor.ui_anim_edit",
        json!({ "op": "add_animation", "name": "card::tint", "property": "text_color" }),
    );
    c.call(
        "editor.ui_anim_edit",
        json!({ "op": "add_animation", "name": "title::pop" }),
    );
    // Without a designer document there is nothing to check targets against.
    assert_eq!(c.call("editor.ui_anim_validate", json!({}))["ok"], true);
    c.call(
        "editor.ui_designer_open",
        json!({ "path": "assets/ui/hud.ui.ron" }),
    );
    let report = c.call("editor.ui_anim_validate", json!({}));
    assert_eq!(report["ok"], false);
    let codes: Vec<(&str, &str)> = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|f| (f["code"].as_str().unwrap_or(""), f["text"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(codes.len(), 2, "{codes:?}");
    assert_eq!(codes[0].0, "ui_anim_property_unsupported");
    assert!(codes[0].1.contains("card::tint"));
    assert_eq!(codes[1].0, "ui_anim_missing_node");
    assert!(codes[1].1.contains("ghost::pop"));
    // The list carries the same findings.
    let list = c.call("editor.ui_anim_list", json!({}));
    assert_eq!(list["findings"].as_array().map(Vec::len), Some(2));
}

#[test]
fn scrub_previews_the_target_node_on_a_copy() {
    let dir = project("ui-anim-preview");
    std::fs::write(dir.join("assets/ui/hud.ui.ron"), UI).expect("ui");
    let mut c = Client::new(&dir);
    c.call(
        "editor.ui_designer_open",
        json!({ "path": "assets/ui/hud.ui.ron" }),
    );
    let before = c.call("editor.ui_designer_state", json!({}));
    c.call(
        "editor.ui_anim_open",
        json!({ "new": true, "path": "assets/ui/hud.uianim.ron" }),
    );
    c.call(
        "editor.ui_anim_edit",
        json!({ "op": "add_animation", "name": "card::grow", "property": "width" }),
    );
    c.call(
        "editor.ui_anim_set_key",
        json!({ "property": "width", "time": 1.0, "value": 300.0 }),
    );
    let at_start = c.call("editor.ui_anim_scrub", json!({ "time": 0.0, "preview": true }));
    let at_end = c.call("editor.ui_anim_scrub", json!({ "time": 1.0, "preview": true }));
    let width = |v: &Value| v["preview"]["rect"][2].as_f64().expect("rect");
    assert!(width(&at_end) > width(&at_start), "{at_start} {at_end}");
    assert_eq!(
        c.call("editor.ui_designer_state", json!({}))["dirty"],
        before["dirty"]
    );
    assert_eq!(
        c.call("editor.ui_designer_state", json!({}))["revision"],
        before["revision"]
    );
}

#[test]
fn a_malformed_library_is_refused_with_the_existing_error() {
    let dir = project("ui-anim-bad");
    std::fs::write(
        dir.join("assets/ui/bad.uianim.ron"),
        r#"(format_version: 1, animations: {"x": (tracks: [(property: "nope", keys: [(time: 0.0, value: 0.0)])])})"#,
    )
    .expect("write");
    std::fs::write(
        dir.join("assets/ui/new.uianim.ron"),
        "(format_version: 9, animations: {})",
    )
    .expect("write");
    let mut c = Client::new(&dir);
    let err = c
        .try_call(
            "editor.ui_anim_open",
            json!({ "path": "assets/ui/bad.uianim.ron" }),
        )
        .expect_err("refused");
    assert_eq!(
        err.data.as_ref().and_then(|d| d["code"].as_str()),
        Some("ui_anim_parse")
    );
    assert!(err.message.contains("unknown property `nope`"), "{}", err.message);
    let err = c
        .try_call(
            "editor.ui_anim_open",
            json!({ "path": "assets/ui/new.uianim.ron" }),
        )
        .expect_err("refused");
    assert!(
        err.message.contains("newer than the supported"),
        "{}",
        err.message
    );
    assert_eq!(c.code("editor.ui_anim_list", json!({})), "ui_anim_no_document");

    // An edit that would break the format's rules is refused and changes nothing.
    c.call("editor.ui_anim_open", json!({ "new": true }));
    c.call(
        "editor.ui_anim_edit",
        json!({ "op": "add_animation", "name": "a" }),
    );
    let rev = c.call("editor.ui_anim_list", json!({}))["revision"].clone();
    assert_eq!(
        c.code(
            "editor.ui_anim_edit",
            json!({ "op": "set_playback", "speed": 0.0 })
        ),
        "ui_anim_invalid"
    );
    assert_eq!(
        c.code(
            "editor.ui_anim_edit",
            json!({ "op": "remove_track", "property": "opacity" })
        ),
        "ui_anim_invalid"
    );
    assert_eq!(
        c.code(
            "editor.ui_anim_set_key",
            json!({ "property": "opacity", "time": 0.0, "value": [1, 1, 1, 1] })
        ),
        "ui_anim_invalid"
    );
    assert_eq!(c.call("editor.ui_anim_list", json!({}))["revision"], rev);
}

#[test]
fn a_cooked_pack_carries_the_library_and_ui_play_anim_plays_it() {
    let dir = project("ui-anim-pack");
    let path = "assets/ui/hud.uianim.ron";
    let mut c = Client::new(&dir);
    author(&mut c, path);
    c.call("editor.ui_anim_save", json!({}));
    let source =
        UiAnimLibrary::from_ron(&std::fs::read_to_string(dir.join(path)).expect("saved")).expect("loads");

    let report = cook_project(CookOptions::for_host(dir.clone())).expect("cook");
    assert!(
        report
            .errors
            .iter()
            .all(|(_, e)| e.starts_with("unsupported source extension")),
        "{:?}",
        report.errors
    );
    let cache = cooked_cache_dir(&dir, &current_platform());
    let content = dir.join("content");
    pack_cooked_directory(&cache, &content, &BTreeMap::new(), true).expect("pack");

    // The shipped layout: a pak and a root with no source file.
    let mut game = Client::new(&regia_test_utils::unique_temp_dir("ui-anim-pack-game"));
    game.app.world_mut().insert_resource(
        AssetServer::open(AssetServerSettings {
            project_root: regia_test_utils::unique_temp_dir("ui-anim-pack-root"),
            platform: current_platform(),
            content_dir: Some(content),
        })
        .expect("open"),
    );
    let names = load_uianim_file(game.app.world_mut(), "ui/hud.uianim.ron").expect("reads the pak");
    assert_eq!(names, vec!["title::pop".to_owned()]);
    assert_eq!(
        game.app.world().resource::<UiAnimLibrary>().animations,
        source.animations
    );

    let listed = game.call("ui.play_anim", json!({}));
    assert_eq!(listed["animations"][0]["name"], "title::pop");
    let node = game.app.world_mut().spawn(Opacity(0.0)).id();
    let played = game.call(
        "ui.play_anim",
        json!({ "action": "play", "name": "title::pop", "entity": node.to_bits() }),
    );
    let plan = source.animations["title::pop"]
        .compile(false, None)
        .expect("compiles");
    assert_eq!(played["duration"], json!(plan.duration()));

    // A fixed-step run of that tween: the opacity after each 50 ms step is the plan's sample.
    let tween = game.app.world().get::<UiTween>(node).expect("playing").clone();
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(50)));
    app.add_plugins(UiTweenPlugin);
    let e = app
        .world_mut()
        .spawn((tween, Opacity(0.0), UiTransform::default()))
        .id();
    for step in 1..=10 {
        app.update();
        let mut raw = Vec::new();
        plan.sample(step as f32 * 0.05, &mut raw);
        let want = raw
            .iter()
            .rev()
            .find_map(|(p, v)| match (p, v) {
                (TweenProperty::Opacity, TweenValue::Number(n)) => Some(*n),
                _ => None,
            })
            .expect("opacity is sampled");
        let have = app.world().get::<Opacity>(e).expect("opacity").0;
        assert!((have - want).abs() < 1e-4, "step {step}: {have} vs {want}");
    }
}

/// The event track over the agent (`.uianim.ron` v2, ADR-8900): `set_event` adds and replaces,
/// `remove_event` deletes, each one undo step; the saved file is v2 and carries the events; a
/// refused event (past the end, an unknown index, a missing name) is reported with its code; and
/// a v1 file opens, gains an event and saves as v2.
#[test]
fn frame_events_are_authored_through_the_agent_and_save_as_v2() {
    let dir = project("ui-anim-events");
    let path = "assets/ui/hud.uianim.ron";
    let mut c = Client::new(&dir);
    author(&mut c, path);
    let added = c.call(
        "editor.ui_anim_edit",
        json!({ "op": "set_event", "time": 0.25, "event": "sfx", "payload": "pop" }),
    );
    let events = |v: &Value| v["animations"][0]["events"].clone();
    assert_eq!(
        events(&added),
        json!([{ "time": 0.25, "name": "sfx", "payload": "pop" }])
    );
    c.call(
        "editor.ui_anim_edit",
        json!({ "op": "set_event", "time": 0.5, "event": "done" }),
    );
    let replaced = c.call(
        "editor.ui_anim_edit",
        json!({ "op": "set_event", "index": 0, "time": 0.1, "event": "sfx", "payload": "pop2" }),
    );
    assert_eq!(events(&replaced)[0]["payload"], "pop2");
    assert_eq!(events(&replaced)[1]["name"], "done");
    assert_eq!(
        c.code(
            "editor.ui_anim_edit",
            json!({ "op": "set_event", "time": 5.0, "event": "late" })
        ),
        "ui_anim_invalid"
    );
    assert_eq!(
        c.code("editor.ui_anim_edit", json!({ "op": "remove_event", "index": 7 })),
        "ui_anim_unknown_event"
    );
    assert_eq!(
        c.code(
            "editor.ui_anim_edit",
            json!({ "op": "set_event", "time": 0.1, "event": "" })
        ),
        "ui_anim_invalid"
    );
    let removed = c.call("editor.ui_anim_edit", json!({ "op": "remove_event", "index": 1 }));
    assert_eq!(events(&removed).as_array().map(Vec::len), Some(1));
    let undone = c.call("editor.ui_anim_edit", json!({ "op": "undo" }));
    assert_eq!(events(&undone).as_array().map(Vec::len), Some(2), "one undo step");
    c.call("editor.ui_anim_save", json!({}));
    let text = std::fs::read_to_string(dir.join(path)).expect("saved");
    assert!(text.contains("format_version: 2"), "{text}");
    let lib = UiAnimLibrary::from_ron(&text).expect("loads");
    assert_eq!(lib.animations["title::pop"].events.len(), 2);

    // A v1 file (the frozen corpus shape) opens, gains an event and saves as v2.
    let v1 = "assets/ui/old.uianim.ron";
    std::fs::write(
        dir.join(v1),
        "(format_version: 1, animations: { \"pop\": (tracks: [(property: \"opacity\", keys: [(time: 0.0, value: 0.0), (time: 1.0, value: 1.0)])]) })",
    )
    .expect("v1 file");
    c.call("editor.ui_anim_open", json!({ "path": v1 }));
    c.call(
        "editor.ui_anim_edit",
        json!({ "op": "set_event", "animation": "pop", "time": 1.0, "event": "end" }),
    );
    c.call("editor.ui_anim_save", json!({}));
    let text = std::fs::read_to_string(dir.join(v1)).expect("saved");
    assert!(text.contains("format_version: 2"), "{text}");
    assert!(text.contains("\"end\""), "{text}");
}
