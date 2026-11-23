// SPDX-License-Identifier: MPL-2.0
//! Localization pipeline, debug locales (ADR-1570): pseudo-localization and key display.
//!
//! Two locales that exist only to test a UI:
//!
//! - **Pseudo** ([`PSEUDO_LOCALE`]): every ascii letter becomes an accented look-alike and the
//!   text grows by a percentage, so clipped glyphs, hard-coded widths and truncated labels show up
//!   before a real translation exists. Fluent syntax is left alone: ids, `{ placeables }`, select
//!   variant keys and attribute names are not touched, so a pseudo table still loads and formats.
//! - **Key display** ([`KEY_LOCALE`]): every message shows its own key (and its `$variables`), so a
//!   screenshot tells you which key a label uses.
//!
//! The text functions work on any string ([`pseudo_localize`]); the table functions
//! ([`pseudo_table`], [`key_display_table`]) build whole [`CompiledTable`]s from a
//! [`CompileOutput`], ready for [`CompileOutput::to_localization`].

use std::collections::BTreeSet;

use crate::loc_compile::{CompileOutput, CompiledTable, TableEntry};
use crate::loc_manifest::SourceLoc;

/// The tag of the pseudo-localized locale. The region `XA` is user-assigned, so it never clashes
/// with a real translation, and the language `en` gives the locale English plural rules (a plural
/// select in a pseudo table picks its branches the way English does).
pub const PSEUDO_LOCALE: &str = "en-XA";

/// The tag of the key-display locale (user-assigned region `XD`, like [`PSEUDO_LOCALE`]).
pub const KEY_LOCALE: &str = "en-XD";

/// Opening marker of key display.
pub const KEY_OPEN: char = '\u{27EA}';

/// Closing marker of key display.
pub const KEY_CLOSE: char = '\u{27EB}';

/// Opening bracket added by pseudo-localization (shows where a string starts and ends).
pub const PSEUDO_OPEN: char = '\u{00AB}';

/// Closing bracket added by pseudo-localization.
pub const PSEUDO_CLOSE: char = '\u{00BB}';

/// Filler used to grow text.
const PAD: char = '~';

/// Largest expansion ratio accepted (the text can at most triple).
pub const MAX_EXPANSION: f32 = 2.0;

/// Settings for pseudo-localization.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PseudoOptions {
    /// Extra length as a ratio of the letters in each text run (0.3 is 30 % longer), clamped to
    /// `0.0..=`[`MAX_EXPANSION`]; a non-finite value counts as 0.
    pub expansion: f32,
    /// Replace ascii letters with accented look-alikes.
    pub accents: bool,
    /// Wrap the whole text in [`PSEUDO_OPEN`] / [`PSEUDO_CLOSE`].
    pub brackets: bool,
}

impl Default for PseudoOptions {
    fn default() -> Self {
        Self {
            expansion: 0.3,
            accents: true,
            brackets: true,
        }
    }
}

/// The accented look-alike of an ascii letter; every other character is returned as is.
pub fn accent_char(c: char) -> char {
    match c {
        'a' => 'á',
        'b' => 'ƀ',
        'c' => 'ç',
        'd' => 'ð',
        'e' => 'é',
        'f' => 'ƒ',
        'g' => 'ĝ',
        'h' => 'ĥ',
        'i' => 'í',
        'j' => 'ĵ',
        'k' => 'ķ',
        'l' => 'ļ',
        'm' => 'ɱ',
        'n' => 'ñ',
        'o' => 'ó',
        'p' => 'þ',
        'q' => 'ǫ',
        'r' => 'ŕ',
        's' => 'š',
        't' => 'ţ',
        'u' => 'ú',
        'v' => 'ṽ',
        'w' => 'ŵ',
        'x' => 'ẋ',
        'y' => 'ý',
        'z' => 'ž',
        'A' => 'Á',
        'B' => 'Ɓ',
        'C' => 'Ç',
        'D' => 'Ð',
        'E' => 'É',
        'F' => 'Ƒ',
        'G' => 'Ĝ',
        'H' => 'Ĥ',
        'I' => 'Í',
        'J' => 'Ĵ',
        'K' => 'Ķ',
        'L' => 'Ļ',
        'M' => 'Ṁ',
        'N' => 'Ñ',
        'O' => 'Ó',
        'P' => 'Þ',
        'Q' => 'Ǫ',
        'R' => 'Ŕ',
        'S' => 'Š',
        'T' => 'Ţ',
        'U' => 'Ú',
        'V' => 'Ṽ',
        'W' => 'Ŵ',
        'X' => 'Ẋ',
        'Y' => 'Ý',
        'Z' => 'Ž',
        other => other,
    }
}

