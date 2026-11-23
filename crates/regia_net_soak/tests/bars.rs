// SPDX-License-Identifier: MPL-2.0
//! The spec 120 § 15.2 bars (ADR-8030): each a function of `result.json`, judged on the v0.5.0
//! gate output (a v1 file, read by `SoakReport::from_json`) and on fresh v2 runs of the engine as
//! it is. Every bar keeps a run it fails on: `no_hijack` on hostile with a full-relay attacker,
//! `missed_input` on links the input feedback cannot save (fades, an uplink bloated past the
//! input horizon) and on a broken input lead, the outage bars on a broken timeout policy; clean,
//! good-wifi, bad-wifi, asym-bufferbloat and hostile's own attacker pass where they should
//! (ADR-8424). The rate controller's owning lane flipped its two: `rate_vs_bottleneck` on
//! congested and `random_loss_kept` on lossy-fat-pipe pass since the rate controller (N202,
//! ADR-8301); `rate_vs_bottleneck` still fails with the controller off (ADR-8487), and
//! `random_loss_kept` has no failing run yet (v0.5's loss-only cut is gone, so the controller off
//! keeps the budget too).
//!
//! The endpoints follow the engine's own policies: the silence policy is
//! `regia_net::TimeoutPolicy::default()` (ADR-7950: 1.5 s to the interruption notice, 20 s to
//! removal) and inputs are stamped by `NetClient::due_input_ticks` (ADR-8077, the lead steered
//! from the server's input feedback). `workload::run_with` replaces either one, which is how the
//! tests here show a bar failing on a deliberately broken policy.

use regia_net::TimeoutPolicy;
use regia_net::reconnect::ReconnectPolicy;
use regia_net_soak::impair::GilbertElliott;
use regia_net_soak::profiles::{LONG_OUTAGE_AT_MS, LONG_OUTAGES_MS, Profile};
use regia_net_soak::report::{self, Bar, NOT_MEASURED_V1, SPEC120_BARS, ScenarioResult, SoakReport};
use regia_net_soak::workload::{self, INPUT_LEAD_TICKS, InputLead, Knobs, RunRecord, Scenario, ServerMode};

/// `cargo xtask net-soak --plan gate` at v0.5.0 (59ff4b61d), lane net-soak-harness: format v1.
const GATE_V050_V1: &str = include_str!("fixtures/gate-result-v050-v1.json");

fn v1() -> SoakReport {
    SoakReport::from_json(GATE_V050_V1).expect("the v1 gate result loads")
}

fn result<'a>(r: &'a SoakReport, label: &str) -> &'a ScenarioResult {
    r.results
        .iter()
        .find(|x| x.label == label)
        .unwrap_or_else(|| panic!("no run {label}"))
}

fn bar<'a>(r: &'a ScenarioResult, id: &str) -> &'a Bar {
    r.bars.iter().find(|b| b.id == id).unwrap_or_else(|| {
        panic!(
            "{}: no bar {id} in {:?}",
            r.label,
            r.bars.iter().map(|b| &b.id).collect::<Vec<_>>()
        )
    })
}

fn run(profile: &str, clients: usize, seconds: u64, seed: u64) -> ScenarioResult {
    run_with(
        Profile::named(profile).expect("known profile"),
        clients,
        seconds,
        seed,
        &Knobs::default(),
    )
}

/// A run whose endpoints follow `knobs` instead of the engine's own policies. The bars judge it
/// against the engine's all the same.
fn run_with(p: Profile, clients: usize, seconds: u64, seed: u64, knobs: &Knobs) -> ScenarioResult {
    record_with(p, clients, seconds, seed, knobs).1
}

/// [`run_with`], with the run's raw record (every end on both sides, every rejoin).
fn record_with(
    mut p: Profile,
    clients: usize,
    seconds: u64,
    seed: u64,
    knobs: &Knobs,
) -> (RunRecord, ScenarioResult) {
    p.fit_to(seconds * 1000);
    let s = Scenario {
        profile: p,
        clients,
        mode: ServerMode::Dedicated,
        seed,
        duration_ms: seconds * 1000,
        wire: Default::default(),
    };
    let record = workload::run_with(&s, knobs).expect("run");
    let result = report::evaluate(&record, 0);
    (record, result)
}

/// The run's missed input ticks and judged input ticks.
fn missed(r: &ScenarioResult) -> (u64, u64) {
    let m = r.measures.as_ref().expect("v2 measures");
    (m.input_ticks_missed, m.input_ticks_judged)
}

/// bad-wifi whose bursts are fades: the same latency, jitter, random loss, reorder, duplication
/// and bandwidth, with the Gilbert-Elliott chain of each direction changed from short lossy
/// bursts (1 % to enter, 30 % to leave, 60 % loss inside) to total-loss fades: 0.4 % to enter
/// (one fade per 250 datagrams), 3.3 % to leave (30 datagrams on average), 100 % loss inside. A
/// soak client sends 44 to 60 datagrams a second on this link and is sent 33 to 54 (measured),
/// so a fade lasts about 0.6 s and comes about every 5 s; in the long run 10.8 % of a
/// direction's datagrams are lost in fades. A wireless link at the edge of its range or under
/// interference does this. It keeps the name, so it is judged at bad-wifi's 2 %.
fn bad_wifi_with_fades() -> Profile {
    let fades = GilbertElliott {
        p_good_to_bad: 0.004,
        p_bad_to_good: 0.033,
        loss_good: 0.0,
        loss_bad: 1.0,
    };
    assert!((fades.mean_loss() - 0.108).abs() < 0.001, "{}", fades.mean_loss());
    let mut p = Profile::named("bad-wifi").expect("bad-wifi");
    p.summary = format!(
        "{}; bursts replaced by total-loss fades of 30 datagrams",
        p.summary
    );
    p.up.burst = Some(fades);
    p.down.burst = Some(fades);
    p
}

/// A v1 `result.json` still loads: same runs, same verdict and counts, and it keeps saying v1.
#[test]
fn a_v1_result_still_loads_with_its_verdict() {
    let r = v1();
    assert_eq!(r.version, 1);
    assert_eq!(r.results.len(), 14);
    assert!(!r.passed);
    assert_eq!(
        (r.known_blocker_failures, r.new_failures),
        (10, 6),
        "the v1 verdict is unchanged"
    );
    assert!(
        r.non_gating_failures > 0,
        "the spec 120 bars were judged and do not gate"
    );
    // The six harness bars keep gating; the spec 120 bars do not.
    for res in &r.results {
        for b in &res.bars {
            assert_eq!(
                b.gating,
                !SPEC120_BARS.contains(&b.id.as_str()),
                "{} {}",
                res.label,
                b.id
            );
        }
    }
    assert!(SoakReport::from_json("{\"format\":\"other\",\"version\":1}").is_err());
    let newer = GATE_V050_V1.replacen("\"version\": 1", "\"version\": 3", 1);
    assert!(
        SoakReport::from_json(&newer)
            .expect_err("v3")
            .contains("reads 1 to 2")
    );
}

