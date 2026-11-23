// SPDX-License-Identifier: MPL-2.0
//! Runtime theme switching (UI D15, ADR-865): text follows the theme through the cross-fade,
//! accessibility settings pick high contrast and instant swaps, and `ThemeMode` follows a scheme
//! preference. Headless: game time advances a fixed 100 ms per update (`motion.base` is 160 ms in
//! the built-in themes, so a swap is part-way after one update and settled after two). No clock
//! is read, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::theme_file::Scheme;
use regia_ui::theme_runtime::{KeepTextColor, SchemePreference, ThemeMode};
use regia_ui::{ActiveTheme, BackgroundKind, ThemeResolver, UiAccessibility};

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

struct Panel {
    button: Entity,
    label: Entity,
    warning: Entity,
    kept: Entity,
}

/// A themed button with a default-colored label, an authored red label and an opted-out label.
fn panel(app: &mut App) -> Panel {
    let world = app.world_mut();
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    let button = world
        .spawn((UiNode, Button, Classes::one("button"), ChildOf(root)))
        .id();
    let label = world.spawn((UiNode, Text::new("Play"), ChildOf(button))).id();
    let warning = world
        .spawn((
            UiNode,
            Text::new("!").with_style(TextStyle {
                color: UiColor([1.0, 0.0, 0.0, 1.0]),
                ..TextStyle::default()
            }),
            ChildOf(button),
        ))
        .id();
    let kept = world
        .spawn((UiNode, Text::new("kept"), KeepTextColor, ChildOf(button)))
        .id();
    Panel {
        button,
        label,
        warning,
        kept,
    }
}

fn span_color(app: &App, e: Entity) -> UiColor {
    app.world().get::<Text>(e).expect("text").spans[0].style.color
}

fn themed_text(app: &App, e: Entity) -> UiColor {
    app.world()
        .get::<Themed>(e)
        .and_then(|t| t.text_color)
        .expect("a resolved text color")
}

fn active(app: &App) -> String {
    app.world().resource::<ThemeResolver>().active().to_owned()
}

fn generation(app: &App) -> u64 {
    app.world().resource::<ThemeResolver>().generation()
}

/// Settles the first look (Themed is added one frame, resolved the next).
fn settle(app: &mut App) {
    for _ in 0..3 {
        app.update();
    }
}

fn strictly_between(v: UiColor, a: UiColor, b: UiColor) -> bool {
    (0..3).any(|i| a.0[i] != b.0[i])
        && (0..3).all(|i| {
            let (lo, hi) = if a.0[i] <= b.0[i] {
                (a.0[i], b.0[i])
            } else {
                (b.0[i], a.0[i])
            };
            if lo == hi {
                v.0[i] == lo
            } else {
                v.0[i] > lo && v.0[i] < hi
            }
        })
}

#[test]
fn a_label_in_a_plain_row_follows_the_theme_and_a_filled_box_keeps_its_own() {
    let mut app = app();
    let p = panel(&mut app);
    let row = app.world_mut().spawn((UiNode, ChildOf(p.button))).id();
    let caption = app
        .world_mut()
        .spawn((UiNode, Text::new("caption"), ChildOf(row)))
        .id();
    let filled = app
        .world_mut()
        .spawn((
            UiNode,
            Background {
                kind: BackgroundKind::Color(UiColor::BLACK),
            },
            ChildOf(p.button),
        ))
        .id();
    let on_black = app
        .world_mut()
        .spawn((UiNode, Text::new("on black"), ChildOf(filled)))
        .id();
    settle(&mut app);
    assert_eq!(span_color(&app, caption), themed_text(&app, p.button));
    assert_eq!(
        span_color(&app, on_black),
        TextStyle::default().color,
        "text on a box with its own fill was written for that fill"
    );
}

#[test]
fn a_label_spawned_after_the_look_settled_follows_the_theme() {
    // A list row spawned into a menu that is already open: the themed node's look does not change,
    // so only the new-label pass can color it (found by the accessibility audit, ADR-3025).
    let mut app = app();
    let p = panel(&mut app);
    let row = app.world_mut().spawn((UiNode, ChildOf(p.button))).id();
    settle(&mut app);
    let themed = themed_text(&app, p.button);
    assert_ne!(
        themed,
        TextStyle::default().color,
        "the theme's text color must differ to prove anything"
    );
    let late = app
        .world_mut()
        .spawn((UiNode, Text::new("late row"), ChildOf(row)))
        .id();
    let kept = app
        .world_mut()
        .spawn((UiNode, Text::new("kept"), KeepTextColor, ChildOf(row)))
        .id();
    let filled = app
        .world_mut()
        .spawn((
            UiNode,
            Background {
                kind: BackgroundKind::Color(UiColor::BLACK),
            },
            ChildOf(row),
        ))
        .id();
    let on_black = app
        .world_mut()
        .spawn((UiNode, Text::new("on black"), ChildOf(filled)))
        .id();
    let generation_before = generation(&app);
    app.update();
    assert_eq!(generation(&app), generation_before, "no look change drove this");
    assert_eq!(span_color(&app, late), themed);
    assert_eq!(span_color(&app, kept), TextStyle::default().color);
    assert_eq!(
        span_color(&app, on_black),
        TextStyle::default().color,
        "text on a box with its own fill was written for that fill"
    );
    // An explicit color on a late label stays.
    let red = UiColor([1.0, 0.0, 0.0, 1.0]);
    let authored = app
        .world_mut()
        .spawn((
            UiNode,
            Text::new("!").with_style(TextStyle {
                color: red,
                ..TextStyle::default()
            }),
            ChildOf(row),
        ))
        .id();
    app.update();
    assert_eq!(span_color(&app, authored), red);
}

