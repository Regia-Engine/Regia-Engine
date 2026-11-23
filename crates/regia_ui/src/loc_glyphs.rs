// SPDX-License-Identifier: MPL-2.0
//! Glyph coverage of translated text (spec 36, ADR-2550): the check for the classic "tofu boxes in one
//! locale" bug.
//!
//! A locale ships text; the fonts the game ships (the project's own fonts, then the bundled default set
//! of [`crate::text::embedded_font_data`]) must be able to draw every character of it. This module is
//! pure: [`FontCoverage`] reads a font's character map, [`FontChain`] is an ordered list of them,
//! [`text_runs`] pulls every piece of displayed text out of an `.ftl` file with its line (every plural
//! and select variant, attributes and terms included) and [`check_glyphs`] reports each message that
//! has a character no font of the chain covers, as a [`Finding`] with the stable code
//! [`GLYPH_MISSING`]. It also returns the per-locale character set, which font subsetting tools take.
//!
//! The chain models what ships on every platform. A desktop also falls back to system fonts at run
//! time; Android loads none, so a character only a desktop font has is a finding here on purpose.
//! Spaces, control characters and default-ignorable format characters (joiners, direction marks,
//! variation selectors) draw nothing and are not checked.

use std::collections::{BTreeMap, BTreeSet};

use cosmic_text::skrifa::{FontRef, MetadataProvider};
use fluent_syntax::ast;

use crate::loc_check::{Finding, Severity};
use crate::loc_pseudo::{PSEUDO_LOCALE, PseudoOptions, pseudo_localize};

/// The code of a finding for a message with a character no font covers.
pub const GLYPH_MISSING: &str = "loc_glyph_missing";

/// How many missing characters one finding lists before it says "and N more".
const LISTED: usize = 12;

/// Add the characters face `index` of the font file `data` maps to `chars`; `false` when there is no such face.
fn add_face_chars(data: &[u8], index: u32, chars: &mut BTreeSet<u32>) -> bool {
    let Ok(font) = FontRef::from_index(data, index) else {
        return false;
    };
    for (code, glyph) in font.charmap().mappings() {
        if glyph.to_u32() != 0 {
            chars.insert(code);
        }
    }
    true
}

/// The characters one font can draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontCoverage {
    /// A name for reports (a file name).
    pub name: String,
    chars: BTreeSet<u32>,
}

impl FontCoverage {
    /// A coverage from a character list (tests, and fonts read elsewhere).
    pub fn new(name: impl Into<String>, chars: impl IntoIterator<Item = char>) -> Self {
        Self {
            name: name.into(),
            chars: chars.into_iter().map(u32::from).collect(),
        }
    }

    /// Read the character map of the font file in `data` (every face of a collection). `None` when
    /// the bytes are not a font.
    #[must_use]
    pub fn from_font_data(name: impl Into<String>, data: &[u8]) -> Option<Self> {
        let mut chars = BTreeSet::new();
        let mut any = false;
        // A collection holds a handful of faces; the cap keeps a hostile index table cheap.
        for index in 0..64u32 {
            if !add_face_chars(data, index, &mut chars) {
                break;
            }
            any = true;
        }
        any.then(|| Self {
            name: name.into(),
            chars,
        })
    }

    /// Whether the font maps `c` to a glyph.
    #[must_use]
    pub fn covers(&self, c: char) -> bool {
        self.chars.contains(&u32::from(c))
    }

    /// How many characters the font maps.
    #[must_use]
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// Whether the font maps nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }
}

/// An ordered font fallback chain.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FontChain {
    /// The fonts, first choice first.
    pub fonts: Vec<FontCoverage>,
}

impl FontChain {
    /// The chain of the bundled default font set.
    #[must_use]
    pub fn embedded() -> Self {
        let fonts = crate::text::embedded_font_data()
            .into_iter()
            .filter_map(|(name, bytes)| FontCoverage::from_font_data(name, bytes))
            .collect();
        Self { fonts }
    }

    /// The chain of a running text system's font database: every face it holds, in load order, merged by
    /// family name (so Regular, Bold and Italic of one family count once). On a desktop this includes the
    /// system fonts the player's machine has.
    #[must_use]
    pub fn from_font_db(db: &cosmic_text::fontdb::Database) -> Self {
        let mut chain = Self::default();
        for face in db.faces() {
            let name = face
                .families
                .first()
                .map_or_else(|| face.post_script_name.clone(), |(family, _)| family.clone());
            let mut chars = BTreeSet::new();
            let id = face.id;
            let read = db.with_face_data(id, |data, index| add_face_chars(data, index, &mut chars));
            if read != Some(true) {
                continue;
            }
            match chain.fonts.iter_mut().find(|f| f.name == name) {
                Some(existing) => existing.chars.extend(chars),
                None => chain.fonts.push(FontCoverage { name, chars }),
            }
        }
        chain
    }

