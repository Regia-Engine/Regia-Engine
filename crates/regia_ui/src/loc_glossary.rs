// SPDX-License-Identifier: MPL-2.0
//! The project glossary and translation memory (spec 36 § 3, ADR-3359).
//!
//! The glossary (`assets/localization/glossary.locglossary.ron`, format `glossary` v1) lists
//! product terms and their approved translation per locale; `regia loc validate` warns when a
//! message translates a term differently ([`glossary_findings`], `loc_glossary_mismatch`).
//!
//! The translation memory is derived, not authored: [`build_memory`] pairs every translated
//! message's source pattern with its translation. On `regia loc export`, an untranslated entry
//! whose source pattern was translated before is offered as a note; an exact match pre-fills
//! `msgstr` and carries the `fuzzy` and [`TM_REVIEW_FLAG`] flags so a translator reviews it; the
//! import skips it until then ([`apply_memory`], ADR-5577). Matching is normalized (case and whitespace) and deterministic.

use std::collections::{BTreeMap, HashMap};

use crate::loc_check::{self, ParsedLocale};
use crate::loc_po;

/// The current glossary format version.
pub const GLOSSARY_FORMAT_VERSION: u32 = 1;
/// The glossary file, relative to `assets/localization/`.
pub const GLOSSARY_FILE: &str = "glossary.locglossary.ron";
/// The PO/XLIFF flag an exact translation-memory match carries: pre-filled, review before use. The
/// entry also carries `fuzzy` (PO) or an `initial` segment state (XLIFF) until it is confirmed.
pub const TM_REVIEW_FLAG: &str = "regia-review";
/// A near match is offered as a note at or above this similarity (Levenshtein ratio).
pub const TM_NEAR_THRESHOLD: f64 = 0.75;
/// Near-match candidates are compared within a bucket of this prefix length; patterns longer
/// than twice this still share a bucket when their first `NEAR_PREFIX` characters agree.
const NEAR_PREFIX: usize = 12;
/// Near matching refuses patterns longer than this (exact matching has no limit); the bounded
/// distance matrix stays a tool, not an attack surface.
const NEAR_MAX_LEN: usize = 4096;
/// At most this many candidates of one bucket are scanned for a near match.
const NEAR_BUCKET_CAP: usize = 128;

/// One glossary term and its approved translations.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlossaryTerm {
    /// The source-language term ("Save", "quick save").
    pub term: String,
    /// The approved translation per locale tag (`"de"`, `"fr-CA"`); a lookup falls back to the
    /// tag's language part.
    pub translations: BTreeMap<String, String>,
    /// A note for translators ("verb, not noun").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The parsed glossary file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlossaryFile {
    /// Must be [`GLOSSARY_FORMAT_VERSION`].
    pub format_version: u32,
    /// The terms, in file order.
    pub terms: Vec<GlossaryTerm>,
}

impl Default for GlossaryFile {
    fn default() -> Self {
        Self {
            format_version: GLOSSARY_FORMAT_VERSION,
            terms: Vec::new(),
        }
    }
}

/// Errors reading a glossary file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GlossaryError {
    /// The file is not valid RON for the shape.
    #[error("glossary does not parse: {0}")]
    Parse(String),
    /// A version this build does not know.
    #[error("glossary has format_version {found}; this build reads up to {supported}")]
    Version {
        /// The file's version.
        found: u32,
        /// The newest version this build reads.
        supported: u32,
    },
    /// A term with an empty `term` or an empty translation.
    #[error("glossary term {index} is empty or has an empty translation")]
    EmptyTerm {
        /// The 0-based term index.
        index: usize,
    },
}

/// Parses the glossary's text and checks it.
///
/// # Errors
///
/// The text does not parse, the version is newer than this build, or a term is empty.
pub fn parse_glossary(text: &str) -> Result<GlossaryFile, GlossaryError> {
    // IMPLICIT_SOME, the `.ui.ron` parser's extension: a bare value fills an `Option` field
    // (`note: "verb"` ≡ `note: Some("verb")`).
    let file: GlossaryFile = ron::Options::default()
        .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
        .from_str(text)
        .map_err(|e| GlossaryError::Parse(e.to_string()))?;
    if file.format_version > GLOSSARY_FORMAT_VERSION || file.format_version == 0 {
        return Err(GlossaryError::Version {
            found: file.format_version,
            supported: GLOSSARY_FORMAT_VERSION,
        });
    }
    for (index, term) in file.terms.iter().enumerate() {
        if term.term.is_empty() || term.translations.values().any(|t| t.is_empty()) {
            return Err(GlossaryError::EmptyTerm { index });
        }
    }
    Ok(file)
}

