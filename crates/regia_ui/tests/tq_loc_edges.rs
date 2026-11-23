// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane (ADR-1744): the localization tools' boundary cases: the gather scanner,
//! project walk and atomic write, and the `localization.ron` reader. No goldens, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;

use regia_ui::loc_config::{GatherConfig, LocalizationConfig, LocalizationConfigError};
use regia_ui::loc_gather::{self, PlannedFile};

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn keys(found: Vec<(String, u32)>) -> Vec<(String, u32)> {
    found
}

#[test]
fn scanner_ignores_names_that_merely_end_in_the_marker() {
    let rust = "let a = alloc!(\"not-a-key\");\nlet b = my_loc!(\"nor-this\");\nlet c = loc!(\"real\");\n";
    assert_eq!(keys(loc_gather::scan_rust(rust)), vec![("real".to_owned(), 3)]);
    let ron = "(a: Alloc(\"nope\"), b: Loc(\"yes\"), c: _Loc(\"no\"))\n";
    assert_eq!(keys(loc_gather::scan_ron(ron)), vec![("yes".to_owned(), 1)]);
}

#[test]
fn scanner_skips_commented_out_keys_in_both_ron_forms() {
    let ron = "(\n  // label: \"loc:old-key\",\n  // text: Loc(\"old-too\"),\n  label: \"loc:live\",\n)\n";
    assert_eq!(keys(loc_gather::scan_ron(ron)), vec![("live".to_owned(), 4)]);
}

#[test]
fn scanner_handles_unterminated_crlf_empty_and_multiple_keys_per_line() {
    assert!(loc_gather::scan_ron("").is_empty());
    assert!(loc_gather::scan_rust("").is_empty());
    // An unterminated string on a line is not a key, and the next line still scans.
    let ron = "(a: Loc(\"open,\n b: Loc(\"closed\"))";
    assert_eq!(keys(loc_gather::scan_ron(ron)), vec![("closed".to_owned(), 2)]);
    // CRLF line endings keep the right line numbers.
    let crlf = "(\r\n a: Loc(\"one\"),\r\n b: Loc(\"two\"))\r\n";
    assert_eq!(
        keys(loc_gather::scan_ron(crlf)),
        vec![("one".to_owned(), 2), ("two".to_owned(), 3)]
    );
    // Several markers on one line, and an empty key (reported, not dropped).
    let two = "(Loc(\"a\"), Loc(\"b\"), Loc(\"\"))";
    let found = keys(loc_gather::scan_ron(two));
    assert_eq!(found.len(), 3, "{found:?}");
    // Multi-byte text before the marker does not break slicing.
    assert_eq!(
        keys(loc_gather::scan_ron("(ñ: \"日本\", t: Loc(\"ok\"))")),
        vec![("ok".to_owned(), 1)]
    );
    // A marker at the very end of the text.
    assert!(loc_gather::scan_ron("Loc(").is_empty());
    assert!(loc_gather::scan_rust("loc!(").is_empty());
    assert!(loc_gather::scan_rust("loc! (\"").is_empty());
}

#[test]
fn scanner_output_is_sorted_and_repeatable() {
    let text = "(b: Loc(\"z\"), c: \"loc:a\")\n(d: Loc(\"m\"))\n";
    let first = loc_gather::scan_ron(text);
    assert_eq!(first, loc_gather::scan_ron(text));
    let lines: Vec<u32> = first.iter().map(|(_, l)| *l).collect();
    assert!(lines.windows(2).all(|w| w[0] <= w[1]), "{first:?}");
}

