// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask preflight`: the merge gate's cheap checks, in the gate's order, stopping at the first
//! failure with a short summary (`docs/04-conventions.md` § 11).
//!
//! The steps: the gate's conflict-marker refusal, `cargo fmt --all --check`, the book's fast fence
//! pre-check, `cargo clippy ... -D warnings`, `cargo doc --no-deps` with `RUSTDOCFLAGS=-D warnings`,
//! then the xtask checks in the farm gate's order (`check-versions`, `notices --check`, `spdx --check`,
//! `check-formats`, `book-check`, `llms-txt --check` and `onboarding-lint` (ADR-10135, ADR-10230), `check-shaders`, `check-unwraps`, `det-scan` (the lockstep scan, ADR-8013),
//! `check-layers`, `check-server-build`, `check-net-core` (the ECS-free regia_net core, ADR-8501), `check-packaging`), `check-kinds` (the kind lint over the samples, spec 101), `api-snapshot`,
//! `doc-examples` (the compiled doc examples of the stable crates),
//! `agent-snapshot` (the agent protocol surface ratchet, ADR-3335), the
//! feature matrix (`check-features`: clippy over every optional
//! Cargo feature set the default build never compiles, ADR-2322) and `cargo test --no-run`. `--changed` limits the cargo steps to the crates a branch touched (plus the
//! workspace crates that depend on them, so a broken caller is still found, and the crates that
//! `include_str!` / `include_bytes!` a changed file from outside their directory); the xtask checks always
//! run whole. Without `--no-dependents`, a workspace-wide file (`Cargo.lock`, the root `Cargo.toml`,
//! `rustfmt.toml`, `.cargo/`, and the rest of `WORKSPACE_WIDE`) still widens the cargo steps to the whole
//! workspace. With `--no-dependents` those files are set aside and the scope stays on the crates that own a
//! changed file, so a lockfile edit does not link every test binary (ADR-8441). A workspace-scoped
//! `test build` is skipped when the cargo target filesystem has under 500 GB free.
//! `--keep-going` runs every step and lists every failure; `--timings` writes the step
//! times as JSON.
//!
//! Never edits a file: a formatting failure prints the command that fixes it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use serde::{Deserialize, Serialize};

/// The workstation build wrapper. Lanes there run every build through it (`docs/04-conventions.md` § 11).
pub const LOCALBUILD: &str = "/srv/regia-scratch/localbuild.sh";

/// Lines of a failing step's output shown in the summary.
const TAIL_LINES: usize = 40;

/// Files at the workspace root (or under `.cargo/`) that affect every crate.
///
/// Without `--no-dependents`, a change here widens `--changed` to the whole workspace. With
/// `--no-dependents` they are set aside ([`scope_choice`], ADR-8441): the fmt step is already
/// `cargo fmt --all`, and a scoped cargo run still reads `.cargo/` and `clippy.toml`.
const WORKSPACE_WIDE: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rustfmt.toml",
    ".rustfmt.toml",
    "clippy.toml",
    ".clippy.toml",
    "deny.toml",
    "rust-toolchain.toml",
    "rust-toolchain",
];

/// Free space, in gibibytes, required before a workspace `test build`. Under this the step is not
/// started (ADR-8441). The count is `free bytes / 2^30`, the same unit `df -BG` prints as `G`.
pub const WORKSPACE_TEST_BUILD_MIN_FREE_GB: u64 = 500;

/// One gibibyte. The disk guard's "GB" is this unit.
const GIB: u64 = 1024 * 1024 * 1024;

/// Set to `1` on a gate tree to run a workspace `test build` below
/// [`WORKSPACE_TEST_BUILD_MIN_FREE_GB`].
pub const WORKSPACE_TEST_BUILD_OVERRIDE_ENV: &str = "REGIA_PREFLIGHT_ALLOW_WORKSPACE_TEST_BUILD";

/// A merge-conflict marker at a line start: `fgate.sh` and `gate.sh` refuse a commit that has one.
const CONFLICT_MARKER: &str = "^(<<<<<<<|>>>>>>>) ";

/// The files the gate searches for `CONFLICT_MARKER`.
const CONFLICT_GLOBS: &[&str] = &["*.rs", "*.toml", "*.md", "*.wgsl"];

/// Arguments of `cargo xtask preflight`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Limit clippy, rustdoc and the test build to the crates that changed against `--base` (and
    /// the workspace crates that depend on them).
    #[arg(long)]
    pub changed: bool,
    /// With `--changed`: only the changed crates, not their dependents.
    #[arg(long, requires = "changed")]
    pub no_dependents: bool,
    /// The ref `--changed` diffs against (the merge base with it is used).
    #[arg(long, default_value = "main")]
    pub base: String,
    /// Skip the last step, `cargo test --workspace --no-run`.
    #[arg(long)]
    pub no_build: bool,
    /// Skip the feature-matrix step (`cargo xtask check-features`).
    #[arg(long)]
    pub no_features: bool,
    /// Print the steps that would run and exit.
    #[arg(long)]
    pub list: bool,
    /// Run every step even after one fails, and list every failure at the end (one fix round
    /// instead of one per failing step). The exit code is still 1 when any step failed.
    #[arg(long, short = 'k')]
    pub keep_going: bool,
    /// Write each step's time and outcome to this file as JSON (written atomically).
    #[arg(long, value_name = "FILE")]
    pub timings: Option<PathBuf>,
}

/// Which crates the cargo steps cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// `--workspace`.
    Workspace,
    /// `-p a -p b`; empty means nothing to build (only docs or scripts changed).
    Packages(Vec<String>),
}

impl Scope {
    /// The cargo package selection arguments, or `None` when there is nothing to build.
    fn cargo_args(&self) -> Option<Vec<String>> {
        match self {
            Scope::Workspace => Some(vec!["--workspace".into()]),
            Scope::Packages(names) if names.is_empty() => None,
            Scope::Packages(names) => {
                Some(names.iter().flat_map(|n| ["-p".to_string(), n.clone()]).collect())
            }
        }
    }
}

/// What one step runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKind {
    /// `cargo <args>` with extra environment.
    Cargo {
        /// Arguments after `cargo`.
        args: Vec<String>,
        /// Extra environment variables.
        env: Vec<(String, String)>,
    },
    /// This xtask binary with a sub-command.
    Xtask(Vec<String>),
    /// The gate's refusal check: no tracked or untracked (not ignored) file of the gate's kinds has
    /// a conflict-marker line.
    ConflictMarkers,
}

/// One preflight step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Short name shown in the summary.
    pub name: String,
    /// What it runs.
    pub kind: StepKind,
    /// The command that fixes a failure, when there is a mechanical one.
    pub fix: Option<&'static str>,
}

impl Step {
    fn cargo(name: &str, args: &[&str], tail: Vec<String>) -> Self {
        let mut all: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        all.extend(tail);
        Step {
            name: name.into(),
            kind: StepKind::Cargo {
                args: all,
                env: Vec::new(),
            },
            fix: None,
        }
    }

    fn xtask(name: &str, args: &[&str]) -> Self {
        Step {
            name: name.into(),
            kind: StepKind::Xtask(args.iter().map(|a| (*a).to_string()).collect()),
            fix: None,
        }
    }

