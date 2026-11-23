// SPDX-License-Identifier: MPL-2.0
//! One failing and one passing fixture per rule (ADR-3025), the published contrast endpoints, and
//! the promise that an audit changes nothing and prints the same bytes twice.

use super::*;

use crate::focus::{FocusState, NavLinks};
use crate::node::Disabled;
use crate::style::Style;
use crate::text::{TextSpan, TextStyle, WrapMode};
use crate::tween::{Tween, TweenValue};
use crate::widgets::Button;

const WHITE_PANEL: &str = "#ffffff";
/// Everything fits inside; a sentinel like the layout's "no clip yet".
const NO_CLIP: [f32; 4] = [-100_000.0, -100_000.0, 300_000.0, 300_000.0];

fn hex(s: &str) -> UiColor {
    UiColor::from_hex(s).expect("test hex")
}

fn computed(rect: [f32; 4], paint_order: u32) -> ComputedNode {
    ComputedNode {
        rect,
        clip_rect: NO_CLIP,
        scale: 1.0,
        paint_order,
        ..Default::default()
    }
}

fn base_world() -> World {
    let mut world = World::new();
    world.init_resource::<FocusState>();
    world
}

/// A root with an opaque background over a 1000x600 screen.
fn root(world: &mut World, fill: &str) -> Entity {
    world
        .spawn((
            UiRoot::default(),
            NodeName("screen".to_owned()),
            computed([0.0, 0.0, 1000.0, 600.0], 0),
            Background {
                kind: BackgroundKind::Color(hex(fill)),
            },
        ))
        .id()
}

fn child(world: &mut World, parent: Entity, name: &str, rect: [f32; 4]) -> Entity {
    world
        .spawn((
            UiNode,
            NodeName(name.to_owned()),
            computed(rect, 1),
            ChildOf(parent),
        ))
        .id()
}

fn label(text: &str, color: UiColor, font_px: f32, bold: bool) -> Text {
    Text {
        spans: vec![TextSpan {
            content: TextContent::Lit(text.to_owned()),
            style: TextStyle {
                color,
                font_px,
                bold,
                ..Default::default()
            },
        }],
        ..Default::default()
    }
}

fn only(rule: AuditRule) -> AuditOptions {
    AuditOptions {
        rules: vec![rule],
        ..Default::default()
    }
}

fn run(world: &mut World, rule: AuditRule) -> AuditReport {
    audit_ui(world, &only(rule))
}

fn ring() -> Themed {
    Themed {
        focus_ring: Some(hex("#000000")),
        focus_ring_width: Some(2.0),
        focus_ring_offset: Some(2.0),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------------------------
// The contrast function itself
// ---------------------------------------------------------------------------------------------

#[test]
fn published_contrast_endpoints() {
    // Black on white is the formula's maximum, a color on itself its minimum.
    assert!((contrast_ratio(hex("#000000"), hex("#ffffff")) - 21.0).abs() < 0.01);
    assert!((contrast_ratio(hex("#777777"), hex("#777777")) - 1.0).abs() < 1e-5);
    // The grey pair commonly quoted around the AA line: #767676 on white passes 4.5, #777777 fails.
    let pass = contrast_ratio(hex("#767676"), hex("#ffffff"));
    let fail = contrast_ratio(hex("#777777"), hex("#ffffff"));
    assert!((4.5..4.6).contains(&pass), "{pass}");
    assert!((4.4..4.5).contains(&fail), "{fail}");
}

#[test]
fn every_rule_has_a_lowercase_code_a_criterion_and_parses_back() {
    for rule in AuditRule::ALL {
        let code = rule.code();
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        );
        assert_eq!(AuditRule::parse(code), Some(rule));
        assert!(!rule.criterion().is_empty());
    }
    assert_eq!(AuditRule::parse("a11y_nope"), None);
}

// ---------------------------------------------------------------------------------------------
// a11y_unnamed_interactive
// ---------------------------------------------------------------------------------------------

#[test]
fn a_button_with_no_name_fails() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let close = child(&mut world, screen, "close", [10.0, 10.0, 100.0, 40.0]);
    world
        .entity_mut(close)
        .insert((Button, Focusable::default(), Interactable::default()));
    let report = run(&mut world, AuditRule::UnnamedInteractive);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    let finding = &report.findings[0];
    assert_eq!(finding.severity, AuditSeverity::Error);
    assert_eq!(finding.path, "screen/close");
    assert_eq!(finding.criterion, "4.1.2");
    assert_eq!(finding.measured, "no accessible name");
}

#[test]
fn a_button_named_by_its_label_or_an_override_passes() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let ok = child(&mut world, screen, "ok", [10.0, 10.0, 100.0, 40.0]);
    world
        .entity_mut(ok)
        .insert((Button, Focusable::default(), Interactable::default()));
    let text = child(&mut world, ok, "ok.label", [10.0, 10.0, 100.0, 40.0]);
    world.entity_mut(text).insert(Text::new("OK"));
    let icon = child(&mut world, screen, "icon", [10.0, 60.0, 40.0, 40.0]);
    world.entity_mut(icon).insert((
        Button,
        Focusable::default(),
        Interactable::default(),
        Accessible::default().with_label("Settings"),
    ));
    let report = run(&mut world, AuditRule::UnnamedInteractive);
    assert!(report.is_clean(), "{}", report.to_lines());
}

#[test]
fn a_whitespace_name_counts_as_no_name_and_decoration_is_named_as_an_exemption() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let blank = child(&mut world, screen, "blank", [10.0, 10.0, 100.0, 40.0]);
    world.entity_mut(blank).insert((
        Button,
        Focusable::default(),
        Interactable::default(),
        Accessible::default().with_label("   "),
    ));
    let catcher = child(&mut world, screen, "catcher", [0.0, 100.0, 100.0, 40.0]);
    world
        .entity_mut(catcher)
        .insert((Interactable::default(), Accessible::decorative()));
    let report = run(&mut world, AuditRule::UnnamedInteractive);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].path, "screen/blank");
    assert_eq!(report.exemptions[0].exemption, "decoration");
    assert_eq!(report.exemptions[0].path, "screen/catcher");
}

// ---------------------------------------------------------------------------------------------
// a11y_text_contrast
// ---------------------------------------------------------------------------------------------

