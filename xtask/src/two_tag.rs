// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask two-tag-upgrade --from <tag> --to <tag>`: the two-tag distribution upgrade check
//! (`docs/specs/94-distribution-upgrade.md` § 9, ADR-2610).
//!
//! The job builds the `regia` CLI of each tag in a git worktree under `target/two-tag/`, packages
//! each tag's engine tree with `regia engine package`, then for every template: `regia new` with the
//! old CLI against the old release, `regia upgrade-project --write` with the new CLI against the new
//! release, and `cargo check` of the upgraded project (its engine dependencies are the new release's
//! crates, so the check is "the project compiles against the new tag"). It needs no network beyond
//! cargo's registry cache and reports one row per template. Exit code 1 when any template fails.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;

/// Arguments of `cargo xtask two-tag-upgrade`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// The older tag: its `regia` creates the projects.
    #[arg(long)]
    pub from: String,
    /// The newer tag: its `regia` upgrades them and their engine release is what they are checked against.
    #[arg(long)]
    pub to: String,
    /// Template ids (comma separated) or `all`.
    #[arg(long, value_delimiter = ',', default_value = "all")]
    pub templates: Vec<String>,
    /// Work folder (default: `target/two-tag` of the workspace).
    #[arg(long)]
    pub work_dir: Option<PathBuf>,
    /// Keep the tag worktrees and the generated projects instead of deleting them.
    #[arg(long)]
    pub keep: bool,
}

/// One external command of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What the step does, for the log and the report (`new`, `upgrade`, `check`).
    pub stage: &'static str,
    /// The program to start.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
}

/// The paths one template's steps work with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The older tag's `regia` binary.
    pub old_regia: PathBuf,
    /// The newer tag's `regia` binary.
    pub new_regia: PathBuf,
    /// The older tag's packaged release directory.
    pub old_release: PathBuf,
    /// The newer tag's packaged release directory.
    pub new_release: PathBuf,
    /// The folder projects are generated into.
    pub projects: PathBuf,
    /// The cargo target directory of the `cargo check` steps.
    pub check_target: PathBuf,
}

/// The project name a template is generated under (its crate names derive from it).
#[must_use]
pub fn project_name(template: &str) -> String {
    format!("tt_{template}")
}

/// `path:<dir>` with `/` separators on every host (spec 94 § 9).
#[must_use]
pub fn path_source(dir: &Path) -> String {
    format!("path:{}", dir.to_string_lossy().replace('\\', "/"))
}

/// The folder name of a tag's worktree and target: a tag may contain `/`.
#[must_use]
pub fn tag_dir_name(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Refuses tags that are empty, equal, or that git could read as an option.
///
/// # Errors
///
/// An unusable tag pair.
pub fn validate_tags(from: &str, to: &str) -> Result<()> {
    for (flag, tag) in [("--from", from), ("--to", to)] {
        if tag.is_empty() {
            bail!("{flag} is empty");
        }
        if tag.starts_with('-')
            || tag.contains("..")
            || tag.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            bail!("{flag} `{tag}` is not a tag name");
        }
    }
    if from == to {
        bail!("--from and --to are the same tag (`{from}`): an upgrade needs two");
    }
    Ok(())
}

/// The templates to check: `requested` (`all`, or ids each validated), in the order given.
///
/// # Errors
///
/// An id that is not a template.
pub fn resolve_templates(requested: &[String]) -> Result<Vec<&'static str>> {
    if requested.is_empty() || requested.iter().any(|t| t == "all") {
        return Ok(regia_templates::TEMPLATE_IDS.to_vec());
    }
    let mut out: Vec<&'static str> = Vec::new();
    for id in requested {
        let known = regia_templates::TEMPLATE_IDS
            .iter()
            .copied()
            .find(|known| known == id)
            .with_context(|| {
                format!(
                    "unknown template `{id}` (available: all, {})",
                    regia_templates::TEMPLATE_IDS.join(", ")
                )
            })?;
        if !out.contains(&known) {
            out.push(known);
        }
    }
    Ok(out)
}

/// The steps of one template: create with the old CLI, upgrade with the new one, `cargo check`.
///
/// `project` is the directory `regia new` printed; the check step needs it, so the plan is built in
/// two parts: [`new_step`] first, then this with the project path.
#[must_use]
pub fn upgrade_and_check_steps(paths: &Paths, project: &Path) -> Vec<Step> {
    let manifest = project.join("Cargo.toml");
    vec![
        Step {
            stage: "upgrade",
            program: paths.new_regia.to_string_lossy().into_owned(),
            args: vec![
                "upgrade-project".into(),
                "--project".into(),
                project.to_string_lossy().into_owned(),
                "--write".into(),
                "--engine".into(),
                path_source(&paths.new_release),
            ],
            env: Vec::new(),
        },
        check_step(paths, &manifest),
    ]
}

