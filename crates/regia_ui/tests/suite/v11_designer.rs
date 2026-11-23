// SPDX-License-Identifier: MPL-2.0
//! The UI designer's additions folded into `.ui.ron` v11 (ADR-7751, under ADR-7724's fold rule):
//! theme-token bindings follow a theme switch, animation bindings drive a property over the game
//! clock, and the frozen v11 corpus file still reads unchanged next to the second v11 fixture that
//! uses every addition. Headless: fixed 100 ms steps, no window, no GPU, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::authoring::{RepeatRowHeight, UiLibrary, spawn_ui};
use regia_ui::prelude::*;
use regia_ui::text::TextContent;
use regia_ui::viewmodel::ViewBindings;
use regia_ui::{
    ActiveTheme, AnimationRepeat, BindSource, ImageFit, NodeName, ThemeResolver, UI_RON_VERSION,
    UiAccessibility, UiMigrators, parse_ui_ron,
};

fn corpus(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../regia_agent/tests/fixtures/format-corpus/ui")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
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
    app.init_resource::<regia_ecs::prelude::AppTypeRegistry>();
    app.update();
    app
}

fn show(app: &mut App, layout: &str) {
    let file = parse_ui_ron(layout, "t.ui.ron", &UiMigrators::default()).expect("layout");
    let world = app.world_mut();
    world
        .resource_mut::<UiLibrary>()
        .templates
        .insert("t".to_owned(), file);
    spawn_ui(world, "t").expect("spawn");
}

fn named(app: &mut App, name: &str) -> Entity {
    let world = app.world_mut();
    let mut q = world.query::<(Entity, &NodeName)>();
    q.iter(world)
        .find(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no node {name}"))
}

fn text(app: &App, entity: Entity) -> String {
    match &app.world().get::<Text>(entity).expect("text").spans[0].content {
        TextContent::Lit(s) => s.clone(),
        TextContent::Loc { key, .. } => format!("loc:{key}"),
    }
}

fn active_accent(app: &App) -> String {
    let theme = app
        .world()
        .resource::<ThemeResolver>()
        .resolved_theme()
        .expect("resolved theme");
    regia_ui::theme_color_ops::to_hex(theme.color("color.accent").expect("accent token"))
}

/// Every v11 file written before the fold still loads unchanged, and the second v11 fixture uses
/// all three additions. The frozen file must not need anything the fold added.
#[test]
fn the_frozen_v11_file_and_the_designer_fixture_both_load() {
    let old = corpus("v11.ui.ron");
    let file = parse_ui_ron(&old, "v11.ui.ron", &UiMigrators::default()).expect("frozen v11");
    assert_eq!(file.version, UI_RON_VERSION);
    let pictures: Vec<_> = file
        .root
        .children
        .iter()
        .filter_map(|c| c.image.as_ref())
        .collect();
    assert_eq!(pictures.len(), 3);
    assert_eq!(pictures[0].effective_fit(), ImageFit::Cover);
    assert_eq!(pictures[2].effective_fit(), ImageFit::Contain);
    // Re-written, the early file still has none of the folded fields.
    let written = regia_ui::write_ui_ron(&file).expect("write");
    for added in ["theme:", "animation:", "row_height", "tile"] {
        assert!(!written.contains(added), "{added} in {written}");
    }

    let designer = parse_ui_ron(
        &corpus("v11-designer.ui.ron"),
        "v11-designer.ui.ron",
        &UiMigrators::default(),
    )
    .expect("v11-designer");
    assert_eq!(designer.version, 11, "the fold keeps the version");
    let children = &designer.root.children;
    assert_eq!(children[0].bindings[0].theme.as_deref(), Some("color.accent"));
    let animation = children[1].bindings[0].animation.as_ref().expect("animation");
    assert_eq!(animation.repeat, AnimationRepeat::PingPong);
    assert_eq!(animation.ease.as_deref(), Some("quad_in_out"));
    let repeat = children[2].repeat.as_ref().expect("repeat");
    assert_eq!(repeat.row_height, RepeatRowHeight::Template);
    assert_eq!(
        regia_ui::authoring::RepeatDef::template_row_height(&children[2].children),
        Some(56.0)
    );
    assert_eq!(
        children[3].image.as_ref().expect("image").effective_fit(),
        ImageFit::Tile
    );
    // The writer keeps the additions.
    let again = parse_ui_ron(
        &regia_ui::write_ui_ron(&designer).expect("write"),
        "again.ui.ron",
        &UiMigrators::default(),
    )
    .expect("re-read");
    assert_eq!(again, designer);
}

/// A theme-token binding shows the active theme's value and follows a theme switch.
#[test]
fn a_theme_token_binding_follows_a_theme_switch() {
    let mut app = app();
    show(
        &mut app,
        r#"(version: 11, root: (children: [
            (name: "accent", text: (spans: ["-"]), bindings: [(theme: "color.accent")]),
            (name: "missing", text: (spans: ["-"]), bindings: [(theme: "color.no-such-token")]),
        ]))"#,
    );
    app.update();
    app.update();
    let accent = named(&mut app, "accent");
    let dark = active_accent(&app);
    assert_eq!(text(&app, accent), dark);
    let bind = &app.world().get::<ViewBindings>(accent).expect("bindings").0[0];
    assert!(matches!(bind.source, BindSource::Theme(ref t) if t == "color.accent"));

    app.world_mut().resource_mut::<ActiveTheme>().name = "regia-light".to_owned();
    app.update();
    let light = active_accent(&app);
    assert_ne!(dark, light, "the built-in light theme has another accent");
    assert_eq!(text(&app, accent), light, "the binding follows the switch");

    app.world_mut().resource_mut::<ActiveTheme>().name = "regia-dark".to_owned();
    app.update();
    assert_eq!(text(&app, accent), dark, "and back");

    let missing = named(&mut app, "missing");
    let bind = &app.world().get::<ViewBindings>(missing).expect("bindings").0[0];
    assert_eq!(text(&app, missing), "-", "an unknown token pushes nothing");
    assert!(
        bind.last_error()
            .is_some_and(|e| e.starts_with("binding_theme_token")),
        "{:?}",
        bind.last_error()
    );
}

