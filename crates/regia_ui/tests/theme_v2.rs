// SPDX-License-Identifier: MPL-2.0
//! `.theme.ron` v2 (ADR-8105 (e), ADR-8303; P-257, P-272): style rules with structured
//! selectors (a descendant type, widget type, variation, classes, a state), their specificity
//! order, the keyboard-only `focus_visible` layer, and the number-only v1 → v2 step. Headless:
//! no GPU, no files, fixed 50 ms steps, no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_platform::{ButtonState, MouseButton};
use regia_ui::authoring::{UiLibrary, spawn_ui};
use regia_ui::focus::FocusState;
use regia_ui::prelude::*;
use regia_ui::theme_file::{RuleState, StyleRule};
use regia_ui::theme_resolve::builtin_theme_set;
use regia_ui::{
    ActiveTheme, NodeName, THEME_FORMAT_VERSION, ThemeError, ThemeFile, UiMigrators, UiThemes,
    parse_theme_ron, parse_ui_ron,
};

const V1: &str = include_str!("../../regia_agent/tests/fixtures/format-corpus/theme/v1.ron");
const V2: &str = include_str!("../../regia_agent/tests/fixtures/format-corpus/theme/v2.ron");
/// Two buttons in a row: `a` is first in tab order.
const BUTTONS: &str = r#"(version: 11, root: (style: (direction: row), children: [
    (name: "a", widget: button, style: (size: (96, 32))),
    (name: "b", widget: button, style: (size: (96, 32))),
]))"#;

const RED: &str = "#ff0000";
const GREEN: &str = "#00ff00";
const BLUE: &str = "#0000ff";

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

/// Long enough for every transition and cross-fade (the longest is `motion.base`, 160 ms).
fn settle(app: &mut App) {
    for _ in 0..8 {
        app.update();
    }
}

/// Loads a v2 theme text (extending `regia-dark`) and makes it the active theme.
fn use_theme(app: &mut App, source: &str) -> String {
    let name = app
        .world_mut()
        .resource_mut::<UiThemes>()
        .set
        .load_ron(source, "test.theme.ron")
        .unwrap_or_else(|e| panic!("theme loads: {e}"));
    app.world_mut().insert_resource(ActiveTheme::named(&name));
    name
}

fn color(hex: &str) -> UiColor {
    UiColor::from_hex(hex).unwrap_or_else(|| panic!("{hex} is a color"))
}

fn node(app: &mut App, classes: &[&str], parent: Option<Entity>) -> Entity {
    let classes = classes.iter().map(|c| (*c).to_owned()).collect();
    let mut e = app.world_mut().spawn((UiNode, Classes(classes)));
    if let Some(p) = parent {
        e.insert(ChildOf(p));
    }
    e.id()
}

fn background(app: &App, e: Entity) -> Option<UiColor> {
    app.world().get::<Themed>(e).unwrap().background
}

#[test]
fn a_descendant_selector_styles_only_matching_nodes() {
    let mut app = ui_app();
    use_theme(
        &mut app,
        &format!(
            r#"(format_version: 2, name: "rules", extends: Some("regia-dark"),
                rules: [(inside: Some("Toolbar"), widget: Some("Button"), props: {{ "background": "{RED}" }})])"#
        ),
    );
    let toolbar = node(&mut app, &["Toolbar"], None);
    let row = node(&mut app, &["panel"], Some(toolbar));
    let deep = node(&mut app, &["button"], Some(row));
    let direct = node(&mut app, &["button"], Some(toolbar));
    let outside = node(&mut app, &["button"], None);
    let not_a_button = node(&mut app, &["panel"], Some(toolbar));
    settle(&mut app);

    let red = Some(color(RED));
    assert_eq!(
        background(&app, deep),
        red,
        "a Button two levels inside the Toolbar"
    );
    assert_eq!(
        background(&app, direct),
        red,
        "a Button directly inside the Toolbar"
    );
    assert_ne!(background(&app, outside), red, "a Button outside any Toolbar");
    assert_ne!(background(&app, not_a_button), red, "a Panel inside the Toolbar");
    assert_ne!(background(&app, toolbar), red, "the Toolbar itself");
    let unstyled = background(&app, outside);

    // The ancestor loses its type: the descendants re-resolve without the rule.
    app.world_mut()
        .entity_mut(toolbar)
        .insert(Classes(vec!["panel".to_owned()]));
    settle(&mut app);
    assert_eq!(background(&app, deep), unstyled, "no Toolbar above any more");
    assert_eq!(background(&app, direct), unstyled);

    // Moving a Button under a Toolbar restyles it.
    let other = node(&mut app, &["Toolbar"], None);
    app.world_mut().entity_mut(outside).insert(ChildOf(other));
    settle(&mut app);
    assert_eq!(background(&app, outside), red, "reparented under a Toolbar");

    // The inspector names the winning rule by its selector text.
    let report = regia_ui::inspect::inspect(app.world_mut(), outside);
    assert_eq!(
        report["theme"]["rules"],
        serde_json::json!(["Toolbar Button"]),
        "{report:#}"
    );
    let bg = report["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "background")
        .expect("background row")
        .clone();
    assert_eq!(bg["detail"], "rule Toolbar Button", "{bg:#}");
}

