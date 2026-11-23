// SPDX-License-Identifier: MPL-2.0
//! Real dispatcher, viewport and read-only capture: graph order, images and freeze/resume.
use crossbeam_channel::Sender;
use regia_agent::protocol::{Request, RpcError};
use regia_agent::server::Inbound;
use regia_app::prelude::*;
use regia_editor::render::{PaneRequest, render_pane, render_pane_gpu};
use regia_editor::viewport::EditorCamera;
use regia_editor::views::ViewKind;
use regia_render::{DebugViewMode, FrameRenderer, RuntimeRenderer};
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

struct Agent {
    app: App,
    inbox: Sender<Inbound>,
    next: u64,
}
impl Agent {
    fn new(root: &std::path::Path) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(regia_scene::ScenePlugin);
        app.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.to_path_buf()),
        });
        app.add_plugins(regia_agent::AgentPlugin);
        regia_editor::agent_ext::install(app.world_mut());
        app.finish_plugins();
        app.update();
        let (inbox, requests) = crossbeam_channel::unbounded();
        app.insert_resource(regia_agent::AgentInbox(requests));
        let mut permissions = regia_agent::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 100_000;
        permissions.config.heavy_rate_limit_per_second = 100_000;
        app.insert_resource(permissions);
        app.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        Self { app, inbox, next: 0 }
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next += 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        self.inbox
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(self.next)),
                    method: method.into(),
                    params,
                },
                reply,
            })
            .expect("inbox");
        self.app.update();
        let answer = answer.try_recv().expect("one deterministic dispatch frame");
        match (answer.result, answer.error) {
            (_, Some(error)) => Err(error),
            (Some(value), None) => Ok(value),
            _ => panic!("empty reply"),
        }
    }
}

