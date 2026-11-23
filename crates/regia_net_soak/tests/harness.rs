// SPDX-License-Identifier: MPL-2.0
//! The harness's own checks (ADR-7899): runs are deterministic from their seed, a clean link
//! loses nothing, and an outage beyond the engine's removal timeout is excused and rejoined
//! (and a broken timeout policy is not).

use regia_net::TimeoutPolicy;
use regia_net_soak::profiles::{Profile, flap_long_outage_ms};
use regia_net_soak::report;
use regia_net_soak::workload::{self, InputLead, Knobs, Scenario, ServerMode};
use regia_net_soak::{DEFAULT_SEED, no_heap_probe, run_from_env};

fn short(profile: &str, clients: usize, seconds: u64, seed: u64) -> Scenario {
    Scenario {
        profile: Profile::named(profile).expect("known profile"),
        clients,
        mode: ServerMode::Dedicated,
        seed,
        duration_ms: seconds * 1000,
        wire: Default::default(),
    }
}

/// The same seed replays the same run: every count and every event time, everything but the
/// wall-clock times and the process's memory (`RunRecord::clear_host_measurements`).
#[test]
fn a_run_is_deterministic_from_its_seed() {
    let s = short("bad-wifi", 4, 12, 42);
    let mut a = workload::run(&s).expect("run");
    let mut b = workload::run(&s).expect("run");
    for r in [&mut a, &mut b] {
        r.clear_host_measurements();
    }
    assert_eq!(a, b);
}

/// On a clean link every client joins, inputs and snapshots flow, every command is acknowledged and
/// the network drops nothing.
#[test]
fn clean_link_has_every_input_and_no_wedge() {
    let s = short("clean", 4, 10, 1);
    let r = workload::run(&s).expect("run");
    let res = report::evaluate(&r, 0);
    assert_eq!(res.metrics.joined, 4, "{res:#?}");
    assert!(res.metrics.server_ticks_with_input > 4 * 200, "{:?}", res.metrics);
    assert!(r.clients.iter().all(|c| c.snapshot_parts > 100));
    let ends = res
        .bars
        .iter()
        .find(|b| b.id == "involuntary_disconnects")
        .expect("bar");
    assert!(ends.passed, "{ends:?}");
    assert!(
        res.metrics.drops.is_empty(),
        "the clean network drops nothing: {:?}",
        res.metrics.drops
    );
    assert_eq!(
        res.metrics.commands_acked, res.metrics.commands_sent,
        "{:?}",
        res.metrics
    );
}

/// How far from `outage start + timeout_ms` a removal may be recorded, milliseconds: a peer's
/// silence starts at its last datagram before the outage (the server sends every 33 ms tick, the
/// link adds up to 8 ms), and each end looks at its clock once per frame or tick.
const REMOVAL_SLACK_MS: u64 = 100;

/// Client 3's long `flappy` outage in a run of `s`: `(start, end)`, milliseconds.
fn long_flappy_outage(s: &Scenario) -> (u64, u64) {
    let link = s.profile.client_link(3, s.duration_ms);
    *link
        .outages
        .iter()
        .find(|(a, b)| b - a == flap_long_outage_ms())
        .unwrap_or_else(|| panic!("client 3 has the long outage: {:?}", link.outages))
}

fn bar<'a>(res: &'a report::ScenarioResult, id: &str) -> &'a report::Bar {
    res.bars
        .iter()
        .find(|b| b.id == id)
        .unwrap_or_else(|| panic!("no bar {id}"))
}