#[test]
fn gather_reads_overlapping_roots_once_and_honors_trailing_slash_excludes() {
    let root = regia_test_utils::unique_temp_dir("tq-gather-roots");
    write(&root, "assets/ui/a.ui.ron", "(t: Loc(\"menu-a\"))\n");
    write(&root, "assets/skip/b.ron", "(t: Loc(\"skipped\"))\n");
    write(&root, "assets/.hidden/c.ron", "(t: Loc(\"hidden\"))\n");
    write(&root, "assets/target/d.ron", "(t: Loc(\"build-output\"))\n");
    write(&root, "assets/notes.txt", "Loc(\"text-file\")\n");
    let config = GatherConfig {
        roots: vec![
            "assets".to_owned(),
            "assets/ui".to_owned(),
            "missing-dir".to_owned(),
        ],
        exclude: vec!["assets/skip/".to_owned()],
    };
    let gathered = loc_gather::gather_project(&root, &config).expect("gather");
    assert_eq!(gathered.files_scanned, 1, "{gathered:?}");
    let keys: Vec<&str> = gathered.keys.keys().map(String::as_str).collect();
    assert_eq!(keys, ["menu-a"]);
    assert_eq!(gathered.keys["menu-a"].len(), 1, "one use, not one per root");
    assert_eq!(gathered.keys["menu-a"][0].file, "assets/ui/a.ui.ron");
    // Same input, same output.
    assert_eq!(
        gathered,
        loc_gather::gather_project(&root, &config).expect("again")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn gather_separates_bad_keys_and_survives_an_empty_project() {
    let root = regia_test_utils::unique_temp_dir("tq-gather-bad");
    write(
        &root,
        "src/a.rs",
        "fn f() { loc!(\"9lives\"); loc!(\"fine-key\"); loc!(\"\"); }\n",
    );
    let gathered = loc_gather::gather_project(&root, &GatherConfig::default()).expect("gather");
    assert_eq!(gathered.key_set().into_iter().collect::<Vec<_>>(), ["fine-key"]);
    let bad: Vec<&str> = gathered.bad_keys.iter().map(|(_, k)| k.as_str()).collect();
    assert_eq!(bad, ["", "9lives"], "sorted by line, then key");
    let empty = regia_test_utils::unique_temp_dir("tq-gather-empty");
    let none = loc_gather::gather_project(&empty, &GatherConfig::default()).expect("gather");
    assert_eq!(none, loc_gather::Gathered::default());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn write_planned_replaces_atomically_and_leaves_no_temp_file_on_failure() {
    let root = regia_test_utils::unique_temp_dir("tq-write-planned");
    let file = |text: &str| PlannedFile {
        locale: "en".to_owned(),
        path: "assets/localization/en/gathered.ftl".to_owned(),
        added: vec![],
        text: text.to_owned(),
    };
    loc_gather::write_planned(&root, &file("a = A\n")).expect("first");
    loc_gather::write_planned(&root, &file("a = A\nb = B\n")).expect("overwrite");
    let dir = root.join("assets/localization/en");
    assert_eq!(
        std::fs::read_to_string(dir.join("gathered.ftl")).unwrap(),
        "a = A\nb = B\n"
    );
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1, "no temp files left: {names:?}");
    // The target is a directory: the rename fails and the temp file is cleaned up.
    let blocked = PlannedFile {
        path: "assets/localization/en".to_owned(),
        ..file("x")
    };
    assert!(loc_gather::write_planned(&root, &blocked).is_err());
    let parent_names: Vec<_> = std::fs::read_dir(root.join("assets/localization"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        parent_names.len(),
        1,
        "temp file removed after the failure: {parent_names:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn plan_with_nothing_missing_writes_nothing() {
    let gathered = loc_gather::Gathered::default();
    let plan = loc_gather::plan(&gathered, &[], "en", "assets/localization", &BTreeMap::new());
    assert!(plan.is_empty());
}

// ---- localization.ron ---------------------------------------------------------------------

#[test]
fn config_rejects_version_zero_empty_source_and_garbage() {
    assert!(matches!(
        LocalizationConfig::from_ron("(format_version: 0)"),
        Err(LocalizationConfigError::Version {
            found: 0,
            supported: 1
        })
    ));
    assert!(matches!(
        LocalizationConfig::from_ron("(format_version: 1, source: \"\")"),
        Err(LocalizationConfigError::BadLocale(t)) if t.is_empty()
    ));
    for bad in [
        "",
        "(",
        "not ron",
        "(format_version: -1)",
        "(format_version: \"1\")",
    ] {
        assert!(
            matches!(
                LocalizationConfig::from_ron(bad),
                Err(LocalizationConfigError::Parse(_))
            ),
            "{bad:?}"
        );
    }
    // A bad tag anywhere is found: locales, fallback keys and fallback values.
    for text in [
        "(locales: [\"en\", \"x y\"])",
        "(fallbacks: {\"bad tag\": [\"en\"]})",
        "(fallbacks: {\"pt-BR\": [\"???\"]})",
    ] {
        assert!(
            matches!(
                LocalizationConfig::from_ron(text),
                Err(LocalizationConfigError::BadLocale(_))
            ),
            "{text}"
        );
    }
}

#[test]
fn config_unknown_fields_are_rejected_at_every_level() {
    assert!(LocalizationConfig::from_ron("(gather: (roots: [], extra: 1))").is_err());
    assert!(LocalizationConfig::from_ron("(gather: (roots: [\"a\"]))").is_ok());
}

#[test]
fn config_default_round_trips_and_is_valid() {
    let default = LocalizationConfig::default();
    let text = ron::to_string(&default).expect("serialize");
    assert_eq!(LocalizationConfig::from_ron(&text).expect("round trip"), default);
    assert_eq!(LocalizationConfig::from_ron("()").expect("empty struct"), default);
    let message = LocalizationConfig::from_ron("(format_version: 9)")
        .unwrap_err()
        .to_string();
    assert!(message.contains('9') && message.contains('1'), "{message}");
}
