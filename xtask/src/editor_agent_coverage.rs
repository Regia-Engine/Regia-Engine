// SPDX-License-Identifier: MPL-2.0
//! `editor-agent-coverage`: every editor agent method meets the runtime's coverage rules
//! (ADR-7307).
//!
//! `agent-coverage` checks the methods `route` dispatches and skips the editor's `AgentExtension`
//! methods; `book-coverage` checks that those have a book row, a page that names them and a test.
//! This check holds the editor's methods to the rest of the runtime's rules. xtask does not link
//! the editor (that would pull the whole editor build into every xtask run), so it reads the
//! method table from `docs/api-snapshots/editor-agent-methods.txt`, which the `regia_editor` test
//! `editor_method_snapshot` keeps equal to what the editor registers. For each method there:
//!
//! 1. a schema row with its params (the snapshot line itself, `name(param: kind, opt?: kind)`),
//! 2. a row in the extension table of `book/COVERAGE.md` (§ 5) whose linked page exists and names
//!    the method as a whole identifier,
//! 3. every parameter named, as a whole identifier, on one of the method's pages,
//! 4. a bundled `agent.guide` topic that names the method ([`regia_agent::guide_mentions`]),
//! 5. a string literal of the method name in test code (the `agent-coverage` rules for test code).
//!
//! `cargo xtask editor-agent-coverage` runs it; `--update` refreshes the snapshot by running the
//! editor test with `REGIA_UPDATE_EDITOR_METHODS=1`. `book-check` runs the check whenever the
//! snapshot exists, which is how the merge gate picks it up.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::agent_coverage::{normalize_page, test_literals};
use crate::book_coverage::{EXTENSION_HEADER, backticked, is_book_link, links, table_rows};

/// The snapshot of the editor's method table, relative to the workspace root.
pub const SNAPSHOT_PATH: &str = "docs/api-snapshots/editor-agent-methods.txt";

/// One method of the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorMethod {
    /// `area.name`.
    pub name: String,
    /// Parameter names, in schema order.
    pub params: Vec<String>,
}

/// What [`check`] found.
#[derive(Debug, Default)]
pub struct Report {
    /// One line per gap. Empty when every editor method is covered.
    pub errors: Vec<String>,
    /// Methods in the snapshot.
    pub methods: usize,
}

/// Parses the snapshot: `#` lines are comments, every other non-empty line is
/// `name(param: kind, optional?: kind)`.
pub fn parse_snapshot(text: &str) -> Result<Vec<EditorMethod>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (Some(open), true) = (line.find('('), line.ends_with(')')) else {
            return Err(format!(
                "{SNAPSHOT_PATH}:{}: expected `name(params)`: {line}",
                n + 1
            ));
        };
        let name = line[..open].trim().to_string();
        let inner = &line[open + 1..line.len() - 1];
        let params = inner
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| {
                p.split(':')
                    .next()
                    .unwrap_or(p)
                    .trim()
                    .trim_end_matches('?')
                    .to_string()
            })
            .collect();
        out.push(EditorMethod { name, params });
    }
    Ok(out)
}

