// SPDX-License-Identifier: MIT-0
//! ADR-7880 in a real window: `display.vsync` and `display.resolution` set over the agent protocol
//! change the live swapchain's present mode and the window size, on the same render device and
//! surface (no re-creation).
//!
//! The sample opens a window, so the run gets a private X server and the software Vulkan driver
//! that can present to it. Without them the test skips. This source must not name the render
//! device types: the gate's GPU pass picks binaries by those words, and a card cannot present to
//! this server.
//!
//! On a device with a real display (the Steam Deck pass), `REGIA_DISPLAY_TEST_NATIVE=1` runs on the
//! session's own display and driver instead, and `SIM_BRIDGE_BIN` names the sample binary when it
//! is not where this test was built.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use regia_agent::client::Client;
use regia_test_utils::process::{PrivateDisplay, Running};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

/// The longest wait for a sample that has stopped making progress: a bound on a hang, not on
/// speed (the first frame on a software adapter under a loaded host takes minutes).
const HANG_BOUND: Duration = Duration::from_secs(600);

/// Where the sample presents: a private X server on the software driver, or (native) the
/// session's own display.
enum Screen {
    Private(PrivateDisplay),
    Native,
}

fn screen() -> Result<Screen, &'static str> {
    if std::env::var_os("REGIA_DISPLAY_TEST_NATIVE").is_some_and(|v| v == "1") {
        return Ok(Screen::Native);
    }
    let drivers = std::fs::read_dir("/usr/share/vulkan/icd.d").map_err(|_| "no Vulkan drivers")?;
    let software = drivers
        .filter_map(Result::ok)
        .any(|driver| driver.file_name().to_string_lossy().starts_with("lvp_icd"));
    if !software {
        return Err("no lavapipe");
    }
    PrivateDisplay::start().map(Screen::Private).ok_or("no Xvfb")
}

fn sample(screen: &Screen, scratch: &Path) -> Command {
    let log = File::create(scratch.join("sample.log")).expect("create the sample log");
    let log_again = log.try_clone().expect("clone the sample log");
    let binary = std::env::var_os("SIM_BRIDGE_BIN").map_or_else(
        || env!("CARGO_BIN_EXE_sim_bridge").into(),
        std::path::PathBuf::from,
    );
    let mut command = Command::new(binary);
    if let Screen::Private(display) = screen {
        display.apply(&mut command);
        command
            .env("HOME", scratch)
            .env("XDG_CONFIG_HOME", scratch.join("config"))
            .env("XDG_DATA_HOME", scratch.join("data"))
            .env("XDG_CACHE_HOME", scratch.join("cache"))
            .env("VK_LOADER_DRIVERS_SELECT", "*lvp*");
    }
    command
        .env("REGIA_AGENT_DIR", scratch.join("agent"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_again));
    command
}

fn log(scratch: &Path) -> String {
    std::fs::read_to_string(scratch.join("sample.log")).unwrap_or_default()
}

/// One agent call. The sample answers between frames, and its first frames on a software adapter
/// under load take longer than the agent's default 30 s answer wait: the call asks for a longer
/// wait (`timeout_ms`) and an answer timeout is retried until [`HANG_BOUND`].
fn call(client: &mut Client, scratch: &Path, method: &str, params: &Value) -> Value {
    let mut params = params.clone();
    if let Some(map) = params.as_object_mut() {
        map.insert("timeout_ms".into(), json!(120_000));
    }
    let deadline = Instant::now() + HANG_BOUND;
    loop {
        match client.call(method, params.clone()) {
            Ok(answer) => return answer,
            Err(err) => assert!(
                Instant::now() < deadline,
                "{method}: {err:?}; log:\n{}",
                log(scratch)
            ),
        }
    }
}

