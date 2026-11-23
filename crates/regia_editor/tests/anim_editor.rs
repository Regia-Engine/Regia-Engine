// SPDX-License-Identifier: MPL-2.0
//! The animation editor end to end (`docs/specs/17-animation.md` § 5):
//!
//! - the `third_person` sample's locomotion graph is built entirely through `editor.anim_*`
//!   calls over the real agent socket, saved, and compared with the sample's graph (the saved
//!   file is also committed as `samples/third_person/assets/locomotion.animgraph.ron`, which
//!   the sample's own test checks against `locomotion_graph()`);
//! - PIE debugging reads a running animator's state through the editor;
//! - every animation panel renders offscreen (PNGs under `REGIA_ANIM_SHOTS`, default a temp
//!   directory, for a person to review).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regia_animation::blendspace::{BlendSample1D, BlendSpace1D};
use regia_animation::graph::{AnimGraph, AnimNode, BlendSpace1DNode, ClipNode, IkTarget, TwoBoneIKNode};
use regia_animation::state_machine::{CmpOp, Condition, StateMachineDef, TransitionCurve, TransitionDef};
use regia_app::prelude::*;
use regia_editor::agent_ext::EditorProject;
use regia_editor::anim::{self, AnimEditor};
use serde_json::{Value, json};

/// Liveness bound for the app coming up. Not a performance budget: only an app that never starts
/// fails on this.
const APP_START_LIMIT: Duration = Duration::from_secs(240);

/// `params.timeout_ms` of every agent call: the same kind of bound. A frame blocks for as long as
/// the kernel takes to page this binary in, which the merge gate saw exceed the server's default
/// 30 s (F0006) on a disk under load.
const CALL_TIMEOUT_MS: u64 = 180_000;

/// The Fox's four leg chains (`samples/third_person/src/anim.rs`).
const CHAINS: [(&str, &str, &str); 4] = [
    ("b_LeftLeg01_015", "b_LeftLeg02_016", "b_LeftFoot01_017"),
    ("b_RightLeg01_019", "b_RightLeg02_020", "b_RightFoot01_021"),
    ("b_LeftUpperArm_09", "b_LeftForeArm_010", "b_LeftHand_011"),
    ("b_RightUpperArm_06", "b_RightForeArm_07", "b_RightHand_08"),
];

/// The sample's locomotion graph root, restated (the editor crate cannot depend on a sample;
/// `samples/third_person/tests/editor_graph.rs` closes the loop against the real function).
fn expected_root() -> AnimNode {
    let clip = |name: &str, speed: f32| {
        AnimNode::Clip(ClipNode {
            clip: name.into(),
            looping: true,
            speed,
            sync_group: Some("gait".into()),
        })
    };
    let float = |op: CmpOp, value: f32| Condition::Float {
        param: "Speed".into(),
        op,
        value,
    };
    let grounded = |value: bool| Condition::Bool {
        param: "Grounded".into(),
        value,
    };
    let t = |from: Option<&str>, to: &str, conditions: Vec<Condition>, duration: f32| TransitionDef {
        from: from.map(str::to_string),
        to: to.into(),
        conditions,
        duration,
        curve: TransitionCurve::Smooth,
        interruptible: true,
        blend: Default::default(),
    };
    let machine = StateMachineDef {
        initial: "Locomote".into(),
        states: vec![
            ("Idle".into(), clip("Survey", 1.0)),
            (
                "Locomote".into(),
                AnimNode::BlendSpace1D(BlendSpace1DNode {
                    space: BlendSpace1D {
                        param: "Speed".into(),
                        samples: vec![
                            BlendSample1D {
                                value: 0.0,
                                clip: "Walk".into(),
                            },
                            BlendSample1D {
                                value: 8.0,
                                clip: "Run".into(),
                            },
                        ],
                    },
                    sync_group: Some("gait".into()),
                }),
            ),
            ("Air".into(), clip("Run", 0.6)),
        ],
        transitions: vec![
            t(Some("Idle"), "Locomote", vec![float(CmpOp::Gt, 0.5)], 0.25),
            t(Some("Locomote"), "Idle", vec![float(CmpOp::Lt, 0.2)], 0.3),
            t(None, "Air", vec![grounded(false)], 0.15),
            t(
                Some("Air"),
                "Locomote",
                vec![grounded(true), float(CmpOp::Gt, 0.5)],
                0.2,
            ),
            t(Some("Air"), "Idle", vec![grounded(true)], 0.2),
        ],
    };
    let mut root = AnimNode::StateMachine(machine);
    for (i, (hip, knee, foot)) in CHAINS.iter().enumerate() {
        root = AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(root),
            chain: [(*hip).into(), (*knee).into(), (*foot).into()],
            target: IkTarget::Param(format!("Foot{i}Target")),
            pole: Some(format!("Foot{i}Pole")),
            weight_param: Some(format!("Foot{i}Weight")),
        });
    }
    root
}