/// The gaps of `methods` against the book rows, pages, guide and test literals (pure; [`check`]
/// gathers the inputs).
pub fn gaps(
    methods: &[EditorMethod],
    rows: &[(String, String)],
    pages: &BTreeMap<String, String>,
    guide: &dyn Fn(&str) -> bool,
    tested: &BTreeSet<String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for method in methods {
        let name = &method.name;
        let own: Vec<&String> = rows
            .iter()
            .filter(|(m, _)| m == name)
            .map(|(_, page)| page)
            .collect();
        if own.is_empty() {
            errors.push(format!(
                "editor method `{name}` has no row in the book/COVERAGE.md extension table"
            ));
        }
        let mut texts = Vec::new();
        for page in own {
            match pages.get(&normalize_page(page)) {
                None => errors.push(format!(
                    "editor method `{name}` maps to `{page}`, which does not exist"
                )),
                Some(text) if !regia_agent::mentions_token(text, name) => errors.push(format!(
                    "editor method `{name}` is mapped to `{page}` but that page does not mention it"
                )),
                Some(text) => texts.push(text),
            }
        }
        if !texts.is_empty() {
            for param in &method.params {
                if !texts.iter().any(|text| regia_agent::mentions_token(text, param)) {
                    errors.push(format!(
                        "editor method `{name}` takes parameter `{param}`, but none of its book/COVERAGE.md pages names it"
                    ));
                }
            }
        }
        if !guide(name) {
            errors.push(format!(
                "editor method `{name}` is not named by any agent.guide topic"
            ));
        }
        if !tested.contains(name) {
            errors.push(format!(
                "editor method `{name}` is not called by any test (no string literal in test code)"
            ));
        }
    }
    errors
}

/// Checks the workspace rooted at `root`.
pub fn check(root: &Path) -> Result<Report> {
    let snapshot = root.join(SNAPSHOT_PATH);
    let text =
        std::fs::read_to_string(&snapshot).with_context(|| format!("reading {}", snapshot.display()))?;
    let methods = parse_snapshot(&text).map_err(anyhow::Error::msg)?;
    let coverage_path = root.join("book/COVERAGE.md");
    let coverage = std::fs::read_to_string(&coverage_path)
        .with_context(|| format!("reading {}", coverage_path.display()))?;
    let mut rows = Vec::new();
    for cells in table_rows(&coverage, &EXTENSION_HEADER) {
        let Some(method) = backticked(cells.first().map_or("", String::as_str))
            .into_iter()
            .next()
        else {
            continue;
        };
        for target in links(cells.get(2).map_or("", String::as_str)) {
            if is_book_link(&target) {
                let page = target.split('#').next().unwrap_or(&target).to_string();
                rows.push((method.clone(), page));
            }
        }
    }
    let book = root.join("book");
    let mut pages = BTreeMap::new();
    for (_, page) in &rows {
        let page = normalize_page(page);
        if pages.contains_key(&page) {
            continue;
        }
        let path = book.join(&page);
        if path.is_file() {
            pages.insert(
                page,
                std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
            );
        }
    }
    let tested = test_literals(root)?;
    let mut errors = gaps(
        &methods,
        &rows,
        &pages,
        &|m| regia_agent::guide_mentions(m),
        &tested,
    );
    errors.sort();
    errors.dedup();
    Ok(Report {
        errors,
        methods: methods.len(),
    })
}

