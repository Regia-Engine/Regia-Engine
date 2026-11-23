// SPDX-License-Identifier: MPL-2.0
//! Every file under `.github/workflows/` is well-formed workflow YAML (ADR-2610).
//!
//! The workspace has no YAML parser and this job may not add one, so the check is a strict reader for
//! the block-style subset the workflows use: spaces only, a mapping or list item per line, block
//! scalars (`|`, `>`) skipped, no duplicate key in one mapping, balanced quotes and flow lists. On top
//! of the syntax: the top level has `name`, `on` and `jobs`; every job has `runs-on` and `steps` (or
//! calls a reusable workflow with `uses`); every `needs` names a job of the file. `actionlint`, where
//! installed, remains the deeper check (`docs/06` § 5).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// What the reader found in a file.
#[derive(Debug, Default)]
struct Shape {
    /// Top-level keys.
    top: BTreeSet<String>,
    /// Job id -> its direct keys, and its `needs` names.
    jobs: Vec<(String, BTreeSet<String>, Vec<String>)>,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Splits `key: value` at the first `: ` or a trailing `:` outside quotes; `None` when it is not a mapping line.
fn split_key(content: &str) -> Option<(&str, &str)> {
    if content.starts_with(['"', '\'']) {
        let quote = content.chars().next()?;
        let end = content[1..].find(quote)? + 1;
        let rest = content[end + 1..].strip_prefix(':')?;
        return Some((&content[..=end], rest.trim()));
    }
    let at = content
        .find(": ")
        .or_else(|| content.strip_suffix(':').map(str::len))?;
    let key = &content[..at];
    // `${{ a }}: b` and URLs aside, a key has no spaces before the colon in these workflows' mapping lines.
    if key.is_empty() || key.contains(" #") {
        return None;
    }
    Some((key, content[at..].trim_start_matches(':').trim()))
}

fn balanced(value: &str) -> bool {
    let v = value.trim();
    if v.starts_with('"') {
        return v.len() >= 2 && v.ends_with('"') && !v.ends_with("\\\"") || v.matches('"').count() >= 2;
    }
    if v.starts_with('\'') {
        return v.len() >= 2 && v.ends_with('\'');
    }
    if v.starts_with('[') {
        return v.ends_with(']') && v.matches('[').count() == v.matches(']').count();
    }
    if v.starts_with('{') && !v.starts_with("${{") {
        return v.ends_with('}');
    }
    true
}

fn is_block_scalar(value: &str) -> bool {
    let v = value.trim();
    matches!(v.chars().next(), Some('|' | '>'))
        && v.len() <= 3
        && v[1..].chars().all(|c| matches!(c, '-' | '+' | '0'..='9'))
}

/// Reads `text`; the error names the 1-based line.
fn read(text: &str) -> Result<Shape, String> {
    let mut shape = Shape::default();
    // (indent, keys seen) of the open mappings.
    let mut stack: Vec<(usize, BTreeSet<String>)> = Vec::new();
    let mut block: Option<usize> = None;
    let mut job: Option<usize> = None;
    let mut in_needs = false;
    let mut in_jobs = false;
    for (n, raw) in text.lines().enumerate() {
        let at = |msg: &str| format!("line {}: {msg}", n + 1);
        if raw.contains('\t') {
            return Err(at("tab character"));
        }
        if raw.ends_with('\r') {
            return Err(at("carriage return"));
        }
        if raw.trim().is_empty() {
            continue;
        }
        let indent = indent_of(raw);
        if let Some(b) = block {
            if indent > b {
                continue;
            }
            block = None;
        }
        let content = raw.trim();
        if content.starts_with('#') {
            continue;
        }
        if content == "---" || content == "..." {
            continue;
        }
        in_needs = in_needs && indent > 4 && content.starts_with("- ");
        let (key_indent, key_text, dash) = if let Some(rest) = content.strip_prefix("- ") {
            (indent + 2, rest.trim_start(), true)
        } else if content == "-" {
            continue;
        } else {
            (indent, content, false)
        };
        stack.retain(|(i, _)| *i <= indent);
        if in_needs {
            if let Some(j) = job {
                shape.jobs[j].2.push(key_text.trim_matches('"').to_owned());
            }
            continue;
        }
        let Some((key, value)) = split_key(key_text) else {
            if dash {
                continue; // a plain list item
            }
            return Err(at(&format!("not a `key: value` line: `{content}`")));
        };
        if !balanced(value) {
            return Err(at(&format!("unbalanced quote or bracket in `{value}`")));
        }
        if dash {
            stack.retain(|(i, _)| *i < key_indent);
            stack.push((key_indent, BTreeSet::new()));
        } else if stack.last().is_none_or(|(i, _)| *i != indent) {
            if stack.last().is_some_and(|(i, _)| indent < *i) {
                return Err(at("indent does not match any open mapping"));
            }
            stack.push((indent, BTreeSet::new()));
        }
        let Some((_, keys)) = stack.last_mut() else {
            return Err(at("no open mapping"));
        };
        if !keys.insert(key.to_owned()) {
            return Err(at(&format!("duplicate key `{key}`")));
        }
        if !indent.is_multiple_of(2) {
            return Err(at("odd indent"));
        }
        if !dash && indent == 0 {
            shape.top.insert(key.to_owned());
            in_jobs = key == "jobs";
            job = None;
        }
        // `jobs:` children sit at indent 2 under the top-level `jobs`.
        if !dash && indent == 2 && in_jobs {
            shape.jobs.push((key.to_owned(), BTreeSet::new(), Vec::new()));
            job = Some(shape.jobs.len() - 1);
        } else if !dash
            && indent == 4
            && let Some(j) = job
        {
            shape.jobs[j].1.insert(key.to_owned());
            if key == "needs" {
                if value.is_empty() {
                    in_needs = true;
                } else {
                    let list = value.trim_start_matches('[').trim_end_matches(']');
                    shape.jobs[j].2.extend(
                        list.split(',')
                            .map(|s| s.trim().trim_matches('"').to_owned())
                            .filter(|s| !s.is_empty()),
                    );
                }
            }
        }
        if is_block_scalar(value) {
            block = Some(indent);
        }
    }
    Ok(shape)
}

/// The structural rules on top of the syntax.
fn check(text: &str) -> Result<(), String> {
    let shape = read(text)?;
    for key in ["name", "on", "jobs"] {
        if !shape.top.contains(key) {
            return Err(format!("no top-level `{key}`"));
        }
    }
    if shape.jobs.is_empty() {
        return Err("`jobs` has no job".into());
    }
    let ids: BTreeSet<&str> = shape.jobs.iter().map(|(id, _, _)| id.as_str()).collect();
    if ids.len() != shape.jobs.len() {
        return Err("a job id appears twice".into());
    }
    for (id, keys, needs) in &shape.jobs {
        if !keys.contains("runs-on") && !keys.contains("uses") {
            return Err(format!("job `{id}` has no `runs-on`"));
        }
        if !keys.contains("steps") && !keys.contains("uses") {
            return Err(format!("job `{id}` has no `steps`"));
        }
        for need in needs {
            if !ids.contains(need.as_str()) {
                return Err(format!(
                    "job `{id}` needs `{need}`, which is not a job of this file"
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn every_workflow_file_is_well_formed() {
    let dir = root().join(".github/workflows");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir)
        .expect("workflows dir")
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read workflow");
        if let Err(err) = check(&text) {
            panic!("{}: {err}", path.display());
        }
        seen += 1;
    }
    assert!(seen >= 4, "expected the repository's workflows, found {seen}");
}

#[test]
fn the_new_ci_jobs_are_defined() {
    let text = std::fs::read_to_string(root().join(".github/workflows/ci.yml")).expect("ci.yml");
    let shape = read(&text).expect("ci.yml reads");
    let ids: Vec<&str> = shape.jobs.iter().map(|(id, _, _)| id.as_str()).collect();
    for job in ["linux", "templates", "docs", "package", "agent-protocol"] {
        assert!(ids.contains(&job), "ci.yml has no `{job}` job: {ids:?}");
    }
}

#[test]
fn the_cla_bot_runs_only_behind_its_switch() {
    // ADR-10254: the CLA bot stays off until the owner sets the repository variable `CLA_ENABLED` to
    // `true` (docs/07-licensing.md). Every job of cla.yml carries that condition.
    let text = std::fs::read_to_string(root().join(".github/workflows/cla.yml")).expect("cla.yml");
    let shape = read(&text).expect("cla.yml reads");
    assert!(!shape.jobs.is_empty(), "cla.yml has no job");
    for (id, keys, _) in &shape.jobs {
        assert!(keys.contains("if"), "cla.yml job `{id}` has no `if:` switch");
    }
    let switches = text
        .lines()
        .filter(|l| l.trim() == "if: ${{ vars.CLA_ENABLED == 'true' }}")
        .count();
    assert_eq!(
        switches,
        shape.jobs.len(),
        "every cla.yml job is gated by CLA_ENABLED"
    );
}

#[test]
fn the_nightly_two_tag_job_is_defined() {
    let text = std::fs::read_to_string(root().join(".github/workflows/nightly.yml")).expect("nightly.yml");
    let shape = read(&text).expect("nightly.yml reads");
    assert!(
        shape.jobs.iter().any(|(id, _, _)| id == "two-tag-upgrade"),
        "nightly.yml has no `two-tag-upgrade` job"
    );
}

const GOOD: &str = "name: x\non:\n  push:\n    branches: [main]\njobs:\n  a:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/checkout@v4\n      - name: s\n        run: |\n          echo: not a key\n          echo \"a: b\"\n  b:\n    needs: [a]\n    runs-on: ubuntu-24.04\n    steps:\n      - run: \"cargo xtask x\"\n";

#[test]
fn the_reader_accepts_the_subset() {
    check(GOOD).expect("good workflow");
}

#[test]
fn the_reader_rejects_broken_files() {
    let cases = [
        (GOOD.replace("  push:", "\tpush:"), "tab"),
        (
            GOOD.replace(
                "    runs-on: ubuntu-24.04\n    steps:\n      - uses",
                "    runs-on: ubuntu-24.04\n    runs-on: x\n    steps:\n      - uses",
            ),
            "duplicate key",
        ),
        (GOOD.replace("needs: [a]", "needs: [zz]"), "needs `zz`"),
        (GOOD.replace("branches: [main]", "branches: [main"), "unbalanced"),
        (
            GOOD.replace("\n    steps:\n      - run: \"cargo xtask x\"", ""),
            "no `steps`",
        ),
        (GOOD.replace("name: x\n", ""), "no top-level `name`"),
        (
            GOOD.replace("run: \"cargo xtask x\"", "run: \"cargo xtask x"),
            "unbalanced",
        ),
        (GOOD.replace("jobs:", "jobs :"), "jobs"),
    ];
    for (text, expect) in cases {
        let err = check(&text).expect_err(expect);
        assert!(err.contains(expect), "wanted `{expect}`, got `{err}`");
    }
}
