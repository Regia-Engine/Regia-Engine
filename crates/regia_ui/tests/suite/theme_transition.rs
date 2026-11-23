// SPDX-License-Identifier: MPL-2.0
//! Theme-swap transitions (UI D15, ADR-1309): the swap fade follows its curve and blends in
//! OKLab, `ThemeTransition` shapes the next swap only, a state change takes a node off the swap
//! fade, `ThemeChanged` / `ThemeFadeFinished` bracket every swap, and the OS scheme the platform
//! reports reaches `ThemeMode { follow: System }`.
//!
//! Headless: game time advances a fixed 100 ms per update (`motion.base` is 160 ms in the
//! built-in themes). No clock is read and no golden is needed: colors are compared with the
//! settled colors of the same run. Fade lengths that are whole multiples of the step (400 ms) are
//! avoided where a test counts frames: in f32 the countdown leaves a 1e-8 s residue and takes one
//! more frame, so the tests use 350 ms (four steps).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::theme_color_ops::to_oklab;
use regia_ui::theme_runtime::{
    BlendSpace, SchemePreference, ThemeChanged, ThemeFadeFinished, ThemeMode, ThemeTransition, ease_progress,
};
use regia_ui::theme_tokens::Easing;
use regia_ui::{ActiveTheme, ThemeResolver, UiAccessibility};

/// Every theme message, in the order the frames wrote them.
#[derive(Resource, Default)]
struct Log {
    changed: Vec<ThemeChanged>,
    finished: Vec<ThemeFadeFinished>,
}

fn collect(
    mut changed: MessageReader<ThemeChanged>,
    mut finished: MessageReader<ThemeFadeFinished>,
    mut log: ResMut<Log>,
) {
    log.changed.extend(changed.read().cloned());
    log.finished.extend(finished.read().cloned());
}

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
    app.init_resource::<Log>().add_systems(Update, collect);
    app
}

/// A root with one themed button; returns the button.
fn button(app: &mut App) -> Entity {
    let world = app.world_mut();
    let root = world.spawn((UiRoot::default(), UiNode)).id();
    world
        .spawn((UiNode, Button, Classes::one("button"), ChildOf(root)))
        .id()
}

/// Settles the first look (Themed is added one frame, resolved the next).
fn settle(app: &mut App) {
    for _ in 0..3 {
        app.update();
    }
}

fn background(app: &App, e: Entity) -> UiColor {
    app.world()
        .get::<Themed>(e)
        .and_then(|t| t.background)
        .expect("a resolved background")
}

fn resolver(app: &App) -> &ThemeResolver {
    app.world().resource::<ThemeResolver>()
}

fn select(app: &mut App, name: &str) {
    app.world_mut().resource_mut::<ActiveTheme>().name = name.to_owned();
}

fn set_transition(app: &mut App, transition: ThemeTransition) {
    *app.world_mut().resource_mut::<ThemeTransition>() = transition;
}

/// How far `mid` is from `a` to `b` in OKLab lightness (0 = `a`, 1 = `b`).
fn lightness_fraction(mid: UiColor, a: UiColor, b: UiColor) -> f32 {
    let (l0, l1) = (to_oklab(a).l, to_oklab(b).l);
    (to_oklab(mid).l - l0) / (l1 - l0)
}

/// The settled dark and light backgrounds of the button, measured in this run.
fn endpoints(app: &mut App, b: Entity) -> (UiColor, UiColor) {
    let dark = background(app, b);
    set_transition(
        app,
        ThemeTransition {
            duration: Some(0.0),
            ..ThemeTransition::default()
        },
    );
    select(app, "regia-light");
    app.update();
    let light = background(app, b);
    select(app, "regia-dark");
    app.update();
    assert_eq!(background(app, b), dark, "an instant swap lands exactly");
    (dark, light)
}

