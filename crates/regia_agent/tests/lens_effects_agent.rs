// SPDX-License-Identifier: MPL-2.0
//! Lens effects through the agent protocol (ADR-1863, `book/agents/lens-effects.md`): a headless
//! app with the runtime renderer, a bright lamp in a dark frame. `render.set_lens_effects` turns
//! the flare, dirt and chromatic aberration on, the next offscreen screenshot differs and is not
//! darker, `render.lens_effects` reports the pass recording and the effective strengths, and a
//! preset without the pass turns them off. Lavapipe is the reference; no golden image.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, Environment, LabVertex, MeshRenderer,
    PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// The script's budget (a timeout for a stuck app, not a speed check).
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);
/// Per-call protocol timeout (`params.timeout_ms`), as in `headless_deterministic_capture.rs`.
const CALL_TIMEOUT_MS: u64 = 60_000;

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

/// A square of half-size `h` centred at `(x, y)` in the plane z = 0, facing +z.
fn square(x: f32, y: f32, h: f32) -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x - h, y - h, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([x + h, y - h, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x + h, y + h, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x - h, y + h, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
    })
}

fn settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    // A dark frame: no physical sky (its daylight would blow the frame out at EV 0), clouds or fog.
    settings.profile.physical_atmosphere = false;
    settings.profile.volumetric_clouds = false;
    settings.profile.volumetric_fog = false;
    settings
}

fn screenshot(client: &mut Client) -> Vec<u8> {
    let shot = call(
        client,
        "seeing.screenshot",
        json!({ "target": "offscreen", "width": 128, "height": 72 }),
    );
    let b64 = shot["png_base64"].as_str().expect("png_base64");
    decode_rgba(&base64_decode(b64).expect("decode png"))
}

fn mean_luma(rgba: &[u8]) -> f64 {
    let n = rgba.len() / 4;
    let sum: f64 = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .sum();
    sum / n.max(1) as f64
}

#[test]
fn set_lens_effects_changes_the_frame_and_reports_the_pass() {
    let project = regia_test_utils::unique_temp_dir("agent-lens-effects");

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
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 0.0,
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
    let lamp = Transform::default();
    app.world_mut().spawn((
        lamp,
        GlobalTransform(lamp.compute_affine()),
        MeshRenderer::new(
            square(0.9, 0.5, 0.06),
            Arc::new(CpuMaterial {
                base_color: [0.0, 0.0, 0.0, 1.0],
                emissive: [1.0, 0.9, 0.75],
                emissive_intensity: 3000.0,
                ..CpuMaterial::default()
            }),
        ),
    ));

    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("agent-lens-effects".into())
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

            let before = call(&mut client, "render.lens_effects", json!({}));
            assert_eq!(before["pass"]["registered"], true, "{before}");
            assert_eq!(before["effective"]["active"], false, "{before}");
            assert_eq!(before["preset_allows"], true, "{before}");
            let recorded_before = before["counters"]["views_recorded"].as_u64().unwrap_or(0);
            let plain = screenshot(&mut client);

            let set = call(
                &mut client,
                "render.set_lens_effects",
                json!({
                    "flare_intensity": 400.0,
                    "flare_threshold": 0.5,
                    "dirt_intensity": 1.0,
                    "chromatic_aberration": 0.01,
                }),
            );
            assert_eq!(set["created"], true, "{set}");
            let _ = call(&mut client, "play.step", json!({ "frames": 1 }));
            let lensed = screenshot(&mut client);
            let after = call(&mut client, "render.lens_effects", json!({}));
            assert_eq!(after["effective"]["flare"], true, "{after}");
            assert_eq!(after["effective"]["dirt"], true, "{after}");
            assert_eq!(after["effective"]["aberration"], true, "{after}");
            assert_eq!(after["pass"]["failures"], 0, "{after}");
            assert!(
                after["counters"]["views_recorded"].as_u64().unwrap_or(0) > recorded_before,
                "the pass recorded no view: {after}"
            );
            assert!(
                after["counters"]["targets_built"].as_u64().unwrap_or(0) >= 1,
                "the recorded view built no targets: {after}"
            );
            assert!(
                after["counters"]["tables_traced"].as_u64().is_some(),
                "no trace counter: {after}"
            );
            assert_eq!(plain.len(), lensed.len());
            assert!(
                mean_luma(&plain) < 128.0,
                "the frame is not dark: {}",
                mean_luma(&plain)
            );
            assert!(
                plain != lensed,
                "the lens effects did not change the frame (luma {} vs {}; {after})",
                mean_luma(&plain),
                mean_luma(&lensed)
            );
            assert!(
                mean_luma(&lensed) >= mean_luma(&plain),
                "flare and dirt only add light: {} < {}",
                mean_luma(&lensed),
                mean_luma(&plain)
            );

            // A preset without the pass: requested stays, effective goes off.
            let _ = call(
                &mut client,
                "render.set_profile",
                json!({ "preset": "MobileLow" }),
            );
            let gated = call(&mut client, "render.lens_effects", json!({}));
            assert_eq!(gated["preset_allows"], false, "{gated}");
            assert_eq!(gated["requested"]["active"], true, "{gated}");
            assert_eq!(gated["effective"]["active"], false, "{gated}");

            let removed = call(&mut client, "render.set_lens_effects", json!({ "remove": true }));
            assert_eq!(removed["removed"], true, "{removed}");
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

fn decode_rgba(bytes: &[u8]) -> Vec<u8> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("png output size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    buf
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}
