// SPDX-License-Identifier: MPL-2.0
//! File-level checks for the provenance command (ADR-4581), with isolated deterministic fixtures.

use std::path::Path;
use std::process::ExitCode;

use xtask::provenance;

fn write_log(root: &Path, source: &str) {
    let docs = root.join("docs");
    std::fs::create_dir_all(&docs).expect("fixture directory");
    let pending = docs.join("provenance.pending");
    std::fs::write(
        &pending,
        format!(
            "| Area | What | Source | Notes |\n|---|---|---|---|\n| Fixture | Work queue | {source} | Original Rust |\n"
        ),
    )
    .expect("fixture log");
    std::fs::rename(pending, docs.join("provenance.md")).expect("atomic fixture log");
}

#[test]
fn command_fails_a_set_gap_and_passes_its_documented_repair() {
    let root = regia_test_utils::unique_temp_dir("provenance-set-url");
    let source = "[class: public-documentation] Public-sources spec set area `fixture` (ADR-1526)";
    write_log(&root, source);
    let report = provenance::check(&root).expect("read fixture");
    assert_eq!(report.rows, 1);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].code, "set_url_missing");
    assert_eq!(provenance::run(&root).expect("run gap check"), ExitCode::FAILURE);
    write_log(&root, &format!("{source}; <https://example.org/public/manual>"));
    assert_eq!(
        provenance::run(&root).expect("run repaired check"),
        ExitCode::SUCCESS
    );
    std::fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn missing_log_is_an_io_error_rather_than_a_passing_empty_report() {
    let root = regia_test_utils::unique_temp_dir("provenance-missing-log");
    let error = provenance::check(&root).expect_err("missing log must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    std::fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn sdk_and_mach_o_rows_pass_with_their_resolved_sources() {
    // ADR-4931: the two historical header rows (ADR-2002 and ADR-3373) carry allowed classes.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let text = std::fs::read_to_string(root.join("docs/provenance.md")).expect("read log");
    let report = provenance::check_text(&text);
    for (marker, class) in [
        (
            "DLSS Super Resolution through NGX (sprint, ADR-2002)",
            "[class: licensed-vendor-sdk; approval: ADR-2002]",
        ),
        (
            "Editor / Mach-O code-signature page check (ADR-3373)",
            "[class: permissive-code; license: Apache-2.0 WITH LLVM-exception]",
        ),
    ] {
        let lines: Vec<usize> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.starts_with(&format!("| {marker} |")))
            .map(|(index, _)| index + 1)
            .collect();
        assert_eq!(lines.len(), 1, "exactly one row for {marker}");
        let row = text.lines().nth(lines[0] - 1).expect("row line");
        assert!(row.contains(&format!("| {class} ")), "{marker} declares {class}");
        let defects: Vec<_> = report.findings.iter().filter(|f| f.line == lines[0]).collect();
        assert!(defects.is_empty(), "{marker}: {defects:?}");
    }
}

#[test]
fn repository_log_has_only_allowed_source_declarations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let report = provenance::check(root).expect("read repository log");
    assert!(report.rows > 0);
    assert!(
        report.findings.is_empty(),
        "provenance record defects: {:?}",
        report.findings
    );
    assert_eq!(report.classes.values().sum::<usize>(), report.rows);
}