#[test]
fn specificity_orders_two_matching_rules() {
    // `[hot]` (one class: specificity (1, 0)) beats `Toolbar Button` ((0, 2)) although it comes
    // first in the file; of two equal rules the later one wins.
    let source = format!(
        r#"(format_version: 2, name: "spec", extends: Some("regia-dark"),
            rules: [
                (classes: ["hot"], props: {{ "background": "{GREEN}" }}),
                (inside: Some("Toolbar"), widget: Some("Button"), props: {{ "background": "{BLUE}", "radius": 1 }}),
                (widget: Some("Button"), props: {{ "radius": 5 }}),
                (widget: Some("Button"), props: {{ "radius": 9 }}),
            ])"#
    );
    let file = parse_theme_ron(&source, "spec.theme.ron").expect("parses");
    let spec: Vec<(usize, usize)> = file.rules.iter().map(StyleRule::specificity).collect();
    assert_eq!(spec, [(1, 0), (0, 2), (0, 1), (0, 1)]);

    let mut app = ui_app();
    use_theme(&mut app, &source);
    let toolbar = node(&mut app, &["Toolbar"], None);
    let hot = node(&mut app, &["button", "hot"], Some(toolbar));
    let cold = node(&mut app, &["button"], Some(toolbar));
    let lone = node(&mut app, &["button"], None);
    settle(&mut app);
    let t = |app: &App, e| *app.world().get::<Themed>(e).unwrap();
    assert_eq!(
        t(&app, hot).background,
        Some(color(GREEN)),
        "the class rule is more specific"
    );
    assert_eq!(
        t(&app, cold).background,
        Some(color(BLUE)),
        "only the descendant rule matches"
    );
    assert_eq!(
        t(&app, cold).radius,
        Some(1.0),
        "(0, 2) beats both (0, 1) rules for radius"
    );
    assert_eq!(
        t(&app, lone).radius,
        Some(9.0),
        "equal specificity: the later rule wins"
    );
    let lone_bg = t(&app, lone).background;
    assert!(
        lone_bg != Some(color(GREEN)) && lone_bg != Some(color(BLUE)),
        "no background rule matches"
    );

    // The order is the specificity, not the file: reversing the rules keeps every winner.
    let mut reversed = file.clone();
    reversed.name = "spec-reversed".to_owned();
    reversed.rules.swap(0, 1);
    let mut app2 = ui_app();
    use_theme(&mut app2, &reversed.to_ron_string().unwrap());
    let toolbar = node(&mut app2, &["Toolbar"], None);
    let hot2 = node(&mut app2, &["button", "hot"], Some(toolbar));
    settle(&mut app2);
    assert_eq!(t(&app2, hot2).background, Some(color(GREEN)));

    // The inspector lists the matching rules in apply order, with their selector text.
    let report = regia_ui::inspect::inspect(app.world_mut(), hot);
    assert_eq!(
        report["theme"]["rules"],
        serde_json::json!(["Button", "Button", "Toolbar Button", "[hot]"]),
        "{report:#}"
    );
}

#[test]
fn a_rule_state_layers_like_a_type_state_and_a_variation_rule_follows_the_chain() {
    let source = format!(
        r#"(format_version: 2, name: "states", extends: Some("regia-dark"),
            rules: [
                (classes: ["danger-zone"], state: Some(hover), props: {{ "background": "{RED}" }}),
                (widget: Some("Button"), variation: Some("primary"), props: {{ "radius": 7 }}),
            ])"#
    );
    let mut app = ui_app();
    use_theme(&mut app, &source);
    let zone = node(&mut app, &["button", "danger-zone"], None);
    let danger = node(&mut app, &["button", "danger"], None);
    let ghost = node(&mut app, &["button", "ghost"], None);
    settle(&mut app);
    let normal = background(&app, zone);
    assert_ne!(normal, Some(color(RED)), "the hover rule waits for hover");
    app.world_mut().entity_mut(zone).insert(regia_ui::Hovered);
    settle(&mut app);
    assert_eq!(background(&app, zone), Some(color(RED)));
    // `Button.danger` extends `Button.primary` in regia-dark: the variation rule matches it.
    let radius = |app: &App, e| app.world().get::<Themed>(e).unwrap().radius;
    assert_eq!(radius(&app, danger), Some(7.0));
    assert_ne!(radius(&app, ghost), Some(7.0), "ghost is not a primary");
}