/// The long `flappy` outage runs past the engine's removal timeout (it is derived from
/// `regia_net::TimeoutPolicy::default()`, ADR-7950: 20 s, so 22 s), so it ends the connection, at
/// the timeout and by silence; the end is excused, and the client is back within the grace after
/// the link returns. The 3 s outages end nobody.
#[test]
fn flappy_long_outage_is_excused_and_rejoins() {
    let policy = report::timeout_policy();
    // Client 3 gets the long outage at half the run.
    let s = short("flappy", 4, 70, 3);
    let (start, end) = long_flappy_outage(&s);
    assert!(
        end - start > policy.timeout_ms,
        "the outage ({} ms) must outlast the removal timeout ({} ms)",
        end - start,
        policy.timeout_ms
    );
    assert!(
        end + report::RECONNECT_GRACE_MS < s.duration_ms,
        "the rejoin fits the run"
    );
    let r = workload::run(&s).expect("run");
    let res = report::evaluate(&r, 0);
    // One lost connection, client 3's, removed by silence at the policy's timeout on both ends.
    assert!(
        r.ends.iter().all(|e| e.client == 3 && !e.voluntary),
        "{:?}",
        r.ends
    );
    for side in ["client", "server"] {
        let ends: Vec<_> = r.ends.iter().filter(|e| e.side == side).collect();
        assert_eq!(ends.len(), 1, "{side}: {:?}", r.ends);
        assert_eq!(ends[0].cause, "timeout", "{:?}", ends[0]);
        assert!(
            ends[0].at_ms.abs_diff(start + policy.timeout_ms) <= REMOVAL_SLACK_MS,
            "{side}: removed at {} ms, the policy says {} ms",
            ends[0].at_ms,
            start + policy.timeout_ms
        );
    }
    // Excused, and nothing else was.
    assert_eq!(res.metrics.ends_excused, 1, "{:?}", res.metrics);
    let ends = bar(&res, "involuntary_disconnects");
    assert!(ends.passed, "{}", ends.detail);
    assert_eq!(
        ends.detail,
        "0 involuntary ends (1 excused by outages beyond the timeout)"
    );
    // Back within the grace of the link's return.
    assert_eq!(r.reconnects.len(), 1, "{:?}", r.reconnects);
    let rc = &r.reconnects[0];
    assert_eq!(
        (rc.client, rc.kind.as_str(), rc.link_up_ms),
        (3, "involuntary", end),
        "{rc:?}"
    );
    assert!(
        rc.connected_ms
            .is_some_and(|t| t >= end && t <= end + report::RECONNECT_GRACE_MS),
        "{rc:?}"
    );
    assert!(bar(&res, "reconnect_within_grace").passed);
}

/// The same run on deliberately broken timeout policies, judged against the engine's own.
/// Removal far too early (2 s): every outage ends its connection, the long one included, none of
/// those ends is the policy's own removal, so none is excused and `involuntary_disconnects`
/// fails. No removal at all (10 minutes): the long outage ends nothing, so nothing is excused and
/// nobody rejoins: `flappy_long_outage_is_excused_and_rejoins` cannot pass on it.
#[test]
fn flappy_outages_fail_on_a_broken_timeout_policy() {
    let policy = TimeoutPolicy::default();
    let s = short("flappy", 4, 70, 3);
    let (start, end) = long_flappy_outage(&s);
    let early = Knobs {
        timeouts: TimeoutPolicy {
            timeout_ms: 2_000,
            interrupt_ms: 1_000,
            ..policy
        },
        ..Knobs::default()
    };
    let r = workload::run_with(&s, &early).expect("run");
    let res = report::evaluate(&r, 0);
    let ends = bar(&res, "involuntary_disconnects");
    assert!(!ends.passed && ends.gating, "{}", ends.detail);
    assert!(
        ends.detail.starts_with("client 0 ended on the client side at ")
            && ends.detail.contains("ms: timeout"),
        "{}",
        ends.detail
    );
    // Every client lost its connection in every outage it was connected for: three clients in
    // three 3 s outages each, client 3 in one and in its long one, eighteen seconds before the
    // policy's time. Nothing is excused.
    let unexcused: u64 = res.metrics.ends_by_cause.values().sum();
    assert_eq!(
        res.metrics.ends_by_cause.get("client:timeout"),
        Some(&unexcused),
        "{:?}",
        res.metrics
    );
    assert_eq!(unexcused, 11, "{:?}", res.metrics.ends_by_cause);
    assert_eq!(res.metrics.ends_excused, 0, "{:?}", res.metrics);
    assert!(
        r.ends
            .iter()
            .any(|e| e.client == 3 && e.side == "client" && e.at_ms > start && e.at_ms < end),
        "client 3 was removed inside its long outage: {:?}",
        r.ends
    );
    let first = r
        .ends
        .iter()
        .find(|e| e.client == 0 && e.side == "client")
        .expect("client 0's first end");
    let flap = s.profile.client_link(0, s.duration_ms).outages[0];
    assert!(
        first.at_ms.abs_diff(flap.0 + 2_000) <= REMOVAL_SLACK_MS && first.at_ms < flap.1,
        "{first:?} in {flap:?}"
    );

    let never = Knobs {
        timeouts: TimeoutPolicy {
            timeout_ms: 600_000,
            ..policy
        },
        ..Knobs::default()
    };
    let r = workload::run_with(&s, &never).expect("run");
    let res = report::evaluate(&r, 0);
    assert!(end - start > policy.timeout_ms);
    assert!(r.ends.is_empty(), "{:?}", r.ends);
    assert_eq!(res.metrics.ends_excused, 0);
    assert!(r.reconnects.is_empty(), "{:?}", r.reconnects);
    // The engine's policy would have removed client 3; this one left it connected through 22 s
    // of silence, interrupted once and restored when the link came back.
    let connected = &r.clients[3].connected;
    assert_eq!(connected.len(), 1, "{connected:?}");
    let kinds: Vec<&str> = r.clients[3]
        .interruptions
        .iter()
        .filter(|(at, _)| *at >= start)
        .map(|(_, kind)| kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        ["interrupted", "restored"],
        "{:?}",
        r.clients[3].interruptions
    );
}

