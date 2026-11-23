// SPDX-License-Identifier: MPL-2.0
//! The dev transport between the editor and a game it launched on an Android device (ADR-8472).
//!
//! An Android launch (`crate::launch`, target `android`) starts the app with one intent string
//! extra, `regia_framework::dev_launch::EXTRA`, holding the launch's options
//! (`regia_framework::dev_launch::DevLaunch`): the start level and application arguments of the
//! launch profile, and the agent transport: a device loopback port ([`device_port`](crate::device_transport::device_port)) and a one-time
//! token ([`new_launch_token`](crate::device_transport::new_launch_token)) the editor makes for this launch. A dev build's agent server binds
//! that port with that token. Once the app runs, the editor forwards a host port to it
//! (`adb -s <serial> forward tcp:0 tcp:<port>`, [`forward_args`](crate::device_transport::forward_args), [`parse_forward_port`](crate::device_transport::parse_forward_port)) and
//! writes a host endpoint (`ws://127.0.0.1:<host port>`) and the token into the launch's own agent
//! directory ([`write_host_endpoint`](crate::device_transport::write_host_endpoint), `crate::launch::agent_dir_in`). Everything that reaches a
//! standalone launch through that directory then reaches the device: the asset pusher
//! (`assets.push`, ADR-7836) and the remote Outliner and Inspector (`crate::remote_world`, source
//! `device`). Stop removes the forward ([`remove_forward_args`](crate::device_transport::remove_forward_args)) and the directory.
//!
//! Nothing listens on the editor host's network: the forward binds host loopback only, the token
//! never leaves the editor, adb and the app, and a shipping build ignores the extra.

use std::path::Path;

use regia_framework::dev_launch::{self, DevLaunch, DevLaunchError};

/// The first device port a launch's agent server binds; launch `id` uses
/// `DEVICE_PORT_BASE + id % DEVICE_PORT_SPAN` (a relaunch moves off a port a dying app may hold).
pub const DEVICE_PORT_BASE: u16 = 47_800;

/// How many device ports launches cycle through.
pub const DEVICE_PORT_SPAN: u16 = 100;

/// The device loopback port launch `id`'s game binds for its agent server.
#[must_use]
pub fn device_port(id: u32) -> u16 {
    // `id % SPAN` is below 100, so the cast cannot truncate.
    DEVICE_PORT_BASE + (id % u32::from(DEVICE_PORT_SPAN)) as u16
}

/// A new one-time agent token for a launch: 64 lowercase hex digits from two random v4 UUIDs (the
/// agent server's own token shape), so it passes `regia_framework::dev_launch::check_token`.
#[must_use]
pub fn new_launch_token() -> String {
    regia_agent::endpoint::new_token()
}

/// The `adb -s <serial>` arguments that start `component` with `launch`'s options: `shell am start
/// -n <component>`, plus `--es <EXTRA> <value>` when the launch carries any option. The value is one
/// word of `0-9`, `a-z` and `.`, so the device shell (which `adb shell` joins the arguments for)
/// needs no quoting.
///
/// # Errors
///
/// The options do not encode (`DevLaunch::encode`).
pub fn am_start_args(component: &str, launch: &DevLaunch) -> Result<Vec<String>, DevLaunchError> {
    let mut args: Vec<String> = ["shell", "am", "start", "-n", component]
        .into_iter()
        .map(str::to_owned)
        .collect();
    if !launch.is_empty() {
        args.extend(["--es".to_owned(), dev_launch::EXTRA.to_owned(), launch.encode()?]);
    }
    Ok(args)
}

/// `adb -s <serial>` arguments that forward a free host loopback port to `device_port` (adb prints
/// the host port it chose).
#[must_use]
pub fn forward_args(device_port: u16) -> Vec<String> {
    vec!["forward".into(), "tcp:0".into(), format!("tcp:{device_port}")]
}

/// `adb -s <serial>` arguments that remove the forward of `host_port`.
#[must_use]
pub fn remove_forward_args(host_port: u16) -> Vec<String> {
    vec!["forward".into(), "--remove".into(), format!("tcp:{host_port}")]
}

/// The host port `adb forward tcp:0 …` printed (its first non-empty line, a decimal port).
#[must_use]
pub fn parse_forward_port(stdout: &str) -> Option<u16> {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
}

