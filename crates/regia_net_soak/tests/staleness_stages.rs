// SPDX-License-Identifier: MPL-2.0
//! ADR-10145: where snapshot staleness goes. The soak splits the age of the newest snapshot a
//! client holds into stages (the sender's hold, the bottleneck queues, serialization, the path,
//! the receiver's hold, the wait for the next snapshot, and the client's tick estimate) and
//! reports the stages of the judged tail in `result.json` (`measures.staleness_stages`) and
//! `summary.md`. Seeded virtual-time runs.

use regia_net_soak::profiles::Profile;
use regia_net_soak::report::{self, ScenarioResult, StalenessStageGroup};
use regia_net_soak::workload::{
    self, Knobs, RunRecord, STALENESS_STAGES, Scenario, ServerMode, StalenessStages, TICK_HZ,
};

fn record(profile: &str, clients: usize, seconds: u64, seed: u64) -> RunRecord {
    record_with(profile, clients, seconds, seed, &Knobs::default())
}

/// The cautious start of ADR-10145 alone: the rate controller starts at 32 KiB/s (the first
/// burst follows: eight packets) instead of 256 KiB/s, with the first flight named off (option
/// (a) of its ruling R1).
fn cautious() -> Knobs {
    Knobs {
        start_bytes_per_second: Some(32 * 1024),
        first_flight: Some(false),
        ..Knobs::default()
    }
}

/// The first flight of ADR-10156 on every endpoint, at the engine's default start rate (opt-in:
/// the engine's default is off in v0.7, ADR-10231).
fn first_flight() -> Knobs {
    Knobs {
        first_flight: Some(true),
        ..Knobs::default()
    }
}

/// The engine's default start, named (ADR-10231): no first flight, the 64-packet initial window
/// at 256 KiB/s. The comparisons below say what they compare whatever the default is.
fn blind_start() -> Knobs {
    Knobs {
        first_flight: Some(false),
        ..Knobs::default()
    }
}

/// The joins that got their world blob, and the median and the largest time from a connect to
/// it, milliseconds.
fn world_ms(r: &RunRecord) -> (usize, u64, u64) {
    let mut v: Vec<u64> = r
        .clients
        .iter()
        .flat_map(|c| c.world_after_connect_ms.iter().copied())
        .collect();
    v.sort_unstable();
    (
        v.len(),
        v.get(v.len() / 2).copied().unwrap_or(0),
        v.last().copied().unwrap_or(0),
    )
}

fn record_with(profile: &str, clients: usize, seconds: u64, seed: u64, knobs: &Knobs) -> RunRecord {
    let mut p = Profile::named(profile).expect("known profile");
    p.fit_to(seconds * 1000);
    let s = Scenario {
        profile: p,
        clients,
        mode: ServerMode::Dedicated,
        seed,
        duration_ms: seconds * 1000,
        wire: Default::default(),
    };
    workload::run_with(&s, knobs).expect("run")
}

fn group(r: &ScenarioResult, limit: u64) -> &StalenessStageGroup {
    r.measures
        .as_ref()
        .expect("measures")
        .staleness_stages
        .iter()
        .find(|g| g.limit_ticks == limit)
        .unwrap_or_else(|| panic!("{}: no staleness group with limit {limit}", r.label))
}

