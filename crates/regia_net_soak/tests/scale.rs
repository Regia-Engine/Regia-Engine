// SPDX-License-Identifier: MPL-2.0
//! The scale soak (spec 120 § 1.1 "It scales", § 15.2 `tick_p99`, plan N215, ADR-8454): the
//! harness's sample game on a dedicated server at the counts DO27 ships (40 clients, gate-sized)
//! on the `mixed` bad-network profile, with one hostile peer, and a listen server at 32 on
//! `listen-uplink`, every link encrypted. Each test is seeded and runs in virtual time; the
//! numbers that need a wall clock (tick p99, server CPU) are printed and judged by the soak run
//! (`cargo xtask net-soak --plan scale`), never asserted here.
//!
//! The binary counts allocations per thread, so every run records the server's allocations per
//! tick (`regia_net_soak::probe`).

// A counting global allocator is an `unsafe impl` by definition (ADR-8454, as ADR-8031's
// relevance_scale_alloc test): it only forwards to `System` and bumps a thread-local counter.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_net_soak::probe;
use regia_net_soak::profiles::Profile;
use regia_net_soak::report::{self, ScenarioResult};
use regia_net_soak::workload::{self, Scenario, ServerMode, WireMode};
use regia_net_soak::{DEFAULT_SEED, GATE_SECONDS, SCALE_SECONDS, scale_gate_plan, scale_plan};

struct Counting;

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn bump() {
    let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
}

// SAFETY: every method forwards its arguments unchanged to `System`, which upholds the
// `GlobalAlloc` contract; the counter is a const-initialized thread-local `Cell` that never
// allocates (`try_with` skips it during thread teardown).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn thread_allocs() -> u64 {
    ALLOCS.try_with(Cell::get).unwrap_or(0)
}

fn run(profile: &str, clients: usize, mode: ServerMode, wire: WireMode, seconds: u64) -> ScenarioResult {
    probe::install(thread_allocs);
    let mut p = Profile::named(profile).unwrap_or_else(|| panic!("profile {profile}"));
    p.fit_to(seconds * 1000);
    let s = Scenario {
        profile: p,
        clients,
        mode,
        seed: DEFAULT_SEED,
        duration_ms: seconds * 1000,
        wire,
    };
    let r = report::evaluate(&workload::run(&s).expect("run"), 0);
    if let Some(m) = &r.scale {
        eprintln!("{}: {m:?}", r.label);
    }
    r
}

fn failed(r: &ScenarioResult, ids: &[&str]) -> Vec<String> {
    ids.iter()
        .map(|id| {
            r.bars
                .iter()
                .find(|b| b.id == *id)
                .unwrap_or_else(|| panic!("{}: no bar {id}", r.label))
        })
        .filter(|b| !b.passed)
        .map(|b| format!("{}: {}", b.id, b.detail))
        .collect()
}

/// The bars a scale run must hold for every honest player (spec 120 § 1.1 "It scales"); each
/// seat's bandwidth is checked against its budget from the scale numbers ([`seats_kept_budget`]).
/// The server's allocation budget per client per tick at 40 clients on `mixed` (ADR-10081).
const ALLOCS_PER_CLIENT_TICK_MAX: f64 = 30.0;

const PLAYER_BARS: [&str; 3] = [
    "involuntary_disconnects",
    "reconnect_within_grace",
    "no_wedged_client",
];

/// Every client's mean downstream and upstream over the run within the per-seat budget
/// (`report::DOWN_BUDGET_BPS`, `report::UP_BUDGET_BPS`).
fn seats_kept_budget(r: &ScenarioResult) {
    let m = r.scale.as_ref().expect("scale numbers");
    assert!(
        m.seats_over_budget.is_empty(),
        "{}: {:?}",
        r.label,
        m.seats_over_budget
    );
}