#[test]
fn white_text_on_a_light_panel_fails_with_the_measured_ratio() {
    let mut world = base_world();
    let screen = root(&mut world, "#101010");
    let panel = child(&mut world, screen, "panel", [0.0, 0.0, 400.0, 200.0]);
    world.entity_mut(panel).insert(Background {
        kind: BackgroundKind::Color(hex("#eeeeee")),
    });
    let text = child(&mut world, panel, "title", [10.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(text)
        .insert(label("Hello", UiColor::WHITE, 18.0, false));
    let report = run(&mut world, AuditRule::TextContrast);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    let finding = &report.findings[0];
    assert_eq!(finding.severity, AuditSeverity::Error);
    assert_eq!(finding.required, "4.50:1");
    assert_eq!(finding.criterion, "1.4.3");
    assert!(finding.measured.starts_with("1.1"), "{}", finding.measured);
}

/// A group layer composites at its `GroupOpacity`: white text on black inside a group at 0.15
/// over black shows at 4:1 (linear blend), and a faded-out dialog does not hide what is under it.
#[test]
fn group_opacity_fades_text_and_does_not_cover() {
    let mut world = base_world();
    let screen = root(&mut world, "#000000");
    let faded = child(&mut world, screen, "faded", [0.0, 0.0, 400.0, 100.0]);
    world
        .entity_mut(faded)
        .insert(crate::group_layer::GroupOpacity(0.15));
    let text = child(&mut world, faded, "title", [10.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(text)
        .insert(label("Hello", UiColor::WHITE, 18.0, false));
    let report = run(&mut world, AuditRule::TextContrast);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    // Linear blending (`UiColor` is linear): 0.15 of white over black has luminance 0.15, so
    // (0.15 + 0.05) / 0.05 = 4.00, under the 4.5 AA line. Without the group it measured 21:1.
    assert_eq!(report.findings[0].measured, "4.00:1");

    // A dialog faded to nothing over a label: the label is still checked.
    let mut world = base_world();
    let screen = root(&mut world, "#eeeeee");
    let text = child(&mut world, screen, "under", [10.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(text)
        .insert(label("Hello", UiColor::WHITE, 18.0, false));
    let dialog = child(&mut world, screen, "dialog", [0.0, 0.0, 400.0, 200.0]);
    world.entity_mut(dialog).insert((
        crate::group_layer::GroupOpacity(0.0),
        Background {
            kind: BackgroundKind::Color(hex("#eeeeee")),
        },
    ));
    let report = run(&mut world, AuditRule::TextContrast);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert!(report.findings[0].path.ends_with("under"));
}

#[test]
fn dark_text_on_a_light_panel_passes() {
    let mut world = base_world();
    let screen = root(&mut world, "#eeeeee");
    let text = child(&mut world, screen, "title", [10.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(text)
        .insert(label("Hello", hex("#111111"), 18.0, false));
    assert!(run(&mut world, AuditRule::TextContrast).is_clean());
}

#[test]
fn large_text_needs_only_three_to_one() {
    // #808080 on white is about 3.95:1: body text fails, large and large-bold text pass.
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    for (name, px, bold, y) in [
        ("body", 18.0, false, 0.0),
        ("large", 24.0, false, 40.0),
        ("large_bold", 19.0, true, 80.0),
    ] {
        let text = child(&mut world, screen, name, [0.0, y, 200.0, 30.0]);
        world
            .entity_mut(text)
            .insert(label("Text", hex("#808080"), px, bold));
    }
    let report = run(&mut world, AuditRule::TextContrast);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].path, "screen/body");
}

#[test]
fn a_translucent_fill_is_composited_with_what_is_under_it() {
    // A half-white panel on black is a mid grey: white text fails on it but passes on plain black.
    let mut world = base_world();
    let screen = root(&mut world, "#000000");
    let panel = child(&mut world, screen, "glass", [0.0, 0.0, 400.0, 200.0]);
    world.entity_mut(panel).insert(Background {
        kind: BackgroundKind::Color(UiColor::WHITE.with_alpha(0.5)),
    });
    let text = child(&mut world, panel, "on_glass", [10.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(text)
        .insert(label("Hello", UiColor::WHITE, 18.0, false));
    let free = child(&mut world, screen, "on_black", [500.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(free)
        .insert(label("Hello", UiColor::WHITE, 18.0, false));
    let report = run(&mut world, AuditRule::TextContrast);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].path, "screen/glass/on_glass");
    assert!(
        report.findings[0].measured.starts_with("1.9"),
        "{}",
        report.findings[0].measured
    );
}

#[test]
fn a_sibling_panel_behind_the_text_counts_not_just_the_ancestors() {
    // The ancestors alone would say black; the panel painted before the text is what shows.
    let mut world = base_world();
    let screen = root(&mut world, "#000000");
    let panel = child(&mut world, screen, "panel", [0.0, 0.0, 400.0, 200.0]);
    world.entity_mut(panel).insert(Background {
        kind: BackgroundKind::Color(hex("#f0f0f0")),
    });
    let text = child(&mut world, screen, "floating", [10.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(text)
        .insert(label("Hello", UiColor::WHITE, 18.0, false));
    let report = run(&mut world, AuditRule::TextContrast);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].severity, AuditSeverity::Error);
}

#[test]
fn text_over_an_image_is_info_and_needs_eyes_never_a_pass() {
    let mut world = base_world();
    let screen = root(&mut world, "#000000");
    let picture = child(&mut world, screen, "picture", [0.0, 0.0, 400.0, 200.0]);
    world.entity_mut(picture).insert(UiImage::default());
    let text = child(&mut world, picture, "caption", [10.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(text)
        .insert(label("Hello", UiColor::WHITE, 18.0, false));
    let report = run(&mut world, AuditRule::TextContrast);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    let finding = &report.findings[0];
    assert_eq!(finding.severity, AuditSeverity::Info);
    assert!(finding.needs_eyes);
    assert!(finding.measured.contains("an image"), "{}", finding.measured);
    assert!(!report.is_clean(), "an unknown background is not a pass");
}

#[test]
fn an_opaque_fill_over_an_image_is_known_again() {
    let mut world = base_world();
    let screen = root(&mut world, "#000000");
    let picture = child(&mut world, screen, "picture", [0.0, 0.0, 400.0, 200.0]);
    world.entity_mut(picture).insert(UiImage::default());
    let plate = child(&mut world, picture, "plate", [0.0, 0.0, 400.0, 200.0]);
    world.entity_mut(plate).insert(Background {
        kind: BackgroundKind::Color(hex("#000000")),
    });
    let text = child(&mut world, plate, "caption", [10.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(text)
        .insert(label("Hello", UiColor::WHITE, 18.0, false));
    assert!(run(&mut world, AuditRule::TextContrast).is_clean());
}

#[test]
fn text_with_nothing_opaque_behind_it_is_unknown_the_scene_shows() {
    let mut world = base_world();
    let screen = world
        .spawn((UiRoot::default(), computed([0.0, 0.0, 1000.0, 600.0], 0)))
        .id();
    let text = child(&mut world, screen, "hud_score", [10.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(text)
        .insert(label("Score", UiColor::WHITE, 18.0, false));
    let report = run(&mut world, AuditRule::TextContrast);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].severity, AuditSeverity::Info);
    assert!(report.findings[0].measured.contains("no opaque fill"));
}

#[test]
fn a_gradient_failing_on_part_of_its_length_is_a_warning_that_needs_eyes() {
    let mut world = base_world();
    let screen = root(&mut world, "#000000");
    let panel = child(&mut world, screen, "banner", [0.0, 0.0, 400.0, 200.0]);
    world.entity_mut(panel).insert(Background {
        kind: BackgroundKind::Gradient {
            start: hex("#000000"),
            end: hex("#ffffff"),
            angle: 0.0,
        },
    });
    let text = child(&mut world, panel, "title", [10.0, 10.0, 200.0, 30.0]);
    world
        .entity_mut(text)
        .insert(label("Hello", UiColor::WHITE, 18.0, false));
    let report = run(&mut world, AuditRule::TextContrast);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].severity, AuditSeverity::Warning);
    assert!(report.findings[0].needs_eyes);
}

#[test]
fn disabled_and_decorative_text_are_excused_by_name() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let off = child(&mut world, screen, "off", [0.0, 0.0, 200.0, 30.0]);
    world
        .entity_mut(off)
        .insert((label("Off", hex("#eeeeee"), 18.0, false), Disabled));
    let trim = child(&mut world, screen, "trim", [0.0, 40.0, 200.0, 30.0]);
    world.entity_mut(trim).insert((
        label("~~~", hex("#eeeeee"), 18.0, false),
        Accessible::decorative(),
    ));
    let report = run(&mut world, AuditRule::TextContrast);
    assert!(report.is_clean(), "{}", report.to_lines());
    let names: Vec<&str> = report.exemptions.iter().map(|e| e.exemption).collect();
    assert_eq!(names, ["inactive_component", "decoration"]);
}

#[test]
fn text_under_an_opaque_cover_is_not_visible_and_a_scrim_tints_it() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let hidden = child(&mut world, screen, "hidden", [0.0, 0.0, 200.0, 30.0]);
    world
        .entity_mut(hidden)
        .insert(label("Under", hex("#111111"), 18.0, false));
    let cover = child(&mut world, screen, "cover", [0.0, 0.0, 200.0, 30.0]);
    world.entity_mut(cover).insert(Background {
        kind: BackgroundKind::Color(hex("#222222")),
    });
    let tinted = child(&mut world, screen, "tinted", [0.0, 100.0, 200.0, 30.0]);
    world
        .entity_mut(tinted)
        .insert(label("Under", hex("#111111"), 18.0, false));
    // A heavy scrim drags dark text on white down to almost nothing.
    let scrim = child(&mut world, screen, "scrim", [0.0, 100.0, 200.0, 30.0]);
    world.entity_mut(scrim).insert(Background {
        kind: BackgroundKind::Color(UiColor::BLACK.with_alpha(0.97)),
    });
    let report = run(&mut world, AuditRule::TextContrast);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].path, "screen/tinted");
    assert_eq!(report.exemptions[0].exemption, "not_visible");
    assert_eq!(report.exemptions[0].path, "screen/hidden");
}

// ---------------------------------------------------------------------------------------------
// a11y_nontext_contrast
// ---------------------------------------------------------------------------------------------

fn field(world: &mut World, parent: Entity, name: &str, themed: Themed) -> Entity {
    let node = child(world, parent, name, [10.0, 10.0, 200.0, 40.0]);
    world.entity_mut(node).insert((
        crate::widgets::TextInput::default(),
        Focusable::default(),
        Interactable::default(),
        themed,
    ));
    node
}

#[test]
fn a_faint_ring_and_a_faint_field_boundary_both_fail() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    field(
        &mut world,
        screen,
        "email",
        Themed {
            focus_ring: Some(hex("#eeeeee")),
            focus_ring_width: Some(2.0),
            border_width: Some(1.0),
            border_color: Some(hex("#eeeeee")),
            ..Default::default()
        },
    );
    let report = run(&mut world, AuditRule::NontextContrast);
    assert_eq!(report.findings.len(), 2, "{}", report.to_lines());
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.measured.starts_with("focus ring"))
    );
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.measured.starts_with("text_input boundary"))
    );
    assert!(report.findings.iter().all(|f| f.required == "3.00:1"));
    assert!(report.findings.iter().all(|f| f.criterion == "1.4.11"));
}

#[test]
fn a_strong_ring_and_boundary_pass() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    field(
        &mut world,
        screen,
        "email",
        Themed {
            border_width: Some(1.0),
            border_color: Some(hex("#555555")),
            ..ring()
        },
    );
    let report = run(&mut world, AuditRule::NontextContrast);
    assert!(report.is_clean(), "{}", report.to_lines());
}

