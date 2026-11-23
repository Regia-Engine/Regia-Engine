// SPDX-License-Identifier: MPL-2.0
//! Spec 23 § 3.4 on the runtime `seeing.screenshot` (ADR-7217), headless on lavapipe through the
//! WebSocket dispatcher:
//! - `target: "camera"` of a named camera renders that camera's view, which differs from the
//!   main (primary window) view;
//! - `target: "ui"` renders the UI layer alone: the UI's pixels, transparent black elsewhere (no
//!   scene under it);
//! - `overlays: ["colliders"]` draws the collider's outline (diffed against the same capture with
//!   overlays off), `["light_radii"]` the point light's reach.
//!
//! No golden: every image is compared with a control capture of the same frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_quick::{Draw, QuickPlugin};
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex,
    MeshRenderer, PointLight, PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);
const CALL_TIMEOUT_MS: u64 = 90_000;
/// `regia_agent::seeing_overlays::COLLIDER_COLOR` (inlined so this file also builds on a base
/// without the module: the fail-before record; `overlay_colors_match_the_library` checks it).
const COLLIDER_COLOR: [u8; 4] = [64, 255, 96, 255];
/// `regia_agent::seeing_overlays::LIGHT_COLOR`.
const LIGHT_COLOR: [u8; 4] = [255, 214, 64, 255];
const W: u32 = 160;
const H: u32 = 120;

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

/// A 40×40 px red square at the center: stands in for the game UI (the quick layer is renamed
/// `ui` below, so the `ui` target has a real overlay layer to draw).
fn hud(draw: Res<Draw>) {
    draw.rect().w_h(40.0, 40.0).color(regia_core::math::Color::RED);
}

fn quad(color: [f32; 4]) -> (Arc<CpuMesh>, Arc<CpuMaterial>) {
    let n = [0.0, 0.0, 1.0];
    let mesh = Arc::new(CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-0.5, -0.5, 0.0], n, [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.5, -0.5, 0.0], n, [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.5, 0.5, 0.0], n, [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([-0.5, 0.5, 0.0], n, [0.0, 1.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
    });
    let material = Arc::new(CpuMaterial {
        base_color: color,
        roughness: 0.6,
        metallic: 0.0,
        ..CpuMaterial::default()
    });
    (mesh, material)
}

/// No temporal effects: two captures of one view are the same pixels, so a diff shows only what
/// the overlay drew (the settings of `headless_deterministic_capture.rs`).
fn deterministic_settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    settings.profile.depth_prepass = false;
    settings
}

fn decode(shot: &Value) -> Vec<u8> {
    let png = base64_decode(shot["png_base64"].as_str().expect("png_base64")).expect("base64");
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("frame");
    buf.truncate(info.buffer_size());
    assert_eq!(buf.len(), (W * H * 4) as usize, "RGBA8 {W}x{H}");
    buf
}

fn pixel(img: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2], img[i + 3]]
}

fn differing(a: &[u8], b: &[u8]) -> usize {
    a.chunks(4).zip(b.chunks(4)).filter(|(p, q)| p != q).count()
}

/// Pixels exactly `color` in `with` but not in `without`.
fn new_pixels_of(with: &[u8], without: &[u8], color: [u8; 4]) -> usize {
    with.chunks(4)
        .zip(without.chunks(4))
        .filter(|(p, q)| *p == color && *q != color)
        .count()
}

fn code(error: &regia_agent::client::ClientError) -> String {
    format!("{error:?}")
}

