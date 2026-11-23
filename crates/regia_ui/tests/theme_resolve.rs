// SPDX-License-Identifier: MPL-2.0
//! Theme resolver (lane D6, ADR-289; `docs/specs/ui-default-design.md` §§ 3, 6.3, 6.4): the
//! precedence table one row per test, subtree themes, variation chains, the runtime swap with its
//! cross-fade, the generation-keyed cache (a swap re-resolves each node once, an idle frame
//! resolves nothing) and the disabled rule. No GPU, no files, fixed 50 ms steps, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::theme::{StyleClass, VisualOverrides};
use regia_ui::theme_color_ops::lighten;
use regia_ui::theme_file::ResolvedTheme;
use regia_ui::theme_resolve::builtin_theme_set;
use regia_ui::{
    ActiveTheme, BackgroundKind, Focused, Hovered, Pressed, ThemeOverride, ThemeResolver, ThemeScope,
    ThemeStats, UiThemes,
};

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
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(50)));
    app
}

fn theme(name: &str) -> ResolvedTheme {
    builtin_theme_set()
        .resolve(name, None)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn tok(t: &ResolvedTheme, key: &str) -> UiColor {
    t.color(key)
        .unwrap_or_else(|| panic!("no color {key} in {}", t.name))
}

fn spawn(app: &mut App, classes: &[&str]) -> Entity {
    let classes = classes.iter().map(|c| (*c).to_owned()).collect();
    app.world_mut().spawn((UiNode, Classes(classes))).id()
}

fn themed(app: &App, e: Entity) -> Themed {
    *app.world().get::<Themed>(e).unwrap()
}

fn stats(app: &App) -> ThemeStats {
    app.world().resource::<ThemeResolver>().stats()
}

/// Long enough for every transition and cross-fade (the longest is `motion.base`, 160 ms).
fn settle(app: &mut App) {
    for _ in 0..8 {
        app.update();
    }
}

fn select(app: &mut App, active: ActiveTheme) {
    app.world_mut().insert_resource(active);
}

// --- § 6.3 precedence, one test per row ---------------------------------------------------------

#[test]
fn row_1_node_override_beats_the_variation_chain() {
    let mut app = ui_app();
    let red = UiColor::linear(1.0, 0.0, 0.0, 1.0);
    let e = spawn(&mut app, &["button", "primary"]);
    app.world_mut()
        .entity_mut(e)
        .insert(ThemeOverride::new(VisualOverrides {
            background: Some(red),
            radius: Some(0.0),
            ..Default::default()
        }));
    settle(&mut app);
    let dark = theme("regia-dark");
    let t = themed(&app, e);
    assert_eq!(t.background, Some(red), "override wins over Button.primary");
    assert_eq!(t.radius, Some(0.0));
    assert_eq!(
        t.text_color,
        Some(tok(&dark, "color.on-accent")),
        "other fields keep the chain"
    );
    // The override's normal state also beats the variation's hover layer.
    app.world_mut().entity_mut(e).insert(Hovered);
    settle(&mut app);
    assert_eq!(themed(&app, e).background, Some(red));
    // An override with its own hover layer composes like any class.
    let blue = UiColor::linear(0.0, 0.0, 1.0, 1.0);
    app.world_mut().entity_mut(e).insert(ThemeOverride(StyleClass {
        hover: VisualOverrides {
            background: Some(blue),
            ..Default::default()
        },
        ..Default::default()
    }));
    settle(&mut app);
    assert_eq!(themed(&app, e).background, Some(blue));
    app.world_mut().entity_mut(e).remove::<ThemeOverride>();
    settle(&mut app);
    assert_eq!(
        themed(&app, e).background,
        Some(tok(&dark, "color.accent-hover")),
        "removing the override falls back to the variation"
    );
}

#[test]
fn row_2_nearest_variation_wins_and_each_layer_is_state_composed() {
    let mut app = ui_app();
    let danger = spawn(&mut app, &["button", "danger"]);
    let primary = spawn(&mut app, &["button", "primary"]);
    settle(&mut app);
    let dark = theme("regia-dark");
    let t = themed(&app, danger);
    // Button.danger -> Button.primary -> Button: the nearest layer that sets a field wins.
    assert_eq!(t.background, Some(tok(&dark, "color.danger")), "danger");
    assert_eq!(t.text_color, Some(tok(&dark, "color.on-danger")), "danger");
    assert_eq!(
        t.border_width,
        Some(0.0),
        "from Button.primary, over Button's hairline"
    );
    assert_eq!(t.radius, dark.number("radius.md"), "from Button");
    // Hover: the variation's own hover layer, not the type's hover fill.
    app.world_mut().entity_mut(danger).insert(Hovered);
    app.world_mut().entity_mut(primary).insert(Pressed);
    settle(&mut app);
    assert_eq!(
        themed(&app, danger).background,
        Some(lighten(tok(&dark, "color.danger"), 0.05))
    );
    let p = themed(&app, primary);
    assert_eq!(p.background, Some(tok(&dark, "color.accent-pressed")));
    assert_eq!(
        p.translate,
        Some([0.0, 1.0]),
        "a field only the type's pressed layer sets"
    );
}

#[test]
fn row_3_widget_type_applies_without_a_variation() {
    let mut app = ui_app();
    let e = spawn(&mut app, &["button"]);
    settle(&mut app);
    let dark = theme("regia-dark");
    let t = themed(&app, e);
    assert_eq!(t.background, Some(tok(&dark, "color.surface-raised")));
    assert_eq!(t.border_color, Some(tok(&dark, "color.border")));
    assert_eq!(t.border_width, dark.number("border.width.hairline"));
    app.world_mut().entity_mut(e).insert(Hovered);
    settle(&mut app);
    let h = themed(&app, e);
    assert_eq!(
        h.background,
        Some(lighten(tok(&dark, "color.surface-raised"), 0.04))
    );
    assert_eq!(h.border_color, Some(tok(&dark, "color.border-strong")));
}

#[test]
fn row_4_theme_defaults_fill_what_no_type_sets() {
    let mut app = ui_app();
    let fill = UiColor::linear(0.2, 0.4, 0.1, 1.0);
    let bare = spawn(&mut app, &["made-up"]);
    let with_bg = spawn(&mut app, &["made-up"]);
    app.world_mut().entity_mut(with_bg).insert(Background {
        kind: BackgroundKind::Color(fill),
    });
    settle(&mut app);
    let dark = theme("regia-dark");
    let t = themed(&app, bare);
    assert_eq!(t.text_color, Some(tok(&dark, "color.text")));
    assert_eq!(t.border_color, Some(tok(&dark, "color.border")));
    assert_eq!(t.radius, dark.number("radius.md"));
    assert_eq!(t.focus_ring_width, dark.number("border.width.focus"));
    assert_eq!(t.border_width, None, "no type, no border");
    assert_eq!(t.background, Some(UiColor::TRANSPARENT));
    assert_eq!(
        themed(&app, with_bg).background,
        Some(fill),
        "an explicit Background fills in when no layer sets one"
    );
}

#[test]
fn tokens_resolve_through_the_theme_chain() {
    let mut app = ui_app();
    let src = r##"(format_version: 1, name: "orange", extends: "regia-dark",
        tokens: {"color.accent": "#e8793a", "radius.md": 3})"##;
    app.world_mut()
        .resource_mut::<UiThemes>()
        .set
        .load_ron(src, "orange.theme.ron")
        .expect("child theme loads");
    select(&mut app, ActiveTheme::named("orange"));
    let e = spawn(&mut app, &["button", "primary"]);
    settle(&mut app);
    let orange = app
        .world()
        .resource::<UiThemes>()
        .set
        .resolve("orange", None)
        .unwrap();
    let t = themed(&app, e);
    assert_eq!(t.background, Some(UiColor::from_hex("#e8793a").unwrap()));
    assert_eq!(t.radius, Some(3.0));
    // Inherited from regia-dark unchanged.
    assert_eq!(t.text_color, Some(tok(&theme("regia-dark"), "color.on-accent")));
    app.world_mut().entity_mut(e).insert(Hovered);
    settle(&mut app);
    assert_eq!(
        themed(&app, e).background,
        Some(tok(&orange, "color.accent-hover")),
        "derived tokens follow the child's accent"
    );
}

