// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/tests/ai_core.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 186-197, plan row S-11, ADR-8462). `option_repertoire_matches_level` keeps DO27's assertions
// (S-09 left it for this row: it needs the difficulty profiles). The other tests are new: the hysteresis loop as
// the AI core's decision policy, scoring filtered and biased by a profile, aim error ordered by level through the
// fire gate, and the bit pins.

//! Tactics, difficulty and aim (plan row S-11, ADR-8462): the hysteresis decision loop, difficulty as decision
//! quality, lead targeting and aim error.

use regia_detsim::ai::{
    AiAgent, AiCadence, AiCore, AiLevel, AiRng, AmmoSlotView, AmmoView, Decision, DifficultyProfile,
    FireTarget, Hysteresis, Inputs, OpenField, OptionId, ShooterView, WeaponRole, aim_error, ballistic_angle,
    catalogue, decide, fire_gate, force_ratio_norm, lead_point,
};
use regia_detsim::math;

/// A unit of the test world.
#[derive(Debug, Clone, Copy)]
struct Unit {
    id: u32,
    level: AiLevel,
}

impl AiAgent for Unit {
    fn entity_id(&self) -> u32 {
        self.id
    }
}

/// Test inputs for one unit on one tick: every input moves with the tick and the id (lockstep trig only).
fn inputs_for(id: u32, tick: u64) -> Inputs {
    let t = tick as f32 * 0.013 + id as f32 * 0.71;
    let wave = |k: f32| 0.5 + 0.5 * math::sin(t * k + id as f32);
    Inputs {
        hull_pct: wave(0.9),
        ammo_pct: wave(0.4),
        range_band_frac: wave(1.3),
        local_force_ratio: force_ratio_norm(2.0 * wave(0.7)),
        cover_availability: wave(2.1),
        distance_to_leader_frac: wave(0.5),
        distance_to_threat_frac: wave(1.7),
        threat_incoming: if wave(3.1) > 0.6 { 1.0 } else { 0.0 },
        commander_priority: wave(0.3),
        scrap_proximity: wave(1.1),
        point_distance_frac: wave(0.8),
        leader_target_focus: if id.is_multiple_of(3) { 1.0 } else { 0.0 },
        relative_energy_frac: wave(2.6),
        turn_rate_frac: wave(1.9),
        hopper_frac: wave(0.2),
    }
}

#[test]
fn option_repertoire_matches_level() {
    // Easy never picks Flank or Dogfight; Hard can pick anything.
    let easy = DifficultyProfile::easy();
    let hard = DifficultyProfile::hard();
    assert!(!easy.repertoire.contains(OptionId::Flank));
    assert!(!easy.repertoire.contains(OptionId::Dogfight));
    assert!(hard.repertoire.contains(OptionId::Flank));
    assert!(hard.repertoire.contains(OptionId::Dogfight));
    assert!(hard.repertoire.contains(OptionId::Push));
    assert!(easy.repertoire.contains(OptionId::Push));
}

/// The hysteresis loop plugs into the AI core as its decision policy: a unit due every tick holds its option
/// through a lead smaller than the margin, and switches after the challenger leads by the margin for
/// `min_hold_ticks` decisions in a row.
#[test]
fn hysteresis_is_the_cores_decision_policy() {
    let hyst = Hysteresis::default();
    let mut core = AiCore::new(1, AiCadence::new(1, 1), hyst);
    let units = [7u32];
    // Tick 0: Hold is best and is committed at once.
    core.decide(0, &units, |_, _, s| {
        s.extend([(OptionId::Hold, 0.5), (OptionId::Push, 0.3)])
    });
    assert_eq!(core.decision_for(7), Some(OptionId::Hold));
    // A knife-edge lead (0.05 < 0.15): hold, every tick.
    for tick in 1..20 {
        core.decide(tick, &units, |_, _, s| {
            s.extend([(OptionId::Hold, 0.5), (OptionId::Push, 0.55)])
        });
        assert_eq!(core.decision_for(7), Some(OptionId::Hold), "tick {tick}");
    }
    // A sustained lead past the margin: Push after min_hold_ticks decisions, not before.
    let mut switched_at = None;
    for tick in 20..40u64 {
        core.decide(tick, &units, |_, _, s| {
            s.extend([(OptionId::Hold, 0.5), (OptionId::Push, 0.7)])
        });
        if switched_at.is_none() && core.decision_for(7) == Some(OptionId::Push) {
            switched_at = Some(tick);
        }
    }
    assert_eq!(
        switched_at,
        Some(20 + u64::from(hyst.min_hold_ticks) - 1),
        "the challenger wins on its min_hold_ticks-th leading decision"
    );
    let row = core.decision(7).copied().unwrap_or_default();
    assert_eq!(row.current, Some(OptionId::Push));
    assert_eq!(row.challenger, None);
    // An invalidated option switches at once, outside the schedule.
    let entry = core.decision_entry(7);
    let chosen = decide(
        &hyst,
        entry,
        &[(OptionId::Push, 0.2), (OptionId::Retreat, 0.9)],
        true,
    );
    assert_eq!(chosen, OptionId::Retreat);
    assert_eq!(core.decision_for(7), Some(OptionId::Retreat));
}