#[test]
fn a_linear_oklab_swap_brightens_evenly() {
    let mut app = app();
    let b = button(&mut app);
    settle(&mut app);
    let (dark, light) = endpoints(&mut app, b);
    assert!(
        (to_oklab(light).l - to_oklab(dark).l) > 0.3,
        "the two schemes differ enough to measure"
    );

    set_transition(
        &mut app,
        ThemeTransition {
            duration: Some(0.35),
            ease: Some(Easing::Linear),
            space: BlendSpace::Oklab,
        },
    );
    select(&mut app, "regia-light");
    for step in 1..=3 {
        app.update();
        let f = lightness_fraction(background(&app, b), dark, light);
        let expected = step as f32 * 0.1 / 0.35;
        assert!(
            (f - expected).abs() < 0.01,
            "step {step}: OKLab lightness {f} of the way, expected {expected}"
        );
        assert!(resolver(&app).fading(), "step {step}: still fading");
    }
    app.update();
    assert_eq!(background(&app, b), light, "the last step lands exactly");
    assert!(!resolver(&app).fading());
}

#[test]
fn a_linear_light_blend_flashes_bright_early_and_oklab_does_not() {
    let mut app = app();
    let b = button(&mut app);
    settle(&mut app);
    let (dark, light) = endpoints(&mut app, b);
    let quarter = |app: &mut App, space: BlendSpace| {
        set_transition(
            app,
            ThemeTransition {
                duration: Some(0.4),
                ease: Some(Easing::Linear),
                space,
            },
        );
        select(app, "regia-light");
        app.update();
        let f = lightness_fraction(background(app, b), dark, light);
        // Back to dark at once for the next measurement.
        set_transition(
            app,
            ThemeTransition {
                duration: Some(0.0),
                ..ThemeTransition::default()
            },
        );
        select(app, "regia-dark");
        app.update();
        f
    };
    let linear = quarter(&mut app, BlendSpace::Linear);
    let oklab = quarter(&mut app, BlendSpace::Oklab);
    assert!(
        (oklab - 0.25).abs() < 0.01,
        "OKLab is a quarter of the way after a quarter of the time: {oklab}"
    );
    assert!(
        linear > oklab + 0.1,
        "a linear-light blend looks further along early (the flash): linear {linear}, oklab {oklab}"
    );
}

#[test]
fn the_default_swap_follows_the_themes_ease_standard() {
    let mut app = app();
    let b = button(&mut app);
    settle(&mut app);
    let (dark, light) = endpoints(&mut app, b);
    set_transition(&mut app, ThemeTransition::default());
    select(&mut app, "regia-light");
    app.update();
    let theme_ease = resolver(&app)
        .fade_ease()
        .expect("the built-in themes define ease.standard");
    assert_eq!(theme_ease, Easing::CubicBezier([0.2, 0.0, 0.0, 1.0]));
    assert!((resolver(&app).fade_secs() - 0.16).abs() < 1e-6, "motion.base");
    // 100 ms of a 160 ms fade is 62.5 % of the time.
    let expected = ease_progress(theme_ease, 0.625);
    assert!(
        expected > 0.85,
        "ease.standard front-loads the change: {expected}"
    );
    let f = lightness_fraction(background(&app, b), dark, light);
    assert!(
        (f - expected).abs() < 0.01,
        "lightness {f} of the way, expected {expected}"
    );
    app.update();
    assert_eq!(background(&app, b), light);
}

#[test]
fn a_transition_change_applies_from_the_next_swap() {
    let mut app = app();
    let b = button(&mut app);
    settle(&mut app);
    select(&mut app, "regia-light");
    app.update();
    assert!(resolver(&app).fading(), "the default 160 ms fade runs");
    set_transition(
        &mut app,
        ThemeTransition {
            duration: Some(0.35),
            ..ThemeTransition::default()
        },
    );
    app.update();
    assert!(!resolver(&app).fading(), "the running fade kept its 160 ms");
    let light = background(&app, b);

    select(&mut app, "regia-dark");
    for _ in 0..3 {
        app.update();
        assert!(resolver(&app).fading(), "the next swap runs 350 ms");
    }
    assert!((resolver(&app).fade_secs() - 0.35).abs() < 1e-6);
    app.update();
    assert!(!resolver(&app).fading());
    assert_ne!(background(&app, b), light);
}