fn line(label: &str, g: &StalenessStageGroup) -> String {
    let stages = |t: &report::StageTicks| {
        t.named()
            .iter()
            .map(|(name, v)| format!("{name} {v:+.2}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "{label}: limit {} p99 {} settled p99 {} over-limit {} of {} traced ({} untraced)\n  mean [{}]\n  tail of {} [{}] most in {}",
        g.limit_ticks,
        g.p99_ticks,
        g.settled_p99_ticks,
        g.over_limit_samples,
        g.samples,
        g.untraced,
        stages(&g.mean),
        g.tail_samples,
        stages(&g.tail),
        g.dominant
    )
}

/// Each client's largest staleness in every second of the run, for the test output.
fn per_second(r: &RunRecord) -> String {
    r.clients
        .iter()
        .map(|c| {
            format!(
                "  client {} joins {:?}: {}",
                c.client,
                c.connected.iter().map(|i| i.1).collect::<Vec<_>>(),
                c.staleness_max_per_s
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The seven stages of every sample add up to its staleness, to the microsecond: the breakdown
/// loses and invents nothing. Every link-up sample is traced on the virtual wire, and no stage
/// of a snapshot's way runs backwards.
#[test]
fn the_stages_of_every_sample_add_up_to_its_staleness() {
    for profile in ["bad-wifi", "congested", "asym-bufferbloat"] {
        let r = record(profile, 4, 20, 7);
        for c in &r.clients {
            let s: &StalenessStages = &c.staleness_stages;
            let traced: u64 = s.samples.iter().sum();
            println!(
                "{profile} client {}: {} link-up samples, {traced} traced, {} untraced",
                c.client,
                c.staleness_ticks_link_up.count(),
                s.untraced
            );
            assert!(
                traced > 500,
                "{profile} client {}: {traced} traced samples",
                c.client
            );
            assert_eq!(
                traced + s.untraced,
                c.staleness_ticks_link_up.count(),
                "{profile} client {}: every link-up sample is counted once",
                c.client
            );
            assert_eq!(s.untraced, 0, "{profile} client {}: untraced samples", c.client);
            for (ticks, (&n, stage_us)) in s.samples.iter().zip(&s.stage_us).enumerate() {
                // The last bucket holds every larger staleness: its sum has no single value.
                if ticks + 1 < StalenessStages::BUCKETS {
                    assert_eq!(
                        stage_us.iter().sum::<i64>(),
                        (n * (ticks as u64 * 1_000_000 / TICK_HZ)) as i64,
                        "{profile} client {}: the stages of the {n} samples at {ticks} ticks",
                        c.client
                    );
                }
                for (name, us) in STALENESS_STAGES.iter().zip(stage_us).take(6) {
                    assert!(
                        *us >= 0,
                        "{profile} client {}: stage {name} is {us} us at {ticks} ticks",
                        c.client
                    );
                }
            }
        }
    }
}

/// A clean link has no queue, no serialization delay and no sender hold: its staleness is the
/// snapshot interval, the client's frame and the tick label.
#[test]
fn a_clean_links_staleness_holds_no_queue() {
    let r = report::evaluate(&record("clean", 4, 20, 7), 0);
    let g = group(&r, 4);
    println!("{}", line("clean/4", g));
    assert!(g.samples > 4_000, "{} samples", g.samples);
    assert_eq!(g.untraced, 0);
    assert_eq!(g.mean.queue, 0.0, "no bottleneck on a clean link");
    assert_eq!(g.mean.serialize, 0.0);
    assert!(
        g.mean.sender_hold < 0.05,
        "a snapshot leaves in the iteration that made it: {} ticks",
        g.mean.sender_hold
    );
    assert!(g.p99_ticks <= 4, "p99 {}", g.p99_ticks);
    assert_eq!(g.over_limit_samples, 0);
}

/// `asym-bufferbloat` is bounded by a queue that is not the game's: cross traffic offered at
/// 4.4 Mbit/s keeps the 1 s buffer of the 4 Mbit/s downlink full, so every snapshot that gets in
/// waits about a second (30 ticks at 30 Hz) whatever the sender does. The bar counts fewer ticks
/// than that because the client's tick estimate halves the round trip and the delay is all on the
/// way down: the estimate runs about half a second behind the server (ADR-10145's physics).
#[test]
fn asym_bufferbloat_staleness_is_the_cross_traffics_queue() {
    let rec = record("asym-bufferbloat", 4, 30, 7);
    let r = report::evaluate(&rec, 0);
    let g = group(&r, 4);
    println!("{}\n{}", line("asym-bufferbloat/4", g), per_second(&rec));
    assert_eq!(g.dominant, "queue");
    for (what, t) in [("mean", &g.mean), ("tail", &g.tail)] {
        assert!(
            t.queue >= 25.0 && t.queue <= 31.0,
            "{what}: the full 1 s queue holds every snapshot about 30 ticks: {}",
            t.queue
        );
        assert!(
            t.sender_hold < 0.5,
            "{what}: the sender holds nothing back: {}",
            t.sender_hold
        );
        assert!(
            t.estimate < -8.0,
            "{what}: the client's estimate runs about half the one-sided delay behind: {}",
            t.estimate
        );
    }
    // The sender's own queue is not what stands there: the cross traffic fills it.
    let cross: u64 = rec.client_net.iter().map(|n| n.cross_admitted).sum();
    assert!(cross > 10_000, "cross traffic admitted: {cross}");
}

/// The congested row's stages, printed for the report (`--nocapture`): the group's table row and
/// every client's largest staleness per second.
#[test]
fn congested_stages_are_reported() {
    let rec = record("congested", 8, 60, regia_net_soak::DEFAULT_SEED);
    let r = report::evaluate(&rec, 0);
    let g = group(&r, 8);
    println!("{}\n{}", line("congested/8", g), per_second(&rec));
    let drops: Vec<[u64; 7]> = rec.client_net.iter().map(|n| n.drops).collect();
    println!("  drops by client {drops:?}");
    assert_eq!(g.untraced, 0);
    assert!(g.samples > 25_000, "{} samples", g.samples);
    let total = g.tail.total();
    assert!(
        total + 0.01 >= g.p99_ticks as f64,
        "the tail's stages add up to at least the p99: {total} against {}",
        g.p99_ticks
    );
}

/// ADR-10145, the fix: a join's queue is drained. Every join on `congested` sends a 16 KiB world
/// blob at the start rate into a 256 kbit/s downlink with a 16 KiB buffer: the buffer is full
/// before the first acknowledgment returns. Before, the rate controller's cut left it standing
/// and it emptied at 15 % of the link's rate, about 2.4 s over the limit per join (1,371 samples
/// of 29,205 on the gate's row), with the judged tail a full buffer deep. Now the cut drains it:
/// at most a second over the limit per join on every seed, and the p99 is under the depth of the
/// full buffer (15.4 ticks). What is left is the buffer itself (ADR-10145's physics): the world
/// blob alone is 0.51 s of this link. The drain is measured on the blind start (no first flight,
/// the default), which is what fills the buffer.
#[test]
fn congested_joins_drain_their_queue() {
    for seed in [regia_net_soak::DEFAULT_SEED, 1, 2, 3] {
        let rec = record_with("congested", 8, 60, seed, &blind_start());
        let joins: usize = rec.clients.iter().map(|c| c.connected.len()).sum();
        let r = report::evaluate(&rec, 0);
        let g = group(&r, 8);
        println!("{}", line(&format!("congested/8 seed {seed} ({joins} joins)"), g));
        assert_eq!(joins, 9, "seed {seed}: eight joins and one scripted rejoin");
        // A client frame every 16 ms: 62.5 samples a second.
        let second = 1_000 / workload::FRAME_MS as usize;
        assert!(
            g.over_limit_samples as usize <= joins * second,
            "seed {seed}: {} samples over the limit, more than a second per join ({joins} joins)",
            g.over_limit_samples
        );
        assert!(
            g.p99_ticks <= 15,
            "seed {seed}: staleness p99 {} ticks, a full 16 KiB buffer (15.4 ticks) or more",
            g.p99_ticks
        );
        assert!(
            g.settled_p99_ticks <= 4,
            "seed {seed}: settled p99 {} ticks",
            g.settled_p99_ticks
        );
    }
}

/// ADR-10145, the start: with the start rate at 32 KiB/s the join's world blob no longer fills
/// the 16 KiB buffer of `congested` in its first burst, and the row's `staleness_p99` passes its
/// limit of 8 ticks on every seed (the engine's default start, 256 KiB/s, fills the buffer:
/// `congested_joins_drain_their_queue`). The price is the join: the world blob arrives later on
/// every link, printed here for the links of the gate plan.
#[test]
fn a_cautious_start_keeps_congested_under_its_staleness_limit() {
    for seed in [regia_net_soak::DEFAULT_SEED, 1, 2, 3] {
        let rec = record_with("congested", 8, 60, seed, &cautious());
        let r = report::evaluate(&rec, 0);
        let g = group(&r, 8);
        println!("{}", line(&format!("congested/8 seed {seed}, start 32 KiB/s"), g));
        let bar = r.bars.iter().find(|b| b.id == "staleness_p99").expect("bar");
        assert!(bar.passed, "seed {seed}: {}", bar.detail);
        assert!(g.p99_ticks <= 8, "seed {seed}: p99 {}", g.p99_ticks);
        let ends: u64 = r.metrics.ends_by_cause.values().sum();
        assert_eq!(ends, 0, "seed {seed}: {:?}", r.metrics.ends_by_cause);
    }
    for profile in ["clean", "good-wifi", "transatlantic", "high-rtt", "congested"] {
        let fast = world_ms(&record_with(profile, 8, 30, 7, &blind_start()));
        let slow = world_ms(&record_with(profile, 8, 30, 7, &cautious()));
        println!(
            "{profile}/8: world blob after the connect, median/largest ms: {}/{} at 256 KiB/s, {}/{} at 32 KiB/s",
            fast.1, fast.2, slow.1, slow.2
        );
        assert!(
            fast.0 >= 8 && slow.0 >= 8,
            "{profile}: every join got its world blob ({} and {} of 8)",
            fast.0,
            slow.0
        );
    }
}

/// ADR-10156, the first flight: with `rate.first_flight` on and the start rate at the engine's
/// default 256 KiB/s, a join no longer fills the 16 KiB buffer of `congested` before the path has
/// answered once, and the row's `staleness_p99` passes its limit of 8 ticks on every seed (off,
/// the same runs read 13 to 14 ticks: `congested_joins_drain_their_queue`). The price is the
/// join on a good link: the world blob's tail waits for the first round, printed here beside
/// the default and beside ADR-10145's 32 KiB/s start for the links of the gate plan.
#[test]
fn a_first_flight_keeps_congested_under_its_staleness_limit_at_the_default_start_rate() {
    for seed in [regia_net_soak::DEFAULT_SEED, 1, 2, 3] {
        let rec = record_with("congested", 8, 60, seed, &first_flight());
        let r = report::evaluate(&rec, 0);
        let g = group(&r, 8);
        println!("{}", line(&format!("congested/8 seed {seed}, first flight"), g));
        let bar = r.bars.iter().find(|b| b.id == "staleness_p99").expect("bar");
        assert!(bar.passed, "seed {seed}: {}", bar.detail);
        assert!(g.p99_ticks <= 8, "seed {seed}: p99 {}", g.p99_ticks);
        let ends: u64 = r.metrics.ends_by_cause.values().sum();
        assert_eq!(ends, 0, "seed {seed}: {:?}", r.metrics.ends_by_cause);
        for id in ["rate_vs_bottleneck", "no_wedged_client"] {
            let bar = r.bars.iter().find(|b| b.id == id).expect("bar");
            println!("  {id}: {} ({})", bar.passed, bar.detail);
        }
    }
    for profile in ["clean", "good-wifi", "transatlantic", "high-rtt", "congested"] {
        let default = world_ms(&record_with(profile, 8, 30, 7, &blind_start()));
        let flight = world_ms(&record_with(profile, 8, 30, 7, &first_flight()));
        let slow = world_ms(&record_with(profile, 8, 30, 7, &cautious()));
        println!(
            "{profile}/8: world blob after the connect, median/largest ms: {}/{} blind, {}/{} with a first flight, {}/{} at 32 KiB/s",
            default.1, default.2, flight.1, flight.2, slow.1, slow.2
        );
        assert!(
            default.0 >= 8 && flight.0 >= 8,
            "{profile}: every join got its world blob ({} and {} of 8)",
            default.0,
            flight.0
        );
        // A first flight never costs a join more than the 32 KiB/s start does.
        assert!(
            flight.1 <= slow.1 && flight.2 <= slow.2,
            "{profile}: a first flight's join ({}/{} ms) is slower than the 32 KiB/s start's ({}/{} ms)",
            flight.1,
            flight.2,
            slow.1,
            slow.2
        );
    }
}

/// ADR-10156, ADR-10200, ADR-10231: what a first flight (opt-in) costs `random_loss_kept`, and
/// what it does not. On `lossy-fat-pipe` (100 Mbit/s, 5 % random loss) the profile's scripted
/// rejoin flies its first flight after 5 s: for that round the server's allowance toward it is
/// the floor's rate, 80 of the 140 entities, under the bar's 126. The soak keeps that minimum
/// apart (`ServerRecord::min_flight_entity_budget`; the bar reads the budget outside a first
/// flight, ADR-10200 (2)), and the flight's own dip goes no further than the floor: loss does not
/// cut it (a loss-driven cut reads 8 entities, as on bad-wifi). Off, the default, the budget
/// stays at 136 or more. What this test does not claim: with the first flight on, the budget
/// outside the flight also dips on this profile (123 of 140 at this seed, 63 to 123 on 6 of 10
/// seeds: ADR-10200 (6), open for the release that turns the first flight on, ADR-10231).
#[test]
fn a_first_flight_dips_the_snapshot_budget_to_the_floor_and_no_further() {
    for clients in [8, 32] {
        let seed = regia_net_soak::DEFAULT_SEED;
        let off = record("lossy-fat-pipe", clients, 30, seed);
        let on = record_with("lossy-fat-pipe", clients, 30, seed, &first_flight());
        let world = on.server.world_entities;
        let floor = workload::entity_budget(Some(32 * 1024), world);
        let (off_min, on_min) = (off.server.min_entity_budget, on.server.min_entity_budget);
        let flight_min = on.server.min_flight_entity_budget;
        println!(
            "lossy-fat-pipe/{clients}: smallest snapshot budget after 5 s: {off_min:?} by default, {on_min:?} with a \
             first flight outside it, {flight_min:?} in the rejoin's first flight (the floor's rate carries {floor}, \
             the world is {world})"
        );
        assert!(
            off_min.is_some_and(|b| b * 10 >= world * 9),
            "by default the budget stays within the bar's 90 % of the world: {off_min:?} of {world}"
        );
        assert_eq!(
            off.server.min_flight_entity_budget, None,
            "by default no sender flies a first flight"
        );
        assert!(
            on.clients.iter().any(|c| c.connected.iter().any(|i| i.1 > 5_000)),
            "lossy-fat-pipe/{clients}: a client connects after 5 s (the scripted rejoin)"
        );
        let flight_min = flight_min.expect("the rejoin flew a first flight after 5 s");
        assert!(
            flight_min * 10 < world * 9,
            "the rejoin's first flight is held to the floor: {flight_min} of {world}"
        );
        assert!(
            flight_min + 2 >= floor,
            "lossy-fat-pipe/{clients}: the flight's budget fell to {flight_min}, under the first flight's own {floor}: loss cut it"
        );
        let ends: u64 = report::evaluate(&on, 0).metrics.ends_by_cause.values().sum();
        assert_eq!(
            ends, 0,
            "lossy-fat-pipe/{clients}: a first flight ended a session"
        );
    }
}
