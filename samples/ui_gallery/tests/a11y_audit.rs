// SPDX-License-Identifier: MIT-0
//! Runs the accessibility audit (ADR-3025) over every gallery screen, the HUD sample and the template
//! HUD in every shipped theme, and compares each screen's errors and warnings with a checked-in list:
//! `tests/a11y_audit_allowed.txt`, one `screen theme rule severity path` per line. `CONFIRMED_CLEAN`
//! screens have no lines; a new finding fails the test, and so does a line that no longer fires,
//! so the list stays what is true. `info` findings are "needs eyes", not defects, so they are counted
//! in the printout and left off the list.
//!
//! CPU only: the audit reads layout, the accessibility tree and the theme, and draws nothing.
//!
//! To see every finding, or to rewrite the list after a reviewed change:
//! `REGIA_A11Y_AUDIT_OUT=/tmp/out.txt cargo test -p ui_gallery --test a11y_audit`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia::prelude::*;
use regia_ui::a11y_audit::{AuditOptions, AuditProfile, AuditReport, AuditRule, AuditSeverity, audit_ui};
use regia_ui::text::TextSystem;
use regia_ui::theme_resolve::ActiveTheme;
use std::collections::BTreeSet;
use ui_gallery::gallery::UiGallery;
use ui_gallery::hud::HudSample;
use ui_gallery::shots::{headless_app, resize};

/// The first-person template's controls screen: the only UI any project template ships. Compiled
/// from the file `regia new` writes, so the audit sees what a new project gets.
#[path = "../../../crates/regia_templates/templates/first_person/game/src/controls.rs"]
mod template_controls;

const ALLOWED: &str = include_str!("a11y_audit_allowed.txt");

/// Every shipped theme.
const THEMES: [&str; 4] = [
    "regia-dark",
    "regia-light",
    "regia-high-contrast",
    "regia-light-high-contrast",
];

fn key(app: &mut App, name: &str) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: name.to_owned(),
            logical_key: name.to_owned(),
            state,
            repeat: false,
            timestamp: InputTimestamp::ZERO,
        });
        app.update();
    }
}

/// The embedded fonts only, so a machine's system fonts cannot move a label.
fn settle(mut app: App) -> App {
    app.insert_resource(TextSystem::embedded_only());
    app.update();
    resize(&mut app, 1920, 1080, None);
    app
}

fn set_theme(app: &mut App, theme: &str) {
    app.insert_resource(ActiveTheme::named(theme));
    // Past the theme's cross-fade (50 ms per update) and every entry tween.
    for _ in 0..24 {
        app.update();
    }
}

fn gallery_page(keys: &[&str]) -> App {
    let mut app = settle(headless_app(UiGallery));
    for name in keys {
        key(&mut app, name);
    }
    app
}

fn hud() -> App {
    settle(headless_app(HudSample))
}

fn template_hud() -> App {
    let config = regia_input::InputConfig::new()
        .action("Look", regia_input::ActionKind::Axis2D)
        .binding(regia_input::Binding::new(
            "Look",
            regia_input::InputSource::MouseDelta,
        ))
        .binding(regia_input::Binding::new(
            "Look",
            regia_input::InputSource::gamepad_stick(regia_input::Stick::Right),
        ));
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only())
        .add_message::<WindowResized>()
        .add_message::<WindowScaleFactorChanged>()
        .add_message::<ImeEvent>()
        .add_plugins(regia_input::InputPlugin::new(config))
        .add_plugins(template_controls::ControlsPlugin);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(50),
    ));
    let mut app = settle(app);
    key(&mut app, "F10");
    assert!(app.world().resource::<template_controls::ControlsOpen>().0);
    app
}

/// Builds one screen's app, settled and ready to audit.
type BuildScreen = Box<dyn Fn() -> App>;

