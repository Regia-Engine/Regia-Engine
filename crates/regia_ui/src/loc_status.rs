// SPDX-License-Identifier: MPL-2.0
//! Per-locale translation status for tools and agents (spec 36 § 10, ADR-3359).
//!
//! [`status`] answers the producer's question "how far along is each language?": for every
//! locale it counts the source messages and how many are translated, missing from the whole
//! fallback chain, stale (the recorded `# regia-source:` hash differs from the current source
//! text) or placeholder-mismatched (the `{$variables}` differ from the source message). The
//! counting is pure (text in, counts out); `regia loc status` and the `loc.status` agent method
//! do the IO.
//!
//! Stale detection reuses the hash comments `imported.ftl` records ([`crate::loc_po`]); a
//! hand-written translation carries no hash, so it is never stale.

use std::collections::{BTreeMap, BTreeSet};

use crate::loc_check::ParsedLocale;
use crate::loc_po;

/// One locale's numbers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LocaleStatus {
    /// The locale tag.
    pub locale: String,
    /// The fallback chain the status was computed against.
    pub chain: Vec<String>,
    /// Source messages with a value (the ones a translator sees).
    pub messages: usize,
    /// Of those, the ones the locale translates itself (a `# TODO translate` placeholder is
    /// not a translation).
    pub translated: usize,
    /// No own translation, but some locale of the chain provides the text (players see a
    /// fallback language).
    pub untranslated: usize,
    /// Of the untranslated ones, those present as a `# TODO translate` placeholder.
    pub placeholders: usize,
    /// Not defined by the locale nor any chain locale: the player sees the missing-key marker.
    pub missing: usize,
    /// Translated messages whose recorded source hash differs from the current source text.
    pub stale: usize,
    /// Translated messages whose `{$variables}` differ from the source message (a dropped or
    /// added placeholder is a mismatch).
    pub mismatched: usize,
    /// `translated / messages` in percent (100 when there is nothing to translate).
    pub percent: f64,
}

