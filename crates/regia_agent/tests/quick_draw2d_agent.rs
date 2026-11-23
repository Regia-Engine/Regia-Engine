// SPDX-License-Identifier: MPL-2.0
//! `quick.draw2d_stats` through the dispatcher (P-05): without `Draw2dPlugin` it is a stable
//! `F0003`; with it, a frame drawn through `ResMut<Draw2d>` reports its instances per kind, the
//! glyph atlas pages and cached glyphs, and the bytes the layer uploaded. CPU only: no device (the
//! layer has drawn nothing, so `bytes_uploaded` is 0 and `layer_frames` stays 0).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_agent::protocol::RpcError;
use regia_agent::{LocalReply, call_local};
use regia_app::prelude::*;
use regia_core::math::{Color, Vec2};
use regia_quick::draw2d::{Draw2d, Draw2dPlugin, PxRect};
use regia_ui::Localization;
use regia_ui::text::{Text, TextSpan, TextStyle, TextSystem};
use serde_json::{Value, json};

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
    match call_local(world, method, params, Some("draw2d-test")) {
        LocalReply::Done(result) => result,
        LocalReply::Pending(_) => panic!("{method} deferred"),
    }
}

fn hud(
    mut draw: ResMut<Draw2d>,
    mut fonts: ResMut<TextSystem>,
    mut shaped: Local<Option<regia_ui::text::ShapedText>>,
) {
    let shaped = shaped.get_or_insert_with(|| {
        let text = Text {
            spans: vec![TextSpan::lit("HP 87").with_style(TextStyle {
                font_px: 12.0,
                ..Default::default()
            })],
            ..Default::default()
        };
        fonts.shape(&text, 1.0, None, &Localization::default())
    });
    draw.push_box(PxRect::new(8.0, 8.0, 100.0, 12.0), Color::GREEN, 3.0);
    draw.push_box(PxRect::new(8.0, 24.0, 100.0, 12.0), Color::RED, 0.0);
    draw.push_segment(Vec2::new(0.0, 0.0), Vec2::new(50.0, 50.0), 2.0, Color::WHITE);
    draw.push_text(&mut fonts, shaped, Vec2::new(8.0, 40.0), Color::WHITE);
}

#[test]
fn quick_draw2d_stats_reports_the_frame() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(regia_agent::AgentPermissions::default());
    let missing = call(app.world_mut(), "quick.draw2d_stats", json!({})).expect_err("no Draw2d yet");
    assert_eq!(missing.data.as_ref().unwrap()["code"], "F0003", "{missing:?}");

    app.insert_resource(TextSystem::embedded_only())
        .add_plugins(Draw2dPlugin)
        .add_systems(Update, hud);
    app.finish_plugins();
    for _ in 0..3 {
        app.update();
    }
    let stats = call(app.world_mut(), "quick.draw2d_stats", json!({})).expect("stats");
    assert_eq!(stats["frame"], 3, "{stats}");
    assert_eq!(stats["by_kind"]["box"], 2, "{stats}");
    assert_eq!(stats["by_kind"]["segment"], 1, "{stats}");
    let glyphs = stats["by_kind"]["glyph"].as_u64().unwrap();
    assert!(glyphs >= 4, "HP 87 has at least four inked glyphs: {stats}");
    assert_eq!(stats["instances"].as_u64().unwrap(), 3 + glyphs, "{stats}");
    assert!(stats["atlas_pages"].as_u64().unwrap() >= 1, "{stats}");
    assert_eq!(stats["glyphs_cached"].as_u64().unwrap(), glyphs, "{stats}");
    // Cached glyphs: only the first frame rasterized them (the space too, once, if the shaper
    // emits one).
    let rasterized = stats["glyphs_rasterized"].as_u64().unwrap();
    assert!((glyphs..=glyphs + 1).contains(&rasterized), "{stats}");
    assert_eq!(stats["bytes_uploaded"], 0, "{stats}");
    assert_eq!(stats["layer_frames"], 0, "{stats}");
}

#[derive(Resource)]
struct Ammo(u32);

