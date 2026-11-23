// SPDX-License-Identifier: MPL-2.0
//! The GitHub workflows agree with the merge gate and the nightly registry (ADR-1379):
//!
//! - `ci.yml` runs every check `cargo xtask preflight` runs (the gate's cheap checks, in the
//!   gate's order), so a check added to the gate cannot be forgotten in CI;
//! - `ci.yml` does not skip a `docs/` file that a crate compiles in;
//! - every `cargo xtask` command in `nightly.yml` is a registered nightly job, and every job that
//!   names `nightly.yml` as its wrapper runs there, with the same command.

use std::path::{Path, PathBuf};

use xtask::ci_registry::{NIGHTLY_PATH, parse_nightly};
use xtask::preflight::{Scope, StepKind, external_includes, members, plan};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

/// The `run:` commands of a workflow, one per line, `run: |` blocks included.
fn run_lines(workflow: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut block_indent: Option<usize> = None;
    for line in workflow.lines() {
        let indent = line.len() - line.trim_start().len();
        if let Some(block) = block_indent {
            if !line.trim().is_empty() && indent > block {
                out.push(line.trim().to_string());
                continue;
            }
            block_indent = None;
        }
        let trimmed = line.trim_start().trim_start_matches("- ");
        if let Some(command) = trimmed.strip_prefix("run:") {
            let command = command.trim();
            if command == "|" {
                block_indent = Some(indent);
            } else {
                out.push(command.trim_matches('"').to_string());
            }
        }
    }
    out
}

/// The `paths-ignore` patterns of a workflow (`[a, b]` lists).
fn paths_ignore(workflow: &str) -> Vec<String> {
    workflow
        .lines()
        .filter_map(|l| l.trim().strip_prefix("paths-ignore:"))
        .flat_map(|list| {
            list.trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Whether a `paths-ignore` pattern (`dir/**` or an exact path) matches `path`.
fn ignored(pattern: &str, path: &str) -> bool {
    match pattern.strip_suffix("/**") {
        Some(dir) => path.starts_with(&format!("{dir}/")),
        None => pattern == path,
    }
}

#[test]
fn ci_runs_every_check_the_gate_runs() {
    let ci = read(".github/workflows/ci.yml");
    let runs = run_lines(&ci);
    let has = |needle: &str| runs.iter().any(|r| r.contains(needle));
    for step in plan(&Scope::Workspace, true, true, "main") {
        match &step.kind {
            StepKind::Xtask(args) => {
                let command = format!("cargo xtask {}", args.join(" "));
                assert!(
                    runs.iter().any(|r| r == &command),
                    "ci.yml does not run the gate step `{}` ({command})",
                    step.name
                );
            }
            StepKind::Cargo { args, .. } => {
                let sub = args.first().map(String::as_str).unwrap_or_default();
                let needle = match sub {
                    "fmt" => "cargo fmt --all",
                    "clippy" => "cargo clippy --workspace --all-targets",
                    "doc" => "cargo doc --workspace --no-deps",
                    "test" => "cargo test --workspace",
                    other => panic!("a new cargo gate step `{other}`: teach this test and ci.yml about it"),
                };
                assert!(
                    has(needle),
                    "ci.yml does not run `{needle}` (gate step `{}`)",
                    step.name
                );
            }
            StepKind::ConflictMarkers => {
                assert!(
                    has("git grep --untracked -l -E '^(<<<<<<<|>>>>>>>) '"),
                    "ci.yml does not refuse conflict markers like the gate"
                );
            }
        }
    }
    // Clippy denies warnings, as the gate does.
    assert!(has("-- -D warnings"), "ci.yml clippy does not deny warnings");
    assert!(
        ci.contains("RUSTDOCFLAGS: -D warnings"),
        "ci.yml rustdoc does not deny warnings"
    );
}

#[test]
fn ci_does_not_skip_docs_that_crates_compile_in() {
    let patterns = paths_ignore(&read(".github/workflows/ci.yml"));
    assert!(!patterns.is_empty(), "ci.yml has no paths-ignore at all");
    assert!(
        !patterns.iter().any(|p| p == "docs/**"),
        "ci.yml skips every docs/ change, but crates include docs/ files: {patterns:?}"
    );
    let root = root();
    let members = members(&root).expect("cargo metadata");
    let includes = external_includes(&root, &members);
    assert!(
        includes.iter().any(|(path, _)| path.starts_with("docs/")),
        "expected at least one docs/ include (docs/formats/*.ron); the include scan found none"
    );
    for (path, member) in &includes {
        for pattern in &patterns {
            assert!(
                !ignored(pattern, path),
                "ci.yml paths-ignore `{pattern}` skips {path}, which {member} compiles in"
            );
        }
    }
}

#[test]
fn nightly_workflow_and_registry_agree() {
    let workflow = read(".github/workflows/nightly.yml");
    let commands: Vec<String> = run_lines(&workflow)
        .into_iter()
        .filter(|r| r.starts_with("cargo xtask ") || r.starts_with("cargo ndk "))
        .collect();
    assert!(!commands.is_empty(), "nightly.yml runs no cargo xtask command");
    let registry = parse_nightly(&read(NIGHTLY_PATH)).expect("the nightly registry parses");
    for command in &commands {
        assert!(
            registry.jobs.iter().any(|j| &j.command == command),
            "nightly.yml runs `{command}`, which no job in {NIGHTLY_PATH} registers"
        );
    }
    for job in registry
        .jobs
        .iter()
        .filter(|j| j.wrapper.as_deref() == Some(".github/workflows/nightly.yml"))
    {
        assert!(
            commands.contains(&job.command),
            "job `{}` names nightly.yml as its wrapper, but nightly.yml does not run `{}`",
            job.id,
            job.command
        );
        assert_eq!(
            job.status.label(),
            "scheduled",
            "job `{}` runs from a CI schedule",
            job.id
        );
    }
}

#[test]
fn the_workflow_parsers_read_block_and_inline_runs() {
    let yaml = "steps:\n  - run: cargo xtask a\n  - name: b\n    run: |\n      export X=1\n      cargo xtask b --flag\n  - run: \"! git grep x\"\n";
    assert_eq!(
        run_lines(yaml),
        [
            "cargo xtask a",
            "export X=1",
            "cargo xtask b --flag",
            "! git grep x"
        ]
    );
    let on = "on:\n  push:\n    paths-ignore: [docs/evidence/**, README.md]\n";
    assert_eq!(paths_ignore(on), ["docs/evidence/**", "README.md"]);
    assert!(ignored("docs/evidence/**", "docs/evidence/a.png"));
    assert!(!ignored("docs/evidence/**", "docs/formats/a.ron"));
    assert!(ignored("README.md", "README.md"));
}
