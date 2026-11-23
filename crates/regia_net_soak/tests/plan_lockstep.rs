// SPDX-License-Identifier: MPL-2.0
//! `lockstep-mix` through the soak's plan (spec 120 § 15.1-15.2; ADR-8209): `cargo xtask net-soak`
//! runs the profile as a lockstep match and judges `desync_zero` and `lockstep_upload` on it.

use regia_net::lockstep::{DesyncVerdict, LockstepConfig, VerdictKind};
use regia_net_soak::lockstep::{
    InputMix, LockstepRecord, SoakHost, UPLOAD_P95_LIMIT_BPS, UPLOAD_PEAK_LIMIT_BPS, run_lockstep,
};
use regia_net_soak::profiles::Profile;
use regia_net_soak::report::{
    self, Bar, LOCKSTEP_KEEP_UP_FRAMES, LOCKSTEP_NO_SNAPSHOTS, LOCKSTEP_UPLOAD_SEATS_ACTION,
    LOCKSTEP_UPLOAD_SEATS_BOTS, LOCKSTEP_UPLOAD_SEATS_ORDERS, NOT_A_LOCKSTEP_RESULT, ScenarioResult,
    SoakReport,
};
use regia_net_soak::workload::{Scenario, ServerMode};
use regia_net_soak::{custom_plan, lockstep_scenario, no_heap_probe, run_plan};

fn bar<'a>(r: &'a ScenarioResult, id: &str) -> &'a Bar {
    r.bars
        .iter()
        .find(|b| b.id == id)
        .unwrap_or_else(|| panic!("{}: no bar {id}", r.label))
}

fn planned(profile: &str, seats: usize, mode: ServerMode, seconds: u64) -> ScenarioResult {
    let plan = custom_plan(&[profile.to_string()], &[seats], &[mode], 7, seconds * 1000).expect("plan");
    run_plan(&plan, 1, no_heap_probe)
        .expect("run")
        .pop()
        .expect("one result")
}

fn scenario(profile: &str, seats: usize, seconds: u64) -> Scenario {
    Scenario {
        profile: Profile::named(profile).expect("profile"),
        clients: seats,
        mode: ServerMode::Dedicated,
        seed: 7,
        duration_ms: seconds * 1000,
        wire: Default::default(),
    }
}

/// A real short match and its record: 4 seats on a dedicated host for 8 virtual seconds.
fn short_record() -> (Scenario, LockstepRecord) {
    let s = scenario("lockstep-mix", 4, 8);
    let r = run_lockstep(&lockstep_scenario(&s).expect("a lockstep profile")).expect("the run starts");
    (s, r)
}

fn upload_bar(s: &Scenario, r: &LockstepRecord) -> Bar {
    bar(&report::evaluate_lockstep(s, r, 0, 0), "lockstep_upload").clone()
}

fn desync_bar(s: &Scenario, r: &LockstepRecord) -> Bar {
    bar(&report::evaluate_lockstep(s, r, 0, 0), "desync_zero").clone()
}

/// The plan runs `lockstep-mix` (it was skipped with `needs: lockstep`), on a dedicated host and
/// on a listen host, and judges its two bars on the run.
#[test]
fn lockstep_mix_runs_through_the_plan_and_its_two_bars_are_judged() {
    for mode in [ServerMode::Dedicated, ServerMode::Listen] {
        let r = planned("lockstep-mix", 4, mode, 8);
        assert_eq!(r.skipped, None, "{}: the run happens", r.label);
        for id in ["desync_zero", "lockstep_upload"] {
            let b = bar(&r, id);
            assert!(b.skipped.is_none(), "{} {id} is judged: {}", r.label, b.detail);
            assert!(b.passed, "{} {id}: {}", r.label, b.detail);
        }
        assert!(r.passed, "{}: {:?}", r.label, r.bars);
    }
}

