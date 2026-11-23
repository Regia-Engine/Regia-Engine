// SPDX-License-Identifier: MPL-2.0
//! `.theme.ron` v1 (lane D1): built-in themes, tokens and expressions, the `extends` chain, load
//! errors with key paths, the v0 migrator, and the WCAG contrast of the default palettes
//! (`docs/specs/ui-default-design.md` §§ 2, 6.1 to 6.3). Pure data: no GPU, no files, no clocks.

use regia_ui::theme_color_ops::{contrast_ratio, lighten, to_hex, to_oklab};
use regia_ui::theme_file::{
    BUILTIN_THEME_SOURCES, Density, ResolvedTheme, Scheme, ThemeError, ThemeFile, ThemeSet,
};
use regia_ui::theme_migrate::{class_target, migrate_v0};
use regia_ui::theme_tokens::Easing;
use regia_ui::{
    THEME_FORMAT_VERSION, ThemeClasses, UiColor, UiTheme, parse_theme_ron, theme_migration_sources,
    theme_schema_fingerprint,
};

fn builtins() -> ThemeSet {
    ThemeSet::with_builtins()
}

fn resolved(set: &ThemeSet, name: &str) -> ResolvedTheme {
    set.resolve(name, None)
        .unwrap_or_else(|e| panic!("resolving {name}: {e}"))
}

fn hex(t: &ResolvedTheme, key: &str) -> String {
    to_hex(
        t.color(key)
            .unwrap_or_else(|| panic!("no color token {key} in {}", t.name)),
    )
}

fn theme(src: &str) -> ThemeFile {
    parse_theme_ron(src, "test.theme.ron").unwrap_or_else(|e| panic!("{e}"))
}

// --- built-ins -------------------------------------------------------------------------------

#[test]
fn every_builtin_parses_and_resolves_in_both_densities() {
    for (name, source) in BUILTIN_THEME_SOURCES {
        let file = parse_theme_ron(source, name).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(file.name, name);
        assert_eq!(file.format_version, THEME_FORMAT_VERSION);
    }
    let set = builtins();
    assert_eq!(
        set.names().collect::<Vec<_>>(),
        [
            "regia-dark",
            "regia-high-contrast",
            "regia-light",
            "regia-light-high-contrast"
        ]
    );
    for name in [
        "regia-dark",
        "regia-light",
        "regia-high-contrast",
        "regia-light-high-contrast",
    ] {
        for density in [Density::Comfortable, Density::Compact] {
            let t = set
                .resolve(name, Some(density))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(t.values.len() > 100, "{name} has {} tokens", t.values.len());
        }
    }
}

#[test]
fn schemes_and_chains_of_the_builtins() {
    let set = builtins();
    assert_eq!(resolved(&set, "regia-dark").scheme, Scheme::Dark);
    assert_eq!(resolved(&set, "regia-light").scheme, Scheme::Light);
    let hc = resolved(&set, "regia-high-contrast");
    assert_eq!(hc.scheme, Scheme::Dark);
    assert_eq!(hc.chain, ["regia-high-contrast", "regia-dark"]);
}

#[test]
fn dark_and_light_hold_the_section_2_1_color_roles() {
    let set = builtins();
    let dark = resolved(&set, "regia-dark");
    let light = resolved(&set, "regia-light");
    let table = [
        ("color.bg", "#0f1318", "#e9edf2"),
        ("color.surface", "#171c23", "#f6f8fa"),
        ("color.surface-raised", "#1f2630", "#ffffff"),
        ("color.surface-overlay", "#262e3a", "#ffffff"),
        ("color.surface-sunken", "#0b0e12", "#e3e8ee"),
        ("color.border", "#2e3744", "#c9d1db"),
        ("color.border-strong", "#5f6b7c", "#7c8796"),
        ("color.text", "#e8edf4", "#18202a"),
        ("color.text-muted", "#9aa4b2", "#566170"),
        ("color.text-disabled", "#5d6673", "#8f99a5"),
        ("color.accent", "#3d9df0", "#1d6fc4"),
        ("color.on-accent", "#0b1220", "#ffffff"),
        ("color.selection", "#3d9df047", "#1d6fc433"),
        ("color.focus-ring", "#9cd3ff", "#0b4f9e"),
        ("color.danger", "#e5534b", "#c4372f"),
        ("color.on-danger", "#1a0806", "#ffffff"),
        ("color.warning", "#e0a33a", "#975a00"),
        ("color.success", "#3fb950", "#1f7a35"),
        ("color.scrim", "#000000a6", "#0f131866"),
        ("color.shadow", "#000000", "#0f1318"),
    ];
    for (key, d, l) in table {
        assert_eq!(hex(&dark, key), d, "dark {key}");
        assert_eq!(hex(&light, key), l, "light {key}");
    }
    // `color.info` is `@color.accent` in both schemes.
    assert_eq!(hex(&dark, "color.info"), hex(&dark, "color.accent"));
    assert_eq!(hex(&light, "color.info"), hex(&light, "color.accent"));
}