/// The serde field-list fingerprint of the `glossary` format (hashed by
/// `cargo xtask check-formats`).
#[must_use]
pub fn glossary_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<GlossaryFile>(),
        regia_core::schema::serde_field_names::<GlossaryTerm>(),
    ])
}

/// The `from` versions of the format's migration steps (none at v1).
#[must_use]
pub fn glossary_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The approved translation of `term` for `locale`, or the language part's when the full tag
/// has none.
#[must_use]
pub fn term_translation<'a>(term: &'a GlossaryTerm, locale: &str) -> Option<&'a str> {
    term.translations
        .get(locale)
        .or_else(|| locale.split('-').next().and_then(|l| term.translations.get(l)))
        .map(String::as_str)
}

/// Whether `text` contains `needle` at a word boundary, case-insensitively. Boundaries are
/// non-alphanumeric characters or the string edges, so "save" matches "Save" and "quick save"
/// but not "saved" or "Savery".
#[must_use]
pub fn contains_word(text: &str, needle: &str) -> bool {
    let text = text.to_lowercase();
    let needle = needle.to_lowercase();
    if needle.is_empty() {
        return false;
    }
    let boundary = |c: char| !c.is_alphanumeric();
    let mut start = 0;
    while let Some(at) = text[start..].find(&needle) {
        let at = start + at;
        let before = at == 0 || text[..at].chars().next_back().is_some_and(boundary);
        let after_end = at + needle.len();
        let after = after_end >= text.len() || text[after_end..].chars().next().is_some_and(boundary);
        if before && after {
            return true;
        }
        // `at` is a char boundary (the needle matched). One byte past it is inside a
        // multibyte scalar, and slicing there panics on the next search.
        let step = text[at..].chars().next().map_or(1, char::len_utf8);
        start = at + step;
    }
    false
}

/// `loc_glossary_mismatch` findings: a message whose source uses a glossary term but whose
/// translation in `locale` does not use the approved translation of that term.
///
/// Only messages the locale really translates are checked (a placeholder or fallback is not an
/// opinion about terminology). One finding per (locale, key, term).
#[must_use]
pub fn glossary_findings(
    glossary: &GlossaryFile,
    source_files: &[(String, String)],
    locales: &[(ParsedLocale, Vec<(String, String)>)],
) -> Vec<loc_check::Finding> {
    let source = loc_po::source_blocks(source_files);
    let mut out = Vec::new();
    for (parsed, files) in locales {
        let tag = parsed.locale.as_str();
        let target = loc_po::translations(files);
        for (key, block) in &target {
            let Some(original) = source.get(key) else {
                continue;
            };
            if original.pattern.is_empty() || block.pattern.is_empty() {
                continue;
            }
            for term in &glossary.terms {
                if !contains_word(&original.pattern, &term.term) {
                    continue;
                }
                let Some(approved) = term_translation(term, tag) else {
                    continue;
                };
                if contains_word(&block.pattern, approved) {
                    continue;
                }
                let (file, line) = parsed
                    .messages
                    .get(key)
                    .map_or((None, None), |m| (Some(m.file.clone()), Some(m.line)));
                out.push(loc_check::Finding {
                    code: "loc_glossary_mismatch",
                    severity: loc_check::Severity::Warning,
                    locale: tag.to_owned(),
                    key: Some(key.clone()),
                    file,
                    line,
                    message: format!(
                        "`{key}` uses the term \"{}\" but its {tag} translation does not use \"{approved}\"",
                        term.term
                    ),
                });
            }
        }
    }
    out.sort_by(|a, b| {
        (a.locale.as_str(), a.key.as_deref(), a.message.as_str()).cmp(&(
            b.locale.as_str(),
            b.key.as_deref(),
            b.message.as_str(),
        ))
    });
    out
}

/// One translation-memory pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmEntry {
    /// The message key the pair was taken from.
    pub key: String,
    /// The locale the translation belongs to.
    pub locale: String,
    /// The source pattern.
    pub source: String,
    /// The translation pattern.
    pub translation: String,
}

