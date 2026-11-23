// SPDX-License-Identifier: MPL-2.0
//! `staleness_p99` on the profiles whose round trip is the point (ADR-10096): `high-rtt`,
//! `rtt-step`, `wifi-to-cell` and `rtt-extreme`, the gate plan's rows at the gate's seed.
//!
//! The bar counts the client's estimate of the server tick minus the tick of its newest snapshot,
//! so the path's one-way latency is inside every sample: a link of 325 ms one way reads 12 ticks
//! at 30 Hz with a perfect sender. The soak tells the two apart (`Measures::staleness_paths`): the
//! same samples with the whole ticks of the path's latency taken off, and the mean of each part
//! (`workload::StalenessParts`). These tests pin the split and what the engine adds on each
//! profile.
//!
//! Each run prints one `staleness-path` line per sub-profile (`--nocapture` shows them). The
//! 32-client rows are measurements for the lane report and are ignored by default.

use std::collections::BTreeMap;

use regia_net_soak::report::{self, ScenarioResult, StalenessPath};
use regia_net_soak::workload::{self, FRAME_MS, Histogram, RunRecord, Scenario};
use regia_net_soak::{DEFAULT_SEED, gate_plan};

/// The gate plan's row `label` at the gate's seed.
fn gate_row(label: &str) -> Scenario {
    gate_plan(DEFAULT_SEED)
        .into_iter()
        .find(|s| s.label() == label)
        .unwrap_or_else(|| panic!("the gate plan has no row {label}"))
}

fn run(label: &str) -> (RunRecord, ScenarioResult) {
    let record = workload::run(&gate_row(label)).expect("run");
    let result = report::evaluate(&record, 0);
    for g in paths(&result) {
        eprintln!(
            "staleness-path {label} {} clients {:?} limit {} p99 {} path {}-{} beyond {} (worst client {}) mean {:.2} = path {:.2} + transit {:.2} + hold {:.2} + rounding {:.2} + estimate {:+.2} over {} samples",
            g.profile,
            g.clients,
            g.limit_ticks,
            g.p99_ticks,
            g.path_ticks_min,
            g.path_ticks_max,
            g.beyond_path_p99_ticks,
            g.worst_client_beyond_path_p99,
            g.mean_ticks,
            g.mean_path_ticks,
            g.mean_transit_ticks,
            g.mean_hold_ticks,
            g.mean_rounding_ticks,
            g.mean_estimate_ticks,
            g.samples
        );
    }
    // Reliable command round trips by sub-profile (send to acknowledgment, in 16 ms frames): the
    // numbers behind "reliable latency" on the slow links.
    let mut by_profile: BTreeMap<&str, (Histogram, u64, u64)> = BTreeMap::new();
    for c in &record.clients {
        let e = by_profile
            .entry(c.profile.as_str())
            .or_insert_with(|| (Histogram::new(512), 0, 0));
        e.0.merge(&c.command_rtt_frames);
        e.1 += c.commands_sent;
        e.2 += c.commands_acked;
    }
    for (profile, (h, sent, acked)) in by_profile {
        eprintln!(
            "reliable-rtt {label} {profile} commands {acked}/{sent} acknowledged, round trip ms p50 {} p95 {} p99 {} max {}",
            h.quantile(0.5) * FRAME_MS,
            h.quantile(0.95) * FRAME_MS,
            h.quantile(0.99) * FRAME_MS,
            h.max * FRAME_MS
        );
    }
    (record, result)
}

fn paths(r: &ScenarioResult) -> &[StalenessPath] {
    &r.measures.as_ref().expect("v2 measures").staleness_paths
}

fn path<'a>(r: &'a ScenarioResult, profile: &str) -> &'a StalenessPath {
    paths(r)
        .iter()
        .find(|g| g.profile == profile)
        .unwrap_or_else(|| panic!("{}: no staleness path of {profile}", r.label))
}

