// SPDX-License-Identifier: MPL-2.0
//! Shipping bundles for `regia loc compile` (spec 36 § 4, ADR-1668).
//!
//! Development reads `.ftl` files directly; a shipped build needs only the locales it ships, free of
//! authoring noise. [`normalize_locale`] turns a locale's files into one deterministic text (files
//! merged in the runtime order, comments dropped, gather placeholders omitted so the fallback chain
//! shows the source text, messages sorted by key) and [`BundleIndex`] lists the bundles with a content
//! hash. Pure: the CLI reads and writes the files and computes the hash.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::loc_po::ftl_blocks_with_terms;

/// The `format_version` of `loc/index.ron`.
pub const BUNDLE_INDEX_VERSION: u32 = 1;

/// One locale's normalized bundle text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NormalizedLocale {
    /// The bundle file text: one block per entry, blank line between blocks, sorted by key.
    pub text: String,
    /// How many entries it holds (messages and Fluent terms).
    pub messages: usize,
}

/// Normalize a locale's `(path, text)` files, given in the order the runtime combines them (a later
/// file wins). A message marked `# TODO translate` removes that key (the source text shows through the
/// fallback chain instead of an untranslated copy). Comments are dropped; attributes and Fluent terms
/// are kept, sorted with the messages (a term id starts with `-`, so it sorts before letter keys).
#[must_use]
pub fn normalize_locale(files: &[(String, String)]) -> NormalizedLocale {
    let mut blocks: BTreeMap<String, String> = BTreeMap::new();
    for (_, text) in files {
        for block in ftl_blocks_with_terms(text) {
            if block.placeholder {
                blocks.remove(&block.key);
            } else {
                blocks.insert(block.key, block.raw);
            }
        }
    }
    let messages = blocks.len();
    let text = blocks.into_values().collect::<Vec<_>>().join("\n\n");
    NormalizedLocale {
        text: if text.is_empty() {
            text
        } else {
            format!("{text}\n")
        },
        messages,
    }
}

/// One bundle of a [`BundleIndex`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleLocale {
    /// The locale tag.
    pub tag: String,
    /// The bundle file, relative to the index (`loc/fr.ftl`).
    pub file: String,
    /// Entries in the file (messages and Fluent terms).
    pub messages: usize,
    /// Content hash of the file (hex).
    pub hash: String,
}

/// `loc/index.ron`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleIndex {
    /// Must be [`BUNDLE_INDEX_VERSION`]. A version a build does not read is rebuilt by the next compile.
    pub format_version: u32,
    /// The source locale.
    pub source: String,
    /// The bundles, sorted by tag.
    pub locales: Vec<BundleLocale>,
    /// Extra fallback edges of the shipped locales.
    pub fallbacks: BTreeMap<String, Vec<String>>,
}

impl BundleIndex {
    /// Pretty RON.
    ///
    /// # Errors
    ///
    /// The message of a serialization failure.
    pub fn to_ron(&self) -> Result<String, String> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| e.to_string())
    }

    /// Parse an index; a newer version is an error.
    ///
    /// # Errors
    ///
    /// Bad RON or a newer `format_version`.
    pub fn from_ron(text: &str) -> Result<Self, String> {
        let index: Self = ron::from_str(text).map_err(|e| e.to_string())?;
        if index.format_version > BUNDLE_INDEX_VERSION {
            return Err(format!(
                "loc/index.ron format_version {} is newer than the supported {BUNDLE_INDEX_VERSION}",
                index.format_version
            ));
        }
        Ok(index)
    }
}

/// The source locale has no `.ftl` files, so a shipping set cannot include it (spec 36 § 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSourceLocale {
    /// The source locale tag that was absent from `available`.
    pub source: String,
}

impl std::fmt::Display for MissingSourceLocale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the source locale `{}` has no files, so it cannot ship",
            self.source
        )
    }
}

impl std::error::Error for MissingSourceLocale {}