#[test]
fn a_state_change_mid_swap_takes_the_node_off_the_swap_fade() {
    let mut app = app();
    let b = button(&mut app);
    settle(&mut app);
    set_transition(
        &mut app,
        ThemeTransition {
            duration: Some(0.35),
            ..ThemeTransition::default()
        },
    );
    select(&mut app, "regia-light");
    app.update();
    assert_eq!(resolver(&app).fading_nodes(), 1);
    app.world_mut().entity_mut(b).insert(Selected);
    app.update();
    assert!(
        resolver(&app).fade_remaining() <= 0.0,
        "no node is left on the swap"
    );
    assert_eq!(
        resolver(&app).fading_nodes(),
        0,
        "the selection animates on its own"
    );
    assert!(
        !resolver(&app).fading(),
        "the swap clock alone does not count as fading"
    );
    let log = app.world().resource::<Log>();
    assert_eq!(
        log.finished.last().map(|f| f.theme.as_str()),
        Some("regia-light"),
        "the swap is reported finished once no node fades"
    );
    for _ in 0..3 {
        app.update();
    }
    // Settled on the light theme's selected look, whatever path it took.
    let settled = background(&app, b);
    app.update();
    assert_eq!(background(&app, b), settled);
    assert_eq!(resolver(&app).stats().animating, 0);
}

#[test]
fn change_messages_bracket_every_swap() {
    let mut app = app();
    button(&mut app);
    settle(&mut app);
    {
        let log = app.world().resource::<Log>();
        assert_eq!(log.changed.len(), 1, "the first generation");
        assert_eq!(log.changed[0].from, None);
        assert_eq!(log.changed[0].to, "regia-dark");
        assert_eq!(log.finished.len(), 1, "a first look applies at once");
        assert_eq!(log.finished[0].generation, log.changed[0].generation);
    }
    let first = resolver(&app).generation();

    select(&mut app, "regia-light");
    app.update();
    {
        let log = app.world().resource::<Log>();
        assert_eq!(log.changed.len(), 2);
        let c = &log.changed[1];
        assert_eq!(c.from.as_deref(), Some("regia-dark"));
        assert_eq!(c.to, "regia-light");
        assert_eq!(c.generation, first + 1);
        assert!((c.fade_secs - 0.16).abs() < 1e-6);
        assert_eq!(log.finished.len(), 1, "the fade is still running");
    }
    assert_eq!(resolver(&app).previous(), Some("regia-dark"));
    app.update();
    {
        let log = app.world().resource::<Log>();
        assert_eq!(log.finished.len(), 2, "the fade ended");
        assert_eq!(log.finished[1].theme, "regia-light");
        assert_eq!(log.finished[1].generation, first + 1);
    }
    for _ in 0..5 {
        app.update();
    }
    let log = app.world().resource::<Log>();
    assert_eq!(
        (log.changed.len(), log.finished.len()),
        (2, 2),
        "idle frames write nothing"
    );
}

#[test]
fn a_swap_replaced_mid_fade_finishes_once_on_the_last_theme() {
    let mut app = app();
    button(&mut app);
    settle(&mut app);
    select(&mut app, "regia-light");
    app.update();
    select(&mut app, "regia-dark");
    for _ in 0..3 {
        app.update();
    }
    let log = app.world().resource::<Log>();
    let names: Vec<&str> = log.changed.iter().map(|c| c.to.as_str()).collect();
    assert_eq!(names, ["regia-dark", "regia-light", "regia-dark"]);
    assert_eq!(log.changed[2].from.as_deref(), Some("regia-light"));
    assert_eq!(log.finished.len(), 2, "the first look, then only the last swap");
    assert_eq!(log.finished[1].theme, "regia-dark");
    assert_eq!(log.finished[1].generation, log.changed[2].generation);
}

#[test]
fn a_reduced_motion_swap_finishes_in_its_own_frame() {
    let mut app = app();
    button(&mut app);
    settle(&mut app);
    app.world_mut().resource_mut::<UiAccessibility>().reduced_motion = true;
    app.update();
    select(&mut app, "regia-light");
    app.update();
    let log = app.world().resource::<Log>();
    let last = log.changed.last().expect("a change");
    assert_eq!(last.to, "regia-light");
    assert_eq!(last.fade_secs, 0.0);
    let done = log.finished.last().expect("a finish");
    assert_eq!(done.generation, last.generation, "finished in the same frame");
    assert!(!resolver(&app).fading());
}

