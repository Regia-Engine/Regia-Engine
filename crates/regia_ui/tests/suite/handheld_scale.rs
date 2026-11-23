// SPDX-License-Identifier: MPL-2.0
//! 800p / 16:10 UI scaling (ADR-526): a UI authored against the default 1920×1080 reference is
//! fitted to the Steam Deck's 1280×800 panel under the handheld policy, and keeps the author's blend
//! on desktops.

use regia_app::prelude::*;
use regia_ui::UiDevicePolicy;
use regia_ui::prelude::*;

/// An app with `UiPlugin` and the platform messages its systems read, under `policy`.
fn ui_app(policy: UiDevicePolicy) -> App {
    let mut app = App::new();
    app.insert_resource(policy);
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app
}

/// Lays out a root with the default (1920×1080) reference and a bar exactly as wide as the
/// reference, in a 1280×800 window. Returns (root scale, bar width in physical px).
fn layout_on_the_deck(policy: UiDevicePolicy) -> (f32, f32) {
    let mut app = ui_app(policy);
    let root = app
        .world_mut()
        .spawn((
            UiRoot::default(),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id();
    let bar = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(1920.0), Dimension::Points(64.0)],
                // Keep the authored width, so an overflow shows instead of being squeezed away.
                flex_shrink: 0.0,
                ..Default::default()
            }),
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(bar);
    for _ in 0..3 {
        app.world_mut().write_message(regia_platform::WindowResized {
            window: Entity::PLACEHOLDER,
            width: 1280,
            height: 800,
        });
        app.update();
    }
    let world = app.world();
    let root = world.get::<ComputedNode>(root).expect("root laid out");
    let bar = world.get::<ComputedNode>(bar).expect("bar laid out");
    (root.scale, bar.rect[2])
}

#[test]
fn handheld_policy_fits_a_16_9_layout_to_the_16_10_panel() {
    let (scale, width) = layout_on_the_deck(UiDevicePolicy::HANDHELD);
    assert!((scale - 1280.0 / 1920.0).abs() < 1e-4, "{scale}");
    assert!(
        (width - 1280.0).abs() < 0.5,
        "the reference-wide bar fills the panel: {width}"
    );
}

#[test]
fn desktop_policy_keeps_the_authored_blend() {
    let (scale, width) = layout_on_the_deck(UiDevicePolicy::DESKTOP);
    let blend = ScaleMode::default().scale_for([1280.0, 800.0], 1.0);
    assert!((scale - blend).abs() < 1e-4, "{scale} vs {blend}");
    assert!(
        width > 1340.0,
        "without the fit the bar runs past the panel: {width}"
    );
}