#[test]
fn a_labeled_button_needs_no_boundary_and_says_why() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let button = child(&mut world, screen, "ok", [10.0, 10.0, 100.0, 40.0]);
    world
        .entity_mut(button)
        .insert((Button, Focusable::default(), Interactable::default(), ring()));
    let text = child(&mut world, button, "label", [10.0, 10.0, 100.0, 40.0]);
    world.entity_mut(text).insert(Text::new("OK"));
    let report = run(&mut world, AuditRule::NontextContrast);
    assert!(report.is_clean(), "{}", report.to_lines());
    assert_eq!(report.exemptions[0].exemption, "text_identifies_control");
}

#[test]
fn a_ring_on_an_unknown_background_needs_eyes() {
    let mut world = base_world();
    let screen = world
        .spawn((UiRoot::default(), computed([0.0, 0.0, 1000.0, 600.0], 0)))
        .id();
    let toggle = child(&mut world, screen, "toggle", [10.0, 10.0, 60.0, 30.0]);
    world.entity_mut(toggle).insert((
        Toggle::default(),
        Focusable::default(),
        Interactable::default(),
        ring(),
    ));
    let report = run(&mut world, AuditRule::NontextContrast);
    assert!(!report.findings.is_empty());
    assert!(report.findings.iter().all(|f| f.severity == AuditSeverity::Info));
    assert!(report.findings.iter().all(|f| f.needs_eyes));
}

