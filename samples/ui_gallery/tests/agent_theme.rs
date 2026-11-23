// SPDX-License-Identifier: MIT-0
//! The theme agent surface (lane D7, `book/agents/ui-theme.md`): the D7 acceptance of
//! `docs/specs/ui-default-design.md` § 8, through real protocol requests against the headless
//! gallery. Requests go through the dispatcher in process under a fixed 50 ms step, so every
//! wait is a counted number of frames; captures are compared with each other, never with a
//! golden. The preview test needs a GPU adapter (lavapipe in CI), like `offscreen_gallery`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::{Receiver, bounded, unbounded};
use regia::agent::protocol::{PERMISSION_DENIED, Request, Response};
use regia::agent::server::Inbound;
use regia::agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, UiAgentHost};
use regia::prelude::*;
use regia_ui::agent_bridge::host_call;
use regia_ui::style::Align;
use regia_ui::theme_agent::{ThemeFiles, ThemePreviewDevice, ThemeWatch, load_theme_file};
use regia_ui::theme_color_ops::to_hex;
use regia_ui::{NodeName, ScaleMode, ThemeResolver, UiThemes, parse_theme_ron};
use serde_json::{Value, json};
use ui_gallery::gallery::UiGallery;
use ui_gallery::shots::{capture, headless_app, resize};

const W: u32 = 1280;
const H: u32 = 800;
const ORANGE: &str = "#e8793a";

/// A headless gallery with the agent dispatcher and a large primary button pinned top-left, so
/// the accent covers a known area of every capture.
fn app() -> App {
    let mut app = headless_app(UiGallery);
    app.add_plugins(AgentPlugin);
    app.insert_resource(UiAgentHost::new(host_call));
    app.update();
    let world = app.world_mut();
    let root = world
        .query_filtered::<Entity, With<UiRoot>>()
        .iter(world)
        .next()
        .expect("the gallery root");
    let swatch = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                position: PositionType::Absolute,
                inset: Edges {
                    left: Dimension::Points(0.0),
                    top: Dimension::Points(0.0),
                    right: Dimension::Auto,
                    bottom: Dimension::Auto,
                },
                size: [Dimension::Points(240.0), Dimension::Points(80.0)],
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
            Button,
            Classes(vec!["button".into(), "primary".into()]),
            Themed::default(),
            NodeName("accent-swatch".into()),
        ))
        .id();
    world.entity_mut(root).add_child(swatch);
    resize(&mut app, W, H, Some(ScaleMode::ConstantPixelSize));
    app
}

fn send(app: &mut App, method: &str, params: Value) -> Receiver<Response> {
    let (reply, answers) = bounded(1);
    let (inbox, requests) = unbounded();
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
        .expect("queued");
    app.insert_resource(AgentInbox(requests));
    answers
}

fn await_answer(app: &mut App, answers: &Receiver<Response>, frames: usize) -> Option<Response> {
    for _ in 0..frames {
        app.update();
        if let Ok(response) = answers.try_recv() {
            return Some(response);
        }
    }
    None
}

fn request(app: &mut App, method: &str, params: Value) -> Response {
    let answers = send(app, method, params);
    await_answer(app, &answers, 2).unwrap_or_else(|| panic!("{method} was not answered"))
}

fn call(app: &mut App, method: &str, params: Value) -> Value {
    let response = request(app, method, params);
    match response.result {
        Some(result) => result,
        None => panic!("{method} failed: {:?}", response.error),
    }
}

fn reason(response: &Response) -> String {
    response
        .error
        .as_ref()
        .and_then(|e| e.data.as_ref())
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_owned()
}

