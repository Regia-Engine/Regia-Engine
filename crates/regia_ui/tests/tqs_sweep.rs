// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep (lane sprint-test-quality-sweep, ADR-2077): the least-covered public
//! functions of `regia_ui` (by a count of test references per public fn) get deterministic tests
//! here: boundary values, NaN and infinite inputs, hostile text, round-trips. A test that failed on
//! the old code is the regression test of its fix (the fix names it in a comment). No goldens, no
//! GPU, no clocks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use regia_app::caption::{CaptionPiece, CaptionText};
use regia_app::prelude::*;
use regia_asset::dialogue::TimedCue;
use regia_ui::a11y::A11yRole;
use regia_ui::captions::{CaptionOptions, Rgb, cue_pieces, parse_text_color};
use regia_ui::color_picker::ColorPicker;
use regia_ui::loc_po::{PLACEHOLDER_MARKER, remove_placeholders};
use regia_ui::menu::{
    ContextMenu, MenuBar, MenuEntry, MenuItem, MenuSource, PopupMenu, entries_at, type_ahead,
};
use regia_ui::popup::popup_bounds;
use regia_ui::scroll::chain_link;
use regia_ui::text::{FontRun, font_runs, is_invisible_control, normalize_line_breaks};
use regia_ui::theme_file::{PropValue, Scheme, ThemeFile, TokenValue, TypeDef, parse_theme_ron};
use regia_ui::theme_lint::contrast_pairs;
use regia_ui::theme_migrate::migrate_v0_ron;
use regia_ui::theme_resolve::builtin_theme_set;
use regia_ui::theme_tokens::{Expr, ResolveError, TokenTable, Value};
use regia_ui::ui_sounds::{UiSoundCue, UiSoundOverride, UiSounds, resolve_sound};
use regia_ui::{
    BindTarget, CaretCluster, CaretLine, CaretMap, ComputedNode, SafeAreaPadding, ScrollView, Split,
    UiDevicePolicy, UiScales, resolve_window_size,
};

// ---------------------------------------------------------------------------------------------
// 1. captions: `parse_text_color`, `Rgb::from_hex`, `Rgb::luminance`, `CaptionOptions::text_rgb`
// ---------------------------------------------------------------------------------------------

#[test]
fn caption_text_color_rejects_sign_prefixed_hex() {
    // `u32::from_str_radix` takes a leading `+`, so `#+12345` used to read as 0x012345.
    for bad in [
        "#+12345", "#+fffff", "#-12345", "# 12345", "#12345 ", "#1234567", "#12345", "#ggg000",
    ] {
        assert_eq!(parse_text_color(bad), None, "{bad:?}");
        assert_eq!(Rgb::from_hex(bad), None, "{bad:?}");
    }
    let mut options = CaptionOptions::default();
    let patch = serde_json::json!({ "text_color": "#+12345" });
    assert!(options.apply_patch(patch.as_object().unwrap()).is_err());
}

#[test]
fn caption_text_color_presets_and_every_byte_round_trip() {
    assert_eq!(parse_text_color("white"), Some(Rgb::WHITE));
    assert_eq!(parse_text_color("yellow"), Some(Rgb::YELLOW));
    assert_eq!(parse_text_color("WHITE"), None, "presets are lowercase");
    assert_eq!(parse_text_color(""), None);
    for v in 0..=255u8 {
        for rgb in [Rgb(v, 0, 0), Rgb(0, v, 0), Rgb(0, 0, v), Rgb(v, v, v)] {
            assert_eq!(parse_text_color(&rgb.to_hex()), Some(rgb));
            assert_eq!(parse_text_color(&rgb.to_hex().to_uppercase()), Some(rgb));
        }
    }
}

#[test]
fn caption_luminance_is_monotonic_and_bounded() {
    assert_eq!(Rgb::BLACK.luminance(), 0.0);
    assert!((Rgb::WHITE.luminance() - 1.0).abs() < 1e-12);
    let mut last = -1.0;
    for v in 0..=255u8 {
        let l = Rgb(v, v, v).luminance();
        assert!(l > last, "gray {v} luminance {l} not above {last}");
        assert!((0.0..=1.0 + 1e-12).contains(&l));
        last = l;
    }
    // The channel weights sum to 1 and green dominates.
    assert!(Rgb(0, 255, 0).luminance() > Rgb(255, 0, 0).luminance());
    assert!(Rgb(255, 0, 0).luminance() > Rgb(0, 0, 255).luminance());
    // Contrast is symmetric and spans 1..=21.
    assert!((Rgb::WHITE.contrast(Rgb::BLACK) - 21.0).abs() < 1e-9);
    assert_eq!(Rgb::WHITE.contrast(Rgb::BLACK), Rgb::BLACK.contrast(Rgb::WHITE));
    assert_eq!(Rgb(12, 34, 56).contrast(Rgb(12, 34, 56)), 1.0);
}

