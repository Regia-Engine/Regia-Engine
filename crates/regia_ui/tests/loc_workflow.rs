// SPDX-License-Identifier: MPL-2.0
//! Localization workflow (`docs/specs/content-data.md` § 1, ADR-1466): the worked example (a menu
//! in three locales with a plural), the plural table against the Fluent runtime, the fallback
//! chain, the locale-switch message, pseudo mode, gather and every validator code.

use std::collections::{BTreeMap, BTreeSet};

use regia_app::prelude::*;
use regia_ui::loc_check::{self, CheckOptions, Severity};
use regia_ui::loc_config::{GatherConfig, LocalizationConfig};
use regia_ui::loc_gather;
use regia_ui::{LocaleChanged, Localization, LocalizationPlugin, plural};

const EN: &str = "menu-start = Start Game
menu-quit = Quit
menu-saves = { $count ->
    [one] One saved game
   *[other] { $count } saved games
}
";
const FR: &str = "menu-start = Commencer
menu-saves = { $count ->
    [one] { $count } partie sauvegardée
   *[other] { $count } parties sauvegardées
}
";
const FR_CA: &str = "menu-start = Démarrer la partie\n";
const RU: &str = "menu-start = Начать игру
menu-quit = Выход
menu-saves = { $count ->
    [one] { $count } сохранение
    [few] { $count } сохранения
    [many] { $count } сохранений
   *[other] { $count } сохранения
}
";

fn s(v: &str) -> String {
    v.to_owned()
}

fn menu() -> Localization {
    Localization::new(
        vec![
            (s("en"), s(EN)),
            (s("fr"), s(FR)),
            (s("fr-CA"), s(FR_CA)),
            (s("ru"), s(RU)),
        ],
        Some(s("en")),
    )
    .expect("menu locales")
}

fn count(n: &str) -> Vec<(String, String)> {
    vec![(s("count"), s(n))]
}

#[test]
fn worked_example_menu_in_three_locales_with_a_plural() {
    let mut l = menu();
    assert_eq!(l.resolve("menu-saves", &count("1")), "One saved game");
    assert_eq!(l.resolve("menu-saves", &count("4")), "4 saved games");
    assert!(l.set_locale("fr"));
    // French: 0 and 1 are both `one`.
    assert_eq!(l.resolve("menu-saves", &count("0")), "0 partie sauvegardée");
    assert_eq!(l.resolve("menu-saves", &count("2")), "2 parties sauvegardées");
    // fr has no `menu-quit`: the chain falls back to English.
    assert_eq!(l.resolve("menu-quit", &[]), "Quit");
    assert!(l.set_locale("ru"));
    assert_eq!(l.resolve("menu-saves", &count("1")), "1 сохранение");
    assert_eq!(l.resolve("menu-saves", &count("3")), "3 сохранения");
    assert_eq!(l.resolve("menu-saves", &count("5")), "5 сохранений");
    assert_eq!(l.resolve("menu-saves", &count("21")), "21 сохранение");
}

#[test]
fn fallback_chain_walks_parents_then_the_source() {
    let mut l = menu();
    assert_eq!(l.chain("fr-CA"), vec![s("fr-CA"), s("fr"), s("en")]);
    assert!(l.set_locale("fr-CA"));
    assert_eq!(l.resolve("menu-start", &[]), "Démarrer la partie");
    assert_eq!(l.resolve("menu-saves", &count("2")), "2 parties sauvegardées");
    assert_eq!(l.resolve("menu-quit", &[]), "Quit");
    assert_eq!(l.provider_of("menu-saves").as_deref(), Some("fr"));
    // A locale that is not loaded selects its nearest loaded parent.
    assert!(l.set_locale("ru-RU"));
    assert_eq!(l.locale(), "ru");
    assert!(!l.set_locale("ru-UA"), "same locale again is not a change");
    // Extra edges come before parents.
    let mut fallbacks = BTreeMap::new();
    fallbacks.insert(s("fr-CA"), vec![s("ru")]);
    l.set_fallbacks(fallbacks);
    assert_eq!(l.chain("fr-CA"), vec![s("fr-CA"), s("ru"), s("fr"), s("en")]);
}