#[test]
fn derived_colors_follow_the_accent_in_every_scheme() {
    let set = builtins();
    for name in ["regia-dark", "regia-light"] {
        let t = resolved(&set, name);
        let accent = t.color("color.accent").expect("accent");
        let hover = to_oklab(t.color("color.accent-hover").expect("hover"));
        let pressed = to_oklab(t.color("color.accent-pressed").expect("pressed"));
        let base = to_oklab(accent);
        assert!((hover.l - base.l - 0.06).abs() < 0.01, "{name}: hover lightness");
        assert!(
            (base.l - pressed.l - 0.08).abs() < 0.01,
            "{name}: pressed lightness"
        );
        assert_eq!(hex(&t, "color.accent-hover"), to_hex(lighten(accent, 0.06)));
    }
}

#[test]
fn spacing_radius_type_sizes_elevation_and_motion_tokens() {
    let t = resolved(&builtins(), "regia-dark");
    let n = |k: &str| t.number(k).unwrap_or_else(|| panic!("no number {k}"));
    let spacing: Vec<f32> = (0..=9).map(|i| n(&format!("space.{i}"))).collect();
    assert_eq!(spacing, [0.0, 2.0, 4.0, 8.0, 12.0, 16.0, 24.0, 32.0, 48.0, 64.0]);
    assert_eq!(
        [
            n("radius.none"),
            n("radius.sm"),
            n("radius.md"),
            n("radius.lg"),
            n("radius.xl"),
            n("radius.pill")
        ],
        [0.0, 4.0, 8.0, 12.0, 16.0, 9999.0]
    );
    assert_eq!(
        [
            n("border.width.hairline"),
            n("border.width.thick"),
            n("border.width.focus")
        ],
        [1.0, 2.0, 2.0]
    );
    assert_eq!(
        (n("type.body.size"), n("type.body.line"), n("type.body.weight")),
        (18.0, 26.0, 400.0)
    );
    assert_eq!((n("type.display.size"), n("type.display.weight")), (48.0, 700.0));
    assert_eq!(
        (n("size.control.md"), n("size.touch-min"), n("size.row")),
        (44.0, 44.0, 44.0)
    );
    assert_eq!(
        (n("elevation.2.y"), n("elevation.2.blur"), n("elevation.2.alpha")),
        (4.0, 12.0, 0.45)
    );
    assert_eq!(t.text("font.body"), Some("Inter"));
    assert_eq!(
        t.text("font.display"),
        Some("Inter"),
        "font.display is @font.body"
    );
    assert_eq!(t.text("font.mono"), Some("JetBrains Mono"));
    assert_eq!(t.duration("motion.instant"), Some(0.0));
    assert_eq!(t.duration("motion.fast"), Some(0.08));
    assert_eq!(t.duration("motion.page"), Some(0.32));
    assert_eq!(
        t.easing("ease.standard"),
        Some(Easing::CubicBezier([0.2, 0.0, 0.0, 1.0]))
    );
    assert_eq!(t.easing("ease.linear"), Some(Easing::Linear));
    assert!(t.fonts["Inter"].files.len() == 3);
}

#[test]
fn compact_density_overlays_sizes_and_type_and_children_inherit_it() {
    let set = builtins();
    for name in ["regia-dark", "regia-light", "regia-high-contrast"] {
        let c = set.resolve(name, Some(Density::Compact)).expect("compact");
        assert_eq!(c.density, Density::Compact);
        assert_eq!(c.number("size.control.md"), Some(28.0), "{name}");
        assert_eq!(c.number("size.row"), Some(24.0), "{name}");
        assert_eq!(c.number("type.body.size"), Some(13.0), "{name}");
        assert_eq!(c.number("type.body.line"), Some(18.0), "{name}");
        // Colors and shapes are shared between densities.
        assert_eq!(c.number("radius.md"), Some(8.0));
        let comfortable = set
            .resolve(name, Some(Density::Comfortable))
            .expect("comfortable");
        assert_eq!(hex(&c, "color.accent"), hex(&comfortable, "color.accent"));
        assert_eq!(comfortable.number("size.control.md"), Some(44.0));
    }
    // With no override the density is the one the chain declares: comfortable.
    assert_eq!(resolved(&set, "regia-dark").density, Density::Comfortable);
}

// --- WCAG contrast (spec § 2.1 table) ----------------------------------------------------------

