// SPDX-License-Identifier: MPL-2.0
//! Accessibility settings and right-to-left layout (ADR-341): the text scale multiplies measured
//! text, `UiAccessibility` carries the flags, and `text_dir` / the locale mirror row layouts.

use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::style::{Align, FlexDirection, TextDir};
use regia_ui::{UiAccessibility, UiDevicePolicy, UiScales};

fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    // No device floor: the test measures the scale alone.
    app.insert_resource(UiDevicePolicy::DESKTOP);
    app
}

fn root(world: &mut World, direction: FlexDirection, text_dir: TextDir) -> Entity {
    world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                direction,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                align_items: Align::Start,
                text_dir,
                ..Default::default()
            }),
        ))
        .id()
}

fn label(world: &mut World, parent: Entity, content: &str, font_px: f32) -> Entity {
    let e = world
        .spawn((
            UiNode,
            Text::new(content).with_style(TextStyle {
                font_px,
                ..Default::default()
            }),
        ))
        .id();
    world.entity_mut(parent).add_child(e);
    e
}

fn boxed(world: &mut World, parent: Entity, w: f32) -> Entity {
    let e = world
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(w), Dimension::Points(20.0)],
                ..Default::default()
            }),
        ))
        .id();
    world.entity_mut(parent).add_child(e);
    e
}

fn settle(app: &mut App) {
    for _ in 0..3 {
        app.update();
    }
}

fn rect(app: &App, e: Entity) -> [f32; 4] {
    app.world().get::<ComputedNode>(e).expect("laid out").rect
}

#[test]
fn text_scale_grows_measured_text_and_stays_inside_the_window() {
    let mut app = ui_app();
    app.update();
    let r = root(app.world_mut(), FlexDirection::Column, TextDir::Ltr);
    let hud = label(app.world_mut(), r, "Score 12345", 20.0);
    settle(&mut app);
    let base = app.world().get::<ShapedText>(hud).expect("shaped").size;
    let base_rect = rect(&app, hud);
    assert!(base[1] > 0.0 && base[0] > 0.0);

    app.world_mut().resource_mut::<UiAccessibility>().text_scale = 1.5;
    settle(&mut app);
    let big = app.world().get::<ShapedText>(hud).expect("reshaped").size;
    for (b, g) in base.iter().zip(big) {
        let ratio = g / b;
        assert!(
            (ratio - 1.5).abs() < 0.08,
            "ratio {ratio} (base {base:?}, scaled {big:?})"
        );
    }
    let window = app.world().resource::<UiScales>().window_size;
    let r = rect(&app, hud);
    assert!(
        r[0] >= 0.0 && r[1] >= 0.0 && r[0] + r[2] <= window[0] && r[1] + r[3] <= window[1],
        "{r:?} in {window:?}"
    );
    assert!(r[3] > base_rect[3], "the node itself grew");

    // Back to 1.0 restores the original measurement.
    app.world_mut().resource_mut::<UiAccessibility>().text_scale = 1.0;
    settle(&mut app);
    let again = app.world().get::<ShapedText>(hud).expect("reshaped").size;
    assert_eq!(again, base);
}

#[test]
fn text_scale_is_clamped_and_the_device_floor_still_applies() {
    let mut app = ui_app();
    app.update();
    let r = root(app.world_mut(), FlexDirection::Column, TextDir::Ltr);
    let hud = label(app.world_mut(), r, "Tiny", 8.0);
    settle(&mut app);
    // 100x is limited to the 3x maximum.
    let base = app.world().get::<ShapedText>(hud).expect("shaped").size[1];
    app.world_mut().resource_mut::<UiAccessibility>().text_scale = 100.0;
    settle(&mut app);
    assert_eq!(
        app.world().resource::<TextSystem>().text_scale,
        UiAccessibility::MAX_TEXT_SCALE
    );
    let huge = app.world().get::<ShapedText>(hud).expect("reshaped").size[1];
    assert!((huge / base - 3.0).abs() < 0.3, "{huge} vs {base}");
    // A shrink below the handheld floor keeps the 12 px floor.
    app.insert_resource(UiDevicePolicy::HANDHELD);
    app.world_mut().resource_mut::<UiAccessibility>().text_scale = 0.5;
    settle(&mut app);
    let floored = app.world().get::<ShapedText>(hud).expect("reshaped").size[1];
    let unfloored_half = base * 0.5;
    assert!(
        floored > unfloored_half,
        "{floored} should exceed {unfloored_half} (12 px floor)"
    );
}

