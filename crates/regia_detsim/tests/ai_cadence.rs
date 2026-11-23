// SPDX-License-Identifier: MPL-2.0
//! Think cadence channels, importance tiers and per-tick caps (audit gap AI-3): named channels with their own
//! period and phase rule, tier multipliers that keep a unit's base phase, and caps that carry the overflow to
//! later ticks oldest first with a proven wait bound. Original code and tests.

use regia_detsim::ai::{
    AiAgent, AiCadence, AiCore, AiRng, ArgMax, CadenceChannel, CadenceError, CadenceReport, CadenceTier,
    ChannelId, OptionId, PhaseRule,
};

/// A unit with an importance tier (as a game would derive it from sim state every peer shares).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unit {
    id: u32,
    tier: u8,
}

impl AiAgent for Unit {
    fn entity_id(&self) -> u32 {
        self.id
    }

    fn cadence_tier(&self) -> u8 {
        self.tier
    }
}

fn unit(id: u32) -> Unit {
    Unit { id, tier: 0 }
}

/// The ids `due_units` returned, in its order.
fn ids(units: &[Unit], out: &[usize]) -> Vec<u32> {
    out.iter().map(|&i| units[i].id).collect()
}

fn per_tick_counts(cadence: &AiCadence, channel: ChannelId, ids: &[u32], window: u64) -> Vec<u32> {
    (0..window)
        .map(|tick| ids.iter().filter(|&&id| cadence.due(channel, tick, id)).count() as u32)
        .collect()
}

#[test]
fn hashed_phase_spreads_clustered_ids() {
    // 100 units whose ids are all multiples of the 10-tick period: the id-modulo phase puts every one of them on
    // tick 0 of the window; the hashed phase spreads them within ceil(100 / 10) + 2 per tick, for every salt
    // tried (not a picked one).
    let clustered: Vec<u32> = (0..100u32).map(|i| i * 10).collect();
    let modulo = AiCadence::new(10, 6)
        .with_channel(CadenceChannel::new("think", 10))
        .expect("a fresh name");
    let think = modulo.channel_id("think").expect("added");
    let counts = per_tick_counts(&modulo, think, &clustered, 10);
    assert_eq!(counts[0], 100, "id modulo: all on one tick {counts:?}");
    assert_eq!(counts[1..].iter().sum::<u32>(), 0);

    let bound = 100u32.div_ceil(10) + 2;
    for salt in 0..256u64 {
        let mut hashed = AiCadence::default();
        let think = hashed
            .add_channel(CadenceChannel::new("think", 10).with_phase(PhaseRule::Hashed { salt }))
            .expect("a fresh name");
        let counts = per_tick_counts(&hashed, think, &clustered, 10);
        assert_eq!(
            counts.iter().sum::<u32>(),
            100,
            "salt {salt}: each unit once per window"
        );
        assert!(
            counts.iter().all(|&n| n <= bound),
            "salt {salt}: per-tick counts {counts:?} over the bound {bound}"
        );
        // The phase repeats every period: the next window is the same slice.
        assert_eq!(counts, per_tick_counts(&hashed, think, &clustered, 20)[..10]);
        for &id in &clustered {
            let next = hashed.next_tick(think, 0, id).expect("known channel");
            assert!(next < 10 && hashed.due(think, next, id));
            assert_eq!(hashed.next_tick(think, next + 1, id), Some(next + 10));
        }
    }
    // Contiguous ids spread evenly too (within one of the mean).
    let mut hashed = AiCadence::default();
    let think = hashed
        .add_channel(CadenceChannel::new("think", 10).with_phase(PhaseRule::Hashed { salt: 7 }))
        .expect("a fresh name");
    let contiguous: Vec<u32> = (1000..1100).collect();
    let counts = per_tick_counts(&hashed, think, &contiguous, 10);
    assert!(counts.iter().all(|&n| n <= 11), "contiguous ids {counts:?}");
}