// --- subtree theme and variation chains ---------------------------------------------------------

#[test]
fn theme_scope_resolves_a_subtree_against_another_theme() {
    let mut app = ui_app();
    let scope = app
        .world_mut()
        .spawn((UiNode, ThemeScope::new("regia-light")))
        .id();
    let inside = app
        .world_mut()
        .spawn((UiNode, Classes(vec!["button".to_owned()]), ChildOf(scope)))
        .id();
    let outside = spawn(&mut app, &["button"]);
    settle(&mut app);
    let (dark, light) = (theme("regia-dark"), theme("regia-light"));
    assert_eq!(
        themed(&app, inside).background,
        Some(tok(&light, "color.surface-raised"))
    );
    assert_eq!(themed(&app, inside).text_color, Some(tok(&light, "color.text")));
    assert_eq!(
        themed(&app, outside).background,
        Some(tok(&dark, "color.surface-raised"))
    );
    // Removing the scope returns the subtree to the active theme.
    app.world_mut().entity_mut(scope).remove::<ThemeScope>();
    settle(&mut app);
    assert_eq!(
        themed(&app, inside).background,
        Some(tok(&dark, "color.surface-raised"))
    );
    // An unknown scope theme falls back to the active one.
    app.world_mut()
        .entity_mut(scope)
        .insert(ThemeScope::new("not-loaded"));
    settle(&mut app);
    assert_eq!(
        themed(&app, inside).background,
        Some(tok(&dark, "color.surface-raised"))
    );
}