fn spawn_buttons(app: &mut App) {
    let file = parse_ui_ron(BUTTONS, "buttons.ui.ron", &UiMigrators::default()).unwrap();
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert("buttons".to_owned(), file);
    spawn_ui(app.world_mut(), "buttons").unwrap();
    settle(app);
}

fn named(app: &mut App, name: &str) -> Entity {
    let world = app.world_mut();
    let mut q = world.query::<(Entity, &NodeName)>();
    q.iter(world)
        .find(|(_, n)| n.0 == name)
        .map(|(e, _)| e)
        .unwrap_or_else(|| panic!("no node {name}"))
}

fn key(app: &mut App, logical: &str) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: logical.into(),
            logical_key: logical.into(),
            state,
            repeat: false,
            timestamp: Default::default(),
        });
        app.update();
    }
}

fn click(app: &mut App, e: Entity) {
    let c = *app.world().get::<ComputedNode>(e).unwrap();
    let at = [c.rect[0] + c.rect[2] * 0.5, c.rect[1] + c.rect[3] * 0.5];
    app.world_mut().write_message(regia_platform::CursorMoved {
        window: Entity::PLACEHOLDER,
        position: (f64::from(at[0]), f64::from(at[1])),
        timestamp: Default::default(),
    });
    app.update();
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(regia_platform::MouseButtonInput {
            window: Entity::PLACEHOLDER,
            button: MouseButton::Left,
            state,
            timestamp: Default::default(),
        });
        app.update();
    }
}

#[test]
fn focus_visible_shows_on_tab_not_on_click() {
    let mut app = ui_app();
    // The layer is authored in the theme file (v2), not built in code.
    use_theme(
        &mut app,
        r#"(format_version: 2, name: "fv", extends: Some("regia-dark"),
            types: { "Button": (focused: { "border_width": 3 }, focus_visible: { "border_width": 7 }) })"#,
    );
    spawn_buttons(&mut app);
    let apply = named(&mut app, "a");
    let quit = named(&mut app, "b");

    key(&mut app, "Tab");
    settle(&mut app);
    let focused = app
        .world()
        .resource::<FocusState>()
        .focused
        .expect("Tab focuses a control");
    let t = *app.world().get::<Themed>(focused).unwrap();
    assert!(t.focused && t.focus_visible, "keyboard focus is visible");
    assert_eq!(
        t.border_width,
        Some(7.0),
        "the focus_visible layer wins over focused"
    );

    // A click keeps the focus and drops the keyboard-only layer.
    click(&mut app, focused);
    settle(&mut app);
    assert_eq!(app.world().resource::<FocusState>().focused, Some(focused));
    let t = *app.world().get::<Themed>(focused).unwrap();
    assert!(t.focused && !t.focus_visible, "pointer input hides visible focus");
    assert_eq!(t.border_width, Some(3.0), "the focused layer only");

    // Focus given while the pointer is the input never shows the layer either.
    let other = if focused == apply { quit } else { apply };
    app.world_mut().entity_mut(focused).remove::<regia_ui::Focused>();
    app.world_mut().entity_mut(other).insert(regia_ui::Focused);
    app.world_mut().resource_mut::<FocusState>().focused = Some(other);
    settle(&mut app);
    assert_eq!(app.world().get::<Themed>(other).unwrap().border_width, Some(3.0));

    // The next key brings it back.
    key(&mut app, "Tab");
    settle(&mut app);
    let now = app.world().resource::<FocusState>().focused.expect("focused");
    assert_eq!(app.world().get::<Themed>(now).unwrap().border_width, Some(7.0));
}