/// On the v0.5.0 output: bad-wifi misses far more than 2 % of its input ticks (F2 / E3), hostile's
/// forwarding attacker moves a victim (F1, 28 moves with none scheduled), clean's staleness passes,
/// and what v1 never recorded is skipped, not guessed.
#[test]
fn spec120_bars_on_the_v050_gate_output() {
    let r = v1();
    let bad = result(&r, "bad-wifi/8/dedicated");
    let missed = bar(bad, "missed_input");
    assert!(!missed.passed, "{}", missed.detail);
    assert!(missed.detail.starts_with("9.20 % of"), "{}", missed.detail);
    assert!(!bar(bad, "corrections").passed);
    let hostile = result(&r, "hostile/8/dedicated");
    let hijack = bar(hostile, "no_hijack");
    assert!(
        !hijack.passed && hijack.detail.starts_with("28 address moves"),
        "{}",
        hijack.detail
    );
    let clean = result(&r, "clean/8/dedicated");
    let stale = bar(clean, "staleness_p99");
    assert!(stale.passed && stale.skipped.is_none(), "{}", stale.detail);
    // At v0.5.0 the random 0.5-1 KiB samples alone make clean miss 8 % of its input ticks (the
    // v051-net-input-mtu blocker).
    assert!(!bar(clean, "missed_input").passed);
    let congested = result(&r, "congested/8/dedicated");
    assert_eq!(
        bar(congested, "rate_vs_bottleneck").skipped.as_deref(),
        Some(NOT_MEASURED_V1)
    );
    assert!(!bar(congested, "staleness_p99").passed, "24 ticks > 8");
    let flappy = result(&r, "flappy/8/dedicated");
    assert_eq!(
        bar(flappy, "interrupted_once").skipped.as_deref(),
        Some(NOT_MEASURED_V1)
    );
    assert_eq!(
        bar(flappy, "staleness_p99").skipped.as_deref(),
        Some(NOT_MEASURED_V1)
    );
    // F7: random loss cuts bad-wifi's budget to 8 entities; random_loss_kept belongs to lossy-fat-pipe
    // only, so it is not reported here.
    assert!(bad.bars.iter().all(|b| b.id != "random_loss_kept"));
    // The summary lists them apart from the gating failures.
    let md = r.summary_md();
    assert!(
        md.contains("## Spec 120 bars") && md.contains("`no_hijack` | FAIL"),
        "{md}"
    );
}

/// A fresh v2 run: with the rate controller (spec 120 § 4.2, ADR-8301) congested holds its offered
/// rate under the 256 kbit/s bottleneck from 5 s after the join (E1 at v0.5.0: "never"; flipped by
/// its owning lane N202).
#[test]
fn rate_vs_bottleneck_passes_on_congested() {
    let r = run("congested", 4, 20, 5);
    let b = bar(&r, "rate_vs_bottleneck");
    assert!(b.passed && b.skipped.is_none(), "{}", b.detail);
    assert!(!b.gating && r.bars.iter().filter(|x| !x.gating).count() >= 2);
}

/// The same run with the rate controller switched off (every due packet leaves at once, as v0.5
/// sent): the offered rate is not held under the bottleneck, so the bar still has a run it fails
/// on (ADR-8424, ADR-8487).
#[test]
fn rate_vs_bottleneck_fails_on_congested_without_the_rate_controller() {
    let r = run_with(
        Profile::named("congested").expect("congested"),
        4,
        20,
        5,
        &Knobs {
            rate_control_off: true,
            ..Knobs::default()
        },
    );
    let b = bar(&r, "rate_vs_bottleneck");
    assert!(!b.passed && b.skipped.is_none(), "{}", b.detail);
    assert!(b.detail.contains(" down: "), "{}", b.detail);
}

/// bad-wifi with the engine's input feedback (ADR-8077): the bar passes where it should. Measured
/// (seeded, virtual time): 0 of 1,659 client ticks missed with the steered lead; 8 of 1,659
/// (0.48 %) with the feedback switched off and the old fixed lead of two ticks, which the tick
/// estimate of ADR-8077 alone keeps inside the 2 % too. At v0.5.0 the gate's bad-wifi run missed
/// 9.20 % (`spec120_bars_on_the_v050_gate_output`).
#[test]
fn missed_input_passes_on_bad_wifi_with_input_feedback() {
    let steered = run("bad-wifi", 4, 15, 9);
    let b = bar(&steered, "missed_input");
    assert!(b.passed && b.skipped.is_none(), "{}", b.detail);
    let (miss, judged) = missed(&steered);
    assert!(judged > 4 * 300, "{}", b.detail);
    // Well inside the limit, not at its edge: at most 0.5 % (measured 0).
    assert!(miss * 200 <= judged, "{}", b.detail);
    let fixed = run_with(
        Profile::named("bad-wifi").expect("bad-wifi"),
        4,
        15,
        9,
        &Knobs {
            input_lead: InputLead::Fixed(INPUT_LEAD_TICKS),
            ..Knobs::default()
        },
    );
    let b = bar(&fixed, "missed_input");
    assert!(b.passed, "{}", b.detail);
    let (miss_fixed, judged_fixed) = missed(&fixed);
    assert!(judged_fixed > 4 * 300, "{}", b.detail);
    // The feedback is what takes it from some missed ticks to none: without it the run misses
    // more, and still under 1 % (measured 0.48 %).
    assert!(
        miss_fixed > miss && miss_fixed * 100 <= judged_fixed,
        "{}",
        b.detail
    );
}

/// The bar still catches missed input on bad-wifi: a client that stamps its inputs for the tick
/// the server is on, with no lead at all (a deliberately broken input policy), misses about a
/// tenth of its ticks, five times the bar's 2 %, and the bar fails. It missed about a fifth
/// (347 of 1,616, 21.47 %) before the server merged late inputs into its next tick and before the
/// rate controller (ADR-8301) kept the bad-wifi queues short; at seed 9 alone it then missed
/// 9.21-9.69 %, under the "more than a tenth" this pin asks (ADR-8534). One seed is a draw from
/// a spread of 8.28-12.69 % (seeds 1-24, the test Mac), so the pin now runs seeds 1-16: every one
/// fails the bar, and together they miss more than a tenth of their ticks (measured 2,799 of
/// 26,454, 10.58 %; seeds 1-8 10.99 %, 1-24 10.42 %).
#[test]
fn missed_input_fails_on_bad_wifi_with_a_broken_input_lead() {
    let (mut miss, mut judged) = (0, 0);
    for seed in 1..=16 {
        let r = run_with(
            Profile::named("bad-wifi").expect("bad-wifi"),
            4,
            15,
            seed,
            &Knobs {
                input_lead: InputLead::Fixed(0),
                ..Knobs::default()
            },
        );
        let b = bar(&r, "missed_input");
        assert!(!b.passed && b.skipped.is_none(), "seed {seed}: {}", b.detail);
        assert!(b.detail.contains("limit 2.0 %"), "{}", b.detail);
        let (m, j) = missed(&r);
        assert!(j > 4 * 300, "seed {seed}: {}", b.detail);
        miss += m;
        judged += j;
    }
    assert!(miss * 10 > judged, "{miss} of {judged} ticks missed");
}

