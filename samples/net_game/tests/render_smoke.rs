// SPDX-License-Identifier: MIT-0
//! The template's client renders (ADR-2039): a server and two clients play on the in-memory
//! network; client 0 also runs the renderer headless (lavapipe in CI) with the game's `view`, and
//! an agent `seeing.screenshot` of it must show both players: a box in slot 0's red and one in
//! slot 1's blue on the ground plate. No golden image: the test counts pixels by hue.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use net_game::hud::NetGameHud;
use net_game::view::NetGameView;
use net_game::{Avatar, Bot, NetGameWire, PadSource, Side, headless_app, link_options};
use regia::agent::client::Client;
use regia::agent::endpoint;
use regia::net::link::{LinkClock, LinkOptions, NetLink};
use regia::net::{ConditionerConfig, LoopbackNet};
use regia::prelude::*;
use regia_render::RenderPlugin;
use serde_json::{Value, json};

/// The endpoint may be advertised before the first extraction or the join snapshot.
/// Wait for both dressed avatars and a renderable extraction before asking the agent
/// to capture; endpoint availability alone does not imply frame readiness (ADR-4760).
fn capture_ready(app: &mut App) -> bool {
    let world = app.world_mut();
    let renderable = world
        .get_resource::<regia_render::runtime::ExtractedFrame>()
        .is_some_and(|frame| frame.is_renderable() && frame.meshes.len() >= 3);
    let dressed = world
        .query_filtered::<(), (With<Avatar>, With<regia_render::MeshRenderer>)>()
        .iter(world)
        .count();
    renderable && dressed == 2
}

/// Hang guard only (device creation on a loaded lavapipe host can take a while).
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(240);
/// Per-call protocol timeout, as in the engine's own headless capture test.
const CALL_TIMEOUT_MS: u64 = 120_000;

/// One render test at a time: each builds its pipelines on the software rasterizer, and two doing
/// that at once on a loaded host miss each other's frame deadlines.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

/// What the frame shows of each player.
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    /// Pixels that read as slot 0 (red).
    red: usize,
    /// Pixels that read as slot 1 (blue).
    blue: usize,
    /// Distance in pixels between the centers of the red and the blue pixels.
    apart: f64,
}

