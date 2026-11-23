// SPDX-License-Identifier: MPL-2.0
//! M18 robustness: mutation fuzzing of the `.ui.ron` loader. See `docs/evidence/m18/robustness.md`.

// `UiError` embeds `ron::SpannedError`. The lib crate allows `result_large_err`; this test crate does not.
#![allow(clippy::result_large_err)]

use regia_test_utils::fuzz::fuzz_text_loader;
use regia_ui::{UiMigrators, parse_ui_ron};

const N: usize = 3_000;

#[test]
fn ui_ron_never_panics() {
    let fixture = include_str!("fixtures/ui.example.ui.ron");
    fuzz_text_loader(fixture, 0xC1, N, |source| {
        parse_ui_ron(source, "fuzz.ui.ron", &UiMigrators::default())
    });
}

/// The `.ui.ron` text upgrade (the v1 → v2 version scanner, ADR-1381) never panics on mutated
/// v1 input either.
#[test]
fn ui_ron_upgrade_never_panics() {
    let fixture = include_str!("fixtures/ui.example.ui.ron");
    fuzz_text_loader(fixture, 0xC3, N, |source| {
        regia_ui::upgrade_ui_ron(source, "fuzz.ui.ron")
    });
}

#[test]
fn theme_ron_never_panics() {
    let fixture = include_str!("fixtures/theme.example.theme.ron");
    regia_ui::parse_theme_ron(fixture, "fuzz.theme.ron").expect("the fixture itself loads");
    fuzz_text_loader(fixture, 0xC2, N, |source| {
        regia_ui::parse_theme_ron(source, "fuzz.theme.ron")
    });
}

/// Table-driven hostile theme documents: each must be a clean error.
#[test]
fn theme_ron_hostile_inputs_are_errors() {
    let cases: [(&str, String); 6] = [
        ("empty", String::new()),
        ("not a struct", "42".into()),
        ("unclosed", "(format_version: 1, name: \"x\", tokens: {".into()),
        (
            "future version",
            "(format_version: 4294967295, name: \"x\")".into(),
        ),
        ("empty name", "(format_version: 1, name: \"\")".into()),
        (
            "bad density",
            "(format_version: 1, name: \"x\", densities: {\"huge\": {}})".into(),
        ),
    ];
    for (what, source) in cases {
        let err = regia_ui::parse_theme_ron(&source, "hostile.theme.ron").expect_err(what);
        assert!(!err.to_string().is_empty(), "{what}: empty error message");
    }
}

/// XLIFF 2.0 translator files (ADR-2550): the importer, from the XML reader to the import plan, returns a
/// value or an error on mutated input, never a panic or a hang.
#[test]
fn xliff_import_never_panics() {
    use regia_ui::loc_po::{ExportFilter, ExportInput, ImportContext, export_po, plan_import};
    use regia_ui::loc_xliff::{plan_import_xliff, read_xliff, write_xliff};

    const SOURCE: &str = "menu-quit = Quit\nhud-ammo = { $count ->\n    [one] { $count } round\n   *[other] { $count } rounds\n}\ngreet = Hello, { $name }! A < B & C\n";
    let mut po = export_po(&ExportInput {
        project: "demo".into(),
        source: "en".into(),
        locale: "fr".into(),
        source_files: vec![("en/main.ftl".into(), SOURCE.into())],
        filter: ExportFilter::All,
        ..ExportInput::default()
    })
    .file;
    for entry in &mut po.entries {
        entry.msgstr = entry.msgid.clone();
    }
    let fixture = write_xliff(&po).expect("the fixture writes");
    // The fixture itself imports exactly like the PO it came from.
    let ctx = ImportContext {
        locale: "fr".into(),
        source: "en".into(),
        source_files: vec![("en/main.ftl".into(), SOURCE.into())],
        ..ImportContext::default()
    };
    let read = read_xliff(&fixture).expect("the fixture reads");
    assert_eq!(
        plan_import_xliff(&read, &ctx).imported_ftl,
        plan_import(&po, &ctx).imported_ftl
    );
    fuzz_text_loader(&fixture, 0x584c_4946, N, |source| {
        let read = read_xliff(source)?;
        let _ = plan_import_xliff(&read, &ImportContext::default());
        Ok::<_, regia_ui::loc_xliff::XliffError>(read)
    });
}
