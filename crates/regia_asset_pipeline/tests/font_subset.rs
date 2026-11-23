// SPDX-License-Identifier: MPL-2.0
//! Font subsetting (ADR-2697): the bytes a subset holds, what shaping sees, and the cook and staging around it.
//!
//! Real fonts are the OFL fonts `regia_ui` bundles; the structures they lack come from the generated fixture.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cosmic_text as ct;
use regia_asset::{AssetManifest, cooked_cache_dir, meta_path_for};
use regia_asset_pipeline::font_subset::{
    BAD_SETTINGS, FONT_SUBSETS_FILE, FontImporterSettings, FontSubsetError, FontSubsetMode, FontSubsetReport,
    KeptWhole, NO_LOCALES_KEPT, UNKNOWN_LOCALE, UNREADABLE_KEPT, glyph_closure, plan_font,
    read_project_charsets, run_plan, shaped_glyph_ids, stage_fonts, subset_font, subset_font_with,
    table_checksum,
};
use regia_asset_pipeline::font_subset_fixture::{
    gid, tiny_collection, tiny_font, tiny_font_with, tiny_variable_font,
};
use regia_asset_pipeline::{CookOptions, CookReport, cook_project};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::raw::{FontRef, TableProvider};
use skrifa::{GlyphId, MetadataProvider};

fn inter() -> &'static [u8] {
    regia_ui::text::embedded_font_data()[0].1
}

fn bundled(name: &str) -> &'static [u8] {
    regia_ui::text::embedded_font_data()
        .into_iter()
        .find(|(n, _)| n.starts_with(name))
        .map(|(_, bytes)| bytes)
        .expect("a bundled font")
}

fn chars(text: &str) -> BTreeSet<char> {
    text.chars().collect()
}

/// Outline commands as text (identical paths give identical strings).
#[derive(Default)]
struct Path_(String);

impl OutlinePen for Path_ {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0 += &format!("M{x} {y} ");
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0 += &format!("L{x} {y} ");
    }
    fn quad_to(&mut self, a: f32, b: f32, x: f32, y: f32) {
        self.0 += &format!("Q{a} {b} {x} {y} ");
    }
    fn curve_to(&mut self, a: f32, b: f32, c: f32, d: f32, x: f32, y: f32) {
        self.0 += &format!("C{a} {b} {c} {d} {x} {y} ");
    }
    fn close(&mut self) {
        self.0 += "Z ";
    }
}

fn outline_at(data: &[u8], glyph: u32, location: LocationRef<'_>) -> String {
    let font = FontRef::new(data).expect("parses");
    let outlines = font.outline_glyphs();
    let outline = outlines.get(GlyphId::new(glyph)).expect("the glyph exists");
    let mut pen = Path_::default();
    outline
        .draw(DrawSettings::unhinted(Size::unscaled(), location), &mut pen)
        .expect("draws");
    pen.0
}

fn outline(data: &[u8], glyph: u32) -> String {
    outline_at(data, glyph, LocationRef::default())
}

fn glyph_of(data: &[u8], c: char) -> u32 {
    let font = FontRef::new(data).expect("parses");
    font.charmap().map(c).expect("mapped").to_u32()
}

/// Shapes `text` with `data` as the only font: (glyph id, advance, x) of each glyph.
fn shape(data: &[u8], text: &str) -> Vec<(u16, u32, u32)> {
    let mut db = ct::fontdb::Database::new();
    db.load_font_data(data.to_vec());
    let family = db.faces().next().expect("a face").families[0].0.clone();
    let mut system = ct::FontSystem::new_with_locale_and_db("en-US".to_owned(), db);
    let mut buffer = ct::Buffer::new_empty(ct::Metrics::new(32.0, 40.0));
    buffer.set_size(Some(4096.0), Some(256.0));
    let attrs = ct::Attrs::new().family(ct::Family::Name(&family));
    buffer.set_text(text, &attrs, ct::Shaping::Advanced, None);
    buffer.shape_until_scroll(&mut system, false);
    let mut out = Vec::new();
    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            out.push((glyph.glyph_id, glyph.w.to_bits(), glyph.x.to_bits()));
        }
    }
    out
}

/// The bytes of table `tag`.
fn table<'a>(data: &'a [u8], tag: &[u8; 4]) -> Option<&'a [u8]> {
    let count = usize::from(u16::from_be_bytes([data[4], data[5]]));
    (0..count).find_map(|i| {
        let record = &data[12 + 16 * i..28 + 16 * i];
        (&record[..4] == tag).then(|| {
            let at = u32::from_be_bytes(record[8..12].try_into().expect("four")) as usize;
            let len = u32::from_be_bytes(record[12..16].try_into().expect("four")) as usize;
            &data[at..at + len]
        })
    })
}

/// Every table's record checksum and `head.checkSumAdjustment`, recomputed.
fn assert_checksums_verify(data: &[u8]) {
    let count = usize::from(u16::from_be_bytes([data[4], data[5]]));
    let mut head_at = None;
    for i in 0..count {
        let record = &data[12 + 16 * i..28 + 16 * i];
        let stored = u32::from_be_bytes(record[4..8].try_into().expect("four"));
        let at = u32::from_be_bytes(record[8..12].try_into().expect("four")) as usize;
        let len = u32::from_be_bytes(record[12..16].try_into().expect("four")) as usize;
        let tag = &record[..4];
        let mut bytes = data[at..at + len].to_vec();
        if tag == b"head" {
            head_at = Some(at);
            bytes[8..12].fill(0);
        }
        assert_eq!(
            table_checksum(&bytes),
            stored,
            "checksum of {}",
            String::from_utf8_lossy(tag)
        );
        assert!(at.is_multiple_of(4), "tables start long-aligned");
    }
    let head_at = head_at.expect("head");
    let stored = u32::from_be_bytes(data[head_at + 8..head_at + 12].try_into().expect("four"));
    let mut whole = data.to_vec();
    whole[head_at + 8..head_at + 12].fill(0);
    assert_eq!(
        0xB1B0_AFBAu32.wrapping_sub(table_checksum(&whole)),
        stored,
        "checkSumAdjustment"
    );
}