#[test]
fn locale_switch_sends_one_message() {
    let mut app = App::new();
    app.add_plugins(LocalizationPlugin);
    app.insert_resource(menu());
    app.update();
    app.world_mut().resource_mut::<Localization>().set_locale("fr");
    app.update();
    // A pseudo toggle bumps the generation but is not a locale change.
    app.world_mut().resource_mut::<Localization>().set_pseudo(true);
    app.update();
    let messages = app.world().resource::<Messages<LocaleChanged>>();
    let mut cursor = messages.get_cursor();
    let seen: Vec<LocaleChanged> = cursor.read(messages).cloned().collect();
    assert_eq!(
        seen,
        vec![LocaleChanged {
            from: s("en"),
            to: s("fr"),
            generation: 1
        }]
    );
}

#[test]
fn pseudo_mode_accents_pads_and_brackets_but_keeps_values() {
    let mut l = menu();
    assert!(l.set_pseudo(true));
    assert_eq!(l.resolve("menu-start", &[]), "[Šŧáŕŧ Ĝáɱé~~~~]");
    let saves = l.resolve("menu-saves", &count("3"));
    assert!(saves.starts_with("[3 "), "{saves}");
    assert!(saves.ends_with(']'), "{saves}");
    assert!(saves.contains('~'), "{saves}");
    assert!(l.set_pseudo(false));
    assert_eq!(l.resolve("menu-start", &[]), "Start Game");
}

/// Every category name as a variant, so the resolved text names the category Fluent picked.
const PROBE: &str = "c = { $n ->
    [zero] zero
    [one] one
    [two] two
    [few] few
    [many] many
   *[other] other
}
";

const LOCALES: [&str; 23] = [
    "en", "fr", "de", "es", "it", "pt", "nl", "sv", "ru", "uk", "be", "pl", "cs", "ar", "he", "ja", "zh",
    "ko", "tr", "da", "nb", "no", "nn",
];

fn probe_numbers() -> impl Iterator<Item = String> {
    // "1.0" is not here. Rust prints it as "1", so `resolve` does not treat the text as a
    // number, and a bare f64 drops the trailing zero. The decimal tests below call
    // `category_of_text` with the written form.
    const DECIMALS: [&str; 10] = [
        "0.5", "1.5", "2.5", "3.5", "10.5", "11.5", "21.25", "100.5", "0.1", "1.25",
    ];
    (0..=1000u32)
        .map(|n| n.to_string())
        .chain(DECIMALS.into_iter().map(s))
}

#[test]
fn players_see_the_cldr_table_categories() {
    for locale in LOCALES {
        let l = Localization::new(vec![(s(locale), s(PROBE))], Some(s(locale))).expect("probe");
        for n in probe_numbers() {
            let shown = l.resolve("c", &[(s("n"), n.clone())]);
            let ours = plural::category_of_text(locale, &n).expect("number");
            assert_eq!(shown, ours.as_str(), "{locale} {n}");
            let used = plural::categories(locale).expect("known");
            assert!(used.contains(&ours), "{locale} {n}: {ours} not in {used:?}");
        }
    }
}

#[test]
fn exact_number_variants_still_win_over_categories() {
    let l = Localization::new(
        vec![(
            s("ar"),
            s("c = { $n ->\n    [0] none\n    [few] few\n   *[other] other\n}\n"),
        )],
        Some(s("ar")),
    )
    .expect("ar");
    assert_eq!(l.resolve("c", &[(s("n"), s("0"))]), "none");
    assert_eq!(l.resolve("c", &[(s("n"), s("103"))]), "few");
    assert_eq!(l.resolve("c", &[(s("n"), s("100"))]), "other");
    // Selectors that are not plural (no category variant) are left alone.
    let l = Localization::new(
        vec![(s("en"), s("g = { $who ->\n    [her] her\n   *[other] their\n}\n"))],
        Some(s("en")),
    )
    .expect("en");
    assert_eq!(l.resolve("g", &[(s("who"), s("her"))]), "her");
}