#[test]
fn camera_ui_targets_and_collider_light_overlays_render() {
    let project = regia_test_utils::unique_temp_dir("agent-seeing-7217");
    std::fs::create_dir_all(&project).expect("project dir");

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        QuickPlugin,
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .insert_resource(regia_quick::QuickSettings {
        fallback: regia_quick::FrameParams {
            size: Vec2::new(W as f32, H as f32),
            scale_factor: 1.0,
        },
        ..Default::default()
    })
    .insert_resource(deterministic_settings())
    .insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    })
    .add_systems(Update, hud);

    // The main camera looks down −Z at the origin; "SideCam" looks at it from +X (order 1, so
    // the main camera stays the primary view).
    let main = Transform::from_translation(Vec3::new(0.0, 0.0, 3.0));
    app.world_mut().spawn((
        main,
        GlobalTransform(main.compute_affine()),
        Camera {
            exposure_ev: Some(14.0),
            clear: ClearMode::Color([0.05, 0.07, 0.12, 1.0]),
            ..Camera::default()
        },
    ));
    let side = Transform {
        translation: Vec3::new(3.0, 0.5, 0.0),
        rotation: Quat::from_rotation_arc(-Vec3::Z, (Vec3::ZERO - Vec3::new(3.0, 0.5, 0.0)).normalize()),
        scale: Vec3::ONE,
    };
    app.world_mut().spawn((
        regia_scene::components::Name::new("SideCam"),
        side,
        GlobalTransform(side.compute_affine()),
        Camera {
            exposure_ev: Some(14.0),
            order: 1,
            clear: ClearMode::Color([0.05, 0.07, 0.12, 1.0]),
            ..Camera::default()
        },
    ));
    // An orange quad facing +Z and a blue one facing +X: the two cameras see different shapes.
    let (mesh, material) = quad([0.9, 0.45, 0.1, 1.0]);
    let front = Transform::default();
    app.world_mut().spawn((
        front,
        GlobalTransform(front.compute_affine()),
        MeshRenderer::new(mesh, material),
    ));
    let (mesh, material) = quad([0.1, 0.3, 0.9, 1.0]);
    let facing_x = Transform {
        translation: Vec3::new(0.0, 0.0, -0.6),
        rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        scale: Vec3::ONE,
    };
    app.world_mut().spawn((
        facing_x,
        GlobalTransform(facing_x.compute_affine()),
        MeshRenderer::new(mesh, material),
    ));
    // A box collider around the front quad and a point light with a 0.8 m reach beside it.
    let collider = Transform::from_translation(Vec3::new(-0.6, 0.0, 0.0));
    app.world_mut().spawn((
        collider,
        GlobalTransform(collider.compute_affine()),
        regia_physics::Collider {
            shape: regia_physics::ColliderShape::Box {
                half_extents: Vec3::new(0.3, 0.6, 0.3),
            },
            ..regia_physics::Collider::default()
        },
    ));
    let light = Transform::from_translation(Vec3::new(0.7, 0.3, 0.2));
    app.world_mut().spawn((
        light,
        GlobalTransform(light.compute_affine()),
        PointLight {
            radius: 0.5,
            intensity: 0.0,
            ..PointLight::default()
        },
    ));
    let sun = Transform::default();
    app.world_mut().spawn((
        sun,
        GlobalTransform(sun.compute_affine()),
        DirectionalLight {
            direction: [0.2, -1.0, -0.4],
            color: [1.0; 3],
            illuminance: 90_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));

    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::spawn(move || {
        let deadline = Instant::now() + SCRIPT_TIMEOUT;
        let mut client = None;
        while Instant::now() < deadline {
            if let Ok((advertised, _)) = endpoint::read(&agent_dir)
                && let Ok(token) = std::fs::read_to_string(agent_dir.join("token"))
                && let Ok(connected) = Client::connect(&advertised.url, token.trim())
            {
                client = Some(connected);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut client = client.expect("agent endpoint");
        call(
            &mut client,
            "play.pause",
            json!({ "timeout_ms": CALL_TIMEOUT_MS }),
        );
        call(
            &mut client,
            "play.step",
            json!({ "frames": 2, "timeout_ms": CALL_TIMEOUT_MS }),
        );
        let shot = |client: &mut Client, extra: Value| -> Value {
            let mut params = json!({
                "width": W, "height": H, "calibration": "neutral", "timeout_ms": CALL_TIMEOUT_MS,
            });
            if let (Some(p), Value::Object(extra)) = (params.as_object_mut(), extra) {
                p.extend(extra);
            }
            call(client, "seeing.screenshot", params)
        };

        // 1. A named camera's view differs from the main view.
        let main_view = decode(&shot(&mut client, json!({ "target": "offscreen", "hud": false })));
        let side_reply = shot(&mut client, json!({ "target": "camera", "name": "SideCam" }));
        assert!(side_reply["camera"].as_u64().is_some(), "{side_reply}");
        let side_view = decode(&side_reply);
        let changed = differing(&main_view, &side_view);
        assert!(
            changed > (W * H / 20) as usize,
            "the SideCam capture must differ from the main view: {changed} of {} pixels",
            W * H
        );
        // The same camera by entity id is the same picture.
        let by_id_reply = shot(
            &mut client,
            json!({ "target": "camera", "entity": side_reply["camera"] }),
        );
        assert_eq!(by_id_reply["camera"], side_reply["camera"], "{by_id_reply}");
        let by_id = decode(&by_id_reply);
        assert_eq!(
            differing(&by_id, &side_view),
            0,
            "entity and name address one camera"
        );
        // An unknown camera is F0003.
        let missing = client.call(
            "seeing.screenshot",
            json!({ "target": "camera", "name": "Nope", "timeout_ms": CALL_TIMEOUT_MS }),
        );
        let error = missing.expect_err("no such camera");
        assert!(code(&error).contains("F0003"), "{error:?}");

        // 2. The UI layer alone: its red square, transparent black around it, no scene.
        let ui = decode(&shot(&mut client, json!({ "target": "ui" })));
        let center = pixel(&ui, W / 2, H / 2);
        assert!(
            center[0] > 200 && center[1] < 40 && center[2] < 40 && center[3] == 255,
            "the UI square at the center: {center:?}"
        );
        assert_eq!(pixel(&ui, 2, 2), [0, 0, 0, 0], "no scene behind the UI");
        assert_eq!(pixel(&ui, W - 3, H - 3), [0, 0, 0, 0], "no scene behind the UI");
        assert_eq!(
            pixel(&main_view, 2, 2)[3],
            255,
            "the scene capture is opaque there"
        );
        let ui_pixels = ui.chunks(4).filter(|p| p[3] != 0).count();
        assert!(
            (1000..=2500).contains(&ui_pixels),
            "only the 40x40 UI square is drawn: {ui_pixels} pixels"
        );

        // 3. Collider and light-radius overlays, diffed against overlays off.
        let plain = decode(&shot(&mut client, json!({ "target": "offscreen", "hud": false })));
        let colliders_reply = shot(
            &mut client,
            json!({ "target": "offscreen", "hud": false, "overlays": ["colliders"] }),
        );
        let colliders = decode(&colliders_reply);
        let outline = new_pixels_of(&colliders, &plain, COLLIDER_COLOR);
        assert!(
            outline > 60,
            "the box outline: {outline} new collider-colored pixels"
        );
        assert_eq!(
            differing(&colliders, &plain),
            outline,
            "the overlay changes only outline pixels"
        );
        assert!(
            colliders_reply["overlay_pixels"].as_u64().is_some_and(|n| n > 0),
            "{colliders_reply}"
        );
        // The outline sits left of the center (the collider is at x = -0.6), not over it.
        let outline_x: Vec<u32> = (0..W * H)
            .filter(|i| {
                let i = (*i * 4) as usize;
                colliders[i..i + 4] == COLLIDER_COLOR && plain[i..i + 4] != COLLIDER_COLOR
            })
            .map(|i| i % W)
            .collect();
        let mean_x = outline_x.iter().map(|x| u64::from(*x)).sum::<u64>() / outline_x.len().max(1) as u64;
        assert!(mean_x < u64::from(W / 2), "outline centroid x {mean_x}");
        let lights = decode(&shot(
            &mut client,
            json!({ "target": "offscreen", "hud": false, "overlays": ["light_radii"] }),
        ));
        let reach = new_pixels_of(&lights, &plain, LIGHT_COLOR);
        assert!(reach > 60, "the light's reach circles: {reach} new pixels");
        // Overlays work on the camera target too.
        let side_colliders = decode(&shot(
            &mut client,
            json!({ "target": "camera", "name": "SideCam", "overlays": ["colliders"] }),
        ));
        assert!(new_pixels_of(&side_colliders, &side_view, COLLIDER_COLOR) > 30);
        // Editor-only overlays are refused with a named reason.
        let refused = client
            .call(
                "seeing.screenshot",
                json!({ "target": "offscreen", "overlays": ["selection"], "timeout_ms": CALL_TIMEOUT_MS }),
            )
            .expect_err("selection is editor state");
        assert!(code(&refused).contains("overlay_needs_editor"), "{refused:?}");

        // 4. The pipeline methods are dispatched (spec 23 § 3.7): without the build permission
        //    they answer F0012, not method-not-found (F0008).
        for method in ["agent.cook", "agent.package"] {
            let denied = client
                .call(
                    method,
                    json!({ "platform": "linux", "dry_run": true, "timeout_ms": CALL_TIMEOUT_MS }),
                )
                .expect_err("the default profile has no build permission");
            assert!(code(&denied).contains("F0012"), "{method}: {denied:?}");
        }
        flag.store(true, Ordering::Relaxed);
    });

    app.finish_plugins();
    // The quick layer stands in for the game UI: re-register its pass under the name "ui".
    {
        let mut overlay = app.world_mut().resource_mut::<regia_render::WindowOverlay>();
        let names = overlay.layer_names();
        let handles = overlay.shared_layers();
        let quick = names
            .iter()
            .position(|name| *name == "quick")
            .map(|index| handles[index].clone())
            .expect("the quick layer");
        overlay.remove("quick");
        overlay.insert_shared(regia_render::WindowOverlay::UI_ORDER, "ui", quick);
    }
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    let joined = script.join();
    let _ = std::fs::remove_dir_all(&project);
    joined.expect("script finished");
    assert!(done.load(Ordering::Relaxed), "script timed out");
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
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= val(c)? << (18 - 6 * i);
        }
        out.push((acc >> 16) as u8);
        if chunk.len() > 2 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(acc as u8);
        }
    }
    Some(out)
}