/// A fixed input lead past the server's input horizon: three seconds at 30 Hz. The horizon
/// (`ServerConfig::default().input_buffer_ticks`, 64 ticks) reaches a client only in the server's
/// input feedback; the engine's steered lead stays inside it (`NetClient::input_lead_ticks`), a
/// lead picked without the feedback (here one sized for a worst-case satellite link) does not.
const LEAD_PAST_HORIZON: u64 = 90;

/// `no_wedged_client` with the input feedback on and off (ADR-8424).
///
/// With the steered lead neither congested nor rtt-step has a wedge. At v0.5.0 a fixed lead of
/// two ticks (the kit's own until it took the engine's loop, ADR-8077) left the server seconds
/// without a client's input on both, thirteen seeds of thirteen. That no longer happens: the
/// server carries a late input into its next tick (the late-input merge, ADR-8217, spec 120
/// § 6.3, up to `LATE_MERGE_TICKS` = 256 ticks late), so a client whose inputs arrive late is
/// still heard.
/// Measured with the fixed two-tick lead at this seed: no wedge on either, 1.71 % of 14,025 ticks
/// missed on congested (0.14 % steered) and 3.26 % of 42,817 on rtt-step (0.03 % steered).
/// With the rate controller (ADR-8301) congested's queue stays short, so the two-tick lead's slack
/// covers most of it: 57-67 ticks missed against the steered 17-21 over four seeds (58 against 20
/// at this seed), under the 5x this pin asks. The late case is now the same policy without its
/// slack, a fixed lead of zero (inputs stamped for the tick estimate itself): no wedge, 1,916-1,968
/// ticks missed against 17-21 on congested and 1,824-1,897 against 9-11 on rtt-step (ADR-8534;
/// the two-tick lead still misses 1,363-1,408 on rtt-step).
///
/// The failing case is now a client whose fixed lead is past the server's input horizon
/// ([`LEAD_PAST_HORIZON`]): the server refuses every input as too far ahead, and goes the whole
/// connection without one while the link is up (before ADR-8424 the harness did not see a peer
/// none of whose inputs ever arrived: it reported "no wedge" here with 100 % of ticks missed).
/// Measured: every client wedged from the input grace after its join to the end of the run.
#[test]
fn no_wedged_client_fails_without_input_feedback_and_passes_with_it() {
    let horizon = regia_net::ServerConfig::default().input_buffer_ticks as u64;
    assert!(
        LEAD_PAST_HORIZON > horizon + 2,
        "{LEAD_PAST_HORIZON} against {horizon}"
    );
    let fixed = |lead| Knobs {
        input_lead: InputLead::Fixed(lead),
        ..Knobs::default()
    };
    for (profile, seconds) in [("congested", 60), ("rtt-step", 180)] {
        let s = short(profile, 8, seconds, DEFAULT_SEED);
        let on = report::evaluate(&workload::run(&s).expect("run"), 0);
        let wedged = bar(&on, "no_wedged_client");
        assert!(wedged.passed && wedged.gating, "{profile}: {}", wedged.detail);
        assert!(on.wedges.is_empty(), "{profile}: {:?}", on.wedges);
        assert_eq!(on.server_totals.inputs_too_far_ahead, 0, "{profile}");

        // The v0.5.0 broken case without its two ticks of slack: late inputs, merged, no wedge;
        // the feedback still saves ticks (ADR-8534).
        let late = report::evaluate(&workload::run_with(&s, &fixed(0)).expect("run"), 0);
        assert!(late.wedges.is_empty(), "{profile}: {:?}", late.wedges);
        let missed = |r: &report::ScenarioResult| r.measures.as_ref().map_or(0, |m| m.input_ticks_missed);
        assert!(
            missed(&late) > 5 * missed(&on),
            "{profile}: {} against {}",
            missed(&late),
            missed(&on)
        );

        // A lead past the horizon: every client is wedged, and the bar fails.
        let rec = workload::run_with(&s, &fixed(LEAD_PAST_HORIZON)).expect("run");
        let off = report::evaluate(&rec, 0);
        let wedged = bar(&off, "no_wedged_client");
        assert!(!wedged.passed && wedged.gating, "{profile}: {}", wedged.detail);
        // Every wedge is the server waiting for an input, on a link that is up and sending: each
        // client in each of its connections, from the input grace after the join to the end of
        // the connection (congested's scripted leaves rejoin some clients mid-run; before
        // ADR-8887 the connection before such a leave was not judged, and this pin asked for one
        // wedge per client).
        let mut wedged_clients: Vec<usize> = off.wedges.iter().map(|w| w.client).collect();
        wedged_clients.dedup();
        assert_eq!(
            wedged_clients,
            (0..8).collect::<Vec<_>>(),
            "{profile}: {:?}",
            off.wedges
        );
        let connections: Vec<(usize, u64, u64)> = rec
            .clients
            .iter()
            .flat_map(|c| c.connected.iter().map(|&(_, a, b)| (c.client, a, b)))
            .collect();
        assert!(
            connections.len() > 8,
            "{profile}: a scripted leave rejoins: {connections:?}"
        );
        assert_eq!(off.wedges.len(), connections.len(), "{profile}: {:?}", off.wedges);
        for (w, &(client, joined, left)) in off.wedges.iter().zip(&connections) {
            assert_eq!(w.kind, "server_no_input", "{profile}: {w:?}");
            assert_eq!(w.client, client, "{profile}: {w:?}");
            assert_eq!(
                w.start_ms + w.length_ms,
                left.min(s.duration_ms),
                "{profile}: {w:?}"
            );
            assert!(
                w.start_ms >= joined && w.start_ms <= joined + workload::INPUT_GRACE_MS,
                "{profile}: {w:?}, connected at {joined} ms"
            );
        }
        assert!(off.metrics.input_send_errors.is_empty(), "{:?}", off.metrics);
        // The inputs did reach the server: it refused them as too far ahead.
        assert!(
            off.server_totals.inputs_too_far_ahead > 1_000,
            "{profile}: {:?}",
            off.server_totals
        );
    }
}