/// A translation memory derived from the project's translations ([`build_memory`]).
#[derive(Debug, Clone, Default)]
pub struct TranslationMemory {
    entries: Vec<TmEntry>,
    /// Normalized source → indices into `entries` (exact matches).
    exact: HashMap<String, Vec<usize>>,
    /// A normalized source prefix → indices into `entries` (near-match candidates).
    near: HashMap<String, Vec<usize>>,
}

/// The matching form of a pattern: lowercased with whitespace collapsed.
fn normalize(pattern: &str) -> String {
    pattern
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The near-match bucket key of a normalized pattern: its first word (truncated). Matches that
/// differ later in the string ("Load games" vs "Load game") share a bucket; buckets are scanned
/// under [`NEAR_BUCKET_CAP`], so a very common first word stays bounded.
fn bucket_key(normalized: &str) -> String {
    normalized
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .take(NEAR_PREFIX)
        .collect()
}

/// Pairs every translated message of `locales` (the source locale excluded) with its source
/// pattern. Later definitions of the same key in one locale win, the runtime merge rule.
#[must_use]
pub fn build_memory(
    source_files: &[(String, String)],
    locales: &[(ParsedLocale, Vec<(String, String)>)],
) -> TranslationMemory {
    let source = loc_po::source_blocks(source_files);
    let mut memory = TranslationMemory::default();
    for (parsed, files) in locales {
        if files.is_empty() {
            continue;
        }
        for (key, block) in loc_po::translations(files) {
            let Some(original) = source.get(&key) else {
                continue;
            };
            if original.pattern.is_empty() || block.pattern.is_empty() {
                continue;
            }
            let normalized = normalize(&original.pattern);
            let index = memory.entries.len();
            memory.exact.entry(normalized.clone()).or_default().push(index);
            memory
                .near
                .entry(bucket_key(&normalized))
                .or_default()
                .push(index);
            memory.entries.push(TmEntry {
                key,
                locale: parsed.locale.clone(),
                source: original.pattern.clone(),
                translation: block.pattern,
            });
        }
    }
    memory
}

/// One translation-memory suggestion.
#[derive(Debug, Clone, PartialEq)]
pub struct TmSuggest {
    /// `1.0` for an exact (normalized) source match, else the Levenshtein ratio.
    pub similarity: f64,
    /// The matching pair.
    pub entry: TmEntry,
}

/// The Levenshtein ratio of two patterns (1 - distance / max length), or 0 when either side is
/// empty or over [`NEAR_MAX_LEN`].
fn similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() || a.len() > NEAR_MAX_LEN || b.len() > NEAR_MAX_LEN {
        return 0.0;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut prev: Vec<usize> = (0..=short.len()).collect();
    let mut cur = vec![0; short.len() + 1];
    for (i, lc) in long.iter().enumerate() {
        cur[0] = i + 1;
        for (j, sc) in short.iter().enumerate() {
            let cost = usize::from(lc != sc);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let distance = prev[short.len()];
    1.0 - distance as f64 / long.len().max(1) as f64
}

/// The best translation-memory match of `pattern` for `locale`: an exact match when the
/// normalized source was translated before, else the closest near match at or above
/// [`TM_NEAR_THRESHOLD`]. `None` when the memory has nothing close.
#[must_use]
pub fn suggest(memory: &TranslationMemory, locale: &str, pattern: &str) -> Option<TmSuggest> {
    let normalized = normalize(pattern);
    if let Some(indices) = memory.exact.get(&normalized) {
        // The newest pair of this locale wins. A later pair of another locale shares the
        // same normalized source and must not hide the earlier exact match.
        if let Some(&index) = indices
            .iter()
            .rev()
            .find(|&&index| memory.entries[index].locale == locale)
        {
            return Some(TmSuggest {
                similarity: 1.0,
                entry: memory.entries[index].clone(),
            });
        }
    }
    let mut best: Option<TmSuggest> = None;
    let candidates = memory.near.get(&bucket_key(&normalized))?;
    for &index in candidates.iter().take(NEAR_BUCKET_CAP) {
        let entry = &memory.entries[index];
        if entry.locale != locale {
            continue;
        }
        let score = similarity(&normalize(&entry.source), &normalized);
        if score >= TM_NEAR_THRESHOLD && best.as_ref().is_none_or(|b| score > b.similarity) {
            best = Some(TmSuggest {
                similarity: score,
                entry: entry.clone(),
            });
        }
    }
    best
}

/// Adds translation-memory notes and pre-fills to an export ([`crate::loc_po::ExportOutput`]):
/// an untranslated entry whose source pattern has an exact match in `memory` for the exported
/// locale gets the match as `msgstr` plus the `fuzzy` and [`TM_REVIEW_FLAG`] flags and a note (the
/// import skips it until the translator confirms it, ADR-5577); a near match
/// at or above [`TM_NEAR_THRESHOLD`] gets a note. Already-translated entries are untouched.
///
/// Returns `(pre_filled, near_notes)`.
pub fn apply_memory(
    output: &mut crate::loc_po::ExportOutput,
    memory: &TranslationMemory,
    locale: &str,
) -> (usize, usize) {
    let mut pre_filled = 0usize;
    let mut near_notes = 0usize;
    for entry in &mut output.file.entries {
        if !entry.msgstr.is_empty() {
            continue;
        }
        let Some(suggest) = suggest(memory, locale, &entry.msgid) else {
            continue;
        };
        if suggest.similarity >= 1.0 {
            entry.comments.push(format!(
                "tm-exact: {locale}:{} = {}",
                suggest.entry.key, suggest.entry.translation
            ));
            // `fuzzy` is the flag every PO tool shows as "needs work" and clears when the
            // translator approves the entry; until then the import skips it (`loc_po_fuzzy`), so
            // an unreviewed suggestion never lands as a translation (ADR-5577).
            if !entry.flags.iter().any(|f| f == "fuzzy") {
                entry.flags.push("fuzzy".to_owned());
            }
            entry.flags.push(TM_REVIEW_FLAG.to_owned());
            entry.msgstr = suggest.entry.translation;
            pre_filled += 1;
        } else {
            entry.comments.push(format!(
                "tm-near ({:.0}%): {locale}:{} = {}",
                suggest.similarity * 100.0,
                suggest.entry.key,
                suggest.entry.translation
            ));
            near_notes += 1;
        }
    }
    (pre_filled, near_notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loc_check::parse_locale;

    fn parsed(tag: &str, text: &str) -> (ParsedLocale, Vec<(String, String)>) {
        let files = vec![(format!("{tag}/main.ftl"), text.to_owned())];
        (parse_locale(tag, &files), files)
    }

    const SOURCE: &str = "menu-quit = Quit\nmenu-save = Save\nmenu-load = Load game\n";

    #[test]
    fn parses_and_validates_the_glossary() {
        let g = parse_glossary(
            "(format_version: 1, terms: [(term: \"Save\", translations: {\"de\": \"Speichern\"}, note: \"verb\")])",
        )
        .expect("glossary");
        assert_eq!(g.terms.len(), 1);
        assert_eq!(
            term_translation(&g.terms[0], "de-AT"),
            Some("Speichern"),
            "the language part is a fallback"
        );
        assert_eq!(term_translation(&g.terms[0], "fr"), None);
        assert!(parse_glossary("(format_version: 2)").is_err(), "version");
        assert!(
            parse_glossary("(format_version: 1, terms: [(term: \"\")])").is_err(),
            "empty"
        );
    }

    #[test]
    fn word_matching_respects_boundaries() {
        assert!(contains_word("Save the game", "save"));
        assert!(contains_word("quick-save now", "save"));
        assert!(!contains_word("saved game", "save"));
        assert!(!contains_word("Savery", "save"));
        assert!(!contains_word("", "save"));
        assert!(!contains_word("anything", ""));
        // A failed match that starts on a multibyte character must advance by a char, not a
        // byte: `text[at + 1..]` panics when `at` is inside a 2- or 3-byte scalar.
        assert!(!contains_word("café", "é"));
        assert!(contains_word("é", "é"));
        assert!(!contains_word("保存", "存"));
        assert!(contains_word("「保存」", "保存"));
    }

    #[test]
    fn a_different_term_translation_warns_once_per_key() {
        let glossary = parse_glossary(
            "(format_version: 1, terms: [(term: \"Save\", translations: {\"de\": \"Speichern\"})])",
        )
        .expect("glossary");
        let en = parsed("en", SOURCE);
        let de = parsed(
            "de",
            "menu-quit = Beenden\nmenu-save = Sichern\nmenu-load = Spiel laden\n",
        );
        let source_files = en.1.clone();
        let found = glossary_findings(&glossary, &source_files, &[de, en]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].code, "loc_glossary_mismatch");
        assert_eq!(found[0].locale, "de");
        assert_eq!(found[0].key.as_deref(), Some("menu-save"));
        assert!(found[0].message.contains("Speichern"));
        assert_eq!(found[0].severity, loc_check::Severity::Warning);
        // The approved term passes; a placeholder is not checked.
        let de = parsed(
            "de",
            "# TODO translate\nmenu-quit = Beenden\nmenu-save = Speichern\nmenu-load = Spiel laden\n",
        );
        let en = parsed("en", SOURCE);
        let source_files = en.1.clone();
        assert!(glossary_findings(&glossary, &source_files, &[de, en]).is_empty());
    }

    #[test]
    fn memory_prefers_exact_then_near_per_locale() {
        let en = parsed("en", SOURCE);
        let de = parsed(
            "de",
            "menu-quit = Beenden\nmenu-save = Speichern\nmenu-load = Spiel laden\n",
        );
        let fr = parsed(
            "fr",
            "menu-quit = Quitter\nmenu-save = Enregistrer\nmenu-load = Charger la partie\n",
        );
        let source_files = en.1.clone();
        let memory = build_memory(&source_files, &[de, en, fr]);
        // Exact, per locale.
        let hit = suggest(&memory, "de", "Save").expect("exact de");
        assert_eq!(hit.similarity, 1.0);
        assert_eq!(hit.entry.translation, "Speichern");
        let hit = suggest(&memory, "fr", "Save").expect("exact fr");
        assert_eq!(hit.entry.translation, "Enregistrer");
        assert!(suggest(&memory, "it", "Save").is_none(), "no it pairs");
        // Case and extra whitespace still match exactly.
        assert_eq!(
            suggest(&memory, "de", "  save ").expect("normalized").similarity,
            1.0
        );
        // Near: one character off, same first-word bucket.
        let near = suggest(&memory, "de", "Load games").expect("near");
        assert!(
            near.similarity >= TM_NEAR_THRESHOLD && near.similarity < 1.0,
            "{}",
            near.similarity
        );
        assert_eq!(near.entry.key, "menu-load");
        assert!(suggest(&memory, "de", "Completely unrelated text").is_none());
    }

    #[test]
    fn exact_memory_keeps_the_newest_pair_of_the_requested_locale() {
        // Two German pairs share the source "Save", and a later French pair does too. The
        // newest German pair wins; the French pair must not hide it.
        let en = parsed("en", "a = Save\nb = Save\n");
        let de = parsed("de", "a = Speichern\nb = Sichern\n");
        let fr = parsed("fr", "a = Enregistrer\nb = Garder\n");
        let source_files = en.1.clone();
        let memory = build_memory(&source_files, &[de, fr]);
        let hit = suggest(&memory, "de", "Save").expect("exact de");
        assert_eq!(hit.similarity, 1.0);
        assert_eq!(hit.entry.key, "b");
        assert_eq!(hit.entry.translation, "Sichern");
        let hit = suggest(&memory, "fr", "Save").expect("exact fr");
        assert_eq!(hit.entry.key, "b");
        assert_eq!(hit.entry.translation, "Garder");
    }

    #[test]
    fn apply_memory_prefills_exact_matches_and_marks_them() {
        let en = parsed("en", SOURCE);
        let de = parsed(
            "de",
            "menu-quit = Beenden\nmenu-save = Speichern\nmenu-load = Spiel laden\n",
        );
        let source_files = en.1.clone();
        let target_files = vec![de, en];
        let memory = build_memory(&source_files, &target_files);
        let mut input = crate::loc_po::ExportInput {
            project: "p".to_owned(),
            source: "en".to_owned(),
            locale: "de".to_owned(),
            source_files,
            target_files: Vec::new(),
            references: BTreeMap::new(),
            filter: crate::loc_po::ExportFilter::All,
        };
        let mut output = crate::loc_po::export_po(&input);
        let (pre, near) = apply_memory(&mut output, &memory, "de");
        assert_eq!(pre, 3, "every entry pre-fills from the exact pairs");
        assert_eq!(near, 0);
        for entry in &output.file.entries {
            assert_eq!(entry.flags.last().map(String::as_str), Some(TM_REVIEW_FLAG));
            assert!(!entry.msgstr.is_empty());
        }
        // A near match notes without pre-filling.
        input.source_files = vec![("en/main.ftl".to_owned(), "menu-load = Load games\n".to_owned())];
        let mut output = crate::loc_po::export_po(&input);
        let (pre, near) = apply_memory(&mut output, &memory, "de");
        assert_eq!(pre, 0);
        assert_eq!(near, 1, "{:?}", output.file.entries);
        assert!(
            output.file.entries[0].msgstr.is_empty(),
            "near notes do not pre-fill"
        );
        assert!(
            output.file.entries[0]
                .comments
                .iter()
                .any(|c| c.starts_with("tm-near"))
        );
    }

    /// Corr-ui-b (ADR-5577): a pre-fill carried only the custom `regia-review` flag, which the
    /// import never looked at, so a file sent back untouched imported every translation-memory
    /// suggestion as a final translation. It now imports only once the translator confirmed it:
    /// PO clears `fuzzy`, an XLIFF tool moves the segment state past `initial`.
    #[test]
    fn an_unreviewed_prefill_does_not_import_until_confirmed() {
        let en = parsed("en", SOURCE);
        let de = parsed(
            "de",
            "menu-quit = Beenden\nmenu-save = Speichern\nmenu-load = Spiel laden\n",
        );
        let source_files = en.1.clone();
        let memory = build_memory(&source_files, &[de, en]);
        let input = crate::loc_po::ExportInput {
            project: "p".to_owned(),
            source: "en".to_owned(),
            locale: "de".to_owned(),
            source_files: source_files.clone(),
            target_files: Vec::new(),
            references: BTreeMap::new(),
            filter: crate::loc_po::ExportFilter::All,
        };
        let mut output = crate::loc_po::export_po(&input);
        assert_eq!(apply_memory(&mut output, &memory, "de").0, 3);
        let ctx = crate::loc_po::ImportContext {
            locale: "de".to_owned(),
            source: "en".to_owned(),
            source_files,
            ..crate::loc_po::ImportContext::default()
        };

        // PO, sent back untouched: nothing imports, every pre-fill is fuzzy.
        let po = crate::loc_po::read_po(&crate::loc_po::write_po(&output.file)).expect("reads");
        let plan = crate::loc_po::plan_import(&po, &ctx);
        assert_eq!(plan.report.imported, 0, "{:?}", plan.report);
        assert_eq!(plan.report.skipped_with("loc_po_fuzzy"), 3);
        assert!(plan.report.failed(true), "--strict fails on unreviewed pre-fills");
        // The translator approves one (a PO tool clears `fuzzy`): that one imports.
        let mut approved = po.clone();
        approved.entries[0].flags.retain(|f| f != "fuzzy");
        let plan = crate::loc_po::plan_import(&approved, &ctx);
        assert_eq!(plan.report.imported, 1, "{:?}", plan.report);
        assert_eq!(plan.report.skipped_with("loc_po_fuzzy"), 2);

        // XLIFF, sent back untouched: the same.
        let xlf = crate::loc_xliff::write_xliff(&output.file).expect("writes");
        assert!(
            !xlf.contains("rg:fuzzy"),
            "a pre-fill is unconfirmed by state, not rg:fuzzy"
        );
        let plan =
            crate::loc_xliff::plan_import_xliff(&crate::loc_xliff::read_xliff(&xlf).expect("reads"), &ctx);
        assert_eq!(plan.report.imported, 0, "{:?}", plan.report);
        assert_eq!(plan.report.skipped_with("loc_po_fuzzy"), 3);
        // A CAT tool confirms every segment: all three import.
        let confirmed = xlf.replace("<segment state=\"initial\">", "<segment state=\"translated\">");
        let plan = crate::loc_xliff::plan_import_xliff(
            &crate::loc_xliff::read_xliff(&confirmed).expect("reads"),
            &ctx,
        );
        assert_eq!(plan.report.imported, 3, "{:?}", plan.report);
    }
}
