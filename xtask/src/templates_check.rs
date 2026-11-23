// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask templates-check`: every project template, generated and built (`docs/06` § 5, ADR-2046).
//!
//! For each template id (`regia_templates::TEMPLATE_IDS`) the job runs `regia new` into a fresh
//! temp directory, builds the generated workspace with `cargo build --workspace` (the Android
//! crate excluded: the `android` job builds that target, and a host build of a `cdylib` for Android
//! proves nothing), then starts the generated game binary with `--exit-after-frames 5` as the
//! headless smoke (a display is needed on Linux: CI wraps the job in `xvfb-run`; `--no-run` skips
//! the smoke where there is none). Any generated project that does not compile, or whose binary
//! exits non-zero, fails the job. A template whose game plugin does not build, as happened when a
//! plugin tuple grew past the bundle limit, is caught here and by nothing else.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;

/// Arguments of `cargo xtask templates-check`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Only these templates (repeatable); default: all of them.
    #[arg(long = "template")]
    pub templates: Vec<String>,
    /// Build only; do not start the generated game (no display available).
    #[arg(long)]
    pub no_run: bool,
    /// Cargo target directory shared by the generated projects (default: a folder under the temp dir).
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
    /// Keep the generated projects (their paths are printed) instead of deleting them.
    #[arg(long)]
    pub keep: bool,
}

/// What to build and run for one template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Template id.
    pub template: String,
    /// The generated crate name (`regia new <name>` with the snake_case name below).
    pub crate_name: String,
    /// Arguments after `cargo`.
    pub build_args: Vec<String>,
    /// Binary to start for the smoke, relative to the target directory's `debug/`.
    pub binary: String,
    /// Run the smoke. The `net_game` client needs a server, and its own tests run the pair, so this
    /// job only builds it.
    pub smoke: bool,
    /// More cargo invocations after the build, each on its own (ADR-2440: the `net_game` server
    /// crate is never built with the client, and its hosted test needs no display).
    pub then: Vec<Vec<String>>,
}

/// The project name every template is generated under; its crate name is the same.
pub const PROJECT_NAME: &str = "tpl_check";

/// The smoke's arguments: five frames, then exit.
pub const SMOKE_ARGS: [&str; 2] = ["--exit-after-frames", "5"];

/// The build and smoke steps of `template`.
///
/// The workspace templates (`blank`, `first_person`, `third_person`, `top_down`) build every member
/// except `<crate>_android` and run the launcher's binary `<crate>`; `quick` is one crate and runs itself;
/// `net_game` builds its default members (the client) and is not run: its manifest explains why
/// `--workspace` does not apply.
#[must_use]
pub fn plan(template: &str) -> Plan {
    let crate_name = PROJECT_NAME.to_owned();
    let (build_args, binary) = if template == "quick" {
        (vec!["build".to_owned()], crate_name.clone())
    } else if template == "net_game" {
        (vec!["build".to_owned()], format!("{crate_name}_client"))
    } else {
        (
            vec![
                "build".to_owned(),
                "--workspace".to_owned(),
                "--exclude".to_owned(),
                format!("{crate_name}_android"),
            ],
            // The launcher crate is `<crate>_launcher`, but its `[[bin]]` is named `<crate>`.
            crate_name.clone(),
        )
    };
    let then = if template == "net_game" {
        vec![
            vec![
                "build".to_owned(),
                "-p".to_owned(),
                format!("{crate_name}_server"),
            ],
            vec![
                "test".to_owned(),
                "-p".to_owned(),
                crate_name.clone(),
                "--test".to_owned(),
                "hosted".to_owned(),
            ],
        ]
    } else {
        Vec::new()
    };
    Plan {
        template: template.to_owned(),
        crate_name,
        build_args,
        binary,
        smoke: template != "net_game",
        then,
    }
}

/// The templates to check: `requested` (each validated), or all of them.
///
/// # Errors
///
/// A requested id that is not a template.
pub fn selected(requested: &[String]) -> Result<Vec<&'static str>> {
    if requested.is_empty() {
        return Ok(regia_templates::TEMPLATE_IDS.to_vec());
    }
    requested
        .iter()
        .map(|id| {
            regia_templates::TEMPLATE_IDS
                .iter()
                .copied()
                .find(|known| known == id)
                .with_context(|| {
                    format!(
                        "unknown template `{id}` (available: {})",
                        regia_templates::TEMPLATE_IDS.join(", ")
                    )
                })
        })
        .collect()
}

fn step(label: &str, command: &mut Command) -> Result<()> {
    println!("templates-check: {label}");
    let status = command
        .status()
        .with_context(|| format!("{label}: cannot start the process"))?;
    if !status.success() {
        bail!("{label}: {status}");
    }
    Ok(())
}

