// SPDX-License-Identifier: MPL-2.0
//! `regia run` — desktop dev run of the project's launcher (`docs/specs/21-tooling-and-packaging.md`
//! § 1), and with `--dev` the live asset loop of `docs/specs/12-assets.md` § 8.
//!
//! Before the game starts the project is cooked (incrementally). With `--dev` this process then
//! hosts the asset database: a [`LiveCooker`] re-cooks every saved source into the cooked cache
//! and prints `reloaded X in N ms` / `cook error in Y: …`. The game is built with the
//! `regia/dev-assets` feature, so its asset plugin watches that cache (cooked assets reload in
//! place) and the source files it reads directly (shaders, levels, VFX, UI layouts, audio,
//! terrain layers, scatter). A shipping build (`regia package`) never has the feature.
//!
//! A dev build has no packaged `net-manifest.ron`: after the cook, `regia run` computes it from the dev
//! cache exactly as `regia server` does (`server_content::write_net_manifest`, into
//! `<state>/net-dev/`), logs how long that took, and hands it to the game in `REGIA_NET_MANIFEST`, so a
//! dev client and a dev server compare the same content (spec 77 § 4.1, ADR-7212). A failure is logged
//! and the game runs without one; a manifest named in the caller's environment wins.
//!
//! `--level L` starts the game in `L` (a `.level.ron` or `.level.d` under the project, or under `assets/`):
//! the absolute path goes to the game in `REGIA_START_LEVEL`, and the engine's
//! `StartLevelPlugin` opens it after the game's own startup (and, with `--dev`, again on save).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_asset::{ReloadMessage, WatchOutcome};
use regia_asset_pipeline::{CookOptions, LiveCooker, LiveCookerConfig, cook_project};

/// Build and run the project's desktop launcher.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Project directory (contains `Regia.toml` and `launcher/`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Build with the release profile.
    #[arg(long)]
    pub release: bool,
    /// Live asset reload: re-cook saved sources in the background and reload them in the game.
    #[arg(long)]
    pub dev: bool,
    /// Start in this level (`.level.ron` or `.level.d`, relative to the project or to its `assets/`).
    #[arg(long)]
    pub level: Option<PathBuf>,
    /// Print one JSON object on one stdout line when the game exits: `ok`, `artifact` (the
    /// launcher binary cargo built), `exit_code`, `package`, `profile`, `level` and the cook
    /// counts. Every other line, the game's stdout included, goes to stderr (spec 21 § 1, ADR-7217).
    #[arg(long)]
    pub json: bool,
    /// Arguments passed to the game after `--`.
    #[arg(last = true)]
    pub game_args: Vec<String>,
}

/// The `[package] name` of the manifest at `path`, when it has one.
fn package_name(path: &Path) -> Option<String> {
    let value: toml::Value = toml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(value.get("package")?.get("name")?.as_str()?.to_string())
}

/// What to run instead when `regia run` (or, for a quick app, `regia editor`) cannot run `dir`
/// (ADR-10616): a `quick` app (a Cargo package that depends on `regia`, with no `Regia.toml`)
/// builds and runs with `cargo`, and a `net_game` project (a `Regia.toml`, no `launcher/`, a
/// `server/` and a `client/` crate) runs its server and client crates. `None` for anything else.
pub(crate) fn not_runnable_hint(dir: &Path) -> Option<String> {
    if !regia_core::brand::is_project_root(dir) {
        let text = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
        let value: toml::Value = toml::from_str(&text).ok()?;
        let depends_on_regia = value
            .get("dependencies")
            .and_then(|deps| deps.get("regia"))
            .is_some();
        return (value.get("package").is_some() && depends_on_regia).then(|| {
            "it looks like a `quick` app (one Cargo crate, no Regia.toml): build and run it with \
             `cargo build` and `cargo run` in that folder. `regia run` and `regia editor` need a project \
             from the blank, first_person, third_person, top_down or net_game template"
                .to_string()
        });
    }
    if dir.join("launcher/Cargo.toml").is_file() {
        return None;
    }
    let server = package_name(&dir.join("server/Cargo.toml"))?;
    let client = package_name(&dir.join("client/Cargo.toml"))?;
    Some(format!(
        "it is a `net_game` project, which has no launcher crate: build it with `cargo build`, then \
         run `cargo run -p {server}` and `cargo run -p {client}` as its README.md says"
    ))
}