fn edit_app(project_dir: PathBuf) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project_dir.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    app.init_resource::<regia_editor::command::UndoStack>()
        .init_resource::<AnimEditor>()
        .insert_resource(EditorProject {
            root: Some(project_dir),
        });
    regia_editor::agent_ext::install(app.world_mut());
    app.finish_plugins();
    app.update();
    app
}

fn call(client: &mut regia_agent::client::Client, method: &str, mut params: Value) -> Value {
    params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
    match client.call(method, params.clone()) {
        Ok(v) => v,
        Err(err) => panic!("{method} {params}: {err}"),
    }
}

/// The agent script: builds the locomotion graph with `editor.anim_*` calls only.
fn build_locomotion(project: &Path, app_up: &AtomicBool) -> Result<Value, String> {
    // The app coming up is the signal, not a time budget: `edit_app` advertises the endpoint before
    // it returns, and nothing else here can tell a slow start from a broken one.
    let started = Instant::now();
    while !app_up.load(Ordering::SeqCst) {
        if started.elapsed() > APP_START_LIMIT {
            return Err(format!("edit_app still running after {APP_START_LIMIT:?}"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let agent_dir = regia_agent::endpoint::agent_dir(project);
    regia_agent::endpoint::read(&agent_dir)
        .map_err(|err| format!("no agent endpoint in {}: {err}", agent_dir.display()))?;
    let mut c = regia_agent::client::Client::connect_project(project).map_err(|e| e.to_string())?;
    // Past the connection's own deadline (`timeout_ms` plus its grace), so an F0006 is what arrives.
    c.set_read_timeout(Some(
        Duration::from_millis(CALL_TIMEOUT_MS) + Duration::from_secs(30),
    ))
    .map_err(|e| e.to_string())?;
    let c = &mut c;
    call(
        c,
        "editor.anim_open",
        json!({ "create": true, "path": "anim/locomotion.animgraph.ron" }),
    );
    call(
        c,
        "editor.anim_param",
        json!({ "name": "Speed", "kind": "float" }),
    );
    call(
        c,
        "editor.anim_param",
        json!({ "name": "Grounded", "kind": "bool", "default": true }),
    );
    for i in 0..4 {
        call(
            c,
            "editor.anim_param",
            json!({ "name": format!("Foot{i}Target"), "kind": "vec3" }),
        );
        call(
            c,
            "editor.anim_param",
            json!({ "name": format!("Foot{i}Pole"), "kind": "vec3" }),
        );
        call(
            c,
            "editor.anim_param",
            json!({ "name": format!("Foot{i}Weight"), "kind": "float" }),
        );
    }
    let sm = call(
        c,
        "editor.anim_add_node",
        json!({ "kind": "state_machine", "pos": [-900, 0] }),
    )["node"]
        .clone();
    for (name, pos) in [
        ("Idle", [-200, -80]),
        ("Locomote", [120, -80]),
        ("Air", [-40, 120]),
    ] {
        call(
            c,
            "editor.anim_state",
            json!({ "machine": sm, "op": "add", "name": name, "pos": pos }),
        );
    }
    call(
        c,
        "editor.anim_state",
        json!({ "machine": sm, "op": "initial", "name": "Locomote" }),
    );
    let scope = |state: &str| json!({ "machine": sm, "state": state });
    let idle = call(
        c,
        "editor.anim_add_node",
        json!({ "kind": "clip", "scope": scope("Idle"),
                "data": { "clip": "Survey", "looping": true, "speed": 1.0, "sync_group": "gait" } }),
    )["node"]
        .clone();
    call(c, "editor.anim_connect", json!({ "child": idle }));
    let locomote = call(
        c,
        "editor.anim_add_node",
        json!({ "kind": "blend_space_1d", "scope": scope("Locomote"),
                "data": { "space": { "param": "Speed", "samples": [
                    { "value": 0.0, "clip": "Walk" }, { "value": 8.0, "clip": "Run" } ] },
                          "sync_group": "gait" } }),
    )["node"]
        .clone();
    call(c, "editor.anim_connect", json!({ "child": locomote }));
    let air = call(
        c,
        "editor.anim_add_node",
        json!({ "kind": "clip", "scope": scope("Air"),
                "data": { "clip": "Run", "looping": true, "speed": 0.6, "sync_group": "gait" } }),
    )["node"]
        .clone();
    call(c, "editor.anim_connect", json!({ "child": air }));
    let speed = |op: &str, value: f32| json!({ "Float": { "param": "Speed", "op": op, "value": value } });
    let grounded = |value: bool| json!({ "Bool": { "param": "Grounded", "value": value } });
    for (from, to, conditions, duration) in [
        (json!("Idle"), "Locomote", json!([speed("Gt", 0.5)]), 0.25),
        (json!("Locomote"), "Idle", json!([speed("Lt", 0.2)]), 0.3),
        (Value::Null, "Air", json!([grounded(false)]), 0.15),
        (
            json!("Air"),
            "Locomote",
            json!([grounded(true), speed("Gt", 0.5)]),
            0.2,
        ),
        (json!("Air"), "Idle", json!([grounded(true)]), 0.2),
    ] {
        call(
            c,
            "editor.anim_transition",
            json!({ "machine": sm, "from": from, "to": to, "conditions": conditions,
                    "duration": duration, "curve": "Smooth", "interruptible": true }),
        );
    }
    let mut inner = sm;
    for (i, (hip, knee, foot)) in CHAINS.iter().enumerate() {
        let ik = call(
            c,
            "editor.anim_add_node",
            json!({ "kind": "two_bone_ik", "pos": [-600 + 260 * i as i32, 0],
                    "data": { "chain": [hip, knee, foot], "target": { "Param": format!("Foot{i}Target") },
                              "pole": format!("Foot{i}Pole"), "weight_param": format!("Foot{i}Weight") } }),
        )["node"]
            .clone();
        call(
            c,
            "editor.anim_connect",
            json!({ "child": inner, "parent": ik, "slot": "inner" }),
        );
        inner = ik;
    }
    call(c, "editor.anim_connect", json!({ "child": inner }));
    let graph = call(c, "editor.anim_graph", json!({}));
    if graph["valid"] != true {
        return Err(format!("graph invalid: {}", graph["error"]));
    }
    // Undo / redo through the shared stack: the last edit was the root output.
    call(c, "editor.undo", json!({}));
    if call(c, "editor.anim_graph", json!({}))["valid"] != false {
        return Err("undo did not remove the output".into());
    }
    call(c, "editor.redo", json!({}));
    let preview = call(
        c,
        "editor.anim_preview",
        json!({ "params": { "Speed": 4.0 }, "advance": 0.5 }),
    );
    let saved = call(c, "editor.anim_save", json!({}));
    Ok(json!({ "saved": saved, "preview": preview }))
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn third_person_locomotion_graph_built_through_editor_calls() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let project = std::env::temp_dir().join(format!("regia-anim-editor-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(project.join("assets")).expect("project");
    let result: Arc<Mutex<Option<Result<Value, String>>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&result);
    let app_up = Arc::new(AtomicBool::new(false));
    let script_app_up = Arc::clone(&app_up);
    let script_project = project.clone();
    std::thread::spawn(move || {
        let script = || build_locomotion(&script_project, &script_app_up);
        let outcome = std::panic::catch_unwind(script).unwrap_or_else(|p| {
            Err(p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "agent script panicked".into()))
        });
        if let Ok(mut s) = slot.lock() {
            *s = Some(outcome);
        }
    });
    let mut app = edit_app(project.clone());
    app_up.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let outcome = loop {
        if let Some(done) = result.lock().ok().and_then(|mut s| s.take()) {
            break done;
        }
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "agent script timed out"
        );
        app.update();
        std::thread::sleep(Duration::from_millis(1));
    };
    let report = outcome.unwrap_or_else(|e| panic!("{e}"));
    // The demo rig has the sample's clip names but not the Fox's leg joints: the preview runs the
    // state machine and reports the IK chains it cannot solve.
    assert!(
        report["preview"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("not found in skeleton")),
        "{report}"
    );
    assert_eq!(report["preview"]["state"], "Locomote", "{report}");

    let path = project.join("assets/anim/locomotion.animgraph.ron");
    // The saved path is absolute and OS-native; compare it as a path (component by component), so
    // `assets\anim/x` and `assets/anim/x` on Windows are the same file.
    let saved = report["saved"]["path"].as_str().expect("saved path");
    assert_eq!(Path::new(saved), path, "{report}");
    let text = std::fs::read_to_string(&path).expect("saved graph");
    let graph = AnimGraph::from_ron_str(&text).expect("the saved graph loads with the runtime loader");
    assert_eq!(
        graph.root,
        expected_root(),
        "the editor-built graph is the sample's graph"
    );
    assert_eq!(graph.parameters.len(), 14, "Speed, Grounded and 3 per foot");
    assert!(
        regia_editor::anim::layout_path(&path).is_file(),
        "layout sidecar written"
    );

    // The committed copy in the sample is exactly this editor output.
    if regia_core::brand::env_var_os("REGIA_ANIM_BLESS").is_some() {
        let _ = std::fs::write(shot_dir().join("locomotion.animgraph.ron"), &text);
    }
    let committed = include_str!("../../../samples/third_person/assets/locomotion.animgraph.ron");
    assert_eq!(
        text, committed,
        "REGIA_ANIM_BLESS=1 writes the new output to the shot dir"
    );
    let _ = std::fs::remove_dir_all(&project);
}

fn shot_dir() -> PathBuf {
    let dir = regia_core::brand::env_var_os("REGIA_ANIM_SHOTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("regia-anim-shots"));
    std::fs::create_dir_all(&dir).expect("shot dir");
    dir
}

/// An editor world (no agent socket) with the handlers callable directly.
fn handler_world() -> World {
    let mut world = World::new();
    world.init_resource::<AnimEditor>();
    world.init_resource::<regia_editor::command::UndoStack>();
    world
}

fn h(world: &mut World, method: &str, params: Value) -> Value {
    let handler = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .map(|(_, h)| h)
        .unwrap_or_else(|| panic!("no {method}"));
    handler(world, &params).unwrap_or_else(|e| panic!("{method}: {e:?}"))
}

/// A demo-rig graph: Layer(base = machine Idle ↔ Move(blend space), wave layer).
fn build_demo_graph(world: &mut World) -> (u64, u64) {
    h(world, "editor.anim_open", json!({ "create": true }));
    h(world, "editor.anim_rig", json!({ "source": "demo" }));
    h(world, "editor.anim_param", json!({ "name": "Speed" }));
    h(
        world,
        "editor.anim_param",
        json!({ "name": "Wave", "default": 0.0 }),
    );
    h(
        world,
        "editor.anim_param",
        json!({ "name": "Jump", "kind": "trigger" }),
    );
    let sm = h(
        world,
        "editor.anim_add_node",
        json!({ "kind": "state_machine", "pos": [-980, -40] }),
    )["node"]
        .as_u64()
        .expect("id");
    for (name, pos) in [("Idle", [-260, -60]), ("Move", [60, -60]), ("Jump", [-100, 110])] {
        h(
            world,
            "editor.anim_state",
            json!({ "machine": sm, "op": "add", "name": name, "pos": pos }),
        );
    }
    let idle = h(
        world,
        "editor.anim_add_node",
        json!({ "kind": "clip", "scope": { "machine": sm, "state": "Idle" }, "data": { "clip": "Idle" } }),
    )["node"]
        .clone();
    h(world, "editor.anim_connect", json!({ "child": idle }));
    let bs = h(
        world,
        "editor.anim_add_node",
        json!({ "kind": "blend_space_1d", "scope": { "machine": sm, "state": "Move" },
                "data": { "space": { "param": "Speed", "samples": [
                    { "value": 0.0, "clip": "Idle" }, { "value": 1.4, "clip": "Walk" }, { "value": 4.5, "clip": "Run" } ] },
                          "sync_group": "gait" } }),
    )["node"]
        .as_u64()
        .expect("id");
    h(world, "editor.anim_connect", json!({ "child": bs }));
    let jump = h(
        world,
        "editor.anim_add_node",
        json!({ "kind": "clip", "scope": { "machine": sm, "state": "Jump" }, "data": { "clip": "Jump", "looping": false } }),
    )["node"]
        .clone();
    h(world, "editor.anim_connect", json!({ "child": jump }));
    let t = |from: Value, to: &str, cond: Value| json!({ "machine": sm, "from": from, "to": to, "conditions": cond, "duration": 0.2 });
    h(
        world,
        "editor.anim_transition",
        t(
            json!("Idle"),
            "Move",
            json!([{ "Float": { "param": "Speed", "op": "Gt", "value": 0.3 } }]),
        ),
    );
    h(
        world,
        "editor.anim_transition",
        t(
            json!("Move"),
            "Idle",
            json!([{ "Float": { "param": "Speed", "op": "Lt", "value": 0.1 } }]),
        ),
    );
    h(
        world,
        "editor.anim_transition",
        t(Value::Null, "Jump", json!([{ "Trigger": { "param": "Jump" } }])),
    );
    h(
        world,
        "editor.anim_transition",
        t(json!("Jump"), "Idle", json!(["ClipFinished"])),
    );
    let layer = h(
        world,
        "editor.anim_add_node",
        json!({ "kind": "layer", "pos": [-440, 0], "data": { "layers": [
            { "mask": { "joints": ["upperarm_r"], "include_children": true }, "mode": "Override", "weight_param": "Wave" } ] } }),
    )["node"]
        .as_u64()
        .expect("id");
    let wave = h(
        world,
        "editor.anim_add_node",
        json!({ "kind": "clip", "pos": [-980, 140], "data": { "clip": "Wave" } }),
    )["node"]
        .clone();
    h(
        world,
        "editor.anim_connect",
        json!({ "child": sm, "parent": layer, "slot": "base" }),
    );
    h(
        world,
        "editor.anim_connect",
        json!({ "child": wave, "parent": layer, "slot": "layer0" }),
    );
    h(world, "editor.anim_connect", json!({ "child": layer }));
    assert_eq!(h(world, "editor.anim_graph", json!({}))["valid"], true);
    (sm, bs)
}

#[test]
fn transition_blend_mode_is_set_through_the_agent() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut world = handler_world();
    let (sm, _) = build_demo_graph(&mut world);
    let graph = h(&mut world, "editor.anim_graph", json!({}));
    let machine = graph["nodes"]
        .as_array()
        .and_then(|nodes| nodes.iter().find(|n| n["id"].as_u64() == Some(sm)))
        .cloned()
        .expect("machine node");
    assert_eq!(
        machine["transitions"][0]["blend"], "Crossfade",
        "blend always reported"
    );

    let set = h(
        &mut world,
        "editor.anim_transition",
        json!({ "machine": sm, "op": "set", "index": 0, "from": "Idle", "to": "Move",
                "conditions": [{ "Float": { "param": "Speed", "op": "Gt", "value": 0.3 } }],
                "duration": 0.15, "blend": "Inertialize" }),
    );
    assert_eq!(set["transitions"][0]["blend"], "Inertialize");
    assert_eq!(set["transitions"][1]["blend"], "Crossfade");
    let ron = h(&mut world, "editor.anim_graph", json!({ "ron": true }))["ron"]
        .as_str()
        .map(str::to_owned)
        .expect("ron");
    assert_eq!(ron.matches("blend: Inertialize").count(), 1, "{ron}");
    let compiled = world
        .resource::<AnimEditor>()
        .docs
        .graph
        .compile_validated()
        .expect("valid")
        .graph;
    let reparsed = AnimGraph::from_ron_str(&ron).expect("saved graph parses");
    assert_eq!(reparsed, compiled);

    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(s, _)| s.name == "editor.anim_transition")
        .expect("method");
    let bad = json!({ "machine": sm, "op": "set", "index": 0, "to": "Move", "blend": "Teleport" });
    assert!(
        handler(&mut world, &bad).is_err(),
        "unknown blend mode is rejected"
    );
}

#[test]
fn pie_debugging_reads_the_running_animator() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut edit = handler_world();
    build_demo_graph(&mut edit);
    let graph = edit
        .resource::<AnimEditor>()
        .docs
        .graph
        .compile_validated()
        .expect("valid")
        .graph;

    // A play world running the same graph on the demo rig.
    let mut play = App::new();
    play.add_plugins((MinimalPlugins, regia_animation::AnimationPlugin));
    play.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(16),
    ));
    let rig = anim::PreviewRig::demo();
    let binding = regia_animation::components::spawn_skeleton(play.world_mut(), rig.skeleton.clone());
    let mut clips = regia_animation::ClipLibrary::new();
    for c in &rig.clips {
        clips.insert((**c).clone());
    }
    let mut animator = regia_animation::Animator::graph(graph.triangulated(), clips);
    animator.parameters = graph.default_parameters();
    let character = play
        .world_mut()
        .spawn((regia_scene::Transform::default(), binding, animator))
        .id();
    for _ in 0..10 {
        play.update();
    }
    anim::sync_pie_debug(&mut edit, Some(play.world_mut()));
    let debug = h(&mut edit, "editor.anim_debug", json!({}));
    assert_eq!(debug["state"], "Idle", "{debug}");
    assert_eq!(debug["entity"], character.to_bits());
    // Speed up in the game: PIE debugging follows the machine into Move, with blend weights.
    if let Some(mut a) = play.world_mut().get_mut::<regia_animation::Animator>(character) {
        a.set("Speed", 2.0f32);
    }
    for _ in 0..30 {
        play.update();
    }
    anim::sync_pie_debug(&mut edit, Some(play.world_mut()));
    let debug = h(&mut edit, "editor.anim_debug", json!({}));
    assert_eq!(debug["state"], "Move", "{debug}");
    let nodes = debug["nodes"].as_array().expect("nodes");
    let bs = nodes
        .iter()
        .find(|n| n["state"]["kind"] == "blend_space")
        .expect("blend space node");
    assert!(bs["node"].is_u64(), "mapped onto a model node: {bs}");
    assert_eq!(bs["active"], true);
    assert!(
        debug["parameters"]
            .as_array()
            .is_some_and(|p| p.iter().any(|p| p["name"] == "Speed")),
        "{debug}"
    );
    // PIE stopped: the snapshot clears.
    anim::sync_pie_debug(&mut edit, None);
    assert!(h(&mut edit, "editor.anim_debug", json!({})).is_null());
}

