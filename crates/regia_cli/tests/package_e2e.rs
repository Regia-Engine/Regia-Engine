// SPDX-License-Identifier: MPL-2.0
//! M16 round-3 end-to-end packaging test (specs/21 § 6, minus Steam).
//!
//! Builds a throwaway project with one cookable mesh,
//! packages it for Linux (`regia package --platform linux --format tar`),
//! extracts the tarball, runs the packaged binary with `--exit-after-frames`
//! and `REGIA_PROBE_ASSET`, and asserts the asset is served **from the pak** —
//! then deletes `content/*.fpak` and asserts the load fails instead of
//! silently falling back to a dev cooked cache.
//!
//! Ignored by default (full engine build + windowed run, > 60 s); run with
//! `cargo test -p regia_cli --test package_e2e -- --ignored`.
//!
//! The nested builds are the cost: the `shipping` package of `samples/arena` is a fat-LTO link of
//! the whole engine, 20 minutes or more of one core. A runner with a per-binary limit makes them in
//! its build phase (`tools/gaterig/package-prebuild.sh`) and sets `REGIA_PACKAGE_E2E_PREBUILT=1`:
//! the tests then refuse to start a build of their own (ADR-5719).

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use regia_test_utils::process::{PrivateDisplay, Running, output_within, wait_within};

/// Root of this repo (the test lives in `crates/regia_cli/tests`).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// The `regia` CLI binary (cargo builds it for this test).
fn regia_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_regia"))
}

/// The target dir of the dev builds these tests start, inside this test's own target dir (cached
/// between runs, as `panic-abort` is for `warm_quit_abort`).
///
/// In the target dir they would take the `debug` build lock that every other cargo a test binary
/// starts takes too, and wait for it with nothing on the log. The shipping build keeps the
/// default: its directory is `shipping`, which only this test uses, and it has the dependencies a
/// new target dir would have to build again.
///
/// `tools/gaterig/package-prebuild.sh` builds into this same directory (the target dir cargo built
/// this binary into, symlinks resolved); `regia_cli`'s unit test
/// `package_prebuild_script_issues_the_cli_builds` holds the two together.
fn dev_target_dir() -> PathBuf {
    // <target>/debug/deps/package_e2e-<hash> -> <target>/package-e2e
    std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .expect("test binary path")
        .ancestors()
        .nth(3)
        .expect("the test binary is under a target dir")
        .join("package-e2e")
}

/// A `regia package` call (run it with [`run_package`]).
fn regia_package() -> Command {
    let mut command = Command::new(regia_cli());
    command.arg("package");
    command
}

/// `REGIA_PACKAGE_E2E_PREBUILT=1`: the runner made the nested builds before the tests started
/// (`tools/gaterig/package-prebuild.sh` in the merge gate's build phase, ADR-5719), so a package
/// here finds them fresh. A build that starts anyway is the runner's step and `regia package`
/// disagreeing about flags or the build identity; it would be the 20-minute link again, inside a
/// test with a limit, so the test stops it and fails with the crate cargo began with.
fn prebuilt() -> bool {
    regia_core::brand::env_var("REGIA_PACKAGE_E2E_PREBUILT").is_ok_and(|v| v.trim() == "1")
}

/// The crate of a cargo `Compiling <crate> v<version> (<path>)` progress line.
fn compiling(line: &str) -> Option<&str> {
    line.trim_start()
        .strip_prefix("Compiling ")?
        .split_whitespace()
        .next()
}

/// How a `regia package` run ended.
#[derive(Debug)]
enum Packaged {
    /// It ran to its end.
    Done {
        /// Its exit status.
        status: ExitStatus,
        /// What it printed (the last line names the package).
        stdout: String,
        /// Cargo's progress and the CLI's log (its stderr).
        progress: String,
    },
    /// Cargo started compiling `krate` in a build that was to be found fresh; the run was stopped.
    Rebuilt {
        /// The first crate cargo compiled.
        krate: String,
        /// What the run printed until then.
        stdout: String,
        /// Cargo's progress until then.
        progress: String,
        /// Why the run's processes could not all be stopped (`None`: the stop was delivered).
        unstopped: Option<String>,
    },
}

/// Gives a run that may be refused a process group of its own (Unix; its id is the run's pid), so
/// [`stop_tree`] reaches cargo and its `rustc` jobs with one signal. Elsewhere this does nothing.
fn own_group(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    command
}