/// Calls `method` until `done` accepts the answer (the change lands one or two frames later).
fn poll(
    client: &mut Client,
    scratch: &Path,
    method: &str,
    params: &Value,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + HANG_BOUND;
    loop {
        let answer = call(client, scratch, method, params);
        if done(&answer) {
            return answer;
        }
        assert!(
            Instant::now() < deadline,
            "{method} never reached the expected state: {answer}; log:\n{}",
            log(scratch)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn vsync_and_resolution_apply_to_the_live_window() {
    let screen = match screen() {
        Ok(screen) => screen,
        Err(why) => {
            eprintln!("skip windowed display settings: {why}");
            return;
        }
    };
    let scratch = unique_temp_dir("display-settings-window");
    let mut running = Running(sample(&screen, &scratch).spawn().expect("start the sample"));
    let agent_dir = scratch.join("agent");
    let deadline = Instant::now() + HANG_BOUND;
    let mut client = loop {
        if let Ok((endpoint, token)) = regia_agent::endpoint::read(&agent_dir)
            && let Ok(client) = Client::connect(&endpoint.url, &token)
        {
            break client;
        }
        if let Ok(Some(status)) = running.0.try_wait() {
            panic!("the sample ended early ({status}); log:\n{}", log(&scratch));
        }
        assert!(
            Instant::now() < deadline,
            "no agent endpoint; log:\n{}",
            log(&scratch)
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    client.set_read_timeout(Some(HANG_BOUND)).expect("read timeout");

    let get_display = json!({ "path": "display.vsync" });
    // The baseline: a surface, vsync on.
    let before = poll(&mut client, &scratch, "settings.get", &get_display, |a| {
        !a["applied"]["display"]["swapchain"].is_null()
    });
    let swapchain = &before["applied"]["display"]["swapchain"];
    assert_eq!(swapchain["vsync"], true, "{before}");
    let lifecycle = call(&mut client, &scratch, "lifecycle.get", &json!({}));
    let created = lifecycle["surfaces"]["created_total"].clone();
    let offers_immediate = swapchain["offers_immediate"] == true;
    let offers_mailbox = swapchain["offers_mailbox"] == true;

    // Vsync off: the swapchain takes Immediate, else Mailbox, else stays on FIFO with a note.
    call(
        &mut client,
        &scratch,
        "settings.set",
        &json!({ "path": "display.vsync", "value": false }),
    );
    let expected = if offers_immediate {
        "Immediate"
    } else if offers_mailbox {
        "Mailbox"
    } else {
        "Fifo"
    };
    let off = poll(&mut client, &scratch, "settings.get", &get_display, |a| {
        a["applied"]["display"]["present_mode"] == "immediate"
            && a["applied"]["display"]["swapchain"]["planned"] == "AutoNoVsync"
    });
    let applied = &off["applied"]["display"];
    assert_eq!(applied["swapchain"]["effective"], expected, "{off}");
    if expected == "Fifo" {
        assert_eq!(applied["vsync_note"], "no_unsynced_mode_offered", "{off}");
    } else {
        assert_eq!(applied["swapchain"]["vsync"], false, "{off}");
        assert_eq!(applied["vsync_note"], Value::Null, "{off}");
    }
    let swapchain = call(&mut client, &scratch, "render.swapchain", &json!({}));
    assert_eq!(
        swapchain["windows"][0]["present_mode_effective"], expected,
        "{swapchain}"
    );

    // Vsync back on.
    call(
        &mut client,
        &scratch,
        "settings.set",
        &json!({ "path": "display.vsync", "value": true }),
    );
    poll(&mut client, &scratch, "settings.get", &get_display, |a| {
        a["applied"]["display"]["swapchain"]["vsync"] == true
            && a["applied"]["display"]["present_mode"] == "vsync"
    });

    // A resolution: the window layer passes it on (the 1280x720 server, or the device's monitor, is
    // the clamp).
    call(
        &mut client,
        &scratch,
        "settings.set",
        &json!({ "path": "display.resolution", "value": [1024, 600] }),
    );
    let sized = poll(
        &mut client,
        &scratch,
        "settings.get",
        &json!({ "path": "display.resolution" }),
        |a| a["applied"]["display"]["applied_logical_size"] == json!([1024.0, 600.0]),
    );
    assert_eq!(sized["applied"]["display"]["refused"], Value::Null, "{sized}");
    // Larger than any screen here: clamped to the monitor.
    call(
        &mut client,
        &scratch,
        "settings.set",
        &json!({ "path": "display.resolution", "value": [4000, 3000] }),
    );
    let clamped = poll(
        &mut client,
        &scratch,
        "settings.get",
        &json!({ "path": "display.resolution" }),
        |a| a["applied"]["display"]["logical_size"] == json!([4000.0, 3000.0]),
    );
    let applied = &clamped["applied"]["display"];
    let monitor = applied["monitor_logical_size"].clone();
    if !monitor.is_null() {
        assert_eq!(applied["applied_logical_size"], monitor, "{clamped}");
    }
    // A size below the minimum: refused with its code, the window keeps the last applied size.
    call(
        &mut client,
        &scratch,
        "settings.set",
        &json!({ "path": "display.resolution", "value": [100, 100] }),
    );
    let refused = poll(
        &mut client,
        &scratch,
        "settings.get",
        &json!({ "path": "display.resolution" }),
        |a| !a["applied"]["display"]["refused"].is_null(),
    );
    assert_eq!(
        refused["applied"]["display"]["refused"]["code"], "window_size_too_small",
        "{refused}"
    );
    assert_eq!(
        refused["applied"]["display"]["applied_logical_size"], applied["applied_logical_size"],
        "a refused size leaves the window as it was"
    );

    // Same surface and device throughout: a present-mode change is a reconfigure.
    let lifecycle = call(&mut client, &scratch, "lifecycle.get", &json!({}));
    assert_eq!(lifecycle["surfaces"]["created_total"], created, "{lifecycle}");

    // What the run saw, for the evidence log (`--nocapture`).
    println!("baseline: {before}\nvsync off: {off}\nclamped: {clamped}\nrefused: {refused}");
    let _ = client.call("quit.request", json!({}));
    drop(running);
    let _ = std::fs::remove_dir_all(&scratch);
}
