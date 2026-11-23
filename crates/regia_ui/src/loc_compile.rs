// SPDX-License-Identifier: MPL-2.0
//! Localization pipeline, step 2 (ADR-1570): compile.
//!
//! [`compile`] turns the [`Gathered`] output into one [`CompiledTable`] per locale. A table holds
//! the verbatim Fluent source of every key the locale can show, taken from the first locale of
//! its fallback chain that defines the key, and remembers where each entry came from. The
//! [`LocalizationReport`] lists, per locale, the keys it lacks (missing), the keys nothing in its
//! chain can show (unresolved), and `$variables` that differ from the default locale's text.
//!
//! Fallback chains: a locale falls back through its own shorter tags (`zh-Hant-TW`, `zh-Hant`,
//! `zh`) and ends at the default locale, unless [`CompileOptions::with_chain`] gives it an
//! explicit list. Tables can be flattened to Fluent source ([`CompiledTable::to_ftl`]) and loaded
//! into a runtime [`Localization`] ([`CompileOutput::to_localization`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::UiError;
use crate::loc_manifest::{Gathered, SourceLoc};
use crate::localization::Localization;

/// Why [`compile`] refused to run. Codes are lowercase ascii, digits and `_`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    /// Nothing was gathered: no locale has a `.ftl` source.
    #[error("no locales: gather found no .ftl sources")]
    NoLocales,
    /// The default locale has no `.ftl` source.
    #[error("default locale {0:?} has no .ftl source")]
    UnknownDefaultLocale(String),
}

impl CompileError {
    /// The stable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoLocales => "no_locales",
            Self::UnknownDefaultLocale(_) => "unknown_default_locale",
        }
    }
}

/// Settings for [`compile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileOptions {
    /// The locale every chain ends in, and the reference for placeholder checks.
    pub default_locale: String,
    /// Explicit fallback lists per locale (the locale itself and the default are added for you).
    pub chains: BTreeMap<String, Vec<String>>,
}

impl CompileOptions {
    /// Options with `default_locale` and no explicit chains.
    pub fn new(default_locale: impl Into<String>) -> Self {
        Self {
            default_locale: default_locale.into(),
            chains: BTreeMap::new(),
        }
    }

    /// Sets the explicit fallback list for `locale` (tried in order after `locale` itself).
    pub fn with_chain(mut self, locale: impl Into<String>, chain: &[&str]) -> Self {
        self.chains
            .insert(locale.into(), chain.iter().map(|s| (*s).to_owned()).collect());
        self
    }
}

/// The fallback chain of `locale`: itself, then its explicit list (or its shorter tags), then
/// `default`, without repeats.
///
/// ```ignore
/// assert_eq!(fallback_chain("pt-BR", "en", &Default::default()), ["pt-BR", "pt", "en"]);
/// ```
pub fn fallback_chain(locale: &str, default: &str, explicit: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut chain = vec![locale.to_owned()];
    match explicit.get(locale) {
        Some(list) => chain.extend(list.iter().cloned()),
        None => {
            let mut tag = locale;
            while let Some((shorter, _)) = tag.rsplit_once('-') {
                chain.push(shorter.to_owned());
                tag = shorter;
            }
        }
    }
    chain.push(default.to_owned());
    let mut seen = BTreeSet::new();
    chain.retain(|l| seen.insert(l.clone()));
    chain
}

/// One entry of a compiled table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableEntry {
    /// The verbatim Fluent source of the message (id line and continuation lines).
    pub source: String,
    /// The locale the entry was taken from.
    pub origin_locale: String,
    /// Where the message is defined.
    pub at: SourceLoc,
    /// `$variables` the message uses, sorted.
    pub variables: BTreeSet<String>,
    /// Whether the message has a value (not attributes only).
    pub has_value: bool,
}

/// The messages one locale can show, with fallbacks applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompiledTable {
    /// The locale tag.
    pub locale: String,
    /// The fallback chain used, starting with `locale`.
    pub chain: Vec<String>,
    /// Entries by key.
    pub entries: BTreeMap<String, TableEntry>,
}

