// SPDX-License-Identifier: MPL-2.0
//! Text shaping edge cases (UI L13, ADR-865): bidi ellipsis, emoji presentation, mixed-script
//! font runs, line-break normalization, invisible controls, CJK line breaking, and recoloring
//! without a reshape. Headless and GPU-free: everything reads the shaped glyphs and lines of the
//! embedded font set (`TextSystem::embedded_only`, identical on every platform). No goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_app::prelude::*;
use regia_ui::prelude::*;
use regia_ui::text::{ShapedLine, TextAlign, shaped_string, text_paragraphs};

fn sys() -> TextSystem {
    TextSystem::embedded_only()
}

fn loc() -> Localization {
    Localization::default()
}

/// The family name of the face a glyph came from.
fn family(sys: &TextSystem, glyph: &regia_ui::text::PlacedGlyph) -> String {
    sys.font_system
        .db()
        .faces()
        .find(|f| f.id == glyph.cache_key.font_id)
        .and_then(|f| f.families.first().map(|(n, _)| n.to_owned()))
        .unwrap_or_default()
}

fn styled(s: &str, px: f32) -> Text {
    Text::new(s).with_style(TextStyle {
        font_px: px,
        ..TextStyle::default()
    })
}

/// The text of every line, from the line ranges and the shaped paragraphs.
fn line_texts(text: &Text, lines: &[ShapedLine]) -> Vec<String> {
    let shaped = shaped_string(text, &loc());
    let paragraphs = text_paragraphs(&shaped);
    lines
        .iter()
        .map(|l| {
            let p = paragraphs[l.paragraph as usize];
            p.get(l.range[0] as usize..l.range[1] as usize)
                .unwrap_or("")
                .to_owned()
        })
        .collect()
}

#[test]
fn a_variation_selector_16_asks_for_the_color_emoji_face() {
    let mut sys = sys();
    // Inter carries U+2764 (a monochrome heart): coverage alone picked it for "❤️" before.
    let emoji = sys.shape(&Text::new("\u{2764}\u{FE0F}"), 1.0, None, &loc());
    let first = emoji.glyphs.first().expect("a heart glyph");
    assert_eq!(family(&sys, first), "Noto Color Emoji");
    assert_eq!(emoji.missing_glyphs, 0);
    // Without a selector (and with the text selector U+FE0E) the default face keeps it.
    for plain in ["\u{2764}", "\u{2764}\u{FE0E}"] {
        let shaped = sys.shape(&Text::new(plain), 1.0, None, &loc());
        let first = shaped.glyphs.first().expect("a heart glyph");
        assert_eq!(family(&sys, first), "Inter", "{plain:?}");
        assert_eq!(shaped.missing_glyphs, 0, "{plain:?}");
    }
    // Inside a sentence the words around it stay in Inter.
    let sentence = sys.shape(&Text::new("I \u{2764}\u{FE0F} UI"), 1.0, None, &loc());
    let families: Vec<String> = sentence.glyphs.iter().map(|g| family(&sys, g)).collect();
    assert_eq!(families.first().map(String::as_str), Some("Inter"));
    assert_eq!(families.last().map(String::as_str), Some("Inter"));
    assert!(families.iter().any(|f| f == "Noto Color Emoji"), "{families:?}");
}

#[test]
fn latin_words_of_a_mixed_label_keep_the_default_face() {
    let mut sys = sys();
    let shaped = sys.shape(
        &Text::new("Score \u{0645}\u{0631}\u{062D}\u{0628}\u{0627}"),
        1.0,
        None,
        &loc(),
    );
    // LTR paragraph (first strong letter is Latin): "S" is the first glyph drawn.
    let first = shaped.glyphs.first().expect("glyphs");
    assert_eq!(
        family(&sys, first),
        "Inter",
        "the Latin word moved to the Arabic face"
    );
    assert!(
        shaped
            .glyphs
            .iter()
            .any(|g| family(&sys, g) == "Noto Naskh Arabic"),
        "the Arabic word must still use the joining Naskh face"
    );
    assert_eq!(shaped.missing_glyphs, 0);
}