#[test]
fn rtl_text_dir_lays_a_row_out_right_to_left() {
    let mut app = ui_app();
    app.update();
    let r = root(app.world_mut(), FlexDirection::Row, TextDir::Rtl);
    let a = boxed(app.world_mut(), r, 100.0);
    let b = boxed(app.world_mut(), r, 60.0);
    settle(&mut app);
    let window = app.world().resource::<UiScales>().window_size;
    let (ra, rb) = (rect(&app, a), rect(&app, b));
    // The first child hugs the right edge, the second sits to its left.
    assert!((ra[0] + ra[2] - window[0]).abs() < 0.5, "{ra:?} in {window:?}");
    assert!((rb[0] + rb[2] - ra[0]).abs() < 0.5, "{rb:?} then {ra:?}");
}

#[test]
fn ltr_row_is_unchanged_and_explicit_ltr_beats_an_rtl_locale() {
    let mut app = ui_app();
    app.update();
    let r = root(app.world_mut(), FlexDirection::Row, TextDir::Ltr);
    let a = boxed(app.world_mut(), r, 100.0);
    let b = boxed(app.world_mut(), r, 60.0);
    settle(&mut app);
    assert!(rect(&app, a)[0].abs() < 0.5);
    assert!((rect(&app, b)[0] - 100.0).abs() < 0.5);
    // An Arabic locale does not mirror a node that says Ltr.
    {
        let mut loc = app.world_mut().resource_mut::<Localization>();
        loc.selected = "ar".to_owned();
        loc.generation.0 += 1;
    }
    settle(&mut app);
    assert!(rect(&app, a)[0].abs() < 0.5);
}

#[test]
fn auto_text_dir_follows_the_locale() {
    let mut app = ui_app();
    app.update();
    let r = root(app.world_mut(), FlexDirection::Row, TextDir::Auto);
    let a = boxed(app.world_mut(), r, 100.0);
    settle(&mut app);
    assert!(rect(&app, a)[0].abs() < 0.5, "LTR by default");
    {
        let mut loc = app.world_mut().resource_mut::<Localization>();
        loc.selected = "ar-EG".to_owned();
        loc.generation.0 += 1;
    }
    settle(&mut app);
    let window = app.world().resource::<UiScales>().window_size;
    let ra = rect(&app, a);
    assert!((ra[0] + ra[2] - window[0]).abs() < 0.5, "{ra:?} in {window:?}");
    {
        let mut loc = app.world_mut().resource_mut::<Localization>();
        loc.selected = "en-US".to_owned();
        loc.generation.0 += 1;
    }
    settle(&mut app);
    assert!(rect(&app, a)[0].abs() < 0.5, "back to LTR");
}

#[test]
fn rtl_mirrors_start_padding_to_the_right() {
    let mut app = ui_app();
    app.update();
    let r = app
        .world_mut()
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeStyle::new(Style {
                direction: FlexDirection::Row,
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                padding: Edges {
                    left: Dimension::Points(40.0),
                    ..Default::default()
                },
                align_items: Align::Start,
                text_dir: TextDir::Rtl,
                ..Default::default()
            }),
        ))
        .id();
    let a = boxed(app.world_mut(), r, 100.0);
    settle(&mut app);
    let window = app.world().resource::<UiScales>().window_size;
    let ra = rect(&app, a);
    // The "start" padding is now on the right: the child stops 40 px short of the right edge.
    assert!(
        (ra[0] + ra[2] - (window[0] - 40.0)).abs() < 0.5,
        "{ra:?} in {window:?}"
    );
}