/// A link that really misses more than 2 % with the input feedback on (the old
/// `missed_input_fails_on_bad_wifi`, which bad-wifi itself no longer fails): bad-wifi with fades
/// ([`bad_wifi_with_fades`]). The steered lead climbs to 14 to 17 ticks on it (about half a
/// second of input delay), and a longer fade still outlasts it, so the inputs of its ticks
/// arrive after it. Measured with the steered lead: 5.65 % of 7,059 client ticks missed at 4
/// clients over 60 s (this run; 3.70 % to 8.64 % over sixteen other seeds), 6.32 % at 8 clients
/// over 60 s and 4.86 % at 8 over 300 s; with the feedback off 9.58 %.
#[test]
fn missed_input_fails_on_bad_wifi_with_fades() {
    let r = run_with(bad_wifi_with_fades(), 4, 60, 9, &Knobs::default());
    let b = bar(&r, "missed_input");
    assert!(!b.passed && b.skipped.is_none(), "{}", b.detail);
    assert!(b.detail.contains("limit 2.0 %"), "{}", b.detail);
    let (miss, judged) = missed(&r);
    assert!(judged > 4 * 1500, "{}", b.detail);
    // Clearly over the limit, not at its edge: more than 2.5 %.
    assert!(miss * 200 > 5 * judged, "{}", b.detail);
    // The feedback helps on this link too; it cannot close it. Without it the run misses more.
    let fixed = run_with(
        bad_wifi_with_fades(),
        4,
        60,
        9,
        &Knobs {
            input_lead: InputLead::Fixed(INPUT_LEAD_TICKS),
            ..Knobs::default()
        },
    );
    assert!(!bar(&fixed, "missed_input").passed);
    assert!(missed(&fixed).0 > miss, "{:?} against {miss}", missed(&fixed));
}

/// The upstream queue of [`asym_bufferbloat_upstream`], milliseconds: beyond the server's input
/// horizon (`ServerConfig::default().input_buffer_ticks`, 64 ticks = 2.13 s at 30 Hz), so no
/// input lead the engine allows gets an input there on its tick.
const UPSTREAM_QUEUE_MS: u64 = 2_500;

/// asym-bufferbloat turned around: the bloated queue is the client's uplink. Good-wifi down; up
/// the profile's 4 Mbit/s bottleneck behind a 2.5 s (1,250,000-byte) drop-tail queue kept full
/// by an upload offered at 4.4 Mbit/s (the profile's cross traffic, now going up). A cable or DSL
/// uplink under a cloud backup does this: upstream buffers of seconds are the classic bufferbloat
/// case (Gettys and Nichols, "Bufferbloat: Dark Buffers in the Internet", CACM 55(1), 2012). Every
/// input waits 2.5 s in the queue, past the server's 2.13 s input horizon. It keeps the name, so it
/// is judged at asym-bufferbloat's 3 %.
fn asym_bufferbloat_upstream() -> Profile {
    let mut p = Profile::named("asym-bufferbloat").expect("asym-bufferbloat");
    let horizon_ms = regia_net::ServerConfig::default().input_buffer_ticks as u64 * 1000 / workload::TICK_HZ;
    let queue_ms = UPSTREAM_QUEUE_MS;
    assert!(queue_ms > horizon_ms, "{queue_ms} ms against {horizon_ms} ms");
    let bloated_down = p.down.clone();
    let bottleneck = bloated_down.bandwidth_bps;
    p.up = regia_net_soak::impair::Direction {
        bandwidth_bps: bottleneck,
        queue_bytes: (bottleneck / 8 * queue_ms / 1000) as usize,
        cross_bps: bloated_down.cross_bps,
        ..p.up
    };
    assert_eq!(p.up.queue_bytes, 1_250_000);
    p.down = Profile::named("good-wifi").expect("good-wifi").down;
    p.summary = format!(
        "{}; turned around: the 2.5 s queue and its cross traffic are on the uplink, good-wifi down",
        p.summary
    );
    p
}