/// The locales a pak ships: `requested` plus every locale their fallback chains need among `available`,
/// and the source locale. Sorted.
///
/// # Errors
///
/// [`MissingSourceLocale`] when `source` is not in `available`. The source locale always ships
/// (spec 36 § 4); a tag with no files cannot.
pub fn locales_to_ship(
    requested: &[String],
    available: &BTreeSet<String>,
    fallbacks: &BTreeMap<String, Vec<String>>,
    source: &str,
) -> Result<Vec<String>, MissingSourceLocale> {
    if !available.contains(source) {
        return Err(MissingSourceLocale {
            source: source.to_owned(),
        });
    }
    let mut out: BTreeSet<String> = BTreeSet::new();
    out.insert(source.to_owned());
    for locale in requested {
        out.extend(crate::localization::fallback_chain(
            locale, available, fallbacks, source,
        ));
        if available.contains(locale) {
            out.insert(locale.clone());
        }
    }
    out.retain(|l| available.contains(l));
    Ok(out.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(parts: &[(&str, &str)]) -> Vec<(String, String)> {
        parts
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn normalization_sorts_merges_drops_comments_and_placeholders() {
        let gathered = "\n# source: a.ron:1\n# TODO translate\nmenu-quit = Quit\n";
        let main = "# a comment\nzeta = Zeta\nalpha = Alpha\n    .tip = Tip\nmenu-quit = Quitter\n";
        let imported = "# regia-source: 00\nalpha = Old alpha\n";
        let n = normalize_locale(&files(&[
            ("fr/gathered.ftl", gathered),
            ("fr/imported.ftl", imported),
            ("fr/main.ftl", main),
        ]));
        assert_eq!(n.messages, 3);
        assert_eq!(
            n.text,
            "alpha = Alpha\n    .tip = Tip\n\nmenu-quit = Quitter\n\nzeta = Zeta\n"
        );
        // Deterministic, and a placeholder-only translation is omitted.
        assert_eq!(
            normalize_locale(&files(&[("fr/gathered.ftl", gathered)])),
            NormalizedLocale::default()
        );
        assert_eq!(
            normalize_locale(&files(&[("fr/gathered.ftl", gathered), ("fr/main.ftl", main)])),
            n
        );
    }

    #[test]
    fn normalization_keeps_a_term_the_message_references() {
        let src = "-brand = Regia\ntitle = Welcome to { -brand }\n";
        let n = normalize_locale(&files(&[("en/main.ftl", src)]));
        assert_eq!(n.messages, 2);
        assert_eq!(n.text, "-brand = Regia\n\ntitle = Welcome to { -brand }\n");
        let placeholder = "# TODO translate\n-brand = Regia\nhello = Bonjour\n";
        let dropped = normalize_locale(&files(&[("fr/main.ftl", placeholder)]));
        assert!(!dropped.text.contains("-brand"), "{}", dropped.text);
        assert!(dropped.text.contains("hello = Bonjour"), "{}", dropped.text);
    }

    #[test]
    fn index_round_trips_and_refuses_newer_versions() {
        let index = BundleIndex {
            format_version: 1,
            source: "en".into(),
            locales: vec![BundleLocale {
                tag: "fr".into(),
                file: "loc/fr.ftl".into(),
                messages: 3,
                hash: "ab".into(),
            }],
            fallbacks: BTreeMap::from([("pt-BR".to_owned(), vec!["pt-PT".to_owned()])]),
        };
        let text = index.to_ron().expect("writes");
        assert_eq!(BundleIndex::from_ron(&text).expect("reads"), index);
        assert!(BundleIndex::from_ron(&text.replace("format_version: 1", "format_version: 9")).is_err());
    }

    #[test]
    fn shipping_set_includes_fallback_chains_and_the_source() {
        let available: BTreeSet<String> = ["en", "fr", "fr-CA", "de", "es"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let none = BTreeMap::new();
        let ship = locales_to_ship(&["fr-CA".to_owned()], &available, &none, "en").expect("en is present");
        assert_eq!(
            ship,
            ["en", "fr", "fr-CA"],
            "the chain needs fr; de and es are absent"
        );
        let explicit = BTreeMap::from([("de".to_owned(), vec!["es".to_owned()])]);
        let ship = locales_to_ship(&["de".to_owned()], &available, &explicit, "en").expect("en is present");
        assert_eq!(ship, ["de", "en", "es"]);
        assert_eq!(
            locales_to_ship(&[], &available, &none, "en").expect("en is present"),
            ["en"]
        );
        let french_only: BTreeSet<String> = ["fr".to_owned()].into_iter().collect();
        let err = locales_to_ship(&["fr".to_owned()], &french_only, &none, "en").expect_err("no en files");
        assert_eq!(err.source, "en");
        assert!(err.to_string().contains("en"));
    }

    /// Fluent lets the `}` closing a select expression start a line at column 0. Normalizing must
    /// keep that line, or the shipped bundle does not parse and the game runs with no texts.
    #[test]
    fn normalization_keeps_a_closing_brace_at_column_zero() {
        let src = "items = { $count ->\n    [one] One item\n   *[other] { $count } items\n}\nnext = Next\n";
        let out = normalize_locale(&[("en/main.ftl".to_owned(), src.to_owned())]);
        assert_eq!(out.messages, 2);
        assert!(out.text.contains("items\n}"), "{}", out.text);
        let loc = crate::localization::Localization::new(
            vec![("en".to_owned(), out.text.clone())],
            Some("en".to_owned()),
        )
        .expect("the normalized bundle parses");
        assert_eq!(
            loc.resolve("items", &[("count".to_owned(), "1".to_owned())]),
            "One item"
        );
        assert_eq!(loc.resolve("next", &[]), "Next");
    }

    /// A continuation line that starts with an ideographic space (U+3000, common in Japanese
    /// text) after the indentation must not be cut inside that character.
    #[test]
    fn normalization_keeps_an_ideographic_space_after_the_indent() {
        let src = "intro =\n  \u{3000}はじめに\n    続き\n";
        let out = normalize_locale(&[("ja/main.ftl".to_owned(), src.to_owned())]);
        assert_eq!(out.messages, 1);
        assert!(out.text.contains("\u{3000}はじめに"), "{}", out.text);
    }
}
