// SPDX-License-Identifier: MPL-2.0
//! Book-listing consistency checks for `book/COVERAGE.md` and `book/SUMMARY.md` (spec 96).
//!
//! Regression tests for review `sprint-rev-kimi-25` (finding 4, lane sprint-post05-docs-drift-k):
//! the `configurator.contract_*` agent methods had two rows each in the COVERAGE § 2 method table
//! and `agents/configurator-contracts.md` was linked twice from SUMMARY, although the page's H1 is
//! "Product viewer contracts (agents)" — the second entry was a stale duplicate.

use std::fs;

use regia_test_utils::book::workspace_root;

/// Data rows (not separators) of a markdown table that contain `needle`.
fn table_rows_containing<'a>(text: &'a str, needle: &str) -> Vec<&'a str> {
    text.lines()
        .filter(|line| {
            line.starts_with('|') && !line[1..].trim_start().starts_with('-') && line.contains(needle)
        })
        .collect()
}

#[test]
fn configurator_contract_methods_have_exactly_one_coverage_row() {
    let coverage_path = workspace_root().join("book/COVERAGE.md");
    let coverage = fs::read_to_string(&coverage_path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", coverage_path.display()));
    for method in ["`configurator.contract_check`", "`configurator.contract_schema`"] {
        let rows = table_rows_containing(&coverage, method);
        assert_eq!(
            rows.len(),
            1,
            "{method} appears in {} COVERAGE rows, expected exactly 1:\n{}",
            rows.len(),
            rows.join("\n")
        );
    }
}

#[test]
fn configurator_contracts_page_is_linked_once_from_summary() {
    let summary_path = workspace_root().join("book/SUMMARY.md");
    let summary = fs::read_to_string(&summary_path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", summary_path.display()));
    let links: Vec<&str> = summary
        .lines()
        .filter(|line| line.contains("](agents/configurator-contracts.md)"))
        .collect();
    assert_eq!(
        links.len(),
        1,
        "agents/configurator-contracts.md is linked {} times from SUMMARY, expected exactly 1:\n{}",
        links.len(),
        links.join("\n")
    );
}
