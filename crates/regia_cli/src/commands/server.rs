// SPDX-License-Identifier: MPL-2.0
//! `regia server`: the dedicated server's dev loop (`docs/specs/76-dedicated-server-package.md`
//! § 10). Cooks, builds the game with the `server` feature in the dev profile and runs it from the
//! project's dev asset cache, no pak. Its server directory is `build/server-dev/`: a `server.toml`
//! kept between runs, `bans.ron`, `logs/`, and a `content/net-manifest.ron` computed from the cache.
//!
//! `--clients N` also builds the client (`--features regia/net`) and starts N of them offscreen,
//! joined to the server on loopback, each told where through `REGIA_CONNECT`. `--agent` starts the
//! agent protocol server in the server process; that dev build links the client crates (ADR-1886).
//!
//! A project split into `server/` and `client/` crates (the `net-game` template, ADR-2440) builds
//! `-p <server crate>` and `-p <client crate>` instead, each in its own cargo invocation. Every dev
//! server runs a relay inside itself (`--relay local:0`) and prints its join code; the clients get
//! it as `REGIA_JOIN_CODE` and `REGIA_RELAY` and join by code.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};

use anyhow::{Context, Result, bail};
use clap::Args;
use regia_asset_pipeline::dev_server::{self, ClientSpec, ServerSpec};
use regia_asset_pipeline::{CookOptions, cook_project};
use regia_net::server_host::CliOverrides;
use regia_net::server_host::config::resolve_config;

use crate::commands::build_profile;
use crate::commands::package::{Profile, cargo_package_name, cargo_workspace_root, find_binary};
use crate::commands::run::resolve_level;
use crate::commands::server_content;
use crate::commands::server_package::{check_server_feature, server_toml_text};
use crate::manifest::Manifest;

/// Most clients one `regia server` starts.
const MAX_CLIENTS: u32 = 64;

/// How long `regia server` waits for the server's join code before it starts the clients with
/// the address alone.
const JOIN_CODE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// A crate of a split project (`server/` or `client/`): its package and binary names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SplitCrate {
    package: String,
    bin: String,
}

/// The `[package]` name and first `[[bin]]` name (else the package name) of a manifest text.
fn split_crate_of(text: &str) -> Option<SplitCrate> {
    let doc: toml::Value = toml::from_str(text).ok()?;
    let package = doc.get("package")?.get("name")?.as_str()?.to_owned();
    let bin = doc
        .get("bin")
        .and_then(toml::Value::as_array)
        .and_then(|bins| bins.first())
        .and_then(|b| b.get("name"))
        .and_then(toml::Value::as_str)
        .map_or_else(|| package.clone(), str::to_owned);
    Some(SplitCrate { package, bin })
}

/// The `server/` and `client/` crates of a split project; `None` unless `server/Cargo.toml` is a
/// package.
fn split_crates(project: &Path) -> Option<(SplitCrate, Option<SplitCrate>)> {
    let read = |dir: &str| std::fs::read_to_string(project.join(dir).join("Cargo.toml")).ok();
    let server = split_crate_of(&read("server")?)?;
    let client = read("client").as_deref().and_then(split_crate_of);
    Some((server, client))
}

/// The cargo package and binary of the project's dedicated server: the `server/` crate of a split
/// project (ADR-2440), else the root (or `launcher/`) package.
///
/// # Errors
///
/// Neither a `server/` crate nor a root or `launcher/` package.
pub(crate) fn server_cargo_target(project: &Path) -> Result<crate::commands::package::CargoTarget> {
    match split_crates(project) {
        Some((server, _)) => Ok(crate::commands::package::CargoTarget {
            package: server.package,
            bin: server.bin,
        }),
        None => cargo_package_name(project),
    }
}

/// The `join code <code> relay <addr>` line the host prints (ADR-2440).
fn parse_join_line(line: &str) -> Option<(String, String)> {
    dev_server::parse_join_line(line)
}