/// A profile scores only the options its level may pick: its repertoire, minus the team abilities its
/// coordination lacks (flank, focus fire), each scaled by its aggression or caution bias.
#[test]
fn a_profile_scores_only_what_its_level_may_pick() {
    let inputs = inputs_for(3, 77);
    let picked = |p: &DifficultyProfile| {
        let mut out = Vec::new();
        p.score_options(&catalogue(&p.thresholds), &inputs, &mut out);
        out
    };
    let ids = |s: &[(OptionId, f32)]| s.iter().map(|(id, _)| *id).collect::<Vec<_>>();

    let easy = picked(&DifficultyProfile::easy());
    assert_eq!(ids(&easy), [OptionId::Push, OptionId::Hold, OptionId::Regroup]);
    let medium = picked(&DifficultyProfile::medium());
    assert_eq!(
        ids(&medium),
        [
            OptionId::Push,
            OptionId::Hold,
            OptionId::Strafe,
            OptionId::Kite,
            OptionId::Retreat,
            OptionId::Regroup
        ]
    );
    let hard = picked(&DifficultyProfile::hard());
    assert_eq!(ids(&hard), OptionId::ALL);
    for (_, s) in easy.iter().chain(&medium).chain(&hard) {
        assert!((0.0..=1.0).contains(s), "scores are clamped to 0..1: {s}");
    }
    // Without its flank ability a Hard repertoire still never scores Flank.
    let no_flank = DifficultyProfile {
        coordination: regia_detsim::ai::CoordinationMask {
            flank: false,
            ..DifficultyProfile::hard().coordination
        },
        ..DifficultyProfile::hard()
    };
    assert!(!ids(&picked(&no_flank)).contains(&OptionId::Flank));
    assert!(no_flank.allows(OptionId::Dogfight) && !no_flank.allows(OptionId::Flank));
}

/// The same shot, the same random stream: a harder level misses less (smaller lead error, narrower spread, less
/// tracking lag), and the shot itself (slot, target) is the same at every level.
#[test]
fn harder_bots_miss_less_on_the_same_shot() {
    let shooter = ShooterView::new(
        [0.0, 0.0, 0.0],
        AmmoView {
            slots: vec![Some(AmmoSlotView {
                role: WeaponRole::Direct,
                projectile_speed: 120.0,
                ammo: None,
                band: [0.0, 150.0],
            })],
        },
    );
    let target = FireTarget {
        id: 9,
        pos: [0.0, 0.0, 100.0],
        vel: [10.0, 0.0, 0.0],
        is_unit: true,
        range: 100.0,
    };
    let shot = |level: AiLevel| {
        let mut rng = AiRng::tick_stream(5, 40, 1);
        fire_gate(
            &shooter,
            &target,
            &DifficultyProfile::for_level(level).aim,
            &mut rng,
            OpenField,
            &[],
        )
    };
    let [easy, medium, hard] = [AiLevel::Easy, AiLevel::Medium, AiLevel::Hard].map(shot);
    let (Some(easy), Some(medium), Some(hard)) = (easy, medium, hard) else {
        panic!("every level fires the same in-band shot: {easy:?} {medium:?} {hard:?}");
    };
    assert_eq!((easy.slot, easy.target), (hard.slot, hard.target));
    assert!(easy.aim_error.lead > medium.aim_error.lead && medium.aim_error.lead > hard.aim_error.lead);
    assert!(
        easy.aim_error.spread_mrad > medium.aim_error.spread_mrad
            && medium.aim_error.spread_mrad > hard.aim_error.spread_mrad
    );
    assert!(easy.aim_error.tracking_lag_ticks > hard.aim_error.tracking_lag_ticks);
    // The lead error is a share of the target's travel during the shell's flight: 10 m/s * 100 m / 120 m/s.
    let lead_distance = 10.0 * (100.0 / 120.0);
    assert_eq!(medium.aim_error.lead, 0.5 * lead_distance);
}

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