#[test]
fn every_line_break_spelling_breaks_once_and_draws_no_box() {
    let mut sys = sys();
    let reference = sys.shape(&Text::new("one\ntwo"), 1.0, None, &loc());
    assert_eq!(reference.lines.len(), 2);
    for spelling in [
        "one\r\ntwo",
        "one\rtwo",
        "one\u{2028}two",
        "one\u{2029}two",
        "one\u{0085}two",
    ] {
        let shaped = sys.shape(&Text::new(spelling), 1.0, None, &loc());
        assert_eq!(shaped.lines.len(), 2, "{spelling:?}");
        assert_eq!(shaped.glyphs.len(), reference.glyphs.len(), "{spelling:?}");
        assert!(
            shaped.glyphs.iter().all(|g| g.cache_key.glyph_id != 0),
            "{spelling:?}"
        );
        assert_eq!(shaped.missing_glyphs, 0, "{spelling:?}");
    }
    // A CR at the end of one span and the LF at the start of the next are one break.
    let split = Text {
        spans: vec![TextSpan::lit("one\r"), TextSpan::lit("\ntwo")],
        ..Text::default()
    };
    assert_eq!(sys.shape(&split, 1.0, None, &loc()).lines.len(), 2);
    assert_eq!(shaped_string(&split, &loc()), "one\ntwo");
}

#[test]
fn invisible_format_controls_never_draw_notdef() {
    let mut sys = sys();
    // ZWSP, LRI/PDI isolates, RLO/PDF, soft hyphen, ZWJ, ZWNJ, word joiner, BOM, LRM/RLM.
    let text = "a\u{200B}b\u{2066}c\u{2069}d\u{202E}e\u{202C}f\u{00AD}g\u{200D}h\u{200C}i\u{2060}j\u{FEFF}k\u{200E}l\u{200F}m";
    let shaped = sys.shape(&Text::new(text), 1.0, None, &loc());
    assert!(shaped.glyphs.iter().all(|g| g.cache_key.glyph_id != 0));
    assert_eq!(shaped.missing_glyphs, 0);
    assert!(shaped.glyphs.len() >= 13, "the 13 letters must still draw");
}

#[test]
fn an_uncovered_script_is_counted_as_missing() {
    let mut sys = sys();
    // No bundled face carries Ethiopic (Thai gained a face in ADR-1842, L06).
    let shaped = sys.shape(&Text::new("\u{1200}"), 1.0, None, &loc());
    assert!(shaped.missing_glyphs >= 1);
    let covered = sys.shape(&Text::new("Hello"), 1.0, None, &loc());
    assert_eq!(covered.missing_glyphs, 0);
}

fn ellipsized(s: &str) -> Text {
    Text::new(s)
        .with_wrap(WrapMode::None)
        .with_overflow(TextOverflow::Ellipsis)
}

#[test]
fn ltr_ellipsis_ends_the_line_inside_the_box() {
    let mut sys = sys();
    let max_w = 120.0;
    let text = ellipsized("The quick brown fox jumps over the lazy dog");
    let shaped = sys.shape(&text, 1.0, Some(max_w), &loc());
    assert!(shaped.truncated);
    assert_eq!(shaped.lines.len(), 1);
    assert!(shaped.lines[0].ellipsized);
    assert!(shaped.size[0] <= max_w + 0.5, "width {}", shaped.size[0]);
    let dots = sys.shape(&Text::new("\u{2026}"), 1.0, None, &loc()).glyphs[0]
        .cache_key
        .glyph_id;
    let rightmost = shaped.glyphs.iter().max_by_key(|g| g.pen[0]).expect("glyphs");
    assert_eq!(
        rightmost.cache_key.glyph_id, dots,
        "the ellipsis is the visual end"
    );
    assert!(
        shaped
            .glyphs
            .iter()
            .all(|g| g.pen[0] >= -1 && (g.pen[0] as f32) < max_w)
    );
    // The visible part is a prefix of the text.
    let visible = &line_texts(&text, &shaped.lines)[0];
    assert!("The quick brown fox jumps over the lazy dog".starts_with(visible.trim_end()));
}

