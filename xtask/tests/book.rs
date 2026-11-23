// SPDX-License-Identifier: MPL-2.0
//! End-to-end tests for `book::run`: a complete book passes; a dead link fails; a schema method
//! missing from `COVERAGE.md` fails. Each test builds a throwaway `book/` tree under the system
//! temp directory (no cargo invocation — the fixtures contain no `rust` blocks).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};

use xtask::book;

/// A fresh, unique temp directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("regia-book-check-{}-{name}-{n}", std::process::id()))
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("rel has a parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

/// A full `COVERAGE.md` method table mapping every schema method to `agents/protocol.md`.
fn coverage(rows: impl Iterator<Item = String>) -> String {
    let mut text =
        String::from("## Agent protocol methods\n\n| Method | MCP tool | Agent page |\n|---|---|---|\n");
    for row in rows {
        text.push_str(&row);
    }
    text
}

/// A `COVERAGE.md` that maps every schema method except `missing`.
fn coverage_without(missing: &str) -> String {
    coverage(
        regia_agent::schema::METHODS
            .iter()
            .filter(|m| m.name != missing)
            .map(|m| {
                format!(
                    "| `{}` | `{}` | [protocol](agents/protocol.md) |\n",
                    m.name, m.tool
                )
            }),
    )
}

/// A complete `COVERAGE.md`.
fn coverage_full() -> String {
    coverage(regia_agent::schema::METHODS.iter().map(|m| {
        format!(
            "| `{}` | `{}` | [protocol](agents/protocol.md) |\n",
            m.name, m.tool
        )
    }))
}

/// An agent page that mentions every schema method (so `method_pages` finds each on a page).
fn protocol_page() -> String {
    let mut text = String::from("# Protocol\n\n");
    for m in regia_agent::schema::METHODS {
        text.push_str(&format!("`{}`\n\n", m.name));
    }
    text
}

/// Writes a book with the given summary extras and coverage; returns the workspace root.
fn write_book(summary_extra: &str, coverage: &str) -> PathBuf {
    let root = temp_dir("book");
    write(
        &root,
        "book/SUMMARY.md",
        &format!("# Summary\n\n- [Protocol](agents/protocol.md)\n- [Coverage](COVERAGE.md)\n{summary_extra}"),
    );
    write(&root, "book/COVERAGE.md", coverage);
    write(&root, "book/agents/protocol.md", &protocol_page());
    // `book-check` reads the anchor baseline (ADR-5992); an empty one accepts no unresolved anchor.
    write(
        &root,
        "book/anchor-baseline.json",
        "{\"format_version\": 1, \"rows\": []}\n",
    );
    root
}

#[test]
fn a_complete_book_passes() {
    let root = write_book("", &coverage_full());
    assert_eq!(book::run(&root).expect("run"), ExitCode::SUCCESS);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_dead_link_fails() {
    let root = write_book("", &coverage_full());
    // A link from a summary-linked page to a page that does not exist.
    write(
        &root,
        "book/agents/protocol.md",
        &format!("{}\n[broken](missing.md)\n", protocol_page()),
    );
    assert_eq!(book::run(&root).expect("run"), ExitCode::FAILURE);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_schema_method_missing_from_coverage_fails() {
    let missing = regia_agent::schema::METHODS[0].name;
    let root = write_book("", &coverage_without(missing));
    assert_eq!(book::run(&root).expect("run"), ExitCode::FAILURE);
    let _ = std::fs::remove_dir_all(&root);
}

/// ADR-1809: a row for a built-in extension method (`abilities.*`, `curves.*`, ...) is a real
/// method, not an unknown one; a row for a name that is neither still fails.
#[test]
fn a_built_in_extension_method_row_passes_and_an_unknown_row_fails() {
    let extension = regia_agent::builtin_extension_methods()[0];
    let row = |method: &str| {
        let tool = method.replace('.', "_");
        format!("| `{method}` | `{tool}` | [protocol](agents/protocol.md) |\n")
    };
    let root = write_book("", &(coverage_full() + &row(extension)));
    assert_eq!(book::run(&root).expect("run"), ExitCode::SUCCESS);
    let _ = std::fs::remove_dir_all(&root);
    let root = write_book("", &(coverage_full() + &row("abilities.no_such_method")));
    assert_eq!(book::run(&root).expect("run"), ExitCode::FAILURE);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_page_not_linked_from_summary_fails() {
    let root = write_book("", &coverage_full());
    // An extra page that SUMMARY.md never links.
    write(&root, "book/user-guide/orphan.md", "# Orphan\n");
    assert_eq!(book::run(&root).expect("run"), ExitCode::FAILURE);
    let _ = std::fs::remove_dir_all(&root);
}
