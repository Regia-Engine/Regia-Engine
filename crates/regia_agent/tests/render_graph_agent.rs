// SPDX-License-Identifier: MPL-2.0
//! The frame graph through the agent protocol, end to end (ADR-4905, ADR-4945, ADR-5178,
//! `book/agents/render-graph.md`): a headless app with the runtime renderer draws offscreen
//! frames, and `render.graph` reports the graph and its dispatch for them. The two-phase shadow
//! cull is the graph-pass proof: `render.set_shadow_occlusion` adds the optional
//! `shadow_occlusion` node right before `shadows`, the next frame records it through the graph's
//! dispatch, and switching it off takes the node out again. On a device that draws no GPU
//! objects phase 1 cannot cull, so the dispatch lists the node as inactive instead (ADR-5626);
//! the second test runs that path on any adapter by withholding `INDIRECT_FIRST_INSTANCE`. Every
//! frame closes its dispatch with no violation. Lavapipe is the reference; no golden image.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex,
    MeshRenderer, PostSettings, RenderDevice, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// The script's budget (a timeout for a stuck app, not a speed check).
/// The first screenshot builds every pipeline. Measured cold: 20.6 s on the test machine RTX 5080,
/// 7 s alone on an RTX 5090; it went past 60 s beside a pregate's other binaries only while
/// the build server's shared NVIDIA shader cache sat full at its 1 GB default (pregate 6984911dd: `no
/// answer within 65000 ms`; the pregate runs with a 10 GB cache since 11-18). The guards leave
/// over 4x the cold figure and still fail a 2-minute stall (ADR-8598,
/// `a_two_minute_screenshot_stall_still_fails_the_hang_guard`).
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);
/// Per-call protocol timeout (`params.timeout_ms`), as in `headless_deterministic_capture.rs`.
const CALL_TIMEOUT_MS: u64 = 90_000;