#[test]
fn label_text_follows_the_theme_through_the_cross_fade() {
    let mut app = app();
    let p = panel(&mut app);
    settle(&mut app);
    let dark = span_color(&app, p.label);
    assert_eq!(
        dark,
        themed_text(&app, p.button),
        "the label takes the theme's text color"
    );
    assert_ne!(
        dark,
        TextStyle::default().color,
        "dark text is not the default white"
    );

    app.world_mut().resource_mut::<ActiveTheme>().name = "regia-light".to_owned();
    app.update();
    assert_eq!(active(&app), "regia-light");
    let mid = span_color(&app, p.label);
    assert_eq!(
        mid,
        themed_text(&app, p.button),
        "the label follows every fade frame"
    );
    app.update();
    app.update();
    let light = span_color(&app, p.label);
    assert!(
        strictly_between(mid, dark, light),
        "part-way after one 100 ms step of a 160 ms fade: {mid:?} between {dark:?} and {light:?}"
    );
    assert_eq!(light, themed_text(&app, p.button));
    assert_eq!(app.world().resource::<ThemeResolver>().stats().animating, 0);

    // Authored and opted-out colors never move.
    assert_eq!(span_color(&app, p.warning), UiColor([1.0, 0.0, 0.0, 1.0]));
    assert_eq!(span_color(&app, p.kept), TextStyle::default().color);
}

#[test]
fn switching_back_and_forth_mid_fade_ends_on_the_last_theme() {
    let mut app = app();
    let p = panel(&mut app);
    settle(&mut app);
    let dark = span_color(&app, p.label);
    app.world_mut().resource_mut::<ActiveTheme>().name = "regia-light".to_owned();
    app.update();
    app.world_mut().resource_mut::<ActiveTheme>().name = "regia-dark".to_owned();
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(active(&app), "regia-dark");
    assert_eq!(span_color(&app, p.label), dark);
    assert_eq!(app.world().resource::<ThemeResolver>().stats().animating, 0);
}

#[test]
fn idle_frames_never_bump_the_generation() {
    let mut app = app();
    panel(&mut app);
    settle(&mut app);
    let g = generation(&app);
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(generation(&app), g);
}

#[test]
fn the_players_high_contrast_setting_picks_the_high_contrast_theme() {
    let mut app = app();
    let p = panel(&mut app);
    settle(&mut app);
    let g = generation(&app);
    app.world_mut().resource_mut::<UiAccessibility>().high_contrast = true;
    app.update();
    assert_eq!(active(&app), "regia-high-contrast");
    assert_eq!(generation(&app), g + 1, "one generation for one setting change");
    for _ in 0..2 {
        app.update();
    }
    assert_eq!(span_color(&app, p.label), themed_text(&app, p.button));
    app.world_mut().resource_mut::<UiAccessibility>().high_contrast = false;
    app.update();
    assert_eq!(active(&app), "regia-dark");
}

#[test]
fn reduced_motion_makes_the_swap_instant() {
    let mut app = app();
    let p = panel(&mut app);
    settle(&mut app);
    app.world_mut().resource_mut::<UiAccessibility>().reduced_motion = true;
    app.update();
    assert!(app.world().resource::<ActiveTheme>().reduced_motion);
    app.world_mut().resource_mut::<ActiveTheme>().name = "regia-light".to_owned();
    app.update();
    assert_eq!(app.world().resource::<ThemeResolver>().stats().animating, 0);
    let now = span_color(&app, p.label);
    app.update();
    assert_eq!(span_color(&app, p.label), now, "already at the final color");
}

#[test]
fn a_host_preset_survives_the_first_accessibility_sync() {
    let mut app = app();
    app.world_mut().resource_mut::<ActiveTheme>().high_contrast = true;
    panel(&mut app);
    settle(&mut app);
    assert!(app.world().resource::<ActiveTheme>().high_contrast);
    assert_eq!(active(&app), "regia-high-contrast");
}

#[test]
fn theme_mode_follows_the_scheme_preference() {
    let mut app = app();
    panel(&mut app);
    settle(&mut app);
    // Not following: a host selection sticks.
    app.world_mut().resource_mut::<ActiveTheme>().name = "regia-light".to_owned();
    app.update();
    assert_eq!(active(&app), "regia-light");

    {
        let mut mode = app.world_mut().resource_mut::<ThemeMode>();
        mode.follow = Some(SchemePreference::System);
        mode.system_scheme = None;
    }
    app.update();
    assert_eq!(active(&app), "regia-dark", "an unknown OS scheme reads as dark");
    app.world_mut().resource_mut::<ThemeMode>().system_scheme = Some(Scheme::Light);
    app.update();
    assert_eq!(active(&app), "regia-light");
    let g = generation(&app);
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(generation(&app), g, "following does not re-select every frame");
    app.world_mut().resource_mut::<ThemeMode>().follow = Some(SchemePreference::Dark);
    app.update();
    assert_eq!(active(&app), "regia-dark");
}
