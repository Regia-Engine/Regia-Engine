// SPDX-License-Identifier: MPL-2.0
//! Soak profiles for the unified transport policy (ADR-7995, spec 120 § 15): one 60 Hz game
//! workload over seeded virtual-time links, one table row per profile.
//!
//! The workload per tick: the client sends one 40-byte input (unreliable); the server sends three
//! 400-byte state frames (unreliable); every tenth tick each side sends one reliable message; at
//! five seconds the server starts a 256 KiB reliable transfer (a join snapshot). Each row prints
//! throughput, measured against injected loss, input latency and resend counts, so the same file
//! run on an older commit gives the "before" numbers. It uses only `Connection` and the crate's
//! `NetConditioner`: no sleeps, no wall-clock asserts.
use std::net::SocketAddr;

use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::connection::{Channel, Connection, LinkConfig};

const INPUT: u8 = 1;
const STATE: u8 = 2;
const RPC: u8 = 3;
const BULK: u8 = 4;
const BULK_MESSAGES: u32 = 256;

fn addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 1))
}

fn message(kind: u8, index: u32, now: u64, len: usize) -> Vec<u8> {
    let mut m = vec![0u8; len.max(13)];
    m[0] = kind;
    m[1..5].copy_from_slice(&index.to_le_bytes());
    m[5..13].copy_from_slice(&now.to_le_bytes());
    m
}

fn parse(m: &[u8]) -> (u8, u32, u64) {
    let index = u32::from_le_bytes([m[1], m[2], m[3], m[4]]);
    let mut at = [0u8; 8];
    at.copy_from_slice(&m[5..13]);
    (m[0], index, u64::from_le_bytes(at))
}

struct Profile {
    name: &'static str,
    link: ConditionerConfig,
    /// Two-way outages: (start, length) in milliseconds.
    outages: Vec<(u64, u64)>,
    seconds: u64,
}

#[derive(Default)]
struct Row {
    inputs_sent: u64,
    inputs_got: u64,
    input_latency: Vec<u64>,
    states_sent: u64,
    states_got: u64,
    rpc_latency: Vec<u64>,
    rpc_sent: u64,
    bulk_done_ms: Option<u64>,
    injected: u64,
    offered: u64,
    counted_lost: u64,
    packets_sent: u64,
    resends: u64,
    recoveries: u64,
    server_bytes: u64,
    reliable_in_order: bool,
}

fn percentile(values: &mut [u64], p: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[(values.len() - 1) * p / 100]
}

fn run(profile: &Profile) -> Row {
    run_seeded(profile, 11, 12)
}