/// The DO27 input-size blocker (branch `v051-net-input-mtu`): clients whose every input carries a
/// legal 0.5-1 KiB command (the heavy cohort, client 2) must keep reaching the server. At v0.5.0 the
/// whole redundancy-15 history goes into one datagram, every send fails `too_large`, and the client
/// is wedged while still connected.
#[test]
#[ignore = "fails at v0.5.0 until branch v051-net-input-mtu lands (ADR-7899); run with --ignored"]
fn large_inputs_never_wedge_a_client() {
    let s = short("clean", 4, 40, 11);
    let res = report::evaluate(&workload::run(&s).expect("run"), 0);
    let bar = res.bars.iter().find(|b| b.id == "no_wedged_client").expect("bar");
    assert!(bar.passed, "{}", bar.detail);
    // On a clean link every legal input reaches the server, so the server never disagrees with a
    // prediction. At v0.5.0 the random 0.5-1 KiB samples alone make 15 % of sends fail `too_large`.
    assert!(res.metrics.input_send_errors.is_empty(), "{:?}", res.metrics);
    assert_eq!(res.metrics.corrections, 0, "{:?}", res.metrics);
}

/// The DO27 hostile-input blocker (branch `v051-net-input-rejection`): a peer whose inputs carry an
/// over-cap command tail inside otherwise valid records is dropped within the bar, and nobody else
/// is harmed. At v0.5.0 each such record earns back the malformed point it costs, so the peer is
/// never dropped.
#[test]
#[ignore = "fails at v0.5.0 until branch v051-net-input-rejection lands (ADR-7899); run with --ignored"]
fn over_cap_input_drops_only_its_sender() {
    let s = short("hostile", 4, 22, 13);
    let r = workload::run(&s).expect("run");
    let hostile = r.hostile.as_ref().expect("hostile record");
    let session = hostile
        .sessions
        .iter()
        .find(|x| x.kind == "over_cap_input")
        .expect("over-cap session");
    let first = session.first_act_ms.expect("the hostile peer acted");
    let dropped = session.dropped_ms.expect("the server dropped the hostile peer");
    assert!(dropped - first <= report::HOSTILE_DROP_MS, "{session:?}");
    let res = report::evaluate(&r, 0);
    let ends = res
        .bars
        .iter()
        .find(|b| b.id == "involuntary_disconnects")
        .expect("bar");
    assert!(ends.passed, "{}", ends.detail);
}