fn frames(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

fn generation(app: &App) -> u64 {
    app.world().resource::<ThemeResolver>().generation()
}

fn accent_of(app: &App, theme: &str) -> String {
    let set = &app.world().resource::<UiThemes>().set;
    let resolved = set.resolve(theme, None).expect("theme resolves");
    to_hex(resolved.color("color.accent").expect("accent"))
}

/// Waits through `play.wait_until` for the theme generation to reach `n`.
fn wait_for_generation(app: &mut App, n: u64) {
    let waiting = send(
        app,
        "play.wait_until",
        json!({ "condition": { "predicate": "ui.theme_generation", "at_least": n }, "timeout_ms": 120_000 }),
    );
    let answer = await_answer(app, &waiting, 6).expect("the generation predicate resolves");
    assert_eq!(answer.result.expect("result")["satisfied"], true);
}

/// Pixels whose RGBA differs by more than 2 in any channel.
fn differing(a: &[u8], b: &[u8]) -> usize {
    assert_eq!(a.len(), b.len(), "captures of the same size");
    let (pa, _) = a.as_chunks::<4>();
    let (pb, _) = b.as_chunks::<4>();
    pa.iter()
        .zip(pb)
        .filter(|(p, q)| p.iter().zip(q.iter()).any(|(x, y)| x.abs_diff(*y) > 2))
        .count()
}

/// Pixels inside the accent swatch (240x80 at the top-left, inset by 8 px against edge
/// antialiasing) whose RGBA differs by more than 2 in any channel.
fn differing_in_swatch(a: &[u8], b: &[u8]) -> usize {
    let mut count = 0;
    for y in 8..72usize {
        for x in 8..232usize {
            let i = (y * W as usize + x) * 4;
            if a[i..i + 4]
                .iter()
                .zip(&b[i..i + 4])
                .any(|(p, q)| p.abs_diff(*q) > 2)
            {
                count += 1;
            }
        }
    }
    count
}

/// Decodes the PNG a preview wrote and checks the reply carries the same image.
fn preview_pixels(reply: &Value) -> Vec<u8> {
    assert_eq!(reply["mime"], "image/png");
    assert!(
        reply["png_base64"]
            .as_str()
            .is_some_and(|b| b.starts_with("iVBOR")),
        "PNG bytes in the reply"
    );
    let path = reply["path"].as_str().expect("path");
    let bytes = std::fs::read(path).expect("the preview file exists");
    let image = image::load_from_memory(&bytes).expect("a PNG").to_rgba8();
    assert_eq!(
        (image.width(), image.height()),
        (
            reply["width"].as_u64().unwrap() as u32,
            reply["height"].as_u64().unwrap() as u32
        )
    );
    image.into_raw()
}

static DIRS: AtomicU64 = AtomicU64::new(0);

/// A fresh directory per test (pid + counter), never in the source tree.
fn temp_dir(tag: &str) -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia_agent_theme_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write_atomically(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write tmp");
    std::fs::rename(&tmp, path).expect("rename");
}

fn game_theme(accent: &str, extra: &str) -> String {
    format!(
        "(format_version: 1, name: \"my-game\", extends: Some(\"regia-dark\"), \
         tokens: {{ \"color.accent\": \"{accent}\", \"radius.md\": 3{extra} }})"
    )
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn preview_leaves_the_live_ui_alone_until_theme_set() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match regia_render::RenderDevice::headless() {
        Ok(d) => d,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let mut app = app();
    app.insert_resource(ThemePreviewDevice(device.clone()));
    let gen0 = generation(&app);
    let set0 = app.world().resource::<UiThemes>().set.clone();
    let live0 = capture(&app, &device, W, H, None).expect("live capture");

    // Previews: the shipped theme, then with an orange accent that is not applied.
    let size = json!([W, H]);
    let plain = call(&mut app, "ui.theme_preview", json!({ "size": size }));
    let orange = call(
        &mut app,
        "ui.theme_preview",
        json!({ "size": size, "tokens": { "color.accent": ORANGE } }),
    );
    assert_eq!(plain["page"], "theme-sheet");
    let plain_px = preview_pixels(&plain);
    let orange_px = preview_pixels(&orange);
    assert!(
        differing(&plain_px, &orange_px) > 1000,
        "the accent override shows in the preview"
    );
    assert!(
        differing(&orange_px, &live0) > 1000,
        "the preview is not the live UI"
    );

    // The live UI did not move: same theme set, same generation, same pixels.
    assert_eq!(generation(&app), gen0, "a preview costs no theme generation");
    assert_eq!(app.world().resource::<UiThemes>().set, set0);
    assert_eq!(orange["generation"], gen0);
    frames(&mut app, 4);
    let live1 = capture(&app, &device, W, H, None).expect("live capture");
    let drift = differing(&live0, &live1);
    assert_eq!(
        differing_in_swatch(&live0, &live1),
        0,
        "the accent swatch did not move"
    );

    // Apply it: the live capture changes once the new generation has cross-faded in.
    let set = call(
        &mut app,
        "ui.theme_set",
        json!({ "tokens": { "color.accent": ORANGE } }),
    );
    assert_eq!(set["changed"], json!(["tokens.color.accent"]));
    let target = set["applies_at_generation"].as_u64().expect("generation");
    assert_eq!(target, gen0 + 1);
    wait_for_generation(&mut app, target);
    frames(&mut app, 8); // motion.base is 160 ms: four 50 ms frames, with margin
    let live2 = capture(&app, &device, W, H, None).expect("live capture");
    // The swatch is the one region known to be accent-filled: it must all change. The whole frame
    // also changes more than it drifted between frames before the set.
    let swatch = differing_in_swatch(&live1, &live2);
    assert!(
        swatch > 224 * 64 * 9 / 10,
        "the accent swatch restyles ({swatch} px)"
    );
    let changed = differing(&live1, &live2);
    assert!(
        changed > drift + swatch / 2,
        "theme_set restyles the live UI ({changed} pixels changed, {drift} drifted before)"
    );
    assert_eq!(accent_of(&app, "regia-dark"), ORANGE);

    // A preview of the live theme now shows what the override showed before.
    let after = call(&mut app, "ui.theme_preview", json!({ "size": size }));
    let after_px = preview_pixels(&after);
    assert!(
        differing(&after_px, &orange_px) * 1000 <= after_px.len() / 4,
        "same theme, same sheet, same pixels"
    );
}

#[test]
fn a_broken_file_on_disk_keeps_the_last_good_theme_and_reports_the_key_path() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = app();
    let dir = temp_dir("reload");
    let path = dir.join("my-game.theme.ron");
    write_atomically(&path, &game_theme(ORANGE, ""));
    assert_eq!(load_theme_file(app.world_mut(), &path).expect("loads"), "my-game");
    {
        let mut files = app.world_mut().resource_mut::<ThemeFiles>();
        files.watch = ThemeWatch::On;
        files.poll_frames = 1;
    }
    let selected = call(&mut app, "ui.theme_select", json!({ "theme": "my-game" }));
    wait_for_generation(&mut app, selected["applies_at_generation"].as_u64().unwrap());
    let good_gen = generation(&app);
    assert_eq!(accent_of(&app, "my-game"), ORANGE);

    // Break it: an unknown reference. The watcher notices within a frame and keeps the old theme.
    write_atomically(
        &path,
        &game_theme(
            "#11aa44",
            ", \"color.accent-hover\": \"lighten(@color.nope, 0.1)\"",
        ),
    );
    frames(&mut app, 2);
    assert_eq!(accent_of(&app, "my-game"), ORANGE, "the last good theme stays");
    assert_eq!(generation(&app), good_gen, "a failed reload swaps nothing");
    let source = &app.world().resource::<ThemeFiles>().sources()[0];
    let issue = source.error.clone().expect("the error is kept");
    assert_eq!(issue.key, "tokens.color.accent-hover");

    let list = call(&mut app, "ui.theme_list", json!({}));
    let entry = list["themes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "my-game")
        .expect("listed")
        .clone();
    assert_eq!(entry["error"]["key"], "tokens.color.accent-hover");
    let shown = app.world().resource::<ThemeFiles>().shown(&path);
    assert_eq!(entry["path"], shown);
    let reload = call(&mut app, "ui.theme_reload", json!({ "theme": "my-game" }));
    assert_eq!(reload["failed"][0]["key"], "tokens.color.accent-hover");
    assert_eq!(reload["reloaded"], json!([]));

    // Fix it: the new accent swaps in on the next look, and the error clears.
    write_atomically(&path, &game_theme("#11aa44", ", \"radius.lg\": 6"));
    frames(&mut app, 2);
    assert_eq!(accent_of(&app, "my-game"), "#11aa44");
    assert!(generation(&app) > good_gen);
    assert!(app.world().resource::<ThemeFiles>().sources()[0].error.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn persist_round_trips_through_the_loader_and_needs_the_assets_permission() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = app();
    let dir = temp_dir("persist");
    let path = dir.join("my-game.theme.ron");
    write_atomically(&path, &game_theme(ORANGE, ""));
    load_theme_file(app.world_mut(), &path).expect("loads");

    let out = call(
        &mut app,
        "ui.theme_set",
        json!({ "theme": "my-game", "tokens": { "radius.md": 5, "color.on-accent": "#140a04" }, "persist": true }),
    );
    assert_eq!(
        out["persisted"],
        app.world().resource::<ThemeFiles>().shown(&path)
    );
    let written = std::fs::read_to_string(&path).expect("written");
    let reparsed = parse_theme_ron(&written, "my-game.theme.ron").expect("D1's loader reads it back");
    assert_eq!(
        Some(&reparsed),
        app.world().resource::<UiThemes>().set.get("my-game"),
        "the file holds exactly the live theme"
    );
    let reload = call(&mut app, "ui.theme_reload", json!({ "theme": "my-game" }));
    assert_eq!(
        reload["reloaded"][0]["changed"], false,
        "reloading the saved file changes nothing"
    );

    // Without `assets`, persisting is denied and nothing is written or applied.
    app.insert_resource(AgentPermissions {
        config: AgentConfig {
            permissions: ["read", "edit_scene", "play"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            ..AgentConfig::default()
        },
        source: None,
    });
    let denied = request(
        &mut app,
        "ui.theme_set",
        json!({ "theme": "my-game", "tokens": { "radius.md": 9 }, "persist": true }),
    );
    assert_eq!(denied.error.as_ref().map(|e| e.code), Some(PERMISSION_DENIED));
    assert_eq!(std::fs::read_to_string(&path).expect("file"), written);
    // A live-only change still works with edit_scene.
    call(
        &mut app,
        "ui.theme_set",
        json!({ "theme": "my-game", "tokens": { "radius.md": 9 } }),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lint_flags_a_two_to_one_pair_and_passes_the_shipped_themes() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = app();
    for theme in ["regia-dark", "regia-light", "regia-high-contrast"] {
        let lint = call(&mut app, "ui.theme_lint", json!({ "theme": theme }));
        assert_eq!(lint["clean"], true, "{theme}: {lint}");
    }
    let set = call(
        &mut app,
        "ui.theme_set",
        json!({ "tokens": { "color.text": "#4d4d4d" } }),
    );
    let warning = set["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["kind"] == "contrast")
        .expect("a contrast warning")
        .clone();
    assert_eq!(warning["pair"], json!(["color.text", "color.surface"]));
    let ratio = warning["ratio"].as_f64().unwrap();
    assert!((1.9..2.1).contains(&ratio), "{ratio}");
    assert_eq!(warning["required"], 4.5);
    let lint = call(&mut app, "ui.theme_lint", json!({}));
    assert_eq!(lint["clean"], false);
}

#[test]
fn the_generation_predicate_resolves_after_a_swap() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = app();
    let before = generation(&app);
    let selected = call(
        &mut app,
        "ui.theme_select",
        json!({ "theme": "regia-light", "reduced_motion": true }),
    );
    assert_eq!(selected["previous"]["theme"], "regia-dark");
    let target = selected["applies_at_generation"].as_u64().unwrap();
    assert!(target > before);
    wait_for_generation(&mut app, target);
    let list = call(&mut app, "ui.theme_list", json!({}));
    assert_eq!(list["resolved"], "regia-light");
    assert!(list["generation"].as_u64().unwrap() >= target);

    // A wait that cannot hold yet stays pending.
    let waiting = send(
        &mut app,
        "play.wait_until",
        json!({ "condition": { "predicate": "ui.theme_generation", "at_least": target + 50 }, "timeout_ms": 120_000 }),
    );
    assert!(await_answer(&mut app, &waiting, 3).is_none());
    // Bad parameters answer with a documented reason.
    let bad = request(&mut app, "ui.theme_select", json!({ "theme": "no-such-theme" }));
    assert_eq!(reason(&bad), "ui_theme_unknown");
}

/// ADR-7208 (roadmap § 2a lanes 7 and 8, spec 36 § 8.3): `ui.theme_list` lists the named token
/// slots with the resolved theme's values (the high-contrast caption theme makes the plate
/// opaque), and `ui.theme_get` reports a type's theme padding and min/max size.
#[test]
fn theme_list_lists_the_token_slots_and_theme_get_reports_the_layout_box() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = app();
    let list = call(&mut app, "ui.theme_list", json!({}));
    let slots = list["slots"].as_array().expect("slots").clone();
    let slot = |slots: &[Value], key: &str| {
        slots
            .iter()
            .find(|s| s["key"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("no slot {key} in {slots:?}"))
    };
    for key in [
        "color.scrollbar-track",
        "color.scrollbar-thumb",
        "color.scrollbar-thumb-hover",
        "color.scrollbar-thumb-active",
        "subtitle.text",
        "subtitle.plate",
        "subtitle.plate-opacity",
    ] {
        let s = slot(&slots, key);
        assert!(s["used_by"].as_str().is_some_and(|t| !t.is_empty()), "{key}");
        assert!(s["fallback"].as_str().is_some(), "{key}");
    }
    assert!(
        slot(&slots, "color.scrollbar-thumb")["value"].is_null(),
        "optional slot unset"
    );
    assert!(
        slot(&slots, "subtitle.plate")["value"].is_string(),
        "dark sets the plate"
    );
    assert!(slot(&slots, "subtitle.plate-opacity")["value"].is_null());

    let selected = call(
        &mut app,
        "ui.theme_select",
        json!({ "theme": "regia-dark", "high_contrast": true, "reduced_motion": true }),
    );
    let target = selected["applies_at_generation"].as_u64().unwrap();
    wait_for_generation(&mut app, target);
    let list = call(&mut app, "ui.theme_list", json!({}));
    assert_eq!(list["resolved"], "regia-high-contrast");
    let slots = list["slots"].as_array().expect("slots").clone();
    assert_eq!(slot(&slots, "subtitle.plate-opacity")["value"], json!(1.0));

    let set = call(
        &mut app,
        "ui.theme_set",
        json!({ "theme": "regia-dark", "types": { "Card": { "props": {
            "padding": [1, 2, 3, 4], "min_height": 20, "max_width": 400, "max_height": 300 } } } }),
    );
    assert!(set["changed"].as_array().is_some_and(|c| !c.is_empty()), "{set}");
    let got = call(
        &mut app,
        "ui.theme_get",
        json!({ "theme": "regia-dark", "type": "Card" }),
    );
    let props = &got["resolved"]["props"];
    assert_eq!(
        props["padding"],
        json!({ "left": 4.0, "top": 1.0, "right": 2.0, "bottom": 3.0 })
    );
    assert_eq!(props["min_height"], json!(20.0));
    assert_eq!(props["max_width"], json!(400.0));
    assert_eq!(props["max_height"], json!(300.0));
}

/// ADR-9340: `ui.theme_get` reports the layout fields a theme moves (`height`, `padding_x`,
/// `gap` under `resolved.props.layout`) and the caption style (`caption`: font, size, outline),
/// through the dispatcher, after `ui.theme_set` changed them.
#[test]
fn theme_get_reports_the_layout_fields_and_the_caption_tokens() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = app();
    let set = call(
        &mut app,
        "ui.theme_set",
        json!({ "theme": "regia-dark",
            "tokens": { "subtitle.size": 1.25, "subtitle.font": "Noto Sans Mono",
                "subtitle.outline": "#800080", "subtitle.outline-width": 3 },
            "types": { "Bar": { "props": { "height": 50, "padding_x": 12, "gap": 9 } } } }),
    );
    assert!(set["changed"].as_array().is_some_and(|c| !c.is_empty()), "{set}");
    let got = call(
        &mut app,
        "ui.theme_get",
        json!({ "theme": "regia-dark", "type": "Bar", "keys": ["subtitle.*"] }),
    );
    assert_eq!(
        got["resolved"]["props"]["layout"],
        json!({ "height": 50.0, "padding_x": 12.0, "gap": 9.0 })
    );
    let caption = &got["caption"];
    assert_eq!(caption["size"], json!(1.25), "{caption}");
    assert_eq!(caption["font"], "Noto Sans Mono");
    assert_eq!(caption["outline"], "#800080");
    assert_eq!(caption["outline_width"], json!(3.0));
    assert_eq!(got["tokens"]["subtitle.size"]["value"], json!(1.25));
}
