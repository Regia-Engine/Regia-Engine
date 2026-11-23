// SPDX-License-Identifier: MPL-2.0
//! Duplicate-dependency ratchet (`cargo xtask duplicate-deps`, ADR-3301).
//!
//! `cargo tree -d --workspace -e normal` lists every crate the workspace resolves to more
//! than one version. Each such crate must be recorded in the checked-in baseline
//! [`BASELINE`] with the versions and a reason it stays (or the upstream move that removes
//! it). The check fails when a duplicate crate name is missing from the baseline or a row
//! carries no reason; removing a duplicate is always allowed — `--update` shrinks the
//! baseline. A preflight step runs this check (`cargo xtask preflight`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The checked-in baseline, relative to the workspace root.
pub const BASELINE: &str = "docs/ci/duplicate-deps.txt";

/// The `cargo tree` invocation the baseline mirrors. `--locked` keeps the check on the
/// committed `Cargo.lock`; `--all-features` and the four shipping `--target`s (the `deny.toml`
/// list, as `Graph::tree` uses) keep Windows/Android-only and optional-feature duplicates
/// visible to the ratchet (review fixrev-03-k finding 2).
const TREE_ARGS: &[&str] = &[
    "tree",
    "-d",
    "--workspace",
    "-e",
    "normal",
    "--locked",
    "--all-features",
    "--target",
    "x86_64-unknown-linux-gnu",
    "--target",
    "x86_64-pc-windows-msvc",
    "--target",
    "aarch64-linux-android",
    "--target",
    "x86_64-linux-android",
];

/// Header written at the top of the baseline file.
const HEADER: &str = "\
# Regia Engine duplicate-dependency baseline (format_version: 1).
# Checked by `cargo xtask duplicate-deps` and `cargo xtask preflight`.
# Every crate `cargo tree -d --workspace -e normal --locked --all-features` resolves to more
# than one version on any of the four shipping targets (x86_64 Linux/Windows, aarch64/x86_64
# Android) must appear below with each resolved version and the reason it stays (or what
# upstream move removes it). A new duplicate fails the check, and so does a recorded duplicate
# gaining a version: unify the versions, or run `cargo xtask duplicate-deps --update` and
# write the reason. Losing a version is a note; rows may leave only when the duplicate is gone.
# format: name | versions (comma-separated) | reason
";

/// One crate resolved to more than one version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duplicate {
    /// Crate name (`cargo tree` spelling).
    pub name: String,
    /// Every resolved version, sorted, as `vX.Y.Z`.
    pub versions: Vec<String>,
}

/// Parse `cargo tree -d` stdout into the duplicate list. Top-level lines are
/// `name vX.Y.Z` at column 0; the reverse-dependency lines drawn with `├──`/`└──`/`│`
/// start at column 0 too, so they are skipped by their first byte. A duplicate name
/// appears once per resolved version; `cargo tree -d` can also list a name twice at the
/// same version (two trees with different feature sets) — that is not a duplicate and
/// drops out with the fewer-than-two-versions filter.
pub fn parse_tree_duplicates(text: &str) -> Vec<Duplicate> {
    let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.lines() {
        let first = line.chars().next().unwrap_or(' ');
        if first.is_whitespace() || matches!(first, '├' | '└' | '│' | '(') {
            continue;
        }
        let Some((name, rest)) = line.split_once(' ') else {
            continue;
        };
        let version = rest.trim_start_matches('v');
        let version = version.split_whitespace().next().unwrap_or_default();
        if name.is_empty() || version.is_empty() {
            continue;
        }
        let entry = names.entry(name.to_string()).or_default();
        let version = format!("v{version}");
        // Record each resolved version once.
        if !entry.contains(&version) {
            entry.push(version);
        }
    }
    names
        .into_iter()
        .filter(|(_, versions)| versions.len() > 1)
        .map(|(name, mut versions)| {
            versions.sort();
            Duplicate { name, versions }
        })
        .collect()
}

/// One baseline row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    name: String,
    versions: Vec<String>,
    reason: String,
}

fn parse_baseline(text: &str) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '|').map(str::trim).collect();
        if parts.len() != 3 || parts[0].is_empty() {
            bail!("{BASELINE}:{}: expected `name | versions | reason`", i + 1);
        }
        let versions: Vec<String> = parts[1]
            .split(',')
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        if versions.is_empty() {
            bail!("{BASELINE}:{}: `{}` lists no versions", i + 1, parts[0]);
        }
        rows.push(Row {
            name: parts[0].to_string(),
            versions,
            reason: parts[2].to_string(),
        });
    }
    Ok(rows)
}

fn render_baseline(rows: &[Row]) -> String {
    let mut out = String::from(HEADER);
    for row in rows {
        out.push_str(&row.name);
        out.push_str(" | ");
        out.push_str(&row.versions.join(", "));
        out.push_str(" | ");
        out.push_str(&row.reason);
        out.push('\n');
    }
    out
}