#[test]
fn caption_text_rgb_falls_back_to_white_for_stored_garbage() {
    let mut options = CaptionOptions::default();
    for (stored, want) in [
        ("yellow", Rgb::YELLOW),
        ("#102030", Rgb(0x10, 0x20, 0x30)),
        ("#+12345", Rgb::WHITE),
        ("", Rgb::WHITE),
        ("#1020", Rgb::WHITE),
        ("rgb(1,2,3)", Rgb::WHITE),
    ] {
        options.text_color = stored.to_owned();
        assert_eq!(options.text_rgb(), want, "{stored:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// 2. `ColorPicker::set_hex` / `ColorPicker::hue_pos`
// ---------------------------------------------------------------------------------------------

#[test]
fn color_picker_set_hex_rejects_sign_prefixed_pairs() {
    // `u8::from_str_radix("+F", 16)` is `Ok(15)`: `#+F+F+F` used to set a dark gray.
    for bad in ["#+F+F+F", "#+F0000", "#FF00+F", "#FF0000+F", "#-10000", "#FF 000"] {
        let mut p = ColorPicker::from_srgb8(10, 20, 30, 255);
        let before = p;
        assert!(!p.set_hex(bad), "{bad:?} accepted");
        assert_eq!(p, before, "{bad:?} changed the picker");
    }
}

#[test]
fn color_picker_hue_pos_is_a_fraction_for_any_hue() {
    for hue in [
        0.0,
        59.5,
        359.999,
        360.0,
        720.0 + 90.0,
        -90.0,
        -0.0,
        -1e-6,
        1e30,
        -1e30,
        f32::MAX,
        f32::MIN,
    ] {
        let p = ColorPicker {
            hue,
            ..ColorPicker::default()
        };
        let f = p.hue_pos();
        assert!((0.0..=1.0).contains(&f), "hue {hue} -> {f}");
    }
    for hue in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let p = ColorPicker {
            hue,
            ..ColorPicker::default()
        };
        assert_eq!(p.hue_pos(), 0.0, "hue {hue}");
    }
    let wrapped = ColorPicker {
        hue: -90.0,
        ..ColorPicker::default()
    };
    assert!((wrapped.hue_pos() - 0.75).abs() < 1e-6);
    // pick_hue and hue_pos agree across the bar.
    for k in 0..=100 {
        let t = k as f32 / 100.0;
        let mut p = ColorPicker::default();
        p.pick_hue(t);
        assert!((p.hue_pos() - t).abs() < 1e-4, "t {t} -> {}", p.hue_pos());
    }
}

// ---------------------------------------------------------------------------------------------
// 3. `loc_po::remove_placeholders`
// ---------------------------------------------------------------------------------------------

fn keys(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn remove_placeholders_is_a_no_op_when_nothing_is_removed() {
    // The importer compares the result with the input to decide whether to rewrite gathered.ftl:
    // a no-op that rewrote CR LF or added a final newline made a spurious file change.
    for text in [
        "menu-quit = Quit\r\nmenu-play = Play\r\n",
        "menu-quit = Quit\nmenu-play = Play",
        "",
        "\n\n",
        "# comment only",
    ] {
        assert_eq!(remove_placeholders(text, &keys(&["missing"])), text, "{text:?}");
        assert_eq!(remove_placeholders(text, &BTreeSet::new()), text, "{text:?}");
    }
}

#[test]
fn remove_placeholders_drops_only_the_named_blocks_and_keeps_line_endings() {
    let text = format!(
        "keep = Kept\r\n\r\n# source: menu.ron:3\r\n{PLACEHOLDER_MARKER}\r\ngone = Gone\r\n\r\n{PLACEHOLDER_MARKER}\r\nstays = Not accepted\r\n"
    );
    let out = remove_placeholders(&text, &keys(&["gone"]));
    assert_eq!(
        out,
        format!("keep = Kept\r\n\r\n{PLACEHOLDER_MARKER}\r\nstays = Not accepted\r\n")
    );
    // A non-placeholder message of a named key stays.
    assert_eq!(
        remove_placeholders("gone = Real\n", &keys(&["gone"])),
        "gone = Real\n"
    );
    // LF text with a multi-line placeholder.
    let lf = format!("{PLACEHOLDER_MARKER}\nammo = {{ $n ->\n   *[other] {{ $n }}\n    }}\nnext = Next\n");
    assert_eq!(remove_placeholders(&lf, &keys(&["ammo"])), "next = Next\n");
    // Idempotent.
    assert_eq!(remove_placeholders(&out, &keys(&["gone"])), out);
}

// ---------------------------------------------------------------------------------------------
// 4. `Split::clamp_ratio` / `Split::ratio_after_drag`
// ---------------------------------------------------------------------------------------------

#[test]
fn split_clamp_ratio_stays_in_range_for_non_finite_bounds() {
    // Bounds come from authored files: NaN bounds used to let any ratio through (5.0 stayed 5.0).
    for (min_ratio, max_ratio) in [
        (f32::NAN, f32::NAN),
        (f32::NAN, 0.9),
        (0.1, f32::NAN),
        (f32::INFINITY, f32::NEG_INFINITY),
        (f32::NEG_INFINITY, f32::INFINITY),
    ] {
        let split = Split {
            min_ratio,
            max_ratio,
            ..Split::default()
        };
        for ratio in [-5.0, 0.0, 0.005, 0.5, 0.995, 1.0, 5.0] {
            let r = split.clamp_ratio(ratio);
            assert!(
                (0.01..=0.99).contains(&r),
                "bounds ({min_ratio}, {max_ratio}) ratio {ratio} -> {r}"
            );
        }
    }
    // A NaN bound does not collapse the range to the other bound.
    let split = Split {
        min_ratio: f32::NAN,
        ..Split::default()
    };
    assert_eq!(split.clamp_ratio(0.5), 0.5);
}

#[test]
fn split_clamp_ratio_orders_swapped_bounds_and_keeps_ratio_for_non_finite_input() {
    let swapped = Split {
        min_ratio: 0.8,
        max_ratio: 0.2,
        ..Split::default()
    };
    assert_eq!(swapped.clamp_ratio(0.0), 0.2);
    assert_eq!(swapped.clamp_ratio(1.0), 0.8);
    assert_eq!(swapped.clamp_ratio(0.5), 0.5);
    let split = Split::default();
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(split.clamp_ratio(bad), split.ratio);
        assert_eq!(split.ratio_after_drag(bad, 100.0), split.ratio);
    }
    assert_eq!(split.ratio_after_drag(10.0, 0.5), split.ratio, "no shared space");
    assert_eq!(split.ratio_after_drag(10.0, f32::NAN), split.ratio, "NaN space");
    assert_eq!(split.ratio_after_drag(1e9, 100.0), 0.9);
    assert_eq!(split.ratio_after_drag(-1e9, 100.0), 0.1);
}

// ---------------------------------------------------------------------------------------------
// 5. menus: `entries_at`, `MenuSource::kind_name`, `type_ahead`
// ---------------------------------------------------------------------------------------------

fn menu_tree() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Item(MenuItem::new("file", "File").with_submenu(vec![
            MenuEntry::Item(MenuItem::new("open", "Open")),
            MenuEntry::Separator,
            MenuEntry::Item(MenuItem::new("recent", "Recent").with_submenu(vec![MenuEntry::Item(
                MenuItem::new("a", "a.txt"),
            )])),
        ])),
        MenuEntry::Separator,
        MenuEntry::Item(MenuItem::new("quit", "Quit")),
    ]
}