/// Every audited screen, by the name the allowed list uses.
fn screens() -> Vec<(&'static str, BuildScreen)> {
    vec![
        ("widgets", Box::new(|| gallery_page(&["Digit1"]))),
        ("text", Box::new(|| gallery_page(&["Digit2"]))),
        ("list", Box::new(|| gallery_page(&["Digit3"]))),
        ("wheel", Box::new(|| gallery_page(&["Digit4"]))),
        // Digit5 also opens the menus page; F5 toggles only that page.
        ("theme_sheet", Box::new(|| gallery_page(&["Digit5", "F5"]))),
        ("parity", Box::new(|| gallery_page(&["Digit6"]))),
        ("menus", Box::new(|| gallery_page(&["Digit1", "F5"]))),
        ("dialog", Box::new(|| gallery_page(&["Digit1", "KeyM"]))),
        ("hud", Box::new(hud)),
        ("template_controls", Box::new(template_hud)),
    ]
}

fn audit(app: &mut App, profile: AuditProfile) -> AuditReport {
    audit_ui(
        app.world_mut(),
        &AuditOptions {
            profile,
            ..Default::default()
        },
    )
}

fn listed(screen: &str, theme: &str, report: &AuditReport) -> Vec<String> {
    report
        .findings
        .iter()
        .filter(|f| f.severity != AuditSeverity::Info)
        .map(|f| format!("{screen} {theme} {}", f.key()))
        .collect()
}

fn allowed_list() -> BTreeSet<String> {
    ALLOWED
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_screen_matches_its_list_in_every_theme() {
    let mut found = BTreeSet::new();
    let mut printout = String::new();
    for (screen, build) in screens() {
        let mut app = build();
        for theme in THEMES {
            set_theme(&mut app, theme);
            let report = audit(&mut app, AuditProfile::Desktop);
            assert_eq!(report.theme, theme, "{screen}: the theme did not switch");
            assert!(report.nodes > 0, "{screen} {theme}: nothing visible");
            assert!(report.skipped.is_empty(), "{screen}: {:?}", report.skipped);
            found.extend(listed(screen, theme, &report));
            printout.push_str(&format!(
                "## {screen} {theme}: {} nodes\n{}",
                report.nodes,
                report.to_lines()
            ));
        }
    }
    if let Some(path) = std::env::var_os("REGIA_A11Y_AUDIT_OUT") {
        let mut path = std::path::PathBuf::from(path);
        std::fs::write(&path, &printout).expect("write the printout");
        path.set_extension("allowed");
        let list: Vec<&str> = found.iter().map(String::as_str).collect();
        std::fs::write(&path, list.join("\n") + "\n").expect("write the list");
    }
    let allowed = allowed_list();
    let new: Vec<&String> = found.difference(&allowed).collect();
    let gone: Vec<&String> = allowed.difference(&found).collect();
    assert!(
        new.is_empty() && gone.is_empty(),
        "the audit no longer matches tests/a11y_audit_allowed.txt\nnew findings ({}):\n{}\nno longer found ({}):\n{}",
        new.len(),
        new.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n"),
        gone.len(),
        gone.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n"),
    );
}

/// These screens had no errors or warnings in any theme when the list was reviewed. They may not gain
/// lines: a finding here is a regression to fix, not one to add to the list.
const CONFIRMED_CLEAN: [&str; 4] = ["text", "list", "wheel", "template_controls"];

#[test]
fn the_confirmed_clean_screens_have_no_allowed_findings() {
    let allowed = allowed_list();
    for screen in CONFIRMED_CLEAN {
        assert!(
            screens().iter().any(|(name, _)| *name == screen),
            "{screen} is not an audited screen"
        );
        let prefix = format!("{screen} ");
        let lines: Vec<&String> = allowed.iter().filter(|l| l.starts_with(&prefix)).collect();
        assert!(lines.is_empty(), "{screen} must stay clean: {lines:?}");
    }
}

#[test]
fn a_screen_reports_the_same_bytes_twice() {
    let mut app = gallery_page(&["Digit1"]);
    set_theme(&mut app, "regia-dark");
    let first = audit(&mut app, AuditProfile::Desktop).to_lines();
    let second = audit(&mut app, AuditProfile::Desktop).to_lines();
    assert_eq!(first, second);
}

#[test]
fn auditing_leaves_the_running_app_alone() {
    // The frame after an audit lays nothing out and draws nothing new because of it.
    let mut app = gallery_page(&["Digit2"]);
    set_theme(&mut app, "regia-dark");
    let before = *app.world().resource::<regia_ui::layout::UiLayoutStats>();
    audit(&mut app, AuditProfile::Desktop);
    let after = *app.world().resource::<regia_ui::layout::UiLayoutStats>();
    assert_eq!(before, after, "the audit ran the live layout");
    let scale = app.world().resource::<TextSystem>().text_scale;
    assert_eq!(scale, 1.0, "the audit left its scratch text scale behind");
}

#[test]
fn the_touch_profile_asks_for_more_than_the_desktop_one() {
    let mut app = gallery_page(&["Digit1"]);
    set_theme(&mut app, "regia-dark");
    let desktop = audit(&mut app, AuditProfile::Desktop);
    let touch = audit(&mut app, AuditProfile::Touch);
    assert_eq!(desktop.min_target_px, 24.0);
    assert!(touch.min_target_px > desktop.min_target_px);
    assert!(
        touch.of_rule(AuditRule::TargetSize).count() >= desktop.of_rule(AuditRule::TargetSize).count(),
        "a larger minimum can only add findings"
    );
}

/// The template's controls screen open on the TEST Pixel 9 Pro's window (2142x960 physical px at
/// OS scale 2.25, landscape), in `layout`.
fn template_on_a_phone(layout: template_controls::ControlsLayout) -> App {
    let config = regia_input::InputConfig::new()
        .action("Look", regia_input::ActionKind::Axis2D)
        .binding(regia_input::Binding::new(
            "Look",
            regia_input::InputSource::gamepad_stick(regia_input::Stick::Right),
        ));
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only())
        .add_message::<WindowResized>()
        .add_message::<WindowScaleFactorChanged>()
        .add_message::<ImeEvent>()
        .add_plugins(regia_input::InputPlugin::new(config))
        .insert_resource(layout)
        .add_plugins(template_controls::ControlsPlugin);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(50),
    ));
    app.insert_resource(TextSystem::embedded_only());
    app.update();
    app.insert_resource(regia_ui::UiScales {
        window_size: [2142.0, 960.0],
        scale_factor: 2.25,
    });
    key(&mut app, "F10");
    assert!(app.world().resource::<template_controls::ControlsOpen>().0);
    set_theme(&mut app, "regia-dark");
    app
}