/// 40 clients on the mixed profile (40 % good-wifi, 25 % bad-wifi, 15 % mobile-4g, 10 %
/// transatlantic, 5 % flappy, 5 % congested) on a dedicated server: every client joins, nobody is
/// dropped or wedged, every client's downstream stays within its budget, and the run records the
/// scale numbers (allocations per tick counted, server bandwidth per seat, end causes).
#[test]
fn forty_clients_on_mixed_keep_every_player() {
    let r = run(
        "mixed",
        40,
        ServerMode::Dedicated,
        WireMode::Virtual,
        GATE_SECONDS,
    );
    let bad = failed(&r, &PLAYER_BARS);
    assert!(bad.is_empty(), "mixed/40: {bad:#?}");
    seats_kept_budget(&r);
    let m = r.scale.as_ref().expect("scale numbers");
    assert_eq!(m.clients, 40);
    assert_eq!(m.involuntary_disconnects, 0);
    assert_eq!(m.tick_budget_us, report::TICK_P99_BUDGET_40_US);
    assert!(
        m.allocs_per_tick_p50.is_some() && m.allocs_per_tick_p99.is_some(),
        "allocations per tick are counted: {m:?}"
    );
    assert!(m.server_up_kbps_per_seat > 0.0 && m.server_down_kbps_per_seat > 0.0);
    // The players' scripted leaves are the only ends anyone records as voluntary.
    assert!(
        m.end_causes
            .keys()
            .all(|k| k.ends_with(":left") || k.contains("timeout")),
        "end causes: {:?}",
        m.end_causes
    );
}

/// The same 40 plus one hostile peer (over-cap and malformed inputs and messages, replays,
/// garbage, oversized datagrams, a flood, forwarded captures): every hostile session is dropped
/// within 5 s of its first act, no honest client is harmed (no end, no wedge, no staleness jump),
/// and the server never moves a client to an address it did not hold.
///
/// Needs the hostile-peer fixes of lanes net-rebind-path-validation (N103, F1: the forwarded
/// captures move client 0) and net-flood-fairness (N105, F3/F4: the malformed-message and flood
/// sessions are never dropped), which are not on this lane's base; ignored until the integration
/// line carries both (ADR-8454). At the base it fails `hostile_dropped`, `others_unaffected` and
/// `no_hijack`.
#[test]
fn forty_clients_on_mixed_with_a_hostile_peer_lose_only_the_hostile_peer() {
    let r = run(
        "mixed+hostile",
        40,
        ServerMode::Dedicated,
        WireMode::Virtual,
        GATE_SECONDS,
    );
    let mut ids = PLAYER_BARS.to_vec();
    ids.extend(["hostile_dropped", "others_unaffected", "no_hijack"]);
    let bad = failed(&r, &ids);
    assert!(bad.is_empty(), "mixed+hostile/40: {bad:#?}");
    seats_kept_budget(&r);
}

/// The hostile variant's bars run at scale: `mixed+hostile` judges `hostile_dropped`,
/// `others_unaffected` and `no_hijack` besides the mixed profile's own, and the hostile peer's
/// sessions all act.
#[test]
fn the_hostile_variant_judges_the_hostile_bars_at_forty() {
    let r = run(
        "mixed+hostile",
        40,
        ServerMode::Dedicated,
        WireMode::Virtual,
        GATE_SECONDS,
    );
    for id in [
        "hostile_dropped",
        "others_unaffected",
        "no_hijack",
        "tick_p99",
        "staleness_p99",
    ] {
        assert!(r.bars.iter().any(|b| b.id == id), "{}: no bar {id}", r.label);
    }
    let h = r.hostile.as_ref().expect("a hostile peer");
    assert!(
        h.sessions.iter().all(|s| s.first_act_ms.is_some()),
        "every hostile session acted: {:?}",
        h.sessions
    );
    // Honest players keep their places whatever the hostile peer does.
    let bad = failed(&r, &PLAYER_BARS);
    assert!(bad.is_empty(), "mixed+hostile/40 honest players: {bad:#?}");
    seats_kept_budget(&r);
}

/// `others_unaffected` judges what the hostile peer does, not a flappy client's own outages: in
/// `mixed+hostile` at 40 the clients with scheduled outages (5 % flappy) are never named for the
/// staleness of their own outage windows (they were, at 362 ticks, before ADR-8454).
#[test]
fn others_unaffected_leaves_out_a_clients_own_outages() {
    let r = run(
        "mixed+hostile",
        40,
        ServerMode::Dedicated,
        WireMode::Virtual,
        GATE_SECONDS,
    );
    let flappy: Vec<usize> = (0..40)
        .filter(|&i| {
            r.scenario
                .profile
                .client_link(i, r.scenario.duration_ms)
                .outages
                .len()
                > 1
        })
        .collect();
    assert!(!flappy.is_empty(), "the mixed profile has flappy clients");
    let b = r.bars.iter().find(|b| b.id == "others_unaffected").expect("bar");
    for i in flappy {
        assert!(
            !b.detail.contains(&format!("client {i} snapshot staleness")),
            "client {i}'s own outages blamed on the hostile peer: {}",
            b.detail
        );
    }
}