/// One profile over links seeded `up_seed` (client to server) and `down_seed`.
fn run_seeded(profile: &Profile, up_seed: u64, down_seed: u64) -> Row {
    let mut client = Connection::new(LinkConfig::default());
    let mut server = Connection::new(LinkConfig::default());
    let mut up = NetConditioner::new(ConditionerConfig {
        seed: up_seed,
        ..profile.link.clone()
    });
    let mut down = NetConditioner::new(ConditionerConfig {
        seed: down_seed,
        ..profile.link.clone()
    });
    let mut row = Row {
        reliable_in_order: true,
        ..Row::default()
    };
    let ticks = profile.seconds * 60;
    // Traffic stops ten seconds before the end so every profile can drain.
    let quiet_from = ticks.saturating_sub(600);
    let mut bulk_queued = 0u32;
    let (mut client_rpc, mut server_rpc) = (0u32, 0u32);
    let (mut next_client_rpc, mut next_server_rpc, mut next_bulk) = (0u32, 0u32, 0u32);
    for tick in 0..ticks {
        let now = tick * 1000 / 60;
        let out = profile
            .outages
            .iter()
            .any(|(start, len)| now >= *start && now < start + len);
        if tick < quiet_from {
            if client
                .send(Channel::Unreliable, message(INPUT, tick as u32, now, 40), 0)
                .is_ok()
            {
                row.inputs_sent += 1;
            }
            for _ in 0..3 {
                if server
                    .send(Channel::Unreliable, message(STATE, tick as u32, now, 400), 0)
                    .is_ok()
                {
                    row.states_sent += 1;
                }
            }
            if tick % 10 == 0 {
                if client
                    .send(Channel::GAME_RPC, message(RPC, client_rpc, now, 100), 0)
                    .is_ok()
                {
                    client_rpc += 1;
                    row.rpc_sent += 1;
                }
                if server
                    .send(Channel::GAME_RPC, message(RPC, server_rpc, now, 200), 0)
                    .is_ok()
                {
                    server_rpc += 1;
                    row.rpc_sent += 1;
                }
            }
        }
        if tick >= 300 {
            while bulk_queued < BULK_MESSAGES
                && server
                    .send(Channel::GAME_RPC, message(BULK, bulk_queued, 5_000, 1024), 0)
                    .is_ok()
            {
                bulk_queued += 1;
            }
        }
        for p in client.poll(now, &[]) {
            row.offered += 1;
            if out {
                row.injected += 1;
                continue;
            }
            let before = up.stats().dropped;
            for (p, _) in up.process(p, addr(), now) {
                server.receive(&p, now).expect("packet");
            }
            row.injected += up.stats().dropped - before;
        }
        for (p, _) in up.release_due(now) {
            server.receive(&p, now).expect("packet");
        }
        for p in server.poll(now, &[]) {
            row.offered += 1;
            if out {
                row.injected += 1;
                continue;
            }
            let before = down.stats().dropped;
            for (p, _) in down.process(p, addr(), now) {
                client.receive(&p, now).expect("packet");
            }
            row.injected += down.stats().dropped - before;
        }
        for (p, _) in down.release_due(now) {
            client.receive(&p, now).expect("packet");
        }
        while let Some((channel, m)) = server.next_message() {
            let (kind, index, at) = parse(&m);
            match (channel, kind) {
                (Channel::Unreliable, INPUT) => {
                    row.inputs_got += 1;
                    row.input_latency.push(now - at);
                }
                (Channel::GAME_RPC, RPC) => {
                    row.reliable_in_order &= index == next_client_rpc;
                    next_client_rpc += 1;
                    row.rpc_latency.push(now - at);
                }
                _ => row.reliable_in_order = false,
            }
        }
        while let Some((channel, m)) = client.next_message() {
            let (kind, index, at) = parse(&m);
            match (channel, kind) {
                (Channel::Unreliable, STATE) => row.states_got += 1,
                (Channel::GAME_RPC, RPC) => {
                    row.reliable_in_order &= index == next_server_rpc;
                    next_server_rpc += 1;
                    row.rpc_latency.push(now - at);
                }
                (Channel::GAME_RPC, BULK) => {
                    row.reliable_in_order &= index == next_bulk;
                    next_bulk += 1;
                    if next_bulk == BULK_MESSAGES {
                        row.bulk_done_ms = Some(now - 5_000);
                    }
                }
                _ => row.reliable_in_order = false,
            }
        }
    }
    row.reliable_in_order &= next_client_rpc == client_rpc && next_server_rpc == server_rpc;
    let (c, s) = (client.stats(), server.stats());
    row.counted_lost = c.packets_lost + s.packets_lost;
    row.packets_sent = c.packets_sent + s.packets_sent;
    row.resends = c.reliable_resends + s.reliable_resends;
    row.recoveries = c.recoveries + s.recoveries;
    row.server_bytes = s.bytes_sent;
    row
}

fn report(profile: &Profile, row: &mut Row) {
    let p50 = percentile(&mut row.input_latency, 50);
    let p95 = percentile(&mut row.input_latency, 95);
    let rpc95 = percentile(&mut row.rpc_latency, 95);
    println!(
        "SOAK profile={} seconds={} server_kbps={} inputs={}/{} states={}/{} input_p50_ms={p50} input_p95_ms={p95} \
         rpc_p95_ms={rpc95} rpc={}/{} bulk_ms={} lost_counted={} lost_injected={} offered={} resends={} recoveries={} in_order={}",
        profile.name,
        profile.seconds,
        row.server_bytes * 8 / 1000 / profile.seconds,
        row.inputs_got,
        row.inputs_sent,
        row.states_got,
        row.states_sent,
        row.rpc_latency.len(),
        row.rpc_sent,
        row.bulk_done_ms
            .map_or_else(|| "unfinished".to_owned(), |ms| ms.to_string()),
        row.counted_lost,
        row.injected,
        row.offered,
        row.resends,
        row.recoveries,
        row.reliable_in_order
    );
}