fn image_stats(image: &egui::ColorImage) -> (f64, usize) {
    let mut sum = 0.0;
    let mut sq = 0.0;
    let mut colours = std::collections::HashSet::new();
    for p in &image.pixels {
        let l = f64::from(p.r()) * 0.3 + f64::from(p.g()) * 0.59 + f64::from(p.b()) * 0.11;
        sum += l;
        sq += l * l;
        if colours.len() < 4096 {
            colours.insert([p.r() / 8, p.g() / 8, p.b() / 8]);
        }
    }
    let n = image.pixels.len().max(1) as f64;
    ((sq / n - (sum / n).powi(2)).max(0.0).sqrt(), colours.len())
}

fn write_png(image: &egui::ColorImage, path: &Path) {
    let mut raw = Vec::with_capacity(image.pixels.len() * 4);
    for p in &image.pixels {
        raw.extend_from_slice(&[p.r(), p.g(), p.b(), p.a()]);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        image.width() as u32,
        image.height() as u32,
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&raw))
        .expect("write png");
}

#[test]
fn every_animation_panel_renders_offscreen() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match regia_render::RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => {
            eprintln!("skip every_animation_panel_renders_offscreen: {err}");
            return;
        }
    };
    let mut world = handler_world();
    let (sm, bs) = build_demo_graph(&mut world);
    h(
        &mut world,
        "editor.anim_clip",
        json!({ "clip": "Walk", "op": "add_notify", "time": 0.5, "name": "Breath" }),
    );
    h(
        &mut world,
        "editor.anim_clip",
        json!({ "clip": "Walk", "op": "set_curve", "name": "FootPlant_L",
                "keys": [[0.0, 1.0, "cubic", 0.0], [0.25, 0.0, "cubic", 0.0], [0.6, 0.2, "linear"], [1.0, 1.0, "linear"]] }),
    );
    h(
        &mut world,
        "editor.anim_skeleton",
        json!({ "op": "add_socket", "name": "weapon", "joint": "hand_r" }),
    );
    h(
        &mut world,
        "editor.anim_skeleton",
        json!({ "op": "add_ik_chain", "name": "LegL", "joints": ["thigh_l", "shin_l", "foot_l"] }),
    );
    h(
        &mut world,
        "editor.anim_preview",
        json!({ "params": { "Speed": 2.0, "Wave": 1.0 }, "advance": 1.0 }),
    );
    let dir = shot_dir();
    let mut shots = Vec::new();
    // 1. Root blend tree with the layer node selected.
    {
        let mut e = world.resource_mut::<AnimEditor>();
        e.ui.selected_node = e.docs.graph.output;
    }
    shots.push(("anim_graph_root", anim::capture::AnimPanel::Graph));
    // 2. The state machine canvas with a transition selected.
    let mut images = Vec::new();
    for (name, panel) in shots.drain(..) {
        images.push((
            name,
            anim::capture::capture_anim_panel(&device, &mut world, panel, [1280, 720]).expect("capture"),
        ));
    }
    {
        let mut e = world.resource_mut::<AnimEditor>();
        e.ui.machine_view = Some(sm as u32);
        e.ui.selected_node = Some(sm as u32);
        e.ui.selected_transition = Some(0);
    }
    images.push((
        "anim_state_machine",
        anim::capture::capture_anim_panel(&device, &mut world, anim::capture::AnimPanel::Graph, [1280, 720])
            .expect("capture"),
    ));
    // 3. Inside the Move state: the blend-space point editor.
    {
        let mut e = world.resource_mut::<AnimEditor>();
        e.ui.machine_view = None;
        e.ui.scope = Some(anim::Scope::State {
            machine: sm as u32,
            state: "Move".into(),
        });
        e.ui.selected_node = Some(bs as u32);
    }
    images.push((
        "anim_blend_space",
        anim::capture::capture_anim_panel(&device, &mut world, anim::capture::AnimPanel::Graph, [1280, 720])
            .expect("capture"),
    ));
    // 4. The clip viewer on Walk with its curve open.
    {
        let mut e = world.resource_mut::<AnimEditor>();
        e.ui.clip = Some("Walk".into());
        e.ui.scrub = 0.3;
        e.ui.selected_notify = Some(1);
        e.ui.curve = Some((false, "FootPlant_L".into()));
    }
    images.push((
        "anim_clip",
        anim::capture::capture_anim_panel(&device, &mut world, anim::capture::AnimPanel::Clip, [1280, 720])
            .expect("capture"),
    ));
    // 5. The skeleton viewer with the right hand selected.
    {
        let mut e = world.resource_mut::<AnimEditor>();
        e.ui.selected_joint = e.rig.as_ref().and_then(|r| r.skeleton.joint_index("hand_r"));
    }
    images.push((
        "anim_skeleton",
        anim::capture::capture_anim_panel(
            &device,
            &mut world,
            anim::capture::AnimPanel::Skeleton,
            [1280, 720],
        )
        .expect("capture"),
    ));
    for (name, image) in &images {
        assert_eq!((image.width(), image.height()), (1280, 720), "{name}");
        let (spread, colours) = image_stats(image);
        assert!(
            spread > 8.0 && colours > 40,
            "{name} looks blank: σ {spread:.1}, {colours} colors"
        );
        write_png(image, &dir.join(format!("{name}.png")));
    }
    // The agent surface captures the same panel.
    world.insert_resource(device);
    let shot = h(
        &mut world,
        "editor.capture_panel",
        json!({ "panel": "anim_skeleton", "width": 640, "height": 360 }),
    );
    assert_eq!(shot["mime"], "image/png");
    assert!(shot["png_base64"].as_str().is_some_and(|s| s.len() > 1000));
    eprintln!("animation panel shots in {}", dir.display());
}