const LATIN_TEXT: &str = "Hello Wörld, fi fl ffi: the quick brown fox 0123456789 éàü!";

#[test]
fn a_subset_of_a_real_font_parses_and_keeps_kept_outlines_identical() {
    let keep = chars(LATIN_TEXT);
    let out = subset_font(inter(), &keep).expect("subsets");
    assert_eq!(out.kept_whole, None);
    let font = FontRef::new(&out.bytes).expect("the subset parses with skrifa");
    let before = FontRef::new(inter()).expect("source");
    assert_eq!(
        font.maxp().expect("maxp").num_glyphs(),
        before.maxp().expect("maxp").num_glyphs(),
        "glyph ids are retained"
    );
    for c in &keep {
        let (old, new) = (before.charmap().map(*c), font.charmap().map(*c));
        assert_eq!(old, new, "{c} keeps its glyph id");
        let glyph = new.expect("mapped").to_u32();
        assert_eq!(
            outline(inter(), glyph),
            outline(&out.bytes, glyph),
            "outline of {c}"
        );
    }
    assert_checksums_verify(&out.bytes);
    assert!(
        out.glyphs_kept < out.glyphs_total / 2,
        "{} of {}",
        out.glyphs_kept,
        out.glyphs_total
    );
}

#[test]
fn a_removed_character_keeps_its_mapping_and_advance_but_draws_nothing() {
    let out = subset_font(inter(), &chars("abc")).expect("subsets");
    let glyph = glyph_of(inter(), 'W');
    assert!(!outline(inter(), glyph).is_empty(), "the source draws W");
    assert_eq!(glyph_of(&out.bytes, 'W'), glyph, "cmap is unchanged");
    assert_eq!(outline(&out.bytes, glyph), "", "W is empty in the subset");
    let advance = |data: &[u8]| {
        let font = FontRef::new(data).expect("parses");
        font.hmtx()
            .expect("hmtx")
            .advance(GlyphId::new(glyph))
            .expect("advance")
    };
    assert_eq!(advance(inter()), advance(&out.bytes));
}

#[test]
fn only_outline_tables_change_and_every_other_table_is_byte_identical() {
    let out = subset_font(inter(), &chars("abc")).expect("subsets");
    let count = usize::from(u16::from_be_bytes([inter()[4], inter()[5]]));
    for i in 0..count {
        let tag: [u8; 4] = inter()[12 + 16 * i..16 + 16 * i].try_into().expect("four");
        if matches!(&tag, b"glyf" | b"loca" | b"head") {
            continue;
        }
        assert_eq!(table(inter(), &tag), table(&out.bytes, &tag), "{tag:?}");
    }
    assert!(table(&out.bytes, b"GSUB").is_some());
    assert!(table(&out.bytes, b"GPOS").is_some());
}

#[test]
fn shaping_kept_text_gives_identical_glyphs_and_advances_with_the_fi_ligature() {
    let source = tiny_font();
    let out = subset_font(&source, &chars("fie ")).expect("subsets");
    let before = shape(&source, "fi e");
    assert_eq!(
        before[0].0,
        gid::F_I,
        "the fixture shapes f + i to the ligature: {before:?}"
    );
    assert_eq!(shape(&out.bytes, "fi e"), before);
    assert_eq!(
        outline(&source, u32::from(gid::F_I)),
        outline(&out.bytes, u32::from(gid::F_I))
    );
    assert!(!outline(&out.bytes, u32::from(gid::F_I)).is_empty());
}

#[test]
fn shaping_real_scripts_is_unchanged_and_no_shaped_glyph_lost_its_outline() {
    // Arabic joining and lam-alef, Devanagari conjuncts and reordering, Thai marks: GSUB has to reach their forms.
    for (font, text) in [
        ("Inter-Regular", "Hello, fine office! 0123"),
        ("NotoNaskhArabic", "مرحبا بالعالم لا إله"),
        ("NotoSansDevanagari", "नमस्ते क्ष त्र श्री हिन्दी"),
        ("NotoSansThai", "สวัสดีครับ น้ำ ที่นี่"),
        ("NotoSansHebrew", "שָׁלוֹם עולם"),
    ] {
        let data = bundled(font);
        let keep = chars(text);
        let out = subset_font(data, &keep).expect("subsets");
        assert!(out.bytes.len() < data.len(), "{font} shrank");
        let before = shape(data, text);
        assert!(!before.is_empty(), "{font}");
        assert_eq!(shape(&out.bytes, text), before, "{font} shapes the same");
        for (glyph, ..) in before {
            let glyph = u32::from(glyph);
            assert_eq!(
                outline(data, glyph),
                outline(&out.bytes, glyph),
                "{font} glyph {glyph}"
            );
        }
    }
}

#[test]
fn accented_composites_keep_their_components_through_nesting() {
    let source = tiny_font();
    let kept = glyph_closure(&source, &chars("\u{113}")).expect("closure");
    for glyph in [gid::NOTDEF, gid::E_MACRON, gid::E_ACUTE, gid::E, gid::ACUTE] {
        assert!(kept.contains(&glyph), "{glyph} in {kept:?}");
    }
    assert_eq!(kept.len(), 5, "{kept:?}");
    let out = subset_font(&source, &chars("\u{113}")).expect("subsets");
    assert_eq!(
        outline(&source, u32::from(gid::E_MACRON)),
        outline(&out.bytes, u32::from(gid::E_MACRON))
    );
    assert!(!outline(&out.bytes, u32::from(gid::E_MACRON)).is_empty());
    assert_eq!(outline(&out.bytes, u32::from(gid::Z)), "");

    // A real font: é in Inter draws identically.
    let out = subset_font(inter(), &chars("é")).expect("subsets");
    let glyph = glyph_of(inter(), 'é');
    assert!(!outline(&out.bytes, glyph).is_empty());
    assert_eq!(outline(inter(), glyph), outline(&out.bytes, glyph));
}