/// The upstream rule data (`intl_pluralrules` 7.0.2, used by `fluent-bundle` 0.16) agrees with
/// Regia's table except two generator bugs (ADR-1466, `docs/specs/content-data.md` § 1.5): Arabic
/// `few`/`many` drop `n % 100`, and Belarusian `few` tests the whole integer `i` against `2..=4`
/// instead of `n % 10`. If this test fails after a dependency bump, the upstream data changed:
/// check the table against CLDR.
#[test]
fn upstream_rules_differ_only_on_the_known_arabic_cases() {
    use fluent_bundle::concurrent::FluentBundle;
    use fluent_bundle::{FluentArgs, FluentResource, FluentValue};
    let mut arabic_differences = 0usize;
    let mut belarusian_differences = 0usize;
    for locale in LOCALES {
        let langid: unic_langid::LanguageIdentifier = locale.parse().expect("tag");
        let mut bundle = FluentBundle::new_concurrent(vec![langid]);
        bundle.set_use_isolating(false);
        bundle.add_resource_overriding(FluentResource::try_new(s(PROBE)).expect("probe"));
        let pattern = bundle.get_message("c").and_then(|m| m.value()).expect("c");
        for n in probe_numbers() {
            let mut args = FluentArgs::new();
            args.set("n", FluentValue::from(n.parse::<f64>().expect("f64")));
            let mut errors = Vec::new();
            let upstream = bundle
                .format_pattern(pattern, Some(&args), &mut errors)
                .into_owned();
            let ours = plural::category_of_text(locale, &n).expect("number");
            if locale == "ar" {
                let reading = upstream_arabic(&n);
                assert_eq!(upstream, reading, "ar {n}: upstream changed? update ADR-1466");
                if reading != ours.as_str() {
                    arabic_differences += 1;
                }
            } else if locale == "be" {
                let reading = upstream_belarusian(&n);
                assert_eq!(upstream, reading, "be {n}: upstream changed? update § 1.5");
                if reading != ours.as_str() {
                    belarusian_differences += 1;
                }
            } else {
                assert_eq!(upstream, ours.as_str(), "{locale} {n}");
            }
        }
    }
    // 103..=110, 111..=199, ... and the decimals 3.5, 10.5, 11.5, 21.25.
    assert!(arabic_differences > 800, "{arabic_differences}");
    // Integers whose last digit is 2..=4 above 4, and decimals Fluent classifies from `i`.
    assert!(belarusian_differences > 0, "{belarusian_differences}");
    assert_eq!(upstream_belarusian("22"), "other");
    assert_eq!(plural::category_of_text("be", "22").expect("22").as_str(), "few");
    assert_eq!(upstream_belarusian("1.0"), "one");
    assert_eq!(upstream_belarusian("1.5"), "one");
    assert_eq!(
        plural::category_of_text("be", "1.5").expect("1.5").as_str(),
        "other"
    );
}

/// `nb`, `no` and `nn` use CLDR `n = 1`, the same rule Fluent 7.0.2 ships, so `1.0` is `one`.
#[test]
fn norwegian_one_point_zero_matches_fluent() {
    for tag in ["nb", "no", "nn"] {
        assert_eq!(
            plural::category_of_text(tag, "1"),
            Some(plural::PluralCategory::One),
            "{tag}"
        );
        assert_eq!(
            plural::category_of_text(tag, "1.0"),
            Some(plural::PluralCategory::One),
            "{tag}"
        );
        assert_eq!(
            plural::category_of_text(tag, "1.00"),
            Some(plural::PluralCategory::One),
            "{tag}"
        );
        assert_eq!(
            plural::category_of_text(tag, "2"),
            Some(plural::PluralCategory::Other),
            "{tag}"
        );
    }
}