/// The bar still catches bufferbloat that stalls input (ADR-8424).
///
/// asym-bufferbloat itself (F2, the downstream queue full) no longer fails: with the input
/// feedback (ADR-8077) and the late-input merge (ADR-8217, spec 120 § 6.3: a late input is carried
/// into the next tick), each join misses its first few ticks and nothing after them. Measured at 4
/// clients, seed 9: 9 of 1,287 ticks (0.70 %) over 15 s and the same 9 of 6,687 (0.13 %) over 60
/// s (0.47 % to 1.09 % over 15 s on seeds 1 to 5); at v0.5.0 the 15 s run missed 10.10 % (the old
/// form of this test).
///
/// The failing case is the same bloat on the uplink, deeper than the server's input horizon
/// ([`asym_bufferbloat_upstream`]): no input reaches the server for the first 2.5 s of each
/// join, whatever the lead (the joins take about 10 s: each handshake round trip crosses the
/// queue). Measured with the steered lead: 39.33 % of 628 ticks missed over 15 s
/// (38.22 % to 42.36 % on seeds 1 to 5) and 4.43 % of 6,028 over 60 s (4.17 % to 4.69 %; 4.79 %
/// at 8 clients). A 1.5 s uplink queue, inside the horizon, passes over 60 s (1.75 %).
///
/// With the input feedback off, asym-bufferbloat fails: 9.87 % (9.01 % to 10.41 % on seeds 1 to
/// 5; 100 % at v0.5.0, before the late-input merge).
///
/// The numbers hold with reliable streams since join snapshots ride a foreground stream
/// (ADR-8624: on the bulk stream each client's reassembly stall deadline ended it once on the
/// profile and every ~10 s on the upstream case, and the rejoins broke both halves).
#[test]
fn missed_input_fails_on_asym_bufferbloat() {
    // The profile itself passes, at 15 s and at 60 s, with the same few missed ticks of the joins.
    let r = run("asym-bufferbloat", 4, 15, 9);
    let b = bar(&r, "missed_input");
    assert!(b.passed && b.skipped.is_none(), "{}", b.detail);
    let (miss, judged) = missed(&r);
    assert!(judged > 4 * 300, "{}", b.detail);
    // Well inside the 3 % (measured 0.70 %).
    assert!(miss * 50 <= judged, "{}", b.detail);
    let long = run("asym-bufferbloat", 4, 60, 9);
    let b = bar(&long, "missed_input");
    assert!(b.passed, "{}", b.detail);
    let (miss_long, judged_long) = missed(&long);
    // The later 45 s add at most 0.1 % of their ticks (measured none).
    assert!(
        judged_long > judged && miss_long.saturating_sub(miss) * 1000 <= judged_long - judged,
        "{} against {}",
        b.detail,
        bar(&r, "missed_input").detail
    );

    // The uplink bloated past the input horizon: the bar fails, over 15 s by far.
    let up = run_with(asym_bufferbloat_upstream(), 4, 15, 9, &Knobs::default());
    let b = bar(&up, "missed_input");
    assert!(!b.passed && b.skipped.is_none(), "{}", b.detail);
    assert!(b.detail.contains("limit 3.0 %"), "{}", b.detail);
    let (miss_up, judged_up) = missed(&up);
    // The joins themselves cross the queue several times (handshake round trips of 2.5 s up),
    // so a client is judged for about the last 5 s of the 15 (measured 628 ticks of 4 clients).
    assert!(judged_up > 4 * 100, "{}", b.detail);
    // More than a quarter of the ticks (measured 39.33 %).
    assert!(miss_up * 4 > judged_up, "{}", b.detail);
    // And over 60 s: each join's stall is longer than the run's 3 % share of it can absorb, and
    // the run never makes it up.
    let up_long = run_with(asym_bufferbloat_upstream(), 4, 60, 9, &Knobs::default());
    let b = bar(&up_long, "missed_input");
    assert!(!b.passed, "{}", b.detail);
    let (miss_up_long, judged_up_long) = missed(&up_long);
    assert!(
        judged_up_long > 4 * 1_200 && miss_up_long >= miss_up && miss_up_long * 100 > 3 * judged_up_long,
        "{}",
        b.detail
    );

    // Input feedback switched off on asym-bufferbloat: the bar fails, with many times the misses.
    let fixed = run_with(
        Profile::named("asym-bufferbloat").expect("asym-bufferbloat"),
        4,
        15,
        9,
        &Knobs {
            input_lead: InputLead::Fixed(INPUT_LEAD_TICKS),
            ..Knobs::default()
        },
    );
    let b = bar(&fixed, "missed_input");
    assert!(!b.passed, "{}", b.detail);
    let (miss_fixed, judged_fixed) = missed(&fixed);
    assert_eq!(judged_fixed, judged, "{}", b.detail);
    // Measured 127 against 9.
    assert!(miss_fixed > 5 * miss, "{miss_fixed} against {miss}");
}

/// The bar still catches a hijack on hostile (ADR-8424). At v0.5.0 the profile's own attacker,
/// which only replays copies of the victim's records from its address, moved the victim's peer
/// there (F1). Path validation (ADR-8299) ended that: the server now probes the new address and
/// moves only when a record from there acknowledges the probe, which a replayer never sees. So the
/// profile's attacker passes the bar (0 address moves, and the server did probe it: measured 13
/// probes on seeds 1 to 4 and 13). The bar's failing case is now the attacker ADR-8299 names as
/// still able to win: a full relay ([`Knobs::hostile_relay`]) carries each probe on to the
/// victim under the server's address, the victim acknowledges it, the next replayed record
/// validates the attacker's address, and the victim's peer moves there. Measured: from the
/// hostile start (10 s), once a second (the candidate's trial budget), 12 moves to the
/// attacker's address over a 22 s run, each followed by the victim's own record moving it back
/// (23 moves in all) on seeds 1 to 4 and 13.
#[test]
fn no_hijack_fails_on_hostile() {
    // The profile's own attacker: replays, garbage, oversized datagrams. The bar passes.
    let replay = run("hostile", 4, 22, 13);
    let b = bar(&replay, "no_hijack");
    assert!(b.passed && b.skipped.is_none(), "{}", b.detail);
    let m = replay.measures.as_ref().expect("v2 measures");
    assert!(m.foreign_moves.is_empty() && m.address_moves == 0, "{m:?}");
    let h = replay.hostile.as_ref().expect("hostile record");
    assert!(h.replays_sent > 1_000, "{h:?}");
    // The replays did reach the server as authentic records from a new address: it probed that
    // address (ADR-8299) and never moved there.
    assert!(replay.server_totals.path_probes > 0, "{:?}", replay.server_totals);
    assert_eq!(replay.server_totals.rebinds, 0, "{:?}", replay.server_totals);

    // A full relay on the same run: the bar fails, naming the victim and the times.
    let relay = run_with(
        Profile::named("hostile").expect("hostile"),
        4,
        22,
        13,
        &Knobs {
            hostile_relay: true,
            ..Knobs::default()
        },
    );
    let b = bar(&relay, "no_hijack");
    assert!(!b.passed && b.skipped.is_none(), "{}", b.detail);
    assert!(
        b.detail
            .starts_with("the server moved client 0's peer 1 to an address the client never held at "),
        "{}",
        b.detail
    );
    let m = relay.measures.as_ref().expect("v2 measures");
    // Every foreign move is the victim's (client 0), from the hostile start on, and more than a
    // handful of them: the relay wins again each time the victim moves back.
    assert!(m.foreign_moves.len() >= 10, "{:?}", m.foreign_moves);
    assert!(
        m.foreign_moves.iter().all(|x| x.client == 0 && x.at_ms >= 10_000),
        "{:?}",
        m.foreign_moves
    );
    assert!(
        m.address_moves > m.foreign_moves.len() as u64,
        "the victim's own records move it back: {m:?}"
    );
    assert_eq!(
        relay.server_totals.rebinds, m.address_moves,
        "{:?}",
        relay.server_totals
    );
}

/// On good-wifi nothing is interrupted, nothing is moved, staleness stays low: those bars pass.
#[test]
fn good_wifi_passes_what_it_should() {
    let r = run("good-wifi", 4, 15, 2);
    for id in ["interrupted_once", "staleness_p99"] {
        let b = bar(&r, id);
        assert!(b.passed && b.skipped.is_none(), "{id}: {}", b.detail);
    }
    let m = r.measures.as_ref().expect("v2 measures");
    assert!(m.foreign_moves.is_empty() && m.address_moves == 0, "{m:?}");
}

/// How far from `outage start + timeout_ms` a removal may be recorded, milliseconds: a peer's
/// silence starts at its last datagram before the outage (the server sends every 33 ms tick, the
/// link adds up to 8 ms), and each end looks at its clock once per frame or tick.
const REMOVAL_SLACK_MS: u64 = 100;