/// The action variant runs through the plan too, on the action input mix: with the same seed and
/// links its host uploads more than the order-driven match does.
#[test]
fn the_action_variant_runs_through_the_plan_and_costs_more_upload() {
    let orders = planned("lockstep-mix", 8, ServerMode::Dedicated, 12);
    let action = planned("lockstep-mix+action", 8, ServerMode::Dedicated, 12);
    let (o, a) = (
        orders.lockstep.as_ref().expect("orders numbers"),
        action.lockstep.as_ref().expect("action numbers"),
    );
    assert_eq!((o.mix.as_str(), a.mix.as_str()), ("orders", "action"));
    assert_eq!((o.seats, a.seats), (8, 8));
    assert!(
        a.upload_mean_bps > o.upload_mean_bps,
        "action {} bit/s, orders {} bit/s",
        a.upload_mean_bps,
        o.upload_mean_bps
    );
    for r in [&orders, &action] {
        assert!(
            bar(r, "desync_zero").passed,
            "{}: {}",
            r.label,
            bar(r, "desync_zero").detail
        );
        assert!(bar(r, "lockstep_upload").skipped.is_none(), "{}", r.label);
    }
    assert_eq!(action.label, "lockstep-mix+action/8/dedicated");
    assert_eq!(
        action.reproduce,
        "cargo xtask net-soak --profiles lockstep-mix+action --clients 8 --modes dedicated --seed 7 --seconds 12"
    );
}

/// What the plan hands the lockstep soak: the scenario's clients as seats, its seed and length,
/// an observer host on a dedicated server and a player host on a listen server, the default
/// settings, and the input mix the profile name says.
#[test]
fn a_planned_lockstep_run_takes_its_seats_host_and_input_mix_from_the_scenario() {
    let mut s = scenario("lockstep-mix", 12, 300);
    s.seed = 99;
    let l = lockstep_scenario(&s).expect("lockstep-mix is a lockstep profile");
    assert_eq!((l.seats, l.seed, l.duration_ms), (12, 99, 300_000));
    assert_eq!((l.host, l.mix), (SoakHost::Observer, InputMix::ORDERS));
    assert_eq!(l.config, LockstepConfig::default());
    s.mode = ServerMode::Listen;
    assert_eq!(lockstep_scenario(&s).expect("listen").host, SoakHost::Listen);
    let action = lockstep_scenario(&scenario("lockstep-mix+action", 24, 300)).expect("the variant");
    assert_eq!((action.seats, action.mix), (24, InputMix::ACTION));
    let bots = lockstep_scenario(&scenario("lockstep-mix+bots", 40, 600)).expect("bots");
    assert_eq!((bots.seats, bots.mix), (40, InputMix::WANDER_FIRE));
    for seat in 0..24 {
        assert_eq!(
            action.profile.client_link(seat, 300_000),
            l.profile.client_link(seat, 300_000),
            "seat {seat}: the action variant runs on the same links"
        );
    }
    assert!(
        lockstep_scenario(&scenario("mixed", 8, 60)).is_none(),
        "a replication profile is not a lockstep match"
    );
}

/// The result of a lockstep run carries the record's numbers, and the bars every run has are
/// judged on the seats.
#[test]
fn the_result_carries_the_runs_numbers_and_the_harness_bars_are_judged_on_the_seats() {
    let (s, r) = short_record();
    let res = report::evaluate_lockstep(&s, &r, 1_234, 5_678);
    let l = res.lockstep.as_ref().expect("lockstep numbers");
    assert_eq!(
        (l.seats, l.mix.as_str(), l.frames_built),
        (4, "orders", r.frames_built)
    );
    assert!(
        l.frames_built > 400,
        "8 s at 60 Hz less the connects: {}",
        l.frames_built
    );
    assert_eq!(
        (
            l.upload_mean_bps,
            l.upload_p50_bps,
            l.upload_p95_bps,
            l.upload_peak_bps,
            l.upload_seconds,
            l.upload_seconds_over,
            l.upload_datagrams_per_s
        ),
        (
            r.host_upload_bps,
            r.host_upload_p50_bps,
            r.host_upload_p95_bps,
            r.host_upload_peak_bps,
            r.host_upload_seconds,
            r.host_upload_seconds_over,
            r.host_upload_datagrams_per_s
        )
    );
    assert!(l.upload_seconds >= 3, "{} whole seconds", l.upload_seconds);
    assert!(
        l.upload_mean_bps > 0
            && l.upload_p50_bps <= l.upload_p95_bps
            && l.upload_p95_bps <= l.upload_peak_bps
    );
    assert_eq!(
        (l.desync_verdicts, l.resyncs, l.in_step, l.unresolved),
        (0, 0, true, false)
    );
    assert!(l.checkpoints_agreed > 0, "state hashes were compared");
    assert_eq!(
        (l.delay_ticks, l.delay_raises),
        (r.host.delay_ticks, r.host.delay_raises)
    );
    assert_eq!(
        (
            l.seats_connected,
            l.never_connected.len(),
            l.ended.len(),
            l.behind.len(),
            l.refused
        ),
        (4, 0, 0, 0, 0)
    );
    assert_eq!(
        (res.wall_ms, res.metrics.peak_heap_bytes, res.metrics.joined),
        (1_234, 5_678, 4)
    );
    assert!(res.measures.is_none() && res.skipped.is_none());
    for id in [
        "involuntary_disconnects",
        "reconnect_within_grace",
        "no_wedged_client",
        "desync_zero",
        "lockstep_upload",
    ] {
        let b = bar(&res, id);
        assert!(b.passed && b.skipped.is_none(), "{id}: {}", b.detail);
    }
    assert!(
        bar(&res, "reconnect_within_grace")
            .detail
            .contains("not applicable")
    );
    // The bars that read snapshots do not apply to a lockstep match.
    assert_eq!(
        bar(&res, "staleness_p99").skipped.as_deref(),
        Some(LOCKSTEP_NO_SNAPSHOTS)
    );
    assert!(res.passed);
    assert_eq!(
        res.reproduce,
        "cargo xtask net-soak --profiles lockstep-mix --clients 4 --modes dedicated --seed 7 --seconds 8"
    );
    // On a listen host seat 0 is the host's own: three remote seats, four players.
    let mut listen = s.clone();
    listen.mode = ServerMode::Listen;
    let lr = run_lockstep(&lockstep_scenario(&listen).expect("listen")).expect("run");
    let lres = report::evaluate_lockstep(&listen, &lr, 0, 0);
    let ll = lres.lockstep.as_ref().expect("lockstep numbers");
    assert_eq!((ll.seats, ll.seats_connected, lres.metrics.joined), (4, 3, 4));
    assert!(lres.passed, "{:?}", lres.bars);
}