impl CompiledTable {
    /// The entry for `key`.
    pub fn get(&self, key: &str) -> Option<&TableEntry> {
        self.entries.get(key)
    }

    /// Whether `key` is shown from this locale's own text (not from a fallback).
    pub fn is_native(&self, key: &str) -> bool {
        self.entries
            .get(key)
            .is_some_and(|e| e.origin_locale == self.locale)
    }

    /// The table as Fluent source, entries sorted by key, ready for [`Localization::new`].
    pub fn to_ftl(&self) -> String {
        let mut out = String::new();
        for entry in self.entries.values() {
            out.push_str(&entry.source);
            out.push('\n');
        }
        out
    }
}

/// `$variables` of a key that differ from the default locale's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaceholderMismatch {
    /// The key.
    pub key: String,
    /// Variables the default locale's text uses.
    pub expected: Vec<String>,
    /// Variables this locale's text uses.
    pub found: Vec<String>,
}

/// What one locale lacks.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocaleReport {
    /// The locale tag.
    pub locale: String,
    /// The fallback chain used.
    pub chain: Vec<String>,
    /// Keys the locale defines itself.
    pub defined: usize,
    /// Keys that exist somewhere (defined or referenced) but not in this locale itself, sorted.
    pub missing: Vec<String>,
    /// Keys no locale in the chain can show, sorted. These render as the missing-key marker.
    pub unresolved: Vec<String>,
    /// Variables that differ from the default locale's text, sorted by key.
    pub placeholder_mismatches: Vec<PlaceholderMismatch>,
    /// Share of the key universe this locale defines itself, 0 to 1 (1 when there are no keys).
    pub coverage: f64,
    /// Locales its explicit chain ([`CompileOptions::chains`]) names that have no `.ftl` source
    /// (a typo falls through to the default); sorted. Not in the JSON when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unknown_chain_locales: Vec<String>,
}

/// A key that is referenced but defined nowhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UndefinedKey {
    /// The key.
    pub key: String,
    /// Where it is used.
    pub references: Vec<SourceLoc>,
}

/// The missing-key report for every locale.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LocalizationReport {
    /// Number of distinct keys (defined in any locale or referenced).
    pub total_keys: usize,
    /// One report per locale, by locale tag.
    pub locales: BTreeMap<String, LocaleReport>,
    /// Keys referenced but defined in no locale.
    pub undefined: Vec<UndefinedKey>,
    /// Keys defined but never referenced (a hint: runtime-built keys look unused).
    pub unused: Vec<String>,
}

impl LocalizationReport {
    /// Whether the report has nothing to fix: no unresolved keys, no undefined references and
    /// no placeholder mismatches. Missing keys that fall back, and unused keys, do not count.
    pub fn is_clean(&self) -> bool {
        self.undefined.is_empty()
            && self
                .locales
                .values()
                .all(|r| r.unresolved.is_empty() && r.placeholder_mismatches.is_empty())
    }

    /// The report as pretty JSON.
    pub fn to_json(&self) -> Result<String, UiError> {
        serde_json::to_string_pretty(self).map_err(|e| UiError::Localization(format!("report json: {e}")))
    }

    /// A short human-readable summary, one block per locale.
    pub fn to_text(&self) -> String {
        let mut out = format!("{} keys\n", self.total_keys);
        for report in self.locales.values() {
            out.push_str(&format!(
                "{}: {} defined ({:.1}%), {} missing, {} unresolved, {} placeholder mismatches\n",
                report.locale,
                report.defined,
                report.coverage * 100.0,
                report.missing.len(),
                report.unresolved.len(),
                report.placeholder_mismatches.len()
            ));
            for key in &report.missing {
                out.push_str(&format!("  missing {key}\n"));
            }
        }
        for u in &self.undefined {
            out.push_str(&format!("undefined {} ({} refs)\n", u.key, u.references.len()));
        }
        out
    }
}