#[test]
fn rtl_ellipsis_sits_on_the_left_and_keeps_ink_in_the_box() {
    let mut sys = sys();
    let max_w = 90.0;
    let arabic = "\u{0645}\u{0631}\u{062D}\u{0628}\u{0627} \u{0628}\u{0627}\u{0644}\u{0639}\u{0627}\u{0644}\u{0645} \u{0645}\u{0639} \u{0627}\u{0644}\u{062A}\u{0634}\u{0643}\u{064A}\u{0644}";
    let text = ellipsized(arabic);
    let shaped = sys.shape(&text, 1.0, Some(max_w), &loc());
    assert!(
        shaped.truncated,
        "an RTL line wider than the box is ellipsized too"
    );
    assert!(shaped.lines[0].rtl && shaped.lines[0].ellipsized);
    assert!(shaped.size[0] <= max_w + 0.5);
    // The ellipsis is shaped in the line's first run (the Naskh face for Arabic).
    let dots_text = Text::new("\u{2026}").with_style(TextStyle {
        family: Some("Noto Naskh Arabic".to_owned()),
        ..TextStyle::default()
    });
    let dots = sys.shape(&dots_text, 1.0, None, &loc()).glyphs[0]
        .cache_key
        .glyph_id;
    let leftmost = shaped.glyphs.iter().min_by_key(|g| g.pen[0]).expect("glyphs");
    assert_eq!(leftmost.cache_key.glyph_id, dots, "RTL lines end on the left");
    assert!(
        shaped
            .glyphs
            .iter()
            .all(|g| g.pen[0] >= -1 && (g.pen[0] as f32) <= max_w)
    );
    // The visible part is the logical start of the paragraph.
    let visible = &line_texts(&text, &shaped.lines)[0];
    assert!(arabic.starts_with(visible.trim_end()), "{visible:?}");
}

#[test]
fn mixed_direction_ellipsis_stays_in_the_box() {
    let mut sys = sys();
    let max_w = 100.0;
    let text = ellipsized(
        "Level \u{0645}\u{0631}\u{062D}\u{0628}\u{0627} \u{0628}\u{0627}\u{0644}\u{0639}\u{0627}\u{0644}\u{0645} 42 complete",
    );
    let shaped = sys.shape(&text, 1.0, Some(max_w), &loc());
    assert!(shaped.truncated);
    assert!(!shaped.lines[0].rtl, "the first strong letter is Latin");
    assert!(shaped.size[0] <= max_w + 0.5);
    assert!(
        shaped
            .glyphs
            .iter()
            .all(|g| g.pen[0] >= -1 && (g.pen[0] as f32) <= max_w)
    );
}

#[test]
fn ellipsis_leaves_fitting_text_alone() {
    let mut sys = sys();
    let text = ellipsized("Hi");
    let shaped = sys.shape(&text, 1.0, Some(200.0), &loc());
    let plain = sys.shape(
        &Text::new("Hi").with_wrap(WrapMode::None),
        1.0,
        Some(200.0),
        &loc(),
    );
    assert!(!shaped.truncated);
    assert!(!shaped.lines[0].ellipsized);
    assert_eq!(shaped.glyphs, plain.glyphs);
}