/// `lockstep_upload` (spec 120 § 7.5 as amended): the 95th-percentile second and the busiest
/// second each have a limit. A record one bit per second over either fails the bar, the limits
/// themselves pass, and a run without a whole measured second is not a pass.
#[test]
fn a_planted_upload_over_either_limit_fails_lockstep_upload_and_the_limits_themselves_pass() {
    let (s, mut r) = short_record();
    r.host_upload_p95_bps = UPLOAD_P95_LIMIT_BPS;
    r.host_upload_peak_bps = UPLOAD_PEAK_LIMIT_BPS;
    let at = upload_bar(&s, &r);
    assert!(at.passed && at.skipped.is_none(), "at both limits: {}", at.detail);

    r.host_upload_p95_bps = UPLOAD_P95_LIMIT_BPS + 1;
    let b = upload_bar(&s, &r);
    assert!(!b.passed && b.skipped.is_none(), "{}", b.detail);
    assert!(
        b.detail
            .starts_with("the 95th-percentile second over the limit: 4 seats, orders mix")
            && b.detail
                .contains("95th-percentile second 4000001 bit/s (limit 4000000)"),
        "{}",
        b.detail
    );

    r.host_upload_p95_bps = UPLOAD_P95_LIMIT_BPS;
    r.host_upload_peak_bps = UPLOAD_PEAK_LIMIT_BPS + 1;
    let b = upload_bar(&s, &r);
    assert!(!b.passed, "{}", b.detail);
    assert!(
        b.detail.starts_with("the busiest second over the limit:")
            && b.detail.contains("busiest second 4400001 bit/s (limit 4400000)"),
        "{}",
        b.detail
    );

    r.host_upload_p95_bps = UPLOAD_P95_LIMIT_BPS + 1;
    let b = upload_bar(&s, &r);
    assert!(
        !b.passed
            && b.detail
                .starts_with("the 95th-percentile second and the busiest second over the limit:"),
        "{}",
        b.detail
    );

    // The bar decides the run's verdict once it is on the gating list.
    let mut res = report::evaluate_lockstep(&s, &r, 0, 0);
    report::apply_gating(&mut res, &["lockstep_upload".to_string()]);
    assert!(!res.passed && bar(&res, "lockstep_upload").gating);
    report::apply_gating(&mut res, &[]);
    assert!(res.passed, "a bar that does not gate does not decide the verdict");

    // Nothing measured is not a pass.
    r.host_upload_p95_bps = 0;
    r.host_upload_peak_bps = 0;
    r.host_upload_seconds = 0;
    let b = upload_bar(&s, &r);
    assert!(
        !b.passed
            && b.detail
                .starts_with("no whole second of host upload was measured"),
        "{}",
        b.detail
    );
}