    /// The command line, for `--list` and failure summaries.
    pub fn display(&self) -> String {
        if self.name == "perf budget counts" {
            return "cargo xtask perf-budgets --counts-only --enforce".into();
        }
        match &self.kind {
            StepKind::Cargo { args, env } => {
                let env: String = env.iter().map(|(k, v)| format!("{k}=\"{v}\" ")).collect();
                format!("{env}cargo {}", args.join(" "))
            }
            StepKind::Xtask(args) => format!("cargo xtask {}", args.join(" ")),
            StepKind::ConflictMarkers => format!(
                "git grep --untracked -l -E '{CONFLICT_MARKER}' -- {}",
                CONFLICT_GLOBS
                    .iter()
                    .map(|g| format!("'{g}'"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        }
    }
}

/// The steps for `scope`, in gate order. `build` adds the final test build; `features` adds the
/// feature matrix (`check-features`, limited to the scope's crates when the scope is a crate list).
/// `base` is forwarded to the snapshot ratchets so an integration branch diffs fragments against
/// the same merge-base `--changed` uses.
pub fn plan(scope: &Scope, build: bool, features: bool, base: &str) -> Vec<Step> {
    let pkgs = scope.cargo_args();
    let with_pkgs = |extra: &[&str]| -> Vec<String> {
        let mut v = pkgs.clone().unwrap_or_default();
        v.extend(extra.iter().map(|s| (*s).to_string()));
        v
    };
    let mut steps = vec![Step {
        name: "conflict markers".into(),
        kind: StepKind::ConflictMarkers,
        fix: None,
    }];
    let mut fmt = Step::cargo("fmt", &["fmt", "--all", "--check"], Vec::new());
    fmt.fix = Some("cargo fmt --all");
    steps.push(fmt);
    steps.push(Step::xtask("book fences", &["book-check", "--fences"]));
    if pkgs.is_some() {
        steps.push(Step::cargo(
            "clippy",
            &["clippy"],
            with_pkgs(&["--all-targets", "--", "-D", "warnings"]),
        ));
        let mut doc = Step::cargo("rustdoc", &["doc"], with_pkgs(&["--no-deps"]));
        if let StepKind::Cargo { env, .. } = &mut doc.kind {
            env.push(("RUSTDOCFLAGS".into(), "-D warnings".into()));
        }
        steps.push(doc);
    }
    // The farm gate's order (`fgate-remote.sh`). `check-formats` is a gate step too.
    for (name, args) in [
        ("check-versions", &["check-versions"][..]),
        ("duplicate-deps", &["duplicate-deps"][..]),
        ("notices", &["notices", "--check"]),
        ("demo-notices", &["demo-notices", "--check"]),
        // The SPDX line on every first-party .rs/.wgsl file (ADR-10225): seconds, no build.
        ("spdx", &["spdx", "--check"]),
        ("check-formats", &["check-formats"]),
        ("book-check", &["book-check"]),
        // ADR-10135, ADR-10230: the assistant index is current and the onboarding files agree
        // with the contract and the CLI. Seconds, no build.
        ("llms-txt", &["llms-txt", "--check"]),
        ("onboarding-lint", &["onboarding-lint"]),
        ("check-shaders", &["check-shaders"]),
        ("check-unwraps", &["check-unwraps"]),
        // The lockstep determinism scan (ADR-8013): seconds, no build.
        ("det-scan", &["det-scan"]),
        ("check-layers", &["check-layers"]),
        ("check-server-build", &["check-server-build"]),
        // Spec 120 § 15.5 (ADR-8501): the regia_net core builds without `ecs`, links no ECS crate
        // and runs its headless two-client example.
        ("check-net-core", &["check-net-core"]),
        ("check-packaging", &["check-packaging"]),
        // The kind lint over the samples (spec 101 § 5, ADR-2488): not a farm-gate step yet.
        ("check-kinds", &["check-kinds"]),
        ("api-snapshot", &["api-snapshot"]),
        // The JSON examples in the agent guide against the protocol (ratchet, docs/ci/agent-examples.toml).
        ("agent-examples", &["agent-examples"]),
        // The compiled doc examples of the stable crates may not drop (ADR-2527 crates).
        ("doc-examples", &["doc-examples"]),
    ] {
        let mut step = Step::xtask(name, args);
        if name == "notices" {
            // THIRD_PARTY_NOTICES.md is generated.
            step.fix = Some("cargo xtask notices");
        }
        if name == "duplicate-deps" {
            step.fix =
                Some("unify the versions, or `cargo xtask duplicate-deps --update` and write the reason");
        }
        if name == "llms-txt" {
            step.fix = Some("cargo xtask llms-txt");
        }
        if name == "demo-notices" {
            step.fix = Some("cargo xtask demo-notices");
        }
        if name == "spdx" {
            step.fix = Some("cargo xtask spdx --write");
        }
        if name == "api-snapshot" {
            step.fix = Some("cargo xtask api-snapshot --update --breaking \"<why>\"");
        }
        if name == "agent-snapshot" {
            step.fix = Some("cargo xtask agent-snapshot --update");
        }
        if name == "doc-examples" {
            step.fix = Some("cargo xtask doc-examples --update");
        }
        steps.push(step);
    }
    // The agent-surface ratchet: forward `--base` so the fragment diff uses the same merge-base
    // as `--changed` (on an integ lane the merge-base with `main` is old, and fragments other
    // lanes merged would otherwise count as "touched by this branch").
    let mut agent_snapshot_args = vec!["agent-snapshot".to_owned()];
    if base != "main" {
        agent_snapshot_args.push("--base".to_owned());
        agent_snapshot_args.push(base.to_owned());
    }
    steps.push(Step {
        name: "agent-snapshot".into(),
        kind: StepKind::Xtask(agent_snapshot_args),
        fix: Some("cargo xtask agent-snapshot --update"),
    });
    if features {
        match scope {
            Scope::Workspace => steps.push(Step::xtask("features", &["check-features"])),
            Scope::Packages(names) if !names.is_empty() => {
                let list = names.join(",");
                steps.push(Step::xtask("features", &["check-features", "--packages", &list]));
            }
            Scope::Packages(_) => {}
        }
    }
    if build && pkgs.is_some() {
        steps.push(Step::cargo("test build", &["test"], with_pkgs(&["--no-run"])));
    }
    steps.push(Step {
        name: "perf budget counts".into(),
        kind: StepKind::Cargo {
            args: vec!["test".into(), "--workspace".into()],
            env: Vec::new(),
        },
        fix: None,
    });
    steps
}

/// The net-soak gate row (ADR-7899): `cargo xtask net-soak --plan gate`. Appended by [`run`] only
/// while `crate::net_soak::gate_row_enabled()` holds (on since the v0.7 freeze, ADR-10265).
pub fn net_soak_step() -> Step {
    Step {
        name: "net-soak".into(),
        kind: StepKind::Xtask(vec!["net-soak".into(), "--plan".into(), "gate".into()]),
        fix: None,
    }
}

/// One workspace member, from `cargo metadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Package name.
    pub name: String,
    /// Manifest directory relative to the workspace root, `/`-separated.
    pub dir: String,
    /// Names of the workspace members this one depends on (any kind).
    pub deps: Vec<String>,
}

/// Maps changed files to the members that own them. `None` when a workspace-wide file changed.
pub fn owners(files: &[String], members: &[Member]) -> Option<BTreeSet<String>> {
    owners_with(files, members, &[])
}

/// The member whose manifest directory is the longest prefix of `file` (`samples/x/android`
/// beats `samples/x`).
fn owner_of<'a>(file: &str, members: &'a [Member]) -> Option<&'a Member> {
    members
        .iter()
        .filter(|m| file.starts_with(&format!("{}/", m.dir)))
        .max_by_key(|m| m.dir.len())
}

/// A root file, or anything under `.cargo/`, that [`owners_with`] treats as the whole workspace.
fn is_workspace_wide(file: &str) -> bool {
    WORKSPACE_WIDE.contains(&file) || file.starts_with(".cargo/")
}

/// [`owners`] plus the compile-time includes (ADR-1379): a changed file that a member compiles
/// in with `include_str!` / `include_bytes!` from outside its own directory (`templates/`,
/// `docs/`, another crate) also selects that member. `includes` is [`external_includes`]' output.
///
/// Returns `None` when any file is workspace-wide. `--no-dependents` does not change that; it
/// goes through [`scope_choice`], which sets those files aside instead of widening.
pub fn owners_with(
    files: &[String],
    members: &[Member],
    includes: &[(String, String)],
) -> Option<BTreeSet<String>> {
    let mut hit = BTreeSet::new();
    for file in files {
        if is_workspace_wide(file) {
            return None;
        }
        if let Some(owner) = owner_of(file, members) {
            hit.insert(owner.name.clone());
        }
        for (path, member) in includes {
            if path == file {
                hit.insert(member.clone());
            }
        }
    }
    Some(hit)
}

/// The string literals of the `include_str!` / `include_bytes!` calls in Rust source `text`.
/// Literals with escapes and computed paths (`concat!(env!(...))`) are skipped.
pub fn include_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for mac in ["include_str!", "include_bytes!"] {
        let mut rest = text;
        while let Some(at) = rest.find(mac) {
            rest = &rest[at + mac.len()..];
            let Some(args) = rest.trim_start().strip_prefix('(') else {
                continue;
            };
            let Some(literal) = args.trim_start().strip_prefix('"') else {
                continue;
            };
            if let Some(end) = literal.find('"') {
                let path = &literal[..end];
                if !path.is_empty() && !path.contains('\\') {
                    out.push(path.to_string());
                }
            }
        }
    }
    out
}