/// Belarusian cardinals follow `n % 10` / `n % 100` with no `v = 0` gate. Russian decimals stay `other`.
#[test]
fn belarusian_decimals_follow_the_published_rule() {
    assert_eq!(
        plural::category_of_text("be", "1.0"),
        Some(plural::PluralCategory::One)
    );
    assert_eq!(
        plural::category_of_text("be", "2.0"),
        Some(plural::PluralCategory::Few)
    );
    assert_eq!(
        plural::category_of_text("be", "22"),
        Some(plural::PluralCategory::Few)
    );
    assert_eq!(
        plural::category_of_text("be", "22.0"),
        Some(plural::PluralCategory::Few)
    );
    assert_eq!(
        plural::category_of_text("be", "13"),
        Some(plural::PluralCategory::Many)
    );
    assert_eq!(
        plural::category_of_text("be", "1.5"),
        Some(plural::PluralCategory::Other)
    );
    assert_eq!(
        plural::category_of_text("ru", "1.0"),
        Some(plural::PluralCategory::Other)
    );
    assert_eq!(
        plural::category_of_text("uk", "22.0"),
        Some(plural::PluralCategory::Other)
    );
}

/// The upstream reading of Belarusian (`intl_pluralrules` 7.0.2): `few` is the whole integer
/// `i` in `2..=4`, and `many` mixes `i % 10 == 0` with whole-integer ranges. CLDR uses `n % 10`
/// and `n % 100` with no `v = 0` gate.
fn upstream_belarusian(n: &str) -> &'static str {
    let o = plural::PluralOperands::from_decimal(n).expect("number");
    if (2..=4).contains(&o.i) && !(12..=14).contains(&o.i) {
        "few"
    } else if o.i.is_multiple_of(10) || (5..=9).contains(&o.i) || (11..=14).contains(&o.i) {
        "many"
    } else if o.i % 10 == 1 && o.i % 100 != 11 {
        "one"
    } else {
        "other"
    }
}

/// The upstream reading of Arabic: `few` / `many` test the integer digits `i` for `3..=10` /
/// `11..=99`, where CLDR tests `n % 100`.
fn upstream_arabic(n: &str) -> &'static str {
    let o = plural::PluralOperands::from_decimal(n).expect("number");
    if (3..=10).contains(&o.i) {
        "few"
    } else if (11..=99).contains(&o.i) {
        "many"
    } else if o.n == 1.0 {
        "one"
    } else if o.n == 2.0 {
        "two"
    } else if o.n == 0.0 {
        "zero"
    } else {
        "other"
    }
}

fn parsed(tag: &str, text: &str) -> (loc_check::ParsedLocale, Vec<(String, String)>) {
    let files = vec![(format!("assets/localization/{tag}/main.ftl"), s(text))];
    (loc_check::parse_locale(tag, &files), files)
}

#[test]
fn validator_reports_every_code() {
    let en = "hello = Hello { $name }\nitems = { $count ->\n    [one] One\n    [few] Few\n   *[other] Many\n}\nunused = Never used\ndup = A\ndup = B\n";
    let de = "hello = Hallo { $player }\nitems = { $count ->\n    [one] Eins\n   *[other] Viele\n}\nextra = Nur hier\n";
    let ru = "hello = Привет { $name }\nitems = { $count ->\n    [one] Один\n   *[other] Много\n}\n";
    let bad = "broken = { $\n";
    let locales = vec![
        parsed("en", en).0,
        parsed("de", de).0,
        parsed("ru", ru).0,
        parsed("pl", bad).0,
    ];
    let used: BTreeSet<String> = ["hello", "items", "dup", "menu-new"].into_iter().map(s).collect();
    let report = loc_check::validate(
        &locales,
        &CheckOptions {
            source: s("en"),
            used_keys: Some(used),
            ..CheckOptions::default()
        },
    );
    let codes: BTreeSet<&str> = report.findings.iter().map(|f| f.code).collect();
    for code in [
        "loc_parse_error",
        "loc_duplicate_key",
        "loc_missing_key",
        "loc_untranslated",
        "loc_unused_key",
        "loc_extra_key",
        "loc_variable_mismatch",
        "loc_plural_missing",
        "loc_plural_unreachable",
        "loc_key_set_differs",
    ] {
        assert!(
            codes.contains(code),
            "{code} not reported: {:#?}",
            report.findings
        );
    }
    // `menu-new` is used but defined nowhere: missing in every locale.
    assert_eq!(
        report
            .with_code("loc_missing_key")
            .filter(|f| f.key.as_deref() == Some("menu-new"))
            .count(),
        4
    );
    // de swapped $name for $player: one error (added) and one warning (dropped).
    let mismatch: Vec<_> = report.with_code("loc_variable_mismatch").collect();
    assert_eq!(mismatch.len(), 2);
    assert!(
        mismatch
            .iter()
            .any(|f| f.severity == Severity::Error && f.message.contains("$player"))
    );
    // Plural: ru lacks few and many; en's [few] is unreachable; de is complete for its rules.
    let plural_missing: Vec<_> = report
        .with_code("loc_plural_missing")
        .map(|f| f.locale.as_str())
        .collect();
    assert_eq!(plural_missing, vec!["ru"]);
    let unused: Vec<_> = report
        .with_code("loc_unused_key")
        .filter_map(|f| f.key.as_deref())
        .collect();
    assert_eq!(unused, vec!["unused"]);
    let de_line = report.with_code("loc_extra_key").next().expect("extra");
    assert_eq!((de_line.locale.as_str(), de_line.line), ("de", Some(6)));
    assert!(report.fails(false));
    let de_summary = report.locales.iter().find(|l| l.locale == "de").expect("de");
    assert_eq!(de_summary.chain, vec![s("de"), s("en")]);
}

