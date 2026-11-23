// SPDX-License-Identifier: MPL-2.0
//! Icon atlases, sizes, tinting and scaling rules (UI D16, ADR-865), end to end in a headless app.
//! No artwork ships with the engine, so the sheets here are generated white squares; no GPU, no
//! goldens, game time advances a fixed 100 ms per update.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::icons::{ICON_IMAGE_BASE, Icon, IconAtlas, IconPage, IconSheet, IconSize, IconState, IconTint};
use regia_ui::prelude::*;
use regia_ui::{ActiveTheme, SharedUiRenderer};

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TextSystem::embedded_only());
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app
}

/// A 64x32 page: `gear` at 16 (top-left) and 32 (right half), `close` at 24.
fn sheet() -> IconSheet {
    let page = IconPage {
        width: 64,
        height: 32,
        rgba8: vec![255; 64 * 32 * 4],
    };
    IconSheet {
        pages: vec![page],
        sprites: vec![
            regia_ui::icons::IconSprite {
                name: "gear@16".to_owned(),
                page: 0,
                rect: [0, 0, 16, 16],
            },
            regia_ui::icons::IconSprite {
                name: "gear@32".to_owned(),
                page: 0,
                rect: [32, 0, 32, 32],
            },
            regia_ui::icons::IconSprite {
                name: "close@24".to_owned(),
                page: 0,
                rect: [0, 8, 24, 24],
            },
        ],
    }
}

/// A root at 1 UI px = 1 physical px (the default scales to a 1920x1080 reference, and a
/// headless app has no window).
fn pixel_root() -> UiRoot {
    UiRoot {
        scale_mode: ScaleMode::ConstantPixelSize,
        ..UiRoot::default()
    }
}

fn state(app: &App, e: Entity) -> IconState {
    *app.world().get::<IconState>(e).expect("icon state")
}

fn settle(app: &mut App) {
    for _ in 0..3 {
        app.update();
    }
}

#[test]
fn an_icon_draws_its_best_drawing_tinted_by_the_theme() {
    let mut app = app();
    app.world_mut()
        .resource_mut::<IconAtlas>()
        .add_sheet(sheet())
        .expect("sheet");
    let root = app.world_mut().spawn((pixel_root(), UiNode)).id();
    let button = app
        .world_mut()
        .spawn((UiNode, Button, Classes::one("button"), ChildOf(root)))
        .id();
    let icon = app
        .world_mut()
        .spawn((
            UiNode,
            Icon::new("gear").with_size(IconSize::Large),
            ChildOf(button),
        ))
        .id();
    settle(&mut app);

    let s = state(&app, icon);
    assert!(!s.missing);
    assert_eq!(s.ui_px, 32.0, "size.icon.lg in the comfortable density");
    assert_eq!((s.variant_px, s.draw_px, s.upscaled), (32, 32.0, false));
    let image = *app.world().get::<UiImage>(icon).expect("image");
    assert_eq!(image.id, ICON_IMAGE_BASE);
    assert_eq!(image.uv, [0.5, 0.0, 1.0, 1.0]);
    let themed = *app.world().get::<Themed>(button).expect("themed button");
    let expected = themed.icon_color.or(themed.text_color).expect("a theme color");
    assert_eq!(image.tint, expected, "icons tint with their context's text color");
    let style = app.world().get::<NodeStyle>(icon).expect("style");
    assert_eq!(
        style.0.size,
        [Dimension::Points(32.0); 2],
        "laid out at the icon size"
    );

    // The page reached the renderer.
    assert!(app.world().resource::<IconAtlas>().pending_pages().is_empty());
    let size = app
        .world()
        .resource::<SharedUiRenderer>()
        .lock()
        .image_size(ICON_IMAGE_BASE);
    assert_eq!(size, Some([64, 32]));

    // A theme switch retints the icon (through the cross-fade, then settled).
    app.world_mut().resource_mut::<ActiveTheme>().name = "regia-light".to_owned();
    settle(&mut app);
    let themed = *app.world().get::<Themed>(button).expect("themed button");
    let light = themed.icon_color.or(themed.text_color).expect("a theme color");
    assert_ne!(light, expected);
    assert_eq!(app.world().get::<UiImage>(icon).expect("image").tint, light);
}