    /// Add `font` after the fonts already in the chain.
    pub fn push(&mut self, font: FontCoverage) {
        self.fonts.push(font);
    }

    /// `font` goes first, ahead of the fonts already in the chain.
    #[must_use]
    pub fn with_first(mut self, font: FontCoverage) -> Self {
        self.fonts.insert(0, font);
        self
    }

    /// Whether some font of the chain covers `c`.
    #[must_use]
    pub fn covers(&self, c: char) -> bool {
        self.fonts.iter().any(|f| f.covers(c))
    }

    /// The names of the fonts, in order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.fonts.iter().map(|f| f.name.clone()).collect()
    }
}

/// Whether `c` draws nothing, so no font needs a glyph for it: whitespace, controls and the
/// default-ignorable format characters.
#[must_use]
pub fn is_invisible(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{115F}'..='\u{1160}'
                | '\u{17B4}'..='\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{E0000}'..='\u{E0FFF}'
        )
}

/// One piece of displayed text of an `.ftl` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextRun {
    /// The message id (`key`, `key.attribute`, or `-term`).
    pub key: String,
    /// 1-based line of the text in the file.
    pub line: u32,
    /// The text, with string-literal escapes resolved.
    pub text: String,
}

/// The 1-based line of the text slice `value` of `source`; `None` when `value` is not a slice of it.
fn line_of_slice(source: &str, value: &str) -> Option<u32> {
    let start = source.as_ptr() as usize;
    let at = value.as_ptr() as usize;
    if at < start || at > start + source.len() {
        return None;
    }
    let offset = at - start;
    let newlines = source.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count();
    u32::try_from(newlines + 1).ok()
}

/// The text a Fluent string literal stands for: `\"`, `\\`, `\uXXXX` and `\UXXXXXX` resolved.
fn unescape_literal(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(e @ ('"' | '\\')) => out.push(e),
            Some(kind @ ('u' | 'U')) => {
                let digits = if kind == 'u' { 4 } else { 6 };
                let hex: String = chars.by_ref().take(digits).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => {
                        out.push('\\');
                        out.push(kind);
                        out.push_str(&hex);
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

struct Runs<'a> {
    source: &'a str,
    out: Vec<TextRun>,
    fallback_line: u32,
}

impl Runs<'_> {
    fn add(&mut self, key: &str, value: &str, text: String) {
        if text.is_empty() {
            return;
        }
        let line = line_of_slice(self.source, value).unwrap_or(self.fallback_line);
        self.out.push(TextRun {
            key: key.to_owned(),
            line,
            text,
        });
    }

    fn pattern(&mut self, key: &str, pattern: &ast::Pattern<&str>) {
        for element in &pattern.elements {
            match element {
                ast::PatternElement::TextElement { value } => self.add(key, value, (*value).to_owned()),
                ast::PatternElement::Placeable { expression } => self.expression(key, expression),
            }
        }
    }

    fn expression(&mut self, key: &str, expression: &ast::Expression<&str>) {
        match expression {
            ast::Expression::Inline(inline) => self.inline(key, inline),
            ast::Expression::Select { variants, .. } => {
                for variant in variants {
                    self.pattern(key, &variant.value);
                }
            }
        }
    }

    /// A placeable's own content: a string literal is displayed text; arguments are not.
    fn inline(&mut self, key: &str, inline: &ast::InlineExpression<&str>) {
        match inline {
            ast::InlineExpression::StringLiteral { value } => {
                self.add(key, value, unescape_literal(value));
            }
            ast::InlineExpression::Placeable { expression } => self.expression(key, expression),
            _ => {}
        }
    }
}

/// Every piece of displayed text of an `.ftl` file: text elements and string-literal placeables of
/// each message, attribute and term, with every plural and select variant, in file order. A file
/// that does not parse yields what the parser recovered.
#[must_use]
pub fn text_runs(source: &str) -> Vec<TextRun> {
    let resource = match fluent_syntax::parser::parse(source) {
        Ok(resource) => resource,
        Err((resource, _)) => resource,
    };
    let mut runs = Runs {
        source,
        out: Vec::new(),
        fallback_line: 0,
    };
    for entry in &resource.body {
        let (id, value, attributes) = match entry {
            ast::Entry::Message(m) => (m.id.name.to_owned(), m.value.as_ref(), &m.attributes),
            ast::Entry::Term(t) => (format!("-{}", t.id.name), Some(&t.value), &t.attributes),
            _ => continue,
        };
        runs.fallback_line = source
            .lines()
            .position(|l| l.starts_with(id.as_str()) && l[id.len()..].trim_start().starts_with('='))
            .map_or(0, |n| u32::try_from(n + 1).unwrap_or(u32::MAX));
        if let Some(value) = value {
            runs.pattern(&id, value);
        }
        for attribute in attributes {
            runs.pattern(&format!("{id}.{}", attribute.id.name), &attribute.value);
        }
    }
    runs.out
}

/// What [`check_glyphs`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlyphReport {
    /// One [`GLYPH_MISSING`] finding per piece of text with a character no font covers.
    pub findings: Vec<Finding>,
    /// Per locale (the pseudo-locale included): every visible character its messages use.
    pub charsets: BTreeMap<String, BTreeSet<char>>,
}