#[test]
fn the_gsub_closure_reaches_every_lookup_kind_and_only_when_its_inputs_are_kept() {
    let source = tiny_font();
    let of = |text: &str| glyph_closure(&source, &chars(text)).expect("closure");
    // Ligature: both inputs, or nothing.
    assert!(of("fi").contains(&gid::F_I));
    assert!(!of("f").contains(&gid::F_I));
    assert!(!of("i").contains(&gid::F_I));
    // Single, alternate, multiple, extension, reverse chaining.
    assert!(of("x").contains(&gid::X_ALT));
    assert!(of("a").contains(&gid::A_ALT));
    let m = of("m");
    assert!(m.contains(&gid::M_PART1) && m.contains(&gid::M_PART2));
    assert!(of("k").contains(&gid::K_EXTENSION));
    assert!(of("r").contains(&gid::R_REVERSE));
    // A lookup only a contextual lookup calls.
    assert!(of("n").contains(&gid::N_CONTEXT));
    // A chain whose lookups are listed backwards needs the fixpoint.
    let q = of("q");
    assert!(q.contains(&gid::Q_STEP1) && q.contains(&gid::Q_STEP2));
    // Nothing else: exactly `.notdef`, z.
    assert_eq!(of("z"), BTreeSet::from([gid::NOTDEF, gid::Z]));
}

/// No always-keep rule: only the locale text and what the shaper does to it.
const LOCALE_TEXT_ONLY: &str = "(keep_ascii: false, keep_punctuation: false, keep_pseudo: false)";

/// What `font` ships with `settings`, for a project of these locales.
fn planned(root: &Path, font: &[u8], settings: &str) -> Vec<u8> {
    let settings = FontImporterSettings::from_meta_string(Some(settings)).expect("settings");
    let plan = plan_font(&settings, &read_project_charsets(root), font).expect("plan");
    run_plan("font", font, &plan).bytes
}

/// A shaper composes `e` + U+0301 into the font's U+00E9 glyph, a character the text does not hold. Nothing but shaping
/// keeps it here: every always-keep rule is off.
#[test]
fn a_decomposed_run_keeps_the_composed_glyph_the_shaper_picks() {
    let text = "e\u{301}";
    let root = project(&[("en", &format!("greet = {text}\n"))]);
    let source = tiny_font();
    let before = shape(&source, text);
    assert_eq!(before.len(), 1, "the shaper composes: {before:?}");
    assert_eq!(before[0].0, gid::E_ACUTE, "{before:?}");
    assert!(
        !glyph_closure(&source, &chars(text))
            .expect("closure")
            .contains(&gid::E_ACUTE),
        "the characters alone do not reach it"
    );
    let shipped = planned(&root, &source, LOCALE_TEXT_ONLY);
    assert_eq!(shape(&shipped, text), before);
    assert_eq!(
        outline(&shipped, u32::from(gid::E_ACUTE)),
        outline(&source, u32::from(gid::E_ACUTE))
    );
    assert!(!outline(&shipped, u32::from(gid::E_ACUTE)).is_empty());
    let _ = std::fs::remove_dir_all(root);
}

/// In a right-to-left paragraph the shaper swaps a bracket or a math sign for its mirror when the font maps it. The
/// message splits at its placeable, so the piece ` ≥ 5` has no direction of its own and is only mirrored at run time.
#[test]
fn right_to_left_text_keeps_the_mirrored_characters_a_message_never_writes() {
    let root = project(&[("en", "level = ok\n"), ("he", "level = רמה { $x } ≥ 5 「ב」\n")]);
    let shipped = planned(&root, inter(), LOCALE_TEXT_ONLY);
    for c in ['≤', '≥'] {
        let glyph = glyph_of(inter(), c);
        assert!(!outline(inter(), glyph).is_empty(), "Inter draws {c}");
        assert_eq!(outline(inter(), glyph), outline(&shipped, glyph), "{c} survives");
    }
    // The same locale without the mirror in reach, as a control: this project's own text is what keeps it.
    let plain = project(&[("en", "level = ok ≥ 5\n")]);
    let shipped = planned(&plain, inter(), LOCALE_TEXT_ONLY);
    assert_eq!(
        outline(&shipped, glyph_of(inter(), '≤')),
        "",
        "no right-to-left text, no mirror"
    );
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(plain);
}

