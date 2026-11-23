// SPDX-License-Identifier: MPL-2.0
//! `regia hooks install|uninstall|status`: the git pre-commit hook that runs
//! `regia assets validate --staged` before each commit (`docs/specs/32-asset-validation-and-audit.md`
//! § 4, ADR-7789, book `agents/project-health.md`).
//!
//! `install` writes the hook where git looks for it (`core.hooksPath` and linked worktrees
//! included) and refuses to replace a hook Regia did not write unless `--force`, which keeps the old
//! one beside it (`pre-commit.before-regia`); `uninstall` removes Regia's hook and puts that one
//! back. The work is `regia_asset_pipeline::precommit`, which the editor's Project Settings ▸
//! Validation page calls too. Errors print their stable code first (`hook_foreign: ...`).

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{Args, Subcommand};
use regia_asset_pipeline::precommit::{self, HookChange, PrecommitError};
use serde_json::json;

/// `regia hooks` subcommands.
#[derive(Debug, Subcommand)]
pub enum HooksCommand {
    /// Write the pre-commit hook that validates the staged assets (`--force` replaces another hook
    /// and keeps it beside the new one).
    Install(HookArgs),
    /// Remove Regia's pre-commit hook and restore the hook it replaced (`--force` removes another
    /// hook too).
    Uninstall(HookArgs),
    /// Where the hook is and whether Regia wrote it.
    Status(HookStatusArgs),
}

/// `regia hooks install|uninstall` arguments.
#[derive(Debug, Args)]
pub struct HookArgs {
    /// Project directory (inside a git work tree).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Replace (install) or remove (uninstall) a hook Regia did not write.
    #[arg(long, default_value_t = false)]
    pub force: bool,
    /// Print JSON on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `regia hooks status` arguments.
#[derive(Debug, Args)]
pub struct HookStatusArgs {
    /// Project directory (inside a git work tree).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Print JSON on stdout.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

fn coded(err: &PrecommitError) -> anyhow::Error {
    anyhow!("{}: {err}", err.code())
}

/// The `regia` program the hook runs: this executable.
fn this_program() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("regia"))
}

fn change_text(change: &HookChange) -> String {
    let mut text = format!("pre-commit hook {}: {}", change.action, change.path.display());
    if let Some(backup) = &change.backup {
        let verb = if change.action == "restored" {
            "put back from"
        } else {
            "previous hook kept at"
        };
        text.push_str(&format!(" ({verb} {})", backup.display()));
    }
    text
}

/// The output of a `regia hooks` command.
///
/// # Errors
///
/// The stable code first: `hook_foreign`, `hook_missing`, `not_a_git_repo`, `git_not_found`,
/// `git_failed`, `hook_io`.
pub fn output(command: &HooksCommand) -> Result<String> {
    let (change, json_out) = match command {
        HooksCommand::Install(args) => (
            precommit::install_hook(&args.project, &this_program(), args.force),
            args.json,
        ),
        HooksCommand::Uninstall(args) => (precommit::uninstall_hook(&args.project, args.force), args.json),
        HooksCommand::Status(args) => {
            let state = precommit::hook_state(&args.project).map_err(|e| coded(&e))?;
            if args.json {
                return Ok(serde_json::to_string_pretty(&state)?);
            }
            let what = match (state.exists, state.regia) {
                (false, _) => "not installed",
                (true, true) => "installed (Regia)",
                (true, false) => "another hook (not Regia's)",
            };
            let backup = if state.backup {
                " (a replaced hook is kept beside it)"
            } else {
                ""
            };
            return Ok(format!(
                "pre-commit hook {what}: {}{backup}",
                state.path.display()
            ));
        }
    };
    let change = change.map_err(|e| coded(&e))?;
    if json_out {
        Ok(serde_json::to_string_pretty(
            &json!({ "ok": true, "change": change }),
        )?)
    } else {
        Ok(change_text(&change))
    }
}