fn describe(c: char) -> String {
    format!("U+{:04X} '{c}'", u32::from(c))
}

fn scan_runs(locale: &str, file: &str, runs: &[TextRun], chain: &FontChain, report: &mut GlyphReport) {
    let charset = report.charsets.entry(locale.to_owned()).or_default();
    for run in runs {
        let mut missing: Vec<char> = Vec::new();
        for c in run.text.chars().filter(|c| !is_invisible(*c)) {
            charset.insert(c);
            if !chain.covers(c) && !missing.contains(&c) {
                missing.push(c);
            }
        }
        if missing.is_empty() {
            continue;
        }
        let shown: Vec<String> = missing.iter().take(LISTED).map(|c| describe(*c)).collect();
        let more = missing.len().saturating_sub(LISTED);
        report.findings.push(Finding {
            code: GLYPH_MISSING,
            severity: Severity::Error,
            locale: locale.to_owned(),
            key: Some(run.key.clone()),
            file: Some(file.to_owned()),
            line: Some(run.line),
            message: format!(
                "`{}` has no glyph in the font chain for {}{}; the player sees boxes",
                run.key,
                shown.join(", "),
                if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                }
            ),
        });
    }
}

/// Check every locale's text against `chain`. `locales` is `(tag, [(path, ftl text)])`. With `pseudo`,
/// the source locale's text is also pseudo-localized in memory and checked as [`PSEUDO_LOCALE`] (unless
/// a locale of that name is among `locales`, which is then checked as written).
#[must_use]
pub fn check_glyphs(
    locales: &[(String, Vec<(String, String)>)],
    source: &str,
    chain: &FontChain,
    pseudo: bool,
) -> GlyphReport {
    let _span = tracing::debug_span!(
        "loc_check_glyphs",
        locales = locales.len(),
        fonts = chain.fonts.len()
    )
    .entered();
    let mut report = GlyphReport::default();
    for (tag, files) in locales {
        report.charsets.entry(tag.clone()).or_default();
        for (path, text) in files {
            scan_runs(tag, path, &text_runs(text), chain, &mut report);
        }
    }
    if pseudo
        && !locales.iter().any(|(tag, _)| tag == PSEUDO_LOCALE)
        && let Some((_, files)) = locales.iter().find(|(tag, _)| tag == source)
    {
        let options = PseudoOptions::default();
        report.charsets.entry(PSEUDO_LOCALE.to_owned()).or_default();
        for (path, text) in files {
            let runs: Vec<TextRun> = text_runs(text)
                .into_iter()
                .map(|mut r| {
                    r.text = pseudo_localize(&r.text, &options);
                    r
                })
                .collect();
            scan_runs(PSEUDO_LOCALE, path, &runs, chain, &mut report);
        }
    }
    report
}