/// The bar's own verdict line of the run.
fn bar_detail(r: &ScenarioResult) -> String {
    r.bars
        .iter()
        .find(|b| b.id == "staleness_p99")
        .map(|b| format!("passed {}: {}", b.passed, b.detail))
        .unwrap_or_default()
}

/// Every sample is split without a remainder, and the split covers exactly the bar's samples.
fn assert_adds_up(record: &RunRecord) {
    for c in &record.clients {
        let p = &c.staleness_parts;
        assert_eq!(
            p.total,
            p.path + p.transit + p.hold + p.rounding + p.estimate,
            "client {}: the parts add up to the bar's samples: {p:?}",
            c.client
        );
        assert_eq!(
            p.samples,
            c.staleness_ticks_link_up.count(),
            "client {}: every link-up sample is split",
            c.client
        );
        assert_eq!(
            p.samples,
            c.staleness_beyond_path_link_up.count(),
            "client {}: every link-up sample has its path taken off",
            c.client
        );
        // Counting in whole ticks costs between 0 and 1 tick a sample.
        assert!(
            p.rounding >= 0 && p.rounding <= p.samples as i64 * 1000,
            "client {}: rounding is within one tick a sample: {p:?}",
            c.client
        );
        assert!(p.hold >= 0, "client {}: {p:?}", c.client);
        // The samples stay under the histogram's last bucket on these rows, so its sum is the total.
        let sum: u64 = c
            .staleness_ticks_link_up
            .buckets
            .iter()
            .enumerate()
            .map(|(v, n)| v as u64 * n)
            .sum();
        if c.staleness_ticks_link_up.max < 255 {
            assert_eq!(p.total, sum as i64 * 1000, "client {}", c.client);
        }
    }
}

/// `high-rtt`: 225 ms or 325 ms one way is 6 or 9 whole ticks at 30 Hz, and the bar's 12 ticks
/// are that path plus less than the limit.
#[test]
fn high_rtt_staleness_is_its_path() {
    let (record, r) = run("high-rtt/8/dedicated");
    assert_adds_up(&record);
    let g = path(&r, "high-rtt");
    assert_eq!((g.path_ticks_min, g.path_ticks_max), (6, 9), "{g:?}");
    assert!(
        g.p99_ticks > g.limit_ticks,
        "the bar counts the path: {g:?}; {}",
        bar_detail(&r)
    );
    assert!(
        g.beyond_path_p99_ticks <= g.limit_ticks && g.worst_client_beyond_path_p99 <= g.limit_ticks,
        "beyond its path high-rtt is within the limit: {g:?}"
    );
    // The mean is the path within the rest of the limit, and the estimate is within a tick of
    // the server's real tick.
    assert!(g.mean_ticks - g.mean_path_ticks <= g.limit_ticks as f64, "{g:?}");
    assert!(g.mean_estimate_ticks.abs() <= 1.0, "{g:?}");
}

/// `rtt-step`: 20 ms, 125 ms from 60 s, 20 ms again from 120 s. The path is 0 or 3 whole ticks;
/// the step itself (the estimate catching up with a round trip that moved by 210 ms) stays inside
/// the limit at the 99th percentile.
#[test]
fn rtt_step_staleness_is_its_path() {
    let (record, r) = run("rtt-step/8/dedicated");
    assert_adds_up(&record);
    let g = path(&r, "rtt-step");
    assert_eq!((g.path_ticks_min, g.path_ticks_max), (0, 3), "{g:?}");
    assert!(
        g.beyond_path_p99_ticks <= g.limit_ticks && g.worst_client_beyond_path_p99 <= g.limit_ticks,
        "beyond its path rtt-step is within the limit: {g:?}"
    );
    assert!(g.mean_estimate_ticks.abs() <= 1.0, "{g:?}");
}