// ---------------------------------------------------------------------------------------------
// a11y_target_size
// ---------------------------------------------------------------------------------------------

fn target(world: &mut World, parent: Entity, name: &str, rect: [f32; 4]) -> Entity {
    let node = child(world, parent, name, rect);
    world
        .entity_mut(node)
        .insert((Button, Focusable::default(), Interactable::default()));
    node
}

#[test]
fn a_twelve_pixel_close_box_next_to_a_button_fails() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    target(&mut world, screen, "close", [100.0, 100.0, 12.0, 12.0]);
    target(&mut world, screen, "neighbor", [114.0, 100.0, 40.0, 40.0]);
    let report = run(&mut world, AuditRule::TargetSize);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    let finding = &report.findings[0];
    assert_eq!(finding.path, "screen/close");
    assert_eq!(finding.measured, "12.0x12.0 px");
    assert_eq!(finding.required, "24.0x24.0 px");
    assert_eq!(finding.criterion, "2.5.8");
}

#[test]
fn a_big_enough_target_passes_and_size_is_measured_in_ui_px() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let node = target(&mut world, screen, "ok", [100.0, 100.0, 48.0, 48.0]);
    // 48 physical px at scale 2 is exactly the minimum in UI px.
    world.entity_mut(node).insert(ComputedNode {
        scale: 2.0,
        ..computed([100.0, 100.0, 48.0, 48.0], 1)
    });
    assert!(run(&mut world, AuditRule::TargetSize).is_clean());
}

#[test]
fn a_lone_small_target_is_excused_by_spacing_and_the_exception_is_named() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    target(&mut world, screen, "close", [100.0, 100.0, 12.0, 12.0]);
    target(&mut world, screen, "far", [400.0, 100.0, 40.0, 40.0]);
    let report = run(&mut world, AuditRule::TargetSize);
    assert!(report.is_clean(), "{}", report.to_lines());
    assert_eq!(report.exemptions[0].exemption, "spacing");
    assert_eq!(report.exemptions[0].path, "screen/close");
}

#[test]
fn two_small_targets_whose_circles_touch_both_fail() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    // Centers 20 px apart: the 24 px circles overlap, though the boxes do not touch.
    target(&mut world, screen, "a", [100.0, 100.0, 10.0, 10.0]);
    target(&mut world, screen, "b", [120.0, 100.0, 10.0, 10.0]);
    assert_eq!(run(&mut world, AuditRule::TargetSize).findings.len(), 2);
}

#[test]
fn spacing_uses_the_hit_box_not_only_the_drawn_rect() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    // The close box's 24 px circle reaches x = 117 (center 105, radius 12).
    target(&mut world, screen, "close", [100.0, 100.0, 10.0, 10.0]);
    // The drawn rect starts at x = 118, outside that circle. Growing the hit box to 24 px
    // pulls its left edge back to x = 111, so the circle overlaps the target the player hits.
    let neighbor = target(&mut world, screen, "neighbor", [118.0, 100.0, 10.0, 10.0]);
    world
        .entity_mut(neighbor)
        .insert(crate::TouchTarget { min_px: 24.0 });
    let report = run(&mut world, AuditRule::TargetSize);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].path, "screen/close");
    assert!(
        report.exemptions.iter().all(|e| e.exemption != "spacing"),
        "{}",
        report.to_lines()
    );
}

#[test]
fn the_touch_profile_raises_the_minimum_and_spacing_no_longer_excuses() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    target(&mut world, screen, "ok", [100.0, 100.0, 40.0, 40.0]);
    assert!(run(&mut world, AuditRule::TargetSize).is_clean());
    let report = audit_ui(
        &mut world,
        &AuditOptions {
            profile: AuditProfile::Touch,
            ..only(AuditRule::TargetSize)
        },
    );
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].required, "44.0x44.0 px");
    assert_eq!(report.min_target_px, TOUCH_MIN_TARGET_PX);
    // An explicit minimum wins over the profile.
    let report = audit_ui(
        &mut world,
        &AuditOptions {
            profile: AuditProfile::Touch,
            min_target_px: Some(40.0),
            ..only(AuditRule::TargetSize)
        },
    );
    assert!(report.is_clean(), "{}", report.to_lines());
}

/// ADR-4932, from the TEST Pixel 9 Pro (2026-11-09): the controls screen ran at UI scale 1.0 on a
/// 2.25x screen, so its 44 UI px hit boxes were 44 physical px, about 20 dp, and the touch profile
/// still passed them. Under the touch profile a target is measured in device-independent px.
#[test]
fn the_touch_profile_measures_targets_in_device_independent_px() {
    let touch = AuditOptions {
        profile: AuditProfile::Touch,
        ..only(AuditRule::TargetSize)
    };
    let phone = UiScales {
        window_size: [2142.0, 960.0],
        scale_factor: 2.25,
    };
    let mut world = base_world();
    world.insert_resource(phone);
    let screen = root(&mut world, WHITE_PANEL);
    target(&mut world, screen, "toggle", [100.0, 100.0, 44.0, 44.0]);
    let report = audit_ui(&mut world, &touch);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].measured, "19.6x19.6 px");
    assert_eq!(report.findings[0].required, "44.0x44.0 px");
    // The desktop profile keeps measuring UI px (SC 2.5.8's 24).
    assert!(run(&mut world, AuditRule::TargetSize).is_clean());

    // The same control laid out at the OS density is touch-sized.
    let mut world = base_world();
    world.insert_resource(phone);
    let screen = root(&mut world, WHITE_PANEL);
    let toggle = target(&mut world, screen, "toggle", [100.0, 100.0, 99.0, 99.0]);
    world.get_mut::<ComputedNode>(toggle).expect("laid out").scale = 2.25;
    let report = audit_ui(&mut world, &touch);
    assert!(report.is_clean(), "{}", report.to_lines());

    // Without a window (no `UiScales`) or at OS scale 1 the touch profile is unchanged.
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    target(&mut world, screen, "toggle", [100.0, 100.0, 44.0, 44.0]);
    assert!(audit_ui(&mut world, &touch).is_clean());
}