/// `base_dir` (workspace-relative, `/`-separated) joined with the relative path `rel`, with `.`
/// and `..` resolved. `None` when the result leaves the workspace or `rel` is absolute.
pub fn join_relative(base_dir: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') || rel.contains(':') {
        return None;
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Every `.rs` file under `dir`, skipping `target` and hidden directories and the directories in
/// `skip` (nested members, which own their own files).
pub fn rust_files(dir: &Path, skip: &[PathBuf], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            if name == "target" || name.starts_with('.') || skip.contains(&path) {
                continue;
            }
            rust_files(&path, skip, out);
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// The workspace files that a member compiles in through a literal `include_str!` /
/// `include_bytes!` path but does not own (outside its directory, or in another member's), as
/// `(workspace-relative path, including member)`, sorted and without duplicates.
pub fn external_includes(root: &Path, members: &[Member]) -> Vec<(String, String)> {
    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    for member in members {
        let dir = root.join(&member.dir);
        let nested: Vec<PathBuf> = members
            .iter()
            .filter(|m| m.dir.starts_with(&format!("{}/", member.dir)))
            .map(|m| root.join(&m.dir))
            .collect();
        let mut files = Vec::new();
        rust_files(&dir, &nested, &mut files);
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Some(file_dir) = file
                .parent()
                .and_then(|d| d.strip_prefix(root).ok())
                .map(|d| d.to_string_lossy().replace('\\', "/"))
            else {
                continue;
            };
            for literal in include_literals(&text) {
                let Some(path) = join_relative(&file_dir, &literal) else {
                    continue;
                };
                if owner_of(&path, members).is_none_or(|owner| owner.name != member.name) {
                    found.insert((path, member.name.clone()));
                }
            }
        }
    }
    found.into_iter().collect()
}

/// `changed` plus every member that (transitively) depends on one of them.
pub fn with_dependents(changed: &BTreeSet<String>, members: &[Member]) -> BTreeSet<String> {
    let mut all = changed.clone();
    loop {
        let before = all.len();
        for m in members {
            if m.deps.iter().any(|d| all.contains(d)) {
                all.insert(m.name.clone());
            }
        }
        if all.len() == before {
            return all;
        }
    }
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetaPackage>,
    workspace_root: String,
}

#[derive(Deserialize)]
struct MetaPackage {
    name: String,
    manifest_path: String,
    dependencies: Vec<MetaDep>,
}

#[derive(Deserialize)]
struct MetaDep {
    name: String,
    path: Option<String>,
}

/// The workspace members and their workspace-internal dependencies.
pub fn members(root: &Path) -> Result<Vec<Member>> {
    let out = Command::new(cargo_bin())
        .current_dir(root)
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .output()
        .context("running cargo metadata")?;
    if !out.status.success() {
        bail!("cargo metadata failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    let meta: Metadata = serde_json::from_slice(&out.stdout).context("parsing cargo metadata")?;
    let ws = PathBuf::from(&meta.workspace_root);
    let names: BTreeSet<&str> = meta.packages.iter().map(|p| p.name.as_str()).collect();
    Ok(meta
        .packages
        .iter()
        .map(|p| {
            let dir = Path::new(&p.manifest_path)
                .parent()
                .and_then(|d| d.strip_prefix(&ws).ok())
                .map(|d| d.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            Member {
                name: p.name.clone(),
                dir,
                deps: p
                    .dependencies
                    .iter()
                    .filter(|d| d.path.is_some() && names.contains(d.name.as_str()))
                    .map(|d| d.name.clone())
                    .collect(),
            }
        })
        .collect())
}

fn cargo_bin() -> std::ffi::OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// Files changed against the merge base with `base`: committed, staged, unstaged and untracked.
fn changed_files(root: &Path, base: &str) -> Result<Vec<String>> {
    let git = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!(
                "git {} failed:\n{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let merge_base = [base.to_string(), format!("origin/{base}")]
        .into_iter()
        .find_map(|candidate| git(&["merge-base", "HEAD", &candidate]).ok())
        .map(|s| s.trim().to_string())
        .with_context(|| format!("no merge base between HEAD and `{base}` (is the ref fetched?)"))?;
    let mut files: BTreeSet<String> = BTreeSet::new();
    // `--no-renames`: a rename lists both paths, so the crate a file moved out of is still built
    // (with rename detection, git prints only the new path).
    for listing in [
        git(&["diff", "--name-only", "--no-renames", &merge_base])?,
        // Index-only changes (staged, then reverted in the worktree) are not in the line above.
        git(&["diff", "--name-only", "--no-renames", "--cached", &merge_base])?,
        git(&["ls-files", "--others", "--exclude-standard"])?,
    ] {
        files.extend(listing.lines().filter(|l| !l.is_empty()).map(str::to_string));
    }
    Ok(files.into_iter().collect())
}

/// How [`changed_scope`] maps a change set to a [`Scope`], with no git or cargo run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeChoice {
    /// The cargo scope.
    pub scope: Scope,
    /// Workspace-wide paths set aside because `--no-dependents` is set, sorted.
    /// Empty when the scope was not narrowed.
    pub set_aside: Vec<String>,
}

/// The scope [`changed_scope`] selects for `files`.
///
/// `no_dependents` is `--no-dependents`. Without it, any workspace-wide file (the root manifest
/// and lockfile, rustfmt and clippy config, `deny.toml`, the toolchain file, or a path under
/// `.cargo/`) selects [`Scope::Workspace`]. With it, those files are listed in
/// [`ScopeChoice::set_aside`] and the scope is the members that own a remaining file (a member
/// manifest or source, and an `include_str!` / `include_bytes!` hit), with no dependents. A
/// lockfile-only change is an empty package list: `Cargo.lock` is not parsed for member names.
/// The members are the ones whose own files changed (ADR-8441).
///
/// `rustfmt.toml` does not widen under `--no-dependents`: the fmt step is already
/// `cargo fmt --all --check`, and clippy does not read it. `.cargo/` and `clippy.toml` do not
/// widen either: every scoped cargo invocation reads them, so the crates this change set owns
/// are checked under the new flags and lints. The test build of every other crate, and a full
/// workspace clippy, stay with the integration line.
pub fn scope_choice(
    files: &[String],
    members: &[Member],
    includes: &[(String, String)],
    no_dependents: bool,
) -> ScopeChoice {
    if !no_dependents {
        return match owners_with(files, members, includes) {
            None => ScopeChoice {
                scope: Scope::Workspace,
                set_aside: Vec::new(),
            },
            Some(changed) => ScopeChoice {
                scope: Scope::Packages(with_dependents(&changed, members).into_iter().collect()),
                set_aside: Vec::new(),
            },
        };
    }
    let mut set_aside: Vec<String> = files.iter().filter(|f| is_workspace_wide(f)).cloned().collect();
    set_aside.sort();
    set_aside.dedup();
    let crate_files: Vec<String> = files.iter().filter(|f| !is_workspace_wide(f)).cloned().collect();
    let changed = owners_with(&crate_files, members, includes).unwrap_or_default();
    ScopeChoice {
        scope: Scope::Packages(changed.into_iter().collect()),
        set_aside,
    }
}

/// The one line printed when `--no-dependents` sets workspace-wide files aside.
///
/// `None` when nothing was set aside. The line names those files and says the full workspace
/// test build is owed to the integration line. An empty package list adds that no member
/// `Cargo.toml` changed, so this run builds nothing.
pub fn narrowed_scope_line(set_aside: &[String], scope: &Scope) -> Option<String> {
    if set_aside.is_empty() {
        return None;
    }
    let names = set_aside.join(", ");
    let tail = match scope {
        Scope::Packages(packages) if packages.is_empty() => {
            "no member Cargo.toml changed, so --no-dependents builds nothing. "
        }
        _ => "--no-dependents keeps the scope on the changed crates. ",
    };
    let owed = "The full workspace test build is owed to the integration line.";
    Some(format!(
        "preflight: workspace-wide files changed ({names}); {tail}{owed}"
    ))
}

/// The scope `--changed` selects. A changed file outside every crate that a crate compiles in
/// (`include_str!` / `include_bytes!`) selects that crate, and the hit is printed. When
/// `--no-dependents` sets workspace-wide files aside, one line names them (ADR-8441).
pub fn changed_scope(root: &Path, args: &Args) -> Result<Scope> {
    let files = changed_files(root, &args.base)?;
    let members = members(root)?;
    let includes = external_includes(root, &members);
    for (path, member) in &includes {
        if files.contains(path) {
            println!("preflight: {path} changed; {member} compiles it in (include_str!/include_bytes!)");
        }
    }
    let choice = scope_choice(&files, &members, &includes, args.no_dependents);
    if let Some(line) = narrowed_scope_line(&choice.set_aside, &choice.scope) {
        println!("{line}");
    }
    Ok(choice.scope)
}

/// The lane name for the wrapper hint: the `lane/<x>` branch, else the worktree directory name.
fn lane_name(root: &Path) -> String {
    let branch = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    branch
        .strip_prefix("lane/")
        .map(str::to_string)
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "lane".into())
}

/// The exact command a the workstation lane runs this through, or `None` where the wrapper does not exist
/// (the build server, CI, a laptop: run `cargo xtask preflight` directly).
pub fn wrapper_command(root: &Path, args: &Args) -> Option<String> {
    wrapper_command_with(Path::new(LOCALBUILD), root, args)
}

fn wrapper_command_with(script: &Path, root: &Path, args: &Args) -> Option<String> {
    if !script.is_file() {
        return None;
    }
    let mut flags = String::new();
    if args.changed {
        flags.push_str(" --changed");
        if args.no_dependents {
            flags.push_str(" --no-dependents");
        }
        if args.base != "main" {
            flags.push_str(&format!(" --base {}", args.base));
        }
    }
    if args.no_build {
        flags.push_str(" --no-build");
    }
    if args.no_features {
        flags.push_str(" --no-features");
    }
    if args.keep_going {
        flags.push_str(" --keep-going");
    }
    if let Some(path) = &args.timings {
        flags.push_str(&format!(" --timings {}", path.display()));
    }
    Some(format!(
        "cd {} && {} {} -- cargo xtask preflight{flags}",
        root.display(),
        script.display(),
        lane_name(root)
    ))
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn secs(s: f64) -> String {
    format!("{s:.1}s")
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The step exited 0.
    Passed,
    /// The step failed, or could not be started.
    Failed,
    /// An earlier step failed and `--keep-going` was not given.
    NotRun,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Passed => "ok",
            Outcome::Failed => "FAILED",
            Outcome::NotRun => "not run",
        }
    }
}

/// One step's time and outcome, as `--timings` writes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepTiming {
    /// The step's short name.
    pub name: String,
    /// The command line it ran.
    pub command: String,
    /// Wall time in seconds; 0 when it did not run.
    pub secs: f64,
    /// How it ended.
    pub outcome: Outcome,
}

/// The whole run, as `--timings` writes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Timings {
    /// `workspace`, `none` or the comma-separated crate list.
    pub scope: String,
    /// Whether `--keep-going` was given.
    pub keep_going: bool,
    /// Wall time of the whole run in seconds.
    pub total_secs: f64,
    /// Every planned step, in order.
    pub steps: Vec<StepTiming>,
}

