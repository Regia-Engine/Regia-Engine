// SPDX-License-Identifier: MPL-2.0
//! `docs/security/threat-model.md` stays true to the code (ADR-3206): every mitigation row has a test
//! or says why it has none, every file and symbol it cites exists, every test it names exists and is a
//! test, and it makes no absolute claim. Line numbers drift, so they are checked only for being inside
//! the file; the symbol is the stable reference.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/<name> has a repository root")
        .to_path_buf()
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Every `` `text` `` span of a table cell.
fn spans(cell: &str) -> Vec<&str> {
    cell.split('`').skip(1).step_by(2).collect()
}

/// The cells of a Markdown table row; a `|` inside backticks does not occur in this document.
fn cells(line: &str) -> Vec<&str> {
    line.trim().trim_matches('|').split('|').map(str::trim).collect()
}

/// Whether `text` defines `name` as a test: `fn name(` with `#[test]` a short way above it.
fn defines_test(text: &str, name: &str) -> bool {
    let needle = format!("fn {name}(");
    text.match_indices(&needle).any(|(at, _)| {
        let from = at.saturating_sub(400);
        // Back to a character boundary: the span is only searched for an ASCII attribute.
        let from = (from..at).find(|i| text.is_char_boundary(*i)).unwrap_or(at);
        text[from..at].contains("#[test]")
    })
}

struct Problems(Vec<String>);

impl Problems {
    fn check_code(&mut self, root: &Path, line: &str, cell: &str) {
        if cell == "none" {
            return;
        }
        let spans = spans(cell);
        let (Some(place), Some(symbol)) = (spans.first(), spans.get(1)) else {
            self.0.push(format!("no `file:line` `symbol`: {line}"));
            return;
        };
        let Some((file, number)) = place.rsplit_once(':') else {
            self.0.push(format!("no line number in {place}: {line}"));
            return;
        };
        let Ok(text) = std::fs::read_to_string(root.join(file)) else {
            self.0.push(format!("no such file {file}: {line}"));
            return;
        };
        if !text.contains(symbol) {
            self.0.push(format!("{file} has no `{symbol}`: {line}"));
        }
        let number: usize = number.parse().unwrap_or(usize::MAX);
        if number == 0 || number > text.lines().count() {
            self.0
                .push(format!("{place} is past the end of the file: {line}"));
        }
    }

    fn check_test(&mut self, root: &Path, line: &str, cell: &str) {
        if let Some(why) = cell.strip_prefix("untested:") {
            if why.trim().len() < 12 {
                self.0.push(format!("`untested:` needs a reason: {line}"));
            }
            return;
        }
        let tests = spans(cell);
        if tests.is_empty() {
            self.0.push(format!("no test and no `untested: why`: {line}"));
        }
        for test in tests {
            let Some((file, name)) = test.split_once("::") else {
                self.0.push(format!("a test is `file::name`, got {test}: {line}"));
                continue;
            };
            match std::fs::read_to_string(root.join(file)) {
                Ok(text) if defines_test(&text, name) => {}
                Ok(_) => self.0.push(format!("{file} has no test `{name}`: {line}")),
                Err(_) => self.0.push(format!("no such test file {file}: {line}")),
            }
        }
    }
}

#[test]
fn every_row_of_the_threat_model_cites_real_code_and_a_test_or_a_reason() {
    let root = repo_root();
    let doc = read(&root, "docs/security/threat-model.md");
    let mut problems = Problems(Vec::new());
    let mut rows = 0;
    let mut in_table = false;
    for line in doc.lines() {
        if line.starts_with("| Mitigation | Code | Test |") {
            in_table = true;
            continue;
        }
        if !line.starts_with('|') {
            in_table = false;
            continue;
        }
        if !in_table || line.starts_with("|---") {
            continue;
        }
        rows += 1;
        let row = cells(line);
        if row.len() != 3 {
            problems.0.push(format!("{} cells, not 3: {line}", row.len()));
            continue;
        }
        problems.check_code(&root, line, row[1]);
        problems.check_test(&root, line, row[2]);
    }
    assert!(rows > 100, "{rows} rows: the tables were not found");
    assert!(problems.0.is_empty(), "{}", problems.0.join("\n"));
}

/// The brief's list of boundaries: a boundary cannot be dropped without changing this test.
#[test]
fn the_threat_model_covers_every_boundary() {
    let doc = read(&repo_root(), "docs/security/threat-model.md");
    for boundary in [
        "Agent WebSocket server and token",
        "MCP over stdio and HTTP",
        "Planted links in `.regia/agent/`",
        "Project files from a cloned repository",
        "Mods, Workshop content and the Lua sandbox",
        "Dynamic extensions and `[[tool]]` commands",
        "Network packets and transport identity",
        "Content delivery client and server",
        "Save files, replays and demos",
        "Paks and cooked assets",
        "Importers",
        "Crash upload and log scrubbing",
        "The DCC feed",
        "The hot-reload dylib",
        "`regia engine verify` and release trees",
    ] {
        assert!(
            doc.lines().any(|l| l.starts_with("## ") && l.ends_with(boundary)),
            "no section for {boundary}"
        );
        let in_overview = doc
            .lines()
            .any(|l| l.starts_with("| ") && l.contains(&format!("[{boundary}]")));
        assert!(in_overview, "no overview row for {boundary}");
    }
    assert!(doc.contains("## Trusted by design"));
}

/// No absolute claims, in the document or the book page: say what was tested.
#[test]
fn the_security_pages_make_no_absolute_claim() {
    let root = repo_root();
    for page in ["docs/security/threat-model.md", "book/user-guide/security.md"] {
        let text = read(&root, page).to_ascii_lowercase();
        for claim in [
            "is secure",
            "are secure",
            "fully secure",
            "guarantee",
            "unhackable",
            "impossible to",
            "cannot be exploited",
            "safe from",
        ] {
            assert!(!text.contains(claim), "{page} says {claim:?}");
        }
    }
}