/// `cargo xtask net-soak --light`: the same plan runner as `regia_test_utils`'s `net_soak_matrix`,
/// without the counting allocator (peak heap reads 0), so it builds without the renderer. A no-op
/// unless `REGIA_NET_SOAK_OUT` is set.
#[test]
fn net_soak_matrix_light() {
    if let Some(result) = run_from_env(no_heap_probe) {
        let report = result.unwrap_or_else(|e| panic!("net-soak harness error: {e}"));
        eprintln!("{}", report.summary_md());
    }
}

/// Path MTU discovery under the real server and clients (spec 120 § 4.3, ADR-8300). Both MTU
/// profiles turn probing on. mtu-1280: every bar passes and the only datagrams the MTU drops are
/// the probes (three lost 1,400-byte probes per sending connection, server and clients).
/// mtu-blackhole (1,200 from 30 s): every bar passes, so the fall back loses nobody.
#[test]
fn mtu_profiles_pass_their_bars_with_probing_on() {
    for (name, clients, seconds) in [("mtu-1280", 4usize, 20u64), ("mtu-blackhole", 4, 40)] {
        let s = short(name, clients, seconds, 8300);
        assert!(s.profile.mtu_probing, "{name} probes");
        let r = workload::run(&s).expect("run");
        let res = report::evaluate(&r, 0);
        let failed: Vec<_> = res.bars.iter().filter(|b| !b.passed).collect();
        let mtu = res.metrics.drops.get("mtu").copied().unwrap_or(0);
        println!(
            "{name}: joined {}, mtu drops {mtu}, drops {:?}, failed bars {failed:?}",
            res.metrics.joined, res.metrics.drops
        );
        assert_eq!(res.metrics.joined, clients, "{name}");
        assert!(failed.is_empty(), "{name}: {failed:?}");
        if name == "mtu-1280" {
            let probe_losses = 3 * 2 * clients as u64;
            assert!(
                mtu > 0 && mtu <= probe_losses,
                "mtu-1280 drops only the 1,400-byte probes: {mtu} (at most {probe_losses})"
            );
        }
    }
}
