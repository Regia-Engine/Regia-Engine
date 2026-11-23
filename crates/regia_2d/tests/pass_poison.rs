// SPDX-License-Identifier: MPL-2.0
//! The shared sprite-pass state under concurrency (audit 11-03): it survives a panic while its
//! lock was held (H1: extraction, statistics and the overlay draw keep working instead of skipping
//! every later frame), and the cull size covers every window drawn (L2). The draws run on a real
//! device (lavapipe in CI).

use std::sync::Arc;

use regia_2d::atlas::{RgbaImage, SpriteAtlas};
use regia_2d::{Sprite, Sprite2dPlugin, SpriteAssets, SpritePass, sprite_overlay};
use regia_app::prelude::*;
use regia_core::math::{UVec2, Vec3};
use regia_render::RenderDevice;
use regia_scene::Transform;

fn one_sprite_atlas() -> SpriteAtlas {
    SpriteAtlas::single(
        "dot",
        RgbaImage {
            width: 4,
            height: 4,
            rgba8: vec![255; 4 * 4 * 4],
        },
    )
}

/// Poisons the pass mutex the way a panic inside the render-thread draw would.
fn poison(pass: &SpritePass) {
    let shared = Arc::clone(&pass.0);
    let _ = std::thread::spawn(move || {
        let _guard = shared.lock();
        panic!("simulated panic while the sprite pass state is locked");
    })
    .join();
    assert!(pass.0.is_poisoned(), "the helper poisoned the lock");
}

#[test]
fn a_panic_under_the_pass_lock_does_not_stop_2d_for_good() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, Sprite2dPlugin));
    app.world_mut()
        .resource_mut::<SpriteAssets>()
        .insert_atlas("a", one_sprite_atlas());
    app.world_mut().spawn((
        Sprite::named("a", "dot"),
        Transform::from_translation(Vec3::new(2.0, 2.0, 0.0)),
    ));
    let pass = app.world().resource::<SpritePass>().clone();
    poison(&pass);

    app.update();
    assert_eq!(
        pass.stats().sprites,
        1,
        "extraction still hands the frame over after the poison"
    );

    let device = RenderDevice::headless().expect("headless GPU (lavapipe)");
    let size = UVec2::new(16, 16);
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pass poison target"),
        size: wgpu::Extent3d {
            width: size.x,
            height: size.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let overlay = sprite_overlay(&pass);
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    overlay
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .draw(
            &device,
            &mut encoder,
            &view,
            wgpu::TextureFormat::Rgba8Unorm,
            size.x,
            size.y,
        );
    device.queue.submit([encoder.finish()]);
    let state = pass.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(state.last_window, Some(size), "the overlay skipped its draw");
    assert!(state.renderer.is_some(), "the overlay never built its renderer");
}

fn draw_once(device: &RenderDevice, overlay: &regia_render::SharedOverlay, size: UVec2) {
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pass window target"),
        size: wgpu::Extent3d {
            width: size.x,
            height: size.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    overlay
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .draw(
            device,
            &mut encoder,
            &view,
            wgpu::TextureFormat::Rgba8Unorm,
            size.x,
            size.y,
        );
    device.queue.submit([encoder.finish()]);
}

/// Two windows draw the same 2D frame (a small tool view and the game view): extraction must cull
/// against a size covering both, not whichever window happened to draw last (audit 11-03 L2).
#[test]
fn the_cull_size_covers_every_window_drawn_since_the_last_extraction() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_scene::ScenePlugin, Sprite2dPlugin));
    let pass = app.world().resource::<SpritePass>().clone();
    let device = RenderDevice::headless().expect("headless GPU (lavapipe)");
    // One overlay layer draws every window, as the plugin installs it.
    let overlay = sprite_overlay(&pass);
    draw_once(&device, &overlay, UVec2::new(64, 32));
    draw_once(&device, &overlay, UVec2::new(16, 48));
    let seen = pass
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .last_window;
    assert_eq!(seen, Some(UVec2::new(64, 48)), "only the last window counts");
}