#[test]
fn frame_debugger_dispatcher_captures_graph_images_and_freezes_until_resume() {
    let root = regia_test_utils::unique_temp_dir("frame-debugger-agent");
    std::fs::create_dir_all(&root).expect("project");
    let mut agent = Agent::new(&root);
    let device = regia_render::RenderDevice::headless().expect("lavapipe");
    agent.app.insert_resource(device.clone());
    let entity = agent.app.world_mut().spawn(Transform::default()).id();
    let request = PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: EditorCamera::default(),
        width: 96,
        height: 64,
        ids: true,
        warm_frames: 0,
        debug_view: DebugViewMode::BaseColor,
        reference: None,
    };
    let control = render_pane(&device, agent.app.world_mut(), &request).expect("control frame");
    agent
        .call("editor.command", json!({"run":"view.toggle_frame_debugger"}))
        .expect("show panel command");
    assert_eq!(
        agent
            .call("editor.frame_capture", json!({"action":"state"}))
            .expect("open state")["open"],
        true
    );
    let schema = agent
        .call("agent.schema", json!({"method":"editor.frame_capture"}))
        .expect("schema");
    assert!(schema.to_string().contains("frame_capture_action"), "{schema}");
    assert_eq!(
        agent
            .call("editor.frame_capture", json!({"action":"start"}))
            .expect("start")["state"],
        "pending"
    );
    let captured = render_pane(&device, agent.app.world_mut(), &request).expect("next frame");
    assert_eq!(
        captured.image.pixels, control.image.pixels,
        "capture observers changed the image"
    );
    let state = agent
        .call("editor.frame_capture", json!({"action":"state"}))
        .expect("state");
    assert_eq!(state["state"], "frozen");
    let graph = agent.call("render.graph", json!({})).expect("render.graph");
    let names: Vec<_> = state["passes"]
        .as_array()
        .expect("passes")
        .iter()
        .map(|row| row["name"].clone())
        .collect();
    assert_eq!(json!(names), graph["graph"]["order"]);
    assert_eq!(state["recorded"], graph["dispatch"]["recorded"]);
    let expected_png = regia_editor::render::viewport_png_base64(&captured.image).expect("PNG");
    let mut saw_final = false;
    let mut saw_depth = false;
    // Pass-boundary copies (ADR-7860): a plain start copies after every built-in pass.
    assert_eq!(state["pass_copies"], true);
    let mut pass_copies = 0;
    for index in 0..names.len() {
        let pass = agent
            .call("editor.frame_pass", json!({"index":index}))
            .expect("pass");
        for target in pass["targets"].as_array().expect("targets") {
            if target["checkpoint"] == "frame_capture_overlay" && target["resource"] == "output" {
                assert_eq!(target["png_base64"], expected_png);
                saw_final = true;
            }
            if target["resource"] == "depth" && target["png_base64"].is_string() {
                saw_depth = true;
            }
            let checkpoint = target["checkpoint"].as_str().expect("checkpoint");
            if !checkpoint.starts_with("frame_capture_") && target["png_base64"].is_string() {
                assert_eq!(
                    target["after_pass"], pass["name"],
                    "a pass copy sits on its own row"
                );
                assert!(
                    graph["dispatch"]["recorded"]
                        .as_array()
                        .is_some_and(|recorded| recorded.contains(&pass["name"])),
                    "{target}"
                );
                pass_copies += 1;
            }
        }
    }
    assert!(saw_final && saw_depth, "missing color or depth checkpoint");
    assert!(pass_copies > 0, "no pass-boundary copy reached editor.frame_pass");
    let resized_request = PaneRequest {
        width: 128,
        height: 96,
        ..request.clone()
    };
    let resized = render_pane(&device, agent.app.world_mut(), &resized_request).expect("resize frozen");
    assert_eq!(resized.image.size, [128, 96]);
    let ids = resized.ids.expect("scaled picking buffer");
    assert_eq!((ids.width, ids.height), (128, 96));
    assert_eq!(ids.ids.len(), 128 * 96);
    assert_eq!(ids.distance.len(), 128 * 96);
    let before = agent.app.world().resource::<RuntimeRenderer>().0.frame_index();
    agent
        .app
        .world_mut()
        .get_mut::<Transform>(entity)
        .expect("entity")
        .translation
        .x = 20.0;
    agent.app.update();
    let frozen = render_pane(&device, agent.app.world_mut(), &request).expect("frozen frame");
    assert_eq!(frozen.image.pixels, captured.image.pixels);
    assert_eq!(
        agent.app.world().resource::<RuntimeRenderer>().0.frame_index(),
        before,
        "frozen view rendered again"
    );
    let gpu = render_pane_gpu(&device, agent.app.world_mut(), &request).expect("immutable GPU picture");
    let other = PaneRequest {
        pane: 1,
        ..request.clone()
    };
    let changed =
        render_pane(&device, agent.app.world_mut(), &other).expect("another pane reuses renderer targets");
    assert_ne!(
        changed.image.pixels, captured.image.pixels,
        "the other pane must overwrite its target with a different picture"
    );
    let kept = regia_render::read_rgba8(
        &device.device,
        &device.queue,
        &gpu.target.texture,
        gpu.target.width,
        gpu.target.height,
    )
    .expect("frozen target");
    let kept = egui::ColorImage::from_rgba_unmultiplied([96, 64], &kept);
    assert_eq!(
        kept.pixels, captured.image.pixels,
        "another pane overwrote the frozen GPU texture"
    );
    assert_eq!(
        agent
            .call("editor.frame_capture", json!({"action":"resume"}))
            .expect("resume")["state"],
        "idle"
    );
    let live = render_pane(&device, agent.app.world_mut(), &request).expect("live again");
    assert_ne!(live.image.pixels, captured.image.pixels);
    // The actual agent capture route also services a pending frame without a GUI host, here
    // with a pass list and a budget.
    let second = agent
        .call(
            "editor.frame_capture",
            json!({"action":"start", "passes":["post_stack"], "budget_bytes":1_048_576}),
        )
        .expect("second capture");
    assert_eq!(second["request"]["passes"], json!(["post_stack"]));
    agent
        .call(
            "editor.capture_viewport",
            json!({"width":96, "height":64, "debug_view":"base_color"}),
        )
        .expect("agent viewport screenshot");
    assert_eq!(
        agent
            .call("editor.frame_capture", json!({"action":"state"}))
            .expect("second state")["completed"],
        2
    );
    agent
        .call("editor.frame_capture", json!({"action":"resume"}))
        .expect("final resume");
    drop(agent);
    std::fs::remove_dir_all(root).expect("cleanup");
}

/// A flat quad of `half` metres on the ground plane, centred at the origin.
fn quad(half: f32) -> regia_render::CpuMesh {
    let v = |x: f32, z: f32| {
        regia_render::LabVertex::from_pos_normal_uv([x, 0.0, z], [0.0, 1.0, 0.0], [0.0, 0.0])
    };
    regia_render::CpuMesh {
        vertices: vec![v(-half, -half), v(half, -half), v(half, half), v(-half, half)],
        indices: vec![0, 2, 1, 0, 3, 2],
    }
}