/// The `code` of a refused call (`book/agents/animation-editor.md` lists them).
fn h_err(world: &mut World, method: &str, params: Value) -> String {
    let handler = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(s, _)| s.name == method)
        .map(|(_, h)| h)
        .unwrap_or_else(|| panic!("no {method}"));
    let err = handler(world, &params).expect_err("the call is refused");
    err.data
        .as_ref()
        .and_then(|d| d["code"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// P-201 and P-206 (spec addendum § 11.1): a live parameter write reaches the running animator
/// without touching the graph document, the recorder keeps a bounded ring of frames that follows
/// the play frame counter, and a scrubbed frame stands in for the live snapshot after play stops.
#[test]
fn live_parameters_and_the_recorder_follow_the_play_animator() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut edit = handler_world();
    build_demo_graph(&mut edit);
    let graph = edit
        .resource::<AnimEditor>()
        .docs
        .graph
        .compile_validated()
        .expect("valid")
        .graph;
    let mut play = App::new();
    play.add_plugins((MinimalPlugins, regia_animation::AnimationPlugin));
    play.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(16),
    ));
    let rig = anim::PreviewRig::demo();
    let binding = regia_animation::components::spawn_skeleton(play.world_mut(), rig.skeleton.clone());
    let mut clips = regia_animation::ClipLibrary::new();
    for c in &rig.clips {
        clips.insert((**c).clone());
    }
    let mut animator = regia_animation::Animator::graph(graph.triangulated(), clips);
    animator.parameters = graph.default_parameters();
    let character = play
        .world_mut()
        .spawn((regia_scene::Transform::default(), binding, animator))
        .id();
    // Before play the write is refused.
    assert_eq!(
        h_err(
            &mut edit,
            "editor.anim_param",
            json!({ "name": "Speed", "live_value": 2.0 })
        ),
        "not_playing"
    );
    for _ in 0..10 {
        play.update();
    }
    anim::sync_pie_debug(&mut edit, Some(play.world_mut()));
    let docs_before = edit.resource::<AnimEditor>().docs.clone();
    let undo_before = edit.resource::<regia_editor::command::UndoStack>().undo_depth();
    assert_eq!(
        h_err(
            &mut edit,
            "editor.anim_param",
            json!({ "name": "Nope", "live_value": 1.0 })
        ),
        "unknown_parameter"
    );
    let queued = h(
        &mut edit,
        "editor.anim_param",
        json!({ "name": "Speed", "live_value": 2.0 }),
    );
    assert_eq!(queued["live"]["queued"], 1, "{queued}");
    anim::sync_pie_debug(&mut edit, Some(play.world_mut()));
    let speed = play
        .world()
        .get::<regia_animation::Animator>(character)
        .map(|a| a.parameters.float("Speed"));
    assert_eq!(speed, Some(2.0), "the write reached the running animator");
    assert_eq!(
        edit.resource::<AnimEditor>().docs,
        docs_before,
        "a live write never edits the graph"
    );
    assert_eq!(
        edit.resource::<regia_editor::command::UndoStack>().undo_depth(),
        undo_before,
        "nor the undo stack"
    );
    for _ in 0..30 {
        play.update();
    }
    anim::sync_pie_debug(&mut edit, Some(play.world_mut()));
    let debug = h(&mut edit, "editor.anim_debug", json!({}));
    assert_eq!(
        debug["state"], "Move",
        "the machine followed the live write: {debug}"
    );

    // Record a bounded ring while play runs.
    let state = h(
        &mut edit,
        "editor.anim_record",
        json!({ "op": "start", "capacity": 8 }),
    );
    assert_eq!(state["recording"], true);
    for _ in 0..20 {
        play.update();
        anim::sync_pie_debug(&mut edit, Some(play.world_mut()));
    }
    let state = h(&mut edit, "editor.anim_record", json!({ "op": "state" }));
    assert_eq!(state["frames"], 8, "{state}");
    assert_eq!(state["dropped"], 12, "{state}");
    assert!(state["bytes"].as_u64().is_some_and(|b| b > 0), "{state}");
    // Paused play (no new frame) records nothing.
    anim::sync_pie_debug(&mut edit, Some(play.world_mut()));
    assert_eq!(
        h(&mut edit, "editor.anim_record", json!({ "op": "state" }))["frames"],
        8
    );
    // Play stops: recording stops, the live writes are forgotten, the frames stay.
    anim::sync_pie_debug(&mut edit, None);
    let state = h(&mut edit, "editor.anim_record", json!({ "op": "state" }));
    assert_eq!(state["recording"], false);
    assert_eq!(state["frames"], 8);
    assert!(edit.resource::<AnimEditor>().live.written.is_empty());
    assert!(h(&mut edit, "editor.anim_debug", json!({})).is_null());
    // Scrub: the recorded frame is what the graph panel and editor.anim_debug show.
    let frame = h(&mut edit, "editor.anim_scrub", json!({ "frame": 3 }));
    assert_eq!(frame["state"], "Move", "{frame}");
    assert_eq!(frame["pose_hash"].as_str().map(str::len), Some(16), "{frame}");
    assert!(
        frame["parameters"]
            .as_array()
            .is_some_and(|p| p.iter().any(|p| p["name"] == "Speed")),
        "{frame}"
    );
    assert!(!frame["weights"].as_array().is_some_and(Vec::is_empty), "{frame}");
    anim::sync_pie_debug(&mut edit, None);
    let debug = h(&mut edit, "editor.anim_debug", json!({}));
    assert_eq!(debug["state"], "Move", "replayed after stop: {debug}");
    assert_eq!(
        h_err(&mut edit, "editor.anim_scrub", json!({ "frame": 99 })),
        "frame_out_of_range"
    );
    assert_eq!(
        h_err(
            &mut edit,
            "editor.anim_param",
            json!({ "name": "Speed", "live_value": 1.0 })
        ),
        "not_playing",
        "a replay is not a live session"
    );
    assert!(h(&mut edit, "editor.anim_scrub", json!({ "frame": null }))["frame"].is_null());
    anim::sync_pie_debug(&mut edit, None);
    assert!(h(&mut edit, "editor.anim_debug", json!({})).is_null());
    h(&mut edit, "editor.anim_record", json!({ "op": "start" }));
    assert_eq!(
        h_err(&mut edit, "editor.anim_scrub", json!({ "frame": 0 })),
        "nothing_recorded"
    );
}