/// The result of [`compile`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompileOutput {
    /// The default locale the chains end in.
    pub default_locale: String,
    /// One table per locale.
    pub tables: BTreeMap<String, CompiledTable>,
    /// Every key that exists (defined in any locale or referenced), sorted.
    pub universe: BTreeSet<String>,
    /// The missing-key report.
    pub report: LocalizationReport,
}

impl CompileOutput {
    /// Builds a runtime [`Localization`] from the tables (each flattened with its fallbacks),
    /// plus any `extra` `(locale, ftl)` sources such as the debug locales. The default locale is
    /// the fallback.
    pub fn to_localization(&self, extra: Vec<(String, String)>) -> Result<Localization, UiError> {
        let mut sources: Vec<(String, String)> = self
            .tables
            .values()
            .map(|t| (t.locale.clone(), t.to_ftl()))
            .collect();
        sources.extend(extra);
        Localization::new(sources, Some(self.default_locale.clone()))
    }
}

/// Compiles per-locale tables and the missing-key report from gathered sources.
///
/// The last definition of a key inside a locale wins (the same rule the runtime loader has when
/// it concatenates a locale's files in path order).
pub fn compile(gathered: &Gathered, options: &CompileOptions) -> Result<CompileOutput, CompileError> {
    let _span = tracing::debug_span!("loc_compile", locales = gathered.messages.len()).entered();
    if gathered.manifest.locales.is_empty() {
        return Err(CompileError::NoLocales);
    }
    if !gathered.manifest.locales.contains(&options.default_locale) {
        return Err(CompileError::UnknownDefaultLocale(options.default_locale.clone()));
    }
    // The last message per (locale, key).
    let mut native: BTreeMap<&str, BTreeMap<&str, &crate::loc_manifest::FtlMessage>> = BTreeMap::new();
    for (locale, messages) in &gathered.messages {
        let map = native.entry(locale.as_str()).or_default();
        for m in messages {
            map.insert(m.key.as_str(), m);
        }
    }
    let universe: BTreeSet<String> = gathered.manifest.keys.keys().cloned().collect();
    let empty = BTreeMap::new();
    let default_native = native.get(options.default_locale.as_str()).unwrap_or(&empty);

    let mut tables = BTreeMap::new();
    let mut locale_reports = BTreeMap::new();
    for locale in &gathered.manifest.locales {
        let chain = fallback_chain(locale, &options.default_locale, &options.chains);
        let mut entries = BTreeMap::new();
        // Walk the chain backwards so earlier locales overwrite later ones.
        for link in chain.iter().rev() {
            let Some(messages) = native.get(link.as_str()) else {
                continue;
            };
            for (key, m) in messages {
                entries.insert(
                    (*key).to_owned(),
                    TableEntry {
                        source: m.block.clone(),
                        origin_locale: link.clone(),
                        at: m.at.clone(),
                        variables: m.variables.clone(),
                        has_value: m.has_value,
                    },
                );
            }
        }
        let own = native.get(locale.as_str()).unwrap_or(&empty);
        let missing: Vec<String> = universe
            .iter()
            .filter(|k| !own.contains_key(k.as_str()))
            .cloned()
            .collect();
        let unresolved: Vec<String> = universe
            .iter()
            .filter(|k| !entries.contains_key(k.as_str()))
            .cloned()
            .collect();
        let mut placeholder_mismatches = Vec::new();
        if *locale != options.default_locale {
            for (key, m) in own {
                if let Some(reference) = default_native.get(key)
                    && reference.variables != m.variables
                {
                    placeholder_mismatches.push(PlaceholderMismatch {
                        key: (*key).to_owned(),
                        expected: reference.variables.iter().cloned().collect(),
                        found: m.variables.iter().cloned().collect(),
                    });
                }
            }
        }
        let coverage = if universe.is_empty() {
            1.0
        } else {
            own.len() as f64 / universe.len() as f64
        };
        let mut unknown_chain_locales: Vec<String> = options
            .chains
            .get(locale)
            .into_iter()
            .flatten()
            .filter(|link| !native.contains_key(link.as_str()))
            .cloned()
            .collect();
        unknown_chain_locales.sort();
        unknown_chain_locales.dedup();
        for link in &unknown_chain_locales {
            tracing::warn!(
                target: "regia_ui",
                "fallback chain of {locale} names {link}, which has no .ftl source"
            );
        }
        locale_reports.insert(
            locale.clone(),
            LocaleReport {
                locale: locale.clone(),
                chain: chain.clone(),
                defined: own.len(),
                missing,
                unresolved,
                placeholder_mismatches,
                coverage,
                unknown_chain_locales,
            },
        );
        tables.insert(
            locale.clone(),
            CompiledTable {
                locale: locale.clone(),
                chain,
                entries,
            },
        );
    }
    let report = LocalizationReport {
        total_keys: universe.len(),
        locales: locale_reports,
        undefined: gathered
            .manifest
            .undefined_keys()
            .into_iter()
            .map(|(key, refs)| UndefinedKey {
                key: key.to_owned(),
                references: refs.iter().map(|r| r.at.clone()).collect(),
            })
            .collect(),
        unused: gathered
            .manifest
            .unused_keys()
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };
    Ok(CompileOutput {
        default_locale: options.default_locale.clone(),
        tables,
        universe,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loc_manifest::{GatherSource, gather};

    fn no_chains() -> BTreeMap<String, Vec<String>> {
        BTreeMap::new()
    }

    fn sources() -> Vec<GatherSource> {
        vec![
            GatherSource::ftl(
                "en/a.ftl",
                "en",
                "start = Start\nquit = Quit\nhello = Hello, { $name }!\nonly-en = English only\n",
            ),
            GatherSource::ftl("fr/a.ftl", "fr", "start = Démarrer\nhello = Bonjour, { $nom }!\n"),
            GatherSource::ftl("fr-CA/a.ftl", "fr-CA", "start = Démarrer (CA)\n"),
            GatherSource::ui_asset("ui.ron", "(a: Loc(\"start\"), b: Loc(\"ghost\"))"),
        ]
    }

    #[test]
    fn chain_truncates_tags_and_ends_in_default() {
        assert_eq!(fallback_chain("pt-BR", "en", &no_chains()), ["pt-BR", "pt", "en"]);
        assert_eq!(
            fallback_chain("zh-Hant-TW", "en", &no_chains()),
            ["zh-Hant-TW", "zh-Hant", "zh", "en"]
        );
        assert_eq!(fallback_chain("en", "en", &no_chains()), ["en"]);
        assert_eq!(fallback_chain("fr", "en", &no_chains()), ["fr", "en"]);
    }

    #[test]
    fn explicit_chain_replaces_truncation_and_dedupes() {
        let mut explicit = BTreeMap::new();
        explicit.insert(
            "gl".to_owned(),
            vec!["es".to_owned(), "gl".to_owned(), "en".to_owned()],
        );
        assert_eq!(fallback_chain("gl", "en", &explicit), ["gl", "es", "en"]);
        // An explicit chain for one locale does not touch another.
        assert_eq!(fallback_chain("es-MX", "en", &explicit), ["es-MX", "es", "en"]);
    }

    #[test]
    fn tables_fall_back_along_the_chain() {
        let out = compile(&gather(&sources()), &CompileOptions::new("en")).expect("compile");
        let ca = &out.tables["fr-CA"];
        assert_eq!(ca.chain, ["fr-CA", "fr", "en"]);
        assert_eq!(ca.get("start").map(|e| e.origin_locale.as_str()), Some("fr-CA"));
        assert_eq!(ca.get("hello").map(|e| e.origin_locale.as_str()), Some("fr"));
        assert_eq!(ca.get("quit").map(|e| e.origin_locale.as_str()), Some("en"));
        assert!(ca.is_native("start"));
        assert!(!ca.is_native("quit"));
        assert!(ca.get("ghost").is_none(), "undefined everywhere");
    }

    #[test]
    fn report_lists_missing_unresolved_and_coverage() {
        let out = compile(&gather(&sources()), &CompileOptions::new("en")).expect("compile");
        assert_eq!(out.report.total_keys, 5);
        let fr = &out.report.locales["fr"];
        assert_eq!(fr.defined, 2);
        assert_eq!(fr.missing, ["ghost", "only-en", "quit"]);
        assert_eq!(fr.unresolved, ["ghost"]);
        assert!((fr.coverage - 0.4).abs() < 1e-9);
        let en = &out.report.locales["en"];
        assert_eq!(en.missing, ["ghost"]);
        assert_eq!(en.unresolved, ["ghost"]);
        assert_eq!(out.report.undefined.len(), 1);
        assert_eq!(out.report.undefined[0].key, "ghost");
        assert_eq!(out.report.undefined[0].references[0].line, 1);
        assert!(!out.report.is_clean());
    }

    #[test]
    fn placeholder_mismatch_is_reported_against_the_default_locale() {
        let out = compile(&gather(&sources()), &CompileOptions::new("en")).expect("compile");
        let fr = &out.report.locales["fr"];
        assert_eq!(fr.placeholder_mismatches.len(), 1);
        let m = &fr.placeholder_mismatches[0];
        assert_eq!(m.key, "hello");
        assert_eq!(m.expected, ["name"]);
        assert_eq!(m.found, ["nom"]);
        assert!(out.report.locales["en"].placeholder_mismatches.is_empty());
    }

    #[test]
    fn clean_report_when_everything_resolves() {
        let g = gather(&[
            GatherSource::ftl("en.ftl", "en", "a = 1\nb = 2\n"),
            GatherSource::ftl("de.ftl", "de", "a = eins\n"),
            GatherSource::ui_asset("u.ron", "(x: Loc(\"a\"), y: Loc(\"b\"))"),
        ]);
        let out = compile(&g, &CompileOptions::new("en")).expect("compile");
        assert!(out.report.is_clean(), "{}", out.report.to_text());
        assert_eq!(
            out.report.locales["de"].missing,
            ["b"],
            "falls back, still listed"
        );
        assert!(out.report.unused.is_empty());
    }

    #[test]
    fn last_definition_in_a_locale_wins() {
        let g = gather(&[
            GatherSource::ftl("en/a.ftl", "en", "x = first\n"),
            GatherSource::ftl("en/b.ftl", "en", "x = second\n"),
        ]);
        let out = compile(&g, &CompileOptions::new("en")).expect("compile");
        assert_eq!(
            out.tables["en"].get("x").map(|e| e.source.as_str()),
            Some("x = second")
        );
        assert_eq!(out.report.locales["en"].defined, 1);
    }

    #[test]
    fn errors_have_stable_codes() {
        let err = compile(&Gathered::default(), &CompileOptions::new("en")).expect_err("empty");
        assert_eq!(err, CompileError::NoLocales);
        assert_eq!(err.code(), "no_locales");
        let g = gather(&[GatherSource::ftl("a.ftl", "fr", "a = 1\n")]);
        let err = compile(&g, &CompileOptions::new("en")).expect_err("no en");
        assert_eq!(err.code(), "unknown_default_locale");
        assert!(err.to_string().contains("\"en\""));
    }

    #[test]
    fn compile_is_deterministic() {
        let a = compile(&gather(&sources()), &CompileOptions::new("en")).expect("a");
        let mut reversed = sources();
        reversed.reverse();
        let b = compile(&gather(&reversed), &CompileOptions::new("en")).expect("b");
        assert_eq!(a, b);
        assert_eq!(
            a.report.to_json().expect("json"),
            b.report.to_json().expect("json")
        );
    }

    #[test]
    fn compiled_tables_load_into_a_runtime_localization() {
        let out = compile(&gather(&sources()), &CompileOptions::new("en")).expect("compile");
        assert!(out.tables["fr"].to_ftl().contains("quit = Quit"));
        let mut loc = out.to_localization(Vec::new()).expect("localization");
        assert!(loc.set_locale("fr-CA"));
        assert_eq!(loc.resolve("start", &[]), "Démarrer (CA)");
        assert_eq!(loc.resolve("quit", &[]), "Quit", "flattened fallback");
        assert_eq!(
            loc.resolve("hello", &[("nom".to_owned(), "Zoé".to_owned())]),
            "Bonjour, Zoé!"
        );
    }

    #[test]
    fn multiline_and_select_messages_survive_flattening() {
        let en = "items = { $n ->\n    [one] one item\n   *[other] { $n } items\n    }\nlong =\n    line one\n    line two\n";
        let g = gather(&[GatherSource::ftl("en.ftl", "en", en)]);
        let out = compile(&g, &CompileOptions::new("en")).expect("compile");
        let loc = out.to_localization(Vec::new()).expect("localization");
        assert_eq!(
            loc.resolve("items", &[("n".to_owned(), "1".to_owned())]),
            "one item"
        );
        assert_eq!(
            loc.resolve("items", &[("n".to_owned(), "5".to_owned())]),
            "5 items"
        );
        assert_eq!(loc.resolve("long", &[]), "line one\nline two");
    }

    #[test]
    fn report_text_and_json() {
        let out = compile(&gather(&sources()), &CompileOptions::new("en")).expect("compile");
        let text = out.report.to_text();
        assert!(text.starts_with("5 keys\n"));
        assert!(text.contains("fr: 2 defined (40.0%), 3 missing, 1 unresolved, 1 placeholder mismatches"));
        assert!(text.contains("undefined ghost (1 refs)"));
        let v: serde_json::Value = serde_json::from_str(&out.report.to_json().expect("json")).expect("parse");
        assert_eq!(v["locales"]["fr"]["unresolved"][0], "ghost");
    }

    #[test]
    fn explicit_chain_option_is_used() {
        let g = gather(&[
            GatherSource::ftl("en.ftl", "en", "a = en\nb = en\n"),
            GatherSource::ftl("es.ftl", "es", "a = es\n"),
            GatherSource::ftl("gl.ftl", "gl", "c = gl\n"),
        ]);
        let out = compile(&g, &CompileOptions::new("en").with_chain("gl", &["es"])).expect("compile");
        let gl = &out.tables["gl"];
        assert_eq!(gl.chain, ["gl", "es", "en"]);
        assert_eq!(gl.get("a").map(|e| e.origin_locale.as_str()), Some("es"));
        assert_eq!(gl.get("b").map(|e| e.origin_locale.as_str()), Some("en"));
        assert!(out.report.locales["gl"].unknown_chain_locales.is_empty());
    }

    #[test]
    fn an_explicit_chain_naming_an_unknown_locale_is_reported() {
        // A typo in a chain used to fall through to the default with no word (ADR-1660).
        let g = gather(&[
            GatherSource::ftl("en.ftl", "en", "a = en\n"),
            GatherSource::ftl("es.ftl", "es", "a = es\n"),
            GatherSource::ftl("gl.ftl", "gl", "c = gl\n"),
        ]);
        let options = CompileOptions::new("en").with_chain("gl", &["es_typo", "es"]);
        let out = compile(&g, &options).expect("compile");
        let gl = &out.report.locales["gl"];
        assert_eq!(gl.unknown_chain_locales, ["es_typo"]);
        assert_eq!(
            out.tables["gl"].get("a").map(|e| e.origin_locale.as_str()),
            Some("es")
        );
        let v: serde_json::Value = serde_json::from_str(&out.report.to_json().expect("json")).expect("parse");
        assert_eq!(v["locales"]["gl"]["unknown_chain_locales"][0], "es_typo");
        // Locales without the problem keep their JSON shape (no empty list).
        assert!(v["locales"]["en"].get("unknown_chain_locales").is_none());
        assert!(out.report.locales["en"].unknown_chain_locales.is_empty());
    }
}