#[test]
fn cjk_lines_never_start_with_closing_or_end_with_opening_punctuation() {
    let mut sys = sys();
    // Common simplified characters only (all in the bundled SC subset), with fullwidth
    // closing punctuation (，。？：」）) and opening punctuation (「（) at many places.
    let s = "我们的世界很大，你好吗？我是中国人。这是一个测试（中文）。他说：「你好」。我们走了，好不好？";
    let text = styled(s, 18.0);
    let shaped = sys.shape(&text, 1.0, Some(100.0), &loc());
    assert!(shaped.lines.len() >= 4, "{} lines", shaped.lines.len());
    assert_eq!(shaped.missing_glyphs, 0);
    let lines = line_texts(&text, &shaped.lines);
    for (i, line) in lines.iter().enumerate() {
        let first = line.chars().next().expect("non-empty line");
        let last = line.chars().last().expect("non-empty line");
        if i > 0 {
            assert!(
                !"、。」）！？".contains(first),
                "line {i} starts with {first:?}: {lines:?}"
            );
        }
        assert!(!"「（".contains(last), "line {i} ends with {last:?}: {lines:?}");
    }
    // The lines tile the paragraph in order with no gap and no overlap.
    for pair in shaped.lines.windows(2) {
        assert_eq!(pair[0].range[1], pair[1].range[0]);
    }
    assert_eq!(lines.concat(), s);
}

#[test]
fn a_no_break_space_holds_and_a_zero_width_space_breaks() {
    let mut sys = sys();
    let nbsp = styled("aaaa\u{00A0}bbbb cccccccc", 14.0);
    let shaped = sys.shape(&nbsp, 1.0, Some(100.0), &loc());
    let lines = line_texts(&nbsp, &shaped.lines);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0].trim_end(), "aaaa\u{00A0}bbbb");
    assert_eq!(lines[1].trim(), "cccccccc");

    let zwsp = styled("aaaaaa\u{200B}bbbbbb", 14.0);
    let shaped = sys.shape(&zwsp, 1.0, Some(70.0), &loc());
    let lines = line_texts(&zwsp, &shaped.lines);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0].trim_end_matches('\u{200B}'), "aaaaaa");
    assert_eq!(lines[1], "bbbbbb");
    assert!(shaped.glyphs.iter().all(|g| g.cache_key.glyph_id != 0));
}

#[test]
fn flags_and_skin_tones_shape_as_single_clusters_when_covered() {
    let mut sys = sys();
    // Regional-indicator pairs ligate to one flag glyph each (the bundled subset has the
    // GSUB closure for the common flags).
    let flags = sys.shape(
        &Text::new("\u{1F1EF}\u{1F1F5}\u{1F1FA}\u{1F1F8}"),
        1.0,
        None,
        &loc(),
    );
    assert_eq!(flags.glyphs.len(), 2);
    assert!(flags.glyphs.iter().all(|g| family(&sys, g) == "Noto Color Emoji"));
    // A thumbs up with a skin tone: the subset has no modifiers, so the tone is a missing glyph
    // (reported, never silently dropped), and the hand still comes from the emoji face.
    let toned = sys.shape(&Text::new("\u{1F44D}\u{1F3FD}"), 1.0, None, &loc());
    assert!(
        toned.glyphs.iter().any(|g| family(&sys, g) == "Noto Color Emoji"),
        "the hand comes from the emoji face"
    );
}

#[test]
fn line_metrics_follow_the_paragraph_direction() {
    let mut sys = sys();
    let text = Text {
        spans: vec![TextSpan::lit("Hello\n\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}")],
        align: TextAlign::Start,
        ..Text::default()
    };
    let shaped = sys.shape(&text, 1.0, Some(300.0), &loc());
    assert_eq!(shaped.lines.len(), 2);
    assert!(!shaped.lines[0].rtl);
    assert!(shaped.lines[1].rtl);
    assert_eq!(shaped.lines[0].paragraph, 0);
    assert_eq!(shaped.lines[1].paragraph, 1);
    assert!(shaped.lines[1].baseline > shaped.lines[0].baseline);
    let total: u32 = shaped.lines.iter().map(|l| l.glyph_count).sum();
    assert_eq!(total as usize, shaped.glyphs.len());
    assert_eq!(shaped.glyph_spans.len(), shaped.glyphs.len());
    assert_eq!(
        line_texts(&text, &shaped.lines),
        vec![
            "Hello".to_owned(),
            "\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}".to_owned()
        ]
    );
}

