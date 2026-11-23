// SPDX-License-Identifier: MPL-2.0
//! End-to-end coverage of the agent surface, and the `book-check` hook that runs it.
//!
//! The workspace check is the gate: every method `route` registers already has a schema entry,
//! a guide topic, a coverage row, a guide page and a test literal. The fixture check proves
//! `book-check` fails when a dispatcher file is present but incomplete, and the existing book
//! fixtures (no `dispatch.rs`) stay green.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};

use xtask::agent_coverage;
use xtask::book;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask parent")
        .to_path_buf()
}

fn temp_dir(name: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("regia-agent-coverage-{}-{name}-{n}", std::process::id()))
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

/// A book that already satisfies `book-check`'s own schema map, plus a dispatcher that
/// registers only `agent.hello`.
fn book_with_partial_dispatcher() -> PathBuf {
    let root = temp_dir("hook");
    let mut coverage =
        String::from("## Agent protocol methods\n\n| Method | MCP tool | Agent page |\n|---|---|---|\n");
    let mut page = String::from("# Protocol\n\n");
    for method in regia_agent::schema::METHODS {
        coverage.push_str(&format!(
            "| `{}` | `{}` | [protocol](agents/protocol.md) |\n",
            method.name, method.tool
        ));
        page.push_str(&format!("`{}`\n\n", method.name));
    }
    write(
        &root,
        "book/SUMMARY.md",
        "# Summary\n\n- [Protocol](agents/protocol.md)\n- [Coverage](COVERAGE.md)\n",
    );
    write(&root, "book/COVERAGE.md", &coverage);
    write(&root, "book/agents/protocol.md", &page);
    // `book-check` reads the anchor baseline (ADR-5992); without it `book::run` fails on the read
    // before it reaches the dispatcher check this test is about. An empty one accepts no
    // unresolved anchor, as in tests/book.rs.
    write(
        &root,
        "book/anchor-baseline.json",
        "{\"format_version\": 1, \"rows\": []}\n",
    );
    write(
        &root,
        "crates/regia_agent/src/dispatch.rs",
        "fn route() {\n    match method {\n        \"agent.hello\" => {}\n        other => {}\n    }\n}\n",
    );
    root
}

#[test]
fn the_workspace_agent_surface_is_covered() {
    let report = agent_coverage::check(&workspace_root()).expect("check");
    assert!(
        report.errors.is_empty(),
        "agent-coverage gaps:\n{}",
        report.errors.join("\n")
    );
    assert_eq!(
        report.methods,
        regia_agent::schema::METHODS.len() + regia_agent::builtin_extension_methods().len()
    );
}

#[test]
fn book_check_fails_when_the_dispatcher_is_incomplete() {
    let root = book_with_partial_dispatcher();
    assert_eq!(book::run(&root).expect("run"), ExitCode::FAILURE);
    let _ = std::fs::remove_dir_all(&root);
}
