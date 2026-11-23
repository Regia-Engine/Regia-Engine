// SPDX-License-Identifier: MPL-2.0
//! `reassembly_stalled` behind a full byte-limited queue (ADR-10125, ADR-10189).
//!
//! `asym-bufferbloat`'s downlink is a 4 Mbit/s bottleneck behind a 500,000-byte drop-tail queue
//! that 4.4 Mbit/s of constant cross traffic keeps full. Such a queue admits a datagram only when
//! the room that drained since the last arrival holds all of it, so it admits by size and by
//! place in a burst: of the server's own packets to one client, the first of a tick gets what
//! room there is, and a packet larger than the ones that leave every tick finds that those have
//! already taken it (a tail-drop queue measured in bytes favors small packets, RFC 7141). A
//! full-size repair of a join snapshot fragment is the largest packet the server sends to that
//! client.
//!
//! - `the_row_that_still_lost_a_client_keeps_every_connection`: the row that lost a client
//!   after the repeat-repair stagger of ADR-10088 (32 clients, seed 2213: client 13's join
//!   snapshot waited 20 repairs of one fragment, each lost, until the client's stall deadline).
//! - `join_snapshots_arrive_well_inside_the_stall_deadline`: the gate rows and the rows of
//!   ADR-8902's measurement, with a bound on the join snapshot's time.
//! - `half_fragments_keep_join_snapshots_inside_half_the_stall_deadline` (ADR-10189): three rows
//!   whose slowest join snapshot is the measure of the repair's size, with a bound at half the
//!   stall deadline.
//! - `join_snapshot_sweep` (ignored: a measurement): rows lost and join snapshot times over
//!   many seeds, the before/after table of the ADR.

use std::sync::Mutex;

use regia_net_soak::profiles::Profile;
use regia_net_soak::workload::{self, RunRecord, Scenario, ServerMode};
use regia_net_soak::{DEFAULT_SEED, GATE_SECONDS};

fn asym_bufferbloat(clients: usize, seed: u64) -> RunRecord {
    let mut profile = Profile::named("asym-bufferbloat").expect("profile");
    profile.fit_to(GATE_SECONDS * 1000);
    let scenario = Scenario {
        profile,
        clients,
        mode: ServerMode::Dedicated,
        seed,
        duration_ms: GATE_SECONDS * 1000,
        wire: Default::default(),
    };
    workload::run(&scenario).expect("run")
}

/// The most a join snapshot may take in these rows, milliseconds: two thirds of the client's
/// reassembly stall deadline on this profile (18.6 s of the server's traffic).
const JOIN_SNAPSHOT_LIMIT_MS: u64 = 12_400;

/// `(client, at ms, side, cause)` of every connection that ended without the script asking.
fn lost(record: &RunRecord) -> Vec<(usize, u64, String, String)> {
    record
        .ends
        .iter()
        .filter(|e| !e.voluntary)
        .map(|e| (e.client, e.at_ms, e.side.clone(), e.cause.clone()))
        .collect()
}

/// Milliseconds from connected to the join snapshot's arrival, every connection of the run that
/// got one, and the number of connections that never did.
fn join_snapshot_ms(record: &RunRecord) -> (Vec<u64>, usize) {
    let mut ms = Vec::new();
    let mut missing = 0;
    for c in &record.clients {
        ms.extend(c.join_snapshots.iter().map(|&(from, at)| at - from));
        missing += c.connected.len().saturating_sub(c.join_snapshots.len());
    }
    (ms, missing)
}

/// 32 clients at seed 2213: after its one-round-trip resume at 21 s, client 13 waited for one
/// fragment of its join snapshot until its reassembly stall deadline (18.6 s of the server's
/// traffic) and ended with `protocol_error`; the server had repaired that fragment 20 times.
/// Every repair left in a tick whose room at the queue the server's own state packets had
/// already brought under a full packet. No connection may be lost, every join snapshot arrives,
/// and none takes longer than two thirds of the stall deadline.
#[test]
fn the_row_that_still_lost_a_client_keeps_every_connection() {
    let record = asym_bufferbloat(32, 2213);
    let lost = lost(&record);
    assert!(lost.is_empty(), "connections lost: {lost:?}");
    let (ms, missing) = join_snapshot_ms(&record);
    assert_eq!(missing, 0, "every connection got its join snapshot");
    let worst = ms.iter().copied().max().unwrap_or(0);
    assert!(
        worst <= JOIN_SNAPSHOT_LIMIT_MS,
        "the slowest join snapshot took {worst} ms of {ms:?}"
    );
}

/// The gate row and the two rows of ADR-8902's measurement, with the same bound on the join
/// snapshot: none of them may come near the stall deadline either.
#[test]
fn join_snapshots_arrive_well_inside_the_stall_deadline() {
    for (clients, seed) in [(8usize, DEFAULT_SEED), (32, DEFAULT_SEED), (32, 44), (32, 55)] {
        let record = asym_bufferbloat(clients, seed);
        let lost = lost(&record);
        assert!(
            lost.is_empty(),
            "{clients} clients, seed {seed}: connections lost: {lost:?}"
        );
        let (ms, missing) = join_snapshot_ms(&record);
        assert_eq!(
            missing, 0,
            "{clients} clients, seed {seed}: every join snapshot arrived"
        );
        let worst = ms.iter().copied().max().unwrap_or(0);
        assert!(
            worst <= JOIN_SNAPSHOT_LIMIT_MS,
            "{clients} clients, seed {seed}: the slowest join snapshot took {worst} ms"
        );
    }
}