#[test]
fn the_os_scheme_reaches_follow_system() {
    let mut app = app();
    button(&mut app);
    settle(&mut app);
    app.world_mut().resource_mut::<ThemeMode>().follow = Some(SchemePreference::System);
    app.update();
    assert_eq!(
        resolver(&app).active(),
        "regia-dark",
        "no report yet reads as dark"
    );

    let window = app.world_mut().spawn_empty().id();
    app.world_mut()
        .write_message(regia_platform::SystemThemeChanged { window, dark: false });
    app.update();
    assert_eq!(
        app.world().resource::<ThemeMode>().system_scheme,
        Some(regia_ui::theme_file::Scheme::Light)
    );
    assert_eq!(resolver(&app).active(), "regia-light");

    // Two reports in one frame: the last one wins.
    app.world_mut()
        .write_message(regia_platform::SystemThemeChanged { window, dark: false });
    app.world_mut()
        .write_message(regia_platform::SystemThemeChanged { window, dark: true });
    app.update();
    assert_eq!(resolver(&app).active(), "regia-dark");

    // Not following: the report is kept but selects nothing.
    app.world_mut().resource_mut::<ThemeMode>().follow = None;
    app.update();
    app.world_mut()
        .write_message(regia_platform::SystemThemeChanged { window, dark: false });
    app.update();
    assert_eq!(resolver(&app).active(), "regia-dark");
}

#[test]
fn high_contrast_keeps_the_players_scheme() {
    let mut app = app();
    button(&mut app);
    settle(&mut app);
    app.world_mut().resource_mut::<ThemeMode>().follow = Some(SchemePreference::Light);
    app.update();
    assert_eq!(resolver(&app).active(), "regia-light");
    app.world_mut().resource_mut::<UiAccessibility>().high_contrast = true;
    app.update();
    assert_eq!(
        resolver(&app).active(),
        "regia-light-high-contrast",
        "a light-scheme player stays light"
    );
    let theme = resolver(&app).resolved_theme().expect("resolved");
    assert_eq!(theme.scheme, regia_ui::theme_file::Scheme::Light);
    app.world_mut().resource_mut::<ThemeMode>().follow = Some(SchemePreference::Dark);
    app.update();
    assert_eq!(resolver(&app).active(), "regia-high-contrast");
    app.world_mut().resource_mut::<UiAccessibility>().high_contrast = false;
    app.update();
    assert_eq!(resolver(&app).active(), "regia-dark");
}

#[test]
fn the_light_high_contrast_theme_meets_its_contrast_targets() {
    use regia_ui::theme_color_ops::contrast_ratio;
    let set = regia_ui::theme_resolve::builtin_theme_set();
    let hc = set.resolve("regia-light-high-contrast", None).expect("built in");
    assert_eq!(
        hc.chain,
        ["regia-light-high-contrast", "regia-light", "regia-dark"]
    );
    let r = |fg: &str, bg: &str| {
        contrast_ratio(
            hc.color(fg).unwrap_or_else(|| panic!("{fg}")),
            hc.color(bg).unwrap_or_else(|| panic!("{bg}")),
        )
    };
    // Text at the WCAG 2.x AAA level (7:1), the rest at least the § 2.1 requirement.
    for (fg, bg, required) in [
        ("color.text", "color.surface", 7.0),
        ("color.text", "color.bg", 7.0),
        ("color.text-muted", "color.surface", 7.0),
        ("color.text-muted", "color.surface-sunken", 7.0),
        ("color.border", "color.surface", 4.5),
        ("color.focus-ring", "color.bg", 7.0),
        ("color.on-accent", "color.accent", 4.5),
        ("color.on-danger", "color.danger", 4.5),
        ("color.accent", "color.surface", 3.0),
        ("color.danger", "color.surface", 4.5),
        ("color.warning", "color.surface", 4.5),
        ("color.success", "color.surface", 4.5),
        ("color.border-strong", "color.surface", 3.0),
    ] {
        let ratio = r(fg, bg);
        assert!(ratio >= required, "{fg} on {bg}: {ratio:.2} < {required}");
    }
    assert_eq!(hc.number("border.width.hairline"), Some(2.0));
    assert_eq!(hc.number("border.width.focus"), Some(3.0));
    // Everything not overridden comes from regia-light.
    let light = set.resolve("regia-light", None).expect("built in");
    assert_eq!(hc.color("color.accent"), light.color("color.accent"));
    assert_eq!(hc.color("color.surface"), light.color("color.surface"));
}