#[test]
fn default_palettes_meet_the_section_2_1_contrast_table() {
    let set = builtins();
    // (foreground, background, dark ratio, light ratio, required). `text-muted` sits on
    // `surface-overlay` in dark and on `surface-sunken` in light, as in the spec table.
    let dark = resolved(&set, "regia-dark");
    let light = resolved(&set, "regia-light");
    let rows: [(&str, &str, &str, f32, f32, f32); 11] = [
        ("color.text", "color.surface", "color.surface", 14.6, 15.4, 4.5),
        (
            "color.text-muted",
            "color.surface",
            "color.surface",
            6.8,
            5.9,
            4.5,
        ),
        (
            "color.text-muted",
            "color.surface-overlay",
            "color.surface-sunken",
            5.4,
            5.1,
            4.5,
        ),
        ("color.on-accent", "color.accent", "color.accent", 6.5, 5.1, 4.5),
        ("color.on-danger", "color.danger", "color.danger", 5.2, 5.3, 4.5),
        ("color.accent", "color.surface", "color.surface", 5.9, 4.8, 3.0),
        ("color.danger", "color.surface", "color.surface", 4.6, 5.0, 4.5),
        ("color.warning", "color.surface", "color.surface", 7.7, 5.2, 4.5),
        ("color.success", "color.surface", "color.surface", 6.7, 5.1, 4.5),
        (
            "color.border-strong",
            "color.surface",
            "color.surface",
            3.2,
            3.4,
            3.0,
        ),
        ("color.focus-ring", "color.bg", "color.bg", 11.7, 6.8, 3.0),
    ];
    let ratio = |t: &ResolvedTheme, fg: &str, bg: &str| {
        contrast_ratio(t.color(fg).expect("fg"), t.color(bg).expect("bg"))
    };
    for (fg, dark_bg, light_bg, want_dark, want_light, required) in rows {
        let d = ratio(&dark, fg, dark_bg);
        let l = ratio(&light, fg, light_bg);
        assert!(d >= required, "dark {fg} on {dark_bg}: {d:.2} < {required}");
        assert!(l >= required, "light {fg} on {light_bg}: {l:.2} < {required}");
        // The spec quotes one decimal; the computed value must agree with it.
        assert!(
            (d - want_dark).abs() < 0.1,
            "dark {fg} on {dark_bg}: {d:.2} vs spec {want_dark}"
        );
        assert!(
            (l - want_light).abs() < 0.1,
            "light {fg} on {light_bg}: {l:.2} vs spec {want_light}"
        );
    }
    // `text-disabled` is exempt (WCAG 1.4.3) and sits near 3.0 / 2.7 as the spec says.
    let d = ratio(&dark, "color.text-disabled", "color.surface");
    let l = ratio(&light, "color.text-disabled", "color.surface");
    assert!((d - 3.0).abs() < 0.15 && (l - 2.7).abs() < 0.15, "{d} {l}");
}

#[test]
fn high_contrast_variant_overrides_exactly_the_documented_tokens() {
    let set = builtins();
    let dark = resolved(&set, "regia-dark");
    let hc = resolved(&set, "regia-high-contrast");
    assert_eq!(hex(&hc, "color.text"), "#ffffff");
    assert_eq!(hex(&hc, "color.text-muted"), "#d0d6de");
    assert_eq!(hex(&hc, "color.border"), "#8d98a7");
    assert_eq!(hex(&hc, "color.focus-ring"), "#ffd400");
    assert_eq!(hc.number("border.width.hairline"), Some(2.0));
    assert_eq!(hc.number("border.width.focus"), Some(3.0));
    assert_eq!(
        hex(&hc, "color.accent"),
        hex(&dark, "color.accent"),
        "everything else is inherited"
    );
    let r = |fg: &str, bg: &str| contrast_ratio(hc.color(fg).expect("fg"), hc.color(bg).expect("bg"));
    assert!(
        r("color.border", "color.surface") >= 5.8,
        "border 5.9:1 on surface"
    );
    assert!(r("color.focus-ring", "color.bg") >= 12.9, "focus ring 13:1 on bg");
    assert!(
        r("color.text-muted", "color.surface") >= 7.0,
        "muted text reaches AAA"
    );
}

// --- extends chain -----------------------------------------------------------------------------

#[test]
fn a_child_theme_overrides_tokens_and_derived_colors_follow() {
    let mut set = builtins();
    set.load_ron(include_str!("fixtures/theme.example.theme.ron"), "my-game")
        .expect("example loads");
    let dark = resolved(&set, "regia-dark");
    let game = resolved(&set, "my-game");
    assert_eq!(game.chain, ["my-game", "regia-dark"]);
    assert_eq!(game.scheme, Scheme::Dark, "scheme inherited from the parent");
    assert_eq!(hex(&game, "color.accent"), "#e8793a");
    assert_eq!(game.number("radius.md"), Some(3.0));
    assert_eq!(game.text("font.body"), Some("Rajdhani"));
    assert_eq!(
        game.text("font.display"),
        Some("Rajdhani"),
        "font.display = @font.body follows"
    );
    // Unlisted tokens come from the parent...
    assert_eq!(game.number("space.5"), Some(16.0));
    assert_eq!(hex(&game, "color.surface"), hex(&dark, "color.surface"));
    // ...and derived ones are recomputed against the child's accent, not the parent's.
    assert_ne!(hex(&game, "color.accent-hover"), hex(&dark, "color.accent-hover"));
    assert_eq!(
        hex(&game, "color.accent-hover"),
        to_hex(lighten(game.color("color.accent").expect("accent"), 0.06))
    );
    assert_eq!(
        hex(&game, "color.selection").len(),
        9,
        "selection keeps its alpha"
    );
    assert_eq!(hex(&game, "color.selection")[..7], hex(&game, "color.accent"));
    // The parent is unchanged.
    assert_eq!(hex(&resolved(&set, "regia-dark"), "color.accent"), "#3d9df0");
}

