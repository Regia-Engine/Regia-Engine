// SPDX-License-Identifier: MPL-2.0
//! End-to-end forced divergence over real sockets (ADR-8146): one real server process and N real
//! seat processes on loopback UDP. One seat's simulation is perturbed through the test-only fault
//! hook, and the server's desync detector must name that seat and the tick.
//!
//! Needs `--features desync-inject` (the hook is in a build exactly when that feature is on);
//! without it this file holds only the checks that the hook is absent.
//!
//! The processes run in real time; the test waits for them to exit and reads what the server
//! printed. Each process gives up on its own after 90 s, so nothing here sleeps or times.

use std::process::Command;

const BOT: &str = env!("CARGO_BIN_EXE_lockstep_bot");

/// Without the feature the bot refuses the flag: the hook is not in the build.
#[cfg(not(feature = "desync-inject"))]
#[test]
fn the_fault_hook_is_absent_without_the_feature() {
    let out = Command::new(BOT)
        .args(["seat", "--server", "127.0.0.1:9", "--seat", "0", "--ticks", "1"])
        .args(["--inject-desync-at", "5"])
        .output()
        .expect("the bot runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("desync-inject"));
}

/// The hook is in the build exactly when the feature is on, whatever the profile: its guard used
/// to key on `debug_assertions`, so an optimized build with debug assertions had it and a build
/// without them refused the feature.
#[test]
fn the_fault_hook_follows_the_feature_and_nothing_else() {
    assert_eq!(regia_net_soak::DESYNC_INJECT, cfg!(feature = "desync-inject"));
    let out = Command::new(BOT)
        .args(["seat", "--server", "127.0.0.1:9", "--seat", "0", "--ticks", "0"])
        .args(["--inject-desync-at", "5", "--check-flags"])
        .output()
        .expect("the bot runs");
    let refused = String::from_utf8_lossy(&out.stderr).contains("desync-inject");
    assert_eq!(
        !refused,
        cfg!(feature = "desync-inject"),
        "the bot takes --inject-desync-at exactly when the feature is on: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(feature = "desync-inject")]
mod forced {
    use std::net::UdpSocket;
    use std::process::Stdio;

    use regia_net::lockstep::{DesyncVerdict, SeatId, VerdictKind, Voter};

    use super::*;

    /// A UDP port the OS just had free.
    fn free_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0")
            .and_then(|s| s.local_addr())
            .expect("a loopback port")
            .port()
    }

    struct Match {
        verdicts: Vec<DesyncVerdict>,
        located: Vec<(u64, String, u64)>,
        probes: Vec<regia_net::desync::DesyncReport>,
        done: bool,
        stderr: String,
        /// Whether the host process exited with success.
        host_ok: bool,
        /// `(seat, exited with success)` for every seat process.
        seats_ok: Vec<(u16, bool)>,
    }

    fn play(seats: u16, perturbed: u16, at_tick: u64) -> Match {
        let port = free_port().to_string();
        let ticks = "420";
        let host = Command::new(BOT)
            .args([
                "host",
                "--port",
                &port,
                "--seats",
                &seats.to_string(),
                "--ticks",
                ticks,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the host starts");
        let server = format!("127.0.0.1:{port}");
        let mut bots = Vec::new();
        for seat in 0..seats {
            let mut cmd = Command::new(BOT);
            cmd.args([
                "seat",
                "--server",
                &server,
                "--seat",
                &seat.to_string(),
                "--ticks",
                ticks,
            ]);
            if seat == perturbed {
                cmd.args(["--inject-desync-at", &at_tick.to_string()]);
            }
            bots.push(cmd.stderr(Stdio::piped()).spawn().expect("a seat starts"));
        }
        let out = host.wait_with_output().expect("the host exits");
        let mut stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let mut seats_ok = Vec::new();
        for (seat, bot) in bots.into_iter().enumerate() {
            let o = bot.wait_with_output().expect("a seat exits");
            stderr.push_str(&String::from_utf8_lossy(&o.stderr));
            seats_ok.push((seat as u16, o.status.success()));
        }
        let mut m = Match {
            verdicts: Vec::new(),
            located: Vec::new(),
            probes: Vec::new(),
            done: false,
            stderr,
            host_ok: out.status.success(),
            seats_ok,
        };
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(json) = line.strip_prefix("desync ") {
                m.verdicts
                    .push(serde_json::from_str(json).expect("a verdict line"));
            } else if let Some(rest) = line.strip_prefix("located ") {
                let f: Vec<&str> = rest.split(' ').collect();
                m.located.push((
                    f[0].parse().expect("tick"),
                    f[1].to_owned(),
                    f[2].parse().expect("first tick"),
                ));
            } else if let Some(json) = line.strip_prefix("probed ") {
                m.probes.push(serde_json::from_str(json).expect("a probe line"));
            } else if line.starts_with("done ") {
                m.done = true;
            }
        }
        m
    }

    #[test]
    fn a_real_server_and_real_seat_processes_name_the_perturbed_seat_and_tick() {
        for (seats, perturbed) in [(2u16, 1u16), (3, 0), (4, 2)] {
            let m = play(seats, perturbed, 137);
            assert!(m.done, "{seats} seats: the match ran to its end\n{}", m.stderr);
            assert!(m.host_ok, "{seats} seats: the host process failed\n{}", m.stderr);
            // A healthy seat that crashed or gave up would still leave a majority: check each.
            for (seat, ok) in &m.seats_ok {
                assert!(
                    *ok || *seat == perturbed,
                    "{seats} seats: the process of healthy seat {seat} failed\n{}",
                    m.stderr
                );
            }
            let first = m
                .verdicts
                .first()
                .unwrap_or_else(|| panic!("{seats} seats: no verdict\n{}", m.stderr));
            // The observer server is one more voter, so two seats already have a majority.
            assert_eq!(first.kind, VerdictKind::Diverged, "{seats} seats");
            assert_eq!(
                first.diverged,
                [Voter::Seat(SeatId(perturbed))],
                "{seats} seats: the right seat"
            );
            assert_eq!(
                first.tick, 150,
                "{seats} seats: the first checkpoint after tick 137"
            );
            assert_eq!(first.last_agreed, Some(120));
            assert!(!first.host_diverged);
            assert_eq!(
                m.located.first(),
                Some(&(150, format!("seat{perturbed}"), 137)),
                "{seats} seats: the right tick"
            );
            // The probe's second stage names the subsystem across real processes (ADR-8211).
            let probe = m
                .probes
                .first()
                .unwrap_or_else(|| panic!("{seats} seats: no probe report\n{}", m.stderr));
            assert_eq!(
                (
                    probe.voter,
                    probe.first_diverged_tick,
                    probe.last_agreed_tick,
                    probe.subsystem.as_deref()
                ),
                (
                    Voter::Seat(SeatId(perturbed)),
                    Some(137),
                    Some(136),
                    Some("motion")
                ),
                "{seats} seats: the right subsystem"
            );
        }
    }
}