/// The launcher's package name from `launcher/Cargo.toml`.
pub(crate) fn launcher_package(project: &Path) -> Result<String> {
    let manifest = project.join("launcher").join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).with_context(|| {
        format!(
            "{} not found: is this a Regia Engine project?",
            manifest.display()
        )
    })?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("cannot parse {}", manifest.display()))?;
    value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("{} has no [package] name", manifest.display()))
}

/// The key under which `launcher/Cargo.toml` depends on the engine's umbrella crate, for the
/// `<key>/dev-assets` feature: `regia`, or the key of a renamed dependency
/// (`<old name> = { package = "regia", ... }`, the smallest change the rename page suggests for a
/// project from before the rename, ADR-1429). `regia` when the manifest cannot be read.
fn engine_dependency_key(project: &Path) -> String {
    let manifest = project.join("launcher").join("Cargo.toml");
    std::fs::read_to_string(manifest)
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|value| {
            let deps = value.get("dependencies")?;
            crate::commands::ship_check::engine_dependency(deps).map(|(key, _)| key.to_owned())
        })
        .unwrap_or_else(|| regia_core::brand::MAIN_CRATE.to_owned())
}

/// [`engine_dependency_key`] for `regia server`: the root package's manifest, else the launcher's.
pub(crate) fn engine_dependency_key_of(project: &Path) -> String {
    let root = std::fs::read_to_string(project.join("Cargo.toml"))
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|value| {
            let deps = value.get("dependencies")?;
            crate::commands::ship_check::engine_dependency(deps).map(|(key, _)| key.to_owned())
        });
    root.unwrap_or_else(|| engine_dependency_key(project))
}

/// Environment variable the game's `StartLevelPlugin` reads (`regia_framework::start_level`).
const START_LEVEL_ENV: &str = "REGIA_START_LEVEL";