/// Generates, builds and smokes one template under `work`.
fn check_one(root: &Path, plan: &Plan, work: &Path, target_dir: &Path, no_run: bool) -> Result<PathBuf> {
    let template = &plan.template;
    step(
        &format!("{template}: regia new"),
        Command::new("cargo")
            .current_dir(root)
            .args([
                "run",
                "--locked",
                "-q",
                "-p",
                "regia_cli",
                "--bin",
                "regia",
                "--",
                "new",
            ])
            .arg(PROJECT_NAME)
            .args(["--template", template, "--path"])
            .arg(work),
    )?;
    let project = work.join(&plan.crate_name);
    step(
        &format!("{template}: cargo {}", plan.build_args.join(" ")),
        Command::new("cargo")
            .current_dir(&project)
            .args(&plan.build_args)
            .env("CARGO_TARGET_DIR", target_dir),
    )?;
    for args in &plan.then {
        step(
            &format!("{template}: cargo {}", args.join(" ")),
            Command::new("cargo")
                .current_dir(&project)
                .args(args)
                .env("CARGO_TARGET_DIR", target_dir),
        )?;
    }
    if !no_run && plan.smoke {
        let exe = target_dir
            .join("debug")
            .join(format!("{}{}", plan.binary, std::env::consts::EXE_SUFFIX));
        step(
            &format!("{template}: {} {}", plan.binary, SMOKE_ARGS.join(" ")),
            Command::new(&exe)
                .current_dir(&project)
                .args(SMOKE_ARGS)
                .env("REGIA_AUDIO", "off"),
        )?;
    }
    Ok(project)
}

/// Runs the job; exit code 1 lists every template that failed.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let templates = selected(&args.templates)?;
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let base = std::env::temp_dir().join(format!(
        "regia-templates-check-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let target_dir = args.target_dir.clone().unwrap_or_else(|| base.join("target"));
    let mut failed = Vec::new();
    for template in templates {
        let work = base.join(template);
        std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
        match check_one(root, &plan(template), &work, &target_dir, args.no_run) {
            Ok(project) => {
                println!("templates-check: {template}: ok ({})", project.display());
                if !args.keep {
                    let _ = std::fs::remove_dir_all(&work);
                }
            }
            Err(err) => {
                eprintln!("templates-check: {template}: FAILED: {err:#}");
                failed.push(template);
            }
        }
    }
    if !args.keep && args.target_dir.is_none() {
        let _ = std::fs::remove_dir_all(&base);
    }
    if failed.is_empty() {
        println!("templates-check: ok");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("templates-check: failed templates: {}", failed.join(", "));
        Ok(ExitCode::FAILURE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_templates_exclude_android_and_run_the_launcher() {
        for id in ["blank", "first_person", "third_person", "top_down"] {
            let plan = plan(id);
            assert!(plan.smoke, "{id}");
            assert_eq!(plan.binary, "tpl_check", "{id}");
            assert_eq!(
                plan.build_args,
                ["build", "--workspace", "--exclude", "tpl_check_android"],
                "{id}"
            );
        }
    }

    #[test]
    fn net_game_builds_the_client_and_is_not_run() {
        let plan = plan("net_game");
        assert_eq!(plan.build_args, ["build"]);
        assert!(!plan.smoke);
    }

    /// ADR-2440: the generated net game's server crate builds on its own, and its hosted test (the
    /// game under the server host, two clients joining by join code) runs.
    #[test]
    fn net_game_also_builds_the_server_and_runs_the_hosted_test() {
        let net = plan("net_game");
        assert_eq!(
            net.then,
            [
                vec!["build", "-p", "tpl_check_server"],
                vec!["test", "-p", "tpl_check", "--test", "hosted"],
            ]
        );
        assert!(plan("quick").then.is_empty());
    }

    #[test]
    fn quick_builds_its_one_crate_and_runs_it() {
        let plan = plan("quick");
        assert_eq!(plan.build_args, ["build"]);
        assert_eq!(plan.binary, "tpl_check");
    }

    #[test]
    fn selection_defaults_to_every_template_and_refuses_unknown_ids() {
        assert_eq!(selected(&[]).expect("all"), regia_templates::TEMPLATE_IDS);
        assert_eq!(selected(&["quick".to_owned()]).expect("one"), ["quick"]);
        let err = selected(&["nope".to_owned()]).expect_err("unknown");
        assert!(err.to_string().contains("blank"), "{err}");
    }

    #[test]
    fn the_project_name_is_its_own_crate_name() {
        // `regia new` snake-cases the name: it must come back unchanged, or the plan's crate names are wrong.
        assert!(
            PROJECT_NAME
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        );
    }
}
