// SPDX-License-Identifier: MPL-2.0
//! Measurement, not a test of a bar: the per-profile resume times of `docs/perf/net-resume-wire6.md`
//! (wire 6, ADR-10088). Every replication row of the gate plan runs twice per seed, in virtual
//! time: as the plan has it, and with the server keeping reconnect tickets
//! (`Profile::resume_grace_ms`), so the scripted rejoin of every profile is a one-round-trip
//! ticket resume. The time is from the instant the rejoining client's link is up to the instant it
//! is connected, the number the `reconnect_within_grace` bar reads.
//!
//! `cargo test --release -p regia_net_soak --test resume_time_table -- --ignored --nocapture`
//! prints the table as Markdown. `RESUME_TABLE_SEEDS=1,2,3` chooses the seeds (default: the gate
//! seed and five more).

use std::collections::BTreeMap;
use std::sync::Mutex;

use regia_net_soak::DEFAULT_SEED;
use regia_net_soak::workload::{self, Scenario};

/// What one run of one row gave: the scripted rejoin times, and how the server admitted them.
#[derive(Debug, Default, Clone)]
struct Cell {
    rejoin_ms: Vec<u64>,
    unconnected: u64,
    one_rtt: u64,
    involuntary_ends: u64,
    runs: u64,
}

/// Nearest-rank percentile of a sorted, non-empty list.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    let rank = (sorted.len() as u64 * p).div_ceil(100).max(1) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

fn stats(cell: &Cell) -> String {
    let mut ms = cell.rejoin_ms.clone();
    ms.sort_unstable();
    if ms.is_empty() {
        return "none".to_owned();
    }
    format!(
        "{} / {} / {}",
        percentile(&ms, 50),
        percentile(&ms, 99),
        ms[ms.len() - 1]
    )
}

#[test]
#[ignore = "measurement for docs/perf/net-resume-wire6.md (ADR-10088): run with --ignored --nocapture"]
fn resume_time_table() {
    let seeds: Vec<u64> = std::env::var("RESUME_TABLE_SEEDS").ok().map_or_else(
        || vec![DEFAULT_SEED, 11, 22, 33, 44, 55],
        |v| v.split(',').map(|s| s.parse().expect("a seed")).collect(),
    );
    // (row order, tickets kept) -> scenario.
    let mut jobs: Vec<(usize, bool, Scenario)> = Vec::new();
    let mut rows: Vec<String> = Vec::new();
    for &seed in &seeds {
        for scenario in regia_net_soak::gate_plan(seed) {
            if scenario.profile.is_lockstep() || scenario.profile.needs.is_some() {
                continue;
            }
            let label = format!(
                "{}/{}/{}",
                scenario.profile.name,
                scenario.clients,
                format!("{:?}", scenario.mode).to_lowercase()
            );
            let row = rows.iter().position(|r| *r == label).unwrap_or_else(|| {
                rows.push(label);
                rows.len() - 1
            });
            let mut kept = scenario.clone();
            kept.profile.resume_grace_ms = Some(60_000);
            jobs.push((row, false, scenario));
            jobs.push((row, true, kept));
        }
    }
    let jobs = Mutex::new(jobs);
    let cells: Mutex<BTreeMap<(usize, bool), Cell>> = Mutex::new(BTreeMap::new());
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let Some((row, kept, scenario)) = jobs.lock().expect("jobs").pop() else {
                        return;
                    };
                    let record = workload::run(&scenario).expect("the run");
                    let mut cells = cells.lock().expect("cells");
                    let cell = cells.entry((row, kept)).or_default();
                    cell.runs += 1;
                    cell.one_rtt += record.server.totals.resumed_one_rtt;
                    cell.involuntary_ends += record.ends.iter().filter(|e| !e.voluntary).count() as u64;
                    for r in record.reconnects.iter().filter(|r| r.kind == "scripted") {
                        match r.connected_ms {
                            Some(at) => cell.rejoin_ms.push(at.saturating_sub(r.link_up_ms)),
                            None => cell.unconnected += 1,
                        }
                    }
                }
            });
        }
    });
    let cells = cells.into_inner().expect("cells");
    println!("Seeds: {seeds:?}. Times in ms, p50 / p99 / max over the scripted rejoins of all seeds.\n");
    println!(
        "| Row | Rejoins | Plan as is: p50 / p99 / max | One-round-trip resumes | Tickets kept: p50 / p99 / max | One-round-trip resumes | Involuntary ends (as is / kept) |"
    );
    println!("|---|---|---|---|---|---|---|");
    let empty = Cell::default();
    for (row, label) in rows.iter().enumerate() {
        let as_is = cells.get(&(row, false)).unwrap_or(&empty);
        let kept = cells.get(&(row, true)).unwrap_or(&empty);
        println!(
            "| {label} | {} | {} | {} | {} | {} | {} / {} |",
            kept.rejoin_ms.len() as u64 + kept.unconnected,
            stats(as_is),
            as_is.one_rtt,
            stats(kept),
            kept.one_rtt,
            as_is.involuntary_ends,
            kept.involuntary_ends
        );
        assert_eq!(as_is.runs, kept.runs, "{label}: both variants ran every seed");
    }
}