/// The two § 1.6 codes added after the first fixture (ADR-1618 `loc_no_self_name`,
/// `loc_fallback_cycle`) come out of `validate` itself, as warnings that `--strict` fails on
/// (spec-43 audit, ADR-5662).
#[test]
fn validator_reports_the_self_name_and_fallback_cycle_codes() {
    let en = parsed("en", "locale-name = English\nhello = Hello\n").0;
    let fr = parsed("fr", "locale-name = Français\nhello = Salut\n").0;
    let pt = parsed("pt", "hello = Olá\n").0;
    let es = parsed("es", "locale-name = Español\nhello = Hola\n").0;
    let mut fallbacks = BTreeMap::new();
    fallbacks.insert(s("pt"), vec![s("es")]);
    fallbacks.insert(s("es"), vec![s("pt")]);
    let report = loc_check::validate(
        &[en, fr, pt, es],
        &CheckOptions {
            source: s("en"),
            fallbacks,
            ..CheckOptions::default()
        },
    );
    let self_name: Vec<_> = report.with_code("loc_no_self_name").collect();
    assert_eq!(self_name.len(), 1, "{:#?}", report.findings);
    assert_eq!(self_name[0].locale, "pt");
    assert_eq!(self_name[0].severity, Severity::Warning);
    let cycles: Vec<_> = report.with_code("loc_fallback_cycle").collect();
    assert_eq!(cycles.len(), 1, "{:#?}", report.findings);
    assert_eq!(
        cycles[0].locale, "es",
        "one finding, on the smallest tag of the cycle"
    );
    assert_eq!(cycles[0].severity, Severity::Warning);
    assert!(!report.fails(false), "warnings only: {:#?}", report.findings);
    assert!(report.fails(true), "--strict fails on them");
}

#[test]
fn a_complete_project_passes_strict() {
    let en = parsed("en", "a = A { $n }\n").0;
    let fr = parsed("fr", "a = Á { $n }\n").0;
    let report = loc_check::validate(&[en, fr], &CheckOptions::default());
    assert!(!report.fails(true), "{:#?}", report.findings);
}

