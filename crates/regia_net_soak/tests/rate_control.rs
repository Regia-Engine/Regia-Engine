// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 4.2 (lane N202, ADR-8301): the rate controller, pacing and the shared uplink, judged
//! by the harness's own bars on seeded virtual-time runs. Each failed at the v0.6 base: congested
//! never held its offered rate under the bottleneck (E1), lossy Wi-Fi cut the snapshot budget to
//! 8 of 140 entities (F7) and a listen server collapsed past 32 clients (F5).

use regia_net_soak::profiles::Profile;
use regia_net_soak::report::{self, Bar, ScenarioResult};
use regia_net_soak::workload::{self, Scenario, ServerMode};

fn run(profile: &str, clients: usize, mode: ServerMode, seconds: u64, seed: u64) -> ScenarioResult {
    let mut p = Profile::named(profile).expect("known profile");
    p.fit_to(seconds * 1000);
    let s = Scenario {
        profile: p,
        clients,
        mode,
        seed,
        duration_ms: seconds * 1000,
        wire: Default::default(),
    };
    report::evaluate(&workload::run(&s).expect("run"), 0)
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

fn rates(r: &ScenarioResult) -> String {
    r.measures
        .as_ref()
        .map(|m| {
            m.rates
                .iter()
                .map(|c| {
                    format!(
                        "{}: {}/{} s over, worst {} bit/s of {}",
                        c.link, c.seconds_over, c.seconds_checked, c.worst_bps, c.cap_bps
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default()
}

/// E1 / bar `rate_vs_bottleneck` on congested: from 5 s after each join, no whole second offers
/// more than the 256 kbit/s downstream bottleneck (at the base: every second over it).
#[test]
fn congested_offers_no_more_than_the_bottleneck_from_five_seconds() {
    let r = run("congested", 4, ServerMode::Dedicated, 20, 5);
    let b = bar(&r, "rate_vs_bottleneck");
    println!("congested rate_vs_bottleneck: {} [{}]", b.detail, rates(&r));
    assert!(b.passed && b.skipped.is_none(), "{} [{}]", b.detail, rates(&r));
    assert_eq!(
        r.metrics.ends_by_cause.values().sum::<u64>(),
        0,
        "{:?}",
        r.metrics.ends_by_cause
    );
}

/// ADR-8490: headroom under the bottleneck. The same bar on congested over seeds 1-16, not only
/// the sentinel's seed 5: the controller grew to the full rate of the drop-tail buffer, so a whole
/// second crossed the cap in most seeds (base 59e5b6263: 11 of these 16 seeds, 142 of 200; the
/// queue-reading fix alone: 8 of 16, 125 of 200). Growth now stops at 97 % of the rate the path
/// delivered when a queue last stood at its bottleneck: none of these 16, 7 of 200 (a loss cut's
/// recovery to its pre-cut rate, spec 120 § 4.2 item 5, overshoots on those).
#[test]
fn congested_keeps_headroom_under_the_bottleneck_over_seeds() {
    let mut over = Vec::new();
    for seed in 1..=16 {
        let r = run("congested", 4, ServerMode::Dedicated, 20, seed);
        let b = bar(&r, "rate_vs_bottleneck");
        println!("seed {seed}: {} [{}]", b.detail, rates(&r));
        if !b.passed || b.skipped.is_some() {
            over.push((seed, b.detail.clone()));
        }
    }
    assert!(over.is_empty(), "seeds over the bottleneck: {over:?}");
}

/// ADR-8534: congested over seeds 17-64. On the integrated line (rate controller plus the
/// lockstep and acknowledgment work merged with it) seeds 24 and 51 put a second over the cap
/// (12 of seeds 1-200, worst 108.2 %): a congestion cut's recovery (spec 120 § 4.2 item 5) went
/// back to its pre-cut rate, past the bottleneck estimate. The recovery now stops at the headroom
/// under the rate the path delivered at the cut (the test Mac sweep: 0 of seeds 1-400, worst
/// 100.0 %).
#[test]
fn congested_recovery_keeps_headroom_over_more_seeds() {
    let mut over = Vec::new();
    for seed in 17..=64 {
        let r = run("congested", 4, ServerMode::Dedicated, 20, seed);
        let b = bar(&r, "rate_vs_bottleneck");
        if !b.passed || b.skipped.is_some() {
            over.push((seed, b.detail.clone()));
        }
    }
    assert!(over.is_empty(), "seeds over the bottleneck: {over:?}");
}

/// Bufferbloat (1 Mbit/s behind a 1 s queue) with a 256 KiB transfer to every client at half the run
/// (the harness's `+bulk` variant: the game alone offers about 450 kbit/s, under the cap): the
/// offered rate holds under the bottleneck and the game's datagrams wait at most 100 ms p95 in
/// that queue (bars `rate_vs_bottleneck`, `queue_delay_p95`).
#[test]
fn bufferbloat_keeps_the_queue_short_and_the_rate_under_the_bottleneck() {
    let r = run("bufferbloat+bulk", 4, ServerMode::Dedicated, 30, 11);
    let rate = bar(&r, "rate_vs_bottleneck");
    let q = bar(&r, "queue_delay_p95");
    println!(
        "bufferbloat rate_vs_bottleneck: {} [{}]; queue_delay_p95: {}",
        rate.detail,
        rates(&r),
        q.detail
    );
    assert!(
        rate.passed && rate.skipped.is_none(),
        "{} [{}]",
        rate.detail,
        rates(&r)
    );
    assert!(q.passed && q.skipped.is_none(), "{}", q.detail);
    let p95 = r.measures.as_ref().expect("measures").queue_delay_p95_ms;
    assert!(p95 <= 100, "queue delay p95 {p95} ms");
}

/// F7 / bar `random_loss_kept`: 5 % random loss on a 100 Mbit/s path with no standing queue is not
/// congestion, so the snapshot budget stays at least 90 % of a clean run's (at the base: 8 of 140
/// entities).
#[test]
fn lossy_fat_pipe_keeps_the_snapshot_budget() {
    let r = run("lossy-fat-pipe", 4, ServerMode::Dedicated, 20, 4);
    let b = bar(&r, "random_loss_kept");
    println!(
        "lossy-fat-pipe random_loss_kept: {} (min budget {:?} of {})",
        b.detail,
        r.metrics.min_entity_budget,
        r.measures.as_ref().map_or(0, |m| m.world_entities)
    );
    assert!(b.passed && b.skipped.is_none(), "{}", b.detail);
}

/// ADR-10157: the same bar on the gate's row (lossy-fat-pipe, 8 clients, 60 s) at the seeds where
/// it was red: the gate plan's own seed and seeds 4, 32, 36, 54 and 56 after ADR-10096 moved the
/// clients' timing (budgets 44, 114, 123, 14, 53 and 13 of 140), and seeds 146, 158, 248 and 307
/// before it (84, 58, 9 and 100). A feedback round that ended just after a server tick held one
/// tick of snapshots more than its length, the game at two thirds of its target read as a
/// saturating sender for that round, and a 10 % reading of the 5 % random loss cut the rate; from
/// there the game did fill its target and every further 10 % reading cut again. The budget now
/// stays at the full world at each of them.
#[test]
fn lossy_fat_pipe_keeps_the_snapshot_budget_at_the_seeds_that_lost_it() {
    let mut red = Vec::new();
    for seed in [1_592_590_337, 4, 32, 36, 54, 56, 146, 158, 248, 307] {
        let r = run("lossy-fat-pipe", 8, ServerMode::Dedicated, 60, seed);
        let b = bar(&r, "random_loss_kept");
        println!(
            "seed {seed}: {} (min budget {:?} of {})",
            b.detail,
            r.metrics.min_entity_budget,
            r.measures.as_ref().map_or(0, |m| m.world_entities)
        );
        if !b.passed || b.skipped.is_some() {
            red.push((seed, b.detail.clone()));
        }
    }
    assert!(red.is_empty(), "seeds that lost the snapshot budget: {red:?}");
}

/// F5: a listen server whose 20 Mbit/s uplink is shared by 64 clients drops nobody and keeps every
/// client's snapshot staleness p99 within 8 ticks (at the base: 739 timeouts at 128 clients and a
/// collapse past 32).
#[test]
fn listen_uplink_at_64_clients_drops_nobody_and_stays_fresh() {
    let r = run("listen-uplink", 64, ServerMode::Listen, 20, 3);
    let ends: u64 = r.metrics.ends_by_cause.values().sum();
    let worst = r.metrics.worst_client_staleness_p99;
    println!(
        "listen-uplink/64: joined {} ends {:?} staleness p99 {} worst client {} [{}]",
        r.metrics.joined,
        r.metrics.ends_by_cause,
        r.metrics.staleness_ticks_p99,
        worst,
        rates(&r)
    );
    assert_eq!(r.metrics.joined, 64, "{:?}", r.metrics.connect_refusals);
    assert_eq!(ends, 0, "timeouts or other ends: {:?}", r.metrics.ends_by_cause);
    assert!(
        r.metrics.connect_timeouts == 0,
        "connect timeouts {}",
        r.metrics.connect_timeouts
    );
    assert!(worst <= 8, "worst client staleness p99 {worst} ticks > 8");
    let rate = bar(&r, "rate_vs_bottleneck");
    assert!(rate.passed, "{} [{}]", rate.detail, rates(&r));
}

/// ADR-10081 (5), on the shared uplink alone: per-peer queue readings sit about one tick apart,
/// right at the 25 ms threshold, so the number of peers over it crosses the witness gate of
/// ADR-8733 (2) from round to round. A round the gate silences says nothing about the host's
/// queue: the next witnessed rounds must not read as a queue building. Before the fix each
/// silenced round zeroed the queue floor, the return to the unchanged 24 ms was a "rise", and the
/// aggregate was held at 97 % of what the peers delivered again, 3 % down every three rounds.
#[test]
fn a_flapping_witness_gate_does_not_ratchet_the_shared_uplink_down() {
    use regia_net::cc::{SharedUplink, UPLINK_SHARED_QUEUE_PEERS, UplinkPeer};
    const PEERS: usize = 16;
    const PEER_TARGET: f64 = 60_000.0;
    let mut uplink = SharedUplink::new(0);
    // Every peer delivers what the uplink allowed it in the round before.
    let mut delivered = vec![PEER_TARGET; PEERS];
    let mut first_hold = None;
    for round in 0..300u64 {
        // Two witnessed rounds, then one the gate silences; the median reading never moves.
        let over = if round % 3 == 2 {
            UPLINK_SHARED_QUEUE_PEERS - 1
        } else {
            UPLINK_SHARED_QUEUE_PEERS + 1
        };
        let peers: Vec<UplinkPeer> = (0..PEERS)
            .map(|i| UplinkPeer {
                target: PEER_TARGET,
                delivery: delivered[i],
                queue_ms: if i < over { 26.0 } else { 24.0 },
                weight: 1.0,
                loss: 0.0,
                starved: false,
            })
            .collect();
        delivered = uplink.update((round + 1) * 50, &peers);
        if first_hold.is_none() {
            first_hold = uplink.target_bytes_per_second();
        }
    }
    let first = first_hold.expect("the first standing queue holds the aggregate");
    let last = uplink.target_bytes_per_second().expect("a target");
    assert!(
        f64::from(last) >= 0.9 * f64::from(first),
        "the aggregate fell from {first} to {last} B/s in 15 s with a median queue that never moved \
         ({} cuts): a silenced round must not make the next one read as a queue building",
        uplink.cuts()
    );
}

/// KI-NT-031 (ADR-10097): the start of a full listen server. On these seeds the host's queue
/// filled while the 64 clients joined; the peers the server flushed last lost every packet for
/// seconds (the tail of each tick's burst into a full drop-tail queue) while the median peer read
/// neither loss nor delay, so the shared uplink took no target. At the base each seed had a second
/// at 106.5 to 107.5 % of the uplink and failed `no_wedged_client`. The starved peers now give the
/// aggregate its first target: nobody is wedged, nobody is dropped, and every whole second from
/// 5 s on stays under the uplink's rate.
#[test]
fn listen_uplink_start_starves_nobody_and_keeps_the_uplink_over_seeds() {
    for seed in [36u64, 90, 177] {
        let r = run("listen-uplink", 64, ServerMode::Listen, 20, seed);
        let ends: u64 = r.metrics.ends_by_cause.values().sum();
        let rate = bar(&r, "rate_vs_bottleneck");
        let wedged = bar(&r, "no_wedged_client");
        println!(
            "listen-uplink/64 seed {seed}: ends {ends}, no_wedged_client {} ({}), rate_vs_bottleneck {} [{}]",
            wedged.passed,
            wedged.detail,
            rate.passed,
            rates(&r)
        );
        assert_eq!(ends, 0, "seed {seed}: {:?}", r.metrics.ends_by_cause);
        assert!(wedged.passed, "seed {seed}: {}", wedged.detail);
        assert!(rate.passed, "seed {seed}: {} [{}]", rate.detail, rates(&r));
    }
}
