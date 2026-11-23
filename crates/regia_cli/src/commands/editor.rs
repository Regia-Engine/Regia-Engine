// SPDX-License-Identifier: MPL-2.0
//! `regia editor [PROJECT_DIR] [--project DIR] [--headless] [--no-gpu] [--game CRATE]`: runs the
//! editor on a project (`docs/specs/21` § 1, `docs/specs/23` § 2, ADR-2412).
//!
//! The editor is its own binary (`regia-editor`, desktop only). This command finds it (`--editor-bin`,
//! then `REGIA_EDITOR`, then next to this `regia`, then on `PATH`) and runs it with the project. On
//! Unix it replaces this process (`exec`), so SIGTERM and SIGINT reach the editor directly and its
//! exit status is this command's. On Windows it runs the editor as a child in the same console:
//! Ctrl+C reaches both, this process only records it and waits for the editor to save and exit.
//!
//! `--headless` runs the editor without a window (CI, cloud agents): it prints one JSON line
//! `{"event":"ready",...}` on stdout once its agent server listens, and exits cleanly on
//! `agent.shutdown`, SIGTERM or Ctrl+C (`book/agents/headless-editor.md`).

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(not(unix))]
use anyhow::bail;
use anyhow::{Context, Result};
use clap::Args;

/// Arguments of `regia editor`.
#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct EditorArgs {
    /// `install-launcher`: add the editor to this desktop's app launcher (ADR-20000).
    #[command(subcommand)]
    pub command: Option<EditorCommand>,
    /// Project directory (default: `--project`, else the current directory).
    #[arg(value_name = "PROJECT_DIR")]
    pub project_dir: Option<PathBuf>,
    /// Project directory (the same as PROJECT_DIR).
    #[arg(long, conflicts_with = "project_dir")]
    pub project: Option<PathBuf>,
    /// Run without a window: the edit world, the asset DB, hot reload and the agent server only.
    #[arg(long)]
    pub headless: bool,
    /// With --headless: open no GPU device; view methods answer `editor_headless`.
    #[arg(long, requires = "headless")]
    pub no_gpu: bool,
    /// Host this game crate for hot reload and play-in-editor.
    #[arg(long, value_name = "CRATE_DIR")]
    pub game: Option<PathBuf>,
    /// Exit after this many frames (a headless editor saves first, like on SIGTERM).
    #[arg(long, value_name = "N")]
    pub exit_after_frames: Option<u64>,
    /// The editor binary (default: `REGIA_EDITOR`, then `regia-editor` next to this `regia`, then
    /// on `PATH`).
    #[arg(long, value_name = "FILE")]
    pub editor_bin: Option<PathBuf>,
}

/// Subcommands of `regia editor`.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum EditorCommand {
    /// Add the Regia Editor to this desktop's app launcher, per user (Linux desktop entry and
    /// icons, Windows Start Menu shortcut, macOS app bundle in ~/Applications); `--uninstall`
    /// removes it (`book/agents/editor-launcher.md`).
    InstallLauncher(super::editor_launcher::InstallLauncherArgs),
}

/// The editor binary's file name on this platform.
#[must_use]
pub fn editor_file_name() -> String {
    format!("regia-editor{}", std::env::consts::EXE_SUFFIX)
}

/// Where the editor binary is: `explicit`, else `env` (`REGIA_EDITOR`), else `sibling_dir`'s
/// `regia-editor` when it exists, else the bare name (looked up on `PATH` when started).
#[must_use]
pub fn resolve_editor_bin(
    explicit: Option<&Path>,
    env: Option<PathBuf>,
    sibling_dir: Option<&Path>,
) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Some(path) = env.filter(|p| !p.as_os_str().is_empty()) {
        return path;
    }
    if let Some(dir) = sibling_dir {
        let sibling = dir.join(editor_file_name());
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(editor_file_name())
}

/// The editor's arguments for `args` on the (resolved) `project`.
#[must_use]
pub fn editor_argv(args: &EditorArgs, project: &Path) -> Vec<String> {
    // Without Windows' `\\?\` prefix, which `canonicalize` adds there.
    let mut argv = vec![
        "--project".to_string(),
        regia_asset_pipeline::generators::plain_path(project),
    ];
    if args.headless {
        argv.push("--headless".to_string());
    }
    if args.no_gpu {
        argv.push("--no-gpu".to_string());
    }
    if let Some(game) = &args.game {
        argv.push("--game".to_string());
        argv.push(game.display().to_string());
    }
    if let Some(frames) = args.exit_after_frames {
        argv.push("--exit-after-frames".to_string());
        argv.push(frames.to_string());
    }
    argv
}