fn link(
    delay_ms: u64,
    jitter_ms: u64,
    loss: f64,
    reorder: f64,
    reorder_max_ms: u64,
    duplicate: f64,
) -> ConditionerConfig {
    ConditionerConfig {
        enabled: true,
        delay_ms,
        jitter_ms,
        loss,
        reorder,
        reorder_max_ms,
        duplicate,
        ..Default::default()
    }
}

fn profiles() -> Vec<Profile> {
    let good_wifi = link(4, 4, 0.002, 0.001, 10, 0.0005);
    vec![
        Profile {
            name: "clean-100ms",
            link: link(50, 0, 0.0, 0.0, 0, 0.0),
            outages: vec![],
            seconds: 40,
        },
        Profile {
            name: "good-wifi",
            link: good_wifi.clone(),
            outages: vec![],
            seconds: 60,
        },
        Profile {
            name: "wan",
            link: ConditionerConfig::from_profile("wan").expect("profile"),
            outages: vec![],
            seconds: 60,
        },
        Profile {
            name: "wan_stress",
            link: ConditionerConfig::from_profile("wan_stress").expect("profile"),
            outages: vec![],
            seconds: 60,
        },
        Profile {
            name: "flappy",
            link: good_wifi.clone(),
            outages: (0..5).map(|i| (15_000 + i * 20_000, 3_000)).collect(),
            seconds: 120,
        },
        Profile {
            name: "outage-25s",
            link: good_wifi,
            outages: vec![(10_000, 25_000)],
            seconds: 70,
        },
        Profile {
            name: "rtt-extreme",
            link: link(1_500, 0, 0.2, 0.0, 0, 0.0),
            outages: vec![],
            seconds: 180,
        },
    ]
}

/// Every profile: reliable data arrives once and in order, the join snapshot completes, and on
/// links without an outage the loss the transport counts is the loss the link injected.
#[test]
fn soak_profiles_hold_delivery_and_loss_accounting() {
    // Every row is printed before anything is asserted, so a run on another commit still
    // yields the whole table.
    let mut rows: Vec<(Profile, Row)> = profiles()
        .into_iter()
        .map(|profile| {
            let mut row = run(&profile);
            report(&profile, &mut row);
            (profile, row)
        })
        .collect();
    for (profile, row) in &mut rows {
        assert!(row.reliable_in_order, "{}: reliable order", profile.name);
        assert!(
            row.bulk_done_ms.is_some(),
            "{}: the join snapshot completes",
            profile.name
        );
        if profile.outages.is_empty() {
            let slack = 3 + row.injected / 20;
            assert!(
                row.counted_lost <= row.injected + slack && row.counted_lost + slack >= row.injected,
                "{}: counted {} lost, link dropped {}",
                profile.name,
                row.counted_lost,
                row.injected
            );
            assert_eq!(row.recoveries, 0, "{}: no outage, no recovery", profile.name);
        }
        // Inputs are never queued behind reliable data: they arrive within the path delay, its
        // jitter and reordering, plus two ticks of polling.
        let path = profile.link.delay_ms + profile.link.jitter_ms + profile.link.reorder_max_ms;
        let p95 = percentile(&mut row.input_latency, 95);
        assert!(
            p95 <= path + 34,
            "{}: input p95 {p95} ms on a {path} ms path",
            profile.name
        );
    }
}

/// Link seeds of the rtt-extreme tail test: the table's own (0), the next twelve, and the seeds
/// where an earlier build lost the tail (04d9ad838: 27, 55, 75, 98; repairs copied only on
/// packets that left anyway: 45, 61, 92, 100, 102, 104, 159, 191).
const TAIL_SEEDS: [u64; 25] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 27, 45, 55, 61, 75, 92, 98, 100, 102, 104, 159, 191,
];