/// A run of text that is either left alone or transformed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Syntax(String),
    Text(String),
}

/// Net `{` minus `}` in `line`, ignoring braces inside `"..."` string literals.
fn brace_delta(line: &str) -> i32 {
    let mut delta = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' if delta > 0 => in_string = true,
            '{' => delta += 1,
            '}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

/// Splits `text` into text runs and balanced `{ ... }` placeables. An unclosed placeable keeps
/// the rest of the text as syntax.
fn split_placeables(text: &str, out: &mut Vec<Piece>) {
    let mut run = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '{' {
            run.push(c);
            continue;
        }
        if !run.is_empty() {
            out.push(Piece::Text(std::mem::take(&mut run)));
        }
        let mut placeable = String::from('{');
        let mut depth = 1;
        let mut in_string = false;
        let mut escaped = false;
        for ch in chars.by_ref() {
            placeable.push(ch);
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push(Piece::Syntax(placeable));
    }
    if !run.is_empty() {
        out.push(Piece::Text(run));
    }
}

/// Text that follows `prefix_end` bytes of syntax on a line: pushes the syntax, then the rest,
/// treating a rest with unbalanced braces (a select head or tail) as syntax.
fn push_line_tail(line: &str, prefix_end: usize, out: &mut Vec<Piece>) {
    let (prefix, rest) = line.split_at(prefix_end);
    if !prefix.is_empty() {
        out.push(Piece::Syntax(prefix.to_owned()));
    }
    if brace_delta(rest) != 0 {
        out.push(Piece::Syntax(rest.to_owned()));
    } else {
        split_placeables(rest, out);
    }
}

/// The byte index of the first character at or after `from` that is not a space or tab.
fn skip_spaces(line: &str, from: usize) -> usize {
    from + line[from..].len() - line[from..].trim_start_matches([' ', '\t']).len()
}

/// Splits one line of a Fluent block.
fn split_ftl_line(line: &str, first: bool, out: &mut Vec<Piece>) {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    if first || trimmed.starts_with('.') {
        // A message head or an attribute: keep everything up to the `=` and its spaces.
        if let Some(eq) = line.find('=') {
            push_line_tail(line, skip_spaces(line, eq + 1), out);
        } else {
            out.push(Piece::Syntax(line.to_owned()));
        }
    } else if trimmed.starts_with('[') || trimmed.starts_with("*[") {
        if let Some(close) = line.find(']') {
            push_line_tail(line, skip_spaces(line, close + 1), out);
        } else {
            out.push(Piece::Syntax(line.to_owned()));
        }
    } else if trimmed.starts_with('}') {
        out.push(Piece::Syntax(line.to_owned()));
    } else {
        push_line_tail(line, indent, out);
    }
}

/// Applies accents and expansion to the text pieces, and brackets to the first and last.
fn transform(pieces: Vec<Piece>, options: &PseudoOptions) -> String {
    let expansion = if options.expansion.is_finite() {
        options.expansion.clamp(0.0, MAX_EXPANSION)
    } else {
        0.0
    };
    // Text runs that carry letters; whitespace-only runs stay as they are.
    let first = pieces
        .iter()
        .position(|p| matches!(p, Piece::Text(t) if !t.trim().is_empty()));
    let last = pieces
        .iter()
        .rposition(|p| matches!(p, Piece::Text(t) if !t.trim().is_empty()));
    let mut out = String::new();
    for (index, piece) in pieces.into_iter().enumerate() {
        match piece {
            Piece::Syntax(s) => out.push_str(&s),
            Piece::Text(t) if t.trim().is_empty() => out.push_str(&t),
            Piece::Text(t) => {
                let body = t.trim_end();
                let tail = &t[body.len()..];
                if options.brackets && Some(index) == first {
                    out.push(PSEUDO_OPEN);
                }
                let mut letters = 0usize;
                for c in body.chars() {
                    if c.is_alphabetic() {
                        letters += 1;
                    }
                    out.push(if options.accents { accent_char(c) } else { c });
                }
                // The epsilon keeps 10 letters at 0.3 (f32 rounding) from growing by 4.
                let extra = (letters as f64 * f64::from(expansion) - 1e-4).ceil() as usize;
                out.extend(std::iter::repeat_n(PAD, extra));
                if options.brackets && Some(index) == last {
                    out.push(PSEUDO_CLOSE);
                }
                out.push_str(tail);
            }
        }
    }
    out
}

/// Pseudo-localizes a plain string. `{ ... }` placeables are copied unchanged (nested braces and
/// quoted strings inside them are respected). Empty or whitespace-only text is returned as is.
pub fn pseudo_localize(text: &str, options: &PseudoOptions) -> String {
    let mut pieces = Vec::new();
    split_placeables(text, &mut pieces);
    transform(pieces, options)
}

/// Pseudo-localizes one Fluent message block (an id line plus continuation lines, as stored in
/// [`TableEntry::source`]). The id, attribute names, select variant keys, selectors and
/// placeables are kept, so the result is still valid Fluent. Brackets wrap the first and last
/// text run of the whole message.
pub fn pseudo_localize_ftl(block: &str, options: &PseudoOptions) -> String {
    let mut pieces = Vec::new();
    for (index, line) in block.split('\n').enumerate() {
        if index > 0 {
            pieces.push(Piece::Syntax("\n".to_owned()));
        }
        split_ftl_line(line, index == 0, &mut pieces);
    }
    transform(pieces, options)
}

/// What key display shows for `key` with `args`: `⟪key name=value⟫`, arguments in the order given.
pub fn key_display(key: &str, args: &[(String, String)]) -> String {
    let mut out = String::new();
    out.push(KEY_OPEN);
    out.push_str(key);
    for (name, value) in args {
        out.push(' ');
        out.push_str(name);
        out.push('=');
        out.push_str(value);
    }
    out.push(KEY_CLOSE);
    out
}

fn derived_entry(source: String, key_entry: &TableEntry, locale: &str) -> TableEntry {
    TableEntry {
        source,
        origin_locale: locale.to_owned(),
        at: key_entry.at.clone(),
        variables: key_entry.variables.clone(),
        // An attribute-only message stays attribute-only after pseudo-localization.
        has_value: key_entry.has_value,
    }
}

/// The pseudo-localized table of `output`: every entry of `base_locale`'s table (so the same keys
/// the real locale shows) run through [`pseudo_localize_ftl`]. `None` when `base_locale` has no
/// table. Attribute-only messages are copied unchanged apart from their attribute text.
pub fn pseudo_table(
    output: &CompileOutput,
    base_locale: &str,
    options: &PseudoOptions,
) -> Option<CompiledTable> {
    let base = output.tables.get(base_locale)?;
    let _span = tracing::debug_span!("loc_pseudo_table", entries = base.entries.len()).entered();
    let entries = base
        .entries
        .iter()
        .map(|(key, entry)| {
            let source = pseudo_localize_ftl(&entry.source, options);
            (key.clone(), derived_entry(source, entry, PSEUDO_LOCALE))
        })
        .collect();
    Some(CompiledTable {
        locale: PSEUDO_LOCALE.to_owned(),
        chain: vec![PSEUDO_LOCALE.to_owned()],
        entries,
    })
}

/// The key-display table of `output`: every key that exists (defined anywhere or referenced,
/// even if undefined) shows `⟪key⟫`, with each `$variable` of the default locale's text appended
/// as `name={ $name }` so arguments are visible too.
pub fn key_display_table(output: &CompileOutput) -> CompiledTable {
    let default_table = output.tables.get(&output.default_locale);
    let mut entries = std::collections::BTreeMap::new();
    for key in &output.universe {
        let variables: BTreeSet<String> = default_table
            .and_then(|t| t.get(key))
            .map(|e| e.variables.clone())
            .unwrap_or_default();
        let mut source = String::new();
        source.push_str(key);
        source.push_str(" = ");
        source.push(KEY_OPEN);
        source.push_str(key);
        for var in &variables {
            source.push_str(&format!(" {var}={{ ${var} }}"));
        }
        source.push(KEY_CLOSE);
        entries.insert(
            key.clone(),
            TableEntry {
                source,
                origin_locale: KEY_LOCALE.to_owned(),
                at: SourceLoc {
                    path: String::new(),
                    line: 0,
                    column: 0,
                },
                variables,
                has_value: true,
            },
        );
    }
    CompiledTable {
        locale: KEY_LOCALE.to_owned(),
        chain: vec![KEY_LOCALE.to_owned()],
        entries,
    }
}

/// Both debug locales as `(locale, ftl)` pairs for [`CompileOutput::to_localization`]: pseudo
/// (built from `base_locale`) and key display.
pub fn debug_locales(
    output: &CompileOutput,
    base_locale: &str,
    options: &PseudoOptions,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(table) = pseudo_table(output, base_locale, options) {
        out.push((table.locale.clone(), table.to_ftl()));
    }
    let keys = key_display_table(output);
    out.push((keys.locale.clone(), keys.to_ftl()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loc_compile::{CompileOptions, compile};
    use crate::loc_manifest::{GatherSource, gather};

    fn plain() -> PseudoOptions {
        PseudoOptions {
            expansion: 0.0,
            accents: false,
            brackets: false,
        }
    }

    #[test]
    fn every_ascii_letter_is_accented_and_others_are_kept() {
        for c in ('a'..='z').chain('A'..='Z') {
            let a = accent_char(c);
            assert_ne!(a, c, "{c}");
            assert!(!a.is_ascii(), "{c}");
            assert!(a.is_alphabetic(), "{c} -> {a}");
        }
        for c in ['1', ' ', '!', 'é', '日', '\n'] {
            assert_eq!(accent_char(c), c);
        }
    }

    #[test]
    fn accent_map_is_injective() {
        let all: BTreeSet<char> = ('a'..='z').chain('A'..='Z').map(accent_char).collect();
        assert_eq!(all.len(), 52);
    }

    #[test]
    fn accents_only() {
        let o = PseudoOptions {
            accents: true,
            ..plain()
        };
        assert_eq!(pseudo_localize("Hello 42!", &o), "Ĥéļļó 42!");
    }

    #[test]
    fn expansion_adds_a_ratio_of_the_letters() {
        let o = PseudoOptions {
            expansion: 0.3,
            ..plain()
        };
        // 10 letters * 0.3 = 3 pad characters.
        assert_eq!(pseudo_localize("Hello world", &o), "Hello world~~~");
        let o = PseudoOptions {
            expansion: 1.0,
            ..plain()
        };
        assert_eq!(pseudo_localize("ab cd", &o), "ab cd~~~~");
    }

    #[test]
    fn expansion_goes_before_trailing_whitespace_and_is_clamped() {
        let o = PseudoOptions {
            expansion: 1.0,
            ..plain()
        };
        assert_eq!(pseudo_localize("ab ", &o), "ab~~ ");
        let big = PseudoOptions {
            expansion: 50.0,
            ..plain()
        };
        assert_eq!(pseudo_localize("ab", &big), "ab~~~~", "clamped to 2.0");
        for bad in [f32::NAN, f32::INFINITY, -1.0] {
            let o = PseudoOptions {
                expansion: bad,
                ..plain()
            };
            let got = pseudo_localize("ab", &o);
            // INFINITY is non-finite, NaN is non-finite, negative clamps to 0: all unchanged.
            assert_eq!(got, "ab", "{bad}");
        }
    }

    #[test]
    fn brackets_wrap_the_text() {
        let o = PseudoOptions {
            brackets: true,
            ..plain()
        };
        assert_eq!(pseudo_localize("Hi", &o), "«Hi»");
        assert_eq!(pseudo_localize("", &o), "");
        assert_eq!(pseudo_localize("   ", &o), "   ");
    }

    #[test]
    fn placeables_are_untouched() {
        let o = PseudoOptions::default();
        let out = pseudo_localize("Hello, { $name }! { NUMBER($n, style: \"percent\") } done", &o);
        assert!(out.contains("{ $name }"));
        assert!(out.contains("{ NUMBER($n, style: \"percent\") }"));
        assert!(out.starts_with('«') && out.ends_with('»'));
        assert!(!out.contains("Hello"));
    }

    #[test]
    fn nested_braces_and_braces_in_strings_are_respected() {
        let o = PseudoOptions {
            accents: true,
            ..plain()
        };
        assert_eq!(pseudo_localize("a { \"}\" } b", &o), "á { \"}\" } ƀ");
        assert_eq!(pseudo_localize("a { { \"x\" } } b", &o), "á { { \"x\" } } ƀ");
    }

    #[test]
    fn unclosed_placeable_is_left_alone() {
        let o = PseudoOptions {
            accents: true,
            ..plain()
        };
        assert_eq!(pseudo_localize("a { b", &o), "á { b");
    }

    #[test]
    fn pseudo_is_deterministic() {
        let o = PseudoOptions::default();
        assert_eq!(pseudo_localize("Same text", &o), pseudo_localize("Same text", &o));
    }

    #[test]
    fn ftl_block_keeps_ids_variants_and_attributes() {
        let block = "items = { $n ->\n    [one] one item\n   *[other] { $n } items\n    }\n    .title = Tip for { $n }";
        let o = PseudoOptions {
            accents: true,
            ..plain()
        };
        let out = pseudo_localize_ftl(block, &o);
        assert_eq!(
            out,
            "items = { $n ->\n    [one] óñé íţéɱ\n   *[other] { $n } íţéɱš\n    }\n    .title = Ţíþ ƒóŕ { $n }"
        );
    }

    #[test]
    fn ftl_block_brackets_wrap_first_and_last_text() {
        let o = PseudoOptions {
            brackets: true,
            ..plain()
        };
        assert_eq!(
            pseudo_localize_ftl("a = Hello { $n } there", &o),
            "a = «Hello { $n } there»"
        );
        assert_eq!(
            pseudo_localize_ftl("long =\n    line one\n    line two", &o),
            "long =\n    «line one\n    line two»"
        );
    }

    #[test]
    fn ftl_block_without_value_text_is_unchanged() {
        let o = PseudoOptions::default();
        assert_eq!(pseudo_localize_ftl("a = { $n }", &o), "a = { $n }");
        assert_eq!(pseudo_localize_ftl("a =", &o), "a =");
    }

    #[test]
    fn key_display_formats_args() {
        assert_eq!(key_display("menu-start", &[]), "⟪menu-start⟫");
        assert_eq!(
            key_display(
                "hp",
                &[
                    ("n".to_owned(), "3".to_owned()),
                    ("who".to_owned(), "Zoé".to_owned())
                ]
            ),
            "⟪hp n=3 who=Zoé⟫"
        );
    }

    fn output() -> CompileOutput {
        let g = gather(&[
            GatherSource::ftl(
                "en.ftl",
                "en",
                "start = Start\nitems = { $n ->\n    [one] one item\n   *[other] { $n } items\n    }\nhello = Hi, { $name }!\n",
            ),
            GatherSource::ftl("fr.ftl", "fr", "start = Démarrer\n"),
            GatherSource::ui_asset("u.ron", "(a: Loc(\"ghost\"))"),
        ]);
        compile(&g, &CompileOptions::new("en")).expect("compile")
    }

    #[test]
    fn pseudo_table_matches_the_base_locale_keys() {
        let out = output();
        let table = pseudo_table(&out, "fr", &PseudoOptions::default()).expect("table");
        assert_eq!(table.locale, PSEUDO_LOCALE);
        let keys: Vec<&str> = table.entries.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["hello", "items", "start"],
            "fr falls back to en for the rest"
        );
        assert!(pseudo_table(&out, "de", &PseudoOptions::default()).is_none());
    }

    #[test]
    fn pseudo_entries_keep_whether_the_message_has_a_value() {
        // The pseudo table claimed a value for attribute-only messages (ADR-1660).
        let g = gather(&[GatherSource::ftl(
            "en.ftl",
            "en",
            "start = Start\nlabel =\n    .title = Tip\n",
        )]);
        let out = compile(&g, &CompileOptions::new("en")).expect("compile");
        let base = &out.tables["en"].entries;
        assert!(base["start"].has_value && !base["label"].has_value);
        let table = pseudo_table(&out, "en", &PseudoOptions::default()).expect("table");
        assert!(table.entries["start"].has_value);
        assert!(!table.entries["label"].has_value);
    }

    #[test]
    fn debug_locales_load_and_format() {
        let out = output();
        let extra = debug_locales(&out, "en", &PseudoOptions::default());
        assert_eq!(extra.len(), 2);
        let mut loc = out.to_localization(extra).expect("localization");
        assert!(loc.set_locale(PSEUDO_LOCALE));
        let start = loc.resolve("start", &[]);
        assert_eq!(start, "«Šţáŕţ~~»");
        let hello = loc.resolve("hello", &[("name".to_owned(), "Zoé".to_owned())]);
        assert!(hello.contains("Zoé"), "{hello}");
        assert!(hello.starts_with('«'), "{hello}");
        let items = loc.resolve("items", &[("n".to_owned(), "1".to_owned())]);
        assert!(items.starts_with("«óñé"), "{items}");
        assert!(loc.set_locale(KEY_LOCALE));
        assert_eq!(loc.resolve("start", &[]), "⟪start⟫");
        assert_eq!(
            loc.resolve("hello", &[("name".to_owned(), "Zoé".to_owned())]),
            "⟪hello name=Zoé⟫"
        );
        assert_eq!(
            loc.resolve("ghost", &[]),
            "⟪ghost⟫",
            "referenced but undefined keys show too"
        );
    }

    #[test]
    fn expansion_makes_every_pseudo_string_longer() {
        let out = output();
        let table = pseudo_table(&out, "en", &PseudoOptions::default()).expect("table");
        let base = &out.tables["en"];
        for (key, entry) in &table.entries {
            assert!(
                entry.source.chars().count() > base.entries[key].source.chars().count(),
                "{key}"
            );
        }
    }
}