/// The most a join snapshot may take once a lost fragment is repaired in half a packet
/// (ADR-10189), milliseconds: half of the client's reassembly stall deadline on this profile.
const HALF_FRAGMENT_JOIN_LIMIT_MS: u64 = 9_300;

/// A full-size repair finds room at this queue about one time in two, so with full fragments the
/// slowest join snapshot of a row depended on how often one fragment lost its turn: 9,568 ms at
/// 32 clients and seed 55, 10,720 ms at 8 clients and seed 2213 (ADR-10125's head, the 200-seed
/// sweep). A message cut into half fragments is repaired in half a packet, which the queue
/// admits about nine times in ten: those two rows, and the slowest row of the same sweep with
/// half fragments (32 clients, seed 286: 7,328 ms), keep every join snapshot inside half the
/// stall deadline, and lose nobody.
#[test]
fn half_fragments_keep_join_snapshots_inside_half_the_stall_deadline() {
    for (clients, seed) in [(32usize, 55u64), (8, 2213), (32, 286)] {
        let record = asym_bufferbloat(clients, seed);
        let lost = lost(&record);
        assert!(
            lost.is_empty(),
            "{clients} clients, seed {seed}: connections lost: {lost:?}"
        );
        let (ms, missing) = join_snapshot_ms(&record);
        assert_eq!(
            missing, 0,
            "{clients} clients, seed {seed}: every join snapshot arrived"
        );
        let worst = ms.iter().copied().max().unwrap_or(0);
        assert!(
            worst <= HALF_FRAGMENT_JOIN_LIMIT_MS,
            "{clients} clients, seed {seed}: the slowest join snapshot took {worst} ms"
        );
    }
}

/// Nearest-rank percentile of a sorted, non-empty list.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    let rank = (sorted.len() as u64 * p).div_ceil(100).max(1) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// Measurement: `asym-bufferbloat` at 8 and 32 clients over `STALL_SWEEP_SEEDS` seeds (default
/// 48: the gate seed, 2213 and 46 more), in virtual time. Prints one `ROW` line per run and a
/// summary: rows that lost a connection, join snapshots that never arrived, and the join
/// snapshot time (p50 / p99 / max).
///
/// `cargo test --release -p regia_net_soak --test reassembly_stall -- --ignored --nocapture`
#[test]
#[ignore = "measurement for ADR-10125: run with --ignored --nocapture"]
fn join_snapshot_sweep() {
    let count: u64 = std::env::var("STALL_SWEEP_SEEDS")
        .ok()
        .map_or(48, |v| v.parse().expect("a seed count"));
    let mut seeds = vec![DEFAULT_SEED, 2213];
    seeds.extend((1..).map(|i| i * 11).take(count.saturating_sub(2) as usize));
    let mut jobs: Vec<(usize, u64)> = Vec::new();
    for &seed in &seeds {
        for clients in [8usize, 32] {
            jobs.push((clients, seed));
        }
    }
    let next = Mutex::new(0usize);
    let out: Mutex<Vec<(usize, u64, RunRecord)>> = Mutex::new(Vec::new());
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = {
                        let mut n = next.lock().expect("counter");
                        *n += 1;
                        *n - 1
                    };
                    let Some(&(clients, seed)) = jobs.get(i) else {
                        break;
                    };
                    let record = asym_bufferbloat(clients, seed);
                    out.lock().expect("results").push((clients, seed, record));
                }
            });
        }
    });
    let mut results = out.into_inner().expect("results");
    results.sort_by_key(|(clients, seed, _)| (*clients, *seed));
    let (mut rows_lost, mut all_ms, mut all_missing) = (0, Vec::new(), 0);
    for (clients, seed, record) in &results {
        let lost = lost(record);
        let (mut ms, missing) = join_snapshot_ms(record);
        ms.sort_unstable();
        rows_lost += usize::from(!lost.is_empty());
        all_missing += missing;
        println!(
            "ROW clients={clients} seed={seed} lost={lost:?} joins={} missing={missing} join_ms p50={} max={}",
            ms.len(),
            if ms.is_empty() { 0 } else { percentile(&ms, 50) },
            ms.last().copied().unwrap_or(0),
        );
        all_ms.extend(ms);
    }
    all_ms.sort_unstable();
    println!(
        "SUMMARY rows={} rows_lost={rows_lost} joins={} missing={all_missing} join_ms p50={} p90={} p99={} max={}",
        results.len(),
        all_ms.len(),
        percentile(&all_ms, 50),
        percentile(&all_ms, 90),
        percentile(&all_ms, 99),
        all_ms.last().copied().unwrap_or(0),
    );
}
