// SPDX-License-Identifier: MPL-2.0
//! Pseudo-localization (`docs/specs/content-data.md` § 1.8, ADR-1466): text that stays readable but
//! shows layout problems before any translation exists.
//!
//! [`pseudo_text`] replaces ASCII letters with accented look-alikes and pads each run of text by
//! about 40 % (languages such as German and Finnish often run that much longer than English);
//! [`wrap`] brackets a whole message so clipped text is visible at both ends. The runtime applies
//! [`pseudo_text`] as Fluent's text transform, so placeables (`{ $count }`) keep their real values.
//! The technique is general localization practice (pseudo-locales such as `en-XA`); the character
//! table is Regia's own.

use std::borrow::Cow;

/// The locale tag `regia loc pseudo` writes by default.
pub const PSEUDO_LOCALE: &str = "en-XA";

/// Expansion added to each text run, in percent of its length.
pub const PSEUDO_EXPANSION_PERCENT: usize = 40;

/// The fill character of the expansion.
pub const PSEUDO_FILL: char = '~';

/// An accented look-alike for an ASCII letter; other characters are kept.
fn accent(c: char) -> char {
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
        'l' => 'ľ',
        'm' => 'ɱ',
        'n' => 'ñ',
        'o' => 'ö',
        'p' => 'þ',
        'q' => 'ǫ',
        'r' => 'ŕ',
        's' => 'š',
        't' => 'ŧ',
        'u' => 'ü',
        'v' => 'ṽ',
        'w' => 'ŵ',
        'x' => 'ẋ',
        'y' => 'ý',
        'z' => 'ž',
        'A' => 'Å',
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
        'O' => 'Ö',
        'P' => 'Þ',
        'Q' => 'Ǫ',
        'R' => 'Ŕ',
        'S' => 'Š',
        'T' => 'Ŧ',
        'U' => 'Ü',
        'V' => 'Ṽ',
        'W' => 'Ŵ',
        'X' => 'Ẋ',
        'Y' => 'Ý',
        'Z' => 'Ž',
        other => other,
    }
}

/// The number of fill characters added to a run of `chars` characters: 40 % rounded up, at least 2.
#[must_use]
pub fn padding_for(chars: usize) -> usize {
    (chars * PSEUDO_EXPANSION_PERCENT).div_ceil(100).max(2)
}

/// Pseudo-localizes one run of text: accented letters plus the expansion fill.
///
/// An empty run is unchanged. Every other run, including digits, punctuation and spaces, grows by
/// [`padding_for`] characters (spec § 1.8). Trailing whitespace stays after the fill, so a gap
/// after a word is not swallowed by the pad. Placeables are separate Fluent elements and never
/// reach this function.
#[must_use]
pub fn pseudo_text(text: &str) -> Cow<'_, str> {
    if text.is_empty() {
        return Cow::Borrowed(text);
    }
    let count = text.chars().count();
    let mut out = String::with_capacity(text.len() * 2 + padding_for(count));
    // Keep trailing whitespace after the fill so "Items: { $n }" pads the word, not the gap.
    let body = text.trim_end();
    out.extend(body.chars().map(accent));
    out.extend(std::iter::repeat_n(PSEUDO_FILL, padding_for(count)));
    out.push_str(&text[body.len()..]);
    Cow::Owned(out)
}

/// Brackets a whole resolved message: `[` + `text` + `]`.
#[must_use]
pub fn wrap(text: &str) -> String {
    format!("[{text}]")
}

/// The text transform the Fluent bundle calls for each text element in pseudo mode.
pub(crate) fn transform(text: &str) -> Cow<'_, str> {
    pseudo_text(text)
}

