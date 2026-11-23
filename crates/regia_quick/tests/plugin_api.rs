// SPDX-License-Identifier: MPL-2.0
//! `QuickPlugin` settings, image registry and frozen-frame behaviour inside a headless app.

use std::sync::Arc;

use regia_platform::{PrimaryWindow, Window};
use regia_quick::prelude::*;
use regia_quick::{QuickDrawStats, QuickOverlay};
use regia_render::{ImmediateImage, ImmediateImageFormat, RenderPlugin};

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(RenderPlugin { headless: true })
        .add_plugins(QuickPlugin);
    app.finish_plugins();
    app
}

fn mask(width: u32, height: u32) -> ImmediateImage {
    ImmediateImage {
        width,
        height,
        format: ImmediateImageFormat::R8,
        data: Arc::from(vec![255u8; (width * height) as usize]),
    }
}

#[test]
fn settings_default_to_four_samples_and_a_720p_fallback() {
    let settings = QuickSettings::default();
    assert_eq!(settings.samples, 4);
    assert_eq!(settings.fallback.size, Vec2::new(1280.0, 720.0));
    assert_eq!(settings.fallback.scale_factor, 1.0);
}

#[test]
fn zero_samples_are_raised_to_one_and_changes_apply_each_frame() {
    let mut app = app();
    app.world_mut().resource_mut::<QuickSettings>().samples = 0;
    app.update();
    assert_eq!(app.world().resource::<QuickOverlay>().lock().samples, 1);
    app.world_mut().resource_mut::<QuickSettings>().samples = 8;
    app.update();
    assert_eq!(app.world().resource::<QuickOverlay>().lock().samples, 8);
}

#[test]
fn samples_still_follow_the_settings_while_frozen_but_the_frame_is_kept() {
    let mut app = app();
    app.add_systems(Update, |draw: Res<Draw>| {
        draw.rect().w_h(10.0, 10.0);
    });
    app.update();
    let frame = app.world().resource::<QuickDrawStats>().frame;
    app.world_mut().resource_mut::<DrawFrozen>().0 = true;
    app.world_mut().resource_mut::<QuickSettings>().samples = 2;
    app.update();
    assert_eq!(app.world().resource::<QuickOverlay>().lock().samples, 2);
    assert_eq!(
        app.world().resource::<QuickDrawStats>().frame,
        frame,
        "no new frame was built"
    );
    assert!(!app.world().resource::<QuickOverlay>().lock().batch.is_empty());
    // Frozen frames also keep the scale factor the batch was built for.
    assert_eq!(app.world().resource::<QuickOverlay>().lock().scale_factor, 1.0);
}

#[test]
fn the_overlay_takes_its_scale_factor_from_the_primary_window() {
    let mut app = app();
    app.world_mut().spawn((
        Window {
            physical_size: (800, 400),
            scale_factor: 2.0,
            ..Window::default()
        },
        PrimaryWindow,
    ));
    app.update();
    assert_eq!(app.world().resource::<QuickOverlay>().lock().scale_factor, 2.0);
}

#[test]
fn a_zero_sized_window_falls_back_to_the_settings() {
    let mut app = app();
    app.world_mut().resource_mut::<QuickSettings>().fallback = regia_quick::FrameParams {
        size: Vec2::new(200.0, 100.0),
        scale_factor: 1.5,
    };
    app.world_mut().spawn((
        Window {
            physical_size: (0, 0),
            scale_factor: 3.0,
            ..Window::default()
        },
        PrimaryWindow,
    ));
    app.update();
    assert_eq!(app.world().resource::<QuickOverlay>().lock().scale_factor, 1.5);
}

#[test]
fn images_register_update_and_unregister_through_extraction() {
    let mut app = app();
    let (a, b) = {
        let mut images = app.world_mut().resource_mut::<QuickImages>();
        (images.add(mask(2, 2)), images.add(mask(4, 4)))
    };
    assert_ne!(a, b, "ids are unique");
    let has = |app: &App, image: QuickImage| {
        app.world()
            .resource::<QuickOverlay>()
            .lock()
            .renderer
            .has_image(image.0)
    };
    assert!(!has(&app, a), "nothing is uploaded before a frame is extracted");
    app.update();
    assert!(has(&app, a) && has(&app, b));

    app.world_mut()
        .resource_mut::<QuickImages>()
        .update(a, mask(8, 8));
    app.world_mut().resource_mut::<QuickImages>().remove(b);
    app.update();
    assert!(has(&app, a), "update keeps the id registered");
    assert!(!has(&app, b), "remove unregisters it");

    // Removing twice and updating a removed id are harmless.
    app.world_mut().resource_mut::<QuickImages>().remove(b);
    app.update();
    assert!(!has(&app, b));
}

#[test]
fn image_ids_never_repeat_even_after_removal() {
    let mut app = app();
    let mut images = app.world_mut().resource_mut::<QuickImages>();
    let first = images.add(mask(1, 1));
    images.remove(first);
    let second = images.add(mask(1, 1));
    assert_ne!(first, second);
}