/// Runs a `regia hooks` command.
///
/// # Errors
///
/// As [`output`].
pub fn run(command: &HooksCommand) -> Result<()> {
    println!("{}", output(command)?);
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// A `git` command with a test identity and no commit signing (the host's other settings,
    /// such as `core.hooksPath`, stay: the library reads the same ones).
    pub(crate) fn git(dir: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .args([
                "-c",
                "user.name=Regia Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"]);
        command
    }

    /// Runs git in `dir` and panics with its stderr when it fails.
    pub(crate) fn git_ok(dir: &Path, args: &[&str]) {
        let out = git(dir).args(args).output().expect("git runs (git is required)");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A new repository with one commit, as a temporary directory.
    pub(crate) fn repo(tag: &str) -> PathBuf {
        let dir = regia_test_utils::unique_temp_dir(tag);
        git_ok(&dir, &["init", "-q"]);
        std::fs::write(dir.join("README.md"), "test\n").expect("readme");
        git_ok(&dir, &["add", "README.md"]);
        git_ok(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    fn args(dir: &Path, force: bool) -> HookArgs {
        HookArgs {
            project: dir.to_path_buf(),
            force,
            json: false,
        }
    }

    #[test]
    fn hooks_install_refuses_a_foreign_hook_and_uninstall_restores_it() {
        let dir = repo("cli-hooks-install");
        let hook = precommit::hook_path(&dir).expect("hook path");
        let text = output(&HooksCommand::Install(args(&dir, false))).expect("installs");
        assert!(text.starts_with("pre-commit hook installed"), "{text}");
        let script = std::fs::read_to_string(&hook).expect("hook written");
        assert!(script.contains(precommit::HOOK_MARKER), "{script}");
        assert!(
            script.contains("assets validate --staged --project '.'"),
            "{script}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&hook).expect("meta").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "executable: {mode:o}");
        }
        // Again: Regia's own hook is rewritten.
        let text = output(&HooksCommand::Install(args(&dir, false))).expect("updates");
        assert!(text.starts_with("pre-commit hook updated"), "{text}");

        // A foreign hook is refused without --force, and left alone.
        std::fs::write(&hook, "#!/bin/sh\necho mine\n").expect("foreign hook");
        let err = output(&HooksCommand::Install(args(&dir, false))).expect_err("refused");
        assert!(err.to_string().starts_with("hook_foreign:"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&hook).expect("kept"),
            "#!/bin/sh\necho mine\n"
        );
        let err = output(&HooksCommand::Uninstall(args(&dir, false))).expect_err("not ours");
        assert!(err.to_string().starts_with("hook_foreign:"), "{err}");

        // --force replaces it and keeps it; uninstall puts it back.
        let text = output(&HooksCommand::Install(args(&dir, true))).expect("forced");
        assert!(text.contains("previous hook kept at"), "{text}");
        let state = precommit::hook_state(&dir).expect("state");
        assert!(state.regia && state.backup, "{state:?}");
        let text = output(&HooksCommand::Uninstall(args(&dir, false))).expect("uninstalls");
        assert!(text.starts_with("pre-commit hook restored"), "{text}");
        assert_eq!(
            std::fs::read_to_string(&hook).expect("restored"),
            "#!/bin/sh\necho mine\n"
        );

        // Uninstall with no hook at all.
        std::fs::remove_file(&hook).expect("rm");
        let err = output(&HooksCommand::Uninstall(args(&dir, false))).expect_err("none");
        assert!(err.to_string().starts_with("hook_missing:"), "{err}");
        let status = output(&HooksCommand::Status(HookStatusArgs {
            project: dir.clone(),
            json: true,
        }))
        .expect("status");
        let value: serde_json::Value = serde_json::from_str(&status).expect("json");
        assert_eq!(value["exists"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review sprint-w12-rev-g5 #16: a hook that is a symlink to a missing script is a foreign
    /// hook, not a free slot: install without --force refuses it and leaves the link.
    #[cfg(unix)]
    #[test]
    fn a_dangling_hook_symlink_is_foreign() {
        let dir = repo("cli-hooks-dangling");
        let hook = precommit::hook_path(&dir).expect("hook path");
        std::fs::create_dir_all(hook.parent().expect("hooks dir")).expect("hooks dir");
        let target = dir.join("missing-hook-script");
        std::os::unix::fs::symlink(&target, &hook).expect("symlink");
        let state = precommit::hook_state(&dir).expect("state");
        assert!(state.exists && !state.regia, "{state:?}");
        let err = output(&HooksCommand::Install(args(&dir, false))).expect_err("refused");
        assert!(err.to_string().starts_with("hook_foreign:"), "{err}");
        assert_eq!(std::fs::read_link(&hook).expect("still a link"), target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review sprint-w12-rev-g5 #17: --force writes the new script before it moves the foreign
    /// hook aside, so a write that fails leaves the foreign hook in place; a second forced replace
    /// keeps the first backup and puts the next hook beside it.
    #[test]
    fn a_forced_install_never_loses_a_hook() {
        let dir = repo("cli-hooks-force-safe");
        let hook = precommit::hook_path(&dir).expect("hook path");
        std::fs::create_dir_all(hook.parent().expect("hooks dir")).expect("hooks dir");
        let first = "#!/bin/sh\necho first\n";
        std::fs::write(&hook, first).expect("foreign hook");
        // A directory where the temporary script goes: the write fails.
        let mut tmp = hook.as_os_str().to_owned();
        tmp.push(".regia-tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::create_dir_all(&tmp).expect("blocker");
        output(&HooksCommand::Install(args(&dir, true))).expect_err("the write fails");
        assert_eq!(
            std::fs::read_to_string(&hook).expect("kept"),
            first,
            "the foreign hook stays"
        );
        let backup = {
            let mut name = hook.as_os_str().to_owned();
            name.push(precommit::HOOK_BACKUP_SUFFIX);
            PathBuf::from(name)
        };
        assert!(!backup.exists(), "nothing was moved aside");
        std::fs::remove_dir_all(&tmp).expect("unblock");
        // Forced: the first hook goes to the backup.
        output(&HooksCommand::Install(args(&dir, true))).expect("forced");
        assert_eq!(std::fs::read_to_string(&backup).expect("backup"), first);
        // Another foreign hook, forced again: the first backup stays, the second sits beside it.
        let second = "#!/bin/sh\necho second\n";
        std::fs::write(&hook, second).expect("second foreign hook");
        output(&HooksCommand::Install(args(&dir, true))).expect("forced again");
        assert_eq!(
            std::fs::read_to_string(&backup).expect("first backup kept"),
            first
        );
        let mut name = backup.as_os_str().to_owned();
        name.push(".2");
        assert_eq!(
            std::fs::read_to_string(PathBuf::from(name)).expect("second backup"),
            second
        );
        let script = std::fs::read_to_string(&hook).expect("hook");
        assert!(script.contains(precommit::HOOK_MARKER), "{script}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hooks_outside_a_repository_is_a_coded_error() {
        let dir = regia_test_utils::unique_temp_dir("cli-hooks-norepo");
        // A directory outside any work tree: GIT_CEILING_DIRECTORIES is not settable per call here,
        // so the temp dir must not sit inside a repository (the system temp dir never does).
        if precommit::repo_root(&dir).is_ok() {
            eprintln!(
                "skipped: the temp dir {} is inside a git work tree",
                dir.display()
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let err = output(&HooksCommand::Install(args(&dir, false))).expect_err("no repo");
        assert!(err.to_string().starts_with("not_a_git_repo:"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