#[test]
fn sizes_follow_the_scaling_rules() {
    let mut app = app();
    app.world_mut()
        .resource_mut::<IconAtlas>()
        .add_sheet(sheet())
        .expect("sheet");
    let root = app.world_mut().spawn((pixel_root(), UiNode)).id();
    let spawn = |app: &mut App, icon: Icon| app.world_mut().spawn((UiNode, icon, ChildOf(root))).id();
    let crisp = spawn(&mut app, Icon::new("gear").with_size(IconSize::Px(16.5)));
    let down = spawn(&mut app, Icon::new("gear").with_size(IconSize::Px(20.0)));
    let up = spawn(&mut app, Icon::new("close").with_size(IconSize::Px(40.0)));
    let medium = spawn(&mut app, Icon::new("close"));
    settle(&mut app);
    assert_eq!(
        (state(&app, crisp).variant_px, state(&app, crisp).draw_px),
        (16, 16.0)
    );
    assert_eq!(
        (state(&app, down).variant_px, state(&app, down).draw_px),
        (32, 20.0)
    );
    let s = state(&app, up);
    assert_eq!((s.variant_px, s.draw_px, s.upscaled), (24, 40.0, true));
    let s = state(&app, medium);
    assert_eq!((s.ui_px, s.variant_px, s.upscaled), (24.0, 24, false));
}

#[test]
fn a_missing_icon_draws_nothing_and_says_so() {
    let mut app = app();
    let root = app.world_mut().spawn((pixel_root(), UiNode)).id();
    let icon = app
        .world_mut()
        .spawn((UiNode, Icon::new("gear"), ChildOf(root)))
        .id();
    settle(&mut app);
    assert!(state(&app, icon).missing);
    assert!(app.world().get::<UiImage>(icon).is_none());
    // Loading a sheet later makes it appear.
    app.world_mut()
        .resource_mut::<IconAtlas>()
        .add_sheet(sheet())
        .expect("sheet");
    settle(&mut app);
    assert!(!state(&app, icon).missing);
    assert!(app.world().get::<UiImage>(icon).is_some());
}

#[test]
fn fixed_and_original_tints_and_author_sizes_are_kept() {
    let mut app = app();
    app.world_mut()
        .resource_mut::<IconAtlas>()
        .add_sheet(sheet())
        .expect("sheet");
    let root = app.world_mut().spawn((pixel_root(), UiNode)).id();
    let red = UiColor([1.0, 0.0, 0.0, 1.0]);
    let fixed = app
        .world_mut()
        .spawn((
            UiNode,
            Icon::new("gear").with_tint(IconTint::Color(red)),
            ChildOf(root),
        ))
        .id();
    let original = app
        .world_mut()
        .spawn((
            UiNode,
            Icon::new("gear").with_tint(IconTint::Original),
            NodeStyle(Style {
                size: [Dimension::Points(48.0); 2],
                ..Style::default()
            }),
            ChildOf(root),
        ))
        .id();
    settle(&mut app);
    assert_eq!(app.world().get::<UiImage>(fixed).expect("image").tint, red);
    assert_eq!(
        app.world().get::<UiImage>(original).expect("image").tint,
        UiColor::WHITE
    );
    assert_eq!(
        app.world().get::<NodeStyle>(original).expect("style").0.size,
        [Dimension::Points(48.0); 2],
        "an author's size wins over the icon size"
    );
    // A size change follows on a node the icon sized itself.
    app.world_mut().get_mut::<Icon>(fixed).expect("icon").size = IconSize::Small;
    settle(&mut app);
    assert_eq!(
        app.world().get::<NodeStyle>(fixed).expect("style").0.size,
        [Dimension::Points(16.0); 2]
    );
}

#[test]
fn steady_frames_change_nothing() {
    let mut app = app();
    app.world_mut()
        .resource_mut::<IconAtlas>()
        .add_sheet(sheet())
        .expect("sheet");
    let root = app.world_mut().spawn((pixel_root(), UiNode)).id();
    let icon = app
        .world_mut()
        .spawn((UiNode, Icon::new("gear"), ChildOf(root)))
        .id();
    settle(&mut app);
    let tick = app
        .world()
        .entity(icon)
        .get_change_ticks::<UiImage>()
        .expect("ticks")
        .changed;
    for _ in 0..3 {
        app.update();
    }
    let after = app
        .world()
        .entity(icon)
        .get_change_ticks::<UiImage>()
        .expect("ticks")
        .changed;
    assert_eq!(tick, after, "no write without a change");
}