#[test]
fn a_game_variation_chain_layers_root_first() {
    let mut app = ui_app();
    let src = r##"(format_version: 1, name: "game", extends: "regia-dark",
        types: {"Card": (props: {"background": "#101010", "radius": 2, "border_width": 3})},
        variations: {
            "Card.raised": (extends: "Card", props: {"background": "#202020", "radius": 4}),
            "Card.hot": (extends: "Card.raised", props: {"background": "#303030"}),
        })"##;
    app.world_mut()
        .resource_mut::<UiThemes>()
        .set
        .load_ron(src, "game.theme.ron")
        .expect("game theme loads");
    select(&mut app, ActiveTheme::named("game"));
    let hot = spawn(&mut app, &["Card.hot"]);
    let raised = spawn(&mut app, &["Card.raised"]);
    let card = spawn(&mut app, &["Card"]);
    settle(&mut app);
    let hot = themed(&app, hot);
    assert_eq!(hot.background, UiColor::from_hex("#303030"), "nearest");
    assert_eq!(hot.radius, Some(4.0), "middle");
    assert_eq!(hot.border_width, Some(3.0), "root type");
    assert_eq!(themed(&app, raised).background, UiColor::from_hex("#202020"));
    assert_eq!(themed(&app, card).radius, Some(2.0));
}

// --- the runtime swap and the cache --------------------------------------------------------------

#[test]
fn a_swap_re_resolves_every_node_once_with_one_style_computed() {
    let mut app = ui_app();
    let buttons: Vec<Entity> = (0..5).map(|_| spawn(&mut app, &["button"])).collect();
    settle(&mut app);
    let before = stats(&app).generation;
    select(&mut app, ActiveTheme::named("regia-light"));
    app.update();
    let swap = stats(&app);
    assert_eq!(swap.generation, before + 1, "one generation per swap");
    assert_eq!(swap.nodes_resolved, 5, "every themed node once");
    assert_eq!(swap.styles_computed, 1, "identical nodes share one cached style");
    app.update();
    let next = stats(&app);
    assert_eq!(next.generation, swap.generation);
    assert_eq!(
        next.nodes_resolved, 0,
        "the frame after the swap resolves nothing"
    );
    settle(&mut app);
    let light = theme("regia-light");
    for e in buttons {
        assert_eq!(
            themed(&app, e).background,
            Some(tok(&light, "color.surface-raised"))
        );
    }
    assert_eq!(app.world().resource::<ThemeResolver>().active(), "regia-light");
}