/// A `cargo check` of `manifest` into the shared check target.
#[must_use]
pub fn check_step(paths: &Paths, manifest: &Path) -> Step {
    Step {
        stage: "check",
        program: "cargo".into(),
        args: vec![
            "check".into(),
            "--manifest-path".into(),
            manifest.to_string_lossy().into_owned(),
        ],
        env: vec![(
            "CARGO_TARGET_DIR".into(),
            paths.check_target.to_string_lossy().into_owned(),
        )],
    }
}

/// The step that generates `template` with the old CLI against the old release.
#[must_use]
pub fn new_step(paths: &Paths, template: &str) -> Step {
    Step {
        stage: "new",
        program: paths.old_regia.to_string_lossy().into_owned(),
        args: vec![
            "new".into(),
            project_name(template),
            "--template".into(),
            template.into(),
            "--path".into(),
            paths.projects.to_string_lossy().into_owned(),
            "--engine-source".into(),
            path_source(&paths.old_release),
        ],
        env: Vec::new(),
    }
}

/// How one template ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every stage passed.
    Ok,
    /// A stage failed: its name and the message.
    Failed {
        /// The stage (`new`, `upgrade`, `check`).
        stage: &'static str,
        /// What went wrong (last lines of the command's output).
        message: String,
    },
}

/// One report row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Template id.
    pub template: String,
    /// The result.
    pub outcome: Outcome,
    /// Whole seconds spent in `regia new`, `upgrade-project` and `cargo check`.
    pub seconds: [u64; 3],
}

/// The markdown report: one line per template, then a summary line.
#[must_use]
pub fn render_report(from: &str, to: &str, rows: &[Row]) -> String {
    let mut out = format!(
        "two-tag-upgrade {from} -> {to}\n\n| template | result | new s | upgrade s | check s |\n|---|---|---|---|---|\n"
    );
    for row in rows {
        let result = match &row.outcome {
            Outcome::Ok => "ok".to_owned(),
            Outcome::Failed { stage, .. } => format!("FAILED at {stage}"),
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            row.template, result, row.seconds[0], row.seconds[1], row.seconds[2]
        ));
    }
    let failed = rows.iter().filter(|r| r.outcome != Outcome::Ok).count();
    out.push_str(&format!(
        "\n{} of {} templates ok\n",
        rows.len() - failed,
        rows.len()
    ));
    for row in rows {
        if let Outcome::Failed { stage, message } = &row.outcome {
            out.push_str(&format!("\n{} ({stage}):\n{message}\n", row.template));
        }
    }
    out
}

/// The last `n` lines of `text`.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Runs a command to completion; returns its stdout. `stream` lets stderr through for long builds.
fn run(step: &Step, cwd: Option<&Path>, stream: bool) -> Result<String> {
    let mut command = Command::new(&step.program);
    command
        .args(&step.args)
        .envs(step.env.iter().map(|(k, v)| (k, v)));
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.stdout(Stdio::piped());
    command.stderr(if stream { Stdio::inherit() } else { Stdio::piped() });
    let output = command
        .output()
        .with_context(|| format!("cannot start `{}`", step.program))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`{} {}` exited with {}\n{}\n{}",
            step.program,
            step.args.join(" "),
            output.status,
            tail(&stdout, 10),
            tail(&stderr, 25)
        );
    }
    Ok(stdout)
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .context("cannot start git")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// One tag, built: its worktree, CLI binary and packaged release.
struct Built {
    regia: PathBuf,
    release: PathBuf,
}