/// Build and run the game's dedicated server from the dev cache.
#[derive(Debug, Args)]
pub struct ServerArgs {
    /// Project directory (contains `Regia.toml`).
    #[arg(long, default_value = ".")]
    pub project: PathBuf,
    /// Serve this level (`.level.ron`, relative to the project or to its `assets/`); replaces the
    /// rotation.
    #[arg(long)]
    pub level: Option<PathBuf>,
    /// UDP game port.
    #[arg(long)]
    pub port: Option<u16>,
    /// Also start this many offscreen clients that join on loopback.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=64))]
    pub clients: u32,
    /// Start the agent protocol server in the server process (dev builds only).
    #[arg(long, default_value_t = false)]
    pub agent: bool,
    /// A server tool instead of the dev loop (`regia server results`).
    #[command(subcommand)]
    pub action: Option<crate::commands::server_results::ServerAction>,
}

/// The dev server directory of `project`.
#[must_use]
pub fn dev_server_dir(project: &Path) -> PathBuf {
    project.join("build").join("server-dev")
}

/// The path of `file` under `assets/`, as a rotation names it.
fn level_under_assets(project: &Path, file: &Path) -> Option<String> {
    let root = project.join("assets").canonicalize().ok()?;
    let rel = file.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// The port the dev server will bind: `--port`, the environment, then `server.toml`.
fn resolved_port(dir: &Path, port: Option<u16>) -> Result<u32> {
    let cli = CliOverrides {
        port: port.map(|p| p.to_string()),
        ..CliOverrides::default()
    };
    let resolved =
        resolve_config(dir, &cli, &|key| regia_core::brand::env_var(key).ok()).map_err(|errors| {
            let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
            anyhow::anyhow!("{}", lines.join("\n"))
        })?;
    Ok(resolved.settings.port)
}

/// The arguments of the dev server's `cargo build`.
fn server_build_args(package: &str, engine: &str, agent: bool) -> Vec<String> {
    let mut features = "server".to_string();
    if agent {
        features.push_str(&format!(",{engine}/agent"));
    }
    vec![
        "build".into(),
        "-p".into(),
        package.into(),
        "--no-default-features".into(),
        "--features".into(),
        features,
    ]
}

/// The `cargo build` of a split project's crate: the crate is the mode (ADR-2440), so no feature
/// switch; `--agent` adds the engine's `agent` feature.
fn split_build_args(package: &str, engine: &str, agent: bool) -> Vec<String> {
    let mut out = vec!["build".into(), "-p".into(), package.into()];
    if agent {
        out.extend(["--features".into(), format!("{engine}/agent")]);
    }
    out
}

/// Copies the server's stdout to this process's, and sends the first join code line on `join`.
fn forward_server_stdout(out: impl std::io::Read, join: &std::sync::mpsc::Sender<(String, String)>) {
    use std::io::{BufRead, Write};
    let mut sent = false;
    for line in std::io::BufReader::new(out).lines() {
        let Ok(line) = line else { break };
        if !sent && let Some(found) = parse_join_line(&line) {
            sent = join.send(found).is_ok();
        }
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

/// The arguments of the dev clients' `cargo build`.
fn client_build_args(package: &str, engine: &str) -> Vec<String> {
    vec![
        "build".into(),
        "-p".into(),
        package.into(),
        "--features".into(),
        format!("{engine}/net"),
    ]
}

/// The server's own arguments: always a relay inside the dev server, so it prints a join code.
#[cfg(test)]
fn server_process_args(args: &ServerArgs, level: Option<&str>) -> Vec<String> {
    dev_server::server_args(level, args.port, args.agent)
}

/// A client process: offscreen, audio off, joined to `addr` (or by `join`: the code and the relay),
/// with the server's manifest.
fn client_command(
    binary: &Path,
    project: &Path,
    addr: &str,
    join: Option<&(String, String)>,
    index: u32,
    manifest: &Path,
) -> Command {
    dev_server::client_command(&ClientSpec {
        binary,
        project,
        addr,
        join,
        index,
        manifest,
    })
}

/// Runs `cargo` with `args` in `workspace`.
fn cargo(workspace: &Path, args: &[String]) -> Result<()> {
    crate::process::run(
        Command::new("cargo").args(args).current_dir(workspace),
        "install Rust with cargo (https://rustup.rs) and run `regia doctor`",
    )
}

/// Copies the built binary out of cargo's way: the client build overwrites that path. Returns the
/// path to run, which is `dest` unless a run left over from a killed `regia server` still holds it
/// (see `place_binary`).
fn keep_binary(project: &Path, bin: &str, dest: &Path) -> Result<PathBuf> {
    let built = find_binary(project, bin, Profile::Dev, cfg!(windows), None)?;
    let tmp = dest.with_extension("tmp");
    std::fs::copy(&built, &tmp).with_context(|| format!("copying {}", built.display()))?;
    place_binary(&tmp, dest, std::process::id(), |from: &Path, to: &Path| {
        std::fs::rename(from, to)
    })
}

/// Moves `tmp` over `dest`. A rename replaces a binary that is still running on Unix; Windows
/// locks a running executable and refuses (`PermissionDenied`), so then the binary goes to a
/// sibling named after this process (`unique_sibling`) and that path is returned.
fn place_binary(
    tmp: &Path,
    dest: &Path,
    pid: u32,
    rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<PathBuf> {
    match rename(tmp, dest) {
        Ok(()) => Ok(dest.to_path_buf()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            let unique = unique_sibling(dest, pid);
            rename(tmp, &unique).with_context(|| format!("replacing {}", unique.display()))?;
            eprintln!(
                "regia server: {} is in use (a server from an earlier run still running?); running {}",
                dest.display(),
                unique.display()
            );
            Ok(unique)
        }
        Err(err) => Err(err).with_context(|| format!("replacing {}", dest.display())),
    }
}

/// `dest` with `-<pid>` after its stem: `game-server.exe` → `game-server-42.exe`.
fn unique_sibling(dest: &Path, pid: u32) -> PathBuf {
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match dest.extension() {
        Some(ext) => format!("{stem}-{pid}.{}", ext.to_string_lossy()),
        None => format!("{stem}-{pid}"),
    };
    dest.with_file_name(name)
}

/// Stops the processes it holds when `regia server` ends, however it ends.
struct Processes(Vec<Child>);

impl Drop for Processes {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Waits for the server, passing a SIGTERM or SIGINT of this process on to it so it drains. A
/// terminal's Ctrl+C reaches the server by itself; a supervisor's SIGTERM reaches only this process.
#[cfg(unix)]
fn wait_forwarding(server: &mut Child) -> Result<ExitStatus> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, stop.clone()).context("registering the signal handler")?;
    }
    loop {
        if let Some(status) = server.try_wait().context("waiting for the server")? {
            return Ok(status);
        }
        if stop.swap(false, Ordering::SeqCst) {
            let _ = Command::new("kill")
                .args(["-TERM", &server.id().to_string()])
                .status();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Waits for the server; the console's control events reach it directly.
#[cfg(not(unix))]
fn wait_forwarding(server: &mut Child) -> Result<ExitStatus> {
    server.wait().context("waiting for the server")
}

/// Writes what the dev server reads: `server.toml` once, the manifest every run.
fn prepare_dev_dir(project: &Path, dir: &Path, manifest: &Manifest, platform: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let closure = build_profile::compute_closure(project, platform, None)?;
    server_content::write_net_manifest(project, platform, &manifest.project.version, &dir.join("content"))?;
    let toml = dir.join("server.toml");
    if !toml.exists() {
        let levels: Vec<String> = closure
            .raw
            .iter()
            .filter(|r| r.path.ends_with(".level.ron"))
            .map(|r| r.path.clone())
            .collect();
        std::fs::write(&toml, server_toml_text(&manifest.project.name, &levels))?;
    }
    Ok(())
}

/// Runs the command; returns when the server exits.
///
/// # Errors
///
/// Not a project, the cook or a build failed, or the server exited with a failure.
pub fn run(args: &ServerArgs) -> Result<()> {
    if let Some(action) = &args.action {
        return crate::commands::server_results::run(&args.project, action);
    }
    let project = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    if !regia_core::brand::is_project_root(&project) {
        bail!(
            "{} is not a Regia Engine project (no Regia.toml)",
            project.display()
        );
    }
    if args.clients > MAX_CLIENTS {
        bail!("--clients {}: at most {MAX_CLIENTS}", args.clients);
    }
    let manifest = Manifest::load(&project)?;
    let split = split_crates(&project);
    let target = server_cargo_target(&project)?;
    if split.is_none() {
        check_server_feature(&project, &target.package)?;
    }
    let level = args
        .level
        .as_deref()
        .map(|level| resolve_level(&project, level))
        .transpose()?;
    let level = match &level {
        Some(file) => Some(
            level_under_assets(&project, file)
                .with_context(|| format!("--level {}: not under the project's assets/", file.display()))?,
        ),
        None => None,
    };

    let report = cook_project(CookOptions::for_host(project.clone())).context("cook")?;
    crate::commands::cook::report_cook_outcome(&report)?;
    let platform = regia_asset::current_platform();
    let dir = dev_server_dir(&project);
    prepare_dev_dir(&project, &dir, &manifest, &platform)?;

    let workspace = cargo_workspace_root(&project)?;
    let engine = crate::commands::run::engine_dependency_key_of(&project);
    let server_build = match &split {
        Some(_) => split_build_args(&target.package, &engine, args.agent),
        None => server_build_args(&target.package, &engine, args.agent),
    };
    cargo(&workspace, &server_build)?;
    // Not next to `content/`: a binary beside that directory reads paks, and this one the dev cache.
    let bin_dir = dir.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let server_bin = bin_dir.join(format!("{}-server{}", target.bin, std::env::consts::EXE_SUFFIX));
    let server_bin = keep_binary(&project, &target.bin, &server_bin)?;

    let mut client_bin = None;
    if args.clients > 0 {
        let (package, bin_name) = match split.as_ref().and_then(|(_, c)| c.clone()) {
            Some(client) => (client.package, client.bin),
            None if split.is_some() => {
                bail!("--clients: this project has a server/ crate but no client/ crate")
            }
            None => (target.package.clone(), target.bin.clone()),
        };
        let build = if split.is_some() {
            split_build_args(&package, &engine, false)
        } else {
            client_build_args(&package, &engine)
        };
        cargo(&workspace, &build)?;
        let bin = bin_dir.join(format!("{}-client{}", target.bin, std::env::consts::EXE_SUFFIX));
        client_bin = Some(keep_binary(&project, &bin_name, &bin)?);
    }

    // The same command play-in-editor's separate processes start (ADR-2682).
    let mut server = dev_server::server_command(&ServerSpec {
        binary: &server_bin,
        project: &project,
        server_dir: &dir,
        level: level.as_deref(),
        port: args.port,
        agent: args.agent,
    });
    eprintln!(
        "regia server: running {} (server directory {})",
        server_bin.display(),
        dir.display()
    );
    let port = resolved_port(&dir, args.port)?;
    let mut server = server
        .stdout(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", server_bin.display()))?;
    // The server's stdout passes through; its join code line is also handed to the clients.
    let (join_tx, join_rx) = std::sync::mpsc::channel();
    if let Some(out) = server.stdout.take() {
        let forward = std::thread::Builder::new()
            .name("server-stdout".into())
            .spawn(move || forward_server_stdout(out, &join_tx));
        if let Err(err) = forward {
            eprintln!("regia server: cannot read the server's output: {err}");
        }
    }
    // Held until the server ended: clients stop first, and a failed client start stops the server.
    let mut clients = Processes(Vec::new());
    if let Some(bin) = &client_bin {
        let addr = format!("127.0.0.1:{port}");
        let manifest_file = dir.join("content/net-manifest.ron");
        let join = join_rx.recv_timeout(JOIN_CODE_WAIT).ok();
        if join.is_none() {
            eprintln!("regia server: no join code from the server; the clients join by address");
        }
        for index in 0..args.clients {
            match client_command(bin, &project, &addr, join.as_ref(), index, &manifest_file).spawn() {
                Ok(child) => clients.0.push(child),
                Err(err) => {
                    clients.0.push(server);
                    return Err(err).with_context(|| format!("starting {}", bin.display()));
                }
            }
        }
    }
    let status = wait_forwarding(&mut server)?;
    drop(clients);
    if !status.success() {
        bail!("the server exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CLI1 platform audit 2: Windows refuses to replace a running executable, so a server left
    /// over from a killed `regia server` must not stop the next run at the copy.
    #[test]
    fn keep_binary_falls_back_to_a_unique_name_when_replace_is_refused() {
        let dir = regia_test_utils::unique_temp_dir("server_keep_binary");
        let dest = dir.join("game-server.exe");
        std::fs::write(&dest, b"old, still running").expect("old");
        let tmp = dest.with_extension("tmp");
        std::fs::write(&tmp, b"new build").expect("tmp");
        let locked = dest.clone();
        let refuse_locked = |from: &Path, to: &Path| {
            if to == locked {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else {
                std::fs::rename(from, to)
            }
        };
        let placed = place_binary(&tmp, &dest, 4242, refuse_locked).expect("placed");
        assert_eq!(placed, dir.join("game-server-4242.exe"));
        assert_eq!(std::fs::read(&placed).expect("new"), b"new build");
        assert_eq!(std::fs::read(&dest).expect("old"), b"old, still running");
        assert!(!tmp.exists());

        // Without the lock the copy replaces the old binary in place.
        std::fs::write(&tmp, b"newer").expect("tmp");
        let placed = place_binary(&tmp, &dest, 4242, |from: &Path, to: &Path| {
            std::fs::rename(from, to)
        })
        .expect("replaced");
        assert_eq!(placed, dest);
        assert_eq!(std::fs::read(&dest).expect("newer"), b"newer");

        // Any other failure is still an error.
        std::fs::write(&tmp, b"x").expect("tmp");
        let broken = |_: &Path, _: &Path| Err(std::io::Error::other("disk gone"));
        assert!(place_binary(&tmp, &dest, 1, broken).is_err());
        // An extensionless (Unix) name gets the suffix at the end.
        assert_eq!(
            unique_sibling(&dir.join("game-server"), 7),
            dir.join("game-server-7")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    fn args(level: Option<&str>, port: Option<u16>, agent: bool) -> ServerArgs {
        ServerArgs {
            project: ".".into(),
            level: level.map(Into::into),
            port,
            clients: 0,
            agent,
            action: None,
        }
    }

    #[test]
    fn the_server_build_has_no_default_features() {
        let line = server_build_args("demo", "regia", false).join(" ");
        assert_eq!(line, "build -p demo --no-default-features --features server");
        let line = server_build_args("demo", "regia", true).join(" ");
        assert_eq!(
            line,
            "build -p demo --no-default-features --features server,regia/agent"
        );
    }

    #[test]
    fn the_client_build_keeps_the_game_defaults_and_adds_net() {
        let line = client_build_args("demo", "engine").join(" ");
        assert_eq!(line, "build -p demo --features engine/net");
    }

    #[test]
    fn server_flags_reach_the_process() {
        assert_eq!(
            server_process_args(&args(None, None, false), None),
            ["--relay", "local:0"]
        );
        assert_eq!(
            server_process_args(&args(None, Some(7000), true), Some("levels/a.level.ron")),
            [
                "--relay",
                "local:0",
                "--level",
                "levels/a.level.ron",
                "--port",
                "7000",
                "--agent"
            ]
        );
    }

    #[test]
    fn clients_are_told_where_to_join() {
        let join = ("ABCDEF".to_owned(), "127.0.0.1:40000".to_owned());
        let cmd = client_command(
            Path::new("/b/client"),
            Path::new("/p"),
            "127.0.0.1:7000",
            Some(&join),
            3,
            Path::new("/d/content/net-manifest.ron"),
        );
        let env = |key: &str| {
            cmd.get_envs()
                .find(|(k, _)| *k == key)
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().into_owned())
        };
        assert_eq!(env("REGIA_CONNECT").as_deref(), Some("127.0.0.1:7000"));
        assert_eq!(env("REGIA_CLIENT_NAME").as_deref(), Some("client3"));
        assert_eq!(env("REGIA_CLIENT_OFFSCREEN").as_deref(), Some("1"));
        assert_eq!(env("REGIA_AUDIO").as_deref(), Some("off"));
        assert_eq!(env("REGIA_JOIN_CODE").as_deref(), Some("ABCDEF"));
        assert_eq!(env("REGIA_RELAY").as_deref(), Some("127.0.0.1:40000"));
    }

    /// ADR-2440: a project with `server/` and `client/` crates (the `net-game` template) builds
    /// each crate on its own, with no feature switch.
    #[test]
    fn a_split_project_builds_its_server_and_client_crates() {
        let dir = regia_test_utils::unique_temp_dir("server-split");
        std::fs::create_dir_all(dir.join("server")).expect("dir");
        std::fs::create_dir_all(dir.join("client")).expect("dir");
        assert_eq!(
            split_crates(&dir),
            None,
            "no server crate: the single-package flow"
        );
        std::fs::write(
            dir.join("server/Cargo.toml"),
            "[package]\nname = \"arena_server\"\nversion = \"0.1.0\"\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("client/Cargo.toml"),
            "[package]\nname = \"arena_client\"\nversion = \"0.1.0\"\n[[bin]]\nname = \"arena\"\npath = \"src/main.rs\"\n",
        )
        .expect("write");
        let (server, client) = split_crates(&dir).expect("split");
        assert_eq!(server.package, "arena_server");
        assert_eq!(server.bin, "arena_server");
        let client = client.expect("client");
        assert_eq!(
            (client.package.as_str(), client.bin.as_str()),
            ("arena_client", "arena")
        );
        assert_eq!(
            split_build_args("arena_server", "regia", false).join(" "),
            "build -p arena_server"
        );
        assert_eq!(
            split_build_args("arena_server", "regia", true).join(" "),
            "build -p arena_server --features regia/agent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_join_code_line_is_read_back() {
        assert_eq!(
            parse_join_line("join code ABCDEF relay 127.0.0.1:40000\n"),
            Some(("ABCDEF".to_owned(), "127.0.0.1:40000".to_owned()))
        );
        assert_eq!(parse_join_line("listening 0.0.0.0:7777 key abc"), None);
        assert_eq!(parse_join_line("join code ABCDEF"), None);
    }

    #[test]
    fn clients_follow_the_port_the_server_binds() {
        let dir = regia_test_utils::unique_temp_dir("server-port");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("server.toml"),
            "format_version = 1\n[server]\nport = 7123\n[levels]\nrotation = [\"a.level.ron\"]\n",
        )
        .expect("write");
        if regia_core::brand::env_var_os("REGIA_SERVER_PORT").is_none() {
            assert_eq!(resolved_port(&dir, None).expect("port"), 7123);
        }
        assert_eq!(resolved_port(&dir, Some(7200)).expect("port"), 7200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_level_is_named_as_the_rotation_names_it() {
        let dir = regia_test_utils::unique_temp_dir("server-level");
        std::fs::create_dir_all(dir.join("assets/levels")).expect("dir");
        let file = dir.join("assets/levels/a.level.ron");
        std::fs::write(&file, "(format_version: 1, entities: [])").expect("write");
        let dir = dir.canonicalize().expect("canonical");
        let file = dir.join("assets/levels/a.level.ron");
        assert_eq!(
            level_under_assets(&dir, &file).as_deref(),
            Some("levels/a.level.ron")
        );
        assert_eq!(
            level_under_assets(&dir, Path::new("/elsewhere/a.level.ron")),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_dev_directory_gets_a_config_once_and_a_manifest_every_run() {
        let dir = regia_test_utils::unique_temp_dir("server-dev");
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
        let manifest = Manifest::load(&dir).expect("manifest");
        let report = cook_project(CookOptions {
            project_root: dir.clone(),
            platform: "linux".into(),
            incremental: true,
        })
        .expect("cook");
        crate::commands::cook::report_cook_outcome(&report).expect("cook outcome");
        let server = dev_server_dir(&dir);
        prepare_dev_dir(&dir, &server, &manifest, "linux").expect("first");
        let toml = std::fs::read_to_string(server.join("server.toml")).expect("toml");
        assert!(toml.contains("levels/a.level.ron"), "{toml}");
        let manifest_text =
            std::fs::read_to_string(server.join("content/net-manifest.ron")).expect("manifest");
        std::fs::write(server.join("server.toml"), "format_version = 1\n# mine\n").expect("edit");
        std::fs::write(
            dir.join("assets/levels/a.level.ron"),
            "(format_version: 1, entities: [ ])",
        )
        .expect("edit level");
        prepare_dev_dir(&dir, &server, &manifest, "linux").expect("second");
        assert!(
            std::fs::read_to_string(server.join("server.toml"))
                .expect("toml")
                .contains("# mine"),
            "an edited server.toml stays"
        );
        assert_ne!(
            std::fs::read_to_string(server.join("content/net-manifest.ron")).expect("manifest"),
            manifest_text,
            "the manifest follows the level"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
