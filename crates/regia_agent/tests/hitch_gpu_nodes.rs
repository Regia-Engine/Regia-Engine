// SPDX-License-Identifier: MPL-2.0
//! `play.hitch_report`'s `gpu_passes.nodes` through the agent dispatcher (ADR-5386,
//! `book/agents/hitches.md`): the runtime renderer draws Desktop High offscreen frames while a
//! hitch report runs, and the report names its GPU-timed passes after the frame-graph nodes that
//! `render.graph`'s `dispatch.recorded` lists for the same frame, in that order, with no stamp
//! outside its node.
//!
//! The report runs on wall-clock seconds (`seconds: 0.1`); the frame loop is capped by a frame
//! count, not a time budget. Without `TIMESTAMP_QUERY` the report's `gpu_passes` is `null`: the
//! test says so on stderr and checks that, instead of passing on an empty list. Lavapipe has
//! timestamps. No golden image.

use crossbeam_channel::unbounded;
use regia_agent::protocol::{Request, Response};
use regia_agent::server::Inbound;
use regia_agent::{AgentInbox, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfilePreset, RenderSettings, RuntimeRenderer,
};
use serde_json::{Value, json};

/// Frames the loop draws at most while the report runs (a stuck-report guard, not a speed check).
const MAX_FRAMES: usize = 2_000;

fn request(method: &str, params: Value) -> Request {
    Request {
        jsonrpc: "2.0".into(),
        id: Some(json!(1)),
        method: method.into(),
        params,
    }
}

/// A call the dispatcher answers within one update.
fn call(app: &mut App, method: &str, params: Value) -> Value {
    let (inbox, requests) = unbounded();
    let (reply, responses) = unbounded();
    app.insert_resource(AgentInbox(requests));
    inbox
        .send(Inbound {
            request: request(method, params),
            reply,
        })
        .expect("queue agent request");
    app.update();
    let response = responses.try_recv().expect("synchronous agent response");
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.expect("successful response")
}

fn scene() -> LabScene {
    let vertices = [
        LabVertex::from_pos_normal_uv([-8.0, 0.0, -8.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
        LabVertex::from_pos_normal_uv([8.0, 0.0, -8.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
        LabVertex::from_pos_normal_uv([8.0, 0.0, 8.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
        LabVertex::from_pos_normal_uv([-8.0, 0.0, 8.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
    ];
    LabScene {
        width: 96,
        height: 64,
        eye: [0.0, 1.6, 6.0],
        target: [0.0, 1.2, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: 14.5,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.85, -0.3], [1.0, 0.97, 0.9], 110_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices: vertices.to_vec(),
            indices: vec![0, 2, 1, 0, 3, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn hitch_report_gpu_passes_name_the_recorded_graph_nodes() {
    let project = regia_test_utils::unique_temp_dir("agent-hitch-gpu-nodes");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins(AgentPlugin);
    app.finish_plugins();
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU: {err}"),
    };
    let timestamps = device.capabilities.timestamp_query;
    let adapter = format!(
        "{} ({:?})",
        device.capabilities.adapter_name, device.capabilities.backend
    );
    app.insert_resource(device);
    app.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
    app.insert_resource(RuntimeRenderer(FrameRenderer::new()));

    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.5,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    let lab = scene();
    let quality = FrameQuality::from_profile(&app.world().resource::<RenderSettings>().profile);
    let mut frame = lab.as_frame_scene_with_quality(quality);
    frame.post = Some(&post);
    frame.ibl = true;

    // The report turns GPU timing on while it runs; it answers once its seconds have passed.
    let (inbox, requests) = unbounded();
    let (reply, responses) = unbounded::<Response>();
    app.insert_resource(AgentInbox(requests));
    inbox
        .send(Inbound {
            request: request("play.hitch_report", json!({ "seconds": 0.1, "warmup_s": 0.0 })),
            reply,
        })
        .expect("queue the hitch report");
    let mut report = None;
    for i in 0..MAX_FRAMES {
        // Draw first, then update: the update that answers reads this frame's timings, and
        // `render.graph` below describes the same frame.
        app.world_mut()
            .resource_scope(|world, mut renderer: Mut<RuntimeRenderer>| {
                renderer
                    .0
                    .render_offscreen(world.resource::<RenderDevice>(), &frame)
                    .unwrap_or_else(|err| panic!("frame {i}: {err}"));
            });
        app.update();
        if let Ok(response) = responses.try_recv() {
            report = Some(response);
            break;
        }
    }
    let response = report.unwrap_or_else(|| panic!("no hitch report after {MAX_FRAMES} frames"));
    assert!(response.error.is_none(), "{:?}", response.error);
    let report = response.result.expect("hitch report result");
    let gpu = &report["gpu_passes"];

    if !timestamps {
        eprintln!(
            "SKIPPED, NOT CHECKED: {adapter} has no TIMESTAMP_QUERY; the hitch report has no GPU pass to \
             name after a graph node. Run this test on a timestamp adapter (lavapipe, a desktop card)."
        );
        assert!(
            gpu.is_null(),
            "an adapter without timestamps reported GPU passes: {gpu}"
        );
        let _ = std::fs::remove_dir_all(&project);
        return;
    }

    let graph = call(&mut app, "render.graph", json!({}));
    let dispatch = &graph["dispatch"];
    assert_eq!(dispatch["violations"], 0, "{dispatch}");
    let recorded = names(&dispatch["recorded"]);
    assert!(!recorded.is_empty(), "{dispatch}");
    eprintln!("recorded {recorded:?}");
    eprintln!("gpu_passes {gpu}");
    assert!(
        gpu.is_object(),
        "timing was on, but the report has no GPU passes: {report}"
    );
    let nodes = gpu["nodes"].as_array().cloned().unwrap_or_default();
    assert!(!nodes.is_empty(), "gpu_passes lists no node: {gpu}");
    let mut previous = 0usize;
    for pass in &nodes {
        let node = pass["node"].as_str().unwrap_or_default();
        assert!(pass["pass"].is_string() && pass["ms"].is_number(), "{pass}");
        let at = recorded.iter().position(|n| n == node).unwrap_or_else(|| {
            panic!("gpu_passes names `{node}`, which the frame did not record: {recorded:?}")
        });
        assert!(
            at >= previous,
            "`{node}` is out of graph order: {nodes:?} vs {recorded:?}"
        );
        previous = at;
    }
    for timed in ["shadows", "forward"] {
        assert!(
            nodes.iter().any(|p| p["node"] == timed),
            "gpu_passes does not list `{timed}`: {nodes:?}"
        );
    }
    assert_eq!(
        gpu["stamps_outside_nodes"].as_array().map(Vec::len),
        Some(0),
        "stamps outside their node: {gpu}"
    );
    // The protocol lists what the renderer reports for the same sample.
    let direct: Vec<&str> = app
        .world()
        .resource::<RuntimeRenderer>()
        .0
        .gpu_timings()
        .node_passes()
        .iter()
        .map(|p| p.node)
        .collect();
    let listed: Vec<&str> = nodes.iter().filter_map(|p| p["node"].as_str()).collect();
    assert_eq!(listed, direct);
    let _ = std::fs::remove_dir_all(&project);
}