/// Stops `root`, a run started with [`own_group`], and every process it started. A killed cargo
/// otherwise leaves its `rustc` jobs running, and the fat-LTO one runs for 20 minutes holding the
/// run's log pipe, so the refused run would only end with it.
///
/// - Unix: one `SIGKILL` to the run's process group, through the `kill` builtin of `/bin/sh` (no
///   `kill` binary on `PATH` needed, no `/proc` walk). The kernel delivers a group signal to every
///   member, a child forked meanwhile included. `root` is not reaped yet, so the group id cannot
///   name another group.
/// - Windows: `taskkill /T /F` on `root` (`%SystemRoot%\System32`), which ends the tree by parent
///   ids while `root` is alive. Its exit status is not the stop's result there: a member that
///   ends by itself while the tree is killed (cargo when its `rustc` dies, the stand-in when its
///   job does) is reported as "no running instance" with a non-zero status (seen on Windows 11,
///   2026-11-11). Only a `taskkill` that cannot run is an error; the caller's bounded wait for
///   the run's pipes is the check that every member is gone. A process started between the
///   listing and its kill can survive; that wait then names it.
///
/// Returns why the stop failed (the shell or `taskkill` could not run; on Unix, the signal was
/// refused).
#[cfg(any(unix, windows))]
fn stop_tree(root: u32) -> Result<(), String> {
    let output = stop_command(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("cannot run the stop of process {root}'s tree: {e}"))?;
    if output.status.success() || cfg!(windows) {
        Ok(())
    } else {
        Err(format!(
            "the stop of process {root}'s tree failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// [`stop_tree`] where neither applies: nothing can be stopped.
#[cfg(not(any(unix, windows)))]
fn stop_tree(root: u32) -> Result<(), String> {
    Err(format!("no way to stop process {root}'s tree on this platform"))
}

/// The command [`stop_tree`] runs on Unix.
#[cfg(unix)]
fn stop_command(root: u32) -> Command {
    let mut command = Command::new("/bin/sh");
    // dash and macOS's sh need `--` before a negative id (else it reads as an option); busybox's
    // builtin refuses `--`, so it takes the second form. bash takes both. Checked 2026-11-11.
    command.args([
        "-c",
        r#"kill -s KILL -- "-$1" 2>/dev/null || kill -s KILL "-$1""#,
        "stop_tree",
        &root.to_string(),
    ]);
    command
}

/// The command [`stop_tree`] runs on Windows.
#[cfg(windows)]
fn stop_command(root: u32) -> Command {
    let taskkill = std::env::var_os("SystemRoot")
        .map(|system| PathBuf::from(system).join("System32").join("taskkill.exe"))
        .unwrap_or_else(|| PathBuf::from("taskkill"));
    let mut command = Command::new(taskkill);
    command.args(["/T", "/F", "/PID", &root.to_string()]);
    command
}

/// Runs a `regia package` call to its end and returns what it wrote.
///
/// A package that may build (`must_be_fresh` false) writes cargo's progress on this process's own
/// stderr: a run stopped from outside then shows what it was doing (a lock it waited for, the link
/// it was in), and the build it had started is not cut off with the test (a runner that stops this
/// binary at a limit and runs it again finds the build done; the merge gate relied on that until its
/// build step, ADR-5719). Its log is not kept.
///
/// With `must_be_fresh` the log comes through this process, line by line as it is written (past the
/// harness's capture), and the first `Compiling` line ends the run ([`Packaged::Rebuilt`]): the CLI,
/// cargo and its `rustc` jobs are stopped ([`stop_tree`], on every platform), and this returns once
/// the last of them has let go of the run's output pipes.
fn run_package(command: &mut Command, must_be_fresh: bool) -> Packaged {
    let log = if must_be_fresh {
        own_group(command);
        Stdio::piped()
    } else {
        Stdio::inherit()
    };
    let mut child = Running(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .expect("run regia package"),
    );
    let mut printed = child.0.stdout.take().expect("the stdout pipe");
    let stdout = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = printed.read_to_string(&mut text);
        text
    });
    // The first crate cargo compiles, sent once; the channel closes when the run's log does.
    let (rebuilt, first_rebuilt) = std::sync::mpsc::channel::<String>();
    let progress = child.0.stderr.take().map(|log| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let mut rebuilt = Some(rebuilt);
            for line in BufReader::new(log).lines().map_while(Result::ok) {
                let _ = writeln!(std::io::stderr(), "{line}");
                if let Some(krate) = compiling(&line)
                    && let Some(rebuilt) = rebuilt.take()
                {
                    let _ = rebuilt.send(krate.to_string());
                }
                text.push_str(&line);
                text.push('\n');
            }
            text
        })
    });
    // Without a log to read the sender is gone already, and this returns at once.
    let rebuilt = first_rebuilt.recv().ok();
    let unstopped = if rebuilt.is_some() {
        let stopped = stop_tree(child.0.id());
        let _ = child.0.kill();
        stopped.err()
    } else {
        None
    };
    let status = child.0.wait().expect("wait for regia package");
    let stdout = stdout.join().unwrap_or_default();
    let progress = progress.and_then(|log| log.join().ok()).unwrap_or_default();
    match rebuilt {
        Some(krate) => Packaged::Rebuilt {
            krate,
            stdout,
            progress,
            unstopped,
        },
        None => Packaged::Done {
            status,
            stdout,
            progress,
        },
    }
}

/// [`run_package`] for a test: the package must succeed. Returns what the CLI printed and its log.
/// `what` names the build for the failure text (`the shipping build of arena`).
fn package(command: &mut Command, must_be_fresh: bool, what: &str) -> (String, String) {
    match run_package(command, must_be_fresh) {
        Packaged::Done {
            status,
            stdout,
            progress,
        } => {
            assert!(status.success(), "regia package failed:\n{stdout}{progress}");
            (stdout, progress)
        }
        Packaged::Rebuilt {
            krate,
            stdout,
            progress,
            unstopped,
        } => panic!(
            "{what} was not found fresh: cargo started compiling `{krate}`. REGIA_PACKAGE_E2E_PREBUILT=1 says \
             tools/gaterig/package-prebuild.sh ran before the tests with this run's target dir, glibc knob and \
             REGIA_GIT_HASH; when it did not, unset the variable and the build runs here (20 minutes or more of \
             one core for the fat-LTO link).{} The run's log:\n{stdout}{progress}",
            unstopped.map_or_else(String::new, |why| format!(" The build may still run: {why}."))
        ),
    }
}

/// Best-effort: keep the packaged game's window off the interactive workspaces
/// (worker rule: windows go silently to workspace 5). No-op without Hyprland.
fn silence_windows() {
    let ok = Command::new("hyprctl")
        .args([
            "eval",
            "hl.window_rule({ name = \"regia-dev-ws5\", match = { class = \"^(regia\\\\..*)$\" }, workspace = \"5 silent\" })",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("package_e2e: hyprctl unavailable; the packaged game's window may be visible");
    }
}

/// Minimal GLB: one triangle with POSITION and TEXCOORD_0 (36 + 24 bytes BIN).
fn tiny_glb() -> Vec<u8> {
    let json = r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"mesh":0}],"meshes":[{"primitives":[{"attributes":{"POSITION":0,"TEXCOORD_0":1}}]}],"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},{"bufferView":1,"componentType":5126,"count":3,"type":"VEC2"}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":24}],"buffers":[{"byteLength":60}]}"#;
    let mut bin = Vec::new();
    for p in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
        bin.extend_from_slice(&p.to_le_bytes());
    }
    for uv in [0.0f32, 0.0, 1.0, 0.0, 0.5, 1.0] {
        bin.extend_from_slice(&uv.to_le_bytes());
    }
    assert_eq!(bin.len(), 60, "glb bin chunk size must match the bufferViews");

    let json_bytes = json.as_bytes();
    let json_pad = (4 - json_bytes.len() % 4) % 4;
    let bin_pad = (4 - bin.len() % 4) % 4;
    let json_len = json_bytes.len() + json_pad;
    let bin_len = bin.len() + bin_pad;
    let total = 12 + 8 + json_len + 8 + bin_len;
    let mut out = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_len as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(json_bytes);
    out.extend(std::iter::repeat_n(b' ', json_pad));
    out.extend_from_slice(&(bin_len as u32).to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin);
    out.extend(std::iter::repeat_n(0, bin_pad));
    out
}