#[test]
fn entries_at_walks_paths_and_refuses_separators_and_bad_indices() {
    let mut world = World::new();
    let host = world
        .spawn((
            PopupMenu {
                entries: menu_tree(),
                ..Default::default()
            },
            MenuBar {
                menus: vec![
                    MenuItem::new("edit", "Edit")
                        .with_submenu(vec![MenuEntry::Item(MenuItem::new("undo", "Undo"))]),
                ],
            },
        ))
        .id();
    let ids = |list: &[MenuEntry]| -> Vec<String> {
        list.iter()
            .map(|e| e.item().map_or("-".to_owned(), |i| i.id.clone()))
            .collect()
    };
    let root = entries_at(&world, host, MenuSource::Popup, &[]).unwrap();
    assert_eq!(ids(root), ["file", "-", "quit"]);
    assert_eq!(
        ids(entries_at(&world, host, MenuSource::Popup, &[0]).unwrap()),
        ["open", "-", "recent"]
    );
    assert_eq!(
        ids(entries_at(&world, host, MenuSource::Popup, &[0, 2]).unwrap()),
        ["a"]
    );
    assert_eq!(
        entries_at(&world, host, MenuSource::Popup, &[2]).unwrap().len(),
        0,
        "leaf"
    );
    assert!(
        entries_at(&world, host, MenuSource::Popup, &[1]).is_none(),
        "separator"
    );
    assert!(
        entries_at(&world, host, MenuSource::Popup, &[0, 1]).is_none(),
        "nested separator"
    );
    assert!(
        entries_at(&world, host, MenuSource::Popup, &[3]).is_none(),
        "past the end"
    );
    assert!(entries_at(&world, host, MenuSource::Popup, &[usize::MAX]).is_none());
    assert!(
        entries_at(&world, host, MenuSource::Context, &[]).is_none(),
        "no context menu"
    );
    assert_eq!(
        ids(entries_at(&world, host, MenuSource::Bar(0), &[]).unwrap()),
        ["undo"]
    );
    assert!(entries_at(&world, host, MenuSource::Bar(1), &[]).is_none());
    assert!(entries_at(&world, host, MenuSource::Bar(usize::MAX), &[]).is_none());
    let empty = world.spawn(ContextMenu::default()).id();
    assert_eq!(
        entries_at(&world, empty, MenuSource::Context, &[]).unwrap().len(),
        0
    );
    assert_eq!(MenuSource::Popup.kind_name(), "popup_menu");
    assert_eq!(MenuSource::Context.kind_name(), "context_menu");
    assert_eq!(
        MenuSource::Bar(0).kind_name(),
        MenuSource::Bar(usize::MAX).kind_name()
    );
}