/// A static panel and bar plus an ammo count in the bottom-right tile.
fn ammo_hud(
    mut draw: ResMut<Draw2d>,
    mut fonts: ResMut<TextSystem>,
    ammo: Res<Ammo>,
    mut shaped: Local<Option<(u32, regia_ui::text::ShapedText)>>,
) {
    if shaped.as_ref().is_none_or(|(n, _)| *n != ammo.0) {
        let text = Text {
            spans: vec![TextSpan::lit(ammo.0.to_string()).with_style(TextStyle {
                font_px: 20.0,
                ..Default::default()
            })],
            ..Default::default()
        };
        *shaped = Some((ammo.0, fonts.shape(&text, 1.0, None, &Localization::default())));
    }
    draw.push_box(PxRect::new(8.0, 8.0, 120.0, 40.0), Color::BLACK, 4.0);
    draw.push_box(PxRect::new(10.0, 200.0, 100.0, 12.0), Color::GREEN, 3.0);
    if let Some((_, run)) = shaped.as_ref() {
        draw.push_text(&mut fonts, run, Vec2::new(440.0, 210.0), Color::WHITE);
    }
}

/// Draws the staged frame with the plugin's own layer into a fresh 512 x 256 target (what the
/// window overlay does each frame), headless.
fn draw_layer(app: &mut App, device: &regia_render::RenderDevice) {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("draw2d agent target"),
        size: wgpu::Extent3d {
            width: 512,
            height: 256,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let mut encoder = device.device.create_command_encoder(&Default::default());
    {
        let overlay = app
            .world()
            .resource::<regia_quick::draw2d::Draw2dOverlay>()
            .clone();
        let mut shared = overlay.lock();
        let regia_quick::draw2d::Draw2dShared { frame, layer } = &mut *shared;
        layer.draw_frame(
            device,
            &mut encoder,
            &view,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            512,
            256,
            frame,
        );
    }
    device.queue.submit([encoder.finish()]);
}

/// `quick.draw2d_stats` reports the layer's dirty regions (P-06): a steady frame redraws no tile
/// and uploads nothing; a changed count redraws one run. Headless (lavapipe in CI).
#[test]
fn quick_draw2d_stats_reports_dirty_tiles() {
    let device = regia_render::RenderDevice::headless().expect("headless device (lavapipe)");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(regia_agent::AgentPermissions::default())
        .insert_resource(TextSystem::embedded_only())
        .insert_resource(Ammo(30))
        .add_plugins(Draw2dPlugin)
        .add_systems(Update, ammo_hud);
    app.finish_plugins();
    for _ in 0..3 {
        app.update();
        draw_layer(&mut app, &device);
    }
    app.update();
    let stats = call(app.world_mut(), "quick.draw2d_stats", json!({})).expect("stats");
    let tiles = &stats["tiles"];
    assert_eq!(tiles["dirty_regions"], true, "{stats}");
    assert_eq!(tiles["tiles"], 2 * 4, "512 x 256 in 256 x 64 tiles: {stats}");
    assert_eq!(tiles["dirty"], 0, "a steady HUD redraws no tile: {stats}");
    assert_eq!(tiles["runs"], 0, "{stats}");
    assert_eq!(tiles["painted"], 3, "{stats}");
    assert_eq!(
        tiles["painted_runs"], 2,
        "row 0 (the panel), row 3 (bar and count side by side): {stats}"
    );
    assert!(tiles["clean_frames"].as_u64().unwrap() >= 1, "{stats}");
    assert_eq!(tiles["redrawn_total"], 8, "only the first frame drew: {stats}");
    assert_eq!(stats["bytes_uploaded"], 0, "{stats}");

    app.world_mut().resource_mut::<Ammo>().0 = 29;
    draw_layer(&mut app, &device);
    app.update();
    draw_layer(&mut app, &device);
    app.update();
    let stats = call(app.world_mut(), "quick.draw2d_stats", json!({})).expect("stats");
    assert_eq!(stats["tiles"]["dirty"], 1, "the count's tile only: {stats}");
    assert_eq!(stats["tiles"]["runs"], 1, "{stats}");
}
