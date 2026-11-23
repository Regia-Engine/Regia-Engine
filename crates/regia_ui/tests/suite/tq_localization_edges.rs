// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): the runtime localization's boundaries: fallback chains with
//! cycles, empty and broken sources, plural routing, argument typing, locale switching and pseudo
//! mode. Headless; no goldens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};

use regia_ui::Localization;
use regia_ui::localization::fallback_chain;

fn set(tags: &[&str]) -> BTreeSet<String> {
    tags.iter().map(|t| (*t).to_owned()).collect()
}

fn loc(sources: &[(&str, &str)], fallback: &str) -> Localization {
    Localization::new(
        sources
            .iter()
            .map(|(l, s)| ((*l).to_owned(), (*s).to_owned()))
            .collect(),
        Some(fallback.to_owned()),
    )
    .expect("localization")
}

fn args(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn missing(key: &str) -> String {
    if cfg!(debug_assertions) {
        format!("\u{27E6}{key}\u{27E7}")
    } else {
        key.to_owned()
    }
}

#[test]
fn fallback_chain_orders_extras_before_parents_then_source() {
    let extra: BTreeMap<String, Vec<String>> = [("fr-CA".to_owned(), vec!["en-GB".to_owned()])].into();
    let loaded = set(&["en", "en-GB", "fr", "fr-CA"]);
    assert_eq!(
        fallback_chain("fr-CA", &loaded, &extra, "en"),
        ["fr-CA", "en-GB", "en", "fr"]
    );
    // A locale that is not loaded is skipped, but its parents still count.
    let loaded = set(&["en", "fr"]);
    assert_eq!(
        fallback_chain("fr-CA", &loaded, &BTreeMap::new(), "en"),
        ["fr", "en"]
    );
    // The source is last, even when the locale is the source.
    assert_eq!(
        fallback_chain("en", &set(&["en"]), &BTreeMap::new(), "en"),
        ["en"]
    );
    // Nothing loaded: nothing to try.
    assert!(fallback_chain("fr", &set(&[]), &BTreeMap::new(), "en").is_empty());
    // Empty tags are ignored.
    assert_eq!(fallback_chain("", &set(&["en"]), &BTreeMap::new(), "en"), ["en"]);
}

#[test]
fn fallback_chain_survives_cycles_and_self_edges() {
    let extra: BTreeMap<String, Vec<String>> = [
        ("a".to_owned(), vec!["b".to_owned(), "a".to_owned()]),
        ("b".to_owned(), vec!["c".to_owned()]),
        ("c".to_owned(), vec!["a".to_owned(), "b".to_owned()]),
    ]
    .into();
    let loaded = set(&["a", "b", "c", "en"]);
    let chain = fallback_chain("a", &loaded, &extra, "en");
    assert_eq!(
        chain,
        ["a", "b", "c", "en"],
        "each locale once, in discovery order"
    );
    // Deterministic.
    assert_eq!(chain, fallback_chain("a", &loaded, &extra, "en"));
}

#[test]
fn construction_errors_name_the_locale() {
    let bad_tag = Localization::new(vec![("not a tag".to_owned(), "a = A\n".to_owned())], None);
    assert!(bad_tag.err().unwrap().to_string().contains("bad locale"));
    let bad_ftl = Localization::new(vec![("de".to_owned(), "a = { $x\n".to_owned())], None);
    let message = bad_ftl.err().unwrap().to_string();
    assert!(message.contains("de"), "{message}");
}

#[test]
fn an_empty_localization_resolves_to_the_missing_marker_without_panicking() {
    let mut empty = Localization::new(Vec::new(), None).expect("empty");
    assert_eq!(empty.resolve("anything", &[]), missing("anything"));
    assert_eq!(empty.locale(), "en");
    assert!(!empty.has("anything"));
    assert_eq!(empty.provider_of("anything"), None);
    assert!(empty.chain("fr").is_empty());
    assert_eq!(empty.cycle_locale(), "en");
    assert!(!empty.set_locale("en"), "not loaded");
    assert!(empty.keys_in("en").is_empty());
    assert_eq!(empty.self_name("en"), None);
}

#[test]
fn an_unloaded_fallback_falls_back_to_the_first_locale() {
    let l = loc(&[("fr", "a = A\n"), ("de", "a = B\n")], "ja");
    assert_eq!(
        l.fallback(),
        "de",
        "available locales are sorted; the first is used"
    );
    assert_eq!(l.locale(), "de");
}

#[test]
fn set_locale_picks_the_nearest_parent_and_counts_real_changes_only() {
    let mut l = loc(&[("en", "a = A\n"), ("fr", "a = Á\n")], "en");
    let g0 = l.generation.0;
    assert!(l.set_locale("fr-CA"), "fr-CA selects fr");
    assert_eq!(l.locale(), "fr");
    assert_ne!(l.generation.0, g0);
    let g1 = l.generation.0;
    assert!(!l.set_locale("fr"), "already selected");
    assert!(!l.set_locale("fr-CA"), "resolves to the selected locale");
    assert!(!l.set_locale("de"), "not loaded");
    assert!(!l.set_locale(""), "empty tag");
    assert!(!l.set_locale("-"), "malformed tag");
    assert_eq!(l.locale(), "fr");
    assert_eq!(l.generation.0, g1, "refused switches do not bump the generation");
    assert_eq!(l.nearest_loaded("en-US-x-foo").as_deref(), Some("en"));
    assert_eq!(l.nearest_loaded("zz"), None);
}

#[test]
fn cycling_wraps_and_a_single_locale_stays_put() {
    let mut l = loc(&[("aa", "k = 1\n"), ("bb", "k = 2\n"), ("cc", "k = 3\n")], "aa");
    let seen: Vec<String> = (0..4).map(|_| l.cycle_locale().to_owned()).collect();
    assert_eq!(seen, ["bb", "cc", "aa", "bb"]);
    let mut one = loc(&[("en", "k = 1\n")], "en");
    let g = one.generation.0;
    assert_eq!(one.cycle_locale(), "en");
    assert_eq!(one.generation.0, g, "nothing changed");
}

#[test]
fn resolution_falls_back_per_key_and_reports_the_provider() {
    let mut l = loc(
        &[
            ("en", "only-en = EN\nboth = EN both\n"),
            ("fr", "both = FR both\n"),
        ],
        "en",
    );
    assert!(l.set_locale("fr"));
    assert_eq!(l.resolve("both", &[]), "FR both");
    assert_eq!(l.resolve("only-en", &[]), "EN");
    assert_eq!(l.resolve("nope", &[]), missing("nope"));
    assert_eq!(l.provider_of("both").as_deref(), Some("fr"));
    assert_eq!(l.provider_of("only-en").as_deref(), Some("en"));
    assert!(l.has("only-en") && !l.has("nope"));
    assert!(l.has_in("fr", "both") && !l.has_in("fr", "only-en") && !l.has_in("zz", "both"));
    assert_eq!(
        l.resolve_in("fr", "only-en", &[]),
        None,
        "resolve_in never falls back"
    );
    assert_eq!(l.resolve_in("zz", "both", &[]), None);
    assert_eq!(l.keys_in("fr"), ["both"]);
    assert!(l.keys_in("zz").is_empty());
    // Extra fallback edges apply to the chain and bump the generation once.
    let g = l.generation.0;
    let edges: BTreeMap<String, Vec<String>> = [("fr".to_owned(), vec!["en".to_owned()])].into();
    l.set_fallbacks(edges.clone());
    l.set_fallbacks(edges);
    assert_eq!(l.generation.0, g + 1);
}

#[test]
fn arguments_are_numbers_only_when_they_print_back_exactly() {
    let l = loc(
        &[(
            "en",
            "show = { $v }\npick = { $v ->\n    [1] one-exact\n   *[other] other-text\n}\n",
        )],
        "en",
    );
    for text in [
        "007",
        "1e3",
        "abc",
        " 1",
        "+1",
        "1.",
        ".5",
        "NaN",
        "inf",
        "18446744073709551615",
    ] {
        assert_eq!(
            l.resolve("show", &args(&[("v", text)])),
            text,
            "{text:?} is kept as written"
        );
    }
    assert_eq!(l.resolve("show", &args(&[("v", "-1.5")])), "-1.5");
    assert_eq!(
        l.resolve("pick", &args(&[("v", "1")])),
        "one-exact",
        "a canonical number matches [1]"
    );
    assert_eq!(
        l.resolve("pick", &args(&[("v", "01")])),
        "other-text",
        "01 is text, not 1"
    );
    // A missing argument does not panic.
    let _ = l.resolve("show", &[]);
}

#[test]
fn plural_selects_use_the_cldr_table_for_every_locale() {
    let ar = "n = { $count ->\n    [zero] z\n    [one] o\n    [two] t\n    [few] f\n    [many] m\n   *[other] x\n}\n";
    let l = loc(&[("ar", ar)], "ar");
    for (count, want) in [
        ("0", "z"),
        ("1", "o"),
        ("2", "t"),
        ("3", "f"),
        ("10", "f"),
        ("11", "m"),
        ("99", "m"),
        ("100", "x"),
        ("103", "f"),
        ("111", "m"),
        ("1.5", "x"),
    ] {
        assert_eq!(l.resolve("n", &args(&[("count", count)])), want, "ar {count}");
    }
    // An exact number beats the category, which still works for the rest.
    let en = "n = { $count ->\n    [0] none\n    [one] one\n   *[other] many\n}\n";
    let l = loc(&[("en", en)], "en");
    for (count, want) in [
        ("0", "none"),
        ("1", "one"),
        ("2", "many"),
        ("1.0", "many"),
        ("-1", "one"),
    ] {
        assert_eq!(l.resolve("n", &args(&[("count", count)])), want, "en {count}");
    }
    // A text argument in a plural select takes the default branch.
    assert_eq!(l.resolve("n", &args(&[("count", "lots")])), "many");
}

#[test]
fn pseudo_mode_toggles_once_wraps_resolved_text_and_is_reversible() {
    let mut l = loc(&[("en", "hi = Hello { $name }\n")], "en");
    let plain = l.resolve("hi", &args(&[("name", "Zoé")]));
    assert_eq!(plain, "Hello Zoé");
    let g = l.generation.0;
    assert!(l.set_pseudo(true));
    assert!(!l.set_pseudo(true), "already on");
    assert_eq!(l.generation.0, g + 1);
    assert!(l.pseudo());
    let pseudo = l.resolve("hi", &args(&[("name", "Zoé")]));
    assert!(pseudo.starts_with('[') && pseudo.ends_with(']'), "{pseudo}");
    assert!(pseudo.contains("Zoé"), "placeable values are kept: {pseudo}");
    assert_ne!(pseudo, plain);
    assert!(l.set_pseudo(false));
    assert_eq!(
        l.resolve("hi", &args(&[("name", "Zoé")])),
        plain,
        "off again gives the original text"
    );
}

#[test]
fn source_text_and_validation_follow_the_loaded_sources() {
    let l = loc(&[("en", "a = A\nb = B\n"), ("fr", "a = Á\n")], "en");
    assert_eq!(l.source_text("fr"), Some("a = Á\n"));
    assert_eq!(l.source_text("zz"), None);
    let report = l.validate();
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.locale == "fr" && f.key.as_deref() == Some("b")),
        "fr lacks b: {:#?}",
        report.findings
    );
}