/// The absolute path of `--level L`: `L` itself when absolute, else under the project, else
/// under its `assets/`.
pub(crate) fn resolve_level(project: &Path, level: &Path) -> Result<PathBuf> {
    let is_level = level
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".level.ron") || n.ends_with(".level.d"));
    if !is_level {
        bail!(
            "--level {}: not a .level.ron file or a .level.d directory",
            level.display()
        );
    }
    let candidates = if level.is_absolute() {
        vec![level.to_path_buf()]
    } else {
        vec![project.join(level), project.join("assets").join(level)]
    };
    candidates
        .iter()
        .find(|p| p.exists())
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .with_context(|| {
            format!(
                "--level {}: no such file (looked at {})",
                level.display(),
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The `cargo run` invocation for the launcher. `level` is the resolved `--level`.
fn cargo_command(project: &Path, package: &str, args: &RunArgs, level: Option<&Path>) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.arg("run")
        .arg("--manifest-path")
        .arg(project.join("Cargo.toml"))
        .arg("-p")
        .arg(package);
    if args.release {
        cmd.arg("--release");
    }
    if args.dev {
        let engine = engine_dependency_key(project);
        cmd.arg("--features").arg(format!("{engine}/dev-assets"));
        cmd.env("REGIA_ASSET_WATCH", "1");
    } else {
        cmd.env("REGIA_ASSET_WATCH", "0");
    }
    if let Some(level) = level {
        cmd.env(START_LEVEL_ENV, level);
    } else {
        cmd.env_remove(START_LEVEL_ENV);
        // The game also reads the legacy spelling (ADR-1388), so a start level an old shell
        // still exports under it is cleared too (ADR-1429).
        if let Some((legacy, true)) = regia_core::brand::alternate_env_name(START_LEVEL_ENV) {
            cmd.env_remove(legacy);
        }
    }
    if !args.game_args.is_empty() {
        cmd.arg("--").args(&args.game_args);
    }
    cmd.current_dir(project);
    cmd
}

/// Where `regia run` writes the dev build's net manifest.
fn dev_net_manifest_dir(project: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(project).join("net-dev")
}

/// Computes the dev build's `net-manifest.ron` from the cooked dev cache, the way `regia server`
/// does (spec 77 § 4.1), and logs the time it took. `None` (logged) when it cannot be computed.
fn dev_net_manifest(project: &Path, platform: &str) -> Option<PathBuf> {
    let start = std::time::Instant::now();
    let version = match crate::manifest::Manifest::load(project) {
        Ok(m) => m.project.version,
        Err(e) => {
            eprintln!("regia run: no net manifest (Regia.toml: {e:#})");
            return None;
        }
    };
    match crate::commands::server_content::write_net_manifest(
        project,
        platform,
        &version,
        &dev_net_manifest_dir(project),
    ) {
        Ok(path) => {
            eprintln!(
                "regia run: net manifest computed from the dev cache in {} ms ({})",
                start.elapsed().as_millis(),
                path.display()
            );
            Some(path)
        }
        Err(e) => {
            eprintln!("regia run: no net manifest from the dev cache ({e:#}); sessions present an empty one");
            None
        }
    }
}

/// Hands the dev manifest to the game (`REGIA_NET_MANIFEST`).
fn with_dev_net_manifest(cmd: &mut Command, manifest: Option<&Path>) {
    if let Some(path) = manifest {
        cmd.env(regia_net::session::NET_MANIFEST_ENV, path);
    }
}

/// The hint shown when cargo is missing.
const CARGO_FIX: &str = "install Rust with cargo (https://rustup.rs) and run `regia doctor`";

/// A run made ready: the project cooked, the live cooker started (with `--dev`), the game's
/// `cargo run` command built.
struct Prepared {
    project: PathBuf,
    package: String,
    level: Option<PathBuf>,
    cmd: Command,
    cooked: usize,
    cook_skipped: usize,
    /// Kept alive while the game runs (`--dev`).
    _cooker: Option<LiveCooker>,
}

/// Cooks the project, starts the live cooker with `--dev`, and builds the game's command.
fn prepare(args: &RunArgs) -> Result<Prepared> {
    let project = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    if let Some(hint) = not_runnable_hint(&project) {
        bail!("`regia run` cannot run {}: {hint}", project.display());
    }
    if !regia_core::brand::is_project_root(&project) {
        bail!(
            "{} is not a Regia Engine project (no Regia.toml)",
            project.display()
        );
    }
    let package = launcher_package(&project)?;
    let level = args
        .level
        .as_deref()
        .map(|level| resolve_level(&project, level))
        .transpose()?;

    let report = cook_project(CookOptions::for_host(project.clone())).context("cook")?;
    crate::commands::cook::report_cook_outcome(&report)?;
    // A manifest the caller named (a `regia server --clients` client, a test) wins.
    let net_manifest = if regia_core::brand::env_var_os(regia_net::session::NET_MANIFEST_ENV).is_some() {
        None
    } else {
        dev_net_manifest(&project, &regia_asset::current_platform())
    };

    let cooker = if args.dev {
        let (tx, rx) = std::sync::mpsc::channel::<ReloadMessage>();
        let mut config = LiveCookerConfig::for_host(project.clone());
        // The game watches the sources it reads itself; this process only cooks.
        config.forward_sources = false;
        let cooker = LiveCooker::spawn(config, tx).context("start the live cooker")?;
        std::thread::Builder::new()
            .name("regia-run-status".into())
            .spawn(move || {
                for message in rx {
                    if let ReloadMessage::Status(entry) = message {
                        match entry.outcome {
                            WatchOutcome::Reloaded => eprintln!("regia run: {}", entry.toast()),
                            WatchOutcome::Error(_) => eprintln!("regia run: {}", entry.toast()),
                        }
                    }
                }
            })
            .context("status thread")?;
        eprintln!(
            "regia run: live asset reload on ({}); save a file under assets/ to see it in the game",
            cooker
                .roots()
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        Some(cooker)
    } else {
        None
    };

    if let Some(level) = &level {
        eprintln!("regia run: starting in {}", level.display());
    }
    let mut cmd = cargo_command(&project, &package, args, level.as_deref());
    with_dev_net_manifest(&mut cmd, net_manifest.as_deref());
    Ok(Prepared {
        project,
        package,
        level,
        cmd,
        cooked: report.cooked,
        cook_skipped: report.skipped,
        _cooker: cooker,
    })
}

/// Runs the command.
///
/// # Errors
///
/// Not a project, the cook failed fatally, or the game build/run failed.
pub fn run(args: &RunArgs) -> Result<()> {
    let mut prepared = prepare(args)?;
    crate::process::run(&mut prepared.cmd, CARGO_FIX)
}

/// `regia run --json`: runs the game with its stdout on stderr and answers the one result object
/// ([`json_result`]); a game that exits non-zero is `ok: false` in it, not an error.
///
/// # Errors
///
/// Not a project, the cook failed fatally, or cargo could not be started.
pub fn run_json(args: &RunArgs) -> Result<serde_json::Value> {
    let mut prepared = prepare(args)?;
    let line = crate::process::command_line(&prepared.cmd);
    tracing::info!("running: {line}");
    prepared.cmd.stdout(crate::json_out::child_stdout());
    let status = prepared.cmd.status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("`cargo` was not found. {CARGO_FIX}")
        } else {
            anyhow::anyhow!(e).context(format!("cannot start `{line}`"))
        }
    })?;
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|dir| {
            if dir.is_relative() {
                prepared.project.join(dir)
            } else {
                dir
            }
        })
        .unwrap_or_else(|| prepared.project.join("target"));
    Ok(json_result(
        &RunSummary {
            artifact: launcher_binary(&target_dir, &prepared.package, args.release),
            package: &prepared.package,
            release: args.release,
            level: prepared.level.as_deref(),
            cooked: prepared.cooked,
            cook_skipped: prepared.cook_skipped,
        },
        status.code(),
    ))
}