#[test]
fn a_v1_theme_upgrades_to_v2() {
    assert_eq!(THEME_FORMAT_VERSION, 2);
    assert_eq!(regia_ui::theme_migration_sources(), [0, 1]);
    let v1 = parse_theme_ron(V1, "v1.theme.ron").expect("v1 loads");
    assert_eq!(v1.format_version, 2, "a v1 file reads as v2");
    assert!(v1.rules.is_empty());
    assert!(v1.types.values().all(|d| d.focus_visible.is_empty()));
    // Saved, it writes v2 and reads back the same.
    let written = v1.to_ron_string().unwrap();
    assert!(written.contains("format_version: 2"), "{written}");
    assert!(
        !written.contains("rules"),
        "an empty rule list is not written: {written}"
    );
    assert_eq!(parse_theme_ron(&written, "again.theme.ron").unwrap(), v1);

    // A v1 theme resolves exactly as before the step: no rule, no focus_visible layer.
    let mut set = builtin_theme_set();
    set.load_ron(V1, "v1.theme.ron").unwrap();
    let resolved = set.resolve("my-game", None).unwrap();
    assert!(resolved.rules.is_empty());

    // The v2 corpus file loads with its rules and layer.
    let v2 = parse_theme_ron(V2, "v2.theme.ron").expect("v2 loads");
    assert_eq!(v2.rules.len(), 3);
    assert_eq!(v2.rules[1].state, Some(RuleState::Hover));
    assert_eq!(v2.rules[2].state, Some(RuleState::FocusVisible));
    assert!(!v2.types["Button"].focus_visible.is_empty());
    let mut set = builtin_theme_set();
    set.load_ron(V2, "v2.theme.ron").expect("v2 resolves");
    assert_eq!(
        v2.rules.iter().map(StyleRule::display).collect::<Vec<_>>(),
        [
            "Toolbar Button",
            "[danger-zone] :hover",
            "Button.primary :focus_visible"
        ]
    );

    // A file that says v1 but uses v2 fields is refused, as is a newer one.
    let mislabeled = V2.replacen("format_version: 2", "format_version: 1", 1);
    let err = parse_theme_ron(&mislabeled, "bad.theme.ron").unwrap_err();
    assert!(err.to_string().contains("format_version 2"), "{err}");
    let newer = V2.replacen("format_version: 2", "format_version: 3", 1);
    assert!(matches!(
        parse_theme_ron(&newer, "new.theme.ron"),
        Err(ThemeError::UnsupportedVersion { found: 3, .. })
    ));
}

#[test]
fn a_rule_needs_a_selector_and_names_its_path_on_error() {
    let bad = |rules: &str| {
        let src =
            format!(r#"(format_version: 2, name: "bad", extends: Some("regia-dark"), rules: [{rules}])"#);
        let mut set = builtin_theme_set();
        set.load_ron(&src, "bad.theme.ron").unwrap_err().to_string()
    };
    assert!(bad(r#"(props: { "radius": 1 })"#).contains("rules[0]"));
    assert!(
        bad(r#"(state: Some(hover), props: { "radius": 1 })"#)
            .contains("needs `inside`, `widget` or `classes`")
    );
    assert!(
        bad(r#"(classes: ["a"]), (classes: ["b"], variation: Some("primary"))"#)
            .contains("rules[1].variation")
    );
    assert!(bad(r#"(widget: Some("Button"), props: { "background": "@no.such" })"#).contains("no.such"));
    assert!(
        bad(r##"(widget: Some("Button"), props: { "background": "#zzz" })"##)
            .contains("rules[0].props.background")
    );
    assert!(
        bad(r#"(widget: Some("Button"), selector: "Button")"#).contains("selector"),
        "unknown fields are refused"
    );
    let many: Vec<String> = (0..65)
        .map(|i| format!(r#"(inside: Some("T{i}"), props: {{}})"#))
        .collect();
    assert!(bad(&many.join(", ")).contains("at most 64"));
    // A rule literal in code works too.
    let mut file = ThemeFile {
        name: "code".to_owned(),
        extends: Some("regia-dark".to_owned()),
        ..ThemeFile::default()
    };
    file.rules.push(StyleRule {
        widget: Some("Button".to_owned()),
        ..StyleRule::default()
    });
    let mut set = builtin_theme_set();
    set.insert(file).expect("inserted");
}

#[test]
fn an_authored_variation_class_wins_over_the_button_constructor_class() {
    // ADR-8303: the widget's own `button` class goes first, so the authored `primary` (later)
    // wins; before, `button` was appended last and the plain Button look covered the variation.
    let mut app = ui_app();
    let doc = r#"(version: 11, root: (children: [
        (name: "go", widget: button, classes: ["primary"], style: (size: (96, 32))),
        (name: "twice", widget: button, classes: ["button", "danger"], style: (size: (96, 32))),
    ]))"#;
    let file = parse_ui_ron(doc, "variation.ui.ron", &UiMigrators::default()).unwrap();
    app.world_mut()
        .resource_mut::<UiLibrary>()
        .templates
        .insert("variation".to_owned(), file);
    spawn_ui(app.world_mut(), "variation").unwrap();
    settle(&mut app);
    let go = named(&mut app, "go");
    let twice = named(&mut app, "twice");
    assert_eq!(app.world().get::<Classes>(go).unwrap().0, ["button", "primary"]);
    assert_eq!(
        app.world().get::<Classes>(twice).unwrap().0,
        ["button", "danger"],
        "an authored `button` is not repeated"
    );
    let dark = builtin_theme_set().resolve("regia-dark", None).unwrap();
    assert_eq!(
        background(&app, go),
        dark.color("color.accent"),
        "Button.primary's fill"
    );
    assert_eq!(
        background(&app, twice),
        dark.color("color.danger"),
        "Button.danger's fill"
    );
}