/// The scope as one word or list, for the timings record.
pub fn scope_label(scope: &Scope) -> String {
    match scope {
        Scope::Workspace => "workspace".into(),
        Scope::Packages(p) if p.is_empty() => "none".into(),
        Scope::Packages(p) => p.join(","),
    }
}

/// The per-step table printed at the end of a run: number, name, time, outcome, then the total.
pub fn summary_table(timings: &Timings) -> String {
    let n = timings.steps.len();
    let mut out = String::from("preflight: step times\n");
    for (i, step) in timings.steps.iter().enumerate() {
        let time = if step.outcome == Outcome::NotRun {
            "-".to_string()
        } else {
            secs(step.secs)
        };
        out.push_str(&format!(
            "  {:>2}/{n} {:<16} {time:>8}  {}\n",
            i + 1,
            step.name,
            step.outcome.label()
        ));
    }
    out.push_str(&format!("  total {}", secs(timings.total_secs)));
    out
}

/// Each step's time from an earlier run's `--timings` file (name → seconds; steps that did not run
/// are left out). An unreadable or foreign file gives an empty map: the times are a hint only.
pub fn previous_secs(path: &Path) -> std::collections::BTreeMap<String, f64> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Default::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Default::default();
    };
    value["steps"]
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .filter(|s| s["outcome"].as_str() != Some("not_run"))
                .filter_map(|s| Some((s["name"].as_str()?.to_string(), s["secs"].as_f64()?)))
                .collect()
        })
        .unwrap_or_default()
}

/// The `--list` table: number, name, command, and the step's last time when `previous` has it,
/// then the sum of the known times.
pub fn list_table(steps: &[Step], previous: &std::collections::BTreeMap<String, f64>) -> String {
    let mut out = String::new();
    let mut known = 0.0;
    for (i, step) in steps.iter().enumerate() {
        let last = match previous.get(&step.name) {
            Some(last_secs) => {
                known += last_secs;
                format!(" (last {})", secs(*last_secs))
            }
            None => String::new(),
        };
        out.push_str(&format!(
            "  {}/{} {:<16} {}{last}\n",
            i + 1,
            steps.len(),
            step.name,
            step.display()
        ));
    }
    if !previous.is_empty() {
        out.push_str(&format!("  last run of these steps: {}\n", secs(known)));
    }
    out
}

/// Writes `timings` as pretty JSON to `path` through a temporary file in the same directory, so a
/// reader never sees half a file.
pub fn write_timings(path: &Path, timings: &Timings) -> Result<()> {
    let text = serde_json::to_string_pretty(timings).context("encoding the step timings")?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
}

/// Runs `git grep` for conflict markers in `root`. `Ok((true, _))` when there are none.
fn conflict_markers(root: &Path) -> Result<(bool, String)> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["grep", "--untracked", "-l", "-E", "-e", CONFLICT_MARKER, "--"])
        .args(CONFLICT_GLOBS)
        .stdin(Stdio::null())
        .output()
        .context("running git grep")?;
    // git grep exits 0 on a match, 1 on none, and above 1 on an error.
    match out.status.code() {
        Some(1) => Ok((true, String::new())),
        Some(0) => Ok((
            false,
            format!(
                "conflict markers (the gate refuses the commit) in:\n{}",
                String::from_utf8_lossy(&out.stdout)
            ),
        )),
        _ => bail!("git grep failed:\n{}", String::from_utf8_lossy(&out.stderr)),
    }
}

fn run_step(root: &Path, step: &Step) -> Result<(bool, String)> {
    if step.name == "perf budget counts" {
        return run_perf_budget_counts(root, &mut |command, log| {
            crate::perf_budgets::run_cargo(root, command, log)
        });
    }
    let mut cmd = match &step.kind {
        StepKind::Cargo { args, env } => {
            let mut c = Command::new(cargo_bin());
            c.args(args).envs(env.iter().cloned());
            c
        }
        StepKind::Xtask(args) => {
            let mut c = Command::new(std::env::current_exe().context("locating the xtask binary")?);
            c.args(args);
            c
        }
        StepKind::ConflictMarkers => return conflict_markers(root),
    };
    let out = cmd
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", step.display()))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text))
}

/// Runs the allocation gate through the harness in this process.
fn run_perf_budget_counts(
    root: &Path,
    runner: &mut dyn FnMut(&str, &Path) -> Result<crate::perf_budgets::CommandOutcome>,
) -> Result<(bool, String)> {
    let args = crate::perf_budgets::Args {
        filter: None,
        enforce: true,
        baseline: None,
        update_baseline: false,
        counts_only: true,
        json: false,
        list: false,
        tolerance: 1.5,
        count_tolerance: 1.0,
        host: None,
        registry: None,
        out: None,
    };
    let code = crate::perf_budgets::run_with(&args, root, runner)?;
    Ok((code == ExitCode::SUCCESS, String::new()))
}

/// Runs `steps` in order through `runner` (which returns success and the step's output), printing
/// progress. Without `keep_going` it stops at the first failure and leaves the rest `NotRun`.
/// Returns one record per planned step and the indices of the failed ones.
fn execute(
    steps: &[Step],
    keep_going: bool,
    mut runner: impl FnMut(&Step) -> (bool, String),
) -> (Vec<StepTiming>, Vec<usize>) {
    let mut records: Vec<StepTiming> = steps
        .iter()
        .map(|s| StepTiming {
            name: s.name.clone(),
            command: s.display(),
            secs: 0.0,
            outcome: Outcome::NotRun,
        })
        .collect();
    let mut failed: Vec<usize> = Vec::new();
    for (i, (step, record)) in steps.iter().zip(records.iter_mut()).enumerate() {
        if !failed.is_empty() && !keep_going {
            break;
        }
        println!("[{}/{}] {} ...", i + 1, steps.len(), step.name);
        let t = Instant::now();
        let (ok, output) = runner(step);
        record.secs = t.elapsed().as_secs_f64();
        if ok {
            record.outcome = Outcome::Passed;
            println!("      ok ({})", secs(record.secs));
        } else {
            record.outcome = Outcome::Failed;
            eprintln!("{}", tail(&output, TAIL_LINES));
            eprintln!("      FAILED ({})", secs(record.secs));
            failed.push(i);
        }
    }
    (records, failed)
}

/// Whether `free_gb` (or a failed probe, `None`) may start a workspace `test build`.
///
/// `allow_override` is [`WORKSPACE_TEST_BUILD_OVERRIDE_ENV`] set to `1`. A number at or above
/// [`WORKSPACE_TEST_BUILD_MIN_FREE_GB`] is allowed. Under that, the returned sentence is the step's
/// failure text. `filesystem` is the mount that holds the cargo target directory.
pub fn workspace_test_build_refusal(
    free_gb: Option<u64>,
    filesystem: &str,
    allow_override: bool,
) -> Option<String> {
    if allow_override {
        return None;
    }
    match free_gb {
        Some(free) if free >= WORKSPACE_TEST_BUILD_MIN_FREE_GB => None,
        Some(free) => Some(format!(
            "{free} GB free on {filesystem}; a workspace test build needs ~400 GB"
        )),
        None => Some(format!(
            "could not read free space on {filesystem}; a workspace test build needs ~400 GB"
        )),
    }
}

/// The `test build` step links every debug test binary only when `scope` is the whole workspace.
fn step_needs_workspace_disk_guard(scope: &Scope, step: &Step) -> bool {
    step.name == "test build" && matches!(scope, Scope::Workspace)
}

/// `CARGO_TARGET_DIR` when it is set, otherwise `<root>/target`. A relative value is under `root`,
/// which is where the cargo steps run.
fn cargo_target_dir(root: &Path) -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) if !dir.is_empty() => {
            let path = PathBuf::from(dir);
            if path.is_absolute() { path } else { root.join(path) }
        }
        _ => root.join("target"),
    }
}

/// The mount point that holds `path`, or `path` itself when the mount table cannot be read.
fn filesystem_of(path: &Path) -> String {
    let existing = path
        .ancestors()
        .find(|p| !p.as_os_str().is_empty() && p.exists())
        .unwrap_or(Path::new("."));
    #[cfg(target_os = "linux")]
    if let Some(mount) = linux_mount_of(existing) {
        return mount;
    }
    existing.display().to_string()
}

#[cfg(target_os = "linux")]
fn linux_mount_of(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    mount_point_from_mountinfo(&text, &canon)
}

/// Longest mount point in a `/proc/self/mountinfo` body that contains `path`.
///
/// Field 5 (before ` - `) is the mount point. Octal escapes (`\040` for a space) are decoded.
pub fn mount_point_from_mountinfo(mountinfo: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        let Some(mount) = mountinfo_mount_point(line) else {
            continue;
        };
        if !path_is_under(path, &mount) {
            continue;
        }
        if best.as_ref().is_none_or(|(len, _)| mount.len() > *len) {
            best = Some((mount.len(), mount));
        }
    }
    best.map(|(_, mount)| mount)
}