#[test]
fn type_ahead_survives_a_current_index_at_usize_max() {
    // A dropdown's `selected` is a public field: `Some(usize::MAX)` overflowed `current + 1`.
    let options: Vec<String> = ["apple", "banana", "avocado"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let got = type_ahead(&options, Some(usize::MAX), "a");
    assert!(matches!(got, Some(0 | 2)), "{got:?}");
    assert_eq!(type_ahead(&options, Some(usize::MAX), "ban"), Some(1));
    // Past-the-end current values behave like their remainder.
    for current in [3usize, 4, 5, 1000] {
        assert_eq!(
            type_ahead(&options, Some(current), "a"),
            type_ahead(&options, Some(current % options.len()), "a"),
            "current {current}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 6. `ThemeFile::check_syntax` (and `parse_theme_ron`, which runs it)
// ---------------------------------------------------------------------------------------------

fn minimal_theme() -> ThemeFile {
    ThemeFile {
        name: "t".to_owned(),
        tokens: BTreeMap::from([("radius.md".to_owned(), TokenValue::Number(4.0))]),
        ..ThemeFile::default()
    }
}

#[test]
fn check_syntax_rejects_non_finite_property_numbers() {
    // Tokens already refused non-finite numbers; a widget property number did not.
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for value in [
            PropValue::Number(bad),
            PropValue::List(vec![PropValue::Number(1.0), PropValue::Number(bad)]),
        ] {
            let mut file = minimal_theme();
            let mut def = TypeDef::default();
            def.hover.insert("radius".to_owned(), value.clone());
            file.types.insert("Button".to_owned(), def);
            assert!(file.check_syntax().is_err(), "{value:?} passed");
        }
    }
    let mut file = minimal_theme();
    let mut def = TypeDef::default();
    def.props.insert("radius".to_owned(), PropValue::Number(6.0));
    file.variations.insert("Button.primary".to_owned(), def);
    assert_eq!(file.check_syntax(), Ok(()));
}

#[test]
fn check_syntax_names_the_bad_key_and_refuses_empty_names() {
    let mut file = minimal_theme();
    file.name = "   ".to_owned();
    assert!(file.check_syntax().is_err());
    let mut file = minimal_theme();
    file.tokens
        .insert("color.x".to_owned(), TokenValue::Text("#12".to_owned()));
    let err = file.check_syntax().unwrap_err().to_string();
    assert!(err.contains("tokens.color.x"), "{err}");
    let mut file = minimal_theme();
    file.densities.insert("roomy".to_owned(), BTreeMap::new());
    let err = file.check_syntax().unwrap_err().to_string();
    assert!(err.contains("densities.roomy"), "{err}");
    let mut file = minimal_theme();
    let mut def = TypeDef::default();
    def.pressed
        .insert("background".to_owned(), PropValue::Text("lighten(@a)".to_owned()));
    file.types.insert("Button".to_owned(), def);
    let err = file.check_syntax().unwrap_err().to_string();
    assert!(err.contains("types.Button"), "{err}");
    // Keywords that are not expressions pass the syntax check.
    let mut file = minimal_theme();
    let mut def = TypeDef::default();
    def.props
        .insert("align".to_owned(), PropValue::Text("center".to_owned()));
    file.types.insert("Label".to_owned(), def);
    assert_eq!(file.check_syntax(), Ok(()));
}

#[test]
fn parse_theme_ron_refuses_a_nan_property() {
    let source = r#"(
        format_version: 1,
        name: "hostile",
        extends: Some("regia-dark"),
        types: { "Button": (props: { "radius": NaN }) },
    )"#;
    assert!(parse_theme_ron(source, "hostile.theme.ron").is_err());
    let fine = source.replace("NaN", "3.0");
    assert!(parse_theme_ron(&fine, "fine.theme.ron").is_ok());
}

// ---------------------------------------------------------------------------------------------
// 7. `theme_migrate::migrate_v0_ron`
// ---------------------------------------------------------------------------------------------

#[test]
fn migrate_v0_ron_maps_slots_and_picks_the_parent_by_lightness() {
    let dark = regia_ui::UiTheme::dark();
    let light = regia_ui::UiTheme::light();
    for (theme, parent) in [(&dark, "regia-dark"), (&light, "regia-light")] {
        let ron = ron::to_string(theme).unwrap();
        let file = migrate_v0_ron(&ron, "v0.theme.ron").unwrap();
        assert_eq!(file.name, theme.name);
        assert_eq!(file.extends.as_deref(), Some(parent));
        assert_eq!(file.tokens["radius.md"], TokenValue::Number(theme.radius));
        assert_eq!(file.tokens["type.body.size"], TokenValue::Number(theme.font_px));
        for key in [
            "color.bg",
            "color.accent",
            "color.text",
            "color.scrim",
            "color.danger",
        ] {
            assert!(
                matches!(file.tokens.get(key), Some(TokenValue::Text(t)) if t.starts_with('#')),
                "{key}"
            );
        }
        assert!(
            file.types.is_empty() && file.variations.is_empty(),
            "v0 files carry no classes"
        );
        // Migrating twice gives the same file.
        assert_eq!(migrate_v0_ron(&ron, "v0.theme.ron").unwrap(), file);
    }
}

#[test]
fn migrate_v0_ron_refuses_garbage_and_non_finite_numbers() {
    assert!(migrate_v0_ron("", "x").is_err());
    assert!(migrate_v0_ron("(", "x").is_err());
    assert!(migrate_v0_ron("[1, 2]", "x").is_err());
    let mut theme = regia_ui::UiTheme::dark();
    theme.radius = f32::NAN;
    let ron = ron::to_string(&theme).unwrap();
    assert!(migrate_v0_ron(&ron, "x").is_err(), "NaN radius migrated");
    let mut theme = regia_ui::UiTheme::dark();
    theme.name = String::new();
    let ron = ron::to_string(&theme).unwrap();
    assert!(migrate_v0_ron(&ron, "x").is_err(), "empty name migrated");
}

// ---------------------------------------------------------------------------------------------
// 8-10. text: `normalize_line_breaks`, `is_invisible_control`, `font_runs`
// ---------------------------------------------------------------------------------------------

#[test]
fn normalize_line_breaks_handles_every_break_form() {
    let cases = [
        ("a\r\nb", false, "a\nb"),
        ("a\rb", false, "a\nb"),
        ("a\r\r\nb", false, "a\n\nb"),
        ("a\n\rb", false, "a\n\nb"),
        ("a\u{85}b\u{2028}c\u{2029}d", false, "a\nb\nc\nd"),
        ("\r", false, "\n"),
        ("\r\n", false, "\n"),
        ("\nx", true, "x"),
        ("\n\nx", true, "\nx"),
        ("\r\nx", true, "\nx"),
        ("", true, ""),
        ("plain", true, "plain"),
        ("é\r\nü", false, "é\nü"),
    ];
    for (input, after_cr, want) in cases {
        assert_eq!(
            normalize_line_breaks(input, after_cr),
            want,
            "{input:?} after_cr {after_cr}"
        );
    }
    // Untouched text is borrowed, and the output never holds a break other than LF.
    assert!(matches!(
        normalize_line_breaks("no breaks\n", false),
        std::borrow::Cow::Borrowed(_)
    ));
    let out = normalize_line_breaks("\r\r\n\u{2028}\n\u{85}", false);
    assert!(
        !out.chars()
            .any(|c| matches!(c, '\r' | '\u{85}' | '\u{2028}' | '\u{2029}'))
    );
    // Splitting a CR LF across two spans gives one break.
    let joined = format!(
        "{}{}",
        normalize_line_breaks("a\r", false),
        normalize_line_breaks("\nb", true)
    );
    assert_eq!(joined, "a\nb");
}

#[test]
fn invisible_controls_cover_the_listed_ranges_and_spare_visible_text() {
    for c in [
        '\u{0}',
        '\t',
        '\u{7f}',
        '\u{85}',
        '\u{AD}',
        '\u{34F}',
        '\u{61C}',
        '\u{200B}',
        '\u{200D}',
        '\u{200F}',
        '\u{202A}',
        '\u{202E}',
        '\u{2060}',
        '\u{206F}',
        '\u{FE00}',
        '\u{FE0F}',
        '\u{FEFF}',
        '\u{E0001}',
        '\u{E007F}',
        '\u{E0FFF}',
        '\u{1D173}',
        '\u{1BCA0}',
    ] {
        assert!(is_invisible_control(c), "U+{:04X}", c as u32);
    }
    for c in ['a', ' ', '0', 'é', 'ب', '中', '\u{2764}', '\u{20E3}', '\u{1F600}'] {
        assert!(!is_invisible_control(c), "U+{:04X}", c as u32);
    }
    // The just-outside neighbours of each range stay visible.
    for c in [
        '\u{200A}',
        '\u{2010}',
        '\u{2029}',
        '\u{2070}',
        '\u{FDFF}',
        '\u{FE10}',
        '\u{E1000}',
    ] {
        assert!(!is_invisible_control(c), "U+{:04X}", c as u32);
    }
}

fn kinds(s: &str) -> Vec<(&str, FontRun)> {
    font_runs(s).into_iter().map(|(r, k)| (&s[r], k)).collect()
}

#[test]
fn font_runs_cover_the_text_exactly_and_merge_neighbours() {
    for s in [
        "",
        " ",
        "Score مرحبا 12",
        "❤️ love",
        "a\u{200D}",
        "\u{FE0F}",
        "x\u{FE0F}\u{200D}",
        "👍🏽 ok",
        "1\u{FE0F}\u{20E3}",
        "\u{200D}\u{FE0F}",
    ] {
        let runs = font_runs(s);
        let mut at = 0;
        for (i, (r, k)) in runs.iter().enumerate() {
            assert_eq!(r.start, at, "{s:?} gap");
            assert!(r.end > r.start, "{s:?} empty run");
            assert!(s.is_char_boundary(r.start) && s.is_char_boundary(r.end));
            if i > 0 {
                assert_ne!(runs[i - 1].1, *k, "{s:?}: adjacent runs of one kind");
            }
            at = r.end;
        }
        assert_eq!(at, s.len(), "{s:?} not covered");
    }
    assert_eq!(
        kinds("Score مرحبا"),
        [("Score ", FontRun::Default), ("مرحبا", FontRun::Arabic)]
    );
    assert_eq!(
        kinds("مرحبا بك"),
        [("مرحبا بك", FontRun::Arabic)],
        "space stays inside"
    );
    assert_eq!(
        kinds("  12 مرحبا"),
        [("  12 مرحبا", FontRun::Arabic)],
        "leading neutrals join"
    );
    assert_eq!(
        kinds("❤️ hi"),
        [("❤️", FontRun::Emoji), (" hi", FontRun::Default)]
    );
    assert_eq!(kinds("12"), [("12", FontRun::Default)]);
    // A trailing ZWJ with nothing after it does not extend the emoji past the text.
    assert_eq!(
        kinds("x\u{FE0F}\u{200D}"),
        [("x\u{FE0F}", FontRun::Emoji), ("\u{200D}", FontRun::Default)]
    );
}

// ---------------------------------------------------------------------------------------------
// 11. `CaretMap::hit_line`
// ---------------------------------------------------------------------------------------------

fn caret_map() -> CaretMap {
    // Line 0: "ab" as two clusters, then an "ffi" ligature cluster; line 1: an RTL cluster "שלום".
    let text = "abffi\nשלום".to_owned();
    let rtl_start = text.find('ש').unwrap();
    CaretMap {
        lines: vec![
            CaretLine {
                top: 0.0,
                height: 10.0,
                start: 0,
                end: 5,
                origin_x: 0.0,
                rtl: false,
                clusters: vec![
                    CaretCluster {
                        start: 0,
                        end: 1,
                        x0: 0.0,
                        x1: 10.0,
                        rtl: false,
                    },
                    CaretCluster {
                        start: 1,
                        end: 2,
                        x0: 10.0,
                        x1: 20.0,
                        rtl: false,
                    },
                    CaretCluster {
                        start: 2,
                        end: 5,
                        x0: 20.0,
                        x1: 50.0,
                        rtl: false,
                    },
                ],
            },
            CaretLine {
                top: 10.0,
                height: 10.0,
                start: rtl_start,
                end: text.len(),
                origin_x: 0.0,
                rtl: true,
                clusters: vec![CaretCluster {
                    start: rtl_start,
                    end: text.len(),
                    x0: 0.0,
                    x1: 40.0,
                    rtl: true,
                }],
            },
            CaretLine {
                top: 20.0,
                height: 10.0,
                start: text.len(),
                end: text.len(),
                origin_x: 3.0,
                rtl: false,
                clusters: Vec::new(),
            },
        ],
        text,
    }
}

#[test]
fn hit_line_maps_x_to_grapheme_boundaries() {
    let map = caret_map();
    assert_eq!(map.hit_line(0, -100.0), 0, "far left");
    assert_eq!(map.hit_line(0, 4.0), 0);
    assert_eq!(map.hit_line(0, 6.0), 1);
    assert_eq!(map.hit_line(0, 1000.0), 5, "far right");
    // Inside the ligature: each grapheme gets a third.
    assert_eq!(map.hit_line(0, 21.0), 2);
    assert_eq!(map.hit_line(0, 30.0), 3);
    assert_eq!(map.hit_line(0, 40.0), 4);
    assert_eq!(map.hit_line(0, 49.0), 5);
    // RTL: the right edge is the start.
    let rtl_start = map.lines[1].start;
    assert_eq!(map.hit_line(1, 40.0), rtl_start);
    assert_eq!(map.hit_line(1, 0.0), map.text.len());
    // An empty line and an out-of-range line.
    assert_eq!(map.hit_line(2, 50.0), map.text.len());
    assert_eq!(map.hit_line(3, 0.0), map.text.len());
    assert_eq!(map.hit_line(usize::MAX, 0.0), map.text.len());
}

#[test]
fn hit_line_returns_a_char_boundary_for_hostile_x() {
    let map = caret_map();
    for line in 0..3 {
        for x in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX, -0.0, 1e-30] {
            let b = map.hit_line(line, x);
            assert!(
                b <= map.text.len() && map.text.is_char_boundary(b),
                "line {line} x {x} -> {b}"
            );
        }
    }
    // hit() agrees with hit_line on the line it picks.
    assert_eq!(map.hit([30.0, 5.0]), map.hit_line(0, 30.0));
    assert_eq!(map.hit([30.0, 1e9]), map.hit_line(2, 30.0));
    assert_eq!(CaretMap::default().hit([1.0, 1.0]), 0);
}

// ---------------------------------------------------------------------------------------------
// 12. `captions::cue_pieces`
// ---------------------------------------------------------------------------------------------

fn cue(start_us: u64, end_us: u64, text: &str) -> TimedCue {
    TimedCue {
        start_us,
        end_us,
        text_key: None,
        text: Some(text.to_owned()),
    }
}

fn literal(c: &TimedCue) -> CaptionText {
    CaptionText::Literal(c.text.clone().unwrap_or_default())
}

#[test]
fn cue_pieces_insert_blank_gaps_and_keep_order() {
    assert!(cue_pieces(&[], literal).is_empty());
    let pieces = cue_pieces(
        &[cue(0, 100, "a"), cue(100, 200, "b"), cue(300, 300, "c")],
        literal,
    );
    assert_eq!(
        pieces,
        [
            CaptionPiece::literal(0, "a"),
            CaptionPiece::literal(100, "b"),
            CaptionPiece::literal(200, ""),
            CaptionPiece::literal(300, "c"),
        ]
    );
    // A last cue with an end gets a closing blank; overlapping cues get none.
    let pieces = cue_pieces(&[cue(0, 150, "a"), cue(100, 200, "b")], literal);
    assert_eq!(
        pieces,
        [
            CaptionPiece::literal(0, "a"),
            CaptionPiece::literal(100, "b"),
            CaptionPiece::literal(200, ""),
        ]
    );
    // The text callback runs once per cue, in order.
    let mut seen = Vec::new();
    let _ = cue_pieces(&[cue(0, 1, "x"), cue(5, 6, "y")], |c| {
        seen.push(c.text.clone().unwrap());
        literal(c)
    });
    assert_eq!(seen, ["x", "y"]);
    // Offsets never decrease for well-formed cues, including u64::MAX ends.
    let pieces = cue_pieces(&[cue(0, 10, "a"), cue(20, u64::MAX, "b")], literal);
    assert!(pieces.windows(2).all(|w| w[0].offset_us <= w[1].offset_us));
}

// ---------------------------------------------------------------------------------------------
// 13. `ui_sounds::resolve_sound`
// ---------------------------------------------------------------------------------------------

#[test]
fn resolve_sound_follows_switch_mute_override_and_table() {
    let mut table = UiSounds::default();
    table.sounds.insert(UiSoundCue::Press, "click".to_owned());
    table.sounds.insert(UiSoundCue::Hover, String::new());
    assert_eq!(resolve_sound(&table, None, UiSoundCue::Press), Some("click"));
    assert_eq!(
        resolve_sound(&table, None, UiSoundCue::Hover),
        None,
        "empty table entry"
    );
    assert_eq!(
        resolve_sound(&table, None, UiSoundCue::Focus),
        None,
        "missing entry"
    );
    let mut node = UiSoundOverride::default();
    assert_eq!(
        resolve_sound(&table, Some(&node), UiSoundCue::Press),
        Some("click")
    );
    node.sounds.insert(UiSoundCue::Press, "pop".to_owned());
    node.sounds.insert(UiSoundCue::Hover, "whoosh".to_owned());
    node.sounds.insert(UiSoundCue::Confirm, String::new());
    table.sounds.insert(UiSoundCue::Confirm, "ding".to_owned());
    assert_eq!(resolve_sound(&table, Some(&node), UiSoundCue::Press), Some("pop"));
    assert_eq!(
        resolve_sound(&table, Some(&node), UiSoundCue::Hover),
        Some("whoosh")
    );
    assert_eq!(
        resolve_sound(&table, Some(&node), UiSoundCue::Confirm),
        None,
        "override silences"
    );
    node.muted = true;
    for cue in UiSoundCue::ALL {
        assert_eq!(resolve_sound(&table, Some(&node), cue), None);
    }
    node.muted = false;
    table.enabled = false;
    for cue in UiSoundCue::ALL {
        assert_eq!(resolve_sound(&table, Some(&node), cue), None);
        assert_eq!(resolve_sound(&table, None, cue), None);
    }
}

// ---------------------------------------------------------------------------------------------
// 14. `popup::popup_bounds`
// ---------------------------------------------------------------------------------------------

#[test]
fn popup_bounds_match_the_safe_area_padding_of_a_root() {
    let mut world = World::new();
    assert_eq!(popup_bounds(&world), [0.0, 0.0, 0.0, 0.0], "no window");
    let scales = UiScales {
        window_size: [1280.0, 800.0],
        scale_factor: 1.0,
    };
    world.insert_resource(scales);
    assert_eq!(popup_bounds(&world), [0.0, 0.0, 1280.0, 800.0], "no safe area");
    world.insert_resource(regia_platform::SafeArea {
        left: 10,
        top: 20,
        right: 30,
        bottom: 40,
    });
    assert_eq!(popup_bounds(&world), [10.0, 20.0, 1240.0, 740.0]);
    world.insert_resource(UiDevicePolicy {
        title_safe: 0.05,
        ..UiDevicePolicy::default()
    });
    let root = world.spawn(SafeAreaPadding).id();
    let (size, pad) = resolve_window_size(&world, root, &scales);
    let b = popup_bounds(&world);
    assert_eq!(
        b,
        [
            pad[0],
            pad[1],
            size[0] - pad[0] - pad[2],
            size[1] - pad[1] - pad[3]
        ]
    );
    assert_eq!(b, [64.0, 40.0, 1280.0 - 128.0, 800.0 - 80.0]);
    // A safe area larger than the window leaves an empty area, never a negative one.
    world.insert_resource(regia_platform::SafeArea {
        left: 5000,
        top: 5000,
        right: 5000,
        bottom: 5000,
    });
    let b = popup_bounds(&world);
    assert_eq!([b[2], b[3]], [0.0, 0.0]);
    // A NaN window size gives an empty area too.
    world.insert_resource(UiScales {
        window_size: [f32::NAN, f32::NAN],
        scale_factor: 1.0,
    });
    let b = popup_bounds(&world);
    assert!(b.iter().all(|v| !v.is_nan()), "{b:?}");
}

// ---------------------------------------------------------------------------------------------
// 15. `theme_lint::contrast_pairs`
// ---------------------------------------------------------------------------------------------

#[test]
fn contrast_pairs_name_real_tokens_that_every_builtin_theme_meets() {
    let set = builtin_theme_set();
    for name in [
        "regia-dark",
        "regia-light",
        "regia-high-contrast",
        "regia-light-high-contrast",
    ] {
        let theme = set.resolve(name, None).unwrap();
        let pairs = contrast_pairs(theme.scheme);
        let mut seen = BTreeSet::new();
        for p in pairs {
            assert!(
                seen.insert((p.foreground, p.background)),
                "{name}: duplicate pair {p:?}"
            );
            assert!(p.required == 3.0 || p.required == 4.5, "{p:?}");
            let fg = theme
                .color(p.foreground)
                .unwrap_or_else(|| panic!("{name}: {}", p.foreground));
            let bg = theme
                .color(p.background)
                .unwrap_or_else(|| panic!("{name}: {}", p.background));
            let ratio = regia_ui::theme_color_ops::contrast_ratio(fg, bg);
            assert!(
                ratio >= p.required,
                "{name}: {} on {} is {ratio:.2}",
                p.foreground,
                p.background
            );
        }
    }
    // The two schemes differ only in the muted-text surface.
    let (d, l) = (contrast_pairs(Scheme::Dark), contrast_pairs(Scheme::Light));
    let differ: Vec<usize> = (0..d.len()).filter(|&i| d[i] != l[i]).collect();
    assert_eq!(differ.len(), 1);
    assert_eq!(d[differ[0]].background, "color.surface-overlay");
    assert_eq!(l[differ[0]].background, "color.surface-sunken");
}

// ---------------------------------------------------------------------------------------------
// 16. tokens: `Value::as_number` / `as_text` / `as_duration`, `TokenTable::resolve_expr`
// ---------------------------------------------------------------------------------------------

#[test]
fn value_accessors_answer_only_for_their_own_kind() {
    let values = [
        Value::Number(2.0),
        Value::Duration(0.08),
        Value::Text("Inter".to_owned()),
        Value::Text("linear".to_owned()),
        Value::Color(regia_ui::UiColor::WHITE),
    ];
    for v in &values {
        assert_eq!(v.as_number().is_some(), matches!(v, Value::Number(_)), "{v:?}");
        assert_eq!(
            v.as_duration().is_some(),
            matches!(v, Value::Duration(_)),
            "{v:?}"
        );
        assert_eq!(v.as_text().is_some(), matches!(v, Value::Text(_)), "{v:?}");
        assert_eq!(v.as_color().is_some(), matches!(v, Value::Color(_)), "{v:?}");
    }
    assert_eq!(values[0].as_number(), Some(2.0));
    assert_eq!(values[1].as_duration(), Some(0.08));
    assert_eq!(values[2].as_text(), Some("Inter"));
    assert!(
        values[3].as_easing().is_some(),
        "`linear` text reads as an easing"
    );
    assert!(values[2].as_easing().is_none());
}

#[test]
fn resolve_expr_names_the_owner_of_a_missing_reference() {
    // `ResolveError::Unknown::key` is documented as the token holding the reference; for an
    // expression outside the table it used to repeat the missing key instead of the owner.
    let mut table = TokenTable::default();
    table.insert("color.a", Expr::parse("#102030").unwrap());
    table.insert("color.b", Expr::parse("@color.missing").unwrap());
    let owner = "types.Button.props.background";
    for text in [
        "@color.missing",
        "lighten(@color.missing, 0.1)",
        "mix(@color.a, @color.missing, 0.5)",
    ] {
        let err = table
            .resolve_expr(owner, &Expr::parse(text).unwrap())
            .unwrap_err();
        assert_eq!(
            err,
            ResolveError::Unknown {
                key: owner.to_owned(),
                reference: "color.missing".to_owned()
            },
            "{text}"
        );
    }
    // Through a table token the token itself is the owner (unchanged behaviour).
    let err = table
        .resolve_expr(owner, &Expr::parse("@color.b").unwrap())
        .unwrap_err();
    assert_eq!(
        err,
        ResolveError::Unknown {
            key: "color.b".to_owned(),
            reference: "color.missing".to_owned()
        }
    );
}

#[test]
fn resolve_expr_reports_cycles_types_and_resolves_ops() {
    let mut table = TokenTable::default();
    table.insert("a", Expr::parse("@b").unwrap());
    table.insert("b", Expr::parse("@a").unwrap());
    table.insert("n", Expr::parse("0.5").unwrap());
    table.insert("c", Expr::parse("#000000").unwrap());
    let err = table
        .resolve_expr("owner", &Expr::parse("@a").unwrap())
        .unwrap_err();
    assert_eq!(
        err,
        ResolveError::Cycle {
            chain: vec!["a".to_owned(), "b".to_owned(), "a".to_owned()]
        }
    );
    let err = table
        .resolve_expr("owner", &Expr::parse("lighten(@n, 0.1)").unwrap())
        .unwrap_err();
    assert!(
        matches!(err, ResolveError::Type { ref key, expected: "color", .. } if key == "owner"),
        "{err:?}"
    );
    let v = table
        .resolve_expr("owner", &Expr::parse("mix(@c, #ffffff, 1)").unwrap())
        .unwrap();
    assert_eq!(
        v.as_color().map(regia_ui::theme_color_ops::to_hex),
        Some("#ffffff".to_owned())
    );
    // A literal resolves to itself without touching the table.
    let lit = Expr::parse("80ms").unwrap();
    assert_eq!(
        TokenTable::default().resolve_expr("x", &lit),
        Ok(Value::Duration(0.08))
    );
}

// ---------------------------------------------------------------------------------------------
// 17. `BindTarget::is_bool` / `is_editable`, `A11yRole::spoken`, `scroll::chain_link`
// ---------------------------------------------------------------------------------------------

#[test]
fn bind_target_kinds_match_their_documented_sets() {
    let all = [
        BindTarget::Text,
        BindTarget::Progress,
        BindTarget::Opacity,
        BindTarget::Checked,
        BindTarget::Value,
        BindTarget::Input,
        BindTarget::Selected,
        BindTarget::Visible,
        BindTarget::Enabled,
    ];
    let names: BTreeSet<&str> = all.iter().map(|t| t.name()).collect();
    assert_eq!(names.len(), all.len(), "names are distinct");
    let bools: Vec<&str> = all.iter().filter(|t| t.is_bool()).map(|t| t.name()).collect();
    assert_eq!(bools, ["checked", "visible", "enabled"]);
    let editable: Vec<&str> = all.iter().filter(|t| t.is_editable()).map(|t| t.name()).collect();
    assert_eq!(editable, ["checked", "value", "input", "selected"]);
}

#[test]
fn every_a11y_role_has_distinct_spoken_text() {
    let mut seen = BTreeSet::new();
    for role in A11yRole::ALL {
        let spoken = role.spoken();
        assert!(!spoken.is_empty() && spoken.is_ascii(), "{role:?}");
        assert_eq!(spoken, spoken.to_lowercase(), "{role:?}");
        assert!(
            !spoken.contains('_'),
            "{role:?} spoken text reads like a code name"
        );
        assert!(seen.insert(spoken), "{role:?} repeats {spoken:?}");
        assert_eq!(A11yRole::parse(role.name()), Some(role));
    }
}

#[test]
fn chain_link_copies_the_view_and_clamps_the_range() {
    let computed = ComputedNode {
        rect: [5.0, 6.0, 100.0, 50.0],
        content_size: [80.0, 400.0],
        ..ComputedNode::default()
    };
    let view = ScrollView::default();
    let link = chain_link(&computed, [3.0, 7.0], &view);
    assert_eq!(link.offset, [3.0, 7.0]);
    assert_eq!(
        link.max,
        [0.0, 350.0],
        "content narrower than the viewport cannot scroll"
    );
    assert_eq!(link.viewport, [100.0, 50.0]);
    assert_eq!(link.axes, view.axes);
    assert_eq!(link.overscroll, view.overscroll);
    assert_eq!(link.chaining, view.chaining);
    let hostile = ComputedNode {
        rect: [0.0, 0.0, f32::NAN, 10.0],
        content_size: [f32::NAN, f32::INFINITY],
        ..ComputedNode::default()
    };
    let link = chain_link(&hostile, [0.0, 0.0], &view);
    assert!(!link.max[0].is_nan() && link.max[0] >= 0.0, "{:?}", link.max);
}
