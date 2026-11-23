// SPDX-License-Identifier: MPL-2.0
//! Input redundancy on the `NetClient` path (spec 120 § 6.1, ADR-8312): unacknowledged-only
//! redundancy from the server's newest contiguous input tick, loss-aware copies, and a history
//! that keeps its inputs across gaps.
//!
//! One client and a 30 Hz server over the loopback net with the crate's conditioner on both
//! directions, 1 ms steps, seeded, virtual time (no wall clock, no sleeps). The client stamps
//! inputs for [`NetClient::due_input_ticks`] (the steered lead of ADR-8077), or, for the legacy
//! loop, `estimated_server_tick + lead` every 16 ms frame.

use std::net::SocketAddr;

use regia_net::prelude::*;
use regia_net::{CryptoMode, LoopbackSocket};

struct Game;
impl NetGame for Game {
    type Message = Vec<u8>;
    type Input = i32;
    type JoinData = ();
    const GAME_ID: &'static str = "test.clock-filters";
    const PROTOCOL_VERSION: u32 = 1;
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

const SERVER: u16 = 7777;
/// Inputs are judged from this long after the start.
const WARMUP_MS: u64 = 3_000;

fn conditioner(seed: u64, delay: u64, jitter: u64, loss: f64) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        seed,
        loss,
        delay_ms: delay,
        jitter_ms: jitter,
        ..ConditionerConfig::default()
    }
}

/// How the client picks the ticks it stamps.
#[derive(Debug, Clone, Copy)]
enum Stamp {
    /// [`NetClient::due_input_ticks`] (every tick once, the steered lead).
    Due,
    /// The pre-ADR-8077 loop: `estimated_server_tick + input_lead_ticks()` every frame, so a
    /// tick repeats when the server clock has not moved and is skipped when it moved two.
    PerFrame,
}

#[derive(Debug, Default)]
struct Outcome {
    missing: u64,
    ticks: u64,
    worst_streak: u64,
    connected: bool,
    bytes_up: u64,
}

impl Outcome {
    fn missing_pct(&self) -> f64 {
        100.0 * self.missing as f64 / self.ticks.max(1) as f64
    }
}