#[test]
fn declared_exceptions_are_listed_and_disabled_controls_are_not_targets() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let inline = target(&mut world, screen, "link", [100.0, 100.0, 30.0, 10.0]);
    world.entity_mut(inline).insert(TargetExemption::Inline);
    let neighbor = target(&mut world, screen, "off", [100.0, 112.0, 30.0, 10.0]);
    world.entity_mut(neighbor).insert(Disabled);
    let report = run(&mut world, AuditRule::TargetSize);
    assert!(report.is_clean(), "{}", report.to_lines());
    let names: Vec<&str> = report.exemptions.iter().map(|e| e.exemption).collect();
    assert_eq!(names, ["inline", "inactive_component"]);
}

// ---------------------------------------------------------------------------------------------
// a11y_focus_unreachable
// ---------------------------------------------------------------------------------------------

#[test]
fn a_node_no_move_reaches_fails() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    // Two nodes in a trap: moves from them never leave, so the one outside stays out of reach.
    let trap = child(&mut world, screen, "trap", [0.0, 0.0, 300.0, 100.0]);
    world
        .entity_mut(trap)
        .insert(crate::focus_graph::FocusGroup::trap());
    let a = target(&mut world, trap, "a", [0.0, 0.0, 100.0, 40.0]);
    target(&mut world, trap, "b", [120.0, 0.0, 100.0, 40.0]);
    let outside = target(&mut world, screen, "outside", [500.0, 500.0, 100.0, 40.0]);
    world.resource_mut::<FocusState>().focused = Some(a);
    let report = run(&mut world, AuditRule::FocusUnreachable);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    let finding = &report.findings[0];
    assert_eq!(finding.measured, "unreachable");
    assert_eq!(finding.node, outside.to_bits());
    assert_eq!(finding.path, "screen/outside");
    assert_eq!(finding.criterion, "2.4.3");
    assert_eq!(finding.severity, AuditSeverity::Error);
}

#[test]
fn a_connected_screen_passes() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    target(&mut world, screen, "a", [0.0, 0.0, 100.0, 40.0]);
    target(&mut world, screen, "b", [0.0, 60.0, 100.0, 40.0]);
    let report = run(&mut world, AuditRule::FocusUnreachable);
    assert!(report.is_clean(), "{}", report.to_lines());
}

#[test]
fn a_button_that_takes_clicks_but_no_focus_fails() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let node = child(&mut world, screen, "buy", [0.0, 0.0, 100.0, 40.0]);
    world.entity_mut(node).insert((Button, Interactable::default()));
    let report = run(&mut world, AuditRule::FocusUnreachable);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert_eq!(report.findings[0].measured, "not_focusable");
}

#[test]
fn the_same_lint_as_ui_nav_lint_decides_reachability() {
    // Explicit links only move focus where they point: nav_lint's verdict is the audit's.
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let a = target(&mut world, screen, "a", [0.0, 0.0, 100.0, 40.0]);
    let b = target(&mut world, screen, "b", [0.0, 60.0, 100.0, 40.0]);
    world.entity_mut(a).insert(Focusable {
        nav: NavLinks {
            down: Some(b),
            ..NavLinks::default()
        },
    });
    let graph = world_graph(&mut world);
    let lint_unreachable = graph
        .lint(Some(a), &[])
        .iter()
        .filter(|i| i.kind == NavIssueKind::Unreachable)
        .count();
    let report = run(&mut world, AuditRule::FocusUnreachable);
    let audit_unreachable = report
        .findings
        .iter()
        .filter(|f| f.measured == "unreachable")
        .count();
    assert_eq!(lint_unreachable, audit_unreachable);
}

// ---------------------------------------------------------------------------------------------
// a11y_no_focus_indicator
// ---------------------------------------------------------------------------------------------

#[test]
fn a_focusable_node_with_no_theme_has_no_ring_and_fails() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    target(&mut world, screen, "plain", [0.0, 0.0, 100.0, 40.0]);
    let zero = target(&mut world, screen, "zero", [0.0, 60.0, 100.0, 40.0]);
    world.entity_mut(zero).insert(Themed {
        focus_ring_width: Some(0.0),
        ..ring()
    });
    let report = run(&mut world, AuditRule::NoFocusIndicator);
    assert_eq!(report.findings.len(), 2, "{}", report.to_lines());
    assert!(report.findings.iter().all(|f| f.measured == "no ring"));
    assert!(report.findings.iter().all(|f| f.criterion == "2.4.7"));
}

#[test]
fn a_themed_ring_passes_and_a_game_drawn_indicator_needs_eyes() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let node = target(&mut world, screen, "ok", [50.0, 50.0, 100.0, 40.0]);
    world.entity_mut(node).insert(ring());
    assert!(run(&mut world, AuditRule::NoFocusIndicator).is_clean());
    world.insert_resource(FocusRingPolicy::Never);
    let report = run(&mut world, AuditRule::NoFocusIndicator);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].severity, AuditSeverity::Info);
    assert!(report.findings[0].needs_eyes);
}

#[test]
fn a_ring_the_clip_cuts_entirely_fails() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let node = target(&mut world, screen, "row", [50.0, 50.0, 100.0, 40.0]);
    world.entity_mut(node).insert((
        ring(),
        ComputedNode {
            clip_rect: [50.0, 50.0, 100.0, 40.0],
            ..computed([50.0, 50.0, 100.0, 40.0], 1)
        },
    ));
    let report = run(&mut world, AuditRule::NoFocusIndicator);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert!(report.findings[0].measured.contains("cuts the whole ring"));
    // The inset ring the list rows use lies inside the same clip.
    world.entity_mut(node).insert(Themed {
        focus_ring_offset: Some(-2.0),
        ..ring()
    });
    assert!(run(&mut world, AuditRule::NoFocusIndicator).is_clean());
}

#[test]
fn a_node_that_clips_its_own_children_still_shows_its_own_ring() {
    // Layout folds a clipping node's own rect into its `clip_rect`, for its children; `extract_ui`
    // paints the node itself under its parent's clip. A scrolling text field does this.
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let node = target(&mut world, screen, "field", [50.0, 50.0, 100.0, 40.0]);
    world.entity_mut(node).insert((
        ring(),
        ClipChildren,
        ComputedNode {
            clip_rect: [50.0, 50.0, 100.0, 40.0],
            ..computed([50.0, 50.0, 100.0, 40.0], 1)
        },
    ));
    let report = run(&mut world, AuditRule::NoFocusIndicator);
    assert!(report.is_clean(), "{}", report.to_lines());
}