fn mountinfo_mount_point(line: &str) -> Option<String> {
    let before = line.split(" - ").next()?;
    let fields: Vec<&str> = before.split_whitespace().collect();
    fields.get(4).map(|raw| unescape_mountinfo(raw))
}

fn unescape_mountinfo(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && let Some(value) = std::str::from_utf8(&bytes[i + 1..i + 4])
                .ok()
                .and_then(|octal| u8::from_str_radix(octal, 8).ok())
        {
            out.push(char::from(value));
            i += 4;
            continue;
        }
        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
}

fn path_is_under(path: &Path, mount: &str) -> bool {
    let mount = Path::new(mount);
    mount == Path::new("/") || path == mount || path.starts_with(mount)
}

/// The failure text when a workspace `test build` must not start, or `None` when it may.
fn workspace_test_build_block(root: &Path, scope: &Scope, step: &Step) -> Option<String> {
    if !step_needs_workspace_disk_guard(scope, step) {
        return None;
    }
    let target = cargo_target_dir(root);
    let filesystem = filesystem_of(&target);
    let free_gb = regia_platform::paths::free_space(&target)
        .ok()
        .map(|bytes| bytes / GIB);
    let allow = std::env::var(WORKSPACE_TEST_BUILD_OVERRIDE_ENV).ok().as_deref() == Some("1");
    workspace_test_build_refusal(free_gb, &filesystem, allow).map(|reason| {
        format!("{reason}\nnot started (set {WORKSPACE_TEST_BUILD_OVERRIDE_ENV}=1 to run it on a gate tree)")
    })
}