/// Runs the check (or, with `update`, refreshes the snapshot first) and returns the exit code.
pub fn run(root: &Path, update: bool) -> Result<ExitCode> {
    if update {
        let status = std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .current_dir(root)
            .env("REGIA_UPDATE_EDITOR_METHODS", "1")
            .args(["test", "-p", "regia_editor", "--test", "editor_method_snapshot"])
            .status()
            .context("running the editor_method_snapshot test")?;
        if !status.success() {
            eprintln!("editor-agent-coverage: refreshing {SNAPSHOT_PATH} failed");
            return Ok(ExitCode::FAILURE);
        }
    }
    let report = check(root)?;
    if report.errors.is_empty() {
        println!("editor-agent-coverage: ok ({} methods)", report.methods);
        Ok(ExitCode::SUCCESS)
    } else {
        for error in &report.errors {
            eprintln!("error: {error}");
        }
        eprintln!("editor-agent-coverage: {} problem(s)", report.errors.len());
        Ok(ExitCode::FAILURE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_lines_parse() {
        let methods = parse_snapshot("# c\neditor.select(entity?: number, mode?: string)\neditor.undo()\n")
            .expect("parse");
        assert_eq!(
            methods,
            vec![
                EditorMethod {
                    name: "editor.select".into(),
                    params: vec!["entity".into(), "mode".into()],
                },
                EditorMethod {
                    name: "editor.undo".into(),
                    params: vec![],
                },
            ]
        );
        assert!(parse_snapshot("editor.bad\n").is_err());
    }

    #[test]
    fn gaps_name_each_missing_rule_and_a_covered_method_has_none() {
        let methods = vec![EditorMethod {
            name: "editor.select".into(),
            params: vec!["entity".into(), "mode".into()],
        }];
        let rows = vec![("editor.select".to_string(), "agents/editor.md".to_string())];
        let mut pages = BTreeMap::from([(
            "agents/editor.md".to_string(),
            "`editor.select { entity }` picks; editor.selection is another.".to_string(),
        )]);
        let errors = gaps(&methods, &rows, &pages, &|_| false, &BTreeSet::new());
        let text = errors.join("\n");
        assert_eq!(errors.len(), 3, "{text}");
        assert!(text.contains("parameter `mode`"), "{text}");
        assert!(text.contains("agent.guide"), "{text}");
        assert!(text.contains("not called by any test"), "{text}");
        // A page row removed: the method has no row.
        let errors = gaps(&methods, &[], &pages, &|_| true, &BTreeSet::new());
        assert!(errors.iter().any(|e| e.contains("has no row")), "{errors:?}");
        pages.insert(
            "agents/editor.md".to_string(),
            "`editor.select { entity, mode }`".to_string(),
        );
        let tested = BTreeSet::from(["editor.select".to_string()]);
        assert!(gaps(&methods, &rows, &pages, &|_| true, &tested).is_empty());
    }

    /// The acceptance proof in a scratch copy: a root holding the real `editor-command.md` page,
    /// a coverage row and a test literal for `editor.actions` passes; removing the row or the
    /// literal fails.
    #[test]
    fn a_scratch_copy_fails_without_its_row_or_test_literal() {
        let real = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let page = std::fs::read_to_string(real.join("book/agents/editor-command.md")).expect("real page");
        // pid + atomic counter, not pid alone (the merge gate runs test binaries in parallel).
        let root = regia_test_utils::unique_temp_dir("xtask-editor-coverage");
        std::fs::create_dir_all(root.join("book/agents")).expect("dir");
        std::fs::create_dir_all(root.join("docs/api-snapshots")).expect("dir");
        std::fs::create_dir_all(root.join("crates/demo/tests")).expect("dir");
        std::fs::write(root.join("book/agents/editor-command.md"), page).expect("page");
        std::fs::write(
            root.join(SNAPSHOT_PATH),
            "# scratch\neditor.actions(panel?: string, class?: string)\n",
        )
        .expect("snapshot");
        let row = "| `editor.actions` | `regia_editor` (`agent_parity`) | [Editor command palette](agents/editor-command.md) |";
        let table = |rows: &str| {
            format!("| Extension method | Registered by | Agent page |\n|---|---|---|\n{rows}\n")
        };
        let literal = "fn t() { let _ = \"editor.actions\"; }\n";
        std::fs::write(root.join("book/COVERAGE.md"), table(row)).expect("coverage");
        std::fs::write(root.join("crates/demo/tests/call.rs"), literal).expect("test");
        let ok = check(&root).expect("check");
        assert!(ok.errors.is_empty(), "{:?}", ok.errors);
        assert_eq!(ok.methods, 1);
        std::fs::write(root.join("book/COVERAGE.md"), table("")).expect("coverage");
        let no_row = check(&root).expect("check");
        assert!(
            no_row.errors.iter().any(|e| e.contains("has no row")),
            "{:?}",
            no_row.errors
        );
        std::fs::write(root.join("book/COVERAGE.md"), table(row)).expect("coverage");
        std::fs::write(root.join("crates/demo/tests/call.rs"), "fn t() {}\n").expect("test");
        let no_literal = check(&root).expect("check");
        assert!(
            no_literal
                .errors
                .iter()
                .any(|e| e.contains("not called by any test")),
            "{:?}",
            no_literal.errors
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