/// P-310 acceptance (ADR-8224), lavapipe: a scene of three mesh entities captured through the
/// dispatcher lists three opaque draws in the forward pass naming exactly those three entities
/// (other opaque records, such as the editor's backdrop, name none of them), with their index
/// counts; `editor.frame_draw` selects a draw and its entity becomes the editor selection. Frames
/// without a capture, before and after a start that was resumed before any frame, push no draw
/// record and make the same counted device calls and allocations as each other.
#[test]
fn frame_debugger_three_meshes_list_three_opaque_draws_and_select_their_entities() {
    use std::sync::Arc;
    let root = regia_test_utils::unique_temp_dir("frame-debugger-draws");
    std::fs::create_dir_all(&root).expect("project");
    let mut agent = Agent::new(&root);
    let device = regia_render::RenderDevice::headless().expect("lavapipe");
    agent.app.insert_resource(device.clone());
    let material = Arc::new(regia_render::LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        double_sided: true,
        ..regia_render::LabMaterial::default()
    });
    let entities: Vec<_> = [-1.7_f32, 0.0, 1.7]
        .into_iter()
        .enumerate()
        .map(|(i, x)| {
            agent
                .app
                .world_mut()
                .spawn((
                    Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, 0.0)),
                    regia_render::MeshRenderer::new(
                        Arc::new(quad(0.5 + 0.1 * i as f32)),
                        Arc::clone(&material),
                    ),
                ))
                .id()
        })
        .collect();
    agent.app.update();
    let request = PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: EditorCamera {
            target: regia_core::math::Vec3::ZERO,
            distance: 6.0,
            yaw: 0.0,
            pitch: 1.2,
            ..EditorCamera::default()
        },
        width: 96,
        height: 64,
        ids: false,
        warm_frames: 0,
        debug_view: DebugViewMode::Lit,
        reference: None,
    };
    for _ in 0..3 {
        render_pane(&device, agent.app.world_mut(), &request).expect("warm frame");
    }
    // The off path: an unarmed frame, a start resumed before any frame, another unarmed frame.
    let log_before = regia_render::draw_log_counters();
    let before = regia_render::frame_path_counters();
    render_pane(&device, agent.app.world_mut(), &request).expect("unarmed frame");
    let unarmed = regia_render::frame_path_counters().since(before);
    agent
        .call("editor.frame_capture", json!({"action":"start"}))
        .expect("arm");
    agent
        .call("editor.frame_capture", json!({"action":"resume"}))
        .expect("disarm");
    let before = regia_render::frame_path_counters();
    render_pane(&device, agent.app.world_mut(), &request).expect("disarmed frame");
    let disarmed = regia_render::frame_path_counters().since(before);
    assert_eq!(
        (
            disarmed.buffers,
            disarmed.textures,
            disarmed.bind_groups,
            disarmed.pipelines,
            disarmed.map_asyncs
        ),
        (
            unarmed.buffers,
            unarmed.textures,
            unarmed.bind_groups,
            unarmed.pipelines,
            unarmed.map_asyncs
        ),
        "arming and disarming changed an unarmed frame's allocations"
    );
    assert_eq!(
        regia_render::draw_log_counters().since(log_before),
        regia_render::DrawLogCounters::default(),
        "an unarmed frame recorded draws"
    );
    // The capture.
    agent
        .call("editor.frame_capture", json!({"action":"start"}))
        .expect("start");
    render_pane(&device, agent.app.world_mut(), &request).expect("captured frame");
    let state = agent
        .call("editor.frame_capture", json!({"action":"state"}))
        .expect("state");
    assert_eq!(state["state"], "frozen", "{}", state["error"]);
    let draws = state["draws"].as_array().expect("draws");
    let ours: Vec<&Value> = draws
        .iter()
        .filter(|d| d["counted"] == "opaque")
        .filter(|d| {
            d["entities"]
                .as_array()
                .is_some_and(|e| e.iter().any(|bits| entities.iter().any(|x| *bits == x.to_bits())))
        })
        .collect();
    assert_eq!(ours.len(), 3, "{draws:#?}");
    let mut named: Vec<u64> = ours
        .iter()
        .map(|d| {
            assert_eq!(d["pass"], "forward");
            assert_eq!(d["entities"].as_array().map(Vec::len), Some(1), "{d}");
            if d["indirect"] == false {
                assert_eq!(d["index_count"], 6, "{d}");
            }
            d["entity"].as_u64().expect("entity")
        })
        .collect();
    named.sort_unstable();
    let mut expected: Vec<u64> = entities.iter().map(|e| e.to_bits()).collect();
    expected.sort_unstable();
    assert_eq!(named, expected, "each entity's draw is listed once");
    let forward = state["passes"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == "forward"))
        .expect("forward row");
    assert_eq!(forward["draw_details"], "recorded");
    let opaque_count = draws.iter().filter(|d| d["counted"] == "opaque").count();
    assert_eq!(
        forward["draw_count"].as_u64(),
        Some((opaque_count + draws.iter().filter(|d| d["counted"] == "translucent").count()) as u64),
        "one record per counted forward draw"
    );
    // Selecting the middle entity's draw selects that entity.
    let middle = ours
        .iter()
        .find(|d| d["entity"] == entities[1].to_bits())
        .expect("middle draw");
    let picked = agent
        .call("editor.frame_draw", json!({"index": middle["index"]}))
        .expect("frame_draw");
    assert_eq!(picked["selected_entity"], entities[1].to_bits());
    let selection = agent.call("editor.get_selection", json!({})).expect("selection");
    assert!(
        selection.to_string().contains(&entities[1].to_bits().to_string()),
        "{selection}"
    );
    agent
        .call("editor.frame_capture", json!({"action":"resume"}))
        .expect("resume");
    drop(agent);
    std::fs::remove_dir_all(root).expect("cleanup");
}