/// The launcher binary `cargo run -p <package>` builds under `target_dir`.
pub(crate) fn launcher_binary(target_dir: &Path, package: &str, release: bool) -> PathBuf {
    target_dir
        .join(if release { "release" } else { "debug" })
        .join(format!("{package}{}", std::env::consts::EXE_SUFFIX))
}

/// What a finished `regia run` reports in its `--json` object.
pub(crate) struct RunSummary<'a> {
    /// The launcher binary cargo built ([`launcher_binary`]).
    pub artifact: PathBuf,
    /// The launcher package.
    pub package: &'a str,
    /// `--release`.
    pub release: bool,
    /// The resolved `--level`.
    pub level: Option<&'a Path>,
    /// Sources the cook before the run cooked.
    pub cooked: usize,
    /// Sources it found current.
    pub cook_skipped: usize,
}

/// The `--json` object of a finished run (spec 21 § 1, ADR-7217): `ok` (the game exited 0),
/// `artifact`, `artifact_exists`, `exit_code` (null when a signal ended the game), `package`,
/// `profile` (`debug` or `release`), `level` and the cook counts.
pub(crate) fn json_result(run: &RunSummary<'_>, exit_code: Option<i32>) -> serde_json::Value {
    serde_json::json!({
        "ok": exit_code == Some(0),
        "artifact": run.artifact.display().to_string(),
        "artifact_exists": run.artifact.is_file(),
        "exit_code": exit_code,
        "package": run.package,
        "profile": if run.release { "release" } else { "debug" },
        "level": run.level.map(|l| l.display().to_string()),
        "cooked": run.cooked,
        "cook_skipped": run.cook_skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-10616 (a public tester's `regia run` in a quick app said only "no Regia.toml"): a quick
    /// app and a net_game project are told what to run instead; the launcher templates and plain
    /// folders get no hint. Every template is generated, so a template change that breaks the rule
    /// fails here.
    #[test]
    fn quick_and_net_game_projects_are_told_what_to_run_instead() {
        let parent = regia_test_utils::unique_temp_dir("run-hint");
        for &template in regia_templates::TEMPLATE_IDS {
            let created = regia_cli::new_project::create_with_engine(
                &regia_cli::new_project::NewProjectRequest {
                    name: format!("Hint {template}"),
                    template: template.into(),
                    parent: parent.clone(),
                },
                "/engine",
            )
            .expect("create");
            let hint = not_runnable_hint(&created.root);
            match template {
                "quick" => {
                    let hint = hint.expect("quick hint");
                    assert!(hint.contains("`cargo run`") && hint.contains("quick"), "{hint}");
                }
                "net_game" => {
                    let hint = hint.expect("net_game hint");
                    let crate_name = &created.crate_name;
                    assert!(
                        hint.contains(&format!("cargo run -p {crate_name}_server"))
                            && hint.contains(&format!("cargo run -p {crate_name}_client")),
                        "{hint}"
                    );
                }
                _ => assert_eq!(hint, None, "{template}"),
            }
            if template == "quick" || template == "net_game" {
                let run_args = RunArgs {
                    project: created.root.clone(),
                    ..args(false, false)
                };
                let error = prepare(&run_args).err().expect("refused").to_string();
                assert!(error.contains("`regia run` cannot run"), "{error}");
            }
        }
        let plain = parent.join("plain");
        std::fs::create_dir_all(&plain).expect("plain dir");
        assert_eq!(not_runnable_hint(&plain), None);
        std::fs::write(plain.join("Cargo.toml"), "[package]\nname = \"other\"\n").expect("manifest");
        assert_eq!(
            not_runnable_hint(&plain),
            None,
            "a crate without regia is not a quick app"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Spec 77 § 4.1 (ADR-7212): a dev run computes the net manifest from the dev cache, the same
    /// text `regia server` writes, follows a changed level, and hands its path to the game.
    #[test]
    fn a_dev_run_computes_the_net_manifest_from_the_dev_cache() {
        let dir = regia_test_utils::unique_temp_dir("run-net-manifest");
        std::fs::create_dir_all(dir.join("assets/levels")).expect("dir");
        std::fs::write(
            dir.join("assets/levels/a.level.ron"),
            "(format_version: 1, entities: [])",
        )
        .expect("write");
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"Dev\"\nid = \"dev.regia.dev\"\nversion = \"1.0.0\"\n",
        )
        .expect("write");
        let cook = || {
            let report = cook_project(CookOptions {
                project_root: dir.clone(),
                platform: "linux".into(),
                incremental: true,
            })
            .expect("cook");
            crate::commands::cook::report_cook_outcome(&report).expect("cook outcome");
        };
        cook();
        let path = dev_net_manifest(&dir, "linux").expect("a dev manifest");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            text,
            crate::commands::server_content::net_manifest_text(&dir, "linux", "1.0.0").expect("server text"),
            "the same manifest `regia server` writes"
        );
        std::fs::write(
            dir.join("assets/levels/a.level.ron"),
            "(format_version: 1, entities: [ ])",
        )
        .expect("edit level");
        cook();
        let again = dev_net_manifest(&dir, "linux").expect("a dev manifest");
        assert_ne!(
            std::fs::read_to_string(&again).expect("read"),
            text,
            "it follows the level"
        );

        let mut cmd = Command::new("game");
        with_dev_net_manifest(&mut cmd, Some(&again));
        let env: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(
            env.contains(&(
                regia_net::session::NET_MANIFEST_ENV.to_owned(),
                Some(again.to_string_lossy().into_owned())
            )),
            "{env:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn args(dev: bool, release: bool) -> RunArgs {
        RunArgs {
            project: ".".into(),
            release,
            dev,
            level: None,
            json: false,
            game_args: vec!["--exit-after-frames".into(), "5".into()],
        }
    }

    #[test]
    fn dev_runs_enable_the_dev_assets_feature_and_watch() {
        let cmd = cargo_command(Path::new("/p"), "demo_launcher", &args(true, false), None);
        let line = crate::process::command_line(&cmd);
        // The manifest path goes to cargo as the host's native path.
        let manifest = Path::new("/p").join("Cargo.toml");
        assert_eq!(
            line,
            format!(
                "cargo run --manifest-path {} -p demo_launcher --features regia/dev-assets -- --exit-after-frames 5",
                manifest.display()
            )
        );
        let watch = cmd
            .get_envs()
            .find(|(k, _)| *k == "REGIA_ASSET_WATCH")
            .and_then(|(_, v)| v);
        assert_eq!(watch, Some(std::ffi::OsStr::new("1")));
    }

    #[test]
    fn plain_runs_keep_the_watch_off() {
        let cmd = cargo_command(Path::new("/p"), "demo_launcher", &args(false, true), None);
        let line = crate::process::command_line(&cmd);
        assert!(
            line.contains("--release") && !line.contains("dev-assets"),
            "{line}"
        );
        let watch = cmd
            .get_envs()
            .find(|(k, _)| *k == "REGIA_ASSET_WATCH")
            .and_then(|(_, v)| v);
        assert_eq!(watch, Some(std::ffi::OsStr::new("0")));
    }

    #[test]
    fn level_resolves_under_the_project_or_assets_and_reaches_the_game() {
        // `unique_temp_dir` clears and uniques the folder: a crashed earlier run under a reused
        // pid is never inherited (sprint-deep-zai-cli-tools finding 5).
        let dir = regia_test_utils::unique_temp_dir("run_level");
        std::fs::create_dir_all(dir.join("assets/levels")).expect("dir");
        let file = dir.join("assets/levels/test.level.ron");
        std::fs::write(&file, "(format_version: 1, entities: [])").expect("write");
        let file = file.canonicalize().expect("canonical");
        for given in ["levels/test.level.ron", "assets/levels/test.level.ron"] {
            assert_eq!(resolve_level(&dir, Path::new(given)).expect(given), file);
        }
        assert!(resolve_level(&dir, Path::new("levels/missing.level.ron")).is_err());
        assert!(resolve_level(&dir, Path::new("levels/test.ron")).is_err());

        let cmd = cargo_command(&dir, "demo_launcher", &args(true, false), Some(&file));
        let env = cmd
            .get_envs()
            .find(|(k, _)| *k == START_LEVEL_ENV)
            .and_then(|(_, v)| v);
        assert_eq!(env, Some(file.as_os_str()));
        let plain = cargo_command(&dir, "demo_launcher", &args(true, false), None);
        assert_eq!(
            plain.get_envs().find(|(k, _)| *k == START_LEVEL_ENV),
            Some((std::ffi::OsStr::new(START_LEVEL_ENV), None)),
            "an inherited start level is cleared"
        );
        if let Some((legacy, true)) = regia_core::brand::alternate_env_name(START_LEVEL_ENV) {
            assert_eq!(
                plain.get_envs().find(|(k, _)| *k == legacy.as_str()),
                Some((std::ffi::OsStr::new(&legacy), None)),
                "a start level inherited under the legacy name is cleared too"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `regia run --json`'s object names the launcher binary as `artifact` and the exit status
    /// (ADR-7217).
    #[test]
    fn the_json_result_names_the_artifact_and_the_exit_status() {
        let target = Path::new("/p/target");
        let artifact = launcher_binary(target, "demo_launcher", true);
        assert_eq!(
            artifact,
            target
                .join("release")
                .join(format!("demo_launcher{}", std::env::consts::EXE_SUFFIX))
        );
        let level = Path::new("/p/assets/levels/a.level.ron");
        let run = RunSummary {
            artifact: artifact.clone(),
            package: "demo_launcher",
            release: true,
            level: Some(level),
            cooked: 3,
            cook_skipped: 1,
        };
        let ok = json_result(&run, Some(0));
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["artifact"], artifact.display().to_string());
        assert_eq!(ok["artifact_exists"], false);
        assert_eq!(ok["exit_code"], 0);
        assert_eq!(ok["profile"], "release");
        assert_eq!(ok["level"], level.display().to_string());
        assert_eq!(
            (ok["cooked"].as_u64(), ok["cook_skipped"].as_u64()),
            (Some(3), Some(1))
        );
        let failed = json_result(&run, Some(101));
        assert_eq!(
            (failed["ok"].as_bool(), failed["exit_code"].as_i64()),
            (Some(false), Some(101))
        );
        assert_eq!(json_result(&run, None)["exit_code"], serde_json::Value::Null);
        assert!(!ok.to_string().contains('\n'), "one line");
    }

    #[test]
    fn launcher_package_is_read_from_the_manifest() {
        let dir = regia_test_utils::unique_temp_dir("run_pkg");
        std::fs::create_dir_all(dir.join("launcher")).expect("dir");
        std::fs::write(
            dir.join("launcher/Cargo.toml"),
            "[package]\nname = \"demo_launcher\"\nversion = \"0.1.0\"\n",
        )
        .expect("write");
        assert_eq!(launcher_package(&dir).expect("name"), "demo_launcher");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_feature_follows_the_engine_dependency_key() {
        let main = regia_core::brand::MAIN_CRATE;
        let legacy = regia_core::brand::LEGACY_MAIN_CRATE;
        let dir = regia_test_utils::unique_temp_dir("run-engine-key");
        std::fs::create_dir_all(dir.join("launcher")).expect("dir");
        let write = |deps: &str| {
            std::fs::write(
                dir.join("launcher/Cargo.toml"),
                format!("[package]\nname = \"demo_launcher\"\n\n[dependencies]\n{deps}"),
            )
            .expect("write");
        };
        write(&format!("{main} = {{ path = \"../engine/crates/{main}\" }}\n"));
        assert_eq!(engine_dependency_key(&dir), main);
        // A project from before the rename that kept its dependency name.
        write(&format!(
            "{legacy} = {{ package = \"{main}\", path = \"../engine/crates/{main}\" }}\n"
        ));
        assert_eq!(engine_dependency_key(&dir), legacy);
        let cmd = cargo_command(&dir, "demo_launcher", &args(true, false), None);
        let line = crate::process::command_line(&cmd);
        assert!(
            line.contains(&format!("--features {legacy}/dev-assets")),
            "{line}"
        );
        assert_eq!(engine_dependency_key(Path::new("/no/such/project")), main);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