/// One outage of `client`, as the run measured it.
fn outage(r: &ScenarioResult, client: usize) -> &report::OutageCheck {
    let m = r.measures.as_ref().expect("v2 measures");
    let mut of_client = m.outages.iter().filter(|o| o.client == client);
    let o = of_client
        .next()
        .unwrap_or_else(|| panic!("no outage of client {client}"));
    assert!(of_client.next().is_none(), "client {client} has one outage");
    o
}

/// long-outage under the engine's silence policy (ADR-7950) and its resume (spec 120 § 10.3,
/// ADR-8298, ADR-8352, ADR-10093; until that lane this test was
/// `long_outage_is_interrupted_once_then_removed_and_rejoins_as_a_new_peer`, which pinned the
/// removal: its asserts live on in `long_outage_without_the_resume_is_removed_and_rejoins_as_a_new_peer`).
/// Every outage is reported once. The 11 s outage is inside the removal timeout: one Interrupted,
/// one Restored, the same peer, no end. The 25 s and 45 s outages are beyond it: one Interrupted,
/// the connection ends at `timeout_ms` of silence on both ends, the server holds the player
/// dormant, the client's reconnect timeline presents its resume ticket, and once the link is back
/// the client is the same peer again, with one Restored.
#[test]
fn long_outage_is_interrupted_once_then_dormant_and_resumes_as_the_same_peer() {
    let policy = report::timeout_policy();
    let (record, r) = record_with(
        Profile::named("long-outage").expect("long-outage"),
        3,
        120,
        3,
        &Knobs::default(),
    );
    let m = r.measures.as_ref().expect("v2 measures");
    assert_eq!(m.outages.len(), 3, "{:?}", m.outages);
    assert_eq!(m.stray_interruptions, 0);
    // Client 0: 11 s, inside the removal timeout.
    assert!(LONG_OUTAGES_MS[0] < policy.timeout_ms && LONG_OUTAGES_MS[0] >= policy.interrupt_ms);
    let kept = outage(&r, 0);
    assert_eq!(
        (kept.interrupted, kept.restored, kept.restore_owed),
        (1, 1, true),
        "{kept:?}"
    );
    assert!(record.ends.iter().all(|e| e.client != 0), "{:?}", record.ends);
    assert!(m.resumes.iter().all(|x| x.client != 0), "{:?}", m.resumes);
    assert_eq!(
        record.clients[0].connected.len(),
        1,
        "client 0 stays the peer it was"
    );
    // Clients 1 and 2: 25 s and 45 s, beyond it.
    let due = LONG_OUTAGE_AT_MS + policy.timeout_ms;
    for client in [1, 2] {
        let len = LONG_OUTAGES_MS[client];
        assert!(len > policy.timeout_ms);
        let o = outage(&r, client);
        assert_eq!(
            (o.start_ms, o.end_ms),
            (LONG_OUTAGE_AT_MS, LONG_OUTAGE_AT_MS + len)
        );
        assert_eq!((o.interrupted, o.restored, o.restore_owed), (1, 1, true), "{o:?}");
        // The connection ended at the timeout, by silence, on both ends; one end each (the
        // server's is the start of the dormancy).
        for side in ["client", "server"] {
            let ends: Vec<_> = record
                .ends
                .iter()
                .filter(|e| e.client == client && e.side == side)
                .collect();
            assert_eq!(ends.len(), 1, "client {client} {side}: {:?}", record.ends);
            assert_eq!(ends[0].cause, "timeout", "{:?}", ends[0]);
            assert!(!ends[0].voluntary);
            assert!(
                ends[0].at_ms.abs_diff(due) <= REMOVAL_SLACK_MS,
                "client {client} {side}: removed at {} ms, the policy says {due} ms",
                ends[0].at_ms
            );
        }
        // Back as the same peer, within the grace of the link's return.
        let resume = m
            .resumes
            .iter()
            .find(|x| x.client == client)
            .unwrap_or_else(|| panic!("client {client} rejoined: {:?}", m.resumes));
        assert_eq!(resume.after_peer, Some(resume.before_peer), "{resume:?}");
        let peers: Vec<u64> = record.clients[client].connected.iter().map(|x| x.0).collect();
        assert_eq!(peers, [resume.before_peer; 2], "one peer id, two connections");
        assert!(record.clients[client].reconnect_attempts >= 1);
        assert_eq!(record.clients[client].reconnects_given_up, 0);
        let rejoin = r
            .reconnects
            .iter()
            .find(|x| x.client == client)
            .expect("a rejoin");
        assert_eq!(rejoin.link_up_ms, LONG_OUTAGE_AT_MS + len);
        assert!(
            rejoin.connected_ms.is_some_and(
                |t| t >= rejoin.link_up_ms && t <= rejoin.link_up_ms + report::RECONNECT_GRACE_MS
            ),
            "{rejoin:?}"
        );
    }
    assert_eq!(record.ends.len(), 4, "{:?}", record.ends);
    // The report counts one end per lost connection: the client's own.
    assert_eq!(r.ends.len(), 2, "{:?}", r.ends);
    assert_eq!(m.resumes.len(), 2, "{:?}", m.resumes);
    // The server held both dormant and resumed both; no hold ran out.
    let totals = &record.server.totals;
    assert_eq!(
        (totals.dormant, totals.resumes, totals.dormant_expired),
        (2, 2, 0),
        "{totals:?}"
    );
    // The bars: every outage has its one Interrupted and one Restored, every rejoin its peer.
    let i = bar(&r, "interrupted_once");
    assert!(i.passed, "{}", i.detail);
    assert_eq!(
        i.detail,
        "3 outages, each one Interrupted and one Restored; none elsewhere"
    );
    let s = bar(&r, "resume_same_peer");
    assert!(s.passed, "{}", s.detail);
    assert_eq!(s.detail, "2 involuntary rejoins, each with its old peer id");
    // The harness bars: both removals are the policy's (outages beyond the timeout), and both
    // clients are back in time.
    let ends = bar(&r, "involuntary_disconnects");
    assert!(ends.passed, "{}", ends.detail);
    assert_eq!(r.metrics.ends_excused, 2, "{}", ends.detail);
    assert!(bar(&r, "reconnect_within_grace").passed);
    // Both outage bars decide the verdict (ADR-10093), and the run passes.
    assert!(i.gating && s.gating && r.passed);
}