/// Adds (or reuses) the tag's worktree, builds its `regia` and packages its engine tree.
fn build_tag(root: &Path, work: &Path, tag: &str) -> Result<Built> {
    let name = tag_dir_name(tag);
    let tree = work.join(&name);
    let commit = git(root, &["rev-parse", "--verify", &format!("{tag}^{{commit}}")])
        .with_context(|| format!("tag `{tag}` does not exist (git tag)"))?;
    let reuse =
        tree.join(".git").exists() && git(&tree, &["rev-parse", "HEAD"]).is_ok_and(|head| head == commit);
    if !reuse {
        if tree.exists() {
            bail!(
                "{} exists and is not a checkout of `{tag}`; delete it",
                tree.display()
            );
        }
        let tree_text = tree.to_string_lossy().into_owned();
        git(root, &["worktree", "add", "--detach", &tree_text, &commit])?;
    }
    println!(
        "two-tag-upgrade: building regia of {tag} ({})",
        &commit[..commit.len().min(10)]
    );
    let target = work.join(format!("target-{name}"));
    let started = Instant::now();
    run(
        &Step {
            stage: "build",
            program: "cargo".into(),
            args: ["build", "--locked", "-p", "regia_cli", "--bin", "regia"]
                .map(String::from)
                .to_vec(),
            env: vec![("CARGO_TARGET_DIR".into(), target.to_string_lossy().into_owned())],
        },
        Some(&tree),
        true,
    )
    .with_context(|| format!("building the CLI of {tag}"))?;
    println!(
        "two-tag-upgrade: {tag} built in {} s",
        started.elapsed().as_secs()
    );
    let regia = target
        .join("debug")
        .join(format!("regia{}", std::env::consts::EXE_SUFFIX));
    let releases = work.join(format!("releases-{name}"));
    let out = run(
        &Step {
            stage: "package",
            program: regia.to_string_lossy().into_owned(),
            args: vec![
                "engine".into(),
                "package".into(),
                "--engine".into(),
                tree.to_string_lossy().into_owned(),
                "--out".into(),
                releases.to_string_lossy().into_owned(),
                "--json".into(),
            ],
            env: Vec::new(),
        },
        None,
        false,
    )
    .with_context(|| format!("`regia engine package` of {tag} (does the tag carry spec 94?)"))?;
    let doc: serde_json::Value =
        serde_json::from_str(&out).with_context(|| format!("`engine package` of {tag} printed no JSON"))?;
    let dir = doc["dir"]
        .as_str()
        .with_context(|| format!("`engine package` of {tag} printed no `dir`"))?;
    Ok(Built {
        regia,
        release: PathBuf::from(dir),
    })
}

fn secs(started: Instant) -> u64 {
    started.elapsed().as_secs()
}

/// Runs one template through the three stages.
fn check_template(paths: &Paths, template: &str) -> Row {
    let mut seconds = [0u64; 3];
    let fail = |stage: &'static str, err: &anyhow::Error, seconds: [u64; 3]| Row {
        template: template.to_owned(),
        outcome: Outcome::Failed {
            stage,
            message: format!("{err:#}"),
        },
        seconds,
    };
    let started = Instant::now();
    let created = match run(&new_step(paths, template), None, false) {
        Ok(out) => out,
        Err(err) => return fail("new", &err, seconds),
    };
    seconds[0] = secs(started);
    let project = PathBuf::from(created.trim());
    let steps = upgrade_and_check_steps(paths, &project);
    let started = Instant::now();
    if let Err(err) = run(&steps[0], None, false) {
        return fail("upgrade", &err, seconds);
    }
    seconds[1] = secs(started);
    let started = Instant::now();
    let mut checks = vec![steps[1].clone()];
    let server = project.join("server").join("Cargo.toml");
    if server.is_file() {
        checks.push(check_step(paths, &server));
    }
    for step in &checks {
        if let Err(err) = run(step, None, false) {
            seconds[2] = secs(started);
            return fail("check", &err, seconds);
        }
    }
    seconds[2] = secs(started);
    Row {
        template: template.to_owned(),
        outcome: Outcome::Ok,
        seconds,
    }
}