/// A listen server holds 32 good-wifi clients on a 20 Mbit/s uplink: nobody is dropped or wedged
/// and the host's offered rate stays at or under the uplink (spec 120 § 1.1).
#[test]
fn a_listen_server_holds_thirty_two_on_its_uplink() {
    let r = run(
        "listen-uplink",
        32,
        ServerMode::Listen,
        WireMode::Virtual,
        GATE_SECONDS,
    );
    let mut ids = PLAYER_BARS.to_vec();
    ids.push("rate_vs_bottleneck");
    let bad = failed(&r, &ids);
    assert!(bad.is_empty(), "listen-uplink/32: {bad:#?}");
    seats_kept_budget(&r);
}

/// A listen server holds 16 clients on its uplink for the scale plan's three minutes, the scripted
/// leave and rejoin at 30 % of the run included (spec 120 § 1.1, ADR-10081 (5)). With the witness
/// gate of ADR-8733 (2) alone the shared uplink's aggregate ratcheted down to its floor in the
/// first 20 s (queue readings sit one tick apart, at the threshold, so the gate flapped and every
/// return from a silenced round read as a queue building); the rejoiners then never finished
/// their world blob and ended with `protocol_error` every two seconds, 108 times.
#[test]
fn a_listen_server_holds_sixteen_on_its_uplink_for_three_minutes() {
    let r = run(
        "listen-uplink",
        16,
        ServerMode::Listen,
        WireMode::Virtual,
        SCALE_SECONDS,
    );
    let mut ids = PLAYER_BARS.to_vec();
    ids.push("rate_vs_bottleneck");
    let bad = failed(&r, &ids);
    assert!(bad.is_empty(), "listen-uplink/16 at {SCALE_SECONDS} s: {bad:#?}");
    seats_kept_budget(&r);
}

/// The real UDP wire carries a run like the virtual one: every client joins over encrypted links
/// through real sockets on 127.0.0.1, every datagram the network pushed is read back, none from
/// the wrong sender, and the player bars hold.
#[test]
fn the_real_udp_wire_carries_an_encrypted_run() {
    let r = run("good-wifi", 8, ServerMode::Dedicated, WireMode::Udp, 15);
    assert_eq!(r.label, "good-wifi/8/dedicated/udp");
    let bad = failed(&r, &PLAYER_BARS);
    assert!(bad.is_empty(), "udp: {bad:#?}");
    seats_kept_budget(&r);
    assert_eq!(r.metrics.joined, 8, "every client joined over real UDP");
    let m = r.scale.as_ref().expect("scale numbers");
    assert_eq!(m.wire, "udp");
    let w = m.wire_stats.as_ref().expect("wire counters");
    assert!(w.sent > 1_000, "datagrams crossed real sockets: {w:?}");
    assert_eq!(w.send_errors, 0, "{w:?}");
    assert_eq!(w.unknown_source, 0, "{w:?}");
    assert_eq!(w.mismatched, 0, "{w:?}");
    assert_eq!(w.kernel_drops, 0, "{w:?}");
    // What was pushed and not read at the end is still in a kernel buffer.
    assert!(w.read <= w.sent, "{w:?}");
}

/// The same seed on the two wires gives the same game: the real path only adds system calls, so
/// joins, ends and rejoins agree.
#[test]
fn the_udp_wire_keeps_the_virtual_run() {
    let v = run("bad-wifi", 6, ServerMode::Dedicated, WireMode::Virtual, 10);
    let u = run("bad-wifi", 6, ServerMode::Dedicated, WireMode::Udp, 10);
    assert_eq!(v.metrics.joined, u.metrics.joined);
    assert_eq!(v.ends.len(), u.ends.len());
    assert_eq!(v.metrics.reconnects, u.metrics.reconnects);
}