/// An animation binding drives its target over the game clock: linear `once` reaches `to` and
/// holds, `ping_pong` comes back, and reduced motion shows the end value at once.
#[test]
fn an_animation_binding_drives_a_property_over_time() {
    let mut app = app();
    show(
        &mut app,
        r#"(version: 11, root: (children: [
            (name: "fade", opacity: 1, bindings: [(target: opacity, animation: (from: 0, to: 1, duration: 1))]),
            (name: "pulse", opacity: 1, bindings: [(target: opacity, animation: (from: 0, to: 1, duration: 0.5, repeat: ping_pong))]),
            (name: "late", text: (spans: ["-"]), bindings: [(format: "{:.2}", animation: (from: 2, to: 4, duration: 1, delay: 0.5))]),
        ]))"#,
    );
    let (fade, pulse, late) = (
        named(&mut app, "fade"),
        named(&mut app, "pulse"),
        named(&mut app, "late"),
    );
    let opacity = |app: &App, e: Entity| app.world().get::<Opacity>(e).expect("opacity").0;
    let clock = |app: &App, e: Entity| app.world().get::<ViewBindings>(e).expect("bindings").0[0].clock();

    app.update();
    assert_eq!(opacity(&app, fade), 0.0, "the first frame pushes `from`");
    assert_eq!(text(&app, late), "2.00", "the delay holds `from`");
    let mut fades = vec![opacity(&app, fade)];
    let mut pulses = vec![opacity(&app, pulse)];
    for _ in 0..14 {
        app.update();
        fades.push(opacity(&app, fade));
        pulses.push(opacity(&app, pulse));
    }
    // Fixed 100 ms steps: the clock moves 0.1 s per frame.
    assert!((clock(&app, pulse) - 1.4).abs() < 1e-4, "{}", clock(&app, pulse));
    for (frame, value) in fades.iter().enumerate() {
        let want = (frame as f32 * 0.1).min(1.0);
        assert!((value - want).abs() < 1e-4, "frame {frame}: {value} vs {want}");
    }
    assert_eq!(*fades.last().unwrap(), 1.0, "once holds `to`");
    // 0.5 s passes: up to 1 at 0.5 s, down to 0 at 1.0 s, up again.
    assert!((pulses[5] - 1.0).abs() < 1e-4, "{pulses:?}");
    assert!(pulses[10].abs() < 1e-4, "{pulses:?}");
    assert!((pulses[13] - 0.6).abs() < 1e-4, "{pulses:?}");
    // 1.4 s - 0.5 s delay = 0.9 of the pass from 2 to 4.
    assert_eq!(text(&app, late), "3.80");

    // A finished `once` animation stops writing its target.
    let ticks = app
        .world()
        .entity(fade)
        .get_ref::<Opacity>()
        .expect("opacity")
        .last_changed();
    app.update();
    assert_eq!(
        app.world()
            .entity(fade)
            .get_ref::<Opacity>()
            .expect("opacity")
            .last_changed(),
        ticks
    );

    // Reduced motion: the end value at once.
    app.world_mut().resource_mut::<UiAccessibility>().reduced_motion = true;
    app.update();
    assert_eq!(opacity(&app, pulse), 1.0);
}