/// Pseudo-localizes a whole `.ftl` file: every text element of every message and term value and
/// attribute goes through [`pseudo_text`], and each message value is bracketed (`{ "[" }` string
/// placeables). Identifiers,
/// variant keys, placeables and comments are kept, so the result is a valid Fluent file for a
/// pseudo locale such as [`PSEUDO_LOCALE`].
///
/// # Errors
///
/// The source does not parse (the message names the first error).
pub fn pseudo_ftl(source: &str) -> Result<String, String> {
    use fluent_syntax::ast;
    fn pattern(p: &mut ast::Pattern<String>) {
        for element in &mut p.elements {
            match element {
                ast::PatternElement::TextElement { value } => {
                    if let Cow::Owned(changed) = pseudo_text(value) {
                        *value = changed;
                    }
                }
                ast::PatternElement::Placeable { expression } => expr(expression),
            }
        }
    }
    fn expr(e: &mut ast::Expression<String>) {
        match e {
            ast::Expression::Select { variants, .. } => {
                for variant in variants {
                    pattern(&mut variant.value);
                }
            }
            ast::Expression::Inline(ast::InlineExpression::Placeable { expression }) => expr(expression),
            ast::Expression::Inline(_) => {}
        }
    }
    // A string-literal placeable, not a text element: a multiline value starts on an indented
    // line, where a bare `[` would read as a variant key.
    fn text(value: &str) -> ast::PatternElement<String> {
        ast::PatternElement::Placeable {
            expression: ast::Expression::Inline(ast::InlineExpression::StringLiteral {
                value: value.to_owned(),
            }),
        }
    }
    let mut resource = fluent_syntax::parser::parse(source.to_owned()).map_err(|(_, errors)| {
        errors
            .first()
            .map_or_else(String::new, |e| format!("{:?} at byte {}", e.kind, e.pos.start))
    })?;
    for entry in &mut resource.body {
        match entry {
            ast::Entry::Message(message) => {
                if let Some(value) = &mut message.value {
                    pattern(value);
                    value.elements.insert(0, text("["));
                    value.elements.push(text("]"));
                }
                for attribute in &mut message.attributes {
                    pattern(&mut attribute.value);
                }
            }
            ast::Entry::Term(term) => {
                pattern(&mut term.value);
                for attribute in &mut term.attributes {
                    pattern(&mut attribute.value);
                }
            }
            _ => {}
        }
    }
    Ok(fluent_syntax::serializer::serialize(&resource))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ftl_files_keep_keys_and_placeables() {
        let out = pseudo_ftl(
            "menu-start = Start\nitems = { $count ->\n    [one] One item\n   *[other] { $count } items\n}\n",
        )
        .expect("parse");
        assert!(out.contains("menu-start = { \"[\" }Šŧáŕŧ~~{ \"]\" }"), "{out}");
        assert!(out.contains("[one] Öñé íŧéɱ~~~~"), "{out}");
        assert!(out.contains("{ $count }"), "{out}");
        assert!(pseudo_ftl("broken = { $").is_err());
    }

    #[test]
    fn letters_are_accented_and_padded() {
        let out = pseudo_text("Start Game");
        assert!(out.starts_with("Šŧáŕŧ Ĝáɱé"), "{out}");
        // 10 characters → 4 fill characters.
        assert!(out.ends_with("~~~~"), "{out}");
        assert_eq!(out.chars().count(), 14);
    }

    #[test]
    fn runs_without_letters_are_padded() {
        assert_eq!(pseudo_text(""), "");
        // One space: two fill characters, then the trailing space.
        assert_eq!(pseudo_text(" "), "~~ ");
        // Four characters → two fill characters. Digits and punctuation stay readable.
        assert_eq!(pseudo_text(": 42"), ": 42~~");
        assert_eq!(pseudo_text("100"), "100~~");
    }

    #[test]
    fn trailing_space_stays_after_the_fill() {
        assert_eq!(pseudo_text("Score: "), "Šçöŕé:~~~ ");
    }

    #[test]
    fn padding_is_at_least_two() {
        assert_eq!(padding_for(1), 2);
        assert_eq!(padding_for(10), 4);
        assert_eq!(padding_for(11), 5);
        assert_eq!(wrap("x"), "[x]");
    }
}