/// `RoundedClip` clips children like `ClipChildren` (layout and extract say so): the node's own
/// ring paints under its parent's clip, as for any clipping node.
#[test]
fn a_rounded_clip_node_still_shows_its_own_ring() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let node = target(&mut world, screen, "card", [50.0, 50.0, 100.0, 40.0]);
    world.entity_mut(node).insert((
        ring(),
        crate::group_layer::RoundedClip,
        ComputedNode {
            clip_rect: [50.0, 50.0, 100.0, 40.0],
            ..computed([50.0, 50.0, 100.0, 40.0], 1)
        },
    ));
    let report = run(&mut world, AuditRule::NoFocusIndicator);
    assert!(report.is_clean(), "{}", report.to_lines());
}

// ---------------------------------------------------------------------------------------------
// a11y_text_clipped_at_scale (real layout, in a scratch world)
// ---------------------------------------------------------------------------------------------

/// A world that lays out for real at 800x600 with only the embedded fonts.
fn layout_world() -> World {
    let mut world = base_world();
    world.insert_resource(TextSystem::embedded_only());
    world.insert_resource(Localization::default());
    world.insert_resource(UiScales {
        window_size: [800.0, 600.0],
        scale_factor: 1.0,
    });
    world.insert_resource(LayoutDirty(true));
    world.init_resource::<crate::layout::UiLayoutStats>();
    world
}

fn laid_out_root(world: &mut World) -> Entity {
    world
        .spawn((
            UiRoot {
                scale_mode: ScaleMode::ConstantPixelSize,
                ..Default::default()
            },
            NodeName("screen".to_owned()),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                ..Default::default()
            }),
        ))
        .id()
}

/// A box of `w`x`h` px that clips (`clip`) holding one unwrapped label.
fn fixed_box(world: &mut World, parent: Entity, w: Dimension, h: Dimension, clip: bool) -> (Entity, Entity) {
    let boxed = world
        .spawn((
            UiNode,
            NodeName("box".to_owned()),
            NodeStyle::new(Style {
                size: [w, h],
                overflow: if clip {
                    [Overflow::Clip, Overflow::Clip]
                } else {
                    [Overflow::Visible, Overflow::Visible]
                },
                ..Default::default()
            }),
            ChildOf(parent),
        ))
        .id();
    let text = world
        .spawn((
            UiNode,
            NodeName("label".to_owned()),
            Text {
                wrap: WrapMode::None,
                ..Text::new("Start the game")
            },
            ChildOf(boxed),
        ))
        .id();
    (boxed, text)
}

#[test]
fn a_label_that_fits_at_full_size_but_not_at_double_is_reported() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    // "Start the game" at 18 px is about 125 px wide: it fits 150 px, and at 36 px it does not.
    fixed_box(
        &mut world,
        screen,
        Dimension::Points(150.0),
        Dimension::Points(40.0),
        true,
    );
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    let finding = &report.findings[0];
    assert_eq!(finding.severity, AuditSeverity::Error);
    assert_eq!(finding.path, "screen/box/label");
    assert!(finding.measured.contains("cuts"), "{}", finding.measured);
    assert!(!finding.measured.contains("100 %"), "{}", finding.measured);
    assert_eq!(finding.required, "no text lost at 200 %");
    assert_eq!(finding.criterion, "1.4.4");
}

#[test]
fn a_box_that_grows_with_its_label_passes() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    fixed_box(&mut world, screen, Dimension::Auto, Dimension::Auto, true);
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert!(report.is_clean(), "{}", report.to_lines());
}

#[test]
fn text_cut_at_full_size_too_says_so() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    fixed_box(
        &mut world,
        screen,
        Dimension::Points(40.0),
        Dimension::Points(40.0),
        true,
    );
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert!(report.findings[0].measured.contains("at 100 % too"));
}

#[test]
fn an_ellipsis_at_the_raised_scale_is_lost_text() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    let (_, text) = fixed_box(
        &mut world,
        screen,
        Dimension::Points(150.0),
        Dimension::Points(40.0),
        false,
    );
    world.entity_mut(text).insert((
        Text {
            wrap: WrapMode::None,
            overflow: TextOverflow::Ellipsis,
            ..Text::new("Start the game")
        },
        NodeStyle::new(Style {
            size: [Dimension::Points(150.0), Dimension::Auto],
            ..Default::default()
        }),
    ));
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert!(report.findings[0].measured.contains("ellipsis"));
}

#[test]
fn text_inside_a_scroll_view_stays_reachable() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    let (boxed, _) = fixed_box(
        &mut world,
        screen,
        Dimension::Points(150.0),
        Dimension::Points(40.0),
        true,
    );
    world.entity_mut(boxed).insert(ScrollView::default());
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert!(report.is_clean(), "{}", report.to_lines());
}

#[test]
fn a_list_below_the_fold_of_a_scrolling_page_stays_reachable() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    // A page that fits the window and scrolls, with a tall spacer pushing a clipped list below it.
    let page = world
        .spawn((
            UiNode,
            NodeName("page".to_owned()),
            NodeStyle::new(Style {
                direction: crate::style::FlexDirection::Column,
                size: [Dimension::Points(400.0), Dimension::Points(300.0)],
                overflow: [Overflow::Clip, Overflow::Clip],
                ..Default::default()
            }),
            ScrollView::vertical(),
            ChildOf(screen),
        ))
        .id();
    world.spawn((
        UiNode,
        NodeStyle::new(Style {
            size: [Dimension::Points(100.0), Dimension::Points(2000.0)],
            ..Default::default()
        }),
        ChildOf(page),
    ));
    let list = world
        .spawn((
            UiNode,
            NodeName("list".to_owned()),
            NodeStyle::new(Style {
                direction: crate::style::FlexDirection::Column,
                size: [Dimension::Points(300.0), Dimension::Points(40.0)],
                overflow: [Overflow::Clip, Overflow::Clip],
                ..Default::default()
            }),
            ScrollView::vertical(),
            ChildOf(page),
        ))
        .id();
    for _ in 0..4 {
        world.spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Points(300.0), Dimension::Points(28.0)],
                ..Default::default()
            }),
            Text::new("Row"),
            ChildOf(list),
        ));
    }
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert!(report.is_clean(), "{}", report.to_lines());
}