/// P-310 item 3 (ADR-8224), lavapipe: with an in-process play window open, `editor.frame_capture
/// { action: "start", source: "play_window" }` captures the play window's next render of the
/// play world (through the same `render_play_window_gpu` the windowed editor calls), and the
/// capture lists the play world's three mesh draws; Resume releases the play window.
#[test]
fn frame_debugger_captures_the_in_process_play_window() {
    use std::sync::Arc;
    let root = regia_test_utils::unique_temp_dir("frame-debugger-play-window");
    std::fs::create_dir_all(&root).expect("project");
    let mut agent = Agent::new(&root);
    let device = regia_render::RenderDevice::headless().expect("lavapipe");
    let mut play = App::new();
    play.add_plugins(MinimalPlugins);
    play.add_plugins(regia_scene::ScenePlugin);
    let material = Arc::new(regia_render::LabMaterial::default());
    for x in [-1.7_f32, 0.0, 1.7] {
        play.world_mut().spawn((
            Transform::from_translation(regia_core::math::Vec3::new(x, 0.0, 0.0)),
            regia_render::MeshRenderer::new(Arc::new(quad(0.5)), Arc::clone(&material)),
        ));
    }
    play.world_mut().insert_resource(EditorCamera {
        target: regia_core::math::Vec3::ZERO,
        distance: 6.0,
        yaw: 0.0,
        pitch: 1.2,
        ..EditorCamera::default()
    });
    play.update();
    let refused = agent
        .call(
            "editor.frame_capture",
            json!({"action":"start", "source":"play_window"}),
        )
        .expect_err("no play window yet");
    assert_eq!(
        refused.data.as_ref().map(|d| d["code"].clone()),
        Some(json!("capture_no_play_window"))
    );
    agent
        .app
        .world_mut()
        .insert_resource(regia_editor::play_window::PlayWindowStatus {
            open: true,
            ..Default::default()
        });
    let state = agent
        .call(
            "editor.frame_capture",
            json!({"action":"start", "source":"play_window"}),
        )
        .expect("start");
    assert_eq!(state["state"], "pending");
    regia_editor::frame_debugger::sync_play_window(agent.app.world_mut(), play.world_mut());
    regia_editor::render::render_play_window_gpu(&device, play.world_mut(), [96, 64], false)
        .expect("play window frame");
    regia_editor::frame_debugger::sync_play_window(agent.app.world_mut(), play.world_mut());
    let state = agent
        .call("editor.frame_capture", json!({"action":"state"}))
        .expect("state");
    assert_eq!(state["state"], "frozen", "{}", state["error"]);
    assert_eq!(state["source"], "play_window");
    let opaque_meshes = state["draws"]
        .as_array()
        .expect("draws")
        .iter()
        .filter(|d| d["counted"] == "opaque" && d["index_count"] == 6)
        .count();
    let batched = state["draws"]
        .as_array()
        .expect("draws")
        .iter()
        .filter(|d| d["counted"] == "opaque" && d["indirect"] == true)
        .map(|d| d["objects"].as_array().map_or(0, Vec::len))
        .sum::<usize>();
    assert_eq!(opaque_meshes + batched, 3, "{state:#}");
    agent
        .call("editor.frame_capture", json!({"action":"resume"}))
        .expect("resume");
    regia_editor::frame_debugger::sync_play_window(agent.app.world_mut(), play.world_mut());
    regia_editor::render::render_play_window_gpu(&device, play.world_mut(), [96, 64], false)
        .expect("live play window frame");
    assert_eq!(
        agent
            .call("editor.frame_capture", json!({"action":"state"}))
            .expect("state")["state"],
        "idle"
    );
    drop(agent);
    std::fs::remove_dir_all(root).expect("cleanup");
}