/// The status of every locale of `locales` against `source_tag`'s files.
///
/// `locales` is each parsed locale with its `(path, text)` files, as
/// [`crate::loc_check::parse_locale`] and the CLI's locale reader produce them. Locales without
/// a directory (configured but not written yet) report everything untranslated. The source
/// locale against itself is fully translated.
///
/// `used_keys`, when given, is the set of keys content and code use (the gather scan): a used
/// key no locale of the chain defines counts as missing even when the source locale does not
/// define it either (players would see the missing-key marker).
#[must_use]
pub fn status(
    source_tag: &str,
    source_files: &[(String, String)],
    locales: &[(ParsedLocale, Vec<(String, String)>)],
    fallbacks: &BTreeMap<String, Vec<String>>,
    used_keys: Option<&BTreeSet<String>>,
) -> Vec<LocaleStatus> {
    let loaded: BTreeSet<String> = locales.iter().map(|(p, _)| p.locale.clone()).collect();
    let by_tag: BTreeMap<&str, &(ParsedLocale, Vec<(String, String)>)> = locales
        .iter()
        .map(|entry| (entry.0.locale.as_str(), entry))
        .collect();
    let empty: (ParsedLocale, Vec<(String, String)>) = (
        ParsedLocale {
            locale: source_tag.to_owned(),
            ..ParsedLocale::default()
        },
        Vec::new(),
    );
    let source = by_tag.get(source_tag).copied().unwrap_or(&empty);
    let source_blocks = loc_po::source_blocks(source_files);
    let mut expected: BTreeSet<&str> = source_blocks
        .values()
        .filter(|b| !b.pattern.is_empty())
        .map(|b| b.key.as_str())
        .collect();
    let source_message_keys = expected.clone();
    if let Some(used) = used_keys {
        expected.extend(used.iter().map(String::as_str));
    }

    let mut out = Vec::new();
    for (parsed, files) in locales {
        let tag = parsed.locale.as_str();
        let chain = crate::localization::fallback_chain(tag, &loaded, fallbacks, source_tag);
        let coverage = loc_po::coverage(tag, source_files, files);
        // Which source keys does the chain provide at all? A placeholder counts: the runtime
        // resolves it to its source-text copy.
        let chain_keys: BTreeSet<&str> = chain
            .iter()
            .filter_map(|l| by_tag.get(l.as_str()))
            .flat_map(|(p, _)| p.messages.keys().map(String::as_str).collect::<Vec<_>>())
            .collect();
        let absent = |k: &str| !parsed.messages.contains_key(k) && !chain_keys.contains(k);
        let missing = expected.iter().copied().filter(|&k| absent(k)).count();
        // Used keys that the source never defined are missing, but they are not among
        // `coverage.messages`, so they must not shrink the fallback untranslated count.
        let missing_messages = expected
            .iter()
            .copied()
            .filter(|&k| source_message_keys.contains(k) && absent(k))
            .count();
        // Placeholder/variable mismatches against the source, one per translated key.
        let mut mismatched = 0usize;
        if tag != source_tag {
            for (key, message) in &parsed.messages {
                let Some(original) = source.0.messages.get(key) else {
                    continue;
                };
                if message.variables != original.variables {
                    mismatched += 1;
                }
            }
        }
        out.push(LocaleStatus {
            locale: tag.to_owned(),
            chain,
            messages: coverage.messages,
            translated: coverage.translated,
            untranslated: coverage
                .messages
                .saturating_sub(coverage.translated)
                .saturating_sub(missing_messages),
            placeholders: coverage.placeholders,
            missing,
            stale: coverage.stale,
            mismatched,
            percent: coverage.percent,
        });
    }
    out.sort_by(|a, b| a.locale.cmp(&b.locale));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loc_check::parse_locale;

    fn parsed(tag: &str, text: &str) -> (ParsedLocale, Vec<(String, String)>) {
        let files = vec![(format!("{tag}/main.ftl"), text.to_owned())];
        (parse_locale(tag, &files), files)
    }

    const SOURCE: &str = "menu-start = Start\nmenu-quit = Quit\nhud-score = Score: { $score }\n";

    #[test]
    fn counts_translated_missing_stale_and_mismatched() {
        // de: one real translation, one placeholder, one variable-dropped mismatch.
        let de = parsed(
            "de",
            "# TODO translate\nmenu-start = Start\nmenu-quit = Beenden\nhud-score = Punkte\n",
        );
        // fr: translations recorded against the old source text (stale) with an added variable.
        let old_hash = loc_po::source_hash("Score: { $points }");
        let fr = parsed(
            "fr",
            &format!(
                "menu-start = Démarrer\nmenu-quit = Quitter\n# regia-source: {old_hash}\nhud-score = Points : {{ $points }}\n"
            ),
        );
        let en = parsed("en", SOURCE);
        let rows = status("en", &en.1.clone(), &[de, en, fr], &BTreeMap::new(), None);
        let de = rows.iter().find(|r| r.locale == "de").expect("de");
        assert_eq!(de.messages, 3);
        assert_eq!(de.translated, 2, "the placeholder is not a translation");
        assert_eq!(de.placeholders, 1);
        assert_eq!(de.missing, 0, "de defines every key itself");
        assert_eq!(de.stale, 0, "no recorded hashes");
        assert_eq!(de.mismatched, 1, "hud-score drops the score variable");
        assert!((de.percent - 66.666_666).abs() < 1e-4, "{}", de.percent);
        let fr = rows.iter().find(|r| r.locale == "fr").expect("fr");
        assert_eq!(fr.translated, 3);
        assert_eq!(fr.stale, 1, "the recorded hash does not match the current source");
        assert_eq!(fr.mismatched, 1, "$points where the source passes $score");
        let en = rows.iter().find(|r| r.locale == "en").expect("en");
        assert_eq!(en.translated, 3);
        assert_eq!(en.percent, 100.0);
        assert_eq!(en.mismatched, 0);
    }

    #[test]
    fn a_configured_locale_without_a_directory_is_all_untranslated() {
        let en = parsed("en", SOURCE);
        let zz = parsed("zz", "");
        let source_files = en.1.clone();
        let rows = status("en", &source_files, &[en, zz], &BTreeMap::new(), None);
        let zz = rows.iter().find(|r| r.locale == "zz").expect("zz");
        assert_eq!(zz.messages, 3);
        assert_eq!(zz.translated, 0);
        assert_eq!(
            zz.missing, 0,
            "the source is the last chain link and has every key"
        );
        assert_eq!(zz.untranslated, 3);
        assert_eq!(zz.percent, 0.0);
    }

    #[test]
    fn a_used_key_nowhere_in_the_chain_is_missing() {
        let en = parsed("en", "menu-start = Start\n");
        let de = parsed("de", "menu-start = Starten\n");
        let source_files = en.1.clone();
        // Content uses `hud-missing`, which no locale of any chain defines.
        let used = BTreeSet::from(["hud-missing".to_owned()]);
        let rows = status("en", &source_files, &[de, en], &BTreeMap::new(), Some(&used));
        let de = rows.iter().find(|r| r.locale == "de").expect("de");
        assert_eq!(de.messages, 1, "missing is counted apart from the message total");
        assert_eq!(de.translated, 1);
        assert_eq!(de.missing, 1, "hud-missing is nowhere in de's chain");
        assert_eq!(de.untranslated, 0);
    }

    #[test]
    fn fallback_chain_text_counts_as_untranslated_not_missing() {
        let en = parsed("en", SOURCE);
        // fr-CA has no directory of its own; fr provides two keys and the third stays missing
        // when fr lacks it.
        let fr = parsed("fr", "menu-start = Démarrer\n");
        let ca = parsed("fr-CA", "");
        let source_files = en.1.clone();
        let rows = status("en", &source_files, &[ca.clone(), en, fr], &BTreeMap::new(), None);
        let ca = rows.iter().find(|r| r.locale == "fr-CA").expect("fr-CA");
        assert_eq!(
            ca.chain,
            vec!["fr-CA".to_owned(), "fr".to_owned(), "en".to_owned()]
        );
        assert_eq!(ca.translated, 0);
        assert_eq!(ca.untranslated, 3, "fr and en provide the text");
        assert_eq!(ca.missing, 0);
    }

    #[test]
    fn a_missing_used_key_does_not_erase_fallback_untranslated() {
        // `hud-missing` is not a source message. Subtracting it from the untranslated count
        // used to report `menu-quit` (present on the source, absent in de) as translated.
        let en = parsed("en", "menu-start = Start\nmenu-quit = Quit\n");
        let de = parsed("de", "menu-start = Starten\n");
        let source_files = en.1.clone();
        let used = BTreeSet::from(["hud-missing".to_owned()]);
        let rows = status("en", &source_files, &[de, en], &BTreeMap::new(), Some(&used));
        let de = rows.iter().find(|r| r.locale == "de").expect("de");
        assert_eq!(de.messages, 2);
        assert_eq!(de.translated, 1);
        assert_eq!(de.missing, 1, "hud-missing is not a source message");
        assert_eq!(de.untranslated, 1, "menu-quit falls back to en");
    }
}
