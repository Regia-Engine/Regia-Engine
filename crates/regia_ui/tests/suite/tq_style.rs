// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): style value parsing and direction rules: dimensions, edge
//! shorthands, locale direction, text direction inheritance, start/end mirroring. Pure data.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_ui::style::{Dimension, Edges, Style, TextDir, locale_is_rtl};

fn dim(text: &str) -> Dimension {
    ron::from_str(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

#[test]
fn dimensions_read_numbers_and_named_forms_and_reject_the_rest() {
    assert_eq!(dim("12"), Dimension::Points(12.0));
    assert_eq!(dim("12.5"), Dimension::Points(12.5));
    assert_eq!(dim("-3"), Dimension::Points(-3.0));
    assert_eq!(dim("Auto"), Dimension::Auto);
    assert_eq!(dim("Points(7)"), Dimension::Points(7.0));
    assert_eq!(dim("Percent(50)"), Dimension::Percent(50.0));
    for bad in ["\"12\"", "Pixels(3)", "Percent", "[1]", "None", "()"] {
        assert!(ron::from_str::<Dimension>(bad).is_err(), "{bad}");
    }
    assert_eq!(Dimension::Points(4.0).scaled(2.5), Dimension::Points(10.0));
    assert_eq!(
        Dimension::Percent(40.0).scaled(9.0),
        Dimension::Percent(40.0),
        "percent does not scale"
    );
    assert_eq!(Dimension::Auto.scaled(9.0), Dimension::Auto);
}

#[test]
fn edges_accept_every_shorthand() {
    let edges = |text: &str| -> Edges { ron::from_str(text).unwrap_or_else(|e| panic!("{text}: {e}")) };
    assert_eq!(edges("8"), Edges::px(8.0));
    assert_eq!(
        edges("(4, 10)"),
        Edges {
            left: Dimension::Points(4.0),
            top: Dimension::Points(10.0),
            right: Dimension::Points(4.0),
            bottom: Dimension::Points(10.0)
        },
        "a pair is (horizontal, vertical)"
    );
    let ltrb = edges("[1, 2, 3, 4]");
    assert_eq!(
        (ltrb.left, ltrb.top, ltrb.right, ltrb.bottom),
        (
            Dimension::Points(1.0),
            Dimension::Points(2.0),
            Dimension::Points(3.0),
            Dimension::Points(4.0)
        )
    );
    let named = edges("(left: 5, bottom: Percent(10))");
    assert_eq!(
        (named.left, named.top, named.right, named.bottom),
        (
            Dimension::Points(5.0),
            Dimension::Auto,
            Dimension::Auto,
            Dimension::Percent(10.0)
        )
    );
    // Three values are not a shorthand, but the named form also reads a
    // sequence positionally (left, top, right) and defaults the rest.
    let three = edges("[1, 2, 3]");
    assert_eq!(
        (three.left, three.top, three.right, three.bottom),
        (
            Dimension::Points(1.0),
            Dimension::Points(2.0),
            Dimension::Points(3.0),
            Dimension::Auto
        )
    );
    assert!(ron::from_str::<Edges>("\"8\"").is_err());
    assert_eq!(Edges::px(3.0).scaled(2.0), Edges::px(6.0));
    let mirrored = ltrb.mirrored();
    assert_eq!(
        (mirrored.left, mirrored.right, mirrored.top, mirrored.bottom),
        (ltrb.right, ltrb.left, ltrb.top, ltrb.bottom)
    );
    assert_eq!(mirrored.mirrored(), ltrb, "mirroring twice is the identity");
}

#[test]
fn locale_direction_uses_the_primary_language_only() {
    for rtl in [
        "ar", "AR", "ar-EG", "ar_EG", "he", "he-IL", "fa_IR", "ur", "ps", "sd", "yi", "dv", "ug", "ckb",
        "ckb-IQ", "Ckb",
    ] {
        assert!(locale_is_rtl(rtl), "{rtl}");
    }
    for ltr in [
        "", "en", "en-US", "fr", "ja", "zh-Hans", "a", "arx", "ckbx", "-ar", "_ar", "x-ar", "hel", "iw ",
    ] {
        assert!(!locale_is_rtl(ltr), "{ltr:?}");
    }
}

#[test]
fn text_direction_inherits_only_when_auto() {
    for inherited in [false, true] {
        assert!(TextDir::Rtl.is_rtl(inherited));
        assert!(!TextDir::Ltr.is_rtl(inherited));
        assert_eq!(TextDir::Auto.is_rtl(inherited), inherited);
    }
    assert_eq!(TextDir::default(), TextDir::Auto);
}

#[test]
fn mirrored_styles_swap_only_the_logical_edges() {
    let style = Style {
        margin: Edges {
            left: Dimension::Points(1.0),
            top: Dimension::Points(2.0),
            right: Dimension::Points(3.0),
            bottom: Dimension::Points(4.0),
        },
        padding: Edges {
            left: Dimension::Points(5.0),
            top: Dimension::Points(6.0),
            right: Dimension::Points(7.0),
            bottom: Dimension::Points(8.0),
        },
        ..Style::default()
    };
    let m = style.mirrored();
    assert_eq!(
        (m.margin.left, m.margin.right),
        (Dimension::Points(3.0), Dimension::Points(1.0))
    );
    assert_eq!(
        (m.padding.left, m.padding.right),
        (Dimension::Points(7.0), Dimension::Points(5.0))
    );
    assert_eq!(
        (m.margin.top, m.margin.bottom),
        (style.margin.top, style.margin.bottom)
    );
    assert_eq!(
        (m.direction, m.justify_content, m.size),
        (style.direction, style.justify_content, style.size)
    );
    assert_eq!(m.mirrored(), style);
    // The taffy step: rtl sets the direction and swaps the edges; ltr is the plain conversion.
    let rtl = style.to_taffy_dir(1.0, true);
    let ltr = style.to_taffy_dir(1.0, false);
    assert_eq!(rtl.direction, taffy::style::Direction::Rtl);
    assert_eq!(ltr.direction, taffy::style::Direction::Ltr);
    assert_ne!(rtl.padding, ltr.padding);
    assert_eq!(rtl.padding, m.to_taffy(1.0).padding);
    assert_eq!(ltr, style.to_taffy(1.0));
}