fn cargo_tree(root: &Path) -> Result<String> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(TREE_ARGS)
        .current_dir(root)
        .output()
        .context("running `cargo tree -d`")?;
    if !output.status.success() {
        bail!(
            "`cargo {}` failed:\n{}",
            TREE_ARGS.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Compare `current` against the baseline rows; the errors a check run reports. A duplicate
/// missing from the baseline is a failure; a row without a reason is a failure; a recorded
/// duplicate whose version set gained a version is a failure (the ratchet exists to stop the
/// list from growing — `--update` plus a written reason is how growth is accepted); a baseline
/// row whose duplicate lost versions or is gone entirely is returned as a note (the caller
/// prints it; only `--update` acts on it).
fn compare(current: &[Duplicate], rows: &[Row]) -> (Vec<String>, Vec<String>) {
    let known: BTreeMap<&str, &Row> = rows.iter().map(|r| (r.name.as_str(), r)).collect();
    let mut errors = Vec::new();
    let mut notes = Vec::new();
    for dup in current {
        match known.get(dup.name.as_str()) {
            None => errors.push(format!(
                "new duplicate `{}` ({}): unify the versions (`cargo update -p {} --precise <v>` \
                 or bump the direct dependency), or accept it with `cargo xtask duplicate-deps \
                 --update` and write the reason in {BASELINE}",
                dup.name,
                dup.versions.join(", "),
                dup.name
            )),
            Some(row) => {
                if row.reason.is_empty() {
                    errors.push(format!(
                        "duplicate `{}` has no reason: write why it stays in {BASELINE}",
                        dup.name
                    ));
                }
                let mut baseline_versions = row.versions.clone();
                baseline_versions.sort();
                if baseline_versions != dup.versions {
                    // Growth (a version in the current set that the baseline row does not list)
                    // is the failure the ratchet exists to catch; a shrunken set is a note.
                    let grew = dup.versions.iter().any(|v| !row.versions.contains(v));
                    if grew {
                        errors.push(format!(
                            "duplicate `{}` gained a version: baseline [{}], now [{}] — unify \
                             them, or accept the growth with `cargo xtask duplicate-deps --update` \
                             and write the reason in {BASELINE}",
                            dup.name,
                            row.versions.join(", "),
                            dup.versions.join(", ")
                        ));
                    } else {
                        notes.push(format!(
                            "duplicate `{}` changed version set: baseline [{}], now [{}] \
                             (`cargo xtask duplicate-deps --update` records it)",
                            dup.name,
                            row.versions.join(", "),
                            dup.versions.join(", ")
                        ));
                    }
                }
            }
        }
    }
    let current_names: std::collections::BTreeSet<&str> = current.iter().map(|d| d.name.as_str()).collect();
    for row in rows {
        if !current_names.contains(row.name.as_str()) {
            notes.push(format!(
                "duplicate `{}` is gone: `cargo xtask duplicate-deps --update` drops the row",
                row.name
            ));
        }
    }
    (errors, notes)
}

/// `cargo xtask duplicate-deps [--update]`.
pub fn run(root: &Path, update: bool) -> Result<Vec<String>> {
    let current = parse_tree_duplicates(&cargo_tree(root)?);
    let path: PathBuf = root.join(BASELINE);
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let rows = parse_baseline(&text)?;
    if update {
        let reasons: BTreeMap<&str, &str> = rows
            .iter()
            .map(|r| (r.name.as_str(), r.reason.as_str()))
            .collect();
        let merged: Vec<Row> = current
            .iter()
            .map(|dup| Row {
                name: dup.name.clone(),
                versions: dup.versions.clone(),
                reason: reasons
                    .get(dup.name.as_str())
                    .map(|r| (*r).to_string())
                    .unwrap_or_default(),
            })
            .collect();
        let rendered = render_baseline(&merged);
        let tmp = path.with_extension("txt.tmp");
        std::fs::write(&tmp, rendered).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("renaming {}", path.display()))?;
        let fresh = merged.iter().filter(|r| r.reason.is_empty()).count();
        println!("{BASELINE}: {} duplicate(s) recorded", merged.len());
        if fresh > 0 {
            println!("{BASELINE}: {fresh} row(s) need a reason");
        }
        return Ok(Vec::new());
    }
    let (errors, notes) = compare(&current, &rows);
    for note in &notes {
        println!("note: {note}");
    }
    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "\
bitflags v1.3.2
└── some crate
bitflags v2.4.1
├── other crate
└── third crate
glam v0.30.9
└── nalgebra
glam v0.33.12
└── bevy_ecs
";

    #[test]
    fn parse_lists_each_duplicate_once_per_version() {
        let dupes = parse_tree_duplicates(TREE);
        assert_eq!(dupes.len(), 2, "{dupes:?}");
        assert_eq!(dupes[0].name, "bitflags");
        assert_eq!(dupes[0].versions, ["v1.3.2", "v2.4.1"]);
        assert_eq!(dupes[1].name, "glam");
        assert_eq!(dupes[1].versions, ["v0.30.9", "v0.33.12"]);
    }

    #[test]
    fn parse_ignores_indented_reverse_dependency_lines() {
        let dupes = parse_tree_duplicates(TREE);
        assert!(
            dupes
                .iter()
                .all(|d| !d.name.contains("└") && !d.name.contains('├')),
            "{dupes:?}"
        );
    }

    #[test]
    fn parse_dedups_same_version_listed_twice() {
        let dupes = parse_tree_duplicates(TREE);
        assert_eq!(dupes[0].versions, ["v1.3.2", "v2.4.1"]);
        let doubled = parse_tree_duplicates("syn v2.0.119 (*)\nsyn v2.0.119\nsyn v3.0.6\n");
        assert_eq!(doubled.len(), 1, "{doubled:?}");
        assert_eq!(doubled[0].versions, ["v2.0.119", "v3.0.6"]);
    }

    #[test]
    fn same_version_listed_twice_is_not_a_duplicate() {
        // `cargo tree -d` lists e.g. winnow v1.0.4 twice (different feature trees);
        // one resolved version is not a duplicate.
        let dupes = parse_tree_duplicates("winnow v1.0.4\n├── a v1\n└── b v1\nwinnow v1.0.4 (*)\n├── c v1\n");
        assert!(dupes.is_empty(), "{dupes:?}");
    }

    fn rows(text: &str) -> Vec<Row> {
        parse_baseline(text).expect("baseline parses")
    }

    #[test]
    fn new_duplicate_is_an_error() {
        let dupes = parse_tree_duplicates(TREE);
        let rows = rows("bitflags | v1.3.2, v2.4.1 | upstream split\n");
        let (errors, _) = compare(&dupes, &rows);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("glam"), "{errors:?}");
        assert!(errors[0].contains("cargo update -p glam"), "{errors:?}");
    }

    #[test]
    fn missing_reason_is_an_error() {
        let dupes = parse_tree_duplicates(TREE);
        let rows = rows("bitflags | v1.3.2, v2.4.1 | ok\nglam | v0.30.9, v0.33.12 | \n");
        let (errors, _) = compare(&dupes, &rows);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("glam"), "{errors:?}");
    }

    #[test]
    fn recorded_duplicates_pass_and_shrinking_is_a_note() {
        let dupes = parse_tree_duplicates(TREE);
        let rows = rows("glam | v0.30.9, v0.31.0, v0.33.12 | gizmo\nbitflags | v1.3.2, v2.4.1 | ok\n");
        let (errors, notes) = compare(&dupes, &rows);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("glam"), "{notes:?}");
    }

    #[test]
    fn a_recorded_duplicate_gaining_a_version_is_an_error() {
        // The ratchet exists to stop the duplicate list from growing: a merge that adds a third
        // resolved version to a recorded duplicate must fail, not print a note (review
        // fixrev-03-k finding 1).
        let dupes = parse_tree_duplicates(TREE);
        let rows = rows("glam | v0.30.9, v0.31.0 | gizmo\nbitflags | v1.3.2, v2.4.1 | ok\n");
        let (errors, notes) = compare(&dupes, &rows);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("glam"), "{errors:?}");
        assert!(errors[0].contains("gained a version"), "{errors:?}");
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_recorded_duplicate_losing_a_version_is_a_note() {
        let dupes = parse_tree_duplicates(TREE);
        let rows = rows("glam | v0.30.9, v0.30.10, v0.33.12 | gizmo\nbitflags | v1.3.2, v2.4.1 | ok\n");
        let (errors, notes) = compare(&dupes, &rows);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("glam"), "{notes:?}");
    }

    #[test]
    fn version_order_in_the_baseline_does_not_matter() {
        let dupes = parse_tree_duplicates(TREE);
        let rows = rows("glam | v0.33.12, v0.30.9 | ok\nbitflags | v2.4.1, v1.3.2 | ok\n");
        let (errors, notes) = compare(&dupes, &rows);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn gone_duplicate_is_a_note_not_an_error() {
        let dupes = parse_tree_duplicates(TREE);
        let rows = rows(
            "bitflags | v1.3.2, v2.4.1 | ok\nglam | v0.30.9, v0.33.12 | ok\nsyn | v2.0.0, v3.0.0 | gone\n",
        );
        let (errors, notes) = compare(&dupes, &rows);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(notes.iter().any(|n| n.contains("syn")), "{notes:?}");
    }

    #[test]
    fn baseline_round_trips_through_render_and_parse() {
        let rows = rows("bitflags | v1.3.2, v2.4.1 | waits on upstream\n");
        let text = render_baseline(&rows);
        assert!(text.contains("format_version: 1"));
        assert_eq!(parse_baseline(&text).expect("parse"), rows);
    }

    #[test]
    fn malformed_row_fails_with_line_number() {
        let err = match parse_baseline("not a row\n") {
            Err(err) => err.to_string(),
            Ok(_) => panic!("a row without `|` parsed"),
        };
        assert!(err.contains(":1:"), "{err}");
    }
}