/// The game binary source of the throwaway project. Mirrors `samples/sim_bridge/src/main.rs`:
/// `regia::run_with` + `--exit-after-frames N`.
const MAIN_RS: &str = r#"//! Generated by the M16 packaging end-to-end test.

use std::process::ExitCode;

use regia::prelude::*;

/// Exit after this many frames (`--exit-after-frames`, automated runs).
#[derive(Resource)]
struct ExitAfterFrames(u64);

fn flag(args: &[String], name: &str) -> Option<u64> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1)?.parse::<u64>().ok()
}

struct E2eProbe;

impl Plugin for E2eProbe {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, exit_after_frames);
    }
}

fn exit_after_frames(limit: Option<Res<ExitAfterFrames>>, time: Res<Time>, mut exit: MessageWriter<AppExit>) {
    if let Some(limit) = limit
        && time.frame_count() >= limit.0
    {
        exit.write(AppExit::Success);
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let exit_after = flag(&args, "--exit-after-frames");
    let exit = regia::run_with(E2eProbe, move |app| {
        app.insert_resource(WindowSettings {
            window: Window {
                title: "Regia Engine e2e probe".into(),
                ..Default::default()
            },
        });
        if let Some(n) = exit_after {
            app.insert_resource(ExitAfterFrames(n));
        }
    });
    match exit {
        AppExit::Success => ExitCode::SUCCESS,
        AppExit::Error(code) => ExitCode::from(code.get()),
    }
}
"#;

/// Removes a directory tree when dropped (even on assertion failure).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Longest wait for a packaged game that has stopped making progress, so a wedged game fails the
/// test with its log instead of holding the test binary. A bound on a hang, not on speed: it is
/// far above what a shipping build on lavapipe under a loaded gate needs.
const HANG_BOUND: Duration = Duration::from_secs(600);

/// A display for a packaged run: a private X server when `Xvfb` exists, else the ambient display
/// (`None`). A skipped run would mask a regression on CI (which has Xvfb), so with neither the
/// test fails instead of silently passing (specs/21 § 6 requires the run).
fn display_for_packaged_run() -> Option<PrivateDisplay> {
    let private = PrivateDisplay::start();
    assert!(
        private.is_some()
            || regia_core::brand::env_var_os("WAYLAND_DISPLAY").is_some()
            || regia_core::brand::env_var_os("DISPLAY").is_some(),
        "package_e2e: no display and no Xvfb; the packaged run cannot execute headlessly"
    );
    private
}

#[test]
#[ignore = "full engine build + windowed run; invoke with -- --ignored"]
fn packaged_linux_game_loads_asset_from_pak() {
    if regia_core::brand::env_var_os("WAYLAND_DISPLAY").is_none()
        && regia_core::brand::env_var_os("DISPLAY").is_none()
    {
        eprintln!("package_e2e: no display; skipping (the packaged game opens a window)");
        return;
    }
    silence_windows();

    let root = repo_root();
    // A throwaway project of its own workspace, outside the repo: as a workspace member it would
    // change the workspace, and rewrite Cargo.lock, under every other cargo running at the time
    // (`--locked` builds fail on it). The lockfile keeps it on the repo's dependency versions, and
    // the profiles match the repo's, which a workspace of its own does not inherit.
    let pkg_name = "e2e-probe";
    let project = regia_test_utils::unique_temp_dir("pkg_e2e_project");
    let _project_cleanup = Cleanup(project.clone());
    let out_dir = regia_test_utils::unique_temp_dir("pkg_e2e");
    let _out_cleanup = Cleanup(out_dir.clone());

    std::fs::create_dir_all(project.join("src")).expect("src dir");
    std::fs::create_dir_all(project.join("assets").join("meshes")).expect("assets dir");
    std::fs::copy(root.join("Cargo.lock"), project.join("Cargo.lock")).expect("Cargo.lock");
    std::fs::write(
        project.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{pkg_name}"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
regia = {{ path = "{}" }}

[workspace]

[profile.dev]
opt-level = 1
debug = "line-tables-only"

[profile.dev.package."*"]
opt-level = 3
"#,
            root.join("crates/regia").display()
        ),
    )
    .expect("Cargo.toml");
    std::fs::write(project.join("src/main.rs"), MAIN_RS).expect("main.rs");
    std::fs::write(
        project.join("Regia.toml"),
        "[project]\nname = \"E2E Probe\"\nid = \"dev.regia.e2e_probe\"\nversion = \"0.1.0\"\n",
    )
    .expect("Regia.toml");
    std::fs::write(project.join("assets/meshes/tiny.glb"), tiny_glb()).expect("tiny.glb");

    // Package for Linux through the real CLI (auto-cooks the platform first). The project is new on
    // every run (a path of its own), so no runner can have built it: this build is always made here.
    let (stdout, _) = package(
        regia_package()
            .args([
                "--platform",
                "linux",
                "--format",
                "tar",
                "--profile",
                "dev",
                "--project",
                &project.to_string_lossy(),
                "--out",
                &out_dir.to_string_lossy(),
            ])
            .env("CARGO_TARGET_DIR", dev_target_dir()),
        false,
        "the dev build of the probe project",
    );
    let tarball = stdout
        .lines()
        .rev()
        .find(|l| l.ends_with(".tar.gz"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(stdout.trim_end()));
    assert!(tarball.is_file(), "tarball missing at {}", tarball.display());

    // Extract and locate the packaged game directory.
    let extract = out_dir.join("extracted");
    std::fs::create_dir_all(&extract).expect("extract dir");
    let tar = Command::new("tar")
        .args([
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &extract.to_string_lossy(),
        ])
        .output()
        .expect("tar");
    assert!(tar.status.success(), "tar failed: {:?}", tar);
    let game_dir = std::fs::read_dir(&extract)
        .expect("extract dir readable")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("one packaged game directory in the tarball");
    let binary = game_dir.join(pkg_name);
    assert!(binary.is_file(), "packaged binary missing: {}", binary.display());
    let paks: Vec<_> = std::fs::read_dir(game_dir.join("content"))
        .expect("content dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "fpak"))
        .map(|e| e.path())
        .collect();
    assert!(!paks.is_empty(), "no paks in the package content dir");

    // Run 1: the packaged game must serve the asset from the pak.
    // Run the packaged game (the paks are deleted before the second run below).
    let run = || {
        output_within(
            Command::new(&binary)
                .arg("--exit-after-frames")
                .arg("5")
                .env("REGIA_PROBE_ASSET", "assets/meshes/tiny.glb")
                .env("REGIA_LOG", "info")
                .current_dir(&game_dir),
            HANG_BOUND,
        )
        .expect("run packaged binary")
    };
    let first = run();
    let stderr = String::from_utf8_lossy(&first.stderr);
    let stdout = String::from_utf8_lossy(&first.stdout);
    let log = format!("{stdout}\n{stderr}");
    assert!(
        log.contains("loaded from pak"),
        "packaged game did not serve the asset from a pak; log:\n{log}"
    );
    assert!(
        first.status.success(),
        "packaged game exited with code {:?}; log:\n{log}",
        first.status.code()
    );
    eprintln!("--- packaged game log (paks present) ---\n{log}");

    // Run 2: delete the paks — the load must fail, not silently fall back to a
    // dev cooked cache (a shipped game has none).
    for pak in &paks {
        std::fs::remove_file(pak).expect("delete pak");
    }
    let second = run();
    let stderr2 = String::from_utf8_lossy(&second.stderr);
    let stdout2 = String::from_utf8_lossy(&second.stdout);
    let log2 = format!("{stdout2}\n{stderr2}");
    assert!(
        log2.contains("FAILED to load"),
        "removing the paks must fail the asset load; log:\n{log2}"
    );
    assert!(
        !log2.contains("loaded from pak") && !log2.contains("loaded from dev cache"),
        "asset load must not fall back after the paks are gone; log:\n{log2}"
    );
    assert!(
        !second.status.success(),
        "packaged game must exit non-zero when its paks are missing (got {:?}); log:\n{log2}",
        second.status.code()
    );
    eprintln!("--- packaged game log (paks deleted) ---\n{log2}");
}

/// Runs the *real* `samples/sim_bridge` through the full packaging pipeline and proves the packaged
/// binary is self-contained: it mounts its paks automatically (relative to its own path), serves the
/// cooked mesh from the pak (the `REGIA_PROBE_ASSET` startup probe), reads `assets://about.txt`
/// (shipped as a raw asset next to the binary), and exits 0 after N frames. Then, with its paks
/// deleted, the same binary must fail that load and exit non-zero instead of falling back to the dev
/// cooked cache the cook left in the source tree. `packaged_linux_game_loads_asset_from_pak` checks
/// that on a project of its own, but returns early on a host without a display (the merge gate's);
/// this run needs none.
///
/// The packaged binary opens a window, so the run gets a private X server when `Xvfb` exists and
/// falls back to the ambient display otherwise. It **fails** (rather than silently skipping) when
/// neither exists: CI provides Xvfb. Ignored by default
/// (full engine build + packaged run, > 60 s); run with
/// `cargo test -p regia_cli --test package_e2e -- --ignored`.
#[test]
#[ignore = "full engine build + packaged run; invoke with -- --ignored"]
fn packaged_sim_bridge_runs_headless_from_pak() {
    let root = repo_root();
    let project = root.join("samples").join("sim_bridge");
    // A copy of the tree without the sample (a stripped review or source archive) has nothing to
    // package: say so instead of panicking on the missing sidecar below.
    if !project.is_dir() {
        eprintln!(
            "skipped: {} is absent in this tree (the packaging e2e needs the sample)",
            project.display()
        );
        return;
    }
    let out_dir = regia_test_utils::unique_temp_dir("pkg_simbridge");
    let _out_cleanup = Cleanup(out_dir.clone());
    silence_windows();
    let sidecar = project.join("assets/meshes/packaging_probe.gltf.meta");
    let sidecar_before = std::fs::read(&sidecar).expect("the probe mesh's tracked sidecar");

    // Package for Linux through the real CLI (auto-cooks the platform first).
    let (stdout, _) = package(
        regia_package()
            .args([
                "--platform",
                "linux",
                "--format",
                "tar",
                "--profile",
                "dev",
                "--project",
                &project.to_string_lossy(),
                "--out",
                &out_dir.to_string_lossy(),
            ])
            .env("CARGO_TARGET_DIR", dev_target_dir()),
        prebuilt(),
        "the dev build of sim_bridge",
    );
    let tarball = stdout
        .lines()
        .rev()
        .find(|l| l.ends_with(".tar.gz"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(stdout.trim_end()));
    assert!(tarball.is_file(), "tarball missing at {}", tarball.display());
    // A glTF's cook rewrites its `.meta`, here in the source tree: a stale tracked copy would change
    // the checkout under every other test and leave it dirty. Commit what the cook wrote.
    assert_eq!(
        std::fs::read(&sidecar).expect("the sidecar after the cook"),
        sidecar_before,
        "the cook changed the tracked {}",
        sidecar.display()
    );

    // Extract and locate the packaged game directory.
    let extract = out_dir.join("extracted");
    std::fs::create_dir_all(&extract).expect("extract dir");
    let tar = Command::new("tar")
        .args([
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &extract.to_string_lossy(),
        ])
        .output()
        .expect("tar");
    assert!(tar.status.success(), "tar failed: {:?}", tar);
    let game_dir = std::fs::read_dir(&extract)
        .expect("extract dir readable")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("one packaged game directory in the tarball");

    // The package must carry a non-empty pak, the raw about.txt, and the notices.
    let paks: Vec<_> = std::fs::read_dir(game_dir.join("content"))
        .expect("content dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "fpak"))
        .collect();
    assert!(!paks.is_empty(), "no paks in the packaged content dir");
    assert!(
        game_dir.join("assets").join("about.txt").is_file(),
        "about.txt not shipped"
    );
    assert!(
        game_dir.join("THIRD_PARTY_NOTICES.md").is_file(),
        "THIRD_PARTY_NOTICES.md not shipped"
    );
    // The engine's MPL notice with the source-location line, its licence text and the default engine
    // credit (ADR-10225). The sample sets no `engine_source_url`: the notice carries the engine's
    // default, the public repository.
    let notice = std::fs::read_to_string(game_dir.join("REGIA-ENGINE-NOTICE.txt"))
        .expect("REGIA-ENGINE-NOTICE.txt shipped");
    assert!(notice.contains("Mozilla Public License"), "{notice}");
    assert!(
        notice.contains("\nhttps://github.com/Regia-Engine/Regia-Engine\n"),
        "the source-location line: {notice}"
    );
    assert!(
        !notice.contains("<release host>"),
        "no placeholder left: {notice}"
    );
    let licence = std::fs::read_to_string(game_dir.join("REGIA-ENGINE-LICENSE.txt"))
        .expect("REGIA-ENGINE-LICENSE.txt shipped");
    assert_eq!(
        licence,
        std::fs::read_to_string(root.join("LICENSE")).expect("the repository LICENSE")
    );
    let credits = std::fs::read_to_string(game_dir.join("CREDITS.txt")).expect("CREDITS.txt shipped");
    assert!(
        credits.lines().any(|l| l == "Made with Regia Engine"),
        "the default credit: {credits}"
    );

    let binary = game_dir.join("sim_bridge");
    assert!(binary.is_file(), "packaged binary missing: {}", binary.display());

    // Run headless: on a private X server when Xvfb exists, otherwise on the ambient display (the
    // window is routed to a hidden workspace by `silence_windows`). The probe must load the mesh
    // from the pak and the sample must read about.txt through assets://, then exit 0 after N frames.
    let display = display_for_packaged_run();
    let mut command = Command::new(&binary);
    if let Some(display) = &display {
        display.apply(&mut command);
    }
    let run = output_within(
        command
            .args(["--exit-after-frames", "10"])
            .env("REGIA_PROBE_ASSET", "assets/meshes/packaging_probe.gltf")
            .env("REGIA_LOG", "info")
            .current_dir(&game_dir),
        HANG_BOUND,
    )
    .expect("run the packaged binary");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    eprintln!("--- packaged sim_bridge log ---\n{log}");

    assert!(
        run.status.success(),
        "packaged sim_bridge exited with {:?}; log:\n{log}",
        run.status.code()
    );
    assert!(
        log.contains("loaded from pak"),
        "the probe mesh must be served from the pak; log:\n{log}"
    );
    assert!(
        log.contains("Sim Bridge"),
        "about.txt must resolve through assets:// in the packaged build; log:\n{log}"
    );
    assert!(
        !log.contains("cannot read assets://about.txt"),
        "about.txt must not fail to load; log:\n{log}"
    );

    // Run 2: without the paks the load must fail, not fall back to a dev cooked cache (a shipped
    // game has none; here the cook has just left one in `samples/sim_bridge/.regia`).
    for pak in &paks {
        std::fs::remove_file(pak.path()).expect("delete pak");
    }
    let mut command = Command::new(&binary);
    if let Some(display) = &display {
        display.apply(&mut command);
    }
    let second = output_within(
        command
            .args(["--exit-after-frames", "10"])
            .env("REGIA_PROBE_ASSET", "assets/meshes/packaging_probe.gltf")
            .env("REGIA_LOG", "info")
            .current_dir(&game_dir),
        HANG_BOUND,
    )
    .expect("run the packaged binary without its paks");
    let log2 = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    eprintln!("--- packaged sim_bridge log (paks deleted) ---\n{log2}");
    assert!(
        log2.contains("FAILED to load"),
        "removing the paks must fail the asset load; log:\n{log2}"
    );
    assert!(
        !log2.contains("loaded from pak") && !log2.contains("loaded from dev cache"),
        "asset load must not fall back after the paks are gone; log:\n{log2}"
    );
    assert!(
        !second.status.success(),
        "packaged sim_bridge must exit non-zero when its paks are missing (got {:?}); log:\n{log2}",
        second.status.code()
    );

    // `--no-engine-credit` (ADR-10225): the same build packaged again as a directory has the MPL
    // notice and licence but no "Made with Regia Engine" line (the sample has no CREDITS.txt of its
    // own, so there is no credits file at all). Same cargo flags: no new build.
    let plain_out = regia_test_utils::unique_temp_dir("pkg_simbridge_nocredit");
    let _plain_cleanup = Cleanup(plain_out.clone());
    let (plain_stdout, _) = package(
        regia_package()
            .args([
                "--platform",
                "linux",
                "--format",
                "dir",
                "--profile",
                "dev",
                "--no-engine-credit",
                "--project",
                &project.to_string_lossy(),
                "--out",
                &plain_out.to_string_lossy(),
            ])
            .env("CARGO_TARGET_DIR", dev_target_dir()),
        prebuilt(),
        "the dev build of sim_bridge (packaged again)",
    );
    let plain_dir = PathBuf::from(
        plain_stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .trim(),
    );
    assert!(
        plain_dir.is_dir(),
        "package directory missing at {}",
        plain_dir.display()
    );
    assert!(
        plain_dir.join("REGIA-ENGINE-NOTICE.txt").is_file(),
        "the notice is not optional"
    );
    assert!(
        plain_dir.join("REGIA-ENGINE-LICENSE.txt").is_file(),
        "the licence is not optional"
    );
    let credit = std::fs::read_to_string(plain_dir.join("CREDITS.txt")).unwrap_or_default();
    assert!(
        !credit.contains("Made with Regia Engine"),
        "--no-engine-credit leaves the credit out: {credit}"
    );
}

/// Newest `GLIBC_x.y` the ELF at `binary` imports, from `objdump -T` (`None`: no glibc imports).
fn max_glibc(binary: &std::path::Path) -> Option<(u32, u32)> {
    let out = Command::new("objdump")
        .arg("-T")
        .arg(binary)
        .output()
        .expect("objdump -T");
    assert!(out.status.success(), "objdump -T failed: {out:?}");
    String::from_utf8_lossy(&out.stdout)
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter_map(|t| t.strip_prefix("GLIBC_"))
        .filter_map(|v| {
            let mut parts = v.split('.');
            Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
        .max()
}

/// Peak resident set (`VmHWM`, kB) of a live process.
fn peak_rss_kb(pid: u64) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|kb| kb.parse().ok())
}

/// The Steam-shipping check of the real `samples/arena` (`docs/specs/21` § 4, "Shipping-binary checks"): package it with the
/// shipping profile (default glibc floor 2.31 = Steam Linux Runtime 3.0, cargo-zigbuild), extract the
/// tarball into a directory outside the repo, run the binary there with no cargo/repo environment,
/// and through the agent protocol capture the presented window frame: it must **not be black**
/// (a black present passed the earlier exit-code-only smoke). Also asserts the binary's glibc floor,
/// no build-machine path in it, and a real git revision in `build_info.json`.
///
/// Knobs: `REGIA_PACKAGE_E2E_GLIBC` (`2.31` default, `host` to skip cargo-zigbuild),
/// `REGIA_PACKAGE_E2E_OUT` (keep the package, the captured PNG and `run.json` there); set
/// `REGIA_GIT_HASH` when the tree has no `.git` (exported build trees).
/// Ignored by default (shipping LTO build + windowed run, minutes); run with
/// `cargo test -p regia_cli --test package_e2e packaged_arena -- --ignored --nocapture`.
#[test]
#[ignore = "shipping build + windowed packaged run; invoke with -- --ignored"]
fn packaged_arena_runs_from_a_clean_dir_and_presents_a_lit_frame() {
    use base64::Engine as _;

    let root = repo_root();
    let project = root.join("samples").join("arena");
    let keep = regia_core::brand::env_var_os("REGIA_PACKAGE_E2E_OUT").map(PathBuf::from);
    let out_dir = keep
        .clone()
        .unwrap_or_else(|| regia_test_utils::unique_temp_dir("pkg_arena"));
    let _out_cleanup = keep.is_none().then(|| Cleanup(out_dir.clone()));
    let _ = std::fs::remove_dir_all(out_dir.join("clean"));
    silence_windows();

    let glibc = regia_core::brand::env_var("REGIA_PACKAGE_E2E_GLIBC").unwrap_or_else(|_| "2.31".into());
    let (stdout, _) = package(
        regia_package()
            .args(["--platform", "linux", "--format", "tar", "--profile", "shipping"])
            .args(["--glibc", &glibc, "--project"])
            .arg(&project)
            .arg("--out")
            .arg(&out_dir),
        prebuilt(),
        "the shipping build of arena",
    );
    let tarball = stdout
        .lines()
        .rev()
        .find(|l| l.ends_with(".tar.gz"))
        .map(PathBuf::from)
        .expect("regia package prints the tarball path");
    let tar_bytes = std::fs::metadata(&tarball).map(|m| m.len()).expect("tarball");

    // A clean directory outside the repo: no Regia.toml above it, no cargo.
    let clean = out_dir.join("clean");
    std::fs::create_dir_all(&clean).expect("clean dir");
    let tar = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&clean)
        .output()
        .expect("tar");
    assert!(tar.status.success(), "tar failed: {tar:?}");
    let game_dir = std::fs::read_dir(&clean)
        .expect("clean dir readable")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("one packaged game directory in the tarball");
    assert!(
        !game_dir.ancestors().any(|d| d.join("Regia.toml").is_file()),
        "the clean dir must not sit inside a Regia Engine project: {}",
        game_dir.display()
    );
    let binary = game_dir.join("arena");
    let binary_bytes = std::fs::metadata(&binary)
        .map(|m| m.len())
        .expect("packaged binary");

    // The binary: glibc floor, no build-machine path (the CLI checks too; this is the independent check).
    let max = max_glibc(&binary);
    if glibc != "host" {
        let floor: Vec<u32> = glibc.split('.').filter_map(|p| p.parse().ok()).collect();
        assert!(
            max.is_none_or(|m| [m.0, m.1] <= [floor[0], floor[1]]),
            "the packaged binary needs GLIBC_{max:?}, above the {glibc} floor"
        );
    }
    let bytes = std::fs::read(&binary).expect("read binary");
    let root_text = root.to_string_lossy().into_owned();
    assert!(
        !bytes.windows(root_text.len()).any(|w| w == root_text.as_bytes()),
        "the shipping binary embeds the build machine's repo path {root_text}"
    );
    let info: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(game_dir.join("build_info.json")).expect("build_info.json"),
    )
    .expect("build_info.json is JSON");
    assert_ne!(
        info["git"], "unknown",
        "build_info.json git must be the source revision: {info}"
    );

    // Run from the clean dir: the agent endpoint goes to a scratch project dir (REGIA_PROJECT_DIR).
    let display = display_for_packaged_run();
    let agent_home = out_dir.join("agent-home");
    let _ = std::fs::remove_dir_all(&agent_home);
    std::fs::create_dir_all(&agent_home).expect("agent dir");
    let mut command = Command::new(&binary);
    if let Some(display) = &display {
        display.apply(&mut command);
    }
    for var in [
        "CARGO_MANIFEST_DIR",
        "CARGO_TARGET_DIR",
        "REGIA_ASSETS",
        "REGIA_KHRONOS_ASSETS",
        "REGIA_DO27_CONTENT",
    ] {
        command.env_remove(var);
    }
    let started = Instant::now();
    let mut child = command
        .args(["--bots", "2", "--exit-after-frames", "100000"])
        .env("REGIA_PROJECT_DIR", &agent_home)
        .env("REGIA_LOG", "info")
        .current_dir(&game_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the packaged arena");
    let log = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let mut readers = Vec::new();
    let streams = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ];
    let mut child = Running(child);
    for stream in streams.into_iter().flatten() {
        let log = log.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if let Ok(mut log) = log.lock() {
                    log.push_str(&line);
                    log.push('\n');
                }
            }
        }));
    }
    let log_text = || log.lock().map(|l| l.clone()).unwrap_or_default();

    // Connect once the endpoint is advertised (cold start: shader pipelines compile on first run).
    let deadline = Instant::now() + HANG_BOUND;
    let mut client = loop {
        if let Ok(client) = regia_agent::client::Client::connect_project(&agent_home) {
            break client;
        }
        if let Ok(Some(status)) = child.0.try_wait() {
            panic!("the packaged arena exited early ({status}); log:\n{}", log_text());
        }
        assert!(
            Instant::now() < deadline,
            "no agent endpoint within {HANG_BOUND:?}; log:\n{}",
            log_text()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    // Longer than the calls' own timeouts below, so a call reports its result.
    client
        .set_read_timeout(Some(HANG_BOUND + Duration::from_secs(30)))
        .expect("read timeout");
    let endpoint_pid = std::fs::read_to_string(agent_home.join(".regia/agent/endpoint.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["pid"].as_u64());

    // The first frame on screen, then capture the window: a shipping build on lavapipe (the gate)
    // can take minutes to get there under load, so this waits for the mark, not for a time.
    let hang_bound_ms = HANG_BOUND.as_millis() as u64;
    let presented = |client: &mut regia_agent::client::Client| {
        client
            .call("render.boot_timeline", serde_json::json!({}))
            .ok()
            .and_then(|boot| boot["marks"].as_array().cloned())
            .is_some_and(|marks| marks.iter().any(|m| m["mark"] == "first_present"))
    };
    while !presented(&mut client) {
        if let Ok(Some(status)) = child.0.try_wait() {
            panic!(
                "the packaged arena exited before its first frame ({status}); log:\n{}",
                log_text()
            );
        }
        assert!(
            Instant::now() < deadline,
            "no first frame within {HANG_BOUND:?}; log:\n{}",
            log_text()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let shot = client
        .call(
            "seeing.screenshot",
            serde_json::json!({ "target": "window", "timeout_ms": hang_bound_ms }),
        )
        .unwrap_or_else(|e| panic!("seeing.screenshot failed: {e}; log:\n{}", log_text()));
    let png = base64::engine::general_purpose::STANDARD
        .decode(shot["png_base64"].as_str().expect("png_base64"))
        .expect("base64 png");
    let image = image::load_from_memory(&png).expect("decode png").to_rgba8();
    let (w, h) = image.dimensions();
    let pixels = (w as u64 * h as u64).max(1);
    let mut lit = 0u64;
    let mut sum = 0u64;
    let mut lo = 255u8;
    let mut hi = 0u8;
    for p in image.pixels() {
        let luma = ((u32::from(p[0]) * 54 + u32::from(p[1]) * 183 + u32::from(p[2]) * 19) >> 8) as u8;
        sum += u64::from(luma);
        lit += u64::from(luma > 16);
        lo = lo.min(luma);
        hi = hi.max(luma);
    }
    let mean = sum as f64 / pixels as f64;
    let lit_fraction = lit as f64 / pixels as f64;
    let boot = client
        .call("render.boot_timeline", serde_json::json!({}))
        .unwrap_or_default();
    let first_present_ms = boot["marks"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["mark"] == "first_present"))
        .and_then(|m| m["ms"].as_f64());
    let rss_kb = endpoint_pid.and_then(peak_rss_kb);

    let _ = client.call("quit.request", serde_json::json!({}));
    let status = wait_within(&mut child.0, HANG_BOUND).ok();
    for reader in readers {
        let _ = reader.join();
    }
    let log = log_text();
    let summary = serde_json::json!({
        "tarball_bytes": tar_bytes,
        "binary_bytes": binary_bytes,
        "glibc_floor": glibc,
        "glibc_max": max.map(|(a, b)| format!("{a}.{b}")),
        "build_info": info,
        "first_present_ms": first_present_ms,
        "agent_ready_ms": started.elapsed().as_millis() as u64,
        "peak_rss_kb": rss_kb,
        "frame": { "width": w, "height": h, "mean_luma": mean, "lit_fraction": lit_fraction, "min": lo, "max": hi },
        "exit": status.and_then(|s| s.code()),
    });
    eprintln!("--- packaged arena log ---\n{log}\n--- summary ---\n{summary:#}");
    if let Some(keep) = &keep {
        let _ = std::fs::write(keep.join("arena-package-frame.png"), &png);
        let _ = std::fs::write(keep.join("run.json"), format!("{summary:#}"));
    }

    // A lit arena: a real share of pixels above black, real contrast, a non-trivial mean.
    assert!(
        lit_fraction > 0.25 && mean > 20.0 && hi > lo.saturating_add(40),
        "the packaged arena presented a black/flat frame (mean {mean:.1}, lit {lit_fraction:.2}, range {lo}..{hi}); log:\n{log}"
    );
    assert!(
        status.is_some_and(|s| s.success()),
        "the packaged arena did not exit cleanly after quit.request ({status:?}); log:\n{log}"
    );
}

/// The dev builds stay under this test's target dir (so they are cached and cleaned with it) and
/// out of the directories other cargo runs lock.
#[test]
fn dev_builds_have_a_target_dir_of_their_own() {
    let dir = dev_target_dir();
    let target = dir.parent().expect("a parent");
    let exe = std::env::current_exe().expect("test binary path");
    assert!(
        exe.starts_with(target),
        "{} is not under {}",
        exe.display(),
        target.display()
    );
    assert!(
        !exe.starts_with(&dir),
        "the test binary is itself under {}",
        dir.display()
    );
}

/// A cargo progress line names the crate it compiles; nothing else on a package's log is a build.
#[test]
fn compiling_lines_name_the_crate() {
    assert_eq!(
        compiling("   Compiling regia_core v0.2.23 (/w/crates/regia_core)"),
        Some("regia_core")
    );
    assert_eq!(
        compiling("Compiling arena v0.2.23 (/w/samples/arena)"),
        Some("arena")
    );
    for line in [
        "    Finished `shipping` profile [optimized + debuginfo] target(s) in 0.46s",
        "    Blocking waiting for file lock on package cache",
        "2026-11-11T07:26:55.172911Z  INFO running: cargo build --profile shipping -p arena",
        "cook: 1 cooked, 0 skipped, 0 errors, 11 non-cookable files skipped",
        "",
    ] {
        assert_eq!(compiling(line), None, "{line}");
    }
}

/// The variable that makes this test binary a stand-in for `regia package` (or for a job one
/// starts): its value names the part [`stand_in_for_regia_package`] plays.
const STAND_IN: &str = "REGIA_PACKAGE_E2E_STAND_IN";

/// A stand-in for `regia package`: this test binary again, running [`stand_in_for_regia_package`]
/// as `role`. No shell and no `PATH` lookup, so the tests below run alike on Linux, macOS and
/// Windows.
fn stand_in(role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command
        .args(["--exact", "stand_in_for_regia_package", "--nocapture"])
        .env(STAND_IN, role);
    command
}

/// Not a test of its own (it returns at once without [`STAND_IN`]): the stand-in that
/// [`stand_in`] starts. Every line it prints for the caller starts on a line of its own, apart from
/// the harness's lines around it.
///
/// - `refused`: a `regia package` whose build is not fresh. It starts a `job` (cargo's `rustc`,
///   holding the run's output pipes as `rustc` holds cargo's), prints `job <pid>`, logs a lock wait
///   and its first `Compiling` line, and waits for the job.
/// - `job`: runs far past the tests' hang bound (and then ends, so a failed stop leaks no process
///   for good).
/// - `building` / `fresh`: a package that compiles, or finds its build fresh, and prints the
///   package's path.
#[test]
fn stand_in_for_regia_package() {
    let Some(role) = std::env::var_os(STAND_IN) else {
        return;
    };
    match role.to_str() {
        Some("refused") => {
            let mut job = stand_in("job")
                .stdin(Stdio::null())
                .spawn()
                .expect("start the stand-in's job");
            println!("\njob {}", job.id());
            eprintln!("\n    Blocking waiting for file lock on package cache");
            eprintln!("   Compiling stand_in_crate v0.0.0 (/stand-in)");
            let _ = job.wait();
        }
        Some("job") => std::thread::sleep(2 * HANG_BOUND),
        Some("building") => {
            eprintln!("\n   Compiling stand_in_crate v0.0.0 (/stand-in)");
            println!("\n/out/game-linux.tar.gz");
        }
        Some("fresh") => {
            eprintln!("\n    Finished `shipping` profile [optimized + debuginfo] target(s) in 0.46s");
            println!("\n/out/game-linux.tar.gz");
        }
        other => panic!("unknown stand-in role {other:?}"),
    }
    // The harness's result lines would follow the package's path; a package's run ends here.
    std::process::exit(0);
}

/// A package that was to be found fresh and starts compiling is stopped at its first crate, with
/// the processes it started. The stand-in's job stands for cargo's `rustc`; neither it nor the
/// stand-in ends by itself within the bound, and the job holds the run's output pipes until it
/// ends, so the run only returns because the whole tree was stopped (the refused build of the
/// review's scenario: cargo killed, its `rustc` alive on the log pipe). The wait for it is a
/// channel's bounded receive, not a poll.
#[test]
fn a_rebuild_of_a_prebuilt_package_stops_the_whole_run() {
    let (sender, returned) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(run_package(&mut stand_in("refused"), true));
    });
    let packaged = match returned.recv_timeout(HANG_BOUND) {
        Ok(packaged) => packaged,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
            "the refused run did not end within {HANG_BOUND:?}: a process it started still holds its output \
             (the stop did not reach the stand-in's job)"
        ),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("the refused run panicked"),
    };
    let Packaged::Rebuilt {
        krate,
        stdout,
        progress,
        unstopped,
    } = packaged
    else {
        panic!("the run was not stopped at its first compiled crate: {packaged:?}");
    };
    assert_eq!(unstopped, None, "the stop was refused");
    assert_eq!(krate, "stand_in_crate");
    assert!(
        progress.contains("Blocking waiting for file lock"),
        "the log until the stop comes back: {progress}"
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("job ")),
        "the stand-in started its job: {stdout}"
    );
}

/// Without the knob a build inside the run is a build like any other (its log stays on this
/// process's stderr), and a fresh build passes with the knob (its log comes back): both runs go to
/// their end with their result.
#[test]
fn a_package_runs_to_its_end_when_nothing_is_refused() {
    for (role, must_be_fresh, logged) in [("building", false, ""), ("fresh", true, "Finished `shipping`")] {
        let Packaged::Done {
            status,
            stdout,
            progress,
        } = run_package(&mut stand_in(role), must_be_fresh)
        else {
            panic!("the run was stopped: {role}");
        };
        assert!(status.success(), "{status}");
        assert!(
            stdout.lines().any(|line| line == "/out/game-linux.tar.gz"),
            "{stdout}"
        );
        assert!(progress.contains(logged), "{progress}");
        assert_eq!(
            progress.is_empty(),
            !must_be_fresh,
            "only a refused build's log is read here"
        );
    }
}