/// The text of a character-set file for font subsetting tools: the characters in code point order on
/// one line, then a newline (the form `pyftsubset --text-file` reads).
#[must_use]
pub fn charset_text(chars: &BTreeSet<char>) -> String {
    let mut out: String = chars.iter().collect();
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latin() -> FontCoverage {
        FontCoverage::new(
            "test-latin",
            ('a'..='z').chain('A'..='Z').chain("0123456789.,!:-«»~".chars()),
        )
    }

    fn chain() -> FontChain {
        FontChain { fonts: vec![latin()] }
    }

    fn locales(fr: &str) -> Vec<(String, Vec<(String, String)>)> {
        vec![
            (
                "en".to_owned(),
                vec![("l/en/main.ftl".to_owned(), "greet = Hello\n".to_owned())],
            ),
            ("fr".to_owned(), vec![("l/fr/main.ftl".to_owned(), fr.to_owned())]),
        ]
    }

    #[test]
    fn a_missing_character_is_reported_with_message_and_line() {
        let fr = "greet = Bonjour\n\nmenu-quit = Quitter é\nlast = ok\n";
        let report = check_glyphs(&locales(fr), "en", &chain(), false);
        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        let f = &report.findings[0];
        assert_eq!(f.code, GLYPH_MISSING);
        assert_eq!(f.locale, "fr");
        assert_eq!(f.key.as_deref(), Some("menu-quit"));
        assert_eq!(f.file.as_deref(), Some("l/fr/main.ftl"));
        assert_eq!(f.line, Some(3));
        assert!(f.message.contains("U+00E9"), "{}", f.message);
    }

    #[test]
    fn every_plural_and_select_variant_is_scanned_with_its_own_line() {
        let fr = "ammo = { $count ->\n    [one] { $count } cartouche\u{e9}\n   *[other] { $count } \u{3b2}\n}\nattr = Hi\n    .title = ok\u{f1}\n";
        let report = check_glyphs(&locales(fr), "en", &chain(), false);
        let lines: Vec<(Option<&str>, Option<u32>)> = report
            .findings
            .iter()
            .map(|f| (f.key.as_deref(), f.line))
            .collect();
        assert_eq!(
            lines,
            [
                (Some("ammo"), Some(2)),
                (Some("ammo"), Some(3)),
                (Some("attr.title"), Some(6))
            ],
            "{:?}",
            report.findings
        );
    }

    #[test]
    fn string_literals_and_terms_count_but_function_arguments_do_not() {
        let fr =
            "-brand = Reg\u{ef}a\nuses = { \"caf\\u00E9\" } { NUMBER($n, style: \"\u{3b2}\") } { -brand }\n";
        let report = check_glyphs(&locales(fr), "en", &chain(), false);
        let keys: Vec<&str> = report.findings.iter().filter_map(|f| f.key.as_deref()).collect();
        assert_eq!(keys, ["-brand", "uses"], "{:?}", report.findings);
    }

    #[test]
    fn invisible_characters_need_no_glyph_and_charsets_collect_the_visible_ones() {
        let fr = "greet = ab\u{200B}c\u{FE0F} d\n";
        let report = check_glyphs(&locales(fr), "en", &chain(), false);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        let set = &report.charsets["fr"];
        assert_eq!(set.iter().collect::<String>(), "abcd");
        assert_eq!(charset_text(set), "abcd\n");
    }

    #[test]
    fn a_later_font_in_the_chain_covers_what_the_first_lacks() {
        let fr = "greet = caf\u{e9}\n";
        let mut chain = chain();
        assert_eq!(check_glyphs(&locales(fr), "en", &chain, false).findings.len(), 1);
        chain.push(FontCoverage::new("accents", ['\u{e9}']));
        assert!(
            check_glyphs(&locales(fr), "en", &chain, false)
                .findings
                .is_empty()
        );
        assert_eq!(chain.names(), ["test-latin", "accents"]);
    }

    #[test]
    fn the_pseudo_locale_is_checked_too() {
        let report = check_glyphs(&locales("greet = Bonjour\n"), "en", &chain(), true);
        assert!(report.charsets.contains_key(PSEUDO_LOCALE));
        // The test font has no accented letters, which the pseudo-locale uses.
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.locale == PSEUDO_LOCALE && f.key.as_deref() == Some("greet")),
            "{:?}",
            report.findings
        );
        let off = check_glyphs(&locales("greet = Bonjour\n"), "en", &chain(), false);
        assert!(!off.charsets.contains_key(PSEUDO_LOCALE));
    }

    #[test]
    fn the_bundled_fonts_cover_latin_cyrillic_cjk_and_flag_unmapped_characters() {
        let chain = FontChain::embedded();
        assert!(chain.fonts.len() >= 9, "{:?}", chain.names());
        for c in ['a', 'é', 'ж', '你', '日', 'あ', 'ก', 'ש', 'ع', 'ह', '한'] {
            assert!(chain.covers(c), "{c} is not covered by {:?}", chain.names());
        }
        assert!(!chain.covers('\u{10FFFD}'));
        assert!(FontCoverage::from_font_data("junk", b"not a font").is_none());
    }

    #[test]
    fn escapes_resolve_and_bad_ones_stay_literal() {
        assert_eq!(
            unescape_literal("a\\\"b\\\\c\\u00E9\\U01F600"),
            "a\"b\\c\u{e9}\u{1F600}"
        );
        assert_eq!(unescape_literal("\\x \\"), "\\x \\");
    }
}
