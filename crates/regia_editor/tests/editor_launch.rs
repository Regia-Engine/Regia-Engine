// SPDX-License-Identifier: MPL-2.0
//! Launch standalone and Launch on Android device (`docs/specs/19-editor.md` § 4, ADR-8980), end to
//! end through the installed agent method table (the one the dispatcher serves).
//!
//! No real device and no real build: a fake `adb`, a fake `regia` CLI, a fake `cargo` and a fake
//! game are shell scripts written into a unique temp dir, and that dir is prepended to the PATH the
//! launcher searches and hands its children (`LaunchTools::path_env`; the process environment is
//! never changed). The Android SDK lookup is switched off (`SdkSource::None`), so an SDK on the
//! build host can never put a real `adb` first. Each fake records its argv, one line per call.
//! Waits block on the launcher's condition variable (woken by every line and state change), bounded
//! only so a broken build cannot hang the run; no assert reads the wall clock.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use regia_ecs::prelude::*;
use regia_editor::launch::{LaunchTools, Launcher, SdkSource};
use serde_json::{Value, json};

/// The hang guard of every wait (never an assertion).
const HANG_GUARD: Duration = Duration::from_secs(120);

/// The log ring only fills under the `RingLayer` subscriber: install it once for this binary.
fn logging() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| regia_core::log::init(regia_core::log::LogConfig::default()).is_ok())
}

fn call(world: &mut World, method: &str, params: Value) -> Result<Value, regia_agent::protocol::RpcError> {
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is installed"));
    handler(world, &params)
}

fn code(err: &regia_agent::protocol::RpcError) -> String {
    err.data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Writes an executable shell script.
fn script(path: &Path, body: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("#!/bin/sh\n{body}")).expect("write script");
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::rename(&tmp, path).expect("publish script");
}

/// A project with a launcher crate and an Android app id.
fn project(root: &Path) -> PathBuf {
    let project = root.join("project");
    std::fs::create_dir_all(project.join("assets")).expect("assets");
    std::fs::create_dir_all(project.join("launcher")).expect("launcher");
    std::fs::write(
        project.join("Regia.toml"),
        "[project]\nname = \"Launch\"\nid = \"dev.regia.launchtest\"\nversion = \"0.1.0\"\n\n\
         [platforms.android]\nabis = [\"arm64-v8a\", \"x86_64\"]\n",
    )
    .expect("Regia.toml");
    std::fs::write(
        project.join("launcher/Cargo.toml"),
        "[package]\nname = \"launch-launcher\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("launcher manifest");
    project
}

/// A project whose `[project] id` is `id` (written as a TOML string, so any text fits).
fn project_with_id(root: &Path, id: &str) -> PathBuf {
    let project = project(root);
    std::fs::write(
        project.join("Regia.toml"),
        format!(
            "[project]\nname = \"Launch\"\nid = {}\nversion = \"0.1.0\"\n",
            toml::Value::String(id.to_owned())
        ),
    )
    .expect("Regia.toml");
    project
}

/// The shell function every fake step uses to hold while `<root>/hold-<name>` exists: it records
/// its pid in `<name>.pid`, starts a grandchild (`<name>-grandchild.pid`, as cargo starts rustc),
/// then says `<name> holding` on stderr (always logged) and waits for the file to go.
fn hold_fn(root: &Path) -> String {
    let root = root.display();
    format!(
        r#"hold() {{
  if [ -e "{root}/hold-$1" ]; then
    echo $$ > "{root}/$1.pid"
    sleep 100000 &
    echo $! > "{root}/$1-grandchild.pid"
    echo "$1 holding" >&2
    while [ -e "{root}/hold-$1" ]; do sleep 0.05; done
    kill $! 2>/dev/null
  fi
}}
"#
    )
}

/// Makes the fake step `name` (`cargo`, `package`, `install`) hold until [`release`].
fn hold(root: &Path, name: &str) {
    std::fs::write(root.join(format!("hold-{name}")), b"").expect("hold file");
}

/// Lets a held fake step go on.
fn release(root: &Path, name: &str) {
    let _ = std::fs::remove_file(root.join(format!("hold-{name}")));
}

/// The fakes' directory, holding the `adb`, `regia`, `cargo` and game scripts. `devices` is the
/// body `adb devices -l` prints after its header.
fn fakes(root: &Path, devices: &str, cargo_fails: bool) -> PathBuf {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let log = root.join("argv.log");
    let log = log.display();
    let root_s = root.display();
    let hold_sh = hold_fn(root);
    script(
        &bin.join("adb"),
        &format!(
            r#"{hold_sh}echo "adb $*" >> "{log}"
if [ "$1" = "-s" ]; then shift; shift; fi
case "$1" in
  devices) printf 'List of devices attached\n{devices}\n' ;;
  install) hold install; echo "Performing Streamed Install"; echo "Success" ;;
  forward)
    if [ "$2" != "--remove" ]; then
      hold forward
      if [ -e "{root_s}/device-agent/forward-port" ]; then cat "{root_s}/device-agent/forward-port"; else echo "41999"; fi
    fi ;;
  logcat)
    echo $$ > "{root_s}/logcat.pid"
    echo "I regia: fake logcat line one"
    echo "I regia: fake logcat line two"
    exec sleep 100000 ;;
  shell)
    case "$2 $3" in
      "getprop ro.product.cpu.abi") echo "arm64-v8a" ;;
      "am start") echo "Starting: Intent {{ cmp=$5 }}" ;;
      "am force-stop") : ;;
      "pidof "*) echo "4242" ;;
    esac ;;
