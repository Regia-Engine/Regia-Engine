// SPDX-License-Identifier: MPL-2.0
//! Headless deterministic capture (DO27 gaps): fixed timestep, N-frame step, offscreen
//! screenshot with no window. A mesh spins on the fixed clock, so frame N and frame N+10
//! differ, and two lavapipe runs of the later frame are byte-identical.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::enable_fixed_step;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex,
    MeshRenderer, PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(90);

/// Per-call protocol timeout sent with every request (`params.timeout_ms`).
///
/// The server's default is 30 s (`regia_agent::server::DEFAULT_TIMEOUT`): under the full
/// workspace gate (four lavapipe tests in parallel plus external load) the app thread can be
/// starved past it while still inside this test's own 90 s budget, and a deferred `play.step`
/// reply would come back as F0006 — the one observed full-gate flake (2026-10-06 00:15; the
/// test passed alone). Aligning every call with the test budget keeps the protocol from giving
/// up first; the content under test (fixed-step determinism) is unaffected.
const CALL_TIMEOUT_MS: u64 = 60_000;

/// Yaw added each fixed step. 2 steps and 12 steps land on different silhouettes.
const SPIN_STEP: f32 = 0.35;

#[derive(Resource)]
struct Spin(f32);

#[derive(Component)]
struct Spinner;

fn call(client: &mut Client, method: &str, params: Value) -> Value {
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

fn spin_mesh(
    mut spin: ResMut<Spin>,
    mut query: Query<(&mut Transform, &mut GlobalTransform), With<Spinner>>,
) {
    spin.0 += SPIN_STEP;
    let rotation = regia_core::math::Quat::from_rotation_y(spin.0);
    for (mut transform, mut global) in &mut query {
        transform.rotation = rotation;
        global.0 = transform.compute_affine();
    }
}

fn triangle() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-0.95, -0.75, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.95, -0.75, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.0, 0.95, 0.0], [0.0, 0.0, 1.0], [0.5, 1.0]),
        ],
        indices: vec![0, 1, 2],
    })
}

fn deterministic_settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    settings.profile.depth_prepass = false;
    settings
}

/// One headless run. Returns PNG bytes after 2 fixed steps and after 12.
fn capture_once(run_id: u32) -> (Vec<u8>, Vec<u8>) {
    let project = std::env::temp_dir().join(format!("regia-agent-headless-{run_id}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(&project).expect("project dir");

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    });
    enable_fixed_step(&mut app, 60.0);
    // Stay paused until `play.step`, so the spin does not depend on how long the
    // client takes to connect.
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(Spin(0.0));
    app.insert_resource(deterministic_settings());
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
    app.add_systems(FixedUpdate, spin_mesh);

    let camera = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 2.6));
    app.world_mut().spawn((
        camera,
        GlobalTransform(camera.compute_affine()),
        Camera {
            exposure_ev: Some(14.0),
            fov_y: 0.9,
            clear: ClearMode::Color([0.05, 0.07, 0.12, 1.0]),
            ..Camera::default()
        },
    ));
    let mesh_tf = Transform::default();
    app.world_mut().spawn((
        mesh_tf,
        GlobalTransform(mesh_tf.compute_affine()),
        Spinner,
        MeshRenderer::new(
            triangle(),
            Arc::new(CpuMaterial {
                base_color: [0.85, 0.42, 0.12, 1.0],
                roughness: 0.55,
                metallic: 0.0,
                ..CpuMaterial::default()
            }),
        ),
        DirectionalLight {
            direction: [0.15, -0.45, -1.0],
            color: [1.0, 0.96, 0.9],
            illuminance: 90_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));

    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let png_bytes = Arc::new(std::sync::Mutex::new((Vec::new(), Vec::new())));
    let png_slot = png_bytes.clone();

    let script = std::thread::Builder::new()
        .name(format!("agent-headless-{run_id}"))
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

            let _ = call(
                &mut client,
                "play.pause",
                json!({ "timeout_ms": CALL_TIMEOUT_MS }),
            );
            let stepped = call(
                &mut client,
                "play.step",
                json!({ "frames": 2, "timeout_ms": CALL_TIMEOUT_MS }),
            );
            assert!(stepped["frame"].as_u64().is_some(), "stepped: {stepped}");
            let early = screenshot(&mut client);

            let stepped = call(
                &mut client,
                "play.step",
                json!({ "frames": 10, "timeout_ms": CALL_TIMEOUT_MS }),
            );
            assert!(stepped["frame"].as_u64().is_some(), "stepped: {stepped}");
            let late = screenshot(&mut client);

            assert_ne!(early, late, "frame N and frame N+10 must differ (the mesh spins)");
            assert_frame_not_blank(&early, "frame N");
            assert_frame_not_blank(&late, "frame N+10");
            *png_slot.lock().expect("png lock") = (early, late);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    // The script's window starts only once the app (and its GPU device) exists: a slow
    // `finish_plugins` must not eat the deadline. A panicked script drops `script_ok` and the
    // loop below notices via `is_finished`, instead of spinning blind for the full budget and
    // reporting a generic timeout over the real panic.
    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !script_ok.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    assert!(
        script_ok.load(Ordering::Relaxed),
        "headless capture script did not finish within {} s",
        SCRIPT_TIMEOUT.as_secs()
    );
    let bytes = png_bytes.lock().expect("png lock").clone();
    assert!(!bytes.0.is_empty() && !bytes.1.is_empty(), "no png captured");
    let _ = std::fs::remove_dir_all(&project);
    bytes
}

fn screenshot(client: &mut Client) -> Vec<u8> {
    let shot = call(
        client,
        "seeing.screenshot",
        json!({
            "target": "offscreen",
            "width": 96,
            "height": 96,
            "timeout_ms": CALL_TIMEOUT_MS,
        }),
    );
    let b64 = shot["png_base64"].as_str().expect("png_base64");
    let bytes = base64_decode(b64).expect("decode png");
    assert!(bytes.len() > 32, "png too small");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "PNG magic");
    bytes
}

fn assert_frame_not_blank(png_bytes: &[u8], label: &str) {
    let pixels = decode_rgba(png_bytes);
    let n = pixels.len() / 4;
    assert!(n > 0, "{label}: empty image");
    let mut sum = 0.0;
    let mut sum2 = 0.0;
    for i in 0..n {
        let y = 0.2126 * pixels[i * 4] as f64
            + 0.7152 * pixels[i * 4 + 1] as f64
            + 0.0722 * pixels[i * 4 + 2] as f64;
        sum += y;
        sum2 += y * y;
    }
    let mean = sum / n as f64;
    let variance = (sum2 / n as f64) - mean * mean;
    assert!(
        (4.0..240.0).contains(&mean),
        "{label}: mean luma {mean:.1} is blank or blown out"
    );
    assert!(
        variance > 25.0,
        "{label}: variance {variance:.1} — frame is a uniform tint"
    );
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
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
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

#[test]
fn two_headless_runs_match_and_the_clock_moves_the_mesh() {
    let (early_a, late_a) = capture_once(1);
    let (early_b, late_b) = capture_once(2);
    assert_ne!(early_a, late_a, "frames N and N+10 differ inside a run");
    assert_eq!(
        early_a,
        early_b,
        "two lavapipe runs of frame N must be byte-identical ({} vs {})",
        early_a.len(),
        early_b.len()
    );
    assert_eq!(
        late_a,
        late_b,
        "two lavapipe runs of frame N+10 must be byte-identical ({} vs {})",
        late_a.len(),
        late_b.len()
    );
}