fn hues(png_bytes: &[u8]) -> Seen {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    let channels = info.color_type.samples();
    let width = info.width as usize;
    buf.truncate(info.buffer_size());
    let mut seen = Seen::default();
    let (mut red_at, mut blue_at) = ((0.0, 0.0), (0.0, 0.0));
    for (i, px) in buf.chunks(channels).enumerate() {
        let (r, g, b) = (i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
        let (x, y) = ((i % width) as f64, (i / width) as f64);
        if r > g + 40 && r > b + 40 {
            seen.red += 1;
            red_at = (red_at.0 + x, red_at.1 + y);
        }
        if b > r + 40 && b > g + 15 {
            seen.blue += 1;
            blue_at = (blue_at.0 + x, blue_at.1 + y);
        }
    }
    if seen.red > 0 && seen.blue > 0 {
        let (rn, bn) = (seen.red as f64, seen.blue as f64);
        let dx = red_at.0 / rn - blue_at.0 / bn;
        let dy = red_at.1 / rn - blue_at.1 / bn;
        seen.apart = (dx * dx + dy * dy).sqrt();
    }
    seen
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

#[test]
fn a_rendered_client_shows_both_players() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let project = regia_test_utils::temp::unique_temp_dir("net-game-render");
    let clock = LinkClock::Manual {
        start_ms: 0,
        step_us: 16_667,
    };
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let mut server = headless_app(Side::Server);
    let opts = link_options(clock, ConditionerConfig::default());
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<NetGameWire>(Box::new(net.bind(server_addr).expect("bind")), &opts)
        .expect("host");
    let client_opts = LinkOptions {
        server_key: server.world().resource::<NetLink>().server_key(),
        ..opts
    };
    let mut clients: Vec<App> = (0..2)
        .map(|_| {
            let mut app = headless_app(Side::Client);
            app.world_mut()
                .resource_mut::<NetLink>()
                .connect::<NetGameWire>(
                    Box::new(net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind")),
                    server_addr,
                    &client_opts,
                )
                .expect("connect");
            app
        })
        .collect();
    clients[0]
        .add_plugins((
            MinimalPlugins,
            RenderPlugin { headless: true },
            NetGameView { input: false },
        ))
        .insert_resource(regia::agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins(regia::agent::AgentPlugin);

    assert!(!capture_ready(&mut clients[0]), "no joined frame before updates");
    for app in &mut clients {
        app.finish_plugins();
    }
    for _ in 0..600 {
        server.update();
        for app in &mut clients {
            app.update();
        }
        if capture_ready(&mut clients[0]) {
            break;
        }
    }
    assert!(
        capture_ready(&mut clients[0]),
        "both avatars joined and extracted"
    );

    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("net-game-render".into())
        .spawn(move || {
            let mut agent = None;
            for _ in 0..6000 {
                if let Ok((advertised, token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &token)
                {
                    agent = Some(connected);
                    break;
                }
                std::thread::yield_now();
            }
            let Some(mut agent) = agent else {
                panic!("no endpoint at {}", agent_dir.display());
            };
            // Shoot until both avatars are in the frame (they arrive with the join snapshot).
            let mut last = Seen::default();
            let mut png = Vec::new();
            for _ in 0..600 {
                let params =
                    json!({ "target": "offscreen", "width": 160, "height": 90, "timeout_ms": CALL_TIMEOUT_MS });
                // The agent can connect before the first frame is extracted (F0011): shoot again.
                let shot = match agent.call("seeing.screenshot", params) {
                    Ok(shot) => shot,
                    Err(err) if format!("{err:?}").contains("F0011") => {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    Err(err) => panic!("seeing.screenshot failed: {err:?}"),
                };
                let bytes = base64_decode(shot["png_base64"].as_str().expect("png_base64")).expect("base64");
                assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "PNG magic");
                last = hues(&bytes);
                png = bytes;
                if last.red > 0 && last.blue > 0 {
                    break;
                }
            }
            if let Some(path) = std::env::var_os("NET_GAME_RENDER_PNG") {
                let _ = std::fs::write(path, &png);
            }
            assert!(last.red > 0, "slot 0's red box is in the frame: {last:?}");
            assert!(last.blue > 0, "slot 1's blue box is in the frame: {last:?}");
            // Two boxes in two places, not one blend: the avatars start 6 m apart.
            assert!(last.apart > 4.0, "the boxes are apart in the frame: {last:?}");
            flag.store(true, Ordering::Relaxed);
        })
        .expect("script thread");

    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        server.update();
        for app in &mut clients {
            app.update();
        }
    }
    script.join().expect("the script passed");
    assert!(done.load(Ordering::Relaxed), "the capture did not finish");
    let _ = std::fs::remove_dir_all(&project);
}

/// The pixel at `(x, y)` of a decoded RGBA or RGB frame.
fn pixel(png_bytes: &[u8], x: usize, y: usize) -> [i32; 3] {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    let channels = info.color_type.samples();
    let at = (y * info.width as usize + x) * channels;
    [i32::from(buf[at]), i32::from(buf[at + 1]), i32::from(buf[at + 2])]
}

fn differs(a: [i32; 3], b: [i32; 3]) -> i32 {
    (a[0] - b[0]).abs() + (a[1] - b[1]).abs() + (a[2] - b[2]).abs()
}

/// The HUD reaches the pixels: the target bracket's corner and a minimap blip draw where the agent
/// surface says they are, in a frame with the UI layers and not in the same frame without them. No
/// golden image: the frame is compared with itself.
#[test]
fn a_rendered_client_shows_its_hud_where_the_agent_says() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    let project = regia_test_utils::temp::unique_temp_dir("net-game-hud-render");
    let clock = LinkClock::Manual {
        start_ms: 0,
        step_us: 16_667,
    };
    let net = LoopbackNet::new();
    let server_addr = SocketAddr::from(([127, 0, 0, 1], 7777));
    let mut server = headless_app(Side::Server);
    let opts = link_options(clock, ConditionerConfig::default());
    server
        .world_mut()
        .resource_mut::<NetLink>()
        .host::<NetGameWire>(Box::new(net.bind(server_addr).expect("bind")), &opts)
        .expect("host");
    let client_opts = LinkOptions {
        server_key: server.world().resource::<NetLink>().server_key(),
        ..opts
    };
    let mut clients: Vec<App> = (0..2)
        .map(|i| {
            let mut app = headless_app(Side::Client);
            // Both stand still after a moment, so the two frames below show one scene.
            app.insert_resource(PadSource::Bot(Bot::new(i as f32, 0, 30)));
            app.world_mut()
                .resource_mut::<NetLink>()
                .connect::<NetGameWire>(
                    Box::new(net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind")),
                    server_addr,
                    &client_opts,
                )
                .expect("connect");
            app
        })
        .collect();
    // The window's messages, which a real client's runner adds and the UI reads.
    {
        use regia::platform as p;
        clients[0]
            .add_message::<p::WindowResized>()
            .add_message::<p::WindowScaleFactorChanged>()
            .add_message::<p::CursorMoved>()
            .add_message::<p::MouseButtonInput>()
            .add_message::<p::TouchInput>()
            .add_message::<p::MouseWheel>()
            .add_message::<p::KeyboardInput>()
            .add_message::<p::ImeEvent>();
    }
    clients[0]
        .add_plugins((
            MinimalPlugins,
            RenderPlugin { headless: true },
            NetGameView { input: false },
            NetGameHud::default(),
        ))
        .insert_resource(regia::agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .insert_resource(regia::agent::UiAgentHost::new(regia::ui::agent_bridge::host_call))
        .add_plugins(regia::agent::AgentPlugin);

    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("net-game-hud-render".into())
        .spawn(move || {
            let mut agent = None;
            for _ in 0..6000 {
                if let Ok((advertised, token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &token)
                {
                    agent = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut agent) = agent else {
                panic!("no endpoint at {}", agent_dir.display());
            };
            // Wait until the bracket and both blips exist and nothing moves any more.
            let mut bracket = Value::Null;
            let mut minimap = Value::Null;
            let mut settled = 0;
            let mut last_center = Value::Null;
            for _ in 0..4000 {
                let wait = json!({ "timeout_ms": CALL_TIMEOUT_MS });
                bracket = call(&mut agent, "ui.target_bracket", wait.clone());
                // By name: without `entity` a call takes the lowest entity bits.
                let node = call(
                    &mut agent,
                    "ui.query",
                    json!({ "name": "minimap", "timeout_ms": CALL_TIMEOUT_MS }),
                );
                minimap = call(
                    &mut agent,
                    "ui.radar",
                    json!({ "entity": node["nodes"][0]["id"], "timeout_ms": CALL_TIMEOUT_MS }),
                );
                let center = bracket["brackets"][0]["center"].clone();
                settled = if center.is_array()
                    && center == last_center
                    && minimap["blips"].as_array().is_some_and(|b| b.len() == 2)
                {
                    settled + 1
                } else {
                    0
                };
                last_center = center;
                if settled >= 30 {
                    break;
                }
                // Two counted calls a round, under the 50 per second the agent server admits.
                std::thread::sleep(Duration::from_millis(60));
            }
            assert!(settled >= 30, "never settled: {bracket} {minimap}");
            let state = call(&mut agent, "ui.state", json!({ "timeout_ms": CALL_TIMEOUT_MS }));
            let scale = state["scale"]["roots"][0]["scale"].as_f64().expect("scale");
            let window = &state["scale"]["window_size"];
            let (w, h) = (
                window[0].as_f64().expect("window width") as u32,
                window[1].as_f64().expect("window height") as u32,
            );
            let shot = |agent: &mut Client, layers: bool| {
                let shot = call(
                    agent,
                    "seeing.screenshot",
                    json!({
                        "target": "offscreen",
                        "width": w,
                        "height": h,
                        "hud": false,
                        "window_layers": layers,
                        "timeout_ms": CALL_TIMEOUT_MS,
                    }),
                );
                base64_decode(shot["png_base64"].as_str().expect("png_base64")).expect("base64")
            };
            let markers = call(
                &mut agent,
                "ui.world_markers",
                json!({ "timeout_ms": CALL_TIMEOUT_MS }),
            );
            let with_ui = shot(&mut agent, true);
            let without = shot(&mut agent, false);
            if let Some(path) = std::env::var_os("NET_GAME_HUD_PNG") {
                let _ = std::fs::write(path, &with_ui);
            }

            // The middle of the bracket's top-left corner's top arm, in physical px.
            let b = &bracket["brackets"][0];
            let cx = b["center"][0].as_f64().expect("center") * scale;
            let cy = b["center"][1].as_f64().expect("center") * scale;
            let half = b["half_px"].as_f64().expect("half") * scale;
            let (x, y) = ((cx - half + 6.0 * scale) as usize, (cy - half + scale) as usize);
            let (on, off) = (pixel(&with_ui, x, y), pixel(&without, x, y));
            assert!(
                differs(on, off) > 60,
                "the bracket's corner at ({x}, {y}): {on:?} with the UI, {off:?} without"
            );

            // The other player's marker: its dot is on the point the query reports.
            let m = &markers["markers"][0];
            assert_eq!(m["visible"], true, "{markers}");
            let (mx, my) = (
                m["pos"][0].as_f64().expect("pos") * scale,
                m["pos"][1].as_f64().expect("pos") * scale,
            );
            let (on, off) = (
                pixel(&with_ui, mx as usize, my as usize),
                pixel(&without, mx as usize, my as usize),
            );
            assert!(
                differs(on, off) > 60,
                "the marker at ({mx}, {my}): {on:?} with the UI, {off:?} without; {m}"
            );

            // The first blip on the minimap: inside the node the query reports.
            let node = call(
                &mut agent,
                "ui.query",
                json!({ "name": "minimap", "timeout_ms": CALL_TIMEOUT_MS }),
            );
            let rect = &node["nodes"][0]["rect"];
            let (rx, ry) = (
                rect[0].as_f64().expect("rect x"),
                rect[1].as_f64().expect("rect y"),
            );
            let blip = &minimap["blips"][0];
            let bx = rx + blip["pos"][0].as_f64().expect("pos") * scale;
            let by = ry + blip["pos"][1].as_f64().expect("pos") * scale;
            let (on, off) = (
                pixel(&with_ui, bx as usize, by as usize),
                pixel(&without, bx as usize, by as usize),
            );
            assert!(
                differs(on, off) > 60,
                "the blip at ({bx}, {by}): {on:?} with the UI, {off:?} without; {blip}"
            );
            // The minimap's own background changes every pixel of its node, so the blip must also
            // stand out against a corner of the node, which no avatar can reach.
            let background = pixel(&with_ui, rx as usize + 3, ry as usize + 3);
            assert!(
                differs(on, background) > 60,
                "the blip at ({bx}, {by}) is {on:?}, the minimap around it {background:?}; {blip}"
            );
            flag.store(true, Ordering::Relaxed);
        })
        .expect("script thread");

    for app in &mut clients {
        app.finish_plugins();
    }
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        server.update();
        for app in &mut clients {
            app.update();
        }
    }
    script.join().expect("the script passed");
    assert!(done.load(Ordering::Relaxed), "the capture did not finish");
    let _ = std::fs::remove_dir_all(&project);
}