esac
"#
        ),
    );
    script(
        &bin.join("regia"),
        &format!(
            r#"{hold_sh}echo "regia $*" >> "{log}"
hold package
echo "packaging the fake apk" >&2
: > "{root_s}/app-debug.apk"
echo '{{"ok":true,"artifact":"{root_s}/app-debug.apk","platform":"android","profile":"dev"}}'
"#
        ),
    );
    let game = bin.join("fake-game");
    script(
        &game,
        &format!(
            r#"echo $$ > "{root_s}/game.pid"
sleep 100000 &
echo $! > "{root_s}/grandchild.pid"
echo "fake game: hello"
echo "fake game: start level=$REGIA_START_LEVEL" >&2
echo "fake game: agent dir=$REGIA_AGENT_DIR" >&2
wait
"#
        ),
    );
    let cargo_body = if cargo_fails {
        format!(
            r#"echo "cargo $*" >> "{log}"
echo "   Compiling launch-launcher v0.1.0" >&2
echo "error[E0425]: cannot find value \`speed\` in this scope" >&2
echo " --> launcher/src/main.rs:3:5" >&2
echo "error: could not compile \`launch-launcher\` (bin \"launch-launcher\") due to 1 previous error" >&2
exit 101
"#
        )
    } else {
        format!(
            r#"{hold_sh}echo "cargo $*" >> "{log}"
hold cargo
echo "   Compiling launch-launcher v0.1.0" >&2
echo '{{"reason":"compiler-artifact","package_id":"path+file:///p/launcher#launch-launcher@0.1.0","target":{{"kind":["bin"],"name":"launch-launcher"}},"executable":"{game}"}}'
echo '{{"reason":"build-finished","success":true}}'
"#,
            game = game.display()
        )
    };
    script(&bin.join("cargo"), &cargo_body);
    bin
}

/// An editor world over `project` whose launcher finds only the fakes.
fn editor_world(project: &Path, bin: &Path) -> World {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let mut world = std::mem::take(app.world_mut());
    world.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(project.to_path_buf()),
    });
    let mut path = std::ffi::OsString::from(bin);
    if let Some(rest) = std::env::var_os("PATH") {
        path.push(":");
        path.push(rest);
    }
    let mut launcher = Launcher::default();
    launcher.tools = LaunchTools {
        cli: Some(bin.join("regia")),
        path_env: Some(path),
        sdk: SdkSource::None,
        ..LaunchTools::default()
    };
    world.insert_resource(launcher);
    world
}