/// Writes the device game's endpoint as the editor host sees it into the launch's agent directory
/// `dir`: `ws://127.0.0.1:<host_port>` (the forward), the app's pid on the device, and the token
/// file. The pusher and the remote panels read it like a standalone game's own endpoint.
///
/// # Errors
///
/// The directory or a file cannot be written.
pub fn write_host_endpoint(
    dir: &Path,
    host_port: u16,
    device_pid: u32,
    app_id: &str,
    token: &str,
) -> std::io::Result<()> {
    regia_agent::endpoint::prepare_dir(dir)?;
    let endpoint = regia_agent::protocol::Endpoint {
        url: format!("ws://127.0.0.1:{host_port}"),
        pid: device_pid,
        protocol: regia_agent::protocol::PROTOCOL_VERSION.into(),
        token_file: dir.join("token").display().to_string(),
        app: format!("android:{app_id}"),
    };
    regia_agent::endpoint::write(dir, &endpoint, token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::dev_launch::DevAgent;

    #[test]
    fn am_start_carries_the_options_as_one_plain_extra() {
        let token = new_launch_token();
        assert!(dev_launch::check_token(&token).is_ok(), "{token}");
        let launch = DevLaunch {
            start_level: Some("levels/b.level.ron".into()),
            args: vec!["--seed".into(), "7 8; reboot".into()],
            agent: Some(DevAgent {
                port: device_port(3),
                token: token.clone(),
            }),
        };
        let args = am_start_args("com.example.game/.RegiaActivity", &launch).expect("encodes");
        assert_eq!(
            args[..7],
            [
                "shell",
                "am",
                "start",
                "-n",
                "com.example.game/.RegiaActivity",
                "--es",
                "regia.dev_launch"
            ]
        );
        assert_eq!(args.len(), 8);
        // Every argument is one shell word: nothing for the device shell to split or expand.
        for arg in &args {
            assert!(
                arg.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b)),
                "{arg}"
            );
        }
        assert_eq!(DevLaunch::decode(&args[7]), Ok(launch));
        // No options, no extra: a plain launch is the old command.
        assert_eq!(
            am_start_args("a.b/.RegiaActivity", &DevLaunch::default()).expect("plain"),
            ["shell", "am", "start", "-n", "a.b/.RegiaActivity"]
        );
        // A bad token never reaches the device.
        let bad = DevLaunch {
            agent: Some(DevAgent {
                port: device_port(1),
                token: "guessable".into(),
            }),
            ..DevLaunch::default()
        };
        assert_eq!(
            am_start_args("a.b/.RegiaActivity", &bad).map_err(|e| e.code()),
            Err("dev_launch_bad_token")
        );
    }

    #[test]
    fn forward_ports_parse_and_device_ports_cycle() {
        assert_eq!(parse_forward_port("41234\n"), Some(41234));
        assert_eq!(parse_forward_port("\n  5555  \n"), Some(5555));
        assert_eq!(parse_forward_port(""), None);
        assert_eq!(parse_forward_port("error: no devices\n"), None);
        assert_eq!(parse_forward_port("0\n"), None);
        assert_eq!(device_port(0), DEVICE_PORT_BASE);
        assert_eq!(device_port(1), DEVICE_PORT_BASE + 1);
        assert_eq!(device_port(u32::from(DEVICE_PORT_SPAN) + 2), DEVICE_PORT_BASE + 2);
        assert!(device_port(u32::MAX) < DEVICE_PORT_BASE + DEVICE_PORT_SPAN);
        assert_eq!(forward_args(47_801), ["forward", "tcp:0", "tcp:47801"]);
        assert_eq!(remove_forward_args(41_234), ["forward", "--remove", "tcp:41234"]);
    }

    #[test]
    fn the_host_endpoint_reads_back_as_a_loopback_endpoint() {
        let dir = regia_test_utils::unique_temp_dir("device_transport_endpoint").join("agent-1");
        let token = new_launch_token();
        write_host_endpoint(&dir, 41_234, 4321, "com.example.game", &token).expect("written");
        let (endpoint, read) = regia_agent::endpoint::read(&dir).expect("reads");
        assert_eq!(endpoint.url, "ws://127.0.0.1:41234");
        assert_eq!(endpoint.pid, 4321);
        assert_eq!(endpoint.app, "android:com.example.game");
        assert_eq!(read, token);
        let _ = std::fs::remove_dir_all(dir.parent().expect("parent"));
    }
}