#[test]
fn a_child_of_light_is_light_and_density_blocks_merge_per_file() {
    let mut set = builtins();
    let child = theme(
        r##"(format_version: 1, name: "kid", extends: "regia-light",
            tokens: {"color.accent": "#aa3300"},
            densities: {"compact": {"size.control.md": 26}})"##,
    );
    set.insert(child).expect("insert");
    let comfortable = resolved(&set, "kid");
    assert_eq!(comfortable.scheme, Scheme::Light);
    assert_eq!(comfortable.number("size.control.md"), Some(44.0));
    let compact = set.resolve("kid", Some(Density::Compact)).expect("compact");
    assert_eq!(
        compact.number("size.control.md"),
        Some(26.0),
        "the child's compact block wins"
    );
    assert_eq!(
        compact.number("size.control.sm"),
        Some(22.0),
        "other keys keep the parent's block"
    );
}

#[test]
fn types_and_variations_merge_down_the_chain() {
    let mut set = builtins();
    set.load_ron(include_str!("fixtures/theme.example.theme.ron"), "my-game")
        .expect("example loads");
    let child = theme(
        r##"(format_version: 1, name: "kid", extends: "my-game",
            variations: {"Button.primary": (props: {"radius": 0}, hover: {"background": "#ffffff"})})"##,
    );
    set.insert(child).expect("insert");
    let t = resolved(&set, "kid");
    let primary = &t.variations["Button.primary"];
    assert_eq!(
        primary.extends.as_deref(),
        Some("Button"),
        "extends survives a partial override"
    );
    assert!(primary.props.contains_key("background"), "parent property kept");
    assert_eq!(
        primary.props["radius"],
        regia_ui::theme_file::PropValue::Number(0.0),
        "child property wins"
    );
    assert_eq!(
        primary.hover["background"],
        regia_ui::theme_file::PropValue::Text("#ffffff".to_owned())
    );
    assert!(primary.pressed.contains_key("background"));
    assert_eq!(
        t.variations["Button.danger"].extends.as_deref(),
        Some("Button.primary")
    );
}

// --- load errors with key paths ----------------------------------------------------------------

#[test]
fn unknown_reference_reports_the_key_path_and_the_missing_token() {
    let mut set = builtins();
    let bad = theme(
        r#"(format_version: 1, name: "bad", extends: "regia-dark",
        tokens: {"color.hover": "lighten(@color.nope, 0.1)"})"#,
    );
    assert_eq!(
        set.insert(bad),
        Err(ThemeError::UnknownReference {
            theme: "bad".to_owned(),
            key: "tokens.color.hover".to_owned(),
            reference: "color.nope".to_owned()
        })
    );
    assert!(set.get("bad").is_none());
}

#[test]
fn reference_cycles_are_errors_with_the_chain() {
    let mut set = builtins();
    let bad = theme(
        r#"(format_version: 1, name: "loop", extends: "regia-dark",
        tokens: {"a": "@b", "b": "lighten(@c, 0.1)", "c": "@a"})"#,
    );
    match set.insert(bad) {
        Err(ThemeError::Cycle { theme, chain }) => {
            assert_eq!(theme, "loop");
            assert_eq!(chain, "tokens.a -> tokens.b -> tokens.c -> tokens.a");
        }
        other => panic!("expected a cycle, got {other:?}"),
    }
    let own = theme(r#"(format_version: 1, name: "self", tokens: {"x": "@x"})"#);
    assert!(matches!(
        ThemeSet::default().insert(own),
        Err(ThemeError::Cycle { .. })
    ));
}