fn call(client: &mut Client, method: &str, mut params: Value) -> Value {
    // Only the methods that wait for frames take a timeout; the others refuse unknown fields.
    if method.starts_with("play.") || method == "seeing.screenshot" {
        params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
    }
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn read_token(agent_dir: &std::path::Path) -> String {
    std::fs::read_to_string(agent_dir.join("token"))
        .expect("token file")
        .trim()
        .to_string()
}

/// A square of half-size `h` centred at the origin in the plane z = 0, facing +z.
fn square(h: f32) -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-h, -h, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([h, -h, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([h, h, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-h, h, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
    })
}

fn settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    // Every frame redraws its sun cascades, so the frame that holds `shadow_occlusion` has
    // shadow jobs for phase 1 to cull (a paged cascade of this still scene redraws nothing,
    // and the dispatch lists the node as inactive then, ADR-5626).
    settings.profile.paged_sun_shadows = false;
    settings
}

/// One offscreen frame of the runtime renderer (the frame `render.graph` then describes).
fn frame(client: &mut Client) {
    let shot = call(
        client,
        "seeing.screenshot",
        json!({ "target": "offscreen", "width": 96, "height": 64 }),
    );
    assert!(
        shot["png_base64"].as_str().is_some_and(|s| !s.is_empty()),
        "no image"
    );
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

/// Whether the app's device draws GPU objects, from `render.get_capabilities`: multi-draw
/// indirect with a first instance (`IndirectStrategy::select(..).uses_gpu_objects()`), and the
/// adapter's name. Only a device that draws them can run the two-phase shadow cull, so only there
/// does the dispatch record `shadow_occlusion` (ADR-5626).
fn draws_gpu_objects(client: &mut Client) -> (bool, String) {
    let caps = call(client, "render.get_capabilities", json!({}));
    let feature = |name: &str| {
        caps["features"][name]
            .as_bool()
            .unwrap_or_else(|| panic!("render.get_capabilities has no features.{name}: {caps}"))
    };
    let gpu_objects = feature("multi_draw_indirect") && feature("indirect_first_instance");
    (
        gpu_objects,
        caps["adapter"].as_str().unwrap_or_default().to_string(),
    )
}

/// The device the scenario's app draws with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestDevice {
    /// The one the first offscreen screenshot opens (the adapter's own features). On the lavapipe
    /// reference it draws GPU objects, so the full path runs there.
    Own,
    /// The same adapter withholding `INDIRECT_FIRST_INSTANCE`: a device that draws no GPU
    /// objects on any adapter, so the inactive path runs everywhere.
    WithoutGpuObjects,
}

/// The graph's order and the dispatch's recorded and inactive lists after a frame, checking the
/// dispatch is clean (no skipped, late or repeated node in any frame so far).
fn graph_after_frame(
    client: &mut Client,
    frames_before: u64,
    label: &str,
) -> (Vec<String>, Vec<String>, Vec<String>, u64) {
    let got = call(client, "render.graph", json!({}));
    let dispatch = &got["dispatch"];
    let frames = dispatch["frames"].as_u64().unwrap_or(0);
    assert!(
        frames > frames_before,
        "{label}: no frame closed its dispatch: {dispatch}"
    );
    assert!(dispatch["last_frame"].is_u64(), "{label}: {dispatch}");
    assert_eq!(dispatch["violations"], 0, "{label}: {dispatch}");
    assert!(dispatch["last_violation"].is_null(), "{label}: {dispatch}");
    assert_eq!(
        dispatch["skipped"].as_array().map(Vec::len),
        Some(0),
        "{label}: {dispatch}"
    );
    assert_eq!(
        dispatch["late"].as_array().map(Vec::len),
        Some(0),
        "{label}: {dispatch}"
    );
    assert!(got["graph"]["error"].is_null(), "{label}: {}", got["graph"]);
    // The listing names the node as a step whether or not the graph holds it.
    let step = got["steps"]
        .as_array()
        .and_then(|steps| steps.iter().find(|s| s["name"] == "shadow_occlusion"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(step["module"], "shadow_two_phase", "{label}: {step}");
    (
        names(&got["graph"]["order"]),
        names(&dispatch["recorded"]),
        names(&dispatch["inactive"]),
        frames,
    )
}

#[test]
fn render_graph_reports_the_shadow_occlusion_node_through_the_agent() {
    scenario(TestDevice::Own);
}

/// Review fixrev4-graph-dispatch finding 1: on a device that draws no GPU objects the two-phase
/// cull cannot run, so the dispatch lists `shadow_occlusion` as inactive (ADR-5626). The test
/// checks that outcome instead of failing on it, and runs it on lavapipe too.
#[test]
fn render_graph_lists_shadow_occlusion_inactive_on_a_device_without_gpu_objects() {
    scenario(TestDevice::WithoutGpuObjects);
}

fn scenario(device: TestDevice) {
    let project = regia_test_utils::unique_temp_dir("agent-render-graph");

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    });
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(settings());
    if device == TestDevice::WithoutGpuObjects {
        let withheld = RenderDevice::headless_withholding(wgpu::Features::INDIRECT_FIRST_INSTANCE)
            .expect("headless device (lavapipe)");
        app.insert_resource(withheld);
    }
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 2.0,
        double_sided: true,
    });
    let camera = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 3.0));
    app.world_mut().spawn((
        camera,
        GlobalTransform(camera.compute_affine()),
        Camera {
            exposure_ev: Some(0.0),
            fov_y: 0.9,
            clear: ClearMode::Color([0.01, 0.01, 0.012, 1.0]),
            ..Camera::default()
        },
    ));
    // A shadow-casting sun: the quad casts into its cascades, so a frame has shadow jobs for
    // phase 1 to cull (without one the cull skips `no_jobs` and its node is inactive).
    app.world_mut().spawn(DirectionalLight {
        direction: [0.2, -0.5, -1.0],
        color: [1.0, 1.0, 1.0],
        illuminance: 4.0,
        cast_shadows: true,
        static_shadow: false,
    });
    let quad = Transform::default();
    app.world_mut().spawn((
        quad,
        GlobalTransform(quad.compute_affine()),
        MeshRenderer::new(
            square(0.8),
            Arc::new(CpuMaterial {
                base_color: [0.6, 0.55, 0.5, 1.0],
                ..CpuMaterial::default()
            }),
        ),
    ));

    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("agent-render-graph".into())
        .spawn(move || {
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            let mut client = None;
            while Instant::now() < deadline {
                if let Ok((advertised, _token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &read_token(&agent_dir))
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no endpoint at {}", agent_dir.display());
            };
            let _ = call(&mut client, "play.pause", json!({}));
            let _ = call(&mut client, "play.step", json!({ "frames": 2 }));

            // Off (the default): the graph does not hold the node and no frame records it.
            frame(&mut client);
            // Read after the first frame: the offscreen screenshot made the device.
            let (gpu_objects, adapter) = draws_gpu_objects(&mut client);
            match device {
                // The reference must take the full path; it may not fall back to the inactive one.
                TestDevice::Own => assert!(
                    gpu_objects || !adapter.to_ascii_lowercase().contains("llvmpipe"),
                    "the lavapipe reference draws no GPU objects ({adapter})"
                ),
                TestDevice::WithoutGpuObjects => assert!(
                    !gpu_objects,
                    "withholding INDIRECT_FIRST_INSTANCE left GPU objects on ({adapter})"
                ),
            }
            let (order, recorded, _, frames) = graph_after_frame(&mut client, 0, "off");
            assert!(!order.iter().any(|n| n == "shadow_occlusion"), "{order:?}");
            assert!(!recorded.iter().any(|n| n == "shadow_occlusion"), "{recorded:?}");
            assert_eq!(
                recorded.first().map(String::as_str),
                Some("shadows"),
                "{recorded:?}"
            );
            assert!(recorded.iter().any(|n| n == "forward"), "{recorded:?}");

            // On: the node is in the graph right before `shadows`, and the next frame records it.
            let set = call(
                &mut client,
                "render.set_shadow_occlusion",
                json!({ "on": true, "min_casters": 1 }),
            );
            assert_eq!(set["after"]["enabled"], true, "{set}");
            frame(&mut client);
            let (order, recorded, inactive, frames) = graph_after_frame(&mut client, frames, "on");
            let at = |list: &[String], name: &str| list.iter().position(|n| n == name);
            assert_eq!(
                at(&order, "shadow_occlusion").map(|i| i + 1),
                at(&order, "shadows"),
                "{order:?}"
            );
            // The dispatch lists the node as recorded only on a frame whose phase 1 can cull
            // (ADR-5626); the cull's own report confirms that phase 1 ran on that frame. A device
            // that draws no GPU objects cannot cull: the node is listed as inactive, the cull
            // says why, and `shadows` still records.
            let occlusion = call(&mut client, "render.get_occlusion", json!({}));
            let shadows = &occlusion["shadows"];
            if gpu_objects {
                assert_eq!(
                    &recorded[..2.min(recorded.len())],
                    ["shadow_occlusion", "shadows"],
                    "{recorded:?}; shadows: {shadows}"
                );
                assert!(!inactive.iter().any(|n| n == "shadow_occlusion"), "{inactive:?}");
                assert_eq!(shadows["active"], true, "phase 1 did not run: {shadows}");
            } else {
                eprintln!(
                    "note: the device draws no GPU objects; checking that shadow_occlusion is \
                     listed as inactive (no_gpu_objects) instead of recorded"
                );
                assert!(!recorded.iter().any(|n| n == "shadow_occlusion"), "{recorded:?}");
                assert_eq!(
                    recorded.first().map(String::as_str),
                    Some("shadows"),
                    "{recorded:?}"
                );
                // Other GPU-object-only nodes (OIT, many-light sampling) are inactive here too.
                assert!(
                    inactive.iter().any(|n| n == "shadow_occlusion"),
                    "{inactive:?}; shadows: {shadows}"
                );
                assert_eq!(shadows["active"], false, "{shadows}");
                assert_eq!(shadows["skipped"], "no_gpu_objects", "{shadows}");
            }

            // Off again: the node leaves the graph.
            let set = call(&mut client, "render.set_shadow_occlusion", json!({ "on": false }));
            assert_eq!(set["after"]["enabled"], false, "{set}");
            frame(&mut client);
            let (order, recorded, _, _) = graph_after_frame(&mut client, frames, "off again");
            assert!(!order.iter().any(|n| n == "shadow_occlusion"), "{order:?}");
            assert!(!recorded.iter().any(|n| n == "shadow_occlusion"), "{recorded:?}");
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !script_ok.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    assert!(
        script_ok.load(Ordering::Relaxed),
        "the agent script did not finish"
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// The hang guard catches a stuck screenshot (sprint-v06-pregate-reds-c review, finding 1): a
/// call that does not answer for 2 minutes is a stuck app, not a cold cache.
#[test]
fn a_two_minute_screenshot_stall_still_fails_the_hang_guard() {
    // Checked at compile time: a wider guard does not build.
    const {
        assert!(
            CALL_TIMEOUT_MS <= 90_000,
            "the call guard passes a 2-minute stall"
        );
        assert!(
            SCRIPT_TIMEOUT.as_secs() <= 120,
            "the script guard passes a 2-minute stall"
        );
        // Room for the measured cold first screenshot (20.6 s) on a loaded host.
        assert!(
            CALL_TIMEOUT_MS >= 4 * 20_600,
            "the call guard fails a cold first screenshot"
        );
    }
}