fn write(root: &std::path::Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

#[test]
fn gather_excludes_match_with_a_trailing_slash_or_backslashes() {
    // `exclude: ["assets/old/"]` never matched (ADR-1660).
    let root = regia_test_utils::unique_temp_dir("loc-gather-exclude");
    write(&root, "assets/ui/menu.ui.ron", "(a: Loc(\"kept\"))\n");
    write(&root, "assets/old/a.ron", "(a: Loc(\"old-a\"))\n");
    write(&root, "assets/legacy/b.ron", "(a: Loc(\"legacy-b\"))\n");
    let config = GatherConfig {
        roots: vec!["assets".into()],
        exclude: vec!["assets/old/".into(), "assets\\legacy".into()],
    };
    let gathered = loc_gather::gather_project(&root, &config).expect("gather");
    let keys: Vec<&str> = gathered.keys.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["kept"]);
    assert_eq!(gathered.files_scanned, 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// audit-ext-b (Low): a symlinked `.ron` or `.rs` file was skipped without a word, so keys in a
/// shared (linked) prefab never reached `gathered.ftl`. A linked file is read; a linked directory
/// is still not descended (it could loop).
#[cfg(unix)]
#[test]
fn gather_reads_linked_files_but_not_linked_directories() {
    let root = regia_test_utils::unique_temp_dir("loc-gather-links");
    let shared = regia_test_utils::unique_temp_dir("loc-gather-links-shared");
    write(&root, "assets/ui/menu.ui.ron", "(a: Loc(\"own\"))\n");
    write(&shared, "prefab.ron", "(a: Loc(\"shared-prefab\"))\n");
    write(&shared, "dir/inner.ron", "(a: Loc(\"inner\"))\n");
    std::os::unix::fs::symlink(shared.join("prefab.ron"), root.join("assets/ui/prefab.ron"))
        .expect("file link");
    std::os::unix::fs::symlink(shared.join("dir"), root.join("assets/linked_dir")).expect("dir link");
    std::os::unix::fs::symlink(shared.join("gone.ron"), root.join("assets/broken.ron")).expect("broken link");
    let config = GatherConfig {
        roots: vec!["assets".into()],
        exclude: vec![],
    };
    let gathered = loc_gather::gather_project(&root, &config).expect("gather");
    let keys: Vec<&str> = gathered.keys.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["own", "shared-prefab"]);
    assert_eq!(gathered.files_scanned, 2);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&shared);
}

#[test]
fn gather_finds_keys_in_ui_scenes_and_rust_and_appends_them() {
    let root = regia_test_utils::unique_temp_dir("loc-gather");
    write(
        &root,
        "assets/ui/menu.ui.ron",
        "(root: (text: (spans: [Loc(\"menu-start\")]), children: [(text: (spans: [Loc(\"menu-options\")]))]))\n",
    );
    write(
        &root,
        "assets/levels/intro.level.ron",
        "(resources: { \"Banner\": (title: \"loc:level-intro\") })\n",
    );
    write(
        &root,
        "src/hud.rs",
        "fn ammo() { let _ = regia_ui::loc!(\"hud-ammo\", count = 3); }\n",
    );
    write(&root, "assets/localization/en/main.ftl", EN);
    write(&root, "assets/localization/fr/main.ftl", FR);
    write(
        &root,
        "assets/localization/ignored.ron",
        "(x: Loc(\"not-gathered\"))\n",
    );

    let gathered = loc_gather::gather_project(&root, &GatherConfig::default()).expect("gather");
    let keys: Vec<&str> = gathered.keys.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["hud-ammo", "level-intro", "menu-options", "menu-start"]
    );
    assert_eq!(gathered.keys["hud-ammo"][0].file, "src/hud.rs");
    assert_eq!(gathered.files_scanned, 3);

    let locales = vec![parsed("en", EN), parsed("fr", FR)];
    let plan = loc_gather::plan(&gathered, &locales, "en", "assets/localization", &BTreeMap::new());
    assert_eq!(plan.len(), 2);
    let en_plan = plan.iter().find(|p| p.locale == "en").expect("en");
    assert_eq!(
        en_plan.added,
        vec![s("hud-ammo"), s("level-intro"), s("menu-options")]
    );
    assert!(
        en_plan
            .text
            .contains("# source: src/hud.rs:1\nhud-ammo = hud-ammo\n"),
        "{}",
        en_plan.text
    );
    let fr_plan = plan.iter().find(|p| p.locale == "fr").expect("fr");
    assert!(
        fr_plan.text.contains("# TODO translate\nhud-ammo = hud-ammo"),
        "{}",
        fr_plan.text
    );
    for file in &plan {
        loc_gather::write_planned(&root, file).expect("write");
    }
    // The written files parse, and a second gather has nothing to add.
    let reread = |tag: &str| {
        let mut files = Vec::new();
        for name in ["main.ftl", "gathered.ftl"] {
            let rel = format!("assets/localization/{tag}/{name}");
            files.push((
                rel.clone(),
                std::fs::read_to_string(root.join(&rel)).expect("read"),
            ));
        }
        (loc_check::parse_locale(tag, &files), files)
    };
    let again = vec![reread("en"), reread("fr")];
    for (p, _) in &again {
        assert!(p.findings.is_empty(), "{:?}", p.findings);
    }
    let plan = loc_gather::plan(&gathered, &again, "en", "assets/localization", &BTreeMap::new());
    assert!(plan.is_empty(), "{plan:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn gather_copies_the_source_message_into_other_locales() {
    let mut gathered = loc_gather::Gathered::default();
    gathered.keys.insert(s("menu-quit"), Vec::new());
    let locales = vec![parsed("en", EN), parsed("fr", FR)];
    let plan = loc_gather::plan(&gathered, &locales, "en", "assets/localization", &BTreeMap::new());
    assert_eq!(plan.len(), 1, "en already has menu-quit");
    assert!(
        plan[0].text.contains("# TODO translate\nmenu-quit = Quit\n"),
        "{}",
        plan[0].text
    );
}