/// One client against a 30 Hz server for `secs` on a symmetric path of `delay` ms one way, up
/// to `jitter` ms extra and `loss` random loss per direction.
fn run(
    secs: u64,
    (delay, jitter, loss): (u64, u64, f64),
    redundancy: usize,
    stamp: Stamp,
    seed: u64,
) -> Outcome {
    let net = LoopbackNet::new();
    let mut server: NetServer<Game, LoopbackSocket> = NetServer::with_secret(
        net.bind(addr(SERVER)).expect("bind"),
        ServerConfig {
            crypto: CryptoMode::Auto,
            conditioner: conditioner(seed.wrapping_mul(2) + 1, delay, jitter, loss),
            ..ServerConfig::default()
        },
        [7; 32],
    );
    let mut client: NetClient<Game, LoopbackSocket> = NetClient::connect(
        net.bind(addr(0)).expect("bind"),
        addr(SERVER),
        ClientConfig {
            crypto: CryptoMode::Auto,
            conditioner: conditioner(seed.wrapping_mul(2) + 2, delay, jitter, loss),
            input_redundancy: redundancy,
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("connect");
    let mut ticker = TickClock::new(30, 8);
    let mut out = Outcome::default();
    let mut streak = 0u64;
    let mut peer = None;
    for now in 0..secs * 1000 {
        client.receive(now);
        while client.poll_event().is_some() {}
        if client.is_connected() && now.is_multiple_of(16) {
            match stamp {
                Stamp::Due => {
                    for tick in client.due_input_ticks(now) {
                        let _ = client.send_input(tick, tick as i32);
                    }
                }
                Stamp::PerFrame => {
                    let tick = client.estimated_server_tick(now) + client.input_lead_ticks();
                    let _ = client.send_input(tick, tick as i32);
                }
            }
        }
        client.flush(now);
        server.receive(now);
        while let Some(event) = server.poll_event() {
            if let ServerEvent::PeerJoined { peer: p, .. } = event {
                peer = Some(p);
            }
        }
        for _ in 0..ticker.advance(1) {
            let inputs = server.advance_tick(|_, _| {});
            if now > WARMUP_MS
                && let Some(p) = peer
            {
                out.ticks += 1;
                if inputs.contains_key(&p) {
                    streak = 0;
                } else {
                    out.missing += 1;
                    streak += 1;
                    out.worst_streak = out.worst_streak.max(streak);
                }
            }
        }
        server.flush(now);
    }
    out.connected = client.is_connected();
    out.bytes_up = client.status().link.map_or(0, |l| l.bytes_sent);
    out
}

fn report(name: &str, redundancy: usize, o: &Outcome) {
    println!(
        "clock-filters profile={name} redundancy={redundancy} connected={} missing={}/{} ({:.3}%) worst_streak={} bytes_up={}",
        o.connected,
        o.missing,
        o.ticks,
        o.missing_pct(),
        o.worst_streak,
        o.bytes_up
    );
}

/// net-soak `lossy-fat-pipe` (spec 120 § 15.1): 20 ms one way, 5 % random loss, no queue to speak
/// of (100 Mbit/s). Spec 120 bar (N207): residual input loss <= 0.1 %, whatever redundancy the
/// game configured: the measured 5 % loss raises a configured redundancy below
/// `loss_aware_copies(0.05)` (3) to it, and an acknowledged input is not repeated.
#[test]
fn lossy_fat_pipe_residual_input_loss_is_at_most_a_thousandth() {
    const LOSSY_FAT_PIPE: (u64, u64, f64) = (20, 0, 0.05);
    for redundancy in [1, 2, 32] {
        let o = run(120, LOSSY_FAT_PIPE, redundancy, Stamp::Due, 7);
        report("lossy-fat-pipe", redundancy, &o);
        assert!(o.connected);
        assert!(
            o.missing_pct() <= 0.1,
            "redundancy {redundancy}: {:.3} % of ticks missed input (bar 0.1 %)",
            o.missing_pct()
        );
    }
}

/// The pre-ADR-8077 loop stamps the same tick on two frames and skips a tick when the clock
/// moves two: the history keeps its inputs across both (spec 120 § 6.1; at the base each repeat
/// or skip threw the run away, so redundancy was lost exactly when the network jittered).
#[test]
fn a_per_frame_stamp_keeps_its_redundancy_through_repeats_and_skips() {
    for (name, path, bar) in [
        ("lossy-fat-pipe", (20, 0, 0.05), 0.1),
        ("bad-wifi", (30, 60, 0.05), 2.0),
    ] {
        let o = run(60, path, 32, Stamp::PerFrame, 11);
        report(name, 32, &o);
        assert!(o.connected, "{name}");
        assert!(
            o.missing_pct() <= bar,
            "{name}: {:.3} % of ticks missed input (bar {bar} %)",
            o.missing_pct()
        );
    }
}

/// The E3 bars (spec 120 § 15.2 `missed_input`) still hold with unacked-only redundancy at the
/// default and at a low configured redundancy.
#[test]
fn missed_input_bars_hold_with_unacked_only_redundancy() {
    for (name, path, bar) in [
        ("good-wifi", (15, 10, 0.01), 0.5),
        ("transatlantic", (45, 5, 0.005), 0.5),
        ("bad-wifi", (30, 60, 0.05), 2.0),
        ("mobile-4g", (40, 80, 0.03), 2.0),
    ] {
        for redundancy in [2, 32] {
            let o = run(40, path, redundancy, Stamp::Due, 4);
            report(name, redundancy, &o);
            assert!(o.connected, "{name}");
            assert!(
                o.missing_pct() <= bar,
                "{name} redundancy {redundancy}: {:.2} % of ticks missed input (bar {bar} %)",
                o.missing_pct()
            );
        }
    }
}