#[test]
fn an_idle_frame_does_no_work() {
    let mut app = ui_app();
    for _ in 0..4 {
        spawn(&mut app, &["button", "primary"]);
    }
    settle(&mut app);
    let before = stats(&app);
    app.update();
    let idle = stats(&app);
    assert_eq!(idle.generation, before.generation);
    assert_eq!(idle.nodes_resolved, 0);
    assert_eq!(idle.styles_computed, 0);
    assert_eq!(idle.animating, 0);
}

#[test]
fn a_state_change_resolves_only_that_node() {
    let mut app = ui_app();
    let nodes: Vec<Entity> = (0..3).map(|_| spawn(&mut app, &["button"])).collect();
    settle(&mut app);
    app.world_mut().entity_mut(nodes[1]).insert(Hovered);
    app.update();
    let s = stats(&app);
    assert_eq!(s.nodes_resolved, 1);
    assert_eq!(s.styles_computed, 1, "the hovered style is new");
    app.world_mut().entity_mut(nodes[1]).remove::<Hovered>();
    app.update();
    let s = stats(&app);
    assert_eq!(s.nodes_resolved, 1, "a removed marker is seen too");
    assert_eq!(s.styles_computed, 0, "the rest style is cached");
}

#[test]
fn removing_classes_drops_the_theme_look() {
    let mut app = ui_app();
    let e = spawn(&mut app, &["button"]);
    settle(&mut app);
    assert_ne!(themed(&app, e), Themed::default());
    app.world_mut().entity_mut(e).remove::<Classes>();
    settle(&mut app);
    assert_eq!(themed(&app, e), Themed::default());
}

#[test]
fn a_swap_cross_fades_over_motion_base_and_reduced_motion_jumps() {
    let mut app = ui_app();
    let e = spawn(&mut app, &["button"]);
    settle(&mut app);
    let (dark, light) = (theme("regia-dark"), theme("regia-light"));
    let (dark_bg, light_bg) = (
        tok(&dark, "color.surface-raised"),
        tok(&light, "color.surface-raised"),
    );
    assert_eq!(themed(&app, e).background, Some(dark_bg));
    select(&mut app, ActiveTheme::named("regia-light"));
    app.update();
    let mid = themed(&app, e);
    assert_ne!(mid.background, Some(dark_bg), "the fade has started");
    assert_ne!(mid.background, Some(light_bg), "the fade has not finished");
    assert_ne!(mid.text_color, Some(tok(&light, "color.text")), "text fades too");
    assert!(stats(&app).animating >= 1);
    // motion.base is 160 ms: four 50 ms steps finish it.
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(themed(&app, e).background, Some(light_bg));
    assert_eq!(themed(&app, e).text_color, Some(tok(&light, "color.text")));
    assert_eq!(stats(&app).animating, 0);
    select(
        &mut app,
        ActiveTheme {
            reduced_motion: true,
            ..ActiveTheme::named("regia-dark")
        },
    );
    app.update();
    assert_eq!(
        themed(&app, e).background,
        Some(dark_bg),
        "reduced motion applies at once"
    );
}

#[test]
fn high_contrast_and_unknown_names_pick_the_documented_theme() {
    let mut app = ui_app();
    let e = spawn(&mut app, &["button"]);
    select(
        &mut app,
        ActiveTheme {
            high_contrast: true,
            ..ActiveTheme::named("regia-dark")
        },
    );
    settle(&mut app);
    assert_eq!(
        app.world().resource::<ThemeResolver>().active(),
        "regia-high-contrast"
    );
    assert_eq!(themed(&app, e).text_color, UiColor::from_hex("#ffffff"));
    select(&mut app, ActiveTheme::named("not-loaded"));
    settle(&mut app);
    assert_eq!(app.world().resource::<ThemeResolver>().active(), "regia-dark");
}

// --- the disabled rule and legacy compatibility -------------------------------------------------

#[test]
fn disabled_suppresses_hover_and_pressed_at_0_45() {
    let mut app = ui_app();
    let e = spawn(&mut app, &["button", "primary"]);
    app.world_mut()
        .entity_mut(e)
        .insert((Hovered, Pressed, Focused, Disabled));
    settle(&mut app);
    let dark = theme("regia-dark");
    let t = themed(&app, e);
    assert_eq!(
        t.background,
        Some(tok(&dark, "color.accent")),
        "normal look, no hover/pressed"
    );
    assert_eq!(t.translate, None, "pressed nudge suppressed");
    assert_eq!(t.opacity, Some(0.45));
    assert_eq!(t.text_color, Some(tok(&dark, "color.text-disabled")));
    app.world_mut().entity_mut(e).remove::<Disabled>();
    settle(&mut app);
    let t = themed(&app, e);
    assert_eq!(t.background, Some(tok(&dark, "color.accent-pressed")));
    assert_eq!(t.opacity, None);
}