fn argv(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("argv.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .trim()
        .parse()
        .expect("a pid")
}

/// Whether `pid` is a live process (`kill -0`; a zombie still answers, so the launcher must have
/// reaped its own child for this to be false).
fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether `pid` is gone or a zombie (an orphan the launcher does not own is reaped by init on its
/// own time; it must not still be *running*).
#[cfg(target_os = "linux")]
fn not_running(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X')),
    }
}

fn status(world: &mut World) -> Value {
    call(world, "editor.launch_status", json!({ "tail": 400 })).expect("launch_status")
}

fn tail_has(state: &Value, needle: &str) -> bool {
    state["log_tail"].as_array().is_some_and(|lines| {
        lines
            .iter()
            .any(|l| l.as_str().is_some_and(|l| l.contains(needle)))
    })
}

/// Blocks until the launch's log tail holds `needle` or it ended.
fn wait_line(world: &World, needle: &str) -> bool {
    world
        .resource::<Launcher>()
        .wait_for(|s| s.tail_contains(needle) || !s.state.is_active(), HANG_GUARD)
}

fn ring_has(needle: &str) -> bool {
    let (records, _) = regia_core::log::ring::recent(&regia_core::log::ring::LogQuery {
        target: Some(regia_editor::launch::LOG_TARGET.to_owned()),
        contains: Some(needle.to_owned()),
        ..Default::default()
    });
    !records.is_empty()
}

#[test]
fn android_launch_runs_devices_install_start_in_order_streams_logcat_and_stop_ends_both() {
    let logs = logging();
    let root = regia_test_utils::unique_temp_dir("launch-android");
    let project = project(&root);
    let bin = fakes(
        &root,
        "FAKE123\tdevice product:fake model:Fake_Phone device:fake transport_id:1",
        false,
    );
    let mut world = editor_world(&project, &bin);

    let devices = call(&mut world, "editor.launch_devices", json!({})).expect("launch_devices");
    assert_eq!(devices["devices"][0]["serial"], "FAKE123", "{devices:#}");
    assert_eq!(devices["devices"][0]["model"], "Fake_Phone");

    let started = call(&mut world, "editor.launch", json!({ "target": "android" })).expect("launch");
    assert_eq!(started["target"], "android");
    assert_eq!(started["device"], "FAKE123", "the only device is picked");
    let id = started["id"].as_u64().expect("a launch id");

    assert!(
        wait_line(&world, "fake logcat line two"),
        "the launch ended early"
    );
    let state = status(&mut world);
    assert_eq!(state["state"], "running", "{state:#}");
    assert_eq!(state["pid"], 4242, "the app pid from pidof");
    assert!(tail_has(&state, "fake logcat line one"), "{state:#}");
    assert!(
        tail_has(&state, "packaging the fake apk"),
        "the build output is streamed too"
    );
    if logs {
        assert!(
            ring_has(&format!("[launch {id}] I regia: fake logcat line two")),
            "logcat lines reach the Output Log tagged by launch id"
        );
    }

    let calls = argv(&root);
    let position = |needle: &str| {
        calls
            .iter()
            .position(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("{needle} in {calls:#?}"))
    };
    let devices_at = position("adb devices -l");
    let package_at = position("regia package --platform android");
    let install_at = position("adb -s FAKE123 install -r");
    let start_at = position("adb -s FAKE123 shell am start -n dev.regia.launchtest/.RegiaActivity");
    let pidof_at = position("adb -s FAKE123 shell pidof dev.regia.launchtest");
    let logcat_at = position("adb -s FAKE123 logcat --pid 4242");
    assert!(
        devices_at < package_at
            && package_at < install_at
            && install_at < start_at
            && start_at < pidof_at
            && pidof_at < logcat_at,
        "devices → build → install → start → logcat: {calls:#?}"
    );
    assert!(
        calls[package_at].contains("--abi arm64-v8a"),
        "the device ABI is built when the manifest lists it: {}",
        calls[package_at]
    );
    assert!(
        calls[install_at].contains("app-debug.apk"),
        "the packaged APK is installed"
    );

    let logcat_pid = read_pid(&root.join("logcat.pid"));
    assert!(alive(logcat_pid), "logcat runs until Stop");

    let stopped = call(&mut world, "editor.launch_stop", json!({})).expect("launch_stop");
    assert_eq!(stopped["stopped"], true, "{stopped:#}");
    assert!(!alive(logcat_pid), "Stop ended logcat and reaped it");
    let calls = argv(&root);
    let force_stop = calls
        .iter()
        .position(|c| c.contains("adb -s FAKE123 shell am force-stop dev.regia.launchtest"))
        .unwrap_or_else(|| panic!("force-stop in {calls:#?}"));
    assert!(force_stop > logcat_at, "the app is force-stopped at Stop");
    let state = status(&mut world);
    assert_eq!(state["state"], "stopped", "{state:#}");
    assert_eq!(state["running"], false);

    // A second Stop has nothing to stop.
    let again = call(&mut world, "editor.launch_stop", json!({})).expect("launch_stop again");
    assert_eq!(again["stopped"], false);
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn standalone_launch_cooks_builds_streams_the_game_and_stop_kills_its_tree() {
    let logs = logging();
    let root = regia_test_utils::unique_temp_dir("launch-standalone");
    let project = project(&root);
    let bin = fakes(&root, "", false);
    let mut world = editor_world(&project, &bin);

    let started = call(&mut world, "editor.launch", json!({ "target": "standalone" })).expect("launch");
    assert_eq!(started["target"], "standalone");
    let id = started["id"].as_u64().expect("a launch id");
    assert!(wait_line(&world, "fake game: hello"), "the launch ended early");
    let state = status(&mut world);
    assert_eq!(state["state"], "running", "{state:#}");
    assert_eq!(state["running"], true);
    let steps: Vec<&str> = state["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(steps, ["cook", "cargo build", "run"], "{state:#}");
    assert!(
        tail_has(&state, "Compiling launch-launcher"),
        "cargo's output is streamed"
    );
    if logs {
        assert!(ring_has(&format!("[launch {id}] fake game: hello")));
    }
    let calls = argv(&root);
    let build = calls
        .iter()
        .find(|c| c.starts_with("cargo build"))
        .unwrap_or_else(|| panic!("cargo build in {calls:#?}"));
    assert!(build.contains("-p launch-launcher"), "{build}");
    assert!(
        build.contains("--message-format=json-render-diagnostics"),
        "{build}"
    );

    let game = read_pid(&root.join("game.pid"));
    let grandchild = read_pid(&root.join("grandchild.pid"));
    assert_eq!(state["pid"], game, "the game's pid");
    assert!(alive(game) && alive(grandchild));

    let stopped = call(&mut world, "editor.launch_stop", json!({})).expect("launch_stop");
    assert_eq!(stopped["stopped"], true);
    assert!(!alive(game), "Stop killed and reaped the game");
    #[cfg(target_os = "linux")]
    assert!(not_running(grandchild), "Stop killed the game's process group");
    assert_eq!(status(&mut world)["state"], "stopped");
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_build_failure_answers_launch_build_failed_with_the_log_tail() {
    let root = regia_test_utils::unique_temp_dir("launch-build-failed");
    let project = project(&root);
    let bin = fakes(&root, "", true);
    let mut world = editor_world(&project, &bin);

    let answer = call(
        &mut world,
        "editor.launch",
        json!({ "target": "standalone", "wait": true }),
    );
    let tail = match answer {
        Err(err) => {
            assert_eq!(code(&err), "launch_build_failed", "{err:?}");
            err.data
                .as_ref()
                .and_then(|d| d.get("log_tail"))
                .and_then(Value::as_array)
                .cloned()
                .expect("log_tail in the error data")
        }
        // `wait` is bounded (ADR-7548): on a loaded host the fake build may still run when the
        // bound passes; the answer is then `pending` and the status carries the same error.
        Ok(pending) => {
            assert_eq!(pending["pending"], true, "{pending:#}");
            assert!(
                world
                    .resource::<Launcher>()
                    .wait_for(|s| !s.state.is_active(), HANG_GUARD),
                "the launch never ended"
            );
            let state = status(&mut world);
            assert_eq!(state["error"]["code"], "launch_build_failed", "{state:#}");
            state["error"]["log_tail"]
                .as_array()
                .cloned()
                .expect("log_tail in the status error")
        }
    };
    assert!(
        tail.iter()
            .any(|l| l.as_str().is_some_and(|l| l.contains("error[E0425]"))),
        "{tail:?}"
    );
    let state = status(&mut world);
    assert_eq!(state["state"], "failed");
    assert_eq!(state["error"]["code"], "launch_build_failed");
    assert!(!root.join("game.pid").exists(), "no game ran");
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn android_device_choice_is_refused_with_none_several_or_an_unknown_serial() {
    let root = regia_test_utils::unique_temp_dir("launch-devices");
    let project = project(&root);

    let bin = fakes(&root, "", false);
    let mut world = editor_world(&project, &bin);
    let err = call(&mut world, "editor.launch", json!({ "target": "android" })).expect_err("no device");
    assert_eq!(code(&err), "launch_no_device");
    drop(world);

    let several = root.join("several");
    let bin = fakes(
        &several,
        "AAA\tdevice model:One\nBBB\tdevice model:Two\nCCC\tunauthorized",
        false,
    );
    let mut world = editor_world(&project, &bin);
    let err = call(&mut world, "editor.launch", json!({ "target": "android" })).expect_err("ambiguous");
    assert_eq!(code(&err), "launch_device_ambiguous");
    let err = call(
        &mut world,
        "editor.launch",
        json!({ "target": "android", "device": "ZZZ" }),
    )
    .expect_err("unknown serial");
    assert_eq!(code(&err), "launch_no_device");
    let err = call(
        &mut world,
        "editor.launch",
        json!({ "target": "android", "device": "CCC" }),
    )
    .expect_err("an unauthorized device is not usable");
    assert_eq!(code(&err), "launch_no_device");
    let err = call(&mut world, "editor.launch", json!({ "target": "console" })).expect_err("bad target");
    assert_eq!(code(&err), "launch_bad_target");
    assert!(
        !argv(&several).iter().any(|c| c.contains(" install ")),
        "nothing was installed"
    );
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_second_launch_while_one_runs_is_refused_until_stop() {
    let root = regia_test_utils::unique_temp_dir("launch-busy");
    let project = project(&root);
    let bin = fakes(&root, "", false);
    let mut world = editor_world(&project, &bin);
    call(&mut world, "editor.launch", json!({ "target": "standalone" })).expect("launch");
    let err = call(&mut world, "editor.launch", json!({ "target": "standalone" })).expect_err("busy");
    assert_eq!(code(&err), "launch_busy");
    assert!(wait_line(&world, "fake game: hello"));
    call(&mut world, "editor.launch_stop", json!({})).expect("stop");
    let game = read_pid(&root.join("game.pid"));
    assert!(!alive(game));
    // After Stop a new launch is accepted (and dropping the editor stops it).
    call(&mut world, "editor.launch", json!({ "target": "standalone" })).expect("relaunch");
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn launch_without_a_project_is_refused() {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    let mut world = std::mem::take(app.world_mut());
    let err = call(&mut world, "editor.launch", json!({ "target": "standalone" })).expect_err("no project");
    assert_eq!(code(&err), "launch_no_project");
    let state = call(&mut world, "editor.launch_status", json!({})).expect("status");
    assert_eq!(state["state"], "idle");
}

/// Whether a held fake step's process and its grandchild are both gone (Stop killed the tree and
/// reaped the child). The child is reaped before Stop answers; the grandchild was SIGKILLed with it,
/// but the kernel retires a killed process asynchronously (it must be scheduled once more), so its
/// state is re-read until it is gone. The bound is a hang guard for a kill that never came, not a
/// timing assert.
fn step_tree_gone(root: &Path, name: &str) -> bool {
    let child = read_pid(&root.join(format!("{name}.pid")));
    let grandchild = read_pid(&root.join(format!("{name}-grandchild.pid")));
    if alive(child) {
        return false;
    }
    #[cfg(target_os = "linux")]
    let gone = || not_running(grandchild);
    #[cfg(not(target_os = "linux"))]
    let gone = || !alive(grandchild);
    let guard = std::time::Instant::now() + HANG_GUARD;
    while !gone() {
        if std::time::Instant::now() > guard {
            return false;
        }
        std::thread::yield_now();
        std::thread::sleep(Duration::from_millis(2));
    }
    true
}

/// Every adb call that reached the device shell (`adb -s <serial> shell ...`).
fn shell_calls(root: &Path) -> Vec<String> {
    argv(root)
        .into_iter()
        .filter(|c| c.starts_with("adb ") && c.contains(" shell "))
        .collect()
}

/// ADR-7548: the Android application id comes from `Regia.toml` and ends up on the device's `sh`
/// command line (`adb shell` joins its arguments). An id outside the package-name grammar is
/// refused at launch with `launch_bad_app_id`, before any adb call, and so a Stop has nothing to
/// force-stop either.
#[test]
fn an_app_id_with_shell_characters_is_refused_and_never_reaches_the_device_shell() {
    let ids = [
        "com.x.game;am force-stop com.android.chrome",
        "com.x.game$(reboot)",
        "com.x.game`id`",
        "com.x.ga me",
        "com.x.\"game\"",
        "com.x.'game'",
        "com.x.game&&pm clear com.android.chrome",
        "com.x.game|sh",
        "com.x.game\nreboot",
        "com.x.game>/sdcard/x",
    ];
    for (i, id) in ids.iter().enumerate() {
        let root = regia_test_utils::unique_temp_dir(&format!("launch-bad-id-{i}"));
        let project = project_with_id(&root, id);
        let bin = fakes(&root, "FAKE123\tdevice model:Fake_Phone", false);
        // Hold the build, so a launch that was wrongly accepted is still building at Stop.
        hold(&root, "package");
        let mut world = editor_world(&project, &bin);
        let launched = call(&mut world, "editor.launch", json!({ "target": "android" }));
        let stopped = call(&mut world, "editor.launch_stop", json!({})).expect("launch_stop");
        let shell = shell_calls(&root);
        assert!(
            shell.is_empty(),
            "id {id:?}: the device shell ran {shell:#?} (launch answered {launched:?})"
        );
        let err = launched.expect_err("an id outside the package-name grammar is refused");
        assert_eq!(code(&err), "launch_bad_app_id", "id {id:?}: {err:?}");
        assert_eq!(stopped["stopped"], false, "id {id:?}: nothing was launched");
        assert!(
            !argv(&root).iter().any(|c| c.starts_with("regia package")),
            "id {id:?}: nothing was packaged"
        );
        drop(world);
        release(&root, "package");
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Stop while the APK is packaging kills the packager's tree and sends no `am force-stop`: the
/// launch never started the app, so an app with that id already on the device (an earlier install)
/// is not its to stop.
#[test]
fn stop_during_android_packaging_kills_the_packager_and_force_stops_nothing() {
    let root = regia_test_utils::unique_temp_dir("launch-stop-packaging");
    let project = project(&root);
    let bin = fakes(&root, "FAKE123\tdevice model:Fake_Phone", false);
    hold(&root, "package");
    let mut world = editor_world(&project, &bin);
    call(&mut world, "editor.launch", json!({ "target": "android" })).expect("launch");
    assert!(wait_line(&world, "package holding"), "the launch ended early");
    assert_eq!(status(&mut world)["state"], "building");

    let stopped = call(&mut world, "editor.launch_stop", json!({})).expect("launch_stop");
    assert_eq!(stopped["stopped"], true, "{stopped:#}");
    assert_eq!(stopped["state"], "stopped");
    assert!(
        step_tree_gone(&root, "package"),
        "Stop killed the packager and its child"
    );
    let calls = argv(&root);
    assert!(
        !calls.iter().any(|c| c.contains("force-stop")),
        "Stop during the build force-stopped an app it never started: {calls:#?}"
    );
    assert!(!calls.iter().any(|c| c.contains(" install ")), "{calls:#?}");
    drop(world);
    release(&root, "package");
    let _ = std::fs::remove_dir_all(&root);
}

/// Stop during `adb install` kills the install's tree and sends no `am force-stop` (the app was
/// never started by this launch); nothing is started afterwards.
#[test]
fn stop_during_android_install_kills_the_install_and_force_stops_nothing() {
    let root = regia_test_utils::unique_temp_dir("launch-stop-install");
    let project = project(&root);
    let bin = fakes(&root, "FAKE123\tdevice model:Fake_Phone", false);
    hold(&root, "install");
    let mut world = editor_world(&project, &bin);
    call(&mut world, "editor.launch", json!({ "target": "android" })).expect("launch");
    assert!(wait_line(&world, "install holding"), "the launch ended early");
    assert_eq!(status(&mut world)["state"], "installing");

    let stopped = call(&mut world, "editor.launch_stop", json!({})).expect("launch_stop");
    assert_eq!(stopped["stopped"], true, "{stopped:#}");
    assert_eq!(stopped["state"], "stopped");
    assert!(
        step_tree_gone(&root, "install"),
        "Stop killed adb install and its child"
    );
    let calls = argv(&root);
    assert!(
        !calls.iter().any(|c| c.contains("force-stop")),
        "Stop during the install force-stopped an app it never started: {calls:#?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains("am start")),
        "nothing starts after Stop: {calls:#?}"
    );
    drop(world);
    release(&root, "install");
    let _ = std::fs::remove_dir_all(&root);
}

/// Stop during the standalone cargo build kills cargo with its children (rustc) and runs no game.
#[test]
fn stop_during_the_standalone_build_kills_cargo_and_its_children() {
    let root = regia_test_utils::unique_temp_dir("launch-stop-cargo");
    let project = project(&root);
    let bin = fakes(&root, "", false);
    hold(&root, "cargo");
    let mut world = editor_world(&project, &bin);
    call(&mut world, "editor.launch", json!({ "target": "standalone" })).expect("launch");
    assert!(wait_line(&world, "cargo holding"), "the launch ended early");
    assert_eq!(status(&mut world)["step"], "cargo build");

    let stopped = call(&mut world, "editor.launch_stop", json!({})).expect("launch_stop");
    assert_eq!(stopped["stopped"], true, "{stopped:#}");
    assert!(step_tree_gone(&root, "cargo"), "Stop killed cargo and its child");
    assert!(!root.join("game.pid").exists(), "no game ran");
    assert_eq!(status(&mut world)["state"], "stopped");
    drop(world);
    release(&root, "cargo");
    let _ = std::fs::remove_dir_all(&root);
}

/// ADR-7548: `editor.launch { wait: true }` runs on the editor thread, so it must not hold the
/// frame for the build. Whatever `timeout_ms` asks, the wait is bounded; a launch still building
/// then answers `pending: true` and the agent follows it with `editor.launch_status`, which keeps
/// answering (the frame ticks) while the build runs. The handler runs on a helper thread here only
/// so that a blocking answer shows as a failure instead of hanging the run.
#[test]
fn launch_wait_never_holds_the_editor_frame_for_a_long_build() {
    let root = regia_test_utils::unique_temp_dir("launch-wait-bounded");
    let project = project(&root);
    let bin = fakes(&root, "", false);
    hold(&root, "cargo");
    let world = editor_world(&project, &bin);

    let (answered_tx, answered_rx) = std::sync::mpsc::channel();
    let handler = std::thread::spawn(move || {
        let mut world = world;
        let answer = call(
            &mut world,
            "editor.launch",
            json!({ "target": "standalone", "wait": true, "timeout_ms": 600_000 }),
        );
        let _ = answered_tx.send(());
        (world, answer)
    });
    let answered = answered_rx.recv_timeout(HANG_GUARD).is_ok();
    if !answered {
        // Let the blocked handler finish, so the run ends and reports.
        release(&root, "cargo");
    }
    let (mut world, answer) = handler.join().expect("the handler thread");
    assert!(
        answered,
        "editor.launch {{ wait: true }} held the editor frame while the build ran: {answer:?}"
    );
    let answer = answer.expect("a building launch is not an error");
    assert_eq!(answer["pending"], true, "{answer:#}");
    assert_eq!(answer["state"], "building", "{answer:#}");
    assert_eq!(answer["running"], true);

    // The editor keeps serving calls (its frame ticks) while the build is held.
    assert!(wait_line(&world, "cargo holding"), "the launch ended early");
    for _ in 0..5 {
        let state = status(&mut world);
        assert_eq!(state["state"], "building", "{state:#}");
        assert_eq!(state["step"], "cargo build", "{state:#}");
    }
    release(&root, "cargo");

    // The released build goes on to run the game, which the agent sees by polling.
    assert!(wait_line(&world, "fake game: hello"), "the launch ended early");
    let state = status(&mut world);
    assert_eq!(state["state"], "running", "{state:#}");
    call(&mut world, "editor.launch_stop", json!({})).expect("stop");
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
}

/// P-301 (ADR-7836): a standalone game gets its own agent directory, and an asset the live cooker
/// rebuilt while it runs is pushed into it with `assets.push` over the real agent client: the
/// endpoint and token are read from that directory, the bytes arrive as base64, and the launch
/// bar's count (`editor.launch_status.pushed`) goes up. The game's side is a loopback agent server
/// whose requests this test answers; the runtime handler has its own dispatcher tests
/// (`regia_agent` `assets_push`).
#[test]
fn a_rebuilt_asset_is_pushed_into_the_running_standalone_game() {
    let root = regia_test_utils::unique_temp_dir("launch-push");
    let project = project(&root);
    let bin = fakes(&root, "", false);
    let mut world = editor_world(&project, &bin);

    let started = call(&mut world, "editor.launch", json!({ "target": "standalone" })).expect("launch");
    let id = u32::try_from(started["id"].as_u64().expect("a launch id")).expect("u32 id");
    let agent_dir = regia_editor::launch::agent_dir_in(&project, id);
    assert!(
        world.resource::<Launcher>().wait_for(
            |s| (s.state == regia_editor::launch::LaunchState::Running && s.tail_contains("agent dir="))
                || !s.state.is_active(),
            HANG_GUARD
        ),
        "the game did not run"
    );
    let state = status(&mut world);
    assert!(
        tail_has(&state, &format!("fake game: agent dir={}", agent_dir.display())),
        "the game runs with REGIA_AGENT_DIR set to the launch's own directory: {state:#}"
    );
    assert_eq!(state["pushed"], 0);

    // The game's agent server: a real loopback listener advertised in the launch's directory.
    let server = regia_agent::server::start("push-token".into()).expect("server starts");
    regia_agent::endpoint::write(
        &agent_dir,
        &regia_agent::protocol::Endpoint {
            url: format!("ws://{}", server.addr),
            pid: std::process::id(),
            protocol: regia_agent::protocol::PROTOCOL_VERSION.to_owned(),
            token_file: agent_dir.join("token").display().to_string(),
            app: "fake-game".to_owned(),
        },
        "push-token",
    )
    .expect("endpoint");
    let inbox = server.inbox.clone();
    let answering = std::thread::spawn(move || {
        let inbound = inbox.recv_timeout(HANG_GUARD).expect("a push reached the game");
        let request_id = inbound.request.id.clone().expect("request id");
        let _ = inbound.reply.send(regia_agent::protocol::Response::ok(
            request_id,
            json!({ "kind": "asset", "changed": true }),
        ));
        (inbound.request.method.clone(), inbound.request.params.clone())
    });

    // The live cooker rebuilt one asset into the editor's cooked cache.
    let asset = regia_asset::AssetId::new_random();
    let cache = regia_asset::cooked_cache_dir(&project, &regia_asset::current_platform());
    std::fs::create_dir_all(&cache).expect("cache");
    let cooked = b"cooked bytes of the rebuilt asset".to_vec();
    std::fs::write(cache.join(regia_asset::cooked_file_name(asset)), &cooked).expect("cooked file");
    let queued = regia_editor::launch::forward_reloads(
        &world,
        &[
            regia_asset::ReloadMessage::Reload(asset),
            regia_asset::ReloadMessage::Reload(asset),
        ],
    );
    assert_eq!(queued, 1, "one push per rebuilt asset");
    assert!(world.resource::<Launcher>().wait_pushes(1, HANG_GUARD));
    let (method, params) = answering.join().expect("answering thread");
    assert_eq!(method, "assets.push");
    assert_eq!(params["asset"], asset.to_string());
    let data = params["data"].as_str().expect("base64 data");
    assert_eq!(
        regia_editor::reference::base64_decode(data).expect("standard base64"),
        cooked
    );
    let state = status(&mut world);
    assert_eq!(state["pushed"], 1, "{state:#}");
    assert_eq!(state["push_failed"], 0, "{state:#}");

    let stopped = call(&mut world, "editor.launch_stop", json!({})).expect("launch_stop");
    assert_eq!(stopped["stopped"], true);
    assert!(!agent_dir.exists(), "Stop removes the launch's agent directory");
    assert_eq!(
        regia_editor::launch::forward_reloads(&world, &[regia_asset::ReloadMessage::Reload(asset)]),
        0,
        "nothing is pushed once the game stopped"
    );
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn named_profile_launch_preserves_level_and_passes_argument_tokens() {
    let root = regia_test_utils::unique_temp_dir("named-profile-launch");
    let project = project(&root);
    std::fs::create_dir_all(project.join("assets/levels")).expect("levels");
    let level = project.join("assets/levels/review.level.ron");
    std::fs::write(&level, b"explicit level remains after stop").expect("level");
    let bin = fakes(&root, "A device", false);
    script(
        &bin.join("fake-game"),
        &format!(
            "printf '%s\\n' \"$@\" > '{}'\necho \"fake game: start level=$REGIA_START_LEVEL\"\nexec sleep 100000\n",
            root.join("game-args").display()
        ),
    );
    let mut world = editor_world(&project, &bin);
    let profile = json!({"name":"Review", "target":"standalone", "build_profile":"shipping", "cook":"skip", "package":false,
        "device":null,"start_level":"levels/review.level.ron","extra_arguments":["--review","two words","literal;$HOME"]});
    call(
        &mut world,
        "editor.launch_profiles",
        json!({"op":"save","profile":profile}),
    )
    .expect("save");
    call(&mut world, "editor.launch", json!({"profile":"Review"})).expect("launch");
    assert!(wait_line(&world, "fake game: start level="));
    let status = status(&mut world);
    assert_eq!(status["steps"], json!(["cargo build", "run"]));
    assert!(argv(&root).iter().any(|line| line.contains("--profile shipping")));
    assert_eq!(
        std::fs::read_to_string(root.join("game-args")).expect("arguments"),
        "--review\ntwo words\nliteral;$HOME\n"
    );
    assert!(
        world
            .resource::<Launcher>()
            .status()
            .expect("status")
            .tail_contains(&level.display().to_string())
    );
    call(&mut world, "editor.launch_stop", json!({})).expect("stop");
    assert!(
        level.is_file(),
        "an explicit start level is never removed as a play copy"
    );
    drop(world);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn android_profile_selects_build_profile_and_refuses_package_off_before_adb() {
    let root = regia_test_utils::unique_temp_dir("android-profile-launch");
    let project = project(&root);
    std::fs::create_dir_all(project.join("assets/levels")).expect("levels");
    std::fs::write(project.join("assets/levels/review.level.ron"), "()").expect("level");
    let bin = fakes(&root, "A device", false);
    let mut world = editor_world(&project, &bin);
    // ADR-8472: a package-off Android launch has nothing current to start; refused before adb.
    let mut profile = json!({"name":"Phone", "target":"android", "build_profile":"shipping", "cook":"incremental", "package":false,
        "device":"A","start_level":"levels/review.level.ron","extra_arguments":[]});
    call(
        &mut world,
        "editor.launch_profiles",
        json!({"op":"save","profile":profile}),
    )
    .expect("save");
    let error = call(&mut world, "editor.launch", json!({"profile":"Phone"})).expect_err("package off");
    assert_eq!(code(&error), "launch_profile_unsupported");
    assert!(argv(&root).is_empty(), "refused before any adb IO");
    // The start level is no longer refused (it rides on the launch intent).
    profile["package"] = json!(true);
    call(
        &mut world,
        "editor.launch_profiles",
        json!({"op":"save","profile":profile}),
    )
    .expect("save");
    call(&mut world, "editor.launch", json!({"profile":"Phone"})).expect("launch");
    assert!(wait_line(&world, "fake logcat line one"));
    assert!(
        argv(&root)
            .iter()
            .any(|line| line.contains("package --platform android --profile shipping"))
    );
    call(&mut world, "editor.launch_stop", json!({})).expect("stop");
    drop(world);
    std::fs::remove_dir_all(root).expect("cleanup");
}

/// A push transport that records each call (method, params) and answers ok.
struct Recorder(
    std::sync::Arc<std::sync::Mutex<Vec<(PathBuf, String, Value)>>>,
    PathBuf,
);

impl regia_editor::launch::PushTransport for Recorder {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.0
            .lock()
            .expect("record")
            .push((self.1.clone(), method.to_owned(), params));
        Ok(json!({ "pushed": true }))
    }
}

/// P-301 / P-342 / P-302 device half (ADR-8472): an Android launch hands the game its profile's
/// start level and arguments and a one-time agent token in one intent extra, forwards a host port
/// to the game's agent port, advertises it in the launch's agent directory (so the remote panels
/// list the device game and the asset pusher reaches it, cooking for android first), and Stop
/// removes the forward and the directory.
#[test]
fn android_launch_hands_the_device_its_options_and_a_transport() {
    let root = regia_test_utils::unique_temp_dir("android-transport");
    let project = project(&root);
    std::fs::create_dir_all(project.join("assets/levels")).expect("levels");
    std::fs::write(project.join("assets/levels/review.level.ron"), "()").expect("level");
    let bin = fakes(&root, "A device", false);
    let mut world = editor_world(&project, &bin);
    let pushes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let cooks = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let mut launcher = world.resource_mut::<Launcher>();
        let record = std::sync::Arc::clone(&pushes);
        launcher.push_connector = Some(std::sync::Arc::new(move |dir: &Path| {
            Ok(
                Box::new(Recorder(std::sync::Arc::clone(&record), dir.to_path_buf()))
                    as Box<dyn regia_editor::launch::PushTransport>,
            )
        }));
        let cooked = std::sync::Arc::clone(&cooks);
        launcher.push_cooker = Some(std::sync::Arc::new(move |_: &Path, platform: &str| {
            cooked.lock().expect("cooks").push(platform.to_owned());
            Ok(())
        }));
    }
    call(
        &mut world,
        "editor.launch_profiles",
        json!({"op":"save","profile":{
            "name":"Phone","target":"android","build_profile":"dev","cook":"incremental","package":true,
            "device":"A","start_level":"levels/review.level.ron","extra_arguments":["--seed","7 8; reboot"]
        }}),
    )
    .expect("save");
    let started = call(&mut world, "editor.launch", json!({"profile":"Phone"})).expect("launch");
    let id = started["id"].as_u64().expect("id") as u32;
    assert!(wait_line(&world, "fake logcat line one"));

    // The options ride on one intent extra: decode what the device got.
    let calls = argv(&root);
    let start = calls
        .iter()
        .find(|c| c.contains("shell am start -n dev.regia.launchtest/.RegiaActivity"))
        .unwrap_or_else(|| panic!("am start in {calls:#?}"));
    let (_, value) = start
        .split_once("--es regia.dev_launch ")
        .unwrap_or_else(|| panic!("the extra in {start}"));
    let options = regia_framework::dev_launch::DevLaunch::decode(value.trim()).expect("decodes");
    assert_eq!(options.start_level.as_deref(), Some("levels/review.level.ron"));
    assert_eq!(options.args, ["--seed", "7 8; reboot"]);
    let agent = options.agent.expect("an agent transport");
    assert_eq!(agent.port, regia_editor::device_transport::device_port(id));
    assert!(regia_framework::dev_launch::check_token(&agent.token).is_ok());

    // The forward, and the host endpoint the panels and the pusher read.
    let forward_at = calls
        .iter()
        .position(|c| c.contains(&format!("adb -s A forward tcp:0 tcp:{}", agent.port)))
        .unwrap_or_else(|| panic!("forward in {calls:#?}"));
    let logcat_at = calls
        .iter()
        .position(|c| c.contains("logcat --pid 4242"))
        .expect("logcat");
    assert!(
        forward_at < logcat_at,
        "the transport opens before logcat streams"
    );
    let state = status(&mut world);
    assert_eq!(state["transport"]["host_port"], 41999, "{state:#}");
    let dir = regia_editor::launch::agent_dir_in(&project, id);
    let (endpoint, token) = regia_agent::endpoint::read(&dir).expect("host endpoint");
    assert_eq!(endpoint.url, "ws://127.0.0.1:41999");
    assert_eq!(endpoint.pid, 4242);
    assert_eq!(
        token, agent.token,
        "the editor reaches the game with the token it handed over"
    );

    // The device game is a remote source.
    let sources = call(&mut world, "editor.remote_sources", json!({})).expect("sources");
    let device = sources["sources"]
        .as_array()
        .and_then(|list| list.iter().find(|s| s["id"] == "device"))
        .unwrap_or_else(|| panic!("a device source in {sources:#}"));
    assert_eq!(device["kind"], "device");
    assert_eq!(device["remote"], true);
    assert!(
        device["label"].as_str().is_some_and(|l| l.contains("(A)")),
        "{device}"
    );

    // A rebuilt texture is cooked for android and pushed into the device game.
    let asset = regia_asset::AssetId::new_random();
    let cache = regia_asset::cooked_cache_dir(&project, "android");
    std::fs::create_dir_all(&cache).expect("cache");
    std::fs::write(
        cache.join(regia_asset::cooked_file_name(asset)),
        b"android-cooked",
    )
    .expect("fckd");
    let launcher = world.resource::<Launcher>();
    assert_eq!(
        launcher.queue_push(vec![regia_editor::launch::PushItem::Cooked(asset)]),
        1
    );
    assert!(launcher.wait_pushes(1, HANG_GUARD), "the push was answered");
    assert_eq!(launcher.push_counts().pushed, 1, "{:?}", launcher.push_counts());
    assert_eq!(*cooks.lock().expect("cooks"), ["android"]);
    {
        let pushes = pushes.lock().expect("pushes");
        let (pushed_dir, method, params) = &pushes[0];
        assert_eq!(
            pushed_dir, &dir,
            "the push goes through the launch's agent directory"
        );
        assert_eq!(method, "assets.push");
        assert_eq!(params["asset"], asset.to_string());
        assert_eq!(params["data"], regia_agent::base64_encode(b"android-cooked"));
    }

    call(&mut world, "editor.launch_stop", json!({})).expect("stop");
    assert!(
        argv(&root)
            .iter()
            .any(|c| c.contains("adb -s A forward --remove tcp:41999")),
        "Stop removes the forward"
    );
    assert!(!dir.exists(), "Stop removes the launch's agent directory");
    let sources = call(&mut world, "editor.remote_sources", json!({})).expect("sources");
    assert!(
        !sources.to_string().contains("\"device\""),
        "no device source after Stop: {sources:#}"
    );
    drop(world);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn desktop_profile_packages_before_starting_the_published_binary() {
    let root = regia_test_utils::unique_temp_dir("desktop-package-profile");
    let project = project(&root);
    let bin = fakes(&root, "", false);
    let bundle = root.join("bundle");
    std::fs::create_dir_all(&bundle).expect("bundle");
    script(
        &bundle.join("launch-launcher"),
        "echo 'published game running'\nexec sleep 100000\n",
    );
    script(
        &bin.join("regia"),
        &format!(
            "echo \"regia $*\" >> '{}'\necho '{{\"ok\":true,\"artifact\":\"{}\"}}'\n",
            root.join("argv.log").display(),
            bundle.display()
        ),
    );
    let mut world = editor_world(&project, &bin);
    call(&mut world,"editor.launch_profiles",json!({"op":"save","profile":{
        "name":"Bundle","target":"standalone","build_profile":"dev","cook":"incremental","package":true,"device":null,"start_level":null,"extra_arguments":[]
    }})).expect("save");
    call(&mut world, "editor.launch", json!({"profile":"Bundle"})).expect("launch");
    assert!(wait_line(&world, "published game running"));
    assert_eq!(status(&mut world)["steps"], json!(["package", "run"]));
    assert!(
        argv(&root)
            .iter()
            .any(|line| line.contains("--format dir --json"))
    );
    call(&mut world, "editor.launch_stop", json!({})).expect("stop");
    drop(world);
    std::fs::remove_dir_all(root).expect("cleanup");
}

/// v06-editor-usability-b item 3 (ADR-8594), on the integrated ADR-8472 transport: an Android
/// launch hands the game a device port and a one-time token, forwards a host port to it, writes
/// the host endpoint into the launch's agent directory and reports it as `agent`; a client that
/// reads only that directory reaches the app's agent server. A REAL loopback `regia_agent` server
/// stands in for the app, started with the token the launch handed the device (the fake `adb
/// forward` holds until it listens, then answers its port). Stop removes the forward and the
/// directory.
#[test]
fn android_launch_attaches_the_apps_agent_through_adb_forward() {
    let root = regia_test_utils::unique_temp_dir("launch-android-agent");
    let project = project(&root);
    let bin = fakes(
        &root,
        "FAKE123\tdevice product:fake model:Fake_Phone device:fake transport_id:1",
        false,
    );
    hold(&root, "forward");
    let mut world = editor_world(&project, &bin);
    let started = call(&mut world, "editor.launch", json!({ "target": "android" })).expect("launch");
    let id = u32::try_from(started["id"].as_u64().expect("a launch id")).expect("id");
    assert!(
        wait_line(&world, "forward holding"),
        "the launch ended before the forward"
    );

    // The app on the device: an agent server with the token the launch handed over.
    let calls = argv(&root);
    let start = calls
        .iter()
        .find(|c| c.contains("shell am start"))
        .unwrap_or_else(|| panic!("am start in {calls:#?}"));
    let (_, value) = start
        .split_once("--es regia.dev_launch ")
        .unwrap_or_else(|| panic!("the extra in {start}"));
    let handed = regia_framework::dev_launch::DevLaunch::decode(value.trim())
        .expect("decodes")
        .agent
        .expect("an agent transport");
    let server = regia_agent::server::start(handed.token.clone()).expect("a loopback agent server");
    let port = server.addr.port();
    let device_agent = root.join("device-agent");
    std::fs::create_dir_all(&device_agent).expect("device agent dir");
    std::fs::write(device_agent.join("forward-port"), format!("{port}\n")).expect("port");
    // The "app": answers one call from the agent inbox.
    let answered = std::thread::spawn(move || {
        let inbound = server
            .inbox
            .recv_timeout(HANG_GUARD)
            .expect("a call reached the app");
        let method = inbound.request.method.clone();
        let id = inbound.request.id.clone().unwrap_or(Value::Null);
        inbound
            .reply
            .send(regia_agent::protocol::Response::ok(
                id,
                json!({ "app": "on the device" }),
            ))
            .expect("reply");
        method
    });
    release(&root, "forward");
    assert!(
        world
            .resource::<Launcher>()
            .wait_for(|s| s.agent.is_some() || !s.state.is_active(), HANG_GUARD),
        "the attach never came"
    );
    assert!(
        world.resource::<Launcher>().wait_for(
            |s| s.state.as_str() == "running" || !s.state.is_active(),
            HANG_GUARD
        ),
        "the launch never reached running"
    );
    let state = status(&mut world);
    assert_eq!(state["state"], "running", "{state:#}");
    let url = format!("ws://127.0.0.1:{port}");
    assert_eq!(state["agent"], json!(url), "{state:#}");
    assert_eq!(state["transport"]["host_port"], port, "{state:#}");

    let dir = regia_editor::launch::agent_dir_in(&project, id);
    assert_eq!(
        regia_editor::launch::device_agent_dir(&world),
        Some((id, "FAKE123".to_owned(), dir.clone())),
        "the remote Outliner and Inspector find the device's agent"
    );
    let (endpoint, token) = regia_agent::endpoint::read(&dir).expect("the forwarded endpoint");
    assert_eq!(endpoint.url, url);
    assert_eq!(endpoint.pid, 4242);
    assert_eq!(token, handed.token);
    let mut client = regia_agent::client::Client::connect(&endpoint.url, &token).expect("connect");
    let answer = client.call("app.ping", json!({})).expect("the app answers");
    assert_eq!(answer["app"], "on the device");
    assert_eq!(answered.join().expect("app thread"), "app.ping");

    let calls = argv(&root);
    let position = |needle: &str| {
        calls
            .iter()
            .position(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("{needle} in {calls:#?}"))
    };
    let start_at = position("adb -s FAKE123 shell am start");
    let forward_at = position(&format!("adb -s FAKE123 forward tcp:0 tcp:{}", handed.port));
    let logcat_at = position("adb -s FAKE123 logcat --pid 4242");
    assert!(start_at < forward_at && forward_at < logcat_at, "{calls:#?}");

    let stopped = call(&mut world, "editor.launch_stop", json!({})).expect("launch_stop");
    assert_eq!(stopped["stopped"], true, "{stopped:#}");
    let calls = argv(&root);
    assert!(
        calls
            .iter()
            .skip(forward_at)
            .any(|c| c.contains(&format!("adb -s FAKE123 forward --remove tcp:{port}"))),
        "Stop removes the forward: {calls:#?}"
    );
    assert!(!dir.exists(), "Stop removes the launch's agent directory");
    assert_eq!(status(&mut world)["agent"], Value::Null);
    assert_eq!(regia_editor::launch::device_agent_dir(&world), None);
    drop(world);
    let _ = std::fs::remove_dir_all(&root);
}
