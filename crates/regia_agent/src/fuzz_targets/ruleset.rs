// SPDX-License-Identifier: MPL-2.0
//! Fuzz target `ruleset_ron` (ADR-8694): `.rules.ron` rule sets are untrusted (mods and downloaded
//! content), so the reader, the check and the resolution must refuse anything without a panic.
//!
//! The seed is the corpus fixture with every genre section (`rules-ron/v1-sections.ron`, ADR-8748), so
//! the mutations reach the newest section types and checks; `rules-ron/v1.ron` (extends, teams,
//! options) stays covered by the corpus test and the reader's unit tests.
//!
//! The load parses the text (`regia_framework::ruleset_file::parse_rules_ron`: the size cap, the
//! version peek, the typed parse), resolves it over the shipped defaults with no row options and with
//! a few hostile ones, and writes the canonical RON back for the round-trip check.

use std::collections::BTreeMap;

use regia_framework::rules::ruleset::RULES_RON_FORMAT_VERSION;
use regia_framework::ruleset_file::{RuleSetRegistry, parse_rules_ron, rules_ron_text};

use super::FuzzTarget;
use super::formats::text;

fn load(text: &str) -> Result<String, String> {
    let set = parse_rules_ron(text).map_err(|e| e.to_string())?;
    let mut library = RuleSetRegistry::with_defaults().library().clone();
    library.replace(set.clone()).map_err(|e| e.to_string())?;
    // Resolution may refuse (a missing parent, a cycle, a bad option); it must never panic.
    let _ = library.resolve(&set.id, &BTreeMap::new());
    let hostile: BTreeMap<String, String> = set
        .options
        .iter()
        .map(|o| (o.key.clone(), "1e39".to_owned()))
        .chain([("unknown_key".to_owned(), "NaN".to_owned())])
        .collect();
    let _ = library.resolve(&set.id, &hostile);
    rules_ron_text(&set)
}

pub(super) fn targets() -> Vec<FuzzTarget> {
    vec![text(
        "ruleset_ron",
        Some("rules-ron"),
        RULES_RON_FORMAT_VERSION,
        true,
        || include_str!("../../tests/fixtures/format-corpus/rules-ron/v1-sections.ron").into(),
        load,
    )]
}