/// long-outage without the resume: a server that keeps no tickets and clients that give up at
/// the timeout and join afresh, which is what every soak run was until ADR-10093 (the asserts
/// are those of the old `long_outage_is_interrupted_once_then_removed_and_rejoins_as_a_new_peer`,
/// unchanged). The 11 s outage is inside the removal timeout: one Interrupted, one Restored, the
/// same peer, no end. The 25 s and 45 s outages are beyond it: one Interrupted, removal at
/// `timeout_ms` of silence on both ends, then a rejoin as a new peer once the link is back.
/// `interrupted_once` and `resume_same_peer` fail on those two, and only on those two: the bars
/// still report a game that does not resume.
#[test]
fn long_outage_without_the_resume_is_removed_and_rejoins_as_a_new_peer() {
    let policy = report::timeout_policy();
    let (record, r) = record_with(
        Profile::named("long-outage").expect("long-outage"),
        3,
        120,
        3,
        &Knobs {
            reconnect: ReconnectPolicy::off(),
            server_resume_grace_ms: Some(0),
            ..Knobs::default()
        },
    );
    let m = r.measures.as_ref().expect("v2 measures");
    assert_eq!(m.outages.len(), 3, "{:?}", m.outages);
    assert_eq!(m.stray_interruptions, 0);
    // Client 0: 11 s, inside the removal timeout.
    assert!(LONG_OUTAGES_MS[0] < policy.timeout_ms && LONG_OUTAGES_MS[0] >= policy.interrupt_ms);
    let kept = outage(&r, 0);
    assert_eq!(
        (kept.interrupted, kept.restored, kept.restore_owed),
        (1, 1, true),
        "{kept:?}"
    );
    assert!(record.ends.iter().all(|e| e.client != 0), "{:?}", record.ends);
    assert!(m.resumes.iter().all(|x| x.client != 0), "{:?}", m.resumes);
    assert_eq!(
        record.clients[0].connected.len(),
        1,
        "client 0 stays the peer it was"
    );
    // Clients 1 and 2: 25 s and 45 s, beyond it.
    let due = LONG_OUTAGE_AT_MS + policy.timeout_ms;
    for client in [1, 2] {
        let len = LONG_OUTAGES_MS[client];
        assert!(len > policy.timeout_ms);
        let o = outage(&r, client);
        assert_eq!(
            (o.start_ms, o.end_ms),
            (LONG_OUTAGE_AT_MS, LONG_OUTAGE_AT_MS + len)
        );
        assert_eq!((o.interrupted, o.restored), (1, 0), "{o:?}");
        // Removed at the timeout, by silence, on both ends; one end each.
        for side in ["client", "server"] {
            let ends: Vec<_> = record
                .ends
                .iter()
                .filter(|e| e.client == client && e.side == side)
                .collect();
            assert_eq!(ends.len(), 1, "client {client} {side}: {:?}", record.ends);
            assert_eq!(ends[0].cause, "timeout", "{:?}", ends[0]);
            assert!(!ends[0].voluntary);
            assert!(
                ends[0].at_ms.abs_diff(due) <= REMOVAL_SLACK_MS,
                "client {client} {side}: removed at {} ms, the policy says {due} ms",
                ends[0].at_ms
            );
        }
        // Back as a new peer, within the grace of the link's return.
        let resume = m
            .resumes
            .iter()
            .find(|x| x.client == client)
            .unwrap_or_else(|| panic!("client {client} rejoined: {:?}", m.resumes));
        assert!(
            resume.after_peer.is_some_and(|p| p != resume.before_peer),
            "{resume:?}"
        );
        let rejoin = r
            .reconnects
            .iter()
            .find(|x| x.client == client)
            .expect("a rejoin");
        assert_eq!(rejoin.link_up_ms, LONG_OUTAGE_AT_MS + len);
        assert!(
            rejoin.connected_ms.is_some_and(
                |t| t >= rejoin.link_up_ms && t <= rejoin.link_up_ms + report::RECONNECT_GRACE_MS
            ),
            "{rejoin:?}"
        );
    }
    assert_eq!(record.ends.len(), 4, "{:?}", record.ends);
    // The report counts one end per lost connection: the client's own.
    assert_eq!(r.ends.len(), 2, "{:?}", r.ends);
    assert_eq!(m.resumes.len(), 2, "{:?}", m.resumes);
    // The bars: the two removed connections are what fails, with these counts.
    let i = bar(&r, "interrupted_once");
    assert!(!i.passed, "{}", i.detail);
    assert_eq!(
        i.detail,
        "client 1 outage 60000-85000 ms: 1 Interrupted, 0 Restored (want 1 and 1); \
         client 2 outage 60000-105000 ms: 1 Interrupted, 0 Restored (want 1 and 1)"
    );
    let s = bar(&r, "resume_same_peer");
    assert!(!s.passed, "{}", s.detail);
    assert_eq!(s.detail.matches("came back as peer").count(), 2, "{}", s.detail);
    assert!(!s.detail.contains("client 0"), "{}", s.detail);
    // The harness bars: both removals are the policy's (outages beyond the timeout), and both
    // clients are back in time.
    let ends = bar(&r, "involuntary_disconnects");
    assert!(ends.passed, "{}", ends.detail);
    assert_eq!(r.metrics.ends_excused, 2, "{}", ends.detail);
    assert!(bar(&r, "reconnect_within_grace").passed);
    // The two outage bars gate since ADR-10093. This run passed the soak until then (its
    // harness bars are green); now it fails it, on these two bars alone: a game that does not
    // resume does not pass.
    let failed: Vec<&str> = r
        .bars
        .iter()
        .filter(|b| b.gating && !b.passed)
        .map(|b| b.id.as_str())
        .collect();
    assert_eq!(failed, ["interrupted_once", "resume_same_peer"]);
    assert!(!r.passed);
}