/// P-207, P-211, P-213, P-215, P-216 (spec addendum §§ 11.2-11.3) through `editor.anim_clip`:
/// speed, ping-pong over a trimmed range, ghost times, the root's travel, and the clip list.
#[test]
fn clip_preview_options_and_the_clip_list_through_the_dispatcher() {
    let mut edit = handler_world();
    h(&mut edit, "editor.anim_rig", json!({ "source": "demo" }));
    let undo_before = edit.resource::<regia_editor::command::UndoStack>().undo_depth();
    let out = h(
        &mut edit,
        "editor.anim_clip",
        json!({
            "op": "preview", "clip": "Walk", "speed": 2.0, "loop": "ping_pong", "range": [0.1, 0.5],
            "onion": { "enabled": true, "before": 2, "after": 2, "step": 0.1 }, "time": 0.4, "advance": 0.1,
        }),
    );
    assert_eq!(out["loop"], "ping_pong", "{out}");
    assert_eq!(out["reverse"], true, "bounced off the trimmed end: {out}");
    assert!((out["time"].as_f64().unwrap_or(-1.0) - 0.4).abs() < 1e-4, "{out}");
    let range: Vec<f64> = out["range"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    assert!(
        range.len() == 2 && (range[0] - 0.1).abs() < 1e-4 && (range[1] - 0.5).abs() < 1e-4,
        "{out}"
    );
    assert_eq!(out["trimmed"], true);
    assert!(
        out["onion_times"]
            .as_array()
            .is_some_and(|t| !t.is_empty() && t.len() <= 4),
        "{out}"
    );
    let travel = h(
        &mut edit,
        "editor.anim_clip",
        json!({ "op": "preview", "clip": "Walk", "range": null, "loop": "loop", "speed": 1.0, "time": 0.9 }),
    );
    let z = travel["root"]["position"][2].as_f64().unwrap_or(0.0);
    assert!(z.abs() > 0.1, "the walk travels: {travel}");
    assert_eq!(
        h_err(
            &mut edit,
            "editor.anim_clip",
            json!({ "op": "preview", "clip": "Walk", "range": [0.5, 0.2] })
        ),
        "bad_range"
    );
    assert_eq!(
        edit.resource::<regia_editor::command::UndoStack>().undo_depth(),
        undo_before,
        "preview options are view state"
    );
    let listed = h(
        &mut edit,
        "editor.anim_clip",
        json!({ "op": "list", "sort": "length" }),
    );
    let durations: Vec<f64> = listed["clips"]
        .as_array()
        .expect("clips")
        .iter()
        .filter_map(|c| c["duration"].as_f64())
        .collect();
    assert!(durations.len() >= 2, "{listed}");
    assert!(durations.windows(2).all(|w| w[0] >= w[1]), "{listed}");
    let filtered = h(
        &mut edit,
        "editor.anim_clip",
        json!({ "op": "list", "filter": "walk", "sort": "name" }),
    );
    assert!(
        filtered["clips"].as_array().is_some_and(|c| !c.is_empty()
            && c.iter().all(|c| c["name"]
                .as_str()
                .is_some_and(|n| n.to_lowercase().contains("walk")))),
        "{filtered}"
    );
}