/// 600 ticks of 48 units, a third at each level, on the default cadence: each due unit scores its level's
/// catalogue through its profile and commits through the hysteresis loop. Hashes every committed decision.
fn tactics_bits() -> u64 {
    let units: Vec<Unit> = (0..48u32)
        .map(|id| Unit {
            id: id * 5 + 2,
            level: [AiLevel::Easy, AiLevel::Medium, AiLevel::Hard][(id % 3) as usize],
        })
        .collect();
    let mut core = AiCore::new(0xD1FF, AiCadence::default(), Hysteresis::default());
    let mut words = Vec::new();
    let mut switches = 0u32;
    let mut last = std::collections::BTreeMap::new();
    for tick in 0..600u64 {
        core.decide(tick, &units, |u, rng, scores| {
            let profile = DifficultyProfile::for_level(u.level);
            let mut inputs = inputs_for(u.id, tick);
            // One random input per decision, from the unit's tick stream.
            inputs.cover_availability = rng.next_f32();
            profile.score_options(&catalogue(&profile.thresholds), &inputs, scores);
        });
        for (&id, d) in core.decisions() {
            let Decision {
                current,
                held_ticks,
                challenger,
                challenger_ticks,
            } = *d;
            let code = |o: Option<OptionId>| o.map_or(0xff, |o| u64::from(o.bit()));
            words.push(u64::from(id) | code(current) << 32 | code(challenger) << 40);
            words.push(u64::from(held_ticks) | u64::from(challenger_ticks) << 32);
            if last.insert(id, current).is_some_and(|before| before != current) {
                switches += 1;
            }
        }
    }
    assert!(
        switches > 10,
        "the pinned scenario must switch options: {switches}"
    );
    fnv(words)
}

/// Lead points, ballistic angles and aim errors over a fixed sweep, as bits.
fn aim_bits() -> u64 {
    let mut words = Vec::new();
    let mut rng = AiRng::new(0xA1);
    for i in 0..200u32 {
        let a = i as f32 * 0.37;
        let shooter = [math::sin(a) * 20.0, 1.0 + (i % 7) as f32, math::cos(a) * 15.0];
        let target = [
            math::cos(a * 1.3) * 180.0,
            (i % 11) as f32 - 3.0,
            math::sin(a * 0.7) * 160.0,
        ];
        let vel = [math::sin(a * 2.0) * 25.0, 0.0, math::cos(a * 1.1) * 25.0];
        let speed = 60.0 + (i % 5) as f32 * 40.0;
        let lead = lead_point(shooter, target, vel, speed);
        words.extend(lead.iter().map(|v| u64::from(v.to_bits())));
        words.push(u64::from(ballistic_angle(shooter, target, speed, 9.81).to_bits()));
        let level = [AiLevel::Easy, AiLevel::Medium, AiLevel::Hard][(i % 3) as usize];
        let e = aim_error(&DifficultyProfile::for_level(level).aim, &mut rng, a);
        words.push(u64::from(e.lead.to_bits()) | u64::from(e.spread_mrad.to_bits()) << 32);
    }
    fnv(words)
}

#[test]
fn tactics_and_aim_bits_match_the_pinned_hashes() {
    // The pins are the same on x86-64 and aarch64 (the cross-arch proof runs this test there too: the Mac, the
    // Android test device and qemu-user). A change here is a sim-version change (ADR-8012 A-4).
    let got = [tactics_bits(), aim_bits()];
    eprintln!("tactics bits: decisions {:#018x} aim {:#018x}", got[0], got[1]);
    assert_eq!(got, PINNED, "tactics or aim bits moved");
}

/// Pinned on x86-64 Linux (the build server); see `tactics_and_aim_bits_match_the_pinned_hashes`.
const PINNED: [u64; 2] = [0x445e_caee_8887_76ed, 0x4adc_91e9_9533_e12b];