/// The halves of the resume, one at a time, on the same run. A server that keeps tickets and a
/// client that gives up at the timeout: the fresh join is a new peer while the old one is still
/// held. A client that reconnects for 10 s only: the 25 s outage (5 s left at the timeout) is
/// resumed, the 45 s outage (25 s left) outlasts the window, the client gives up once and joins
/// afresh as a new peer with no Restored. A server whose grace is 10 s with clients that try for
/// 60 s: the same, decided by the server (the hold is gone when the link returns, so the ticket
/// names nobody).
#[test]
fn long_outage_needs_both_halves_of_the_resume() {
    let run = |knobs: Knobs| {
        record_with(
            Profile::named("long-outage").expect("long-outage"),
            3,
            120,
            3,
            &knobs,
        )
    };
    // The client gives up at the timeout.
    let (record, r) = run(Knobs {
        reconnect: ReconnectPolicy::off(),
        ..Knobs::default()
    });
    assert_eq!(
        record.clients.iter().map(|c| c.reconnect_attempts).sum::<u64>(),
        0
    );
    let s = bar(&r, "resume_same_peer");
    assert!(!s.passed, "{}", s.detail);
    assert_eq!(s.detail.matches("came back as peer").count(), 2, "{}", s.detail);
    let i = bar(&r, "interrupted_once");
    assert_eq!(
        i.detail,
        "client 1 outage 60000-85000 ms: 1 Interrupted, 0 Restored (want 1 and 1); \
         client 2 outage 60000-105000 ms: 1 Interrupted, 0 Restored (want 1 and 1)"
    );
    // The client's window is shorter than the 45 s outage.
    let (record, r) = run(Knobs {
        reconnect: ReconnectPolicy {
            window_ms: 10_000,
            ..ReconnectPolicy::default()
        },
        ..Knobs::default()
    });
    assert_eq!(
        record
            .clients
            .iter()
            .map(|c| c.reconnects_given_up)
            .collect::<Vec<_>>(),
        [0, 0, 1]
    );
    let s = bar(&r, "resume_same_peer");
    assert!(
        !s.passed && s.detail.starts_with("client 2 lost peer") && !s.detail.contains(';'),
        "{}",
        s.detail
    );
    let i = bar(&r, "interrupted_once");
    assert_eq!(
        i.detail,
        "client 2 outage 60000-105000 ms: 1 Interrupted, 0 Restored (want 1 and 1)"
    );
    // The server's grace is shorter than the 45 s outage.
    let (record, r) = run(Knobs {
        server_resume_grace_ms: Some(10_000),
        ..Knobs::default()
    });
    assert_eq!(
        record.server.totals.dormant_expired, 1,
        "{:?}",
        record.server.totals
    );
    let s = bar(&r, "resume_same_peer");
    assert!(
        !s.passed && s.detail.starts_with("client 2 lost peer") && !s.detail.contains(';'),
        "{}",
        s.detail
    );
}

/// `interrupted_once` owes no Restored for an outage the client left on purpose inside
/// (ADR-10093): the gate's flappy row has one, client 5's scripted leave at 18 s inside its
/// 15.7-18.7 s outage. A leave outside the outage excuses nothing.
#[test]
fn a_leave_on_purpose_inside_an_outage_owes_no_restored() {
    let (record, r) = record_with(
        Profile::named("flappy").expect("flappy"),
        8,
        60,
        1_592_590_337,
        &Knobs::default(),
    );
    let m = r.measures.as_ref().expect("v2 measures");
    let leave = record.clients[5].leaves[0];
    assert_eq!(record.clients[5].leaves.len(), 1);
    let o = m
        .outages
        .iter()
        .find(|o| o.client == 5 && o.start_ms <= leave && leave <= o.end_ms)
        .unwrap_or_else(|| panic!("client 5 leaves at {leave} ms inside an outage: {:?}", m.outages));
    assert_eq!(
        (o.interrupted, o.restored, o.restore_owed),
        (1, 0, false),
        "{o:?}"
    );
    assert!(bar(&r, "interrupted_once").passed);
    // The same record with the leave moved out of the outage: the Restored is owed and missing.
    let mut moved = record.clone();
    moved.clients[5].leaves = vec![o.end_ms + 1];
    let m = report::measure(&moved);
    let o = m
        .outages
        .iter()
        .find(|x| x.client == 5 && x.start_ms == o.start_ms)
        .expect("the same outage");
    assert_eq!((o.interrupted, o.restored, o.restore_owed), (1, 0, true), "{o:?}");
    let judged = report::evaluate(&moved, 0);
    let i = bar(&judged, "interrupted_once");
    assert!(
        !i.passed && i.detail.starts_with("client 5 outage ") && i.detail.ends_with("(want 1 and 1)"),
        "{}",
        i.detail
    );
}

/// The outage bars on deliberately broken timeout policies, judged against the engine's own.
/// Removal far too early (5 s instead of 20 s): the 11 s outage the engine's policy survives now
/// ends client 0's connection, and the other two are removed fifteen seconds before the policy's
/// time. `involuntary_disconnects` excuses none of the three (an outage excuses only the
/// policy's own removal); `interrupted_once` and `resume_same_peer` report client 0 too when the
/// game does not resume, and pass when it does (the resume hides nothing from the gating bar). No
/// interruption notice (`interrupt_ms` one millisecond short of removal): `interrupted_once`
/// reports the outage nobody was told about.
#[test]
fn a_broken_timeout_policy_fails_the_outage_bars_on_long_outage() {
    let policy = TimeoutPolicy::default();
    let early = Knobs {
        timeouts: TimeoutPolicy {
            timeout_ms: 5_000,
            ..policy
        },
        ..Knobs::default()
    };
    let r = run_with(
        Profile::named("long-outage").expect("long-outage"),
        3,
        120,
        3,
        &early,
    );
    let ends = bar(&r, "involuntary_disconnects");
    assert!(!ends.passed && ends.gating, "{}", ends.detail);
    for client in 0..3 {
        assert!(
            ends.detail
                .contains(&format!("client {client} ended on the client side at ")),
            "{}",
            ends.detail
        );
    }
    assert_eq!(ends.detail.matches("ms: timeout").count(), 3, "{}", ends.detail);
    assert_eq!(r.metrics.ends_excused, 0, "{}", ends.detail);
    let end = r
        .ends
        .iter()
        .find(|e| e.client == 0 && e.side == "client")
        .expect("client 0's end");
    assert!(
        end.at_ms.abs_diff(LONG_OUTAGE_AT_MS + 5_000) <= REMOVAL_SLACK_MS,
        "{end:?}"
    );
    // The early removal is the gating bar's to report. Each of the three clients still resumes
    // (the server holds it for 60 s from its early timeout, the client tries for as long), so it
    // is told once that the link is back and keeps its peer id (ADR-10093; before the resume
    // these two bars failed on client 0 as well).
    let i = bar(&r, "interrupted_once");
    assert!(i.passed, "{}", i.detail);
    let s = bar(&r, "resume_same_peer");
    assert!(s.passed, "{}", s.detail);
    assert_eq!(s.detail, "3 involuntary rejoins, each with its old peer id");
    // Without the resume they report it too, as they did.
    let r = run_with(
        Profile::named("long-outage").expect("long-outage"),
        3,
        120,
        3,
        &Knobs {
            reconnect: ReconnectPolicy::off(),
            server_resume_grace_ms: Some(0),
            ..early
        },
    );
    let i = bar(&r, "interrupted_once");
    assert!(
        !i.passed
            && i.detail
                .contains("client 0 outage 60000-71000 ms: 1 Interrupted, 0 Restored (want 1 and 1)"),
        "{}",
        i.detail
    );
    let s = bar(&r, "resume_same_peer");
    assert!(
        !s.passed && s.detail.contains("client 0 lost peer"),
        "{}",
        s.detail
    );
    assert_eq!(s.detail.matches("came back as peer").count(), 3, "{}", s.detail);

    let mute = Knobs {
        timeouts: TimeoutPolicy {
            interrupt_ms: policy.timeout_ms - 1,
            ..policy
        },
        ..Knobs::default()
    };
    let r = run_with(
        Profile::named("long-outage").expect("long-outage"),
        3,
        120,
        3,
        &mute,
    );
    let i = bar(&r, "interrupted_once");
    assert!(
        !i.passed
            && i.detail
                .contains("client 0 outage 60000-71000 ms: 0 Interrupted, 0 Restored (want 1 and 1)"),
        "{}",
        i.detail
    );
    // The connection itself survived the 11 s: nothing ended, nothing to excuse for client 0.
    assert!(r.ends.iter().all(|e| e.client != 0), "{:?}", r.ends);
    assert!(bar(&r, "involuntary_disconnects").passed);
}