#[test]
fn a_scroll_view_that_runs_off_the_screen_does_not_excuse_its_text() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    let view = world
        .spawn((
            UiNode,
            NodeName("view".to_owned()),
            NodeStyle::new(Style {
                size: [Dimension::Points(300.0), Dimension::Points(5000.0)],
                ..Default::default()
            }),
            ScrollView::vertical(),
            ChildOf(screen),
        ))
        .id();
    world.spawn((
        UiNode,
        NodeName("far".to_owned()),
        NodeStyle::new(Style {
            position: crate::style::PositionType::Absolute,
            inset: crate::style::Edges {
                top: Dimension::Points(2000.0),
                ..crate::style::Edges::all(Dimension::Auto)
            },
            ..Default::default()
        }),
        Text::new("Far below"),
        ChildOf(view),
    ));
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.measured.contains("past the screen")),
        "{}",
        report.to_lines()
    );
}

#[test]
fn text_reported_as_lost_gets_no_second_line_for_spilling() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    let (boxed, _) = fixed_box(
        &mut world,
        screen,
        Dimension::Points(790.0),
        Dimension::Points(40.0),
        false,
    );
    world.entity_mut(boxed).insert(Background {
        kind: BackgroundKind::Color(UiColor::BLACK),
    });
    let long = "A long sentence that fits this wide box at its authored size but not at double";
    let mut text_query = world.query::<&mut Text>();
    for mut text in text_query.iter_mut(&mut world) {
        *text = Text {
            wrap: WrapMode::None,
            spans: vec![TextSpan {
                content: TextContent::Lit(long.to_owned()),
                style: TextStyle {
                    font_px: 14.0,
                    ..Default::default()
                },
            }],
            ..Default::default()
        };
    }
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert_eq!(report.findings.len(), 1, "{}", report.to_lines());
    assert!(
        report.findings[0].measured.contains("past the screen"),
        "{}",
        report.to_lines()
    );
}

#[test]
fn text_pushed_off_the_screen_is_reported() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    fixed_box(
        &mut world,
        screen,
        Dimension::Points(790.0),
        Dimension::Points(40.0),
        false,
    );
    let long = "A long sentence that fits this wide box at its authored size but not at double";
    let mut text_query = world.query::<&mut Text>();
    for mut text in text_query.iter_mut(&mut world) {
        *text = Text {
            wrap: WrapMode::None,
            spans: vec![TextSpan {
                content: TextContent::Lit(long.to_owned()),
                style: TextStyle {
                    font_px: 14.0,
                    ..Default::default()
                },
            }],
            ..Default::default()
        };
    }
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.measured.contains("past the screen")),
        "{}",
        report.to_lines()
    );
}

#[test]
fn the_scratch_pass_leaves_the_live_ui_as_it_was() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    let (boxed, text) = fixed_box(
        &mut world,
        screen,
        Dimension::Points(150.0),
        Dimension::Points(40.0),
        true,
    );
    run_ui_layout(&mut world).expect("layout");
    let before = (
        *world.get::<ComputedNode>(boxed).expect("box"),
        *world.get::<ComputedNode>(text).expect("text"),
        *world.resource::<crate::layout::UiLayoutStats>(),
        world.resource::<Localization>().generation.0,
        world.resource::<LayoutDirty>().0,
        world.resource::<TextSystem>().text_scale,
        world.query::<Entity>().iter(&world).count(),
    );
    let report = audit_ui(&mut world, &AuditOptions::default());
    assert!(report.of_rule(AuditRule::TextClippedAtScale).count() > 0);
    let after = (
        *world.get::<ComputedNode>(boxed).expect("box"),
        *world.get::<ComputedNode>(text).expect("text"),
        *world.resource::<crate::layout::UiLayoutStats>(),
        world.resource::<Localization>().generation.0,
        world.resource::<LayoutDirty>().0,
        world.resource::<TextSystem>().text_scale,
        world.query::<Entity>().iter(&world).count(),
    );
    assert_eq!(before, after);
}

#[test]
fn with_no_text_system_the_rule_says_it_did_not_run() {
    let mut world = base_world();
    root(&mut world, WHITE_PANEL);
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0, AuditRule::TextClippedAtScale);
}

#[test]
fn a_caption_block_is_excepted_by_name() {
    let mut world = layout_world();
    let screen = laid_out_root(&mut world);
    let (boxed, _) = fixed_box(
        &mut world,
        screen,
        Dimension::Points(150.0),
        Dimension::Points(40.0),
        true,
    );
    world.entity_mut(boxed).insert(CaptionBlock);
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TextClippedAtScale);
    assert!(report.is_clean(), "{}", report.to_lines());
    assert_eq!(report.exemptions[0].exemption, "captions");
}

// ---------------------------------------------------------------------------------------------
// a11y_motion_ignores_setting
// ---------------------------------------------------------------------------------------------

/// A tween that says its plan already obeys reduced motion.
fn claiming(plan: TweenPlan) -> UiTween {
    let mut tween = UiTween::new(plan);
    tween.follows_reduced_motion = true;
    tween
}

fn slide() -> TweenPlan {
    TweenPlan::Tween(Tween::to(
        TweenProperty::TranslateX,
        TweenValue::Number(40.0),
        0.3,
    ))
}

#[test]
fn a_tween_that_claims_reduced_motion_but_still_slides_fails() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let panel = child(&mut world, screen, "panel", [0.0, 0.0, 100.0, 40.0]);
    world.entity_mut(panel).insert(claiming(slide()));
    let long_fade = child(&mut world, screen, "fade", [0.0, 60.0, 100.0, 40.0]);
    world
        .entity_mut(long_fade)
        .insert(claiming(TweenPlan::Tween(Tween::to(
            TweenProperty::Opacity,
            TweenValue::Number(0.0),
            2.0,
        ))));
    let report = run(&mut world, AuditRule::MotionIgnoresSetting);
    assert_eq!(report.findings.len(), 2, "{}", report.to_lines());
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.measured.starts_with("fade of 2.00"))
    );
    assert!(report.findings.iter().any(|f| f.measured.contains("0.30 s")));
    assert!(report.findings.iter().all(|f| f.severity == AuditSeverity::Error));
}