#[test]
fn cap_carries_over_in_wait_then_id_order() {
    // A 4-tick channel with a cap of 3. Over the cap, units wait for later ticks; the longest-waiting go first
    // and, among equal waits, the lowest entity id. Each tick's due list is in ascending entity id.
    let mut cadence = AiCadence::default();
    let think = cadence
        .add_channel(CadenceChannel::new("think", 4).with_max_per_tick(Some(3)))
        .expect("a fresh name");
    let mut core = AiCore::new(1, cadence, ArgMax);
    // Scrambled slice order on purpose.
    let units: Vec<Unit> = [16u32, 9, 0, 12, 5, 4, 8, 1, 6, 2]
        .into_iter()
        .map(unit)
        .collect();
    let mut out = Vec::new();

    // Tick 0: due 0, 4, 8, 12, 16 (phase 0); the cap runs 0, 4, 8 and carries 12 and 16.
    let r = core.due_units(think, 0, &units, &mut out).expect("known channel");
    assert_eq!(ids(&units, &out), [0, 4, 8]);
    assert_eq!(
        r,
        CadenceReport {
            channel: think,
            tick: 0,
            due: 5,
            ran: 3,
            carried: 2,
            max_wait: 0
        }
    );
    // Tick 1: due 1, 5, 9; 12 and 16 have waited a tick, so they go first: 12, 16 and then the lowest new id, 1.
    let r = core.due_units(think, 1, &units, &mut out).expect("known channel");
    assert_eq!(ids(&units, &out), [1, 12, 16], "ascending id in the list");
    assert_eq!((r.due, r.ran, r.carried, r.max_wait), (3, 3, 2, 1));
    // Tick 2: due 2, 6; 5 and 9 waited a tick: 5, 9, then 2; 6 waits.
    let r = core.due_units(think, 2, &units, &mut out).expect("known channel");
    assert_eq!(ids(&units, &out), [2, 5, 9]);
    assert_eq!((r.due, r.ran, r.carried, r.max_wait), (2, 3, 1, 1));
    let queued: Vec<(u32, u64)> = core
        .queued(think)
        .expect("known channel")
        .iter()
        .map(|q| (q.entity, q.since))
        .collect();
    assert_eq!(queued, [(6, 2)]);
    // Tick 3: nothing new is due (no id has phase 3); 6 runs.
    let r = core.due_units(think, 3, &units, &mut out).expect("known channel");
    assert_eq!(ids(&units, &out), [6]);
    assert_eq!((r.due, r.ran, r.carried, r.max_wait), (0, 1, 0, 1));
    assert_eq!(core.cadence_report(think), Some(r));

    // Wait beats id: a high id that has waited longer runs before a low id that is new.
    let mut cadence = AiCadence::default();
    let every = cadence
        .add_channel(CadenceChannel::new("every", 1).with_max_per_tick(Some(1)))
        .expect("a fresh name");
    let mut core = AiCore::new(1, cadence, ArgMax);
    let three: Vec<Unit> = [30u32, 10, 20].into_iter().map(unit).collect();
    let mut order = Vec::new();
    for tick in 0..9u64 {
        let r = core
            .due_units(every, tick, &three, &mut out)
            .expect("known channel");
        assert_eq!(r.ran, 1);
        assert!(r.max_wait <= 2, "tick {tick}: bound ceil(3 / 1) - 1");
        order.extend(ids(&three, &out));
    }
    // A unit already waiting that falls due again keeps its older place (one run serves both).
    assert_eq!(order, [10, 20, 30, 10, 20, 30, 10, 20, 30]);

    // The decide channel takes a cap too; `decide` runs exactly the capped list.
    let mut cadence = AiCadence::new(2, 6);
    cadence
        .set_max_per_tick(ChannelId::DECIDE, Some(2))
        .expect("decide is always there");
    let mut core = AiCore::new(1, cadence, ArgMax);
    let six: Vec<Unit> = [10u32, 8, 6, 4, 2, 0].into_iter().map(unit).collect();
    let mut called = Vec::new();
    let report = core.decide(0, &six, |u, _, scores| {
        called.push(u.id);
        scores.push((OptionId::Hold, 1.0));
    });
    assert_eq!(called, [0, 2]);
    assert_eq!(report.due, 2, "AiTickReport::due counts the units that ran");
    let r = core.cadence_report(ChannelId::DECIDE).expect("decide ran");
    assert_eq!((r.due, r.ran, r.carried), (6, 2, 4));
    // Forgetting a unit also drops it from every queue.
    core.forget(4);
    let queued: Vec<u32> = core
        .queued(ChannelId::DECIDE)
        .expect("known channel")
        .iter()
        .map(|q| q.entity)
        .collect();
    assert_eq!(queued, [6, 8, 10]);
}

