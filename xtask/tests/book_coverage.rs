// SPDX-License-Identifier: MPL-2.0
//! Spec 96 (ADR-1704): the workspace's host extension methods, Cargo features and `regia`
//! subcommands are all in `book/COVERAGE.md`, and a fixture workspace with one undocumented
//! extension method fails until its row, page and test exist.

use std::path::{Path, PathBuf};

use xtask::book_coverage;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask parent")
        .to_path_buf()
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, contents).expect("write");
}

#[test]
fn the_workspace_book_covers_every_shipped_surface() {
    let report = book_coverage::check(&workspace_root()).expect("check");
    let lines: Vec<String> = report
        .findings
        .iter()
        .map(|f| format!("[{}] {}", f.code, f.message))
        .collect();
    assert!(lines.is_empty(), "book-coverage gaps:\n{}", lines.join("\n"));
    // The scan sees the editor, Steam and cloth methods, and the constructor-built ones.
    for method in [
        "editor.select",
        "editor.tool_list",
        "editor.anim_open",
        "steam.status",
        "physics.cloth_stats",
    ] {
        assert!(
            report.extension_methods.contains_key(method),
            "{method} not found by the scan"
        );
    }
    // Test-only extension methods (`regia_agent`'s `#[cfg(test)] mod scope_tests;`) are not shipped.
    assert!(!report.extension_methods.contains_key("editor.whoami"));
    assert!(report.cli_commands.iter().any(|c| c == "install-android"));
    assert!(
        report
            .features
            .get("regia")
            .is_some_and(|f| f.iter().any(|x| x == "steam"))
    );
    assert_eq!(report.to_json()["ok"], true);
}

#[test]
fn an_undocumented_extension_method_fails_until_documented() {
    let root = regia_test_utils::unique_temp_dir("book-coverage-fixture");
    write(
        &root,
        "crates/regia_fixture/Cargo.toml",
        "[package]\nname = \"regia_fixture\"\n\n[features]\nextra = []\n",
    );
    write(
        &root,
        "crates/regia_fixture/src/lib.rs",
        "pub fn schema() -> MethodSchema {\n    MethodSchema { name: \"fixture.hello\", tool: \"hello\" }\n}\n\
         #[cfg(test)]\nmod tests {\n    fn t() { MethodSchema { name: \"fixture.test_only\" }; }\n}\n",
    );
    let feature_row = "## 1\n\n| Crate | Feature | What it does | Page |\n|---|---|---|---|\n\
                       | `regia_fixture` | `extra` | Extra. | [Fixture](agents/fixture.md) |\n\n";
    write(&root, "book/COVERAGE.md", feature_row);
    write(&root, "book/agents/fixture.md", "# Fixture\n");

    let report = book_coverage::check(&root).expect("check");
    let codes: Vec<(&str, &str)> = report
        .findings
        .iter()
        .map(|f| (f.code, f.subject.as_str()))
        .collect();
    assert_eq!(
        codes,
        [("missing_row", "fixture.hello"), ("untested", "fixture.hello")]
    );
    assert_eq!(report.to_json()["findings"][0]["section"], "extension");

    write(
        &root,
        "book/COVERAGE.md",
        &format!(
            "{feature_row}## 5\n\n| Extension method | Registered by | Agent page |\n|---|---|---|\n\
             | `fixture.hello` | `regia_fixture` | [Fixture](agents/fixture.md) |\n"
        ),
    );
    write(
        &root,
        "book/agents/fixture.md",
        "# Fixture\n\n`fixture.hello` says hello.\n",
    );
    write(
        &root,
        "crates/regia_fixture/tests/hello.rs",
        "#[test]\nfn hello() { let _ = \"fixture.hello\"; }\n",
    );
    let report = book_coverage::check(&root).expect("check");
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    let _ = std::fs::remove_dir_all(&root);
}

/// Spec 96 § 5: `book-coverage` exits 1 when there is any finding and 0 when there is none.
#[test]
fn the_exit_code_follows_the_findings() {
    let root = regia_test_utils::unique_temp_dir("book-coverage-exit");
    write(
        &root,
        "crates/regia_fixture/Cargo.toml",
        "[package]\nname = \"regia_fixture\"\n\n[features]\nextra = []\n",
    );
    write(&root, "book/COVERAGE.md", "# Coverage\n");
    let failure = format!("{:?}", std::process::ExitCode::FAILURE);
    let success = format!("{:?}", std::process::ExitCode::SUCCESS);
    let code = book_coverage::run(&root, true).expect("run");
    assert_eq!(format!("{code:?}"), failure, "an undocumented feature must fail");
    write(
        &root,
        "book/COVERAGE.md",
        "| Crate | Feature | What it does | Page |\n|---|---|---|---|\n\
         | `regia_fixture` | `extra` | Extra. | [Fixture](agents/fixture.md) |\n",
    );
    write(&root, "book/agents/fixture.md", "# Fixture\n");
    let code = book_coverage::run(&root, false).expect("run");
    assert_eq!(format!("{code:?}"), success);
    let _ = std::fs::remove_dir_all(&root);
}

/// Spec 96 § 7: the report is a pure function of the tree; two runs give the same JSON bytes.
#[test]
fn the_report_is_the_same_bytes_twice() {
    let root = workspace_root();
    let first = book_coverage::check(&root).expect("check").to_json().to_string();
    let second = book_coverage::check(&root).expect("check").to_json().to_string();
    assert_eq!(first, second);
}
