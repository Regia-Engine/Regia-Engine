// SPDX-License-Identifier: MPL-2.0
//! Real NGX evaluation and live counters through the agent dispatcher (ADR-4758).
//! Run explicitly with `REGIA_DLSS_SMOKE=1`, the SDK-linked renderer and a locked NVIDIA card.

use crossbeam_channel::unbounded;
use regia_agent::protocol::Request;
use regia_agent::server::Inbound;
use regia_agent::{AgentInbox, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderProfilePreset, RenderSettings, RuntimeRenderer, UpscaleQuality,
};
use serde_json::{Value, json};

fn call(app: &mut App, method: &str, params: Value) -> Value {
    let (inbox, requests) = unbounded();
    let (reply, responses) = unbounded();
    app.insert_resource(AgentInbox(requests));
    inbox
        .send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: method.into(),
                params,
            },
            reply,
        })
        .expect("queue agent request");
    app.update();
    let response = responses.try_recv().expect("synchronous agent response");
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.expect("successful response")
}

#[test]
#[ignore = "requires the real NGX SDK and an exclusive NVIDIA Vulkan card session"]
fn agent_reports_real_dlss_evaluations_for_every_mode() {
    assert_eq!(std::env::var("REGIA_DLSS_SMOKE").as_deref(), Ok("1"));
    let project = regia_test_utils::unique_temp_dir("dlss-agent-proof");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins(AgentPlugin);
    app.finish_plugins();
    app.insert_resource(RenderDevice::headless().expect("NVIDIA Vulkan device"));
    app.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
    app.insert_resource(RuntimeRenderer(FrameRenderer::new()));
    let before = call(&mut app, "render.get_upscalers", json!({}));
    let dlss = before["upscalers"]
        .as_array()
        .expect("upscaler list")
        .iter()
        .find(|row| row["backend"] == "dlss")
        .expect("DLSS row");
    assert_eq!(dlss["runtime"]["binding_built"], true, "{before}");
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: 14.0,
        bloom: false,
        ..PostSettings::golden_deterministic()
    };
    let scene = LabScene {
        width: 960,
        height: 540,
        eye: [0.0, 0.0, 3.0],
        target: [0.0; 3],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        exposure_ev: 14.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.2, -0.5, -0.8], [1.0; 3], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial::default()],
        meshes: vec![LabMesh {
            vertices: [[-1.0, -0.8, 0.0], [1.0, -0.8, 0.0], [0.0, 0.8, 0.0]]
                .map(|p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0; 2]))
                .to_vec(),
            indices: vec![0, 1, 2],
            material: 0,
            ..LabMesh::unskinned()
        }],
    };
    let render = |app: &mut App, quality| {
        let mut frame = scene.as_frame_scene_with_quality(quality);
        frame.post = Some(&post);
        app.world_mut()
            .resource_scope(|world, mut renderer: Mut<RuntimeRenderer>| {
                let image = renderer
                    .0
                    .render_offscreen(world.resource::<RenderDevice>(), &frame)
                    .expect("real DLSS frame");
                assert_eq!(image.pixels.len(), (960 * 540 * 4) as usize);
            });
    };
    // Discovery starts at `found`; an actual frame initializes NGX before the ordinary
    // availability-gated setter can select it. Use the documented diagnostic setter once.
    call(&mut app, "render.set_vendor_upscaler", json!({"vendor": "dlss"}));
    let mut initial = FrameQuality::from_profile(&app.world().resource::<RenderSettings>().profile);
    initial.taau = true;
    render(&mut app, initial);
    let mut evidence = Vec::new();
    for mode in UpscaleQuality::ALL {
        call(
            &mut app,
            "render.set_upscaler",
            json!({"backend": "dlss", "quality": mode.name()}),
        );
        let mut quality = FrameQuality::from_profile(&app.world().resource::<RenderSettings>().profile);
        quality.taau = true;
        quality.render_scale_permille = u16::try_from(mode.permille()).expect("quality scale fits u16");
        quality.upscale_quality = Some(mode);
        let mut previous = 0;
        for _ in 0..3 {
            render(&mut app, quality);
            let status = call(&mut app, "render.get_upscalers", json!({}));
            let report = &status["vendor_frame"];
            assert_eq!(report["selected"], "dlss", "{status}");
            assert_eq!(report["active"], true, "{status}");
            assert_eq!(report["fallbacks"], 0, "{status}");
            assert_eq!(report["runtime"], "ready", "{status}");
            let count = report["frames"].as_u64().expect("evaluation counter");
            assert!(
                count > previous,
                "counter must increase after each rendered frame"
            );
            previous = count;
            evidence.push(status);
        }
    }
    if let Some(directory) = std::env::var_os("REGIA_DLSS_PROOF_DIR") {
        let directory = std::path::PathBuf::from(directory);
        assert!(directory.is_absolute());
        std::fs::create_dir_all(&directory).expect("evidence directory");
        regia_test_utils::write_atomic(
            &directory.join("agent-status.json"),
            &serde_json::to_vec_pretty(&evidence).expect("status JSON"),
        )
        .expect("publish agent evidence");
    }
    drop(app);
    std::fs::remove_dir_all(project).expect("remove agent project");
}