/// ADR-4932, from the TEST Pixel 9 Pro (2026-11-09): the screen ran at UI scale 1.0 on a 2.25x
/// phone, so every control was about 20 dp. The desktop layout must fail the touch audit there
/// (the audit sees the device's density), and the touch layout, the Android default, must pass.
#[test]
fn the_template_controls_screen_is_touch_sized_on_a_phone() {
    let touch = AuditOptions {
        profile: AuditProfile::Touch,
        ..Default::default()
    };
    let mut desktop = template_on_a_phone(template_controls::ControlsLayout::Desktop);
    let report = audit_ui(desktop.world_mut(), &touch);
    let small = report
        .of_rule(AuditRule::TargetSize)
        .filter(|f| f.severity == AuditSeverity::Error)
        .count();
    assert!(small >= 10, "{}", report.to_lines());

    let mut phone = template_on_a_phone(template_controls::ControlsLayout::Touch);
    let report = audit_ui(phone.world_mut(), &touch);
    let defects: Vec<String> = report
        .findings
        .iter()
        .filter(|f| f.severity != AuditSeverity::Info)
        .map(|f| f.key())
        .collect();
    assert!(defects.is_empty(), "{defects:#?}\n{}", report.to_lines());
    assert!(report.nodes > 20, "{}", report.to_lines());
}