#[test]
fn bad_values_name_the_token_and_say_why() {
    let cases = [
        (r##"{"color.accent": "#12"}"##, "tokens.color.accent"),
        (r#"{"color.accent": "wobble(#fff, 1)"}"#, "tokens.color.accent"),
        (r#"{"color.x": "lighten(#fff)"}"#, "tokens.color.x"),
        (r#"{"motion.x": "cubic-bezier(1, 2, 3)"}"#, "tokens.motion.x"),
    ];
    for (tokens, key) in cases {
        let src = format!(r#"(format_version: 1, name: "bad", tokens: {tokens})"#);
        match parse_theme_ron(&src, "bad.theme.ron") {
            Err(ThemeError::Value {
                theme,
                key: k,
                message,
            }) => {
                assert_eq!((theme.as_str(), k.as_str()), ("bad", key));
                assert!(!message.is_empty());
            }
            other => panic!("{tokens}: expected a value error, got {other:?}"),
        }
    }
}

#[test]
fn operations_reject_the_wrong_kind_of_value() {
    let mut set = builtins();
    let bad = theme(
        r#"(format_version: 1, name: "bad", extends: "regia-dark",
        tokens: {"color.x": "lighten(@motion.fast, 0.1)"})"#,
    );
    match set.insert(bad) {
        Err(ThemeError::Value { key, message, .. }) => {
            assert_eq!(key, "tokens.color.x");
            assert!(
                message.contains("color") && message.contains("duration"),
                "{message}"
            );
        }
        other => panic!("expected a type error, got {other:?}"),
    }
}

#[test]
fn extends_errors() {
    let mut set = builtins();
    let orphan = theme(r#"(format_version: 1, name: "orphan", extends: "nope")"#);
    assert_eq!(
        set.insert(orphan),
        Err(ThemeError::UnknownParent {
            theme: "orphan".to_owned(),
            parent: "nope".to_owned()
        })
    );
    let own = theme(r#"(format_version: 1, name: "ouro", extends: "ouro")"#);
    assert!(matches!(set.insert(own), Err(ThemeError::ExtendsCycle { .. })));
    assert_eq!(set, builtins(), "failed inserts leave the set untouched");
}

#[test]
fn parse_errors_and_versions() {
    match parse_theme_ron("(format_version: 1, name: ", "broken.theme.ron") {
        Err(ThemeError::Parse { path, message }) => {
            assert_eq!(path, "broken.theme.ron");
            assert!(!message.is_empty());
        }
        other => panic!("expected a parse error, got {other:?}"),
    }
    assert_eq!(
        parse_theme_ron(r#"(format_version: 99, name: "future")"#, "future.theme.ron"),
        Err(ThemeError::UnsupportedVersion {
            path: "future.theme.ron".to_owned(),
            found: 99,
            supported: THEME_FORMAT_VERSION
        })
    );
    assert!(matches!(
        parse_theme_ron(r#"(format_version: 1, name: "")"#, "x"),
        Err(ThemeError::Value { .. })
    ));
}

#[test]
fn a_rejected_reload_keeps_the_previous_good_theme() {
    let mut set = ThemeSet::default();
    set.load_ron(
        r##"(format_version: 1, name: "base", tokens: {"color.accent": "#112233"})"##,
        "base",
    )
    .expect("base");
    set.load_ron(
        r#"(format_version: 1, name: "kid", extends: "base", tokens: {"x": "@color.accent"})"#,
        "kid",
    )
    .expect("kid");
    let before = set.clone();
    // Replacing the parent with one that drops `color.accent` would break the child: rejected,
    // and the error names the child's key.
    let err = set
        .load_ron(
            r##"(format_version: 1, name: "base", tokens: {"color.other": "#000000"})"##,
            "base",
        )
        .expect_err("must be rejected");
    assert_eq!(
        err,
        ThemeError::UnknownReference {
            theme: "kid".to_owned(),
            key: "tokens.x".to_owned(),
            reference: "color.accent".to_owned()
        }
    );
    assert_eq!(set, before);
    assert_eq!(hex(&resolved(&set, "kid"), "x"), "#112233");
    assert_eq!(set.descendants_of("base"), ["kid"]);
}

#[test]
fn property_values_are_checked_with_their_key_path() {
    let mut set = builtins();
    let cases = [
        (
            r#"types: {"Button": (hover: {"background": "@color.nope"})}"#,
            "types.Button.hover.background",
        ),
        (
            r#"variations: {"Button.primary": (sizes: {"lg": {"border": [1, "@color.nope"]}})}"#,
            "variations.Button.primary.sizes.lg.border",
        ),
    ];
    for (body, key) in cases {
        let t = theme(&format!(
            r#"(format_version: 1, name: "p", extends: "regia-dark", {body})"#
        ));
        match set.insert(t) {
            Err(ThemeError::UnknownReference {
                key: k, reference, ..
            }) => {
                assert_eq!((k.as_str(), reference.as_str()), (key, "color.nope"));
            }
            other => panic!("{key}: got {other:?}"),
        }
    }
    // A keyword that is not an expression is not parsed.
    let ok = theme(
        r#"(format_version: 1, name: "p", extends: "regia-dark",
        types: {"Label": (props: {"align": "center", "text": "@type.body.size"})})"#,
    );
    set.insert(ok)
        .expect("keywords and references to number tokens load");
}

#[test]
fn variation_bases_must_exist_and_must_not_loop() {
    let mut set = builtins();
    let missing =
        theme(r#"(format_version: 1, name: "v", variations: {"Button.x": (extends: "Button.nope")})"#);
    match ThemeSet::default().insert(missing) {
        Err(ThemeError::Variation { key, .. }) => assert_eq!(key, "variations.Button.x.extends"),
        other => panic!("got {other:?}"),
    }
    let looped = theme(
        r#"(format_version: 1, name: "v", variations: {"A.x": (extends: "A.y"), "A.y": (extends: "A.x")})"#,
    );
    assert!(matches!(set.insert(looped), Err(ThemeError::Variation { .. })));
    // A bare widget-type base is supplied by the widget code and needs no entry.
    let bare = theme(r#"(format_version: 1, name: "v", variations: {"Button.x": (extends: "Button")})"#);
    set.insert(bare).expect("bare type base");
}

// --- round trip --------------------------------------------------------------------------------

#[test]
fn files_round_trip_and_keep_unknown_tokens() {
    let src = include_str!("fixtures/theme.example.theme.ron");
    let file = theme(src);
    let text = file.to_ron_string().expect("serialise");
    assert_eq!(theme(&text), file, "save then load is the identity");
    for (name, source) in BUILTIN_THEME_SOURCES {
        let f = theme(source);
        assert_eq!(theme(&f.to_ron_string().expect("serialise")), f, "{name}");
    }
    let mut set = builtins();
    set.insert(file).expect("insert");
    assert_eq!(set.unknown_token_keys("my-game"), ["game.hud-warning"]);
    assert!(set.unknown_token_keys("regia-light").is_empty());
    assert!(resolved(&set, "my-game").color("game.hud-warning").is_some());
}

#[test]
fn schema_fingerprint_lists_nested_structs_and_enum_variants() {
    let fp = theme_schema_fingerprint();
    for needle in [
        "format_version",
        "extends",
        "densities",
        "variations",
        "sounds",
        "hover",
        "sizes",
        "files",
        "dark",
        "compact",
        // v2 (ADR-8105 (e)): the keyboard-only layer, the style rules and their states.
        "focus_visible",
        "rules",
        "inside",
        "variation",
        "pressed",
    ] {
        assert!(fp.contains(needle), "{needle} missing from {fp}");
    }
    // v0 → current (the migrator) and the number-only v1 → v2 step.
    assert_eq!(theme_migration_sources(), [0, 1]);
}

// --- v0 -> v1 migrator -------------------------------------------------------------------------

const SLOTS: [&str; 10] = [
    "color.bg",
    "color.surface",
    "color.surface-raised",
    "color.accent",
    "color.on-accent",
    "color.text",
    "color.text-muted",
    "color.border",
    "color.scrim",
    "color.danger",
];

fn v0_slots(t: &UiTheme) -> [UiColor; 10] {
    let c = &t.colors;
    [
        c.background,
        c.surface,
        c.surface_hi,
        c.primary,
        c.on_primary,
        c.text,
        c.text_dim,
        c.border,
        c.overlay,
        c.danger,
    ]
}

#[test]
fn v0_dark_and_light_migrate_to_identical_resolved_colors() {
    for (v0, parent) in [(UiTheme::dark(), "regia-dark"), (UiTheme::light(), "regia-light")] {
        // A v0 file as the serde shape writes it (colors as linear channels, no format_version).
        let text = ron::to_string(&v0).expect("serialise v0");
        let file = parse_theme_ron(&text, "old.theme.ron").expect("migrate");
        assert_eq!(file.format_version, THEME_FORMAT_VERSION);
        assert_eq!(file.name, v0.name);
        assert_eq!(file.extends.as_deref(), Some(parent));
        let mut set = builtins();
        set.insert(file).expect("migrated theme loads");
        let t = resolved(&set, &v0.name);
        for (key, want) in SLOTS.iter().zip(v0_slots(&v0)) {
            assert_eq!(t.color(key), Some(want), "{} {key}", v0.name);
        }
        assert_eq!(t.number("radius.md"), Some(v0.radius));
        assert_eq!(t.number("type.body.size"), Some(v0.font_px));
    }
}

#[test]
fn v0_with_arbitrary_colors_survives_within_one_8_bit_code() {
    let mut v0 = UiTheme::dark();
    v0.name = "custom".to_owned();
    v0.colors.primary = UiColor::linear(0.2, 0.4, 0.6, 1.0);
    v0.colors.overlay = UiColor::linear(0.0, 0.0, 0.0, 0.5);
    v0.radius = 3.0;
    v0.font_px = 20.0;
    let file = migrate_v0(&v0, &ThemeClasses::default());
    let mut set = builtins();
    set.insert(file).expect("insert");
    let t = resolved(&set, "custom");
    assert_eq!(hex(&t, "color.accent"), to_hex(v0.colors.primary));
    assert_eq!(hex(&t, "color.scrim"), to_hex(v0.colors.overlay));
    // Derived tokens come from the migrated accent.
    assert_eq!(
        hex(&t, "color.accent-hover"),
        to_hex(lighten(t.color("color.accent").expect("accent"), 0.06))
    );
}

#[test]
fn a_partial_v0_file_fills_missing_slots_with_the_v0_defaults() {
    let text = r##"(name: "orange", colors: (primary: "#ff8800"), radius: 3.0)"##;
    let file = parse_theme_ron(text, "old.theme.ron").expect("migrate");
    let mut set = builtins();
    set.insert(file).expect("insert");
    let t = resolved(&set, "orange");
    assert_eq!(hex(&t, "color.accent"), "#ff8800");
    assert_eq!(hex(&t, "color.bg"), "#10141a", "v0 default background");
    assert_eq!(t.number("radius.md"), Some(3.0));
    assert_eq!(t.number("type.body.size"), Some(18.0), "v0 default font_px");
}

#[test]
fn explicit_format_version_zero_also_migrates() {
    let file = parse_theme_ron(r#"(format_version: 0, name: "zero")"#, "z.theme.ron").expect("migrate");
    assert_eq!(
        file.format_version,
        regia_ui::THEME_FORMAT_VERSION,
        "migrated to the current version"
    );
    assert_eq!(file.extends.as_deref(), Some("regia-dark"));
}

#[test]
fn migrated_files_save_as_v1_and_reload_equal() {
    let file = migrate_v0(&UiTheme::dark(), &ThemeClasses::builtin());
    let text = file.to_ron_string().expect("serialise");
    // Saved at the current version (v2 since ADR-8105 (e)).
    assert!(
        text.contains(&format!("format_version: {}", regia_ui::THEME_FORMAT_VERSION)),
        "{text}"
    );
    assert_eq!(theme(&text), file);
}

#[test]
fn v0_classes_become_types_and_variations() {
    assert_eq!(class_target("button").key, "Button");
    assert!(!class_target("button").variation);
    assert_eq!(class_target("slider-track").key, "Slider.track");
    assert_eq!(class_target("primary").key, "Button.primary");
    assert!(class_target("primary").variation);
    assert_eq!(
        class_target("weird").key,
        "Panel.weird",
        "unknown names are Panel variations"
    );

    let mut classes = ThemeClasses::builtin();
    let mut primary = regia_ui::theme::StyleClass::default();
    primary.base.background = UiColor::from_hex("#ff8800");
    primary.hover.background = UiColor::from_hex("#ffaa33");
    classes.0.push(("primary".to_owned(), primary));
    classes
        .0
        .push(("weird".to_owned(), regia_ui::theme::StyleClass::default()));
    let file = migrate_v0(&UiTheme::dark(), &classes);

    use regia_ui::theme_file::PropValue::{List, Number, Text};
    let button = &file.types["Button"];
    assert_eq!(button.props["border_width"], Number(1.0));
    assert_eq!(button.props["radius"], Number(8.0));
    assert_eq!(button.focused["border_width"], Number(2.0));
    assert_eq!(
        button.props["transition"],
        List(vec![Text("80ms".to_owned()), Text("@ease.standard".to_owned())])
    );
    assert_eq!(file.types["Toggle"].props["radius"], Number(16.0));
    assert_eq!(file.types["Panel"].props["border_width"], Number(1.0));
    assert!(file.types.contains_key("Slider.track") && file.types.contains_key("Tabs"));
    let p = &file.variations["Button.primary"];
    assert_eq!(p.extends.as_deref(), Some("Button"));
    assert_eq!(p.props["background"], Text("#ff8800".to_owned()));
    assert_eq!(p.hover["background"], Text("#ffaa33".to_owned()));
    assert_eq!(file.variations["Panel.weird"].extends.as_deref(), Some("Panel"));
    assert!(file.variations.contains_key("Button.nav") && file.variations.contains_key("Panel.surface"));

    let mut set = builtins();
    set.insert(file).expect("the migrated classes load and resolve");
}

/// audit-v035-formats 1: the v0 → v1 migrator kept 6 of the 18 visual fields and only the
/// base/hover/pressed/focused/disabled layers, so `selected`, `error`, the `sm`/`lg` sizes and
/// the parts of a class vanished from the migrated file.
#[test]
fn a_fully_populated_v0_class_survives_migration() {
    use regia_ui::theme::{StyleClass, VisualOverrides};
    use regia_ui::theme_file::PropValue::{List, Number, Text};
    let full = |seed: f32| VisualOverrides {
        background: UiColor::from_hex("#102030"),
        border_color: UiColor::from_hex("#203040"),
        border_width: Some(seed + 1.0),
        radius: Some(seed + 2.0),
        text_color: UiColor::from_hex("#304050"),
        focus_ring: UiColor::from_hex("#405060"),
        focus_ring_width: Some(seed + 3.0),
        focus_ring_offset: Some(seed + 4.0),
        padding_x: Some(seed + 5.0),
        height: Some(seed + 6.0),
        min_width: Some(seed + 7.0),
        gap: Some(seed + 8.0),
        text_px: Some(seed + 9.0),
        icon_color: UiColor::from_hex("#506070"),
        elevation: Some(3),
        opacity: Some(0.5),
        translate: Some([0.0, 1.0]),
        scale: Some(1.25),
        padding: Some([seed + 10.0, seed + 11.0, seed + 12.0, seed + 13.0]),
        min_height: Some(seed + 14.0),
        max_width: Some(seed + 15.0),
        max_height: Some(seed + 16.0),
    };
    let thumb = StyleClass {
        base: full(100.0),
        ..StyleClass::default()
    };
    let class = StyleClass {
        base: full(0.0),
        hover: full(10.0),
        pressed: full(20.0),
        focused: full(30.0),
        disabled: full(40.0),
        selected: full(50.0),
        error: full(60.0),
        sm: full(70.0),
        lg: full(80.0),
        parts: vec![("thumb".to_owned(), thumb)],
        transition: 0.08,
        ..StyleClass::default()
    };
    let classes = ThemeClasses(vec![("toggle".to_owned(), class)]);
    let file = migrate_v0(&UiTheme::dark(), &classes);
    let def = &file.types["Toggle"];
    let layers = [
        ("props", &def.props, 0.0),
        ("hover", &def.hover, 10.0),
        ("pressed", &def.pressed, 20.0),
        ("focused", &def.focused, 30.0),
        ("disabled", &def.disabled, 40.0),
        ("selected", &def.selected, 50.0),
        ("error", &def.error, 60.0),
    ];
    let sizes = [("sm", 70.0f32), ("lg", 80.0)];
    let mut checked = Vec::new();
    for (name, props, seed) in layers
        .into_iter()
        .chain(sizes.iter().map(|(size, seed)| (*size, &def.sizes[*size], *seed)))
    {
        assert_eq!(props["border_width"], Number(seed + 1.0), "{name}");
        assert_eq!(props["radius"], Number(seed + 2.0), "{name}");
        assert_eq!(props["focus_ring_width"], Number(seed + 3.0), "{name}");
        assert_eq!(props["focus_ring_offset"], Number(seed + 4.0), "{name}");
        assert_eq!(props["padding_x"], Number(seed + 5.0), "{name}");
        assert_eq!(props["height"], Number(seed + 6.0), "{name}");
        assert_eq!(props["min_width"], Number(seed + 7.0), "{name}");
        assert_eq!(props["gap"], Number(seed + 8.0), "{name}");
        assert_eq!(props["text_px"], Number(seed + 9.0), "{name}");
        assert_eq!(props["elevation"], Number(3.0), "{name}");
        assert_eq!(props["opacity"], Number(0.5), "{name}");
        assert_eq!(props["scale"], Number(1.25), "{name}");
        assert_eq!(props["translate"], List(vec![Number(0.0), Number(1.0)]), "{name}");
        // ADR-7208: padding is written top, right, bottom, left (stored left, top, right, bottom).
        assert_eq!(
            props["padding"],
            List(vec![
                Number(seed + 11.0),
                Number(seed + 12.0),
                Number(seed + 13.0),
                Number(seed + 10.0)
            ]),
            "{name}"
        );
        assert_eq!(props["min_height"], Number(seed + 14.0), "{name}");
        assert_eq!(props["max_width"], Number(seed + 15.0), "{name}");
        assert_eq!(props["max_height"], Number(seed + 16.0), "{name}");
        for key in [
            "background",
            "border_color",
            "text_color",
            "focus_ring",
            "icon_color",
        ] {
            assert!(matches!(props.get(key), Some(Text(_))), "{name}: {key}");
        }
        checked.push(name);
    }
    assert_eq!(checked.len(), 9);
    let part = file.types.get("Toggle.thumb").expect("the class's thumb part");
    assert_eq!(part.props["height"], Number(106.0));
    let mut set = builtins();
    set.insert(file).expect("the migrated class loads and resolves");
}

#[test]
fn density_overlay_errors_use_the_overlay_path() {
    let mut set = builtins();
    let bad = theme(
        r#"(format_version: 1, name: "dens", extends: "regia-dark",
        densities: {"compact": {"size.x": "@nope"}})"#,
    );
    match set.insert(bad) {
        Err(ThemeError::UnknownReference { key, reference, .. }) => {
            assert_eq!(key, "densities.compact.size.x");
            assert_eq!(reference, "nope");
        }
        other => panic!("expected an unknown reference, got {other:?}"),
    }
}

#[test]
fn unknown_theme_is_reported_by_name() {
    assert_eq!(
        builtins().resolve("missing", None),
        Err(ThemeError::UnknownTheme {
            theme: "missing".to_owned()
        })
    );
}