/// Thai splits U+0E33 before the font sees it, and Hebrew points compose; through the plan these reach the same glyphs.
#[test]
fn the_plan_keeps_every_glyph_real_scripts_shape_to() {
    for (font, text) in [
        ("NotoSansThai", "น้ำ ทำ สวัสดีครับ"),
        ("NotoSansHebrew", "שָׁלוֹם שׁ"),
        ("NotoSansDevanagari", "क़ ड़ नमस्ते"),
        ("NotoNaskhArabic", "لا إله (مرحبا)"),
    ] {
        let data = bundled(font);
        let root = project(&[("en", &format!("text = {text}\n"))]);
        let shipped = planned(&root, data, LOCALE_TEXT_ONLY);
        let before = shape(data, text);
        assert_eq!(shape(&shipped, text), before, "{font}");
        for (glyph, ..) in before {
            let glyph = u32::from(glyph);
            assert_eq!(
                outline(data, glyph),
                outline(&shipped, glyph),
                "{font} glyph {glyph}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn shaped_glyph_ids_skips_a_font_with_nothing_to_draw_and_refuses_junk_quietly() {
    let runs = BTreeSet::from(["\u{3042}".to_owned()]);
    assert_eq!(
        shaped_glyph_ids(&tiny_font(), &runs),
        BTreeSet::new(),
        "no mapped character"
    );
    assert_eq!(shaped_glyph_ids(b"nope", &runs), BTreeSet::new());
    let runs = BTreeSet::from(["fi".to_owned()]);
    assert_eq!(shaped_glyph_ids(&tiny_font(), &runs), BTreeSet::from([gid::F_I]));
    // What it found goes into the subset even with no character.
    let out = subset_font_with(&tiny_font(), &BTreeSet::new(), &BTreeSet::from([gid::Z])).expect("subsets");
    assert!(!outline(&out.bytes, u32::from(gid::Z)).is_empty());
    assert_eq!(outline(&out.bytes, u32::from(gid::X)), "");
}

#[test]
fn a_variable_font_keeps_kept_outlines_at_every_location_and_drops_other_variation_data() {
    let source = tiny_variable_font();
    let keep = chars("e\u{E9}");
    let out = subset_font(&source, &keep).expect("subsets");
    assert_checksums_verify(&out.bytes);
    let font = FontRef::new(&out.bytes).expect("parses");
    let location = font.axes().location([("wght", 900.0)]);
    let heavy = |data: &[u8], glyph: u16| outline_at(data, u32::from(glyph), LocationRef::from(&location));
    for glyph in [gid::E, gid::ACUTE, gid::E_ACUTE] {
        assert_eq!(heavy(&source, glyph), heavy(&out.bytes, glyph), "glyph {glyph}");
        assert_ne!(
            outline(&source, u32::from(glyph)),
            heavy(&source, glyph),
            "the axis moves glyph {glyph}"
        );
    }
    assert_eq!(
        heavy(&out.bytes, gid::Z),
        "",
        "a removed glyph draws nothing at any location"
    );

    // gvar: shared tuples carried over, zero-length data for a removed glyph, kept data byte for byte.
    let gvar = table(&out.bytes, b"gvar").expect("gvar");
    let source_gvar = table(&source, b"gvar").expect("gvar");
    assert!(gvar.len() < source_gvar.len());
    let shared_at = u32::from_be_bytes(gvar[8..12].try_into().expect("four")) as usize;
    assert_eq!(&gvar[shared_at..shared_at + 2], &[0x40, 0x00], "the shared tuple");
    let offset = |glyph: u16| {
        let at = 20 + usize::from(glyph) * 2;
        usize::from(u16::from_be_bytes([gvar[at], gvar[at + 1]])) * 2
    };
    assert_eq!(offset(gid::Z), offset(gid::Z + 1), "no data for a removed glyph");
    assert_ne!(offset(gid::E), offset(gid::E + 1), "data for a kept glyph");
    assert_eq!(table(&out.bytes, b"fvar"), table(&source, b"fvar"));
}

fn loc_format(data: &[u8]) -> i16 {
    let head = table(data, b"head").expect("head");
    i16::from_be_bytes([head[50], head[51]])
}

#[test]
fn a_small_subset_goes_to_short_loca_and_checksums_verify() {
    let source = tiny_font();
    assert_checksums_verify(&source);
    assert_eq!(loc_format(&source), 1, "the fixture comes in long");
    let out = subset_font(&source, &chars("fi")).expect("subsets");
    assert_checksums_verify(&out.bytes);
    assert_eq!(loc_format(&out.bytes), 0);
    assert_eq!(
        table(&out.bytes, b"loca").expect("loca").len(),
        (usize::from(gid::COUNT) + 1) * 2
    );
}

/// Inter with most of its glyphs: `glyf` stays past what short offsets reach, so `loca` must stay long.
#[test]
fn a_big_subset_stays_on_long_loca_and_draws_the_same() {
    let font = FontRef::new(inter()).expect("source");
    let keep: BTreeSet<char> = font
        .charmap()
        .mappings()
        .filter_map(|(code, _)| char::from_u32(code))
        .filter(|c| !matches!(*c, '\u{0400}'..='\u{04FF}'))
        .collect();
    let out = subset_font(inter(), &keep).expect("subsets");
    assert!(out.bytes.len() < inter().len(), "Cyrillic goes");
    assert!(
        table(&out.bytes, b"glyf").expect("glyf").len() > 0x1FFFE,
        "the fixture must pass the limit"
    );
    assert_eq!(loc_format(&out.bytes), 1);
    assert_checksums_verify(&out.bytes);
    let glyph = glyph_of(inter(), 'W');
    assert_eq!(outline(inter(), glyph), outline(&out.bytes, glyph));
    // Some Cyrillic stays: a kept glyph reaches it through GSUB or a composite. Most of it goes.
    let cyrillic: Vec<char> = ('\u{0400}'..='\u{04FF}')
        .filter(|c| font.charmap().map(*c).is_some())
        .collect();
    let emptied = cyrillic
        .iter()
        .filter(|c| outline(&out.bytes, glyph_of(inter(), **c)).is_empty())
        .count();
    assert!(emptied > cyrillic.len() / 2, "{emptied} of {}", cyrillic.len());
}

#[test]
fn a_dsig_table_is_dropped_because_its_signature_no_longer_holds() {
    let source = tiny_font_with([0, 1, 0, 0], &[(*b"DSIG", b"\0\0\0\x01\0\0\0\0")], None);
    let out = subset_font(&source, &chars("a")).expect("subsets");
    assert!(table(&source, b"DSIG").is_some());
    assert!(table(&out.bytes, b"DSIG").is_none());
    assert_checksums_verify(&out.bytes);
}

/// Bytes saved are asserted exactly: both sources and the writer are deterministic, so any change here is a change in
/// what ships.
#[test]
fn the_size_reduction_is_this_many_bytes() {
    let tiny = tiny_font();
    let out = subset_font(&tiny, &chars("fie")).expect("subsets");
    assert_eq!(
        (tiny.len(), out.bytes.len()),
        (TINY_BEFORE, TINY_AFTER),
        "the fixture's bytes before and after"
    );
    let out = subset_font(inter(), &chars(LATIN_TEXT)).expect("subsets");
    assert_eq!(
        (inter().len(), out.bytes.len(), out.glyphs_kept),
        (INTER_BEFORE, INTER_AFTER, INTER_KEPT),
        "Inter-Regular's bytes before, after, and glyphs kept"
    );
}

const TINY_BEFORE: usize = 1860;
const TINY_AFTER: usize = 1176;
const INTER_BEFORE: usize = 411_640;
const INTER_AFTER: usize = 213_076;
const INTER_KEPT: u32 = 247;

#[test]
fn the_output_is_deterministic() {
    let keep = chars(LATIN_TEXT);
    assert_eq!(
        subset_font(inter(), &keep).expect("first"),
        subset_font(inter(), &keep).expect("second")
    );
}

#[test]
fn cff_collection_color_and_bitmap_fonts_are_kept_whole_with_their_codes() {
    let keep = chars("a");
    let cff = tiny_font_with(*b"OTTO", &[(*b"CFF ", b"\x01\0\x04\x02\0\0\0\0")], Some(*b"glyf"));
    let out = subset_font(&cff, &keep).expect("kept");
    assert_eq!(out.kept_whole, Some(KeptWhole::Cff));
    assert_eq!(out.bytes, cff);
    assert_eq!(KeptWhole::Cff.code(), "font_subset_cff_kept");

    let collection = tiny_collection();
    let out = subset_font(&collection, &keep).expect("kept");
    assert_eq!(out.kept_whole, Some(KeptWhole::Collection));
    assert_eq!(out.bytes, collection);
    assert_eq!(out.glyphs_kept, u32::from(gid::COUNT));
    assert_eq!(KeptWhole::Collection.code(), "font_subset_collection_kept");

    // COLR layers are glyphs the closure would not reach, so emptying them would blank the color glyph.
    let color = tiny_font_with([0, 1, 0, 0], &[(*b"COLR", b"\0\0\0\0")], None);
    let out = subset_font(&color, &keep).expect("kept");
    assert_eq!(out.kept_whole, Some(KeptWhole::Color));
    assert_eq!(out.bytes, color);

    // The shaper applies `morx` instead of `GSUB`, and the closure does not read it.
    let aat = tiny_font_with([0, 1, 0, 0], &[(*b"morx", b"\0\x02\0\0\0\0\0\0")], None);
    let out = subset_font(&aat, &keep).expect("kept");
    assert_eq!(out.kept_whole, Some(KeptWhole::Aat));
    assert_eq!(out.bytes, aat);
    assert_eq!(KeptWhole::Aat.code(), "font_subset_aat_kept");

    let bitmap = subset_font(bundled("NotoColorEmoji"), &chars("😀")).expect("kept");
    assert_eq!(bitmap.kept_whole, Some(KeptWhole::NoOutlines));
    assert_eq!(bitmap.bytes, bundled("NotoColorEmoji"));
}

/// A font the reader finds no character map in would otherwise subset to `.notdef` alone, and the shaper may still
/// read it: format 6 is one it follows and `Charmap` does not.
#[test]
fn a_font_whose_cmap_the_reader_cannot_follow_is_kept_whole() {
    let mut cmap = Vec::new();
    cmap.extend_from_slice(&[0, 0, 0, 1, 0, 3, 0, 1, 0, 0, 0, 12]); // one (3,1) record at offset 12
    cmap.extend_from_slice(&[0, 6, 0, 12, 0, 0, 0, 0x61, 0, 1, 0, 2]); // format 6: U+0061 -> glyph 2
    let font = tiny_font_with([0, 1, 0, 0], &[(*b"cmap", &cmap)], Some(*b"cmap"));
    let parsed = FontRef::new(&font).expect("parses");
    assert!(
        !parsed.charmap().has_map(),
        "the fixture is the case: no format 4 or 12"
    );
    let out = subset_font(&font, &chars("a")).expect("kept");
    assert_eq!(out.kept_whole, Some(KeptWhole::Cmap));
    assert_eq!(out.bytes, font);
    assert_eq!(KeptWhole::Cmap.code(), "font_subset_cmap_kept");
}

/// One lookup reached 16,384 times, each with 16,384 subtable offsets to one coverage: under 100 KB claiming hundreds of
/// millions of subtable reads per pass. It must stop on the budget, not run.
#[test]
fn overlapping_gsub_offsets_stop_on_the_work_limit() {
    let offset = |value: usize| u16::try_from(value).expect("fits").to_be_bytes();
    let subtables = 16_384usize;
    let subtable_at = 6 + subtables * 2;
    let mut lookup = vec![0, 1, 0, 0];
    lookup.extend_from_slice(&offset(subtables));
    for _ in 0..subtables {
        lookup.extend_from_slice(&offset(subtable_at));
    }
    lookup.extend_from_slice(&[0, 1, 0, 6, 0, 0]); // single, format 1, coverage at 6, delta 0
    lookup.extend_from_slice(&[0, 1, 0, 1, 0, 2]); // coverage: glyph 2

    let lookups = 16_384usize;
    let mut gsub = vec![0, 1, 0, 0, 0, 10, 0, 12, 0, 14]; // version, script, feature and lookup list offsets
    gsub.extend_from_slice(&[0, 0, 0, 0]); // empty script and feature lists
    gsub.extend_from_slice(&offset(lookups));
    for _ in 0..lookups {
        gsub.extend_from_slice(&offset(2 + lookups * 2));
    }
    gsub.extend_from_slice(&lookup);
    let font = tiny_font_with([0, 1, 0, 0], &[(*b"GSUB", &gsub)], Some(*b"GSUB"));
    assert_eq!(
        subset_font(&font, &chars("f")),
        Err(FontSubsetError::TooComplex),
        "{} bytes of font",
        font.len()
    );
}

/// Thousands of directory records over the whole file: the writer must not copy each of them.
#[test]
fn overlapping_table_ranges_return_the_input_without_copying_them() {
    let mut font = tiny_font();
    let count = usize::from(u16::from_be_bytes([font[4], font[5]]));
    let last = 12 + 16 * count;
    // Stretch `post`, the last record, over the whole file: the sum of the tables now passes the file's size.
    let post = (0..count)
        .map(|i| 12 + 16 * i)
        .find(|at| &font[*at..*at + 4] == b"post")
        .expect("post");
    let len = u32::try_from(font.len()).expect("fits");
    font[post + 8..post + 12].fill(0);
    font[post + 12..post + 16].copy_from_slice(&len.to_be_bytes());
    assert!(last < font.len());
    let out = subset_font(&font, &chars("f")).expect("reads");
    assert_eq!(out.bytes, font, "not smaller, so the input");
    assert_eq!(
        out.glyphs_kept, out.glyphs_total,
        "and the report says all of them keep their outline"
    );
}

#[test]
fn corrupt_input_gives_a_typed_error_and_never_panics() {
    let keep = chars("fie\u{113}");
    assert_eq!(subset_font(b"", &keep), Err(FontSubsetError::NotAFont));
    assert_eq!(
        subset_font(b"nope nope nope", &keep),
        Err(FontSubsetError::NotAFont)
    );
    let source = tiny_variable_font();
    // Every cut: most refuse with a code, none panic.
    let mut refused = 0;
    for cut in 0..source.len() {
        if let Err(e) = subset_font(&source[..cut], &keep) {
            assert!(e.code().starts_with("font_subset_"), "{e}");
            refused += 1;
        }
    }
    assert!(refused > source.len() / 2, "{refused} of {}", source.len());
    // Every single-byte change, three ways. An accepted smaller result must read back with valid checksums.
    let mut mutated = source.clone();
    let mut rewritten = 0;
    for at in 0..source.len() {
        for value in [0u8, 0xFF, source[at] ^ 0x41] {
            mutated[at] = value;
            if let Ok(out) = subset_font(&mutated, &keep)
                && out.kept_whole.is_none()
                && out.bytes.len() < mutated.len()
            {
                rewritten += 1;
                assert!(FontRef::new(&out.bytes).is_ok(), "byte {at} to {value}");
                assert_checksums_verify(&out.bytes);
            }
        }
        mutated[at] = source[at];
    }
    assert!(rewritten > source.len(), "{rewritten} rewritten fonts checked");
}

/// A directory whose tags are out of order hides tables from `skrifa`'s binary search; the closure must not run blind.
#[test]
fn an_unsorted_table_directory_is_refused() {
    let mut font = tiny_font();
    let (first, second) = (12, 28);
    for i in 0..16 {
        font.swap(first + i, second + i);
    }
    let err = subset_font(&font, &chars("a")).expect_err("refused");
    assert_eq!(err.code(), "font_subset_malformed", "{err}");
}

fn write_font(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join("assets/fonts").join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(&path, bytes).expect("font");
    path
}

/// A project with these locales, and a stand-in for the rest of a project.
fn project(locales: &[(&str, &str)]) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir("font_subset");
    for (tag, text) in locales {
        let dir = root.join("assets/localization").join(tag);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("main.ftl"), text).expect("ftl");
    }
    std::fs::create_dir_all(root.join("assets")).expect("assets");
    root
}

fn cook(root: &Path) -> CookReport {
    let report = cook_project(CookOptions {
        project_root: root.to_path_buf(),
        platform: "linux".to_owned(),
        incremental: true,
    })
    .expect("cook");
    let real: Vec<_> = report
        .errors
        .iter()
        .filter(|(_, e)| !e.starts_with("unsupported source extension"))
        .collect();
    assert!(real.is_empty(), "{real:?}");
    report
}

fn set_settings(font: &Path, settings: &str) {
    let path = meta_path_for(font);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut meta = regia_asset::MetaSidecar::from_ron(text.as_bytes()).expect("meta");
    meta.settings = Some(settings.to_owned());
    std::fs::write(path, meta.to_ron_string().expect("ron")).expect("write");
}

const FR: &str = "greet = fi é\n";

#[test]
fn the_cook_subsets_to_the_locales_and_reports_bytes_before_and_after() {
    let root = project(&[("en", "greet = hi\n"), ("fr", FR)]);
    let source = tiny_font();
    write_font(&root, "ui.ttf", &source);
    let report = cook(&root);
    assert_eq!(report.font_subsets.len(), 1, "{:?}", report.font_subsets);
    let row = &report.font_subsets[0];
    assert_eq!(row.path, "assets/fonts/ui.ttf");
    assert_eq!(row.mode, "auto");
    assert_eq!(row.locales, ["en", "fr"]);
    assert_eq!(row.bytes_before, source.len() as u64);
    assert!(row.subset && row.bytes_after < row.bytes_before, "{row:?}");
    assert_eq!(row.code, None);
    assert_eq!(
        row.missing.get("en").map(String::as_str),
        Some("h"),
        "the fixture has no h"
    );

    // The report file holds the same row.
    let cache = cooked_cache_dir(&root, "linux");
    let text = std::fs::read_to_string(cache.join(FONT_SUBSETS_FILE)).expect("report");
    let file: FontSubsetReport = serde_json::from_str(&text).expect("json");
    assert_eq!(file.fonts, report.font_subsets);

    // A cooked row, but never a manifest row: paks do not ship fonts.
    let manifest = AssetManifest::read_from_cache(&cache)
        .expect("manifest")
        .unwrap_or_default();
    assert!(
        manifest.entries.iter().all(|e| !e.path.ends_with(".ttf")),
        "{:?}",
        manifest.entries
    );
    assert_eq!(report.cooked, 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_recook_of_an_unchanged_project_keeps_the_font_report() {
    let root = project(&[("en", "greet = hi\n"), ("fr", FR)]);
    write_font(&root, "ui.ttf", &tiny_font());
    let first = cook(&root);
    assert_eq!((first.cooked, first.font_subsets.len()), (1, 1));
    let report_path = cooked_cache_dir(&root, "linux").join(FONT_SUBSETS_FILE);
    // Back-dated, so a rewrite moves the mtime even inside one coarse timestamp tick.
    let then = std::time::SystemTime::now() - std::time::Duration::from_secs(1800);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&report_path)
        .expect("open")
        .set_modified(then)
        .expect("set mtime");
    let bytes = std::fs::read(&report_path).expect("report");
    let modified = std::fs::metadata(&report_path)
        .and_then(|m| m.modified())
        .expect("mtime");

    // The font is skipped: its row comes back from its cooked file, and the report is not written again.
    let again = cook(&root);
    assert_eq!((again.cooked, again.skipped), (0, 1), "nothing changed");
    assert_eq!(
        again.font_subsets, first.font_subsets,
        "the skipped font keeps its row"
    );
    assert_eq!(std::fs::read(&report_path).expect("report"), bytes);
    assert_eq!(
        std::fs::metadata(&report_path)
            .and_then(|m| m.modified())
            .expect("mtime"),
        modified,
        "an unchanged report is not rewritten"
    );

    // A deleted font leaves the report.
    std::fs::remove_file(root.join("assets/fonts/ui.ttf")).expect("remove font");
    let gone = cook(&root);
    assert!(gone.font_subsets.is_empty(), "{:?}", gone.font_subsets);
    let text = std::fs::read_to_string(&report_path).expect("report");
    let file: FontSubsetReport = serde_json::from_str(&text).expect("json");
    assert!(file.fonts.is_empty(), "{:?}", file.fonts);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_cook_fingerprint_holds_the_character_set() {
    // The source locale (en) must have files to ship anything (locales_to_ship); it uses the same
    // characters as fr, so only the fr edits below change the charset.
    let root = project(&[("en", "greet = fi\n"), ("fr", "greet = fi\n")]);
    write_font(&root, "ui.ttf", &tiny_font());
    assert_eq!(cook(&root).cooked, 1);
    let again = cook(&root);
    assert_eq!((again.cooked, again.skipped), (0, 1), "nothing changed");
    // A translation that reaches a new character recooks the font (one no always-keep rule holds).
    std::fs::write(
        root.join("assets/localization/fr/main.ftl"),
        "greet = fi \u{A66E}\n",
    )
    .expect("edit");
    let edited = cook(&root);
    assert_eq!((edited.cooked, edited.skipped), (1, 0), "the charset changed");
    // The same characters in another order do not.
    std::fs::write(
        root.join("assets/localization/fr/main.ftl"),
        "greet = if \u{A66E}\n",
    )
    .expect("edit");
    let same = cook(&root);
    assert_eq!(
        (same.cooked, same.skipped),
        (0, 1),
        "same characters, same clusters"
    );
    // Composing needs the mark after its base: the same characters, shaped to other glyphs, recook.
    let composed = "greet = if \u{A66E} e\u{301}\n";
    std::fs::write(root.join("assets/localization/fr/main.ftl"), composed).expect("edit");
    assert_eq!(cook(&root).cooked, 1, "a new mark");
    let apart = "greet = if \u{A66E} \u{301}e\n";
    std::fs::write(root.join("assets/localization/fr/main.ftl"), apart).expect("edit");
    let moved = cook(&root);
    assert_eq!(
        (moved.cooked, moved.skipped),
        (1, 0),
        "same characters, other glyphs"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn meta_settings_choose_off_locales_and_extra_chars() {
    let root = project(&[("en", "a = f\n"), ("fr", "a = é\n")]);
    let font = write_font(&root, "ui.ttf", &tiny_font());
    let keep_ascii_off = "keep_ascii: false, keep_punctuation: false, keep_pseudo: false";
    let kept = |root: &Path, settings: &str, font: &Path| {
        set_settings(font, settings);
        let report = cook(root);
        report.font_subsets[0].clone()
    };
    cook(&root);
    let whole = kept(&root, "(subset: off)", &font);
    assert_eq!((whole.mode.as_str(), whole.subset), ("off", false));
    assert_eq!(whole.bytes_after, whole.bytes_before);
    assert_eq!(whole.glyphs_kept, u32::from(gid::COUNT));

    let settings = format!("(subset: locales([\"en\"]), {keep_ascii_off})");
    let en = kept(&root, &settings, &font);
    assert_eq!(en.locales, ["en"]);
    // .notdef and f: no ligature, since i is not kept.
    assert_eq!(en.glyphs_kept, 2, "{en:?}");

    let settings = format!("(subset: locales([\"fr\"]), {keep_ascii_off})");
    let fr = kept(&root, &settings, &font);
    // .notdef, é and its two components.
    assert_eq!(fr.glyphs_kept, 4, "{fr:?}");

    let settings = format!("(subset: locales([\"en\"]), extra_chars: \"z\", {keep_ascii_off})");
    let extra = kept(&root, &settings, &font);
    assert_eq!(extra.glyphs_kept, 3, "{extra:?}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn bad_settings_and_an_unknown_locale_fail_the_cook_with_their_codes() {
    let root = project(&[("en", "a = f\n")]);
    let font = write_font(&root, "ui.ttf", &tiny_font());
    cook(&root);
    let run = |root: &Path| {
        cook_project(CookOptions {
            project_root: root.to_path_buf(),
            platform: "linux".to_owned(),
            incremental: true,
        })
        .expect("cook")
        .errors
    };
    set_settings(&font, "(subset: everything)");
    let errors = run(&root);
    assert!(errors.iter().any(|(_, e)| e.contains(BAD_SETTINGS)), "{errors:?}");
    set_settings(&font, "(subset: locales([\"xx\"]))");
    let errors = run(&root);
    assert!(
        errors.iter().any(|(_, e)| e.contains(UNKNOWN_LOCALE)),
        "{errors:?}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_file_that_is_not_a_font_warns_and_ships_as_it_is() {
    let root = project(&[("en", "a = f\n")]);
    write_font(&root, "body.ttf", b"not really a font");
    let report = cook(&root);
    assert!(
        report.warnings.iter().any(|w| w.contains(UNREADABLE_KEPT)),
        "{:?}",
        report.warnings
    );
    let row = &report.font_subsets[0];
    assert_eq!(row.code.as_deref(), Some(UNREADABLE_KEPT));
    assert!(!row.subset);
    assert_eq!(row.bytes_after, row.bytes_before);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_project_without_localization_keeps_its_fonts_whole_with_a_warning() {
    let root = project(&[]);
    write_font(&root, "ui.ttf", &tiny_font());
    let report = cook(&root);
    let row = &report.font_subsets[0];
    assert_eq!(row.code.as_deref(), Some(NO_LOCALES_KEPT));
    assert!(!row.subset);
    assert!(
        report.warnings.iter().any(|w| w.contains(NO_LOCALES_KEPT)),
        "{:?}",
        report.warnings
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Spec 36 § 4: the source locale always ships, so a project whose source locale (`en` by default) has
/// no files has no shipping set. The cook keeps its fonts whole and says why (3dd5ea263).
#[test]
fn a_project_without_its_source_locale_keeps_its_fonts_whole() {
    let root = project(&[("fr", "greet = fi\n")]);
    write_font(&root, "ui.ttf", &tiny_font());
    let report = cook(&root);
    let row = &report.font_subsets[0];
    assert_eq!(row.code.as_deref(), Some(NO_LOCALES_KEPT));
    assert!(!row.subset);
    assert_eq!(row.bytes_after, row.bytes_before);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains(NO_LOCALES_KEPT) && w.contains("the source locale `en` has no files")),
        "{:?}",
        report.warnings
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_collection_warns_through_the_cook() {
    let root = project(&[("en", "a = f\n")]);
    write_font(&root, "all.ttc", &tiny_collection());
    let report = cook(&root);
    assert_eq!(
        report.font_subsets[0].code.as_deref(),
        Some("font_subset_collection_kept")
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("font_subset_collection_kept")),
        "{:?}",
        report.warnings
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn staging_ships_the_cooks_subset_and_subsets_again_when_a_translation_moved() {
    // The source locale (en) must have files to ship anything (locales_to_ship); it uses the same
    // characters as fr, so only the fr edits below change the charset.
    let root = project(&[("en", "greet = fi\n"), ("fr", "greet = fi\n")]);
    let source = tiny_font();
    let font = write_font(&root, "ui.ttf", &source);
    cook(&root);
    set_settings(&font, "(keep_pseudo: false)");
    cook(&root);
    let staged_dir = root.join("staged");
    let staged_font = write_font_at(&staged_dir.join("fonts/ui.ttf"), &source);

    let staged = stage_fonts(&root, &staged_dir).expect("stage");
    assert_eq!((staged.fonts, staged.subset), (1, 1), "{staged:?}");
    assert_eq!(staged.bytes_before, source.len() as u64);
    let shipped = std::fs::read(&staged_font).expect("shipped");
    assert_eq!(staged.bytes_after, shipped.len() as u64);
    assert!(shipped.len() < source.len());
    assert_eq!(glyph_of(&shipped, 'f'), u32::from(gid::F));
    assert!(!outline(&shipped, u32::from(gid::F_I)).is_empty());
    assert_eq!(
        outline(&shipped, u32::from(gid::E_ACUTE)),
        "",
        "the translation does not use é"
    );

    // No cook since this edit: a package whose paks are fresh must still ship what the new text needs.
    std::fs::write(root.join("assets/localization/fr/main.ftl"), "greet = fi é\n").expect("edit");
    write_font_at(&staged_font, &source);
    stage_fonts(&root, &staged_dir).expect("stage");
    let shipped = std::fs::read(&staged_font).expect("shipped");
    assert!(
        !outline(&shipped, u32::from(gid::E_ACUTE)).is_empty(),
        "é reached the stale-cook package"
    );
    let _ = std::fs::remove_dir_all(root);
}

fn write_font_at(path: &Path, bytes: &[u8]) -> PathBuf {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, bytes).expect("font");
    path.to_path_buf()
}

#[test]
fn settings_parse_with_defaults_and_refuse_typos() {
    let parsed = FontImporterSettings::from_meta_string(None).expect("none");
    assert_eq!(parsed.subset, FontSubsetMode::Auto);
    assert!(parsed.keep_ascii && parsed.keep_punctuation && parsed.keep_pseudo);
    let parsed =
        FontImporterSettings::from_meta_string(Some("(subset: locales([\"fr\"]), extra_chars: \"©\")"))
            .expect("locales");
    assert_eq!(parsed.subset, FontSubsetMode::Locales(vec!["fr".to_owned()]));
    assert_eq!(parsed.extra_chars, "©");
    let err = FontImporterSettings::from_meta_string(Some("(sbuset: off)")).expect_err("typo");
    assert_eq!(err.code(), BAD_SETTINGS, "{err}");
}