#[test]
fn the_legacy_toggle_still_switches_the_resolved_theme() {
    let mut app = ui_app();
    let e = spawn(&mut app, &["button"]);
    settle(&mut app);
    app.world_mut().resource_mut::<UiThemes>().toggle();
    settle(&mut app);
    assert_eq!(app.world().resource::<ActiveTheme>().name, "regia-light");
    assert_eq!(
        themed(&app, e).background,
        Some(tok(&theme("regia-light"), "color.surface-raised"))
    );
    // And the other way: selecting through ActiveTheme moves the legacy palette.
    select(&mut app, ActiveTheme::named("regia-dark"));
    settle(&mut app);
    assert_eq!(app.world().resource::<UiThemes>().active, 0);
    // A game palette without a theme file is migrated when selected.
    let mut game = UiTheme::dark();
    game.name = "game-v0".to_owned();
    game.colors.surface_hi = UiColor::from_hex("#334455").unwrap();
    {
        let mut themes = app.world_mut().resource_mut::<UiThemes>();
        themes.themes.push(game);
        assert!(themes.select("game-v0"));
    }
    settle(&mut app);
    assert_eq!(app.world().resource::<ThemeResolver>().active(), "game-v0");
    assert_eq!(
        themed(&app, e).background.map(regia_ui::theme_color_ops::to_hex),
        Some("#334455".to_owned())
    );
}

#[test]
fn a_legacy_selection_made_before_the_first_frame_is_kept() {
    let mut app = ui_app();
    app.world_mut().resource_mut::<UiThemes>().toggle();
    let e = spawn(&mut app, &["button"]);
    settle(&mut app);
    assert_eq!(app.world().resource::<ThemeResolver>().active(), "regia-light");
    assert_eq!(
        themed(&app, e).background,
        Some(tok(&theme("regia-light"), "color.surface-raised"))
    );
}

#[test]
fn a_customised_legacy_class_still_layers_over_the_theme() {
    let mut app = ui_app();
    {
        let mut themes = app.world_mut().resource_mut::<UiThemes>();
        let classes = &mut themes.classes.0;
        if let Some((_, button)) = classes.iter_mut().find(|(n, _)| n == "button") {
            button.base.radius = Some(3.0);
        }
        classes.push((
            "chip".to_owned(),
            StyleClass {
                base: VisualOverrides {
                    background: Some(UiColor::linear(0.5, 0.0, 0.5, 1.0)),
                    ..Default::default()
                },
                ..Default::default()
            },
        ));
    }
    let button = spawn(&mut app, &["button"]);
    let chip = spawn(&mut app, &["chip"]);
    let plain_panel = spawn(&mut app, &["panel"]);
    settle(&mut app);
    assert_eq!(themed(&app, button).radius, Some(3.0), "game edit to `button`");
    assert_eq!(
        themed(&app, chip).background,
        Some(UiColor::linear(0.5, 0.0, 0.5, 1.0)),
        "game class"
    );
    assert_eq!(
        themed(&app, plain_panel).radius,
        theme("regia-dark").number("radius.md"),
        "an unchanged built-in class defers to the theme"
    );
}

#[test]
fn removing_classes_during_a_cross_fade_leaves_no_hidden_background() {
    let mut app = ui_app();
    let e = spawn(&mut app, &["button"]);
    settle(&mut app);
    select(&mut app, ActiveTheme::named("regia-light"));
    app.update();
    assert!(stats(&app).animating >= 1, "the fade is running");
    app.world_mut().entity_mut(e).remove::<Classes>();
    app.update();
    assert_eq!(themed(&app, e), Themed::default());
    settle(&mut app);
    assert_eq!(themed(&app, e).background, None);
    assert_eq!(stats(&app).animating, 0);
}