/// `wifi-to-cell`: 15 ms, then 60 ms and 3 % loss after the switch (the 2 s outage is not
/// sampled).
#[test]
fn wifi_to_cell_staleness_is_its_path() {
    let (record, r) = run("wifi-to-cell/8/dedicated");
    assert_adds_up(&record);
    let g = path(&r, "wifi-to-cell");
    assert_eq!((g.path_ticks_min, g.path_ticks_max), (0, 1), "{g:?}");
    assert!(
        g.beyond_path_p99_ticks <= g.limit_ticks && g.worst_client_beyond_path_p99 <= g.limit_ticks,
        "beyond its path wifi-to-cell is within the limit: {g:?}"
    );
}

/// `rtt-extreme`: the seven good-wifi clients are inside the limit as the bar counts it; the
/// extreme peer's 1,500 ms one way is 45 ticks of path.
#[test]
fn rtt_extreme_staleness_is_its_path() {
    let (record, r) = run("rtt-extreme/8/dedicated");
    assert_adds_up(&record);
    let good = path(&r, "good-wifi");
    assert!(good.p99_ticks <= good.limit_ticks, "{good:?}");
    let peer = path(&r, regia_net_soak::profiles::RTT_EXTREME_PEER);
    assert_eq!((peer.path_ticks_min, peer.path_ticks_max), (45, 45), "{peer:?}");
    assert!(peer.p99_ticks >= 45, "the bar counts the path: {peer:?}");
    assert!(
        peer.beyond_path_p99_ticks <= peer.limit_ticks,
        "beyond its path the extreme peer is within the limit: {peer:?}"
    );
    // The peer's tick estimate is on the server's tick from the accept on (ADR-10096): it was a
    // whole one-way delay behind until the connection's first round trip came back.
    assert!(
        peer.mean_estimate_ticks.abs() <= 1.0,
        "the extreme peer's estimate is within a tick of the server's tick on average: {peer:?}"
    );
}

/// The 32-client rows of the four profiles, for the report (about two minutes in a debug build).
#[test]
#[ignore = "a measurement: four 32-client gate rows"]
fn the_32_client_rows_report_their_numbers() {
    for label in [
        "high-rtt/32/dedicated",
        "rtt-step/32/dedicated",
        "wifi-to-cell/32/dedicated",
        "rtt-extreme/32/dedicated",
    ] {
        let (record, _) = run(label);
        assert_adds_up(&record);
    }
}

/// Every snapshot row of the gate plan at 8 clients, for the report.
#[test]
#[ignore = "a measurement: every 8-client gate row"]
fn every_8_client_row_reports_its_numbers() {
    for s in gate_plan(DEFAULT_SEED) {
        if s.clients > 10 || s.profile.name.starts_with("lockstep") || s.profile.name == "relay-path" {
            continue;
        }
        let (record, _) = run(&s.label());
        assert_adds_up(&record);
    }
}

/// The summary names every group over its limit with the path beside it.
#[test]
fn the_summary_tells_the_path_from_what_is_added() {
    let (_, r) = run("wifi-to-cell/8/dedicated");
    let over = paths(&r).iter().any(|g| g.p99_ticks > g.limit_ticks);
    let md = report::SoakReport::new("custom", "", vec![r]).summary_md();
    assert_eq!(
        md.contains("## Staleness over the limit: the path and what is added"),
        over,
        "{md}"
    );
    // A result written before the split loads, with nothing to show.
    let (_, old) = run("high-rtt/8/dedicated");
    let mut json = serde_json::to_value(report::SoakReport::new("custom", "", vec![old])).expect("json");
    let removed = json["results"][0]["measures"]
        .as_object_mut()
        .and_then(|m| m.remove("staleness_paths"));
    assert!(removed.is_some(), "the result carries the split");
    let back = report::SoakReport::from_json(&json.to_string()).expect("loads without staleness_paths");
    assert!(!back.summary_md().contains("## Staleness over the limit"));
}