/// The plans: `scale` runs mixed and mixed+hostile at 40, 64 and 128 and listen-uplink at 16 and
/// 32 for 180 s; `scale-gate` is the gate-sized 40 / 32 set at 60 s.
#[test]
fn the_scale_plans_have_their_shape() {
    let labels = |p: Vec<Scenario>| p.iter().map(|s| (s.label(), s.duration_ms)).collect::<Vec<_>>();
    let full = labels(scale_plan(DEFAULT_SEED));
    let want: Vec<(String, u64)> = [
        "mixed/40/dedicated",
        "mixed+hostile/40/dedicated",
        "mixed/64/dedicated",
        "mixed+hostile/64/dedicated",
        "mixed/128/dedicated",
        "mixed+hostile/128/dedicated",
        "listen-uplink/16/listen",
        "listen-uplink/32/listen",
    ]
    .iter()
    .map(|l| (l.to_string(), 180_000))
    .collect();
    assert_eq!(full, want);
    let gate = labels(scale_gate_plan(DEFAULT_SEED));
    assert_eq!(
        gate,
        vec![
            ("mixed/40/dedicated".to_string(), 60_000),
            ("mixed+hostile/40/dedicated".to_string(), 60_000),
            ("listen-uplink/32/listen".to_string(), 60_000),
        ]
    );
}

/// One CPU budget per tier (spec 120 § 15.2 `tick_p99`, docs/perf/budgets.ron): a quarter of the
/// 30 Hz tick up to 40 clients, a third up to 64, half above; never more than the tick.
/// ADR-10081 (spec 120 § 1.1 "It scales"): what the server allocates per client per tick at 40
/// clients on `mixed`, counted by this binary's allocator (the harness's own datagram copies
/// left out). Before ADR-10081 it was 41.8: a message writer that grew from empty for every state
/// message, three per-stream vectors and a body buffer per poll, and two more per packet.
#[test]
fn the_server_allocates_within_its_budget_per_client_per_tick() {
    let r = run(
        "mixed",
        40,
        ServerMode::Dedicated,
        WireMode::Virtual,
        GATE_SECONDS,
    );
    let m = r.scale.as_ref().expect("scale numbers");
    let per_client = m
        .allocs_per_tick_per_client
        .expect("this binary counts allocations");
    assert!(
        per_client <= ALLOCS_PER_CLIENT_TICK_MAX,
        "the server allocated {per_client:.1} times per client per tick at {} clients \
         (p50 {:?} per tick); the budget is {ALLOCS_PER_CLIENT_TICK_MAX}",
        m.clients,
        m.allocs_per_tick_p50
    );
}

#[test]
fn tick_budgets_have_one_tier_per_scale_count() {
    assert_eq!(report::tick_budget_us(8), 8_333);
    assert_eq!(report::tick_budget_us(40), 8_333);
    assert_eq!(report::tick_budget_us(41), 11_111);
    assert_eq!(report::tick_budget_us(64), 11_111);
    assert_eq!(report::tick_budget_us(128), 16_666);
    assert!(report::tick_budget_us(10_000) < report::TICK_P99_BUDGET_US);
}

/// `cargo xtask net-soak`-style plans through this binary's counting allocator: with
/// `REGIA_NET_SOAK_OUT` set (and `REGIA_NET_SOAK_PLAN=scale`, `scale-gate` or a custom set,
/// `REGIA_NET_SOAK_WIRE=udp` for the real sockets), it runs the plan and writes `result.json` and
/// `summary.md` with the allocations per tick filled in; otherwise a no-op.
#[test]
fn net_soak_scale_from_env() {
    probe::install(thread_allocs);
    if let Some(result) = regia_net_soak::run_from_env(regia_net_soak::no_heap_probe) {
        let report = result.unwrap_or_else(|e| panic!("net-soak harness error: {e}"));
        eprintln!("{}", report.summary_md());
    }
}

/// The docs/perf rows `net_scale_tick_p99_40`, `_64` and `_128` (spec 120 § 15.2 `tick_p99`):
/// `mixed` on a dedicated server at each tier for 60 virtual seconds, one run at a time, printing
/// each tier's server tick p99 for `cargo xtask perf-budgets`. Wall-clock numbers, so ignored in
/// the test run.
#[test]
#[ignore = "wall-clock perf numbers; cargo xtask perf-budgets runs it"]
fn scale_tick_p99_tiers() {
    for n in [40, 64, 128] {
        let r = run("mixed", n, ServerMode::Dedicated, WireMode::Virtual, GATE_SECONDS);
        let m = r.scale.as_ref().expect("scale numbers");
        println!(
            "perf-scale net_scale_tick_p99_{n}: {:.3} ms (budget {:.3} ms, cpu {:.1} % of a core, allocs/tick p50 {})",
            f64::from(m.tick_us_p99) / 1000.0,
            f64::from(m.tick_budget_us) / 1000.0,
            m.server_cpu_pct_one_core,
            m.allocs_per_tick_p50.unwrap_or(0)
        );
    }
}
