// SPDX-License-Identifier: MPL-2.0
//! Spec conformance evidence for `docs/specs/ui-default-design.md` §§ 2.1 to 2.6 (audit ADR-5664): every
//! value of the token tables, in both densities and every built-in theme, and the hard-coded
//! `UiDensity` tables kept equal to the theme files. Pure data: no GPU, no files, no clocks.

use regia_ui::theme_color_ops::to_hex;
use regia_ui::theme_file::{Density, ResolvedTheme, ThemeSet};
use regia_ui::theme_tokens::Easing;
use regia_ui::{UiDensity, WidgetSize};

const BUILTINS: [&str; 4] = [
    "regia-dark",
    "regia-light",
    "regia-high-contrast",
    "regia-light-high-contrast",
];

fn resolve(name: &str, density: Density) -> ResolvedTheme {
    ThemeSet::with_builtins()
        .resolve(name, Some(density))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn num(t: &ResolvedTheme, key: &str) -> f32 {
    t.number(key)
        .unwrap_or_else(|| panic!("no number {key} in {} ({:?})", t.name, t.density))
}

/// § 2.3 type scale: role, comfortable (size, line), compact (size, line), weight.
const TYPE_SCALE: [(&str, [f32; 2], [f32; 2], f32); 7] = [
    ("caption", [14.0, 20.0], [11.0, 16.0], 400.0),
    ("body", [18.0, 26.0], [13.0, 18.0], 400.0),
    ("label", [18.0, 24.0], [13.0, 18.0], 700.0),
    ("title", [24.0, 32.0], [16.0, 22.0], 700.0),
    ("headline", [32.0, 40.0], [20.0, 28.0], 700.0),
    ("display", [48.0, 56.0], [28.0, 36.0], 700.0),
    ("mono", [16.0, 24.0], [12.0, 18.0], 400.0),
];

#[test]
fn the_type_scale_holds_in_both_densities_for_every_builtin() {
    for name in BUILTINS {
        for density in [Density::Comfortable, Density::Compact] {
            let t = resolve(name, density);
            for (role, comfortable, compact, weight) in TYPE_SCALE {
                let [size, line] = match density {
                    Density::Comfortable => comfortable,
                    Density::Compact => compact,
                };
                let got = [
                    num(&t, &format!("type.{role}.size")),
                    num(&t, &format!("type.{role}.line")),
                    num(&t, &format!("type.{role}.weight")),
                ];
                assert_eq!(got, [size, line, weight], "{name} {density:?} type.{role}");
            }
        }
    }
}

/// § 2.4: token, comfortable, compact.
const SIZES: [(&str, f32, f32); 12] = [
    ("size.control.sm", 32.0, 22.0),
    ("size.control.md", 44.0, 28.0),
    ("size.control.lg", 56.0, 36.0),
    ("size.icon.sm", 16.0, 12.0),
    ("size.icon.md", 24.0, 16.0),
    ("size.icon.lg", 32.0, 20.0),
    ("size.touch-min", 44.0, 28.0),
    ("size.pad-x.sm", 12.0, 6.0),
    ("size.pad-x.md", 16.0, 8.0),
    ("size.pad-x.lg", 24.0, 12.0),
    ("size.row", 44.0, 24.0),
    ("size.scrollbar", 10.0, 8.0),
];

#[test]
fn the_size_table_holds_in_both_densities_for_every_builtin() {
    for name in BUILTINS {
        for density in [Density::Comfortable, Density::Compact] {
            let t = resolve(name, density);
            for (key, comfortable, compact) in SIZES {
                let want = match density {
                    Density::Comfortable => comfortable,
                    Density::Compact => compact,
                };
                assert_eq!(num(&t, key), want, "{name} {density:?} {key}");
            }
            // "size.scrollbar: 10 (hover 14) / 8 (hover 10)".
            let hover = match density {
                Density::Comfortable => 14.0,
                Density::Compact => 10.0,
            };
            assert_eq!(num(&t, "size.scrollbar-hover"), hover, "{name} {density:?}");
            // § 3 Tree: indent 16 (comfortable) / 12 (compact).
            let indent = match density {
                Density::Comfortable => 16.0,
                Density::Compact => 12.0,
            };
            assert_eq!(num(&t, "size.tree-indent"), indent, "{name} {density:?}");
        }
    }
}

/// The widget code reads sizes from `UiDensity` (`widget_style.rs`), not from the theme: the two must
/// say the same thing, or a theme edit and the layout would disagree.
#[test]
fn the_ui_density_tables_equal_the_theme_size_tokens() {
    for (ui, density) in [
        (UiDensity::Comfortable, Density::Comfortable),
        (UiDensity::Compact, Density::Compact),
    ] {
        let t = resolve("regia-dark", density);
        for (size, suffix) in [
            (WidgetSize::Sm, "sm"),
            (WidgetSize::Md, "md"),
            (WidgetSize::Lg, "lg"),
        ] {
            assert_eq!(
                ui.control_height(size),
                num(&t, &format!("size.control.{suffix}"))
            );
            assert_eq!(ui.icon(size), num(&t, &format!("size.icon.{suffix}")));
            assert_eq!(ui.pad_x(size), num(&t, &format!("size.pad-x.{suffix}")));
        }
        assert_eq!(ui.touch_min(), num(&t, "size.touch-min"));
        assert_eq!(ui.row(), num(&t, "size.row"));
        assert_eq!(
            ui.scrollbar(),
            (num(&t, "size.scrollbar"), num(&t, "size.scrollbar-hover"))
        );
    }
}

#[test]
fn spacing_radius_and_border_tokens_are_density_independent() {
    for name in BUILTINS {
        let high_contrast = name.ends_with("high-contrast");
        for density in [Density::Comfortable, Density::Compact] {
            let t = resolve(name, density);
            let spacing: Vec<f32> = (0..=9).map(|i| num(&t, &format!("space.{i}"))).collect();
            assert_eq!(
                spacing,
                [0.0, 2.0, 4.0, 8.0, 12.0, 16.0, 24.0, 32.0, 48.0, 64.0],
                "{name}"
            );
            let radius: Vec<f32> = ["none", "sm", "md", "lg", "xl", "pill"]
                .iter()
                .map(|r| num(&t, &format!("radius.{r}")))
                .collect();
            assert_eq!(radius, [0.0, 4.0, 8.0, 12.0, 16.0, 9999.0], "{name}");
            // § 2.2 / § 2.1: high contrast widens the hairline to 2 and the ring to 3, with the
            // inset at minus the ring width.
            let (hairline, focus, inset) = if high_contrast {
                (2.0, 3.0, -3.0)
            } else {
                (1.0, 2.0, -2.0)
            };
            assert_eq!(num(&t, "border.width.hairline"), hairline, "{name}");
            assert_eq!(num(&t, "border.width.thick"), 2.0, "{name}");
            assert_eq!(num(&t, "border.width.focus"), focus, "{name}");
            assert_eq!(num(&t, "border.width.focus-offset"), 2.0, "{name}");
            assert_eq!(num(&t, "border.width.focus-inset"), inset, "{name}");
        }
    }
}

#[test]
fn elevation_rows_hold_in_both_schemes() {
    // § 2.5: (y, blur) shared, alpha of `color.shadow` per scheme.
    let rows = [(0.0, 0.0), (1.0, 2.0), (4.0, 12.0), (12.0, 32.0)];
    for (name, alphas) in [
        ("regia-dark", [0.0, 0.35, 0.45, 0.55]),
        ("regia-light", [0.0, 0.10, 0.14, 0.20]),
    ] {
        let t = resolve(name, Density::Comfortable);
        for (level, ((y, blur), alpha)) in rows.iter().zip(alphas).enumerate() {
            let at = |part: &str| num(&t, &format!("elevation.{level}.{part}"));
            assert_eq!((at("y"), at("blur")), (*y, *blur), "{name} elevation.{level}");
            assert!(
                (at("alpha") - alpha).abs() < 1e-6,
                "{name} elevation.{level}.alpha"
            );
        }
        assert_eq!(
            t.color("elevation.color"),
            t.color("color.shadow"),
            "{name}: shadows use color.shadow"
        );
    }
}

#[test]
fn motion_durations_and_easings_hold() {
    let t = resolve("regia-dark", Density::Comfortable);
    let durations: Vec<f32> = ["instant", "fast", "base", "slow", "page"]
        .iter()
        .map(|m| t.duration(&format!("motion.{m}")).expect(m))
        .collect();
    assert_eq!(durations, [0.0, 0.08, 0.16, 0.24, 0.32]);
    assert_eq!(
        t.easing("ease.standard"),
        Some(Easing::CubicBezier([0.2, 0.0, 0.0, 1.0]))
    );
    assert_eq!(
        t.easing("ease.enter"),
        Some(Easing::CubicBezier([0.0, 0.0, 0.0, 1.0]))
    );
    assert_eq!(
        t.easing("ease.exit"),
        Some(Easing::CubicBezier([0.3, 0.0, 1.0, 1.0]))
    );
    assert_eq!(t.easing("ease.linear"), Some(Easing::Linear));
}

/// § 2.1: the derived accent states of both schemes are the values the table prints (the loader's
/// OKLab steps, ADR-290). The dark pair is also pinned in `color_ops.rs`; the light pair was not.
#[test]
fn derived_accent_values_match_the_section_2_1_table() {
    for (name, hover, pressed) in [
        ("regia-dark", "#5ab0ff", "#1c84d5"),
        ("regia-light", "#3382d8", "#0057a5"),
    ] {
        let t = resolve(name, Density::Comfortable);
        let hex = |key: &str| to_hex(t.color(key).unwrap_or_else(|| panic!("{key}")));
        assert_eq!(hex("color.accent-hover"), hover, "{name}");
        assert_eq!(hex("color.accent-pressed"), pressed, "{name}");
        assert_eq!(
            hex("color.info"),
            hex("color.accent"),
            "{name}: info is the accent"
        );
    }
}