/// The bar is stated for a seat count per input mix: a run with more seats reports its numbers
/// and skips the bar; a run at the count is judged. The run length is in the detail.
#[test]
fn the_upload_bar_is_judged_up_to_the_seat_count_it_is_stated_for_on_each_mix() {
    let (_, mut r) = short_record();
    // Over the limit wherever the bar is judged.
    r.host_upload_p95_bps = UPLOAD_P95_LIMIT_BPS + 1;
    for (profile, mix, stated) in [
        ("lockstep-mix", "orders", LOCKSTEP_UPLOAD_SEATS_ORDERS),
        ("lockstep-mix+action", "action", LOCKSTEP_UPLOAD_SEATS_ACTION),
        ("lockstep-mix+bots", "bots", LOCKSTEP_UPLOAD_SEATS_BOTS),
    ] {
        let at = upload_bar(&scenario(profile, stated, 300), &r);
        assert!(
            !at.passed && at.skipped.is_none(),
            "{profile} at {stated} seats is judged: {}",
            at.detail
        );
        assert!(
            at.detail
                .contains(&format!("{stated} seats, {mix} mix, 300 s run:")),
            "{}",
            at.detail
        );
        let beyond = upload_bar(&scenario(profile, stated + 1, 300), &r);
        assert!(beyond.passed, "a skipped bar does not fail the run");
        assert_eq!(
            beyond.skipped,
            Some(format!(
                "beyond the {stated} seats the bar is stated for on the {mix} mix"
            ))
        );
        assert!(
            beyond.detail.contains("measured:")
                && beyond
                    .detail
                    .contains("95th-percentile second 4000001 bit/s (limit 4000000)"),
            "the numbers are still reported: {}",
            beyond.detail
        );
    }
    // A custom run of another length is judged on its seconds and says so.
    r.host_upload_p95_bps = UPLOAD_P95_LIMIT_BPS;
    let short = upload_bar(&scenario("lockstep-mix", 8, 60), &r);
    assert!(short.passed && short.skipped.is_none());
    assert!(
        short
            .detail
            .contains("8 seats, orders mix, 60 s run (the bar is stated over 300 s):"),
        "{}",
        short.detail
    );
}

/// `desync_zero`: any verdict, any state transfer (a seat's or the host's), a removed seat,
/// undecided votes or simulations out of step fail it; so does a run that compared nothing.
#[test]
fn a_verdict_a_state_transfer_or_simulations_out_of_step_fail_desync_zero() {
    let (s, clean) = short_record();
    let ok = desync_bar(&s, &clean);
    assert!(ok.passed && ok.skipped.is_none(), "{}", ok.detail);
    assert!(
        ok.detail.starts_with("0 desync verdicts and 0 state transfers;"),
        "{}",
        ok.detail
    );

    let mut r = clean.clone();
    r.verdicts.push(DesyncVerdict {
        tick: 450,
        last_agreed: Some(420),
        kind: VerdictKind::SelfHealed,
        hash: Some(1),
        agreeing: Vec::new(),
        diverged: Vec::new(),
        self_healed: Vec::new(),
        abstained: Vec::new(),
        host_diverged: false,
        source: None,
    });
    let b = desync_bar(&s, &r);
    assert!(
        !b.passed && b.detail.contains("1 desync verdicts"),
        "{}",
        b.detail
    );

    let mut r = clean.clone();
    r.seats[0].resyncs = 1;
    let b = desync_bar(&s, &r);
    assert!(
        !b.passed && b.detail.contains("1 state transfers"),
        "{}",
        b.detail
    );

    let mut r = clean.clone();
    r.host_resyncs = 2;
    let b = desync_bar(&s, &r);
    assert!(
        !b.passed && b.detail.contains("2 state transfers"),
        "{}",
        b.detail
    );

    let mut r = clean.clone();
    r.removed.push(3);
    let b = desync_bar(&s, &r);
    assert!(
        !b.passed && b.detail.contains("seats [3] were removed"),
        "{}",
        b.detail
    );

    let mut r = clean.clone();
    r.unresolved = true;
    let b = desync_bar(&s, &r);
    assert!(
        !b.passed && b.detail.contains("could not be decided"),
        "{}",
        b.detail
    );

    let mut r = clean.clone();
    r.in_step = false;
    let b = desync_bar(&s, &r);
    assert!(!b.passed && b.detail.contains("do not agree"), "{}", b.detail);

    let mut r = clean.clone();
    r.host.checkpoints_agreed = 0;
    let b = desync_bar(&s, &r);
    assert!(
        !b.passed && b.detail.contains("nothing was compared"),
        "{}",
        b.detail
    );
}