#[test]
fn config_names_source_and_fallbacks() {
    let config = LocalizationConfig::from_ron(
        "(format_version: 1, source: \"en\", locales: [\"en\", \"fr\", \"fr-CA\"], fallbacks: { \"fr-CA\": [\"fr\"] })",
    )
    .expect("config");
    assert_eq!(config.locales.len(), 3);
    assert_eq!(config.gather.roots, vec![s("assets"), s("src")]);
}

#[test]
fn pseudo_locale_file_is_valid_fluent() {
    let out = regia_ui::pseudo::pseudo_ftl(EN).expect("pseudo");
    let l = Localization::new(vec![(s("en-XA"), out)], Some(s("en-XA"))).expect("pseudo locale parses");
    assert_eq!(l.resolve("menu-start", &[]), "[Šŧáŕŧ Ĝáɱé~~~~]");
    assert!(l.resolve("menu-saves", &count("2")).contains('2'));
}

/// audit-ext-b test gap: CRLF files. A key never picks up the `\r` (the quote ends it, an
/// unterminated one is dropped at the line end), and lines count the same as with LF.
#[test]
fn crlf_files_give_the_same_keys_and_lines_as_lf() {
    let lf = "(a: Loc(\"one\"))\n(b: \"loc:two\")\n(c: Loc(\"open\n(d: Loc(\"four\"))\n";
    let crlf = lf.replace('\n', "\r\n");
    assert_eq!(loc_gather::scan_ron(&crlf), loc_gather::scan_ron(lf));
    assert_eq!(
        loc_gather::scan_ron(&crlf),
        vec![
            ("one".to_owned(), 1),
            ("two".to_owned(), 2),
            ("four".to_owned(), 4)
        ]
    );
    let rust = "let a = loc!(\"r-one\");\nlet b = loc!(\"r-open\nlet c = loc!(\r\n    \"r-two\");\n";
    let rust_crlf = rust.replace('\n', "\r\n").replace("\r\r\n", "\r\n");
    assert_eq!(loc_gather::scan_rust(&rust_crlf), loc_gather::scan_rust(rust));
    assert!(
        loc_gather::scan_rust(&rust_crlf)
            .iter()
            .all(|(k, _)| !k.contains('\r'))
    );
}

/// Levels audit L6: a value that formats to nothing is the locale's answer. It used to read as
/// missing, so an English player got the French text.
#[test]
fn a_message_that_formats_empty_stops_the_fallback_chain() {
    let l = Localization::new(
        vec![
            (s("en"), s("blank = { \"\" }\nother = text\n")),
            (s("fr"), s("blank = texte\nonly-fr = oui\n")),
        ],
        Some(s("en")),
    )
    .expect("bundles");
    assert_eq!(l.resolve_in("en", "blank", &[]), Some(String::new()));
    assert_eq!(l.resolve("blank", &[]), "");
    assert_eq!(l.resolve("other", &[]), "text");
}