/// Runs the whole preflight; the process exit code is the result.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let scope = if args.changed {
        changed_scope(root, args)?
    } else {
        Scope::Workspace
    };
    let mut steps = plan(&scope, !args.no_build, !args.no_features, &args.base);
    if crate::net_soak::gate_row_enabled() {
        steps.push(net_soak_step());
    }
    if let Some(command) = wrapper_command(root, args) {
        println!("preflight: on the workstation run this through the build wrapper:\n  {command}");
    }
    match &scope {
        Scope::Workspace => println!("preflight: scope = whole workspace"),
        Scope::Packages(p) if p.is_empty() => {
            println!("preflight: scope = no crate changed (clippy, rustdoc and the test build are skipped)");
        }
        Scope::Packages(p) => println!("preflight: scope = {} crate(s): {}", p.len(), p.join(", ")),
    }
    if args.list {
        let previous = args.timings.as_deref().map(previous_secs).unwrap_or_default();
        print!("{}", list_table(&steps, &previous));
        return Ok(ExitCode::SUCCESS);
    }
    let started = Instant::now();
    let (records, failed) = execute(&steps, args.keep_going, |step| {
        // A workspace `cargo test --no-run` links every debug test binary (~400 GB). Refuse it
        // before cargo starts when the target filesystem is under the floor (ADR-8441).
        if let Some(reason) = workspace_test_build_block(root, &scope, step) {
            return (false, reason);
        }
        // A step that cannot start (cargo or git missing) counts as a failed step, so the
        // summary and the timings record still come out.
        run_step(root, step).unwrap_or_else(|err| (false, format!("{err:#}")))
    });
    let timings = Timings {
        scope: scope_label(&scope),
        keep_going: args.keep_going,
        total_secs: started.elapsed().as_secs_f64(),
        steps: records,
    };
    println!("{}", summary_table(&timings));
    if let Some(path) = &args.timings {
        write_timings(path, &timings)?;
        println!("preflight: step timings written to {}", path.display());
    }
    let total = secs(timings.total_secs);
    let Some(&first) = failed.first() else {
        println!("preflight: all {} steps passed in {total}", steps.len());
        return Ok(ExitCode::SUCCESS);
    };
    if args.keep_going {
        let names: Vec<&str> = failed.iter().map(|&i| steps[i].name.as_str()).collect();
        eprintln!(
            "\npreflight: {} of {} steps FAILED after {total}: {}",
            failed.len(),
            steps.len(),
            names.join(", ")
        );
    } else {
        eprintln!(
            "\npreflight: FAILED at step {}/{} `{}` after {total} (stopped; later steps not run)",
            first + 1,
            steps.len(),
            steps[first].name
        );
    }
    for &i in &failed {
        eprintln!("  {:<16} command: {}", steps[i].name, steps[i].display());
        if let Some(fix) = steps[i].fix {
            eprintln!("                   fix:     {fix}");
        }
    }
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn member(name: &str, dir: &str, deps: &[&str]) -> Member {
        Member {
            name: name.into(),
            dir: dir.into(),
            deps: deps.iter().map(|d| (*d).into()).collect(),
        }
    }

    fn sample() -> Vec<Member> {
        vec![
            member("regia_core", "crates/regia_core", &[]),
            member("regia_render", "crates/regia_render", &["regia_core"]),
            member("regia_test_utils", "crates/regia_test_utils", &["regia_render"]),
            member("hello", "samples/hello", &["regia_core"]),
            member("hello_android", "samples/hello/android", &["hello"]),
            member("xtask", "xtask", &["regia_test_utils"]),
        ]
    }

    fn files(f: &[&str]) -> Vec<String> {
        f.iter().map(|s| (*s).into()).collect()
    }

    /// A fresh directory per call: pid plus a counter, so parallel tests and runs never share one.
    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-preflight-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// git in `dir`, isolated from the host's identity, signing and hooks settings.
    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A repository with one commit of `files` on `main`.
    fn repo_with(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = temp_dir(tag);
        git_in(&dir, &["init", "-q", "-b", "main"]);
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, text).expect("write");
        }
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-q", "-m", "base"]);
        dir
    }

    fn args() -> Args {
        Args {
            changed: false,
            no_dependents: false,
            base: "main".into(),
            no_build: false,
            no_features: false,
            list: false,
            keep_going: false,
            timings: None,
        }
    }

    #[test]
    fn perf_budget_count_step_enforces_fixture_baseline() {
        let root = temp_dir("perf_budget_counts");
        std::fs::create_dir_all(root.join("docs/perf")).expect("perf dir");
        std::fs::write(
            root.join("docs/perf/budgets.ron"),
            r#"(format_version: 1, entries: [(name: "fixture_allocs", spec: "ADR-2704", package: "demo", command: "cargo test -p demo --release --test perf -- --ignored --nocapture perf_count", metric: "^perf-count fixture: .*$", stat: AllocsPerOp, budget: Unbudgeted)])"#,
        )
        .expect("registry");
        std::fs::write(
            root.join("docs/perf/budget-counts.json"),
            r#"{"version":1,"recorded_on":"fixture","entries":{"fixture_allocs":5.0}}"#,
        )
        .expect("baseline");

        for (allocs, expected) in [(10, true), (12, false)] {
            let mut runner = |_: &str, _: &Path| {
                Ok(crate::perf_budgets::CommandOutcome {
                    output: format!("perf-count fixture: ops=2 allocs={allocs} bytes=64\n"),
                    code: Some(0),
                })
            };
            let (passed, _) = run_perf_budget_counts(&root, &mut runner).expect("counts step");
            assert_eq!(passed, expected, "allocs={allocs}");
        }
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn the_plan_runs_in_gate_order() {
        let steps = plan(&Scope::Workspace, true, true, "main");
        let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "conflict markers",
                "fmt",
                "book fences",
                "clippy",
                "rustdoc",
                "check-versions",
                "duplicate-deps",
                "notices",
                "demo-notices",
                "spdx",
                "check-formats",
                "book-check",
                "llms-txt",
                "onboarding-lint",
                "check-shaders",
                "check-unwraps",
                "det-scan",
                "check-layers",
                "check-server-build",
                "check-net-core",
                "check-packaging",
                "check-kinds",
                "api-snapshot",
                "agent-examples",
                "doc-examples",
                "agent-snapshot",
                "features",
                "test build",
                "perf budget counts"
            ]
        );
        assert_eq!(steps[1].display(), "cargo fmt --all --check");
        assert_eq!(steps[1].fix, Some("cargo fmt --all"));
        assert_eq!(
            steps[3].display(),
            "cargo clippy --workspace --all-targets -- -D warnings"
        );
        assert!(
            steps[4]
                .display()
                .starts_with("RUSTDOCFLAGS=\"-D warnings\" cargo doc --workspace")
        );
        assert_eq!(steps[5].display(), "cargo xtask check-versions");
        assert_eq!(steps[6].display(), "cargo xtask duplicate-deps");
        assert_eq!(
            steps[6].fix,
            Some("unify the versions, or `cargo xtask duplicate-deps --update` and write the reason")
        );
        assert_eq!(steps[7].display(), "cargo xtask notices --check");
        assert_eq!(steps[7].fix, Some("cargo xtask notices"));
        assert_eq!(steps[8].display(), "cargo xtask demo-notices --check");
        assert_eq!(steps[8].fix, Some("cargo xtask demo-notices"));
        assert_eq!(steps[9].display(), "cargo xtask spdx --check");
        assert_eq!(steps[9].fix, Some("cargo xtask spdx --write"));
        assert_eq!(steps[10].display(), "cargo xtask check-formats");
        assert_eq!(steps[11].display(), "cargo xtask book-check");
        assert_eq!(steps[12].display(), "cargo xtask llms-txt --check");
        assert_eq!(steps[12].fix, Some("cargo xtask llms-txt"));
        assert_eq!(steps[13].display(), "cargo xtask onboarding-lint");
        assert_eq!(steps[15].display(), "cargo xtask check-unwraps");
        assert_eq!(steps[16].display(), "cargo xtask det-scan");
        assert_eq!(steps[19].display(), "cargo xtask check-net-core");
        assert_eq!(steps[21].display(), "cargo xtask check-kinds");
        assert_eq!(steps[22].display(), "cargo xtask api-snapshot");
        assert_eq!(steps[23].display(), "cargo xtask agent-examples");
        assert_eq!(steps[24].display(), "cargo xtask doc-examples");
        assert_eq!(steps[24].fix, Some("cargo xtask doc-examples --update"));
        assert_eq!(steps[25].display(), "cargo xtask agent-snapshot");
        assert_eq!(steps[25].fix, Some("cargo xtask agent-snapshot --update"));
        assert_eq!(steps[26].display(), "cargo xtask check-features");
        assert_eq!(steps[27].display(), "cargo test --workspace --no-run");
        assert_eq!(
            steps[28].display(),
            "cargo xtask perf-budgets --counts-only --enforce"
        );
    }

    #[test]
    fn the_agent_snapshot_step_forwards_the_base() {
        let steps = plan(&Scope::Workspace, true, true, "main");
        assert_eq!(steps[25].display(), "cargo xtask agent-snapshot");
        let steps = plan(&Scope::Workspace, true, true, "integ/v044");
        assert_eq!(
            steps[25].display(),
            "cargo xtask agent-snapshot --base integ/v044",
            "the ratchet diffs fragments against the same merge-base --changed uses"
        );
    }

    #[test]
    fn the_conflict_marker_step_uses_the_gate_pattern_and_file_kinds() {
        let steps = plan(&Scope::Workspace, true, true, "main");
        assert_eq!(steps[0].kind, StepKind::ConflictMarkers);
        assert_eq!(
            steps[0].display(),
            "git grep --untracked -l -E '^(<<<<<<<|>>>>>>>) ' -- '*.rs' '*.toml' '*.md' '*.wgsl'"
        );
    }

    #[test]
    fn no_build_drops_only_the_test_build() {
        let steps = plan(&Scope::Workspace, false, true, "main");
        assert_eq!(steps.len(), 28);
        assert!(steps.iter().all(|s| s.name != "test build"));
    }

    #[test]
    fn no_features_drops_only_the_feature_matrix() {
        let steps = plan(&Scope::Workspace, true, false, "main");
        assert_eq!(steps.len(), 28);
        assert!(steps.iter().all(|s| s.name != "features"));
        assert_eq!(steps.last().map(|s| s.name.as_str()), Some("perf budget counts"));
    }

    #[test]
    fn the_feature_matrix_follows_the_scope() {
        let steps = plan(
            &Scope::Packages(vec!["regia_media".into(), "regia".into()]),
            true,
            true,
            "main",
        );
        let features = steps
            .iter()
            .find(|s| s.name == "features")
            .expect("features step");
        assert_eq!(
            features.display(),
            "cargo xtask check-features --packages regia_media,regia"
        );
        let steps = plan(&Scope::Packages(Vec::new()), true, true, "main");
        assert!(
            steps.iter().all(|s| s.name != "features"),
            "nothing changed, nothing to compile"
        );
    }

    #[test]
    fn a_package_scope_selects_with_dash_p_and_an_empty_scope_skips_cargo_builds() {
        let steps = plan(&Scope::Packages(vec!["a".into(), "b".into()]), true, true, "main");
        assert_eq!(
            steps[3].display(),
            "cargo clippy -p a -p b --all-targets -- -D warnings"
        );
        assert_eq!(
            steps.get(steps.len() - 2).map(Step::display).as_deref(),
            Some("cargo test -p a -p b --no-run")
        );
        assert_eq!(steps.last().map(|s| s.name.as_str()), Some("perf budget counts"));
        let steps = plan(&Scope::Packages(Vec::new()), true, true, "main");
        let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"clippy") && !names.contains(&"rustdoc") && !names.contains(&"test build"));
        assert!(
            names.contains(&"fmt") && names.contains(&"check-layers") && names.contains(&"check-formats")
        );
        assert!(names.contains(&"conflict markers"));
    }

    #[test]
    fn files_map_to_the_innermost_member() {
        let m = sample();
        let hit = owners(
            &files(&[
                "crates/regia_render/src/lib.rs",
                "samples/hello/android/src/lib.rs",
                "docs/x.md",
            ]),
            &m,
        )
        .expect("not workspace-wide");
        assert_eq!(
            hit.into_iter().collect::<Vec<_>>(),
            ["hello_android", "regia_render"]
        );
        assert_eq!(owners(&files(&["docs/progress.md"]), &m), Some(BTreeSet::new()));
    }

    #[test]
    fn a_root_file_widens_to_the_workspace() {
        let m = sample();
        assert_eq!(owners(&files(&["Cargo.lock"]), &m), None);
        assert_eq!(owners(&files(&[".cargo/config.toml"]), &m), None);
        assert_eq!(
            owners(&files(&["crates/regia_core/Cargo.toml"]), &m).map(|s| s.len()),
            Some(1)
        );
    }

    fn crate_a_and_b() -> Vec<Member> {
        vec![member("a", "crates/a", &[]), member("b", "crates/b", &["a"])]
    }

    #[test]
    fn no_dependents_keeps_a_lockfile_change_on_the_owning_crate() {
        let members = crate_a_and_b();
        let changed = files(&["Cargo.lock", "crates/a/src/lib.rs", "crates/a/Cargo.toml"]);
        // The widen path is unchanged: `owners_with` still returns None for a workspace-wide file.
        assert_eq!(owners_with(&changed, &members, &[]), None);
        let choice = scope_choice(&changed, &members, &[], true);
        assert_eq!(choice.scope, Scope::Packages(vec!["a".into()]));
        assert_eq!(choice.set_aside, vec!["Cargo.lock".to_string()]);
        let line = narrowed_scope_line(&choice.set_aside, &choice.scope).expect("owed line");
        assert!(line.contains("Cargo.lock"), "{line}");
        assert!(line.contains("integration line"), "{line}");
        assert!(
            !line.contains("builds nothing"),
            "a changed member still builds: {line}"
        );
        assert_eq!(
            scope_choice(&changed, &members, &[], false).scope,
            Scope::Workspace,
            "without --no-dependents the whole workspace stays selected"
        );
    }

    #[test]
    fn no_dependents_lockfile_only_is_an_empty_scope_with_a_note() {
        let members = crate_a_and_b();
        let choice = scope_choice(&files(&["Cargo.lock"]), &members, &[], true);
        assert_eq!(choice.scope, Scope::Packages(Vec::new()));
        let line = narrowed_scope_line(&choice.set_aside, &choice.scope).expect("note");
        assert!(line.contains("Cargo.lock"), "{line}");
        assert!(line.contains("no member Cargo.toml"), "{line}");
        assert!(line.contains("builds nothing"), "{line}");
        assert!(line.contains("integration line"), "{line}");
    }

    #[test]
    fn no_dependents_lockfile_selects_members_whose_manifest_changed() {
        let members = crate_a_and_b();
        let choice = scope_choice(
            &files(&["Cargo.lock", "crates/b/Cargo.toml", "docs/readme.md"]),
            &members,
            &[],
            true,
        );
        assert_eq!(choice.scope, Scope::Packages(vec!["b".into()]));
        let only_docs = scope_choice(&files(&["Cargo.lock", "docs/readme.md"]), &members, &[], true);
        assert_eq!(only_docs.scope, Scope::Packages(Vec::new()));
    }

    #[test]
    fn no_dependents_rustfmt_and_cargo_config_do_not_widen_the_test_build() {
        // fmt is already `cargo fmt --all`. `.cargo/` and `clippy.toml` are read by the scoped
        // cargo steps, so they do not select the whole workspace under `--no-dependents`. The
        // workspace test build stays owed to the integration line (ADR-8441).
        let members = crate_a_and_b();
        let changed = files(&[
            "rustfmt.toml",
            ".cargo/config.toml",
            "clippy.toml",
            "crates/a/src/lib.rs",
        ]);
        let choice = scope_choice(&changed, &members, &[], true);
        assert_eq!(choice.scope, Scope::Packages(vec!["a".into()]));
        assert_eq!(
            choice.set_aside,
            vec![
                ".cargo/config.toml".to_string(),
                "clippy.toml".to_string(),
                "rustfmt.toml".to_string(),
            ]
        );
        let steps = plan(&choice.scope, true, true, "main");
        assert_eq!(
            steps
                .iter()
                .find(|s| s.name == "fmt")
                .map(Step::display)
                .as_deref(),
            Some("cargo fmt --all --check")
        );
        assert_eq!(
            steps
                .iter()
                .find(|s| s.name == "test build")
                .map(Step::display)
                .as_deref(),
            Some("cargo test -p a --no-run")
        );
        assert_eq!(
            steps
                .iter()
                .find(|s| s.name == "clippy")
                .map(Step::display)
                .as_deref(),
            Some("cargo clippy -p a --all-targets -- -D warnings")
        );
        assert_eq!(
            scope_choice(&changed, &members, &[], false).scope,
            Scope::Workspace
        );
        let line = narrowed_scope_line(&choice.set_aside, &choice.scope).expect("owed line");
        assert!(
            line.contains("rustfmt.toml") && line.contains(".cargo/config.toml"),
            "{line}"
        );
        assert!(line.contains("integration line"), "{line}");
    }

    #[test]
    fn scope_choice_adds_dependents_only_without_the_flag() {
        let members = crate_a_and_b();
        let changed = files(&["crates/a/src/lib.rs"]);
        assert_eq!(
            scope_choice(&changed, &members, &[], true).scope,
            Scope::Packages(vec!["a".into()])
        );
        assert_eq!(
            scope_choice(&changed, &members, &[], false).scope,
            Scope::Packages(vec!["a".into(), "b".into()])
        );
        assert!(scope_choice(&changed, &members, &[], true).set_aside.is_empty());
    }

    #[test]
    fn no_dependents_keeps_an_include_hit_beside_a_lockfile() {
        let members = crate_a_and_b();
        let includes = vec![("templates/t.txt".to_string(), "a".to_string())];
        let choice = scope_choice(
            &files(&["Cargo.lock", "templates/t.txt"]),
            &members,
            &includes,
            true,
        );
        assert_eq!(choice.scope, Scope::Packages(vec!["a".into()]));
    }

    #[test]
    fn workspace_test_build_is_refused_under_500_gb_unless_overridden() {
        let fs = "/srv/build-data";
        assert_eq!(
            workspace_test_build_refusal(Some(499), fs, false).as_deref(),
            Some("499 GB free on /srv/build-data; a workspace test build needs ~400 GB")
        );
        assert_eq!(
            workspace_test_build_refusal(Some(0), fs, false).as_deref(),
            Some("0 GB free on /srv/build-data; a workspace test build needs ~400 GB")
        );
        assert_eq!(workspace_test_build_refusal(Some(500), fs, false), None);
        assert_eq!(workspace_test_build_refusal(Some(1200), fs, false), None);
        assert_eq!(workspace_test_build_refusal(Some(12), fs, true), None);
        let unread = workspace_test_build_refusal(None, fs, false).expect("probe failure refuses");
        assert!(
            unread.contains("could not read free space on /srv/build-data"),
            "{unread}"
        );
        assert!(
            unread.contains("a workspace test build needs ~400 GB"),
            "{unread}"
        );
        assert_eq!(workspace_test_build_refusal(None, fs, true), None);
    }

    #[test]
    fn the_disk_guard_applies_only_to_a_workspace_test_build() {
        let workspace = plan(&Scope::Workspace, true, true, "main");
        let test_build = workspace
            .iter()
            .find(|s| s.name == "test build")
            .expect("test build");
        assert!(step_needs_workspace_disk_guard(&Scope::Workspace, test_build));
        let packages = plan(&Scope::Packages(vec!["a".into()]), true, true, "main");
        let scoped = packages
            .iter()
            .find(|s| s.name == "test build")
            .expect("scoped test build");
        assert!(!step_needs_workspace_disk_guard(
            &Scope::Packages(vec!["a".into()]),
            scoped
        ));
        let perf = workspace
            .iter()
            .find(|s| s.name == "perf budget counts")
            .expect("perf");
        assert!(!step_needs_workspace_disk_guard(&Scope::Workspace, perf));
        assert_eq!(test_build.display(), "cargo test --workspace --no-run");
        assert_eq!(scoped.display(), "cargo test -p a --no-run");
    }

    #[test]
    fn mountinfo_picks_the_longest_mount_and_unescapes_spaces() {
        let body = "\
36 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw
80 36 8:2 / /srv/build-data rw - ext4 /dev/nvme0n1 rw
81 36 8:3 / /srv/build-fast2 rw - xfs /dev/nvme1n1 rw
82 36 8:4 / /mnt/my\\040disk rw - ext4 /dev/sdb1 rw
";
        assert_eq!(
            mount_point_from_mountinfo(body, Path::new("/srv/build-data/ferrite-lanes/x/target")).as_deref(),
            Some("/srv/build-data")
        );
        assert_eq!(
            mount_point_from_mountinfo(body, Path::new("/srv/build-fast2/ferrite/y")).as_deref(),
            Some("/srv/build-fast2")
        );
        assert_eq!(
            mount_point_from_mountinfo(body, Path::new("/home/a")).as_deref(),
            Some("/")
        );
        assert_eq!(
            mount_point_from_mountinfo(body, Path::new("/mnt/my disk/target")).as_deref(),
            Some("/mnt/my disk")
        );
        assert_eq!(
            mount_point_from_mountinfo(body, Path::new("/srv/build-database")).as_deref(),
            Some("/"),
            "a longer name that only shares a prefix is not that mount"
        );
    }

    #[test]
    fn include_literals_finds_both_macros_and_skips_computed_paths() {
        let text = r#"
            const A: &str = include_str!("../../../templates/game/Cargo.toml.tmpl");
            const B: &[u8] = include_bytes!( "../shaders/s.wgsl" );
            const C: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/x.txt"));
            const D: &str = include_str!("a\\b.txt");
            // A mention of include_str! in prose has no literal after it.
        "#;
        assert_eq!(
            include_literals(text),
            ["../../../templates/game/Cargo.toml.tmpl", "../shaders/s.wgsl"]
        );
    }

    #[test]
    fn relative_paths_join_inside_the_workspace_only() {
        assert_eq!(
            join_relative("crates/a/src", "../../../templates/t.txt").as_deref(),
            Some("templates/t.txt")
        );
        assert_eq!(
            join_relative("crates/a/src/sub", "./../x.rs").as_deref(),
            Some("crates/a/src/x.rs")
        );
        assert_eq!(join_relative("crates/a", "../../../outside"), None);
        assert_eq!(join_relative("crates/a", "/etc/passwd"), None);
    }

    #[test]
    fn a_changed_file_a_crate_includes_selects_that_crate() {
        // `a` includes a template outside every crate and one of its own shaders; `b` includes a
        // file inside `a`; `a/nested` is its own member and owns its includes.
        let dir = temp_dir("includes");
        let write = |rel: &str, text: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, text).expect("write");
        };
        write(
            "crates/a/src/lib.rs",
            r#"const T: &str = include_str!("../../../templates/t.txt");
               const S: &str = include_str!("../shaders/s.wgsl");"#,
        );
        write("crates/a/shaders/s.wgsl", "");
        write("crates/a/data.bin", "");
        write(
            "crates/a/nested/src/lib.rs",
            r#"const D: &str = include_str!("../../../../docs/d.md");"#,
        );
        write(
            "crates/b/tests/t.rs",
            r#"const X: &[u8] = include_bytes!("../../a/data.bin");"#,
        );
        write(
            "crates/b/target/debug/build/out.rs",
            r#"include_str!("../../../../../templates/ignored.txt")"#,
        );
        let members = vec![
            member("a", "crates/a", &[]),
            member("a_nested", "crates/a/nested", &[]),
            member("b", "crates/b", &[]),
        ];
        let includes = external_includes(&dir, &members);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            includes,
            [
                ("crates/a/data.bin".to_string(), "b".to_string()),
                ("docs/d.md".to_string(), "a_nested".to_string()),
                ("templates/t.txt".to_string(), "a".to_string()),
            ],
            "the crate's own shader and anything under target/ are not listed"
        );
        let scope = |changed: &[&str]| {
            owners_with(&files(changed), &members, &includes).map(|s| s.into_iter().collect::<Vec<_>>())
        };
        assert_eq!(scope(&["templates/t.txt"]), Some(vec!["a".to_string()]));
        assert_eq!(
            scope(&["crates/a/data.bin"]),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            scope(&["docs/d.md", "docs/other.md"]),
            Some(vec!["a_nested".to_string()])
        );
        assert_eq!(scope(&["docs/other.md"]), Some(Vec::new()));
        // Without the include map (the old behaviour) a template change selected nothing.
        assert_eq!(
            owners(&files(&["templates/t.txt"]), &members),
            Some(BTreeSet::new())
        );
    }

    #[test]
    fn dependents_are_transitive() {
        let m = sample();
        let changed: BTreeSet<String> = ["regia_core".to_string()].into();
        let all = with_dependents(&changed, &m);
        assert_eq!(
            all.into_iter().collect::<Vec<_>>(),
            [
                "hello",
                "hello_android",
                "regia_core",
                "regia_render",
                "regia_test_utils",
                "xtask"
            ]
        );
        let leaf: BTreeSet<String> = ["hello_android".to_string()].into();
        assert_eq!(with_dependents(&leaf, &m), leaf);
    }

    #[test]
    fn the_wrapper_command_names_the_script_lane_and_flags() {
        let script = std::env::current_exe().expect("exe");
        let args = Args {
            changed: true,
            no_build: true,
            ..args()
        };
        let root = Path::new("/work/.claude/worktrees/lane-x");
        let command = wrapper_command_with(&script, root, &args).expect("script exists");
        assert!(
            command.starts_with("cd /work/.claude/worktrees/lane-x && "),
            "{command}"
        );
        assert!(
            command.ends_with("-- cargo xtask preflight --changed --no-build"),
            "{command}"
        );
        assert_eq!(
            wrapper_command_with(Path::new("/nonexistent/localbuild.sh"), root, &args),
            None
        );
    }

    #[test]
    fn keep_going_and_timings_reach_the_wrapper_command() {
        let script = std::env::current_exe().expect("exe");
        let args = Args {
            keep_going: true,
            timings: Some(PathBuf::from("/tmp/pf.json")),
            ..args()
        };
        let command = wrapper_command_with(&script, Path::new("/w/lane-y"), &args).expect("script exists");
        assert!(
            command.ends_with("-- cargo xtask preflight --keep-going --timings /tmp/pf.json"),
            "{command}"
        );
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail("a", 5), "a");
    }

    /// Three cheap steps; the runner fails the ones named in `fail`.
    fn three_steps() -> Vec<Step> {
        ["one", "two", "three"]
            .into_iter()
            .map(|n| Step::xtask(n, &[n]))
            .collect()
    }

    #[test]
    fn without_keep_going_the_run_stops_at_the_first_failure() {
        let steps = three_steps();
        let mut ran = Vec::new();
        let (records, failed) = execute(&steps, false, |step| {
            ran.push(step.name.clone());
            (step.name != "two", "boom".into())
        });
        assert_eq!(ran, ["one", "two"]);
        assert_eq!(failed, [1]);
        let outcomes: Vec<Outcome> = records.iter().map(|r| r.outcome).collect();
        assert_eq!(outcomes, [Outcome::Passed, Outcome::Failed, Outcome::NotRun]);
        assert_eq!(records[2].command, "cargo xtask three");
    }

    #[test]
    fn keep_going_runs_every_step_and_lists_every_failure() {
        let steps = three_steps();
        let mut ran = 0;
        let (records, failed) = execute(&steps, true, |step| {
            ran += 1;
            (step.name == "two", String::new())
        });
        assert_eq!(ran, 3);
        assert_eq!(failed, [0, 2]);
        let outcomes: Vec<Outcome> = records.iter().map(|r| r.outcome).collect();
        assert_eq!(outcomes, [Outcome::Failed, Outcome::Passed, Outcome::Failed]);
    }

    fn sample_timings() -> Timings {
        Timings {
            scope: scope_label(&Scope::Packages(vec!["a".into(), "b".into()])),
            keep_going: false,
            total_secs: 12.3,
            steps: vec![
                StepTiming {
                    name: "fmt".into(),
                    command: "cargo fmt --all --check".into(),
                    secs: 1.0,
                    outcome: Outcome::Passed,
                },
                StepTiming {
                    name: "clippy".into(),
                    command: "cargo clippy".into(),
                    secs: 11.3,
                    outcome: Outcome::Failed,
                },
                StepTiming {
                    name: "rustdoc".into(),
                    command: "cargo doc".into(),
                    secs: 0.0,
                    outcome: Outcome::NotRun,
                },
            ],
        }
    }

    #[test]
    fn the_summary_table_lists_every_step_with_time_and_outcome() {
        let table = summary_table(&sample_timings());
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 5, "{table}");
        assert!(lines[1].contains("1/3 fmt") && lines[1].contains("1.0s") && lines[1].ends_with("ok"));
        assert!(
            lines[2].contains("11.3s") && lines[2].ends_with("FAILED"),
            "{table}"
        );
        assert!(
            lines[3].contains(" - ") && lines[3].ends_with("not run"),
            "{table}"
        );
        assert_eq!(lines[4], "  total 12.3s");
    }

    #[test]
    fn scope_labels_name_the_workspace_nothing_or_the_crates() {
        assert_eq!(scope_label(&Scope::Workspace), "workspace");
        assert_eq!(scope_label(&Scope::Packages(Vec::new())), "none");
        assert_eq!(scope_label(&Scope::Packages(vec!["a".into(), "b".into()])), "a,b");
    }

    #[test]
    fn list_shows_the_last_times_from_an_earlier_timings_file() {
        let dir = temp_dir("list");
        let path = dir.join("pf.json");
        write_timings(&path, &sample_timings()).expect("write");
        let previous = previous_secs(&path);
        let missing = previous_secs(&dir.join("absent.json"));
        std::fs::write(dir.join("junk.json"), "not json").expect("write");
        let junk = previous_secs(&dir.join("junk.json"));
        let _ = std::fs::remove_dir_all(&dir);
        // `rustdoc` did not run in the sample, so it has no time.
        assert_eq!(previous.len(), 2);
        assert_eq!(previous.get("clippy"), Some(&11.3));
        assert!(missing.is_empty() && junk.is_empty());
        let steps = plan(&Scope::Packages(vec!["a".into()]), true, true, "main");
        let table = list_table(&steps, &previous);
        let fmt = table.lines().find(|l| l.contains(" fmt ")).expect("fmt line");
        assert!(fmt.ends_with("cargo fmt --all --check (last 1.0s)"), "{table}");
        let rustdoc = table
            .lines()
            .find(|l| l.contains(" rustdoc "))
            .expect("rustdoc line");
        assert!(!rustdoc.contains("last"), "{table}");
        assert!(table.ends_with("  last run of these steps: 12.3s\n"), "{table}");
        assert!(!list_table(&steps, &Default::default()).contains("last run"));
    }

    #[test]
    fn timings_are_written_as_json_with_no_temporary_file_left() {
        let dir = temp_dir("timings");
        let path = dir.join("nested").join("preflight.json");
        write_timings(&path, &sample_timings()).expect("first write");
        // A second run replaces the file.
        write_timings(&path, &sample_timings()).expect("second write");
        let text = std::fs::read_to_string(&path).expect("read back");
        let leftovers: Vec<_> = std::fs::read_dir(dir.join("nested"))
            .expect("list")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(leftovers, ["preflight.json"]);
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["scope"], "a,b");
        assert_eq!(value["keep_going"], false);
        assert_eq!(value["total_secs"], 12.3);
        assert_eq!(value["steps"][1]["name"], "clippy");
        assert_eq!(value["steps"][1]["outcome"], "failed");
        assert_eq!(value["steps"][2]["outcome"], "not_run");
    }

    #[test]
    fn changed_files_sees_index_only_changes() {
        let dir = repo_with("index", &[("a.rs", "one")]);
        git_in(&dir, &["checkout", "-q", "-b", "feat"]);
        // Staged, then reverted in the worktree: only the index differs from the merge base.
        std::fs::write(dir.join("a.rs"), "two").expect("write");
        git_in(&dir, &["add", "a.rs"]);
        std::fs::write(dir.join("a.rs"), "one").expect("write");
        let files = changed_files(&dir, "main");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(files.expect("changed files"), vec!["a.rs".to_string()]);
    }

    #[test]
    fn a_rename_lists_the_old_and_the_new_path() {
        // A file that moves from one crate to another changes both crates.
        let body = "pub fn moved() {}\n// enough identical lines for git to call it a rename\n";
        let dir = repo_with("rename", &[("crates/a/src/moved.rs", body)]);
        git_in(&dir, &["checkout", "-q", "-b", "feat"]);
        std::fs::create_dir_all(dir.join("crates/b/src")).expect("dir");
        git_in(&dir, &["mv", "crates/a/src/moved.rs", "crates/b/src/moved.rs"]);
        git_in(&dir, &["commit", "-q", "-m", "move"]);
        let files = changed_files(&dir, "main");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            files.expect("changed files"),
            ["crates/a/src/moved.rs", "crates/b/src/moved.rs"]
        );
    }

    #[test]
    fn conflict_markers_fail_only_on_a_marker_at_a_line_start() {
        let dir = repo_with(
            "markers",
            &[
                ("src/lib.rs", "// a `<<<<<<< ours` mention mid-line is fine\n"),
                (".gitignore", "ignored.md\n"),
            ],
        );
        let clean = conflict_markers(&dir).expect("git grep");
        // An ignored file is not searched (the gate archives the commit, which has no ignored files).
        std::fs::write(dir.join("ignored.md"), "<<<<<<< ours\n").expect("write");
        let ignored = conflict_markers(&dir).expect("git grep");
        // An untracked notes file with a marker line is found, like a tracked one.
        std::fs::write(dir.join("notes.md"), "text\n>>>>>>> theirs\n").expect("write");
        let untracked = conflict_markers(&dir).expect("git grep");
        // A file kind the gate does not search is not searched here either.
        std::fs::remove_file(dir.join("notes.md")).expect("remove");
        std::fs::write(dir.join("notes.txt"), "<<<<<<< ours\n").expect("write");
        let other_kind = conflict_markers(&dir).expect("git grep");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(clean.0, "{}", clean.1);
        assert!(ignored.0, "{}", ignored.1);
        assert!(!untracked.0);
        assert!(untracked.1.contains("notes.md"), "{}", untracked.1);
        assert!(other_kind.0, "{}", other_kind.1);
    }
}
