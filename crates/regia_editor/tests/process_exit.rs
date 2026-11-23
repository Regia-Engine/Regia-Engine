// SPDX-License-Identifier: MPL-2.0
//! How the real `regia-editor` process ends (ADR-1907): the binary, not the library, because the
//! defects this guards live after the last frame, in the teardown and the exit handlers. Quitting
//! has ended in a signal here before (v0.2.4, a dead display connection).
//!
//! The editor opens a window, so each run gets a private X server, on the software Vulkan driver
//! the server can present from. No such server or driver: the test skips. This source must not
//! name the render device types: the gate's GPU pass picks binaries by those words, and a real
//! card cannot present to this server.

use std::fs::File;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use regia_agent::client::Client;
use regia_test_utils::process::{PrivateDisplay, Running, wait_within};
use regia_test_utils::unique_temp_dir;

/// The longest wait for an editor that has stopped making progress: a bound on a hang, not on
/// speed. The first frame on a software adapter under a loaded gate takes minutes.
const HANG_BOUND: Duration = Duration::from_secs(600);

/// A scratch project and home, removed at the end of the test.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        Self(unique_temp_dir(tag))
    }

    /// What the editor wrote to stdout and stderr.
    fn log(&self) -> String {
        std::fs::read_to_string(self.0.join("editor.log")).unwrap_or_default()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A private X server and the software driver, or why this machine cannot run the test.
fn display() -> Result<PrivateDisplay, &'static str> {
    let drivers = std::fs::read_dir("/usr/share/vulkan/icd.d").map_err(|_| "no Vulkan drivers")?;
    let software = drivers
        .filter_map(Result::ok)
        .any(|driver| driver.file_name().to_string_lossy().starts_with("lvp_icd"));
    if !software {
        return Err("no lavapipe");
    }
    PrivateDisplay::start().ok_or("no Xvfb")
}

/// The editor on `display`, with `scratch` as its project and home: nothing it saves is shared
/// with another run, and its agent endpoint is not in the source tree.
fn editor(display: &PrivateDisplay, scratch: &Path) -> Command {
    let log = File::create(scratch.join("editor.log")).expect("create the editor log");
    let log_again = log.try_clone().expect("clone the editor log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_regia-editor"));
    display.apply(&mut command);
    command
        .arg("--project")
        .arg(scratch)
        .env("HOME", scratch)
        .env("XDG_CONFIG_HOME", scratch.join("config"))
        .env("XDG_DATA_HOME", scratch.join("data"))
        .env("VK_LOADER_DRIVERS_SELECT", "*lvp*")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_again));
    command
}

fn assert_clean_end(status: ExitStatus, scratch: &Scratch) {
    let log = scratch.log();
    assert_eq!(status.code(), Some(0), "{status:?}; log:\n{log}");
    assert!(!log.contains("panicked"), "{log}");
}

#[test]
fn the_editor_ends_with_status_zero_after_its_frames() {
    let display = match display() {
        Ok(display) => display,
        Err(why) => {
            eprintln!("skip editor process test: {why}");
            return;
        }
    };
    let scratch = Scratch::new("editor-exit-frames");
    let child = editor(&display, &scratch.0)
        .args(["--exit-after-frames", "3"])
        .spawn()
        .expect("start the editor");
    let mut running = Running(child);
    let status = wait_within(&mut running.0, HANG_BOUND).expect("wait for the editor");
    assert_clean_end(status, &scratch);
}

#[test]
fn quit_request_ends_the_editor_with_status_zero() {
    let display = match display() {
        Ok(display) => display,
        Err(why) => {
            eprintln!("skip editor process test: {why}");
            return;
        }
    };
    let scratch = Scratch::new("editor-exit-quit");
    let child = editor(&display, &scratch.0).spawn().expect("start the editor");
    let mut running = Running(child);

    let deadline = Instant::now() + HANG_BOUND;
    let mut client = loop {
        if let Ok(client) = Client::connect_project(&scratch.0) {
            break client;
        }
        if let Ok(Some(status)) = running.0.try_wait() {
            panic!("the editor ended early ({status}); log:\n{}", scratch.log());
        }
        assert!(
            Instant::now() < deadline,
            "no agent endpoint within {HANG_BOUND:?}; log:\n{}",
            scratch.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    client.set_read_timeout(Some(HANG_BOUND)).expect("read timeout");
    // The answer may be lost with the connection: the exit status is what counts.
    let _ = client.call("quit.request", serde_json::json!({}));

    let status = wait_within(&mut running.0, HANG_BOUND).expect("wait for the editor");
    assert_clean_end(status, &scratch);
}