/// The harness's bars on a lockstep run: a seat whose connection ended fails
/// `involuntary_disconnects`; a seat that never connected, fell behind the frames or was dropped
/// as lagging fails `no_wedged_client`.
#[test]
fn a_seat_that_ended_never_connected_or_fell_behind_fails_the_harness_bars() {
    let (s, clean) = short_record();
    let judge = |r: &LockstepRecord| report::evaluate_lockstep(&s, r, 0, 0);

    let mut r = clean.clone();
    r.seats[1].ended = Some("timeout".into());
    let res = judge(&r);
    let b = bar(&res, "involuntary_disconnects");
    assert!(
        !b.passed && b.detail == "seat 1's connection ended: timeout",
        "{}",
        b.detail
    );
    assert!(!res.passed, "the harness's bars gate");
    assert_eq!(res.metrics.ends_by_cause.get("client:timeout"), Some(&1));

    let mut r = clean.clone();
    r.seats[2].connected_ms = None;
    let res = judge(&r);
    let b = bar(&res, "no_wedged_client");
    assert!(!b.passed && b.detail == "seat 2 never connected", "{}", b.detail);
    assert!(!res.passed);

    // The frames still in flight at the end are not a wedge; one frame more is.
    let mut r = clean.clone();
    r.seats[3].frames_stepped = r.frames_built - LOCKSTEP_KEEP_UP_FRAMES;
    assert!(bar(&judge(&r), "no_wedged_client").passed);
    r.seats[3].frames_stepped -= 1;
    let res = judge(&r);
    let b = bar(&res, "no_wedged_client");
    assert!(
        !b.passed
            && b.detail.starts_with(&format!(
                "seat 3 stepped {} of the {} frames built",
                r.seats[3].frames_stepped, r.frames_built
            )),
        "{}",
        b.detail
    );

    // A seat the host dropped as lagging stopped stepping: one finding, not two.
    let mut r = clean.clone();
    r.lagging.push(0);
    r.seats[0].frames_stepped = 10;
    let b = bar(&judge(&r), "no_wedged_client").clone();
    assert!(
        !b.passed && b.detail == "seat 0 fell a frame log behind and was dropped from the frame stream",
        "{}",
        b.detail
    );

    // A seat removed for divergence stops stepping too: that is `desync_zero`'s finding.
    let mut r = clean.clone();
    r.removed.push(3);
    r.seats[3].frames_stepped = 10;
    let res = judge(&r);
    assert!(
        bar(&res, "no_wedged_client").passed,
        "{}",
        bar(&res, "no_wedged_client").detail
    );
    assert!(!bar(&res, "desync_zero").passed);
}

/// The lockstep block is additive in `result.json`: the format version stays 2, a report with a
/// lockstep run round-trips, a v2 result written before the plan ran lockstep still loads, and the
/// summary has the lockstep table.
#[test]
fn a_report_with_a_lockstep_run_round_trips_and_an_older_result_still_loads() {
    let (s, r) = short_record();
    let res = report::evaluate_lockstep(&s, &r, 10, 0);
    let clean = planned("clean", 2, ServerMode::Dedicated, 6);
    let report = SoakReport::new("custom", "t", vec![res, clean]);
    assert_eq!(report.version, 2);
    let text = serde_json::to_string(&report).expect("json");
    let back = SoakReport::from_json(&text).expect("a report with a lockstep run loads");
    assert_eq!(back, report);

    let mut v: serde_json::Value = serde_json::from_str(&text).expect("value");
    for result in v["results"].as_array_mut().expect("results") {
        assert!(
            result
                .as_object_mut()
                .expect("object")
                .remove("lockstep")
                .is_some()
        );
    }
    let old = SoakReport::from_json(&v.to_string()).expect("a result without the lockstep key loads");
    assert!(old.results.iter().all(|r| r.lockstep.is_none()));
    assert_eq!(
        old.results[0].bars, report.results[0].bars,
        "bars read as written"
    );
    assert_eq!(
        old.results[1], report.results[1],
        "a replication run is unchanged"
    );
    // Judged again from such a result, the lockstep bars are skipped, not guessed.
    let again = report::spec120_bars(&old.results[0]);
    for id in ["desync_zero", "lockstep_upload"] {
        let b = again.iter().find(|b| b.id == id).expect("bar");
        assert_eq!(b.skipped.as_deref(), Some(NOT_A_LOCKSTEP_RESULT), "{id}");
    }

    let md = report.summary_md();
    assert!(md.contains("## Lockstep runs (ADR-8209)"), "{md}");
    assert!(
        md.contains(&format!(
            "| lockstep-mix/4/dedicated | pass | 4 (orders) | {} |",
            r.frames_built
        )),
        "{md}"
    );
    assert!(
        md.contains("| lockstep-mix/4/dedicated | pass | 4/4 | 0 | - |"),
        "{md}"
    );
    let replication_only = SoakReport::new("custom", "t", vec![report.results[1].clone()]);
    assert!(!replication_only.summary_md().contains("Lockstep runs"));
}