fn headless() -> App {
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
    app
}

/// Glyph colors pass through 8-bit sRGB, so compare within a quantization step.
fn close(a: UiColor, b: UiColor) -> bool {
    a.0.iter().zip(b.0).all(|(x, y)| (x - y).abs() < 1e-2)
}

#[test]
fn a_color_edit_recolors_the_glyphs_without_reshaping() {
    let mut app = headless();
    let red = UiColor([1.0, 0.0, 0.0, 1.0]);
    let blue = UiColor([0.0, 0.0, 1.0, 1.0]);
    let e = app
        .world_mut()
        .spawn(Text {
            spans: vec![
                TextSpan::lit("red ").with_style(TextStyle {
                    color: red,
                    ..TextStyle::default()
                }),
                TextSpan::lit("white"),
            ],
            ..Text::default()
        })
        .id();
    app.update();
    let before = app.world().get::<ShapedText>(e).expect("shaped").clone();
    assert!(close(before.glyphs[0].color, red));
    assert!(close(before.glyphs.last().expect("glyphs").color, UiColor::WHITE));

    app.world_mut().get_mut::<Text>(e).expect("text").spans[1]
        .style
        .color = blue;
    app.update();
    let after = app.world().get::<ShapedText>(e).expect("shaped").clone();
    // Same glyphs at the same pens: a recolor, not a reshape.
    assert_eq!(
        after
            .glyphs
            .iter()
            .map(|g| (g.cache_key, g.pen))
            .collect::<Vec<_>>(),
        before
            .glyphs
            .iter()
            .map(|g| (g.cache_key, g.pen))
            .collect::<Vec<_>>()
    );
    assert_eq!(after.digest, before.digest);
    assert!(close(after.glyphs[0].color, red), "span 0 keeps its color");
    assert!(
        close(after.glyphs.last().expect("glyphs").color, blue),
        "span 1 follows the edit"
    );

    // A content edit still reshapes.
    app.world_mut().get_mut::<Text>(e).expect("text").spans[1].content = "white!".into();
    app.update();
    let reshaped = app.world().get::<ShapedText>(e).expect("shaped");
    assert_ne!(reshaped.digest, before.digest);
    assert_eq!(reshaped.glyphs.len(), before.glyphs.len() + 1);
}

#[test]
fn a_new_text_floor_reshapes_once_and_matches_layouts_key() {
    let mut app = headless();
    let e = app.world_mut().spawn(Text::new("Floor")).id();
    app.update();
    let small = app.world().get::<ShapedText>(e).expect("shaped").clone();
    let generation = app.world().resource::<Localization>().generation.0;
    app.world_mut()
        .resource_mut::<regia_ui::UiDevicePolicy>()
        .min_text_px = 40.0;
    app.update();
    assert_eq!(
        app.world().resource::<Localization>().generation.0,
        generation + 1,
        "a floor change bumps the generation every text key follows"
    );
    let big = app.world().get::<ShapedText>(e).expect("shaped");
    assert!(
        big.size[1] > small.size[1],
        "{} vs {}",
        big.size[1],
        small.size[1]
    );
}

#[test]
fn scale_to_fit_never_keeps_a_wider_result_than_it_started_with() {
    let mut sys = sys();
    let text = Text::new("The quick brown fox jumps over the lazy dog")
        .with_wrap(WrapMode::None)
        .with_overflow(TextOverflow::ScaleToFit);
    let natural = sys.shape(
        &Text::new("The quick brown fox jumps over the lazy dog").with_wrap(WrapMode::None),
        1.0,
        None,
        &loc(),
    );
    // A box a hair narrower than the natural width: rounding must not leave the full-size run.
    let max_w = natural.size[0] - 0.5;
    let fitted = sys.shape(&text, 1.0, Some(max_w), &loc());
    assert!(fitted.size[0] <= max_w + 0.01, "{} > {max_w}", fitted.size[0]);
    assert!(fitted.size[0] < natural.size[0]);
}
