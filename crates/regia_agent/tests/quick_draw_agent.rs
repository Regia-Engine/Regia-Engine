// SPDX-License-Identifier: MPL-2.0
//! The quick draw layer through the agent protocol, headless on lavapipe: `quick.draw_list`
//! reports the frame's drawings, a `QuickDraw` predicate passes `play.expect`, and an offscreen
//! `seeing.screenshot` shows the 2D layer only with `window_layers: true`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_core::math::{Vec2, Vec3};
use regia_quick::{Draw, Draw3d, QuickPlugin};
use regia_render::{Camera, ClearMode, DirectionalLight, RenderPlugin};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// The first screenshot builds every pipeline. Measured cold: 20.6 s on the test machine RTX 5080,
/// 7 s alone on an RTX 5090; it went past 60 s beside a pregate's other binaries only while
/// the build server's shared NVIDIA shader cache sat full at its 1 GB default (pregate 6984911dd: `no
/// answer within 65000 ms`; the pregate runs with a 10 GB cache since 11-18). The guards leave
/// over 4x the cold figure and still fail a 2-minute stall (ADR-8598,
/// `a_two_minute_screenshot_stall_still_fails_the_hang_guard`).
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);
const CALL_TIMEOUT_MS: u64 = 90_000;

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn view(draw: Res<Draw>, draw3d: Res<Draw3d>) {
    // A red square over the centre of the frame and a HUD line.
    draw.rect().w_h(40.0, 40.0).color(regia_core::math::Color::RED);
    draw.text("AGENT SEES ME").font_size(10).y(-50.0);
    draw.line()
        .start(Vec2::new(-60.0, 50.0))
        .end(Vec2::new(60.0, 50.0))
        .weight(3.0);
    draw3d.cube(Vec3::ZERO, Vec3::splat(0.5));
}

fn shot(client: &mut Client, window_layers: bool) -> Vec<u8> {
    let shot = call(
        client,
        "seeing.screenshot",
        json!({
            "target": "offscreen",
            "width": 160,
            "height": 120,
            "window_layers": window_layers,
            "timeout_ms": CALL_TIMEOUT_MS,
        }),
    );
    let png = base64_decode(shot["png_base64"].as_str().expect("png_base64")).expect("base64");
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("frame");
    buf.truncate(info.buffer_size());
    buf
}

fn centre(pixels: &[u8]) -> [u8; 4] {
    let i = ((60 * 160 + 80) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

#[test]
fn agents_read_and_see_the_quick_layer() {
    let project = std::env::temp_dir().join(format!("regia-agent-quick-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
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
            size: Vec2::new(160.0, 120.0),
            scale_factor: 1.0,
        },
        ..Default::default()
    })
    .add_systems(Update, view);
    let camera = Transform::from_translation(Vec3::new(0.0, 1.0, 3.0));
    app.world_mut().spawn((
        camera,
        GlobalTransform(camera.compute_affine()),
        Camera {
            exposure_ev: Some(14.0),
            clear: ClearMode::Color([0.05, 0.07, 0.12, 1.0]),
            ..Camera::default()
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

        let list = call(
            &mut client,
            "quick.draw_list",
            json!({ "timeout_ms": CALL_TIMEOUT_MS }),
        );
        assert_eq!(list["texts"][0], "AGENT SEES ME", "{list}");
        assert_eq!(list["by_kind"]["rect"], 1, "{list}");
        assert_eq!(list["by_kind"]["line"], 1, "{list}");
        assert_eq!(list["primitives_3d"], 1, "{list}");
        assert!(list["frame"].as_u64().is_some_and(|f| f >= 2), "{list}");

        let expect = call(
            &mut client,
            "play.expect",
            json!({ "assertions": [
                { "name": "HUD text", "condition": { "resource": "QuickDraw", "path": "texts.0",
                  "op": "eq", "value": "AGENT SEES ME" } },
                { "name": "one rect", "condition": { "resource": "QuickDraw", "path": "by_kind.rect",
                  "op": "eq", "value": 1 } }
            ], "timeout_ms": CALL_TIMEOUT_MS }),
        );
        assert_eq!(expect["pass"], true, "{expect}");

        let plain = shot(&mut client, false);
        let layered = shot(&mut client, true);
        let (p, l) = (centre(&plain), centre(&layered));
        assert!(
            l[0] > 200 && l[1] < 40 && l[2] < 40,
            "the red square covers the center: {l:?}"
        );
        assert!(
            !(p[0] > 200 && p[1] < 40),
            "without window_layers the 2D layer is absent: {p:?}"
        );
        flag.store(true, Ordering::Relaxed);
    });

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    assert!(done.load(Ordering::Relaxed), "script timed out");
    let _ = std::fs::remove_dir_all(&project);
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