/// `lockstep_upload` gates (flipped in `xtask/src/net_soak.rs` `BARS` by ADR-8209): by the soak's
/// own default list, without xtask's environment, a lockstep run over the limit fails, and the
/// report counts it as a new failure. `desync_zero` waits for its lane and does not gate.
#[test]
fn lockstep_upload_gates_by_default_and_desync_zero_does_not() {
    assert!(report::GATING_BARS.contains(&"lockstep_upload"));
    assert!(!report::GATING_BARS.contains(&"desync_zero"));
    let (s, mut r) = short_record();
    let ok = report::evaluate_lockstep(&s, &r, 0, 0);
    assert!(ok.passed && bar(&ok, "lockstep_upload").gating && !bar(&ok, "desync_zero").gating);
    r.host_upload_p95_bps = UPLOAD_P95_LIMIT_BPS + 1;
    let over = report::evaluate_lockstep(&s, &r, 0, 0);
    assert!(!over.passed, "{:?}", over.bars);
    let report = SoakReport::new("custom", "t", vec![over]);
    assert!(!report.passed);
    assert_eq!(
        (
            report.new_failures,
            report.known_blocker_failures,
            report.non_gating_failures
        ),
        (1, 0, 0)
    );
    let md = report.summary_md();
    assert!(
        md.contains(
            "- **lockstep-mix/4/dedicated** `lockstep_upload` (NEW; seed 7): the 95th-percentile second over the \
             limit"
        ),
        "{md}"
    );
    // Out of step does not fail the run until the desync lane flips its bar; it is reported.
    r.host_upload_p95_bps = UPLOAD_P95_LIMIT_BPS;
    r.in_step = false;
    let split = report::evaluate_lockstep(&s, &r, 0, 0);
    assert!(split.passed && !bar(&split, "desync_zero").passed);
    assert_eq!(SoakReport::new("custom", "t", vec![split]).non_gating_failures, 1);
}

/// The plan's lockstep block carries the delay over the run and the seats the host flagged for
/// holding their inputs back (ADR-8209); an honest run flags nobody, and a flag in the record is
/// in the block and in the summary table.
#[test]
fn the_lockstep_block_carries_the_delay_over_the_run_and_the_late_stamp_flags() {
    let (s, clean) = short_record();
    let l = report::measure_lockstep(&s, &clean);
    assert_eq!(
        (l.delay_mean_milliticks, l.delay_max_ticks),
        (clean.delay_mean_milliticks, clean.delay_max_ticks)
    );
    assert!(
        l.delay_max_ticks >= l.delay_ticks.min(LockstepConfig::default().initial_delay_ticks),
        "{l:?}"
    );
    assert!(l.delay_mean_milliticks > 0, "the run built frames under a delay");
    assert!(
        l.late_stamp_flagged.is_empty(),
        "no honest seat is flagged: {:?}",
        l.late_stamp_flagged
    );
    let mut flagged = clean.clone();
    let seat = flagged.host.seats.last().map(|row| row.seat).expect("a seat row");
    if let Some(row) = flagged.host.seats.last_mut() {
        row.late_stamp_flags = 1;
    }
    let res = report::evaluate_lockstep(&s, &flagged, 0, 0);
    assert_eq!(
        res.lockstep.as_ref().map(|l| l.late_stamp_flagged.clone()),
        Some(vec![seat])
    );
    let md = SoakReport::new("custom", "", vec![res]).summary_md();
    assert!(md.contains("Seats flagged for late stamping"), "{md}");
    assert!(md.contains(&format!("| [{seat}] |")), "{md}");
}