/// Random 5 % loss on a fat pipe is not congestion: the snapshot budget stays at least 90 % of a
/// clean run's (F7 at v0.5.0: 8 of 140 entities; flipped by its owning lane N202, ADR-8301).
#[test]
fn random_loss_kept_passes_on_lossy_fat_pipe() {
    let r = run("lossy-fat-pipe", 4, 20, 4);
    let b = bar(&r, "random_loss_kept");
    assert!(b.passed, "{}", b.detail);
}

/// The bulk transfer's 256 KiB no longer delays the commands: it rides the bulk stream, which
/// yields to gameplay (spec 120 § 5.1, ADR-8302). On the v0.5.0 engine, with one reliable stream,
/// this bar failed (every command waited behind the transfer).
#[test]
fn rpc_p95_with_bulk_passes_on_congested_bulk() {
    let r = run("congested+bulk", 4, 60, 6);
    let m = r
        .measures
        .as_ref()
        .and_then(|m| m.rpc_with_bulk.clone())
        .expect("bulk measured");
    assert_eq!(m.bulk_start_ms, 30_000);
    assert!(m.control_n > 0 && m.bulk_n > 0, "{m:?}");
    let b = bar(&r, "rpc_p95_with_bulk");
    assert!(b.passed, "{}", b.detail);
    let groups = &r.measures.as_ref().expect("measures").staleness;
    assert!(
        groups.iter().all(|g| g.limit_ticks == 8),
        "congested clients keep their 8-tick limit: {groups:?}"
    );
}

/// ADR-8692: the seeds the up leg failed. During the transfer each client's delay detector reads
/// the size of the packets carrying its acknowledgments (an acknowledgment riding alone in a small
/// packet on the 256 kbit/s down link sets a lower minimum) as a queue and cuts its app-limited
/// upload target to ~17 kB/s. With the pacer's old two-packet floor a 4 KiB command then trickled
/// out at that target: these five seeds put the transfer window's p95 at 1.55-2.22x the
/// control's. With the four-packet floor the command leaves whole and all five pass.
#[test]
fn rpc_p95_with_bulk_passes_on_the_seeds_the_up_leg_failed() {
    let mut failed = Vec::new();
    for seed in [1, 2, 12, 21, 24] {
        let r = run("congested+bulk", 4, 60, seed);
        let b = bar(&r, "rpc_p95_with_bulk");
        if !b.passed {
            failed.push(format!("seed {seed}: {}", b.detail));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// ADR-8859: the seeds ADR-8692 left over the bar inside 1-104 (42, 77) and the one the up leg's
/// fix alone leaves (640). On 42 and 77 a 4 KiB command met a pacing bucket its client's own
/// input bundles had drained and trickled out at a cut target (last fragment up to 304 ms at the
/// pacer); on 640 one round that acknowledged a single packet cut the server's target to a
/// quarter of the link and every command reply waited for the pacer. A message's burst is now
/// the message's, and a cut reads the latest second.
#[test]
fn rpc_p95_with_bulk_passes_on_the_residual_seeds() {
    let mut failed = Vec::new();
    for seed in [42, 77, 640] {
        let r = run("congested+bulk", 4, 60, seed);
        let b = bar(&r, "rpc_p95_with_bulk");
        if !b.passed {
            failed.push(format!("seed {seed}: {}", b.detail));
        }
    }
    assert!(failed.is_empty(), "{failed:#?}");
}

/// Forged ConnectRequests from 1,000 /24s, and the honest joins: the bar reports the fraction.
#[test]
fn join_under_flood_is_measured() {
    let r = run("forged-handshake", 4, 8, 8);
    let m = r.measures.as_ref().expect("v2 measures");
    assert!(m.forged_sent >= 100_000 * 8, "{}", m.forged_sent);
    assert!(m.join_attempts >= 4, "{m:?}");
    let b = bar(&r, "join_under_flood");
    assert!(b.skipped.is_none(), "{}", b.detail);
}

/// The flood profile's peer sends valid sealed records at 100,000 datagrams/s from 10 s, and the
/// hostile bars judge it.
#[test]
fn flood_peer_is_judged_by_the_hostile_bars() {
    let r = run("flood", 3, 12, 2);
    let h = r.hostile.as_ref().expect("flood record");
    assert_eq!(h.sessions.len(), 1);
    assert_eq!(h.sessions[0].kind, "sealed_flood");
    assert!(h.sessions[0].acts > 100_000, "{:?}", h.sessions[0]);
    assert_eq!(
        (h.replays_sent, h.garbage_sent),
        (0, 0),
        "no raw attacks on flood"
    );
    bar(&r, "hostile_dropped");
    bar(&r, "others_unaffected");
}

/// The rtt-extreme peer and the mixed profile's tick bar are reported; desync bars wait for
/// lockstep.
#[test]
fn every_profile_reports_its_bars() {
    let r = run("rtt-extreme", 8, 20, 1);
    let b = bar(&r, "never_kicked_for_ping");
    assert!(b.skipped.is_none(), "{}", b.detail);
    let mixed = run("mixed", 8, 10, 1);
    assert!(bar(&mixed, "tick_p99").skipped.is_none());
    let groups = &mixed.measures.as_ref().expect("measures").staleness;
    assert!(
        groups.iter().all(|g| g.limit_ticks == 4),
        "8 mixed clients hold no congested one: {groups:?}"
    );
    let ids: Vec<&str> = report::bars_for(&Profile::named("lockstep-mix").expect("profile"));
    assert_eq!(ids, vec!["staleness_p99", "desync_zero", "lockstep_upload"]);
}

/// A v2 report round-trips through `result.json` and the reader.
#[test]
fn a_v2_report_round_trips() {
    let res = run("clean", 2, 6, 1);
    let report = SoakReport::new("custom", "t", vec![res]);
    assert_eq!(report.version, 2);
    let text = serde_json::to_string(&report).expect("json");
    let back = SoakReport::from_json(&text).expect("v2 loads");
    assert_eq!(back, report);
}