/// Runs the job.
///
/// # Errors
///
/// Bad arguments, a missing tag, or a tag whose CLI does not build or package. A template that fails is
/// a report row and exit code 1, not an error.
pub fn run_job(root: &Path, args: &Args) -> Result<ExitCode> {
    validate_tags(&args.from, &args.to)?;
    let templates = resolve_templates(&args.templates)?;
    let work = args
        .work_dir
        .clone()
        .unwrap_or_else(|| root.join("target").join("two-tag"));
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let old = build_tag(root, &work, &args.from)?;
    let new = build_tag(root, &work, &args.to)?;
    let projects = work.join("projects");
    let _ = std::fs::remove_dir_all(&projects);
    std::fs::create_dir_all(&projects).with_context(|| format!("creating {}", projects.display()))?;
    let paths = Paths {
        old_regia: old.regia,
        new_regia: new.regia,
        old_release: old.release,
        new_release: new.release,
        projects: projects.clone(),
        check_target: work.join("target-check"),
    };
    let mut rows = Vec::new();
    for template in templates {
        println!("two-tag-upgrade: {template}");
        let row = check_template(&paths, template);
        println!(
            "two-tag-upgrade: {template}: {}",
            if row.outcome == Outcome::Ok {
                "ok"
            } else {
                "FAILED"
            }
        );
        rows.push(row);
    }
    println!("\n{}", render_report(&args.from, &args.to, &rows));
    if !args.keep {
        let _ = std::fs::remove_dir_all(&projects);
        for tag in [&args.from, &args.to] {
            let tree = work.join(tag_dir_name(tag));
            let _ = git(root, &["worktree", "remove", "--force", &tree.to_string_lossy()]);
        }
    }
    Ok(if rows.iter().all(|r| r.outcome == Outcome::Ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths {
            old_regia: "/w/a/regia".into(),
            new_regia: "/w/b/regia".into(),
            old_release: "/w/ra/regia-engine-0.2.8".into(),
            new_release: "/w/rb/regia-engine-0.2.11".into(),
            projects: "/w/projects".into(),
            check_target: "/w/target-check".into(),
        }
    }

    #[test]
    fn new_uses_the_old_cli_and_the_old_release() {
        let step = new_step(&paths(), "third_person");
        assert_eq!(step.program, "/w/a/regia");
        assert_eq!(
            step.args,
            [
                "new",
                "tt_third_person",
                "--template",
                "third_person",
                "--path",
                "/w/projects",
                "--engine-source",
                "path:/w/ra/regia-engine-0.2.8"
            ]
        );
    }

    #[test]
    fn upgrade_uses_the_new_cli_and_check_the_new_target() {
        let steps = upgrade_and_check_steps(&paths(), Path::new("/w/projects/tt_blank"));
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].stage, "upgrade");
        assert_eq!(steps[0].program, "/w/b/regia");
        assert_eq!(
            steps[0].args,
            [
                "upgrade-project",
                "--project",
                "/w/projects/tt_blank",
                "--write",
                "--engine",
                "path:/w/rb/regia-engine-0.2.11"
            ]
        );
        assert_eq!(steps[1].stage, "check");
        assert_eq!(steps[1].program, "cargo");
        assert_eq!(
            steps[1].args,
            ["check", "--manifest-path", "/w/projects/tt_blank/Cargo.toml"]
        );
        assert_eq!(
            steps[1].env,
            [("CARGO_TARGET_DIR".to_owned(), "/w/target-check".to_owned())]
        );
    }

    #[test]
    fn path_sources_use_forward_slashes() {
        assert_eq!(path_source(Path::new("C:\\rel\\x")), "path:C:/rel/x");
    }

    #[test]
    fn tags_are_validated() {
        assert!(validate_tags("v0.2.8", "v0.2.11").is_ok());
        assert!(validate_tags("", "v1").is_err());
        assert!(validate_tags("v1", "v1").is_err());
        assert!(validate_tags("--upload-pack=x", "v1").is_err());
        assert!(validate_tags("a..b", "v1").is_err());
        assert!(validate_tags("a b", "v1").is_err());
        assert_eq!(tag_dir_name("rel/v0.2.8"), "rel_v0.2.8");
    }

    #[test]
    fn templates_resolve_all_or_a_validated_list() {
        let all = resolve_templates(&["all".to_owned()]).expect("all");
        assert_eq!(all, regia_templates::TEMPLATE_IDS);
        assert_eq!(resolve_templates(&[]).expect("default"), all);
        let two =
            resolve_templates(&["quick".to_owned(), "blank".to_owned(), "quick".to_owned()]).expect("two");
        assert_eq!(two, ["quick", "blank"]);
        let err = resolve_templates(&["nope".to_owned()]).expect_err("unknown");
        assert!(err.to_string().contains("third_person"), "{err}");
    }

    #[test]
    fn the_report_counts_and_names_failures() {
        let rows = vec![
            Row {
                template: "blank".into(),
                outcome: Outcome::Ok,
                seconds: [1, 2, 3],
            },
            Row {
                template: "quick".into(),
                outcome: Outcome::Failed {
                    stage: "check",
                    message: "error[E0432]".into(),
                },
                seconds: [1, 1, 9],
            },
        ];
        let text = render_report("v0.2.8", "v0.2.11", &rows);
        assert!(text.contains("| blank | ok | 1 | 2 | 3 |"), "{text}");
        assert!(text.contains("| quick | FAILED at check | 1 | 1 | 9 |"), "{text}");
        assert!(text.contains("1 of 2 templates ok"), "{text}");
        assert!(text.contains("quick (check):\nerror[E0432]"), "{text}");
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail("a", 5), "a");
    }
}