/// ADR-8400: the rtt-extreme row (3 s round trip, 20 % loss each way) on 25 seeded links. Every
/// reliable message arrives once and in order within the row's 10 s drain, on every seed: a
/// small message lost near the end needs its repairs to get through, and with the game quiet
/// no other traffic carries their redundant copies. The table's one seed cannot show a tail
/// of a few percent; 25 seeds do.
#[test]
fn rtt_extreme_reliable_tail_holds_on_every_seed() {
    let profile = profiles()
        .into_iter()
        .find(|p| p.name == "rtt-extreme")
        .expect("the rtt-extreme row");
    let mut failed = Vec::new();
    for k in TAIL_SEEDS {
        let row = run_seeded(&profile, 11 + 1_000 * k, 12 + 1_000 * k);
        let delivered = row.rpc_latency.len() as u64;
        println!(
            "TAIL seed={k} rpc={delivered}/{} in_order={} bulk_ms={:?} resends={}",
            row.rpc_sent, row.reliable_in_order, row.bulk_done_ms, row.resends
        );
        if !row.reliable_in_order || delivered != row.rpc_sent || row.bulk_done_ms.is_none() {
            failed.push(k);
        }
    }
    assert!(
        failed.is_empty(),
        "rtt-extreme lost reliable messages in the drain on seeds {failed:?}"
    );
}

/// High rate: the largest reliable stream the default settings allow, on a clean 20 ms path and
/// on good-wifi, next to the per-tick game traffic. The stream runs at the poll cap and what is
/// repeated is what the link dropped.
#[test]
fn soak_high_rate_bulk_next_to_game_traffic() {
    for (name, config) in [
        ("high-rate-clean", link(10, 0, 0.0, 0.0, 0, 0.0)),
        ("high-rate-good-wifi", link(4, 4, 0.002, 0.001, 10, 0.0005)),
    ] {
        // Raw transport throughput: more reliable messages per second than a class cap allows
        // (spec 120 § 4.5 A2, ADR-8407), so the caps are off.
        let mut client = Connection::new(LinkConfig::default());
        let mut server = Connection::new(LinkConfig {
            message_caps: false,
            ..LinkConfig::default()
        });
        let mut up = NetConditioner::new(ConditionerConfig {
            seed: 21,
            ..config.clone()
        });
        let mut down = NetConditioner::new(ConditionerConfig { seed: 22, ..config });
        let (mut next, mut got, mut inputs, mut input_latency) = (0u32, 0u32, 0u64, Vec::new());
        let mut in_order = true;
        // Keepalives measure the path for a second before the stream starts.
        for tick in 0..1_860u64 {
            let now = tick * 1000 / 60;
            if (60..1_800).contains(&tick) {
                client
                    .send(Channel::Unreliable, message(INPUT, tick as u32, now, 40), 0)
                    .expect("input");
                while server
                    .send(Channel::GAME_RPC, message(BULK, next, now, 1000), 0)
                    .is_ok()
                {
                    next += 1;
                }
            }
            for p in client.poll(now, &[]) {
                for (p, _) in up.process(p, addr(), now) {
                    server.receive(&p, now).expect("packet");
                }
            }
            for (p, _) in up.release_due(now) {
                server.receive(&p, now).expect("packet");
            }
            for p in server.poll(now, &[]) {
                for (p, _) in down.process(p, addr(), now) {
                    client.receive(&p, now).expect("packet");
                }
            }
            for (p, _) in down.release_due(now) {
                client.receive(&p, now).expect("packet");
            }
            while let Some((_, m)) = server.next_message() {
                inputs += 1;
                input_latency.push(now - parse(&m).2);
            }
            while let Some((_, m)) = client.next_message() {
                in_order &= parse(&m).1 == got;
                got += 1;
            }
        }
        let stats = server.stats();
        let injected = down.stats().dropped;
        let p95 = percentile(&mut input_latency, 95);
        println!(
            "SOAK profile={name} seconds=29 bulk_bytes_per_s={} delivered={got} inputs={inputs}/1740 input_p95_ms={p95} \
             lost_counted={} lost_injected={injected} resends={} recoveries={}",
            u64::from(got) * 1000 / 29,
            stats.packets_lost,
            stats.reliable_resends,
            stats.recoveries
        );
        assert!(in_order, "{name}: reliable order");
        assert!(
            u64::from(got) * 1000 / 29 >= 2_000_000,
            "{name}: at least 16 Mbit/s of reliable data"
        );
        assert!(
            stats.packets_lost <= injected + 5 && stats.packets_lost + 5 >= injected,
            "{name}: counted {} lost, link dropped {injected}",
            stats.packets_lost
        );
        assert!(
            stats.reliable_resends <= injected + 16,
            "{name}: {} resends for {injected} dropped packets",
            stats.reliable_resends
        );
        assert_eq!(stats.recoveries, 0);
    }
}