/// A deterministic tier for unit `id` on `tick`: re-drawn every 150 ticks (a game would read sim state).
fn tier_at(id: u32, tick: u64) -> u8 {
    (AiRng::mix(u64::from(id), tick / 150) % 3) as u8
}

#[test]
fn no_unit_waits_longer_than_the_bound() {
    // 900 seeded units (clustered ids: many multiples of the period), tiers 1x, 2x and 4x re-drawn every 150
    // ticks, 3,600 ticks. Under a cap the load fits (90 per tick) and one it does not (20 per tick), for both
    // phase rules: no queue wait passes ceil(N / cap) - 1 and no gap between two runs of a unit passes
    // period x lcm(multipliers) + that wait (the ADR's bound), and every due unit runs.
    let mut rng = AiRng::new(0xCADE_0900);
    let mut ids: Vec<u32> = Vec::new();
    while ids.len() < 900 {
        let id = if ids.len().is_multiple_of(3) {
            (rng.range(0, 4000) as u32) * 10
        } else {
            rng.range(0, 1_000_000) as u32
        };
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let tiers = vec![CadenceTier::new(1), CadenceTier::new(2), CadenceTier::new(4)];
    for phase in [PhaseRule::IdModulo, PhaseRule::Hashed { salt: 0x51 }] {
        for cap in [90u32, 20] {
            let cadence = AiCadence::default()
                .with_tiers(tiers.clone())
                .expect("three tiers")
                .with_channel(
                    CadenceChannel::new("think", 10)
                        .with_phase(phase)
                        .with_max_per_tick(Some(cap)),
                )
                .expect("a fresh name");
            let think = cadence.channel_id("think").expect("added");
            let wait_bound = cadence.max_queue_wait(think, 900).expect("known channel");
            let gap_bound = cadence.max_gap(think, 900).expect("known channel");
            assert_eq!(wait_bound, u64::from(900u32.div_ceil(cap)) - 1);
            assert_eq!(gap_bound, 10 * 4 + wait_bound);
            let mut core = AiCore::new(1, cadence, ArgMax);
            let mut last_run: Vec<Option<u64>> = vec![None; ids.len()];
            let mut units: Vec<Unit> = ids.iter().map(|&id| unit(id)).collect();
            let mut out = Vec::new();
            let (mut due_total, mut ran_total, mut worst_wait, mut worst_gap) = (0u64, 0u64, 0u64, 0u64);
            for tick in 0..3_600u64 {
                for u in units.iter_mut() {
                    u.tier = tier_at(u.id, tick);
                }
                let r = core
                    .due_units(think, tick, &units, &mut out)
                    .expect("known channel");
                assert!(r.ran <= cap, "tick {tick}: ran {} over the cap", r.ran);
                assert!(
                    r.max_wait <= wait_bound,
                    "{phase:?} cap {cap} tick {tick}: waited {} > {wait_bound}",
                    r.max_wait
                );
                assert!(out.windows(2).all(|w| units[w[0]].id < units[w[1]].id));
                worst_wait = worst_wait.max(r.max_wait);
                due_total += u64::from(r.due);
                ran_total += u64::from(r.ran);
                for &i in &out {
                    if let Some(prev) = last_run[i] {
                        let gap = tick - prev;
                        assert!(
                            gap <= gap_bound,
                            "{phase:?} cap {cap}: unit {} ran at {prev} then {tick} (bound {gap_bound})",
                            units[i].id
                        );
                        worst_gap = worst_gap.max(gap);
                    }
                    last_run[i] = Some(tick);
                }
            }
            // Every unit ran, the first time within one slowest period plus the wait bound.
            assert!(last_run.iter().all(Option::is_some));
            let carried = u64::from(core.cadence_report(think).expect("ran").carried);
            if cap == 90 {
                // The cap fits the load: every due unit ran (merges aside, nothing piles up).
                assert!(carried < u64::from(cap), "carried {carried}");
                assert!(ran_total + carried <= due_total);
            } else {
                // Over the load the queue saturates, yet the bound still holds.
                assert!(worst_wait > 0 && carried > 0);
            }
            eprintln!(
                "{phase:?} cap {cap}: due {due_total} ran {ran_total} worst wait {worst_wait} worst gap {worst_gap}"
            );
        }
    }
}

#[test]
fn tier_change_keeps_the_phase_rule() {
    // A unit thinks when tick % (period x m) == base + period x sub, with base its tier-free phase. Every think
    // tick of the unit sits on its base lattice (tick % period == base) whatever its tier, and after a tier
    // change it next thinks at the documented `next_tick_in_tier`.
    let tiers = vec![CadenceTier::new(1), CadenceTier::new(2), CadenceTier::new(4)];
    for phase in [PhaseRule::IdModulo, PhaseRule::Hashed { salt: 3 }] {
        let mut cadence = AiCadence::new(6, 6)
            .with_tiers(tiers.clone())
            .expect("three tiers");
        cadence
            .set_phase(ChannelId::DECIDE, phase)
            .expect("decide is always there");
        let mut core = AiCore::new(9, cadence.clone(), ArgMax);
        for id in [0u32, 7, 23, 60, 61, 1234] {
            let base = cadence.phase(ChannelId::DECIDE, 0, id).expect("known channel");
            assert!(base < 6);
            for tier in 0..3u8 {
                let p = cadence.phase(ChannelId::DECIDE, tier, id).expect("known channel");
                assert_eq!(p % 6, base, "{phase:?} id {id} tier {tier}: base phase kept");
                assert_eq!(
                    cadence.period(ChannelId::DECIDE, tier),
                    Some(6 * [1, 2, 4][tier as usize])
                );
            }
            // Nested multipliers nest the think ticks: a 4x tick is also a 2x tick and a 1x tick.
            for tick in 0..96u64 {
                if cadence.due_in_tier(ChannelId::DECIDE, 2, tick, id) {
                    assert!(cadence.due_in_tier(ChannelId::DECIDE, 1, tick, id));
                }
                if cadence.due_in_tier(ChannelId::DECIDE, 1, tick, id) {
                    assert!(cadence.due_in_tier(ChannelId::DECIDE, 0, tick, id));
                }
            }
        }
        // One unit: tier 0 for ticks 0..20, tier 2 (4x) from 20, tier 1 (2x) from 50.
        let id = 23u32;
        let tier_of = |tick: u64| -> u8 {
            match tick {
                0..20 => 0,
                20..50 => 2,
                _ => 1,
            }
        };
        let mut thought = Vec::new();
        for tick in 0..100u64 {
            let u = [Unit {
                id,
                tier: tier_of(tick),
            }];
            let report = core.decide(tick, &u, |_, _, s| s.push((OptionId::Hold, 1.0)));
            if report.due == 1 {
                thought.push(tick);
            }
        }
        let base = cadence.phase(ChannelId::DECIDE, 0, id).expect("known channel");
        let mut expected = Vec::new();
        let mut tick = 0u64;
        while tick < 100 {
            let tier = tier_of(tick);
            let next = cadence
                .next_tick_in_tier(ChannelId::DECIDE, tier, tick, id)
                .expect("known channel");
            // Stay inside the stretch of ticks with this tier; otherwise re-ask from the change.
            let end = match tick {
                0..20 => 20,
                20..50 => 50,
                _ => 100,
            };
            if next < end {
                expected.push(next);
                tick = next + 1;
            } else {
                tick = end;
            }
        }
        assert_eq!(thought, expected, "{phase:?}");
        assert!(
            thought.iter().all(|t| t % 6 == base),
            "{phase:?}: {thought:?} off the base lattice"
        );
        // The documented wait after a change is at most the new period minus one.
        let after = thought.iter().find(|&&t| t >= 20).copied().expect("thinks in 4x");
        assert!(after - 20 < 24, "{phase:?}: {after}");
        let after = thought.iter().find(|&&t| t >= 50).copied().expect("thinks in 2x");
        assert!(after - 50 < 12, "{phase:?}: {after}");
    }
    // Tier indices past the list use the last tier; no tiers means every tier is 1x.
    let cadence = AiCadence::default()
        .with_tiers(vec![CadenceTier::new(3)])
        .expect("one tier");
    assert_eq!(cadence.multiplier(0), 3);
    assert_eq!(cadence.multiplier(200), 3);
    assert_eq!(AiCadence::default().multiplier(5), 1);
}

#[test]
fn channels_are_independent() {
    // A `route` channel at 30 ticks (hashed) and a `target` channel at 4 ticks (id modulo, capped) next to the
    // default decide and sight channels: each runs on its own phase, a cap on one never touches another, and
    // `due_units` lists are in ascending entity id whatever the slice order.
    let cadence = AiCadence::default()
        .with_channel(CadenceChannel::new("route", 30).with_phase(PhaseRule::Hashed { salt: 0x40 }))
        .expect("a fresh name")
        .with_channel(CadenceChannel::new("target", 4).with_max_per_tick(Some(5)))
        .expect("a fresh name");
    let route = cadence.channel_id("route").expect("added");
    let target = cadence.channel_id("target").expect("added");
    assert_eq!(cadence.channel_id("decide"), Some(ChannelId::DECIDE));
    assert_eq!(cadence.channel_id("sight"), Some(ChannelId::SIGHT));
    assert_eq!(cadence.channel_id("nope"), None);
    assert_eq!(cadence.channels().len(), 4);
    let mut core = AiCore::new(5, cadence.clone(), ArgMax);
    let mut uncapped = AiCore::new(5, AiCadence::default(), ArgMax);
    let mut units: Vec<Unit> = (0..50u32).map(|i| unit(i * 7 + 3)).collect();
    units.reverse();
    units.swap(2, 30);
    let mut out = Vec::new();
    let mut route_ticks: Vec<Vec<u64>> = vec![Vec::new(); units.len()];
    let mut target_runs = 0u32;
    for tick in 0..120u64 {
        let r = core
            .due_units(route, tick, &units, &mut out)
            .expect("known channel");
        let expected: Vec<u32> = {
            let mut v: Vec<u32> = units
                .iter()
                .filter(|u| cadence.due(route, tick, u.id))
                .map(|u| u.id)
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(ids(&units, &out), expected, "route tick {tick}");
        assert_eq!((r.carried, r.max_wait), (0, 0), "route is uncapped");
        for &i in &out {
            route_ticks[i].push(tick);
        }
        let r = core
            .due_units(target, tick, &units, &mut out)
            .expect("known channel");
        assert!(r.ran <= 5);
        assert!(out.windows(2).all(|w| units[w[0]].id < units[w[1]].id));
        target_runs += r.ran;
        // Decide on the same core is the default schedule, untouched by the other channels' queues.
        let mut called = Vec::new();
        core.decide(tick, &units, |u, _, s| {
            called.push(u.id);
            s.push((OptionId::Hold, 1.0));
        });
        let mut plain = Vec::new();
        uncapped.decide(tick, &units, |u, _, s| {
            plain.push(u.id);
            s.push((OptionId::Hold, 1.0));
        });
        assert_eq!(called, plain, "decide tick {tick}");
    }
    for (i, ticks) in route_ticks.iter().enumerate() {
        assert_eq!(
            ticks.len(),
            4,
            "unit {}: one route refresh per 30 ticks",
            units[i].id
        );
        assert!(ticks.windows(2).all(|w| w[1] - w[0] == 30));
    }
    // 50 units every 4 ticks is 12.5 per tick on average, over the cap of 5: the queue holds the rest.
    assert!(target_runs >= 5 * 100, "target ran {target_runs}");
    let q = core.queued(target).expect("known channel");
    assert!(!q.is_empty() && q.windows(2).all(|w| w[0].entity < w[1].entity));
    assert!(core.queued(route).expect("known channel").is_empty());
    // An unknown channel is an error, not an empty list.
    let foreign = ChannelId::from_index(9);
    assert_eq!(
        core.due_units(foreign, 0, &units, &mut out).map(|_| ()),
        Err(CadenceError::UnknownChannel { index: 9 })
    );
    assert!(out.is_empty());
    assert_eq!(cadence.next_tick(foreign, 0, 1), None);
    assert!(!cadence.due(foreign, 0, 1));
}

#[test]
fn cadence_settings_round_trip_and_validate() {
    // The default writes and reads the two legacy fields as before, so old settings and snapshots read back.
    let legacy: AiCadence =
        serde_json::from_str(r#"{"decision_ticks":10,"sight_ticks":6}"#).expect("legacy text");
    assert_eq!(legacy, AiCadence::default());
    let zero: AiCadence =
        serde_json::from_str(r#"{"decision_ticks":0,"sight_ticks":0}"#).expect("legacy text");
    assert_eq!(zero, AiCadence::new(1, 1), "zero ticks clamp to 1 as before");
    assert_eq!(AiCadence::new(0, 0), AiCadence::new(1, 1));

    // A full cadence round-trips.
    let mut full = AiCadence::new(12, 5)
        .with_tiers(vec![
            CadenceTier::new(1),
            CadenceTier::new(0),
            CadenceTier::new(8),
        ])
        .expect("three tiers")
        .with_channel(
            CadenceChannel::new("route", 0)
                .with_phase(PhaseRule::Hashed { salt: u64::MAX })
                .with_max_per_tick(Some(0)),
        )
        .expect("a fresh name");
    full.set_max_per_tick(ChannelId::SIGHT, Some(40))
        .expect("sight is always there");
    full.set_ticks(ChannelId::DECIDE, 0)
        .expect("decide is always there");
    let route = full.channel_id("route").expect("added");
    let ch = full.channel(route).expect("known channel");
    assert_eq!(
        (ch.ticks, ch.max_per_tick),
        (1, Some(1)),
        "zero ticks and a zero cap clamp to 1"
    );
    assert_eq!(full.decision_ticks(), 1);
    assert_eq!(full.tiers()[1].multiplier, 1, "a zero multiplier clamps to 1");
    let text = serde_json::to_string(&full).expect("serializes");
    let back: AiCadence = serde_json::from_str(&text).expect("reads back");
    assert_eq!(back, full, "{text}");

    // Names: empty and duplicate names are refused with stable codes.
    let mut c = AiCadence::default();
    let err = c.add_channel(CadenceChannel::new("", 4)).expect_err("empty name");
    assert_eq!(err, CadenceError::EmptyName);
    assert_eq!(err.code(), "ai_cadence_empty_name");
    let err = c
        .add_channel(CadenceChannel::new("decide", 4))
        .expect_err("duplicate");
    assert_eq!(err.code(), "ai_cadence_duplicate_name");
    assert!(err.to_string().contains("decide"), "{err}");
    c.add_channel(CadenceChannel::new("route", 4)).expect("fresh");
    assert_eq!(
        c.add_channel(CadenceChannel::new("route", 9)).map(|_| ()),
        Err(CadenceError::DuplicateName { name: "route".into() })
    );
    assert_eq!(c.channels().len(), 3, "a refused channel is not added");
    assert_eq!(
        c.set_phase(ChannelId::from_index(3), PhaseRule::IdModulo)
            .map_err(|e| e.code()),
        Err("ai_cadence_unknown_channel")
    );

    // The same checks guard deserialization.
    let dup = r#"{"decision_ticks":10,"sight_ticks":6,"channels":[
        {"name":"decide","ticks":10,"phase":"IdModulo","max_per_tick":null},
        {"name":"sight","ticks":6,"phase":"IdModulo","max_per_tick":null},
        {"name":"route","ticks":30,"phase":"IdModulo","max_per_tick":null},
        {"name":"route","ticks":4,"phase":"IdModulo","max_per_tick":null}],"tiers":[]}"#;
    let err = serde_json::from_str::<AiCadence>(dup).expect_err("duplicate names");
    assert!(err.to_string().contains("ai_cadence_duplicate_name"), "{err}");
    let empty = dup.replace(r#""name":"route","ticks":4"#, r#""name":"","ticks":4"#);
    let err = serde_json::from_str::<AiCadence>(&empty).expect_err("empty name");
    assert!(err.to_string().contains("ai_cadence_empty_name"), "{err}");
    let no_sight = r#"{"channels":[{"name":"decide","ticks":10,"phase":"IdModulo","max_per_tick":null}]}"#;
    let err = serde_json::from_str::<AiCadence>(no_sight).expect_err("sight missing");
    assert!(err.to_string().contains("ai_cadence_builtin_channels"), "{err}");

    // Limits.
    let mut many = AiCadence::default();
    for i in 2..regia_detsim::ai::MAX_CADENCE_CHANNELS {
        many.add_channel(CadenceChannel::new(format!("c{i}"), 2))
            .expect("under the limit");
    }
    assert_eq!(
        many.add_channel(CadenceChannel::new("one_more", 2))
            .map_err(|e| e.code()),
        Err("ai_cadence_too_many_channels")
    );
    let tiers = vec![CadenceTier::new(2); regia_detsim::ai::MAX_CADENCE_TIERS + 1];
    assert_eq!(
        AiCadence::default()
            .with_tiers(tiers)
            .map(|_| ())
            .map_err(|e| e.code()),
        Err("ai_cadence_too_many_tiers")
    );
}

#[test]
fn a_snapshot_keeps_the_queues() {
    // The carry-over queues are sim state: a snapshot taken mid-run resumes with the same due lists.
    let cadence = AiCadence::default()
        .with_channel(CadenceChannel::new("think", 3).with_max_per_tick(Some(4)))
        .expect("a fresh name");
    let think = cadence.channel_id("think").expect("added");
    let units: Vec<Unit> = (0..30u32).map(|i| unit(i * 3)).collect();
    let mut core = AiCore::new(2, cadence, ArgMax);
    let mut out = Vec::new();
    for tick in 0..10u64 {
        core.due_units(think, tick, &units, &mut out)
            .expect("known channel");
    }
    assert!(!core.queued(think).expect("known channel").is_empty());
    let text = serde_json::to_string(&core).expect("serializes");
    let mut back: AiCore<ArgMax> = serde_json::from_str(&text).expect("reads back");
    let mut out_back = Vec::new();
    for tick in 10..40u64 {
        let a = core
            .due_units(think, tick, &units, &mut out)
            .expect("known channel");
        let b = back
            .due_units(think, tick, &units, &mut out_back)
            .expect("known channel");
        assert_eq!((a, &out), (b, &out_back), "tick {tick}");
    }
    // A snapshot from before the queues existed still reads (no queues: nothing waiting).
    let old =
        r#"{"world_seed":2,"cadence":{"decision_ticks":10,"sight_ticks":6},"policy":null,"decisions":{}}"#;
    let old: AiCore<ArgMax> = serde_json::from_str(old).expect("old snapshot");
    assert_eq!(old.cadence(), &AiCadence::default());
}

/// FNV-1a over little-endian words.
fn fnv(words: impl IntoIterator<Item = u64>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for w in words {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Every due list and report of a seeded run: 900 units, tiers re-drawn every 150 ticks, a hashed route
/// channel, a capped target channel and a capped hashed decide channel, 600 ticks.
fn cadence_bits() -> u64 {
    let mut rng = AiRng::new(0xCADE_0B17);
    let mut units: Vec<Unit> = (0..900u32)
        .map(|i| {
            unit(if i.is_multiple_of(4) {
                i * 10
            } else {
                rng.range(0, 2_000_000) as u32
            })
        })
        .collect();
    units.sort_unstable_by_key(|u| u.id);
    units.dedup_by_key(|u| u.id);
    let mut cadence = AiCadence::default()
        .with_tiers(vec![
            CadenceTier::new(1),
            CadenceTier::new(2),
            CadenceTier::new(4),
        ])
        .and_then(|c| {
            c.with_channel(CadenceChannel::new("route", 30).with_phase(PhaseRule::Hashed { salt: 11 }))
        })
        .and_then(|c| c.with_channel(CadenceChannel::new("target", 4).with_max_per_tick(Some(150))))
        .expect("valid settings");
    cadence
        .set_phase(ChannelId::DECIDE, PhaseRule::Hashed { salt: 12 })
        .expect("decide is always there");
    cadence
        .set_max_per_tick(ChannelId::DECIDE, Some(40))
        .expect("decide is always there");
    let channels = [
        ChannelId::DECIDE,
        ChannelId::SIGHT,
        cadence.channel_id("route").expect("added"),
        cadence.channel_id("target").expect("added"),
    ];
    let mut core = AiCore::new(3, cadence, ArgMax);
    let mut out = Vec::new();
    let mut words = Vec::new();
    for tick in 0..600u64 {
        for u in units.iter_mut() {
            u.tier = tier_at(u.id, tick);
        }
        for ch in channels {
            let r = core.due_units(ch, tick, &units, &mut out).expect("known channel");
            words.push(
                u64::from(r.due) | u64::from(r.ran) << 16 | u64::from(r.carried) << 32 | r.max_wait << 48,
            );
            words.extend(out.iter().map(|&i| u64::from(units[i].id)));
        }
    }
    fnv(words)
}

#[test]
fn cadence_bits_match_the_pinned_hash() {
    // Integer-only, so the same on every arch; the cross-arch run (aarch64 macOS, Windows) checks it like the
    // ai_core pins. A change here moves results for a game that opted into these settings.
    let got = cadence_bits();
    eprintln!("cadence bits {got:#018x}");
    assert_eq!(got, PINNED_CADENCE, "cadence due lists moved");
}

/// Recorded on x86-64 Linux (the build server).
const PINNED_CADENCE: u64 = 0xb1e0_3d1d_c25f_17ea;