/// Runs `regia editor`. On Unix this only returns on failure (the editor replaced the process).
///
/// # Errors
///
/// The project folder does not exist, or the editor binary could not start (`editor_not_found`).
pub fn run(args: &EditorArgs) -> Result<()> {
    if let Some(EditorCommand::InstallLauncher(launcher)) = &args.command {
        return super::editor_launcher::run(launcher);
    }
    let raw = args
        .project_dir
        .clone()
        .or_else(|| args.project.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    let project = raw
        .canonicalize()
        .with_context(|| format!("project directory {} not found", raw.display()))?;
    if !project.join("Regia.toml").is_file() {
        tracing::warn!(target: "regia_cli", "{} has no Regia.toml; the editor opens it as a plain folder", project.display());
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let editor = resolve_editor_bin(
        args.editor_bin.as_deref(),
        regia_core::brand::env_var("REGIA_EDITOR").ok().map(PathBuf::from),
        sibling.as_deref(),
    );
    let mut command = Command::new(&editor);
    command.args(editor_argv(args, &project));
    let not_found = |e: std::io::Error| {
        anyhow::anyhow!(
            "editor_not_found: cannot start {}: {e} (build it with `cargo build -p regia_editor`, or pass --editor-bin / set REGIA_EDITOR)",
            editor.display()
        )
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // `exec` returns only on failure.
        let err = command.exec();
        Err(not_found(err))
    }
    #[cfg(not(unix))]
    {
        // Ctrl+C reaches the editor too (same console): record it here and keep waiting for the
        // editor to save and exit, so this process ends with the editor's status.
        #[cfg(windows)]
        let _ =
            regia::platform::console_ctrl::install(regia::platform::console_ctrl::ConsoleFlags::default());
        let status = command.status().map_err(not_found)?;
        match status.code() {
            Some(0) => Ok(()),
            Some(code) => std::process::exit(code),
            None => bail!("the editor ended without an exit status"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> EditorArgs {
        EditorArgs {
            command: None,
            project_dir: None,
            project: None,
            headless: false,
            no_gpu: false,
            game: None,
            exit_after_frames: None,
            editor_bin: None,
        }
    }

    #[test]
    fn the_editor_binary_is_found_in_order() {
        let dir = regia_test_utils::unique_temp_dir("cli-editor-bin");
        std::fs::create_dir_all(&dir).expect("dir");
        let explicit = PathBuf::from("/opt/regia/editor");
        assert_eq!(
            resolve_editor_bin(Some(&explicit), Some("/env/editor".into()), Some(&dir)),
            explicit
        );
        assert_eq!(
            resolve_editor_bin(None, Some("/env/editor".into()), Some(&dir)),
            PathBuf::from("/env/editor")
        );
        // No sibling yet: the bare name, looked up on PATH.
        assert_eq!(
            resolve_editor_bin(None, Some(PathBuf::new()), Some(&dir)),
            PathBuf::from(editor_file_name())
        );
        std::fs::write(dir.join(editor_file_name()), b"").expect("sibling");
        assert_eq!(
            resolve_editor_bin(None, None, Some(&dir)),
            dir.join(editor_file_name())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn headless_flags_reach_the_editor() {
        let project = Path::new("/p");
        assert_eq!(editor_argv(&args(), project), ["--project", "/p"]);
        let headless = EditorArgs {
            headless: true,
            no_gpu: true,
            game: Some("game".into()),
            exit_after_frames: Some(3),
            ..args()
        };
        assert_eq!(
            editor_argv(&headless, project),
            [
                "--project",
                "/p",
                "--headless",
                "--no-gpu",
                "--game",
                "game",
                "--exit-after-frames",
                "3"
            ]
        );
    }

    #[derive(clap::Parser)]
    struct Wrapper {
        #[command(flatten)]
        args: EditorArgs,
    }

    #[test]
    fn install_launcher_is_a_subcommand_and_a_project_dir_still_parses() {
        use clap::Parser as _;
        let parsed =
            Wrapper::try_parse_from(["editor", "install-launcher", "--uninstall", "--json"]).expect("parse");
        match parsed.args.command {
            Some(EditorCommand::InstallLauncher(a)) => assert!(a.uninstall && a.json && !a.dry_run),
            None => panic!("install-launcher parsed as a project directory"),
        }
        let parsed = Wrapper::try_parse_from(["editor", "games/demo", "--headless"]).expect("parse");
        assert!(parsed.args.command.is_none());
        assert_eq!(parsed.args.project_dir.as_deref(), Some(Path::new("games/demo")));
    }
}