#[test]
fn a_plain_tween_and_a_short_capped_fade_pass() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    // A plain tween jumps to its end under reduced motion (`advance_tweens`).
    let plain = child(&mut world, screen, "plain", [0.0, 0.0, 100.0, 40.0]);
    world.entity_mut(plain).insert(UiTween::new(slide()));
    // What the motion presets plan: a fade at or under `motion.fast`, and a jump.
    let capped = child(&mut world, screen, "capped", [0.0, 60.0, 100.0, 40.0]);
    world.entity_mut(capped).insert(claiming(TweenPlan::Parallel(vec![
        TweenPlan::Tween(Tween::to(TweenProperty::Opacity, TweenValue::Number(1.0), 0.08)),
        TweenPlan::Tween(Tween::to(TweenProperty::TranslateX, TweenValue::Number(0.0), 0.0)),
    ])));
    let report = run(&mut world, AuditRule::MotionIgnoresSetting);
    assert!(report.is_clean(), "{}", report.to_lines());
}

/// A delay holds the start value (`Tween::sample_at`): nothing moves while it runs. Under
/// reduced motion a delayed jump is still a jump, and a capped fade after a delay is still a
/// capped fade. Counting `duration + delay` reported "moves for 0.30 s" and "fade of 0.38 s".
#[test]
fn a_delay_before_a_jump_or_a_capped_fade_is_not_motion() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let panel = child(&mut world, screen, "panel", [0.0, 0.0, 100.0, 40.0]);
    world.entity_mut(panel).insert(claiming(TweenPlan::Parallel(vec![
        TweenPlan::Tween(Tween::to(TweenProperty::Opacity, TweenValue::Number(1.0), 0.08).delay(0.3)),
        TweenPlan::Tween(Tween::to(TweenProperty::TranslateX, TweenValue::Number(0.0), 0.0).delay(0.3)),
    ])));
    let report = run(&mut world, AuditRule::MotionIgnoresSetting);
    assert!(report.is_clean(), "{}", report.to_lines());
}

// ---------------------------------------------------------------------------------------------
// The report as a whole
// ---------------------------------------------------------------------------------------------

/// A screen that breaks many rules at once.
fn broken_screen(world: &mut World) {
    let screen = laid_out_root(world);
    world.entity_mut(screen).insert(Background {
        kind: BackgroundKind::Color(hex("#f4f4f4")),
    });
    let (boxed, text) = fixed_box(
        world,
        screen,
        Dimension::Points(150.0),
        Dimension::Points(40.0),
        true,
    );
    world.entity_mut(text).insert(Text {
        wrap: WrapMode::None,
        ..label("Start the game", UiColor::WHITE, 18.0, false)
    });
    world
        .entity_mut(boxed)
        .insert((Button, Focusable::default(), Interactable::default()));
    let close = world
        .spawn((
            UiNode,
            NodeName("close".to_owned()),
            NodeStyle::new(Style {
                size: [Dimension::Points(12.0), Dimension::Points(12.0)],
                ..Default::default()
            }),
            Button,
            Focusable::default(),
            Interactable::default(),
            claiming(slide()),
            ChildOf(screen),
        ))
        .id();
    let slider = world
        .spawn((
            UiNode,
            NodeName("volume".to_owned()),
            NodeStyle::new(Style {
                size: [Dimension::Points(200.0), Dimension::Points(8.0)],
                ..Default::default()
            }),
            Slider::default(),
            Focusable::default(),
            Interactable::default(),
            ChildOf(screen),
        ))
        .id();
    assert_ne!(close, slider);
}

#[test]
fn findings_come_in_rule_order_and_two_runs_print_the_same_bytes() {
    let mut world = layout_world();
    broken_screen(&mut world);
    run_ui_layout(&mut world).expect("layout");
    let options = AuditOptions::default();
    let first = audit_ui(&mut world, &options);
    let second = audit_ui(&mut world, &options);
    assert_eq!(first.to_lines(), second.to_lines());
    assert!(!first.to_lines().is_empty());
    let ranks: Vec<AuditRule> = first.findings.iter().map(|f| f.rule).collect();
    let mut sorted = ranks.clone();
    sorted.sort();
    assert_eq!(ranks, sorted);
    for rule in [
        AuditRule::UnnamedInteractive,
        AuditRule::TextContrast,
        AuditRule::TargetSize,
        AuditRule::NoFocusIndicator,
        AuditRule::TextClippedAtScale,
        AuditRule::MotionIgnoresSetting,
    ] {
        assert!(
            first.of_rule(rule).count() > 0,
            "{} did not fire:\n{}",
            rule.code(),
            first.to_lines()
        );
    }
}

#[test]
fn a_second_world_built_the_same_way_prints_the_same_bytes() {
    let print = || {
        let mut world = layout_world();
        broken_screen(&mut world);
        run_ui_layout(&mut world).expect("layout");
        audit_ui(&mut world, &AuditOptions::default()).to_lines()
    };
    assert_eq!(print(), print());
}

#[test]
fn the_rule_filter_and_the_root_narrow_the_report() {
    let mut world = layout_world();
    broken_screen(&mut world);
    run_ui_layout(&mut world).expect("layout");
    let report = run(&mut world, AuditRule::TargetSize);
    assert!(report.findings.iter().all(|f| f.rule == AuditRule::TargetSize));
    let close = world
        .query::<(Entity, &NodeName)>()
        .iter(&world)
        .find(|(_, n)| n.0 == "volume")
        .map(|(e, _)| e)
        .expect("volume");
    let report = audit_ui(
        &mut world,
        &AuditOptions {
            root: Some(close),
            ..Default::default()
        },
    );
    assert!(!report.findings.is_empty());
    assert!(report.findings.iter().all(|f| f.path.ends_with("volume")));
}

#[test]
fn a_hidden_page_is_not_audited() {
    let mut world = base_world();
    let screen = root(&mut world, WHITE_PANEL);
    let page = child(&mut world, screen, "page", [0.0, 0.0, 100.0, 100.0]);
    world.entity_mut(page).insert(NodeStyle::new(Style {
        display: Display::None,
        ..Default::default()
    }));
    let button = target(&mut world, page, "buried", [0.0, 0.0, 5.0, 5.0]);
    assert!(world.get::<Button>(button).is_some());
    let report = audit_ui(&mut world, &AuditOptions::default());
    assert!(report.is_clean(), "{}", report.to_lines());
}

#[test]
fn an_empty_world_is_clean_and_runs_nothing_it_cannot() {
    let mut world = World::new();
    let report = audit_ui(&mut world, &AuditOptions::default());
    assert!(report.is_clean());
    assert_eq!(report.nodes, 0);
}
