// SPDX-License-Identifier: MIT-0
//! Font coverage guard (M7 UI round 5): shapes every string the gallery renders — the
//! text samples *and* every localized UI message in all three locales — with the engine's
//! default font set alone, and asserts no glyph resolves to `.notdef` (glyph id 0).
//!
//! This is the test that would have caught the round-4 gallery tofu: the CJK sample
//! "你好世界 — 日本語のかなと漢字" ended in two missing-glyph boxes because the shipped
//! Noto Sans SC subset predates the sample string and fontdb + cosmic-text found no
//! fallback for 漢/字 (no system CJK fonts on the machine, and none at all on Android).
//!
//! `TextSystem::embedded_only()` deliberately skips the desktop system font scan: passing
//! here means a stock machine with zero fonts — the Android situation — renders the
//! gallery without tofu.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_ui::text::{Text, TextOverflow, TextSystem};
use regia_ui::{Localization, TextSpan, TextStyle};
use ui_gallery::gallery::sample_strings;

/// One localized message per key per locale, extracted from the gallery's `.ftl` files.
/// The values keep their Fluent `{ $arg }` placeholders (they resolve through `Localization`
/// in the app; raw placeholders shape fine and stress the same fonts).
fn localized_strings() -> Vec<String> {
    let mut out = Vec::new();
    let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/localization");
    for locale in ["en", "ar", "zh"] {
        let bytes = std::fs::read(assets.join(locale).join("ui.ftl")).expect("locale file");
        let content = String::from_utf8(bytes).expect("utf8 ftl");
        for line in content.lines() {
            if let Some((_, value)) = line.split_once(" = ")
                && !value.is_empty()
            {
                out.push(value.to_owned());
            }
        }
    }
    out
}

/// Shapes `text` at a typical UI size and returns the indices of glyphs that resolved to
/// `.notdef` (empty = full coverage).
fn notdef_positions(sys: &mut TextSystem, text: &str) -> Vec<usize> {
    let shaped = sys.shape(&Text::new(text), 1.0, Some(600.0), &Localization::default());
    shaped
        .glyphs
        .iter()
        .enumerate()
        .filter(|(_, g)| g.cache_key.glyph_id == 0)
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn gallery_samples_shape_without_notdef() {
    let mut sys = TextSystem::embedded_only();
    let samples = sample_strings();
    assert!(samples.len() >= 7, "gallery sample list shrank: {samples:?}");
    for sample in &samples {
        let tofu = notdef_positions(&mut sys, sample);
        assert!(
            tofu.is_empty(),
            "{sample:?}: {} of {} glyphs resolved to .notdef (missing font coverage)",
            tofu.len(),
            sample.chars().count()
        );
    }
}

#[test]
fn localized_strings_shape_without_notdef_in_every_locale() {
    let mut sys = TextSystem::embedded_only();
    let strings = localized_strings();
    assert!(strings.len() >= 30, "locale files look unparsed: {strings:?}");
    for s in &strings {
        let tofu = notdef_positions(&mut sys, s);
        assert!(
            tofu.is_empty(),
            "{s:?}: {} glyphs resolved to .notdef (missing font coverage)",
            tofu.len()
        );
    }
}

/// The gallery's CJK strings end in the two characters that shipped as tofu in round 4:
/// pin them explicitly so a future charset regression cannot slip past the sample list.
#[test]
fn the_round4_tofu_characters_are_covered() {
    let mut sys = TextSystem::embedded_only();
    for text in ["漢字", "你好世界 — 日本語のかなと漢字、한국어."] {
        let tofu = notdef_positions(&mut sys, text);
        assert!(tofu.is_empty(), "{text:?}: still resolves to .notdef at {tofu:?}");
    }
    // Bold spans query weight 700, which the bundled subsets do not carry: the fallback
    // walk must still resolve every glyph.
    let bold = Text {
        spans: vec![TextSpan::lit("漢字 한국어 مرحبا").with_style(TextStyle {
            bold: true,
            ..TextStyle::default()
        })],
        overflow: TextOverflow::Visible,
        ..Text::default()
    };
    let shaped = sys.shape(&bold, 1.0, Some(600.0), &Localization::default());
    assert!(
        shaped.glyphs.iter().all(|g| g.cache_key.glyph_id != 0),
        "bold multi-script pass left .notdef glyphs"
    );
}

/// Hebrew, Devanagari and Thai ship in the default set (ADR-1842, L06): shape a sentence of each
/// with the embedded fonts alone and require no `.notdef`.
#[test]
fn hebrew_devanagari_and_thai_are_covered() {
    let mut sys = TextSystem::embedded_only();
    for text in [
        "שלום עולם — טקסט בעברית עם ניקוד: בְּרֵאשִׁית",
        "नमस्ते दुनिया — हिन्दी में पाठ: क्षत्रिय संस्कृत ज्ञान",
        "สวัสดีชาวโลก — ข้อความภาษาไทย ที่มีวรรณยุกต์ เก้า ก็ได้",
    ] {
        let tofu = notdef_positions(&mut sys, text);
        assert!(
            tofu.is_empty(),
            "{text:?}: {} glyphs resolved to .notdef at {tofu:?}",
            tofu.len()
        );
    }
}
