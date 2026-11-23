// SPDX-License-Identifier: MIT-0
//! Offscreen UI captures for by-eye review and tests: drives a headless app at a chosen window
//! size, then draws its UI overlay into an sRGB texture (optionally over a backdrop frame, the
//! way the overlay composites over the tonemapped 3D frame) and reads the pixels back.

use regia::prelude::*;
use regia_render::RenderDevice;
use regia_ui::render::SharedUiRenderer;
use regia_ui::{LayoutDirty, OffscreenTarget, UiRoot, UiScales};

/// The review resolutions: Steam Deck, 1080p, 1440p, 4K.
pub const REVIEW_SIZES: [(&str, u32, u32); 4] = [
    ("800p", 1280, 800),
    ("1080p", 1920, 1080),
    ("1440p", 2560, 1440),
    ("4k", 3840, 2160),
];

/// A headless app with the platform messages the UI systems read.
pub fn headless_app(plugin: impl Plugin) -> App {
    let mut app = App::new();
    app.add_plugins(plugin);
    app.add_message::<WindowResized>()
        .add_message::<WindowScaleFactorChanged>()
        .add_message::<CursorMoved>()
        .add_message::<MouseButtonInput>()
        .add_message::<TouchInput>()
        .add_message::<MouseWheel>()
        .add_message::<KeyboardInput>()
        .add_message::<ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(50),
    ));
    app
}

/// Sets the UI window size (physical px) and relayouts; `scale_mode` overrides every root's
/// scale mode when set (the gallery is authored at constant pixel size).
pub fn resize(app: &mut App, width: u32, height: u32, scale_mode: Option<ScaleMode>) {
    let world = app.world_mut();
    world.insert_resource(UiScales {
        window_size: [width as f32, height as f32],
        scale_factor: 1.0,
    });
    if let Some(mode) = scale_mode {
        for mut root in world.query::<&mut UiRoot>().iter_mut(world) {
            root.scale_mode = mode;
        }
    }
    if let Some(mut dirty) = world.get_resource_mut::<LayoutDirty>() {
        dirty.0 = true;
    }
    for _ in 0..4 {
        app.update();
    }
}

/// Draws the app's extracted UI into a `width`×`height` sRGB target that first holds `backdrop`
/// (tightly packed sRGB RGBA8, same size) or a dark clear, and returns the pixels.
pub fn capture(
    app: &App,
    device: &RenderDevice,
    width: u32,
    height: u32,
    backdrop: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let shared = app
        .world()
        .get_resource::<SharedUiRenderer>()
        .ok_or("app has no SharedUiRenderer (UiPlugin missing)")?
        .clone();
    let target = OffscreenTarget::new(device, width, height);
    if !backdrop.is_some_and(|b| target.upload(device, b)) {
        target.clear(device, [0.006, 0.007, 0.009, 1.0]);
    }
    target.draw_ui(device, &mut shared.lock());
    target.read(device)
}
