// SPDX-License-Identifier: MPL-2.0
//! Rollback lockstep acceptance over the impaired network (spec 120 § 7.7, ADR-8361): a real
//! `NetServer` and real `NetClient`s carry a `LockstepHost` and rollback seats in virtual time.

use regia_net::lockstep::{
    LockstepConfig, LockstepFrame, MAX_ROLLBACK_TICKS, MembershipChange, MembershipKind, SeatId, SeatInput,
};
use regia_net_soak::DEFAULT_SEED;
use regia_net_soak::lockstep::{
    InputMix, InputSource, LockSim, LockstepRecord, LockstepScenario, run_lockstep,
};
use regia_net_soak::profiles::Profile;

fn named(name: &str) -> Profile {
    Profile::named(name).unwrap_or_else(|| panic!("profile {name}"))
}

fn summary(label: &str, r: &LockstepRecord) {
    for s in &r.seats {
        let st = &s.status;
        println!(
            "{label}: seat {} confirmed {} predicted {} rollbacks {} (max {} ticks, {} replayed) \
             confirmed-without-rollback {} window stalls {} local lag {:?} delay {}",
            s.seat,
            s.frames_stepped,
            st.ticks_predicted,
            st.rollbacks,
            st.rollback_ticks_max,
            st.rollback_ticks_replayed,
            st.predictions_confirmed,
            st.window_stalls,
            st.local_input_lag_ticks,
            st.delay_ticks,
        );
    }
    let missing: Vec<(u64, u64, u64)> = r
        .host
        .seats
        .iter()
        .map(|s| (s.inputs_missing, s.inputs_late_merged, s.inputs_held))
        .collect();
    println!(
        "{label}: frames {} delay mean {} milliticks max {} desyncs {} in_step {} common tick {:?} \
         (missing, late merged, held) {missing:?}",
        r.frames_built,
        r.delay_mean_milliticks,
        r.delay_max_ticks,
        r.verdicts.len(),
        r.in_step,
        r.common_tick
    );
}

fn rollback_config(delay: u16) -> LockstepConfig {
    LockstepConfig {
        initial_delay_ticks: delay,
        min_delay_ticks: delay,
        max_delay_ticks: delay,
        ..LockstepConfig::rollback(60)
    }
}

/// The lockstep reference: the match stepped offline from the seats' input scripts as if no
/// input were ever late (the capture at tick `c` is stepped at `c + delay`; the ticks below the
/// delay are the start's gap ticks), through `through`. Returns the state hash after it.
fn reference_hash(seats: u16, seed: u64, delay: u64, through: u64) -> u64 {
    let mut sources: Vec<InputSource> = (0..seats)
        .map(|s| InputSource::new(seed, s, InputMix::ORDERS))
        .collect();
    let mut sim = LockSim::default();
    for tick in 0..=through {
        let inputs = (0..seats)
            .map(|s| {
                let input = if tick < delay {
                    SeatInput::Absent
                } else {
                    SeatInput::Present(sources[usize::from(s)].next_input())
                };
                (SeatId(s), input)
            })
            .collect();
        let membership = if tick == 0 {
            (0..seats)
                .map(|s| MembershipChange {
                    seat: SeatId(s),
                    kind: MembershipKind::Join,
                })
                .collect()
        } else {
            Vec::new()
        };
        sim.step(&LockstepFrame {
            tick,
            inputs,
            delay: delay as u16,
            delay_from: 0,
            membership,
            match_end: false,
        });
    }
    sim.hash()
}

/// Spec 120 § 7.7 acceptance: two seats on transatlantic links, rollback lockstep with an input
/// delay of 1, 2 and 3 ticks. The local input is stepped exactly the configured delay after it
/// was sampled, no rollback is deeper than 8 ticks, and the confirmed state of every simulation
/// (both seats and the observer host) equals the lockstep reference stepped offline from the same
/// input scripts, bit for bit (the state hash folds every float's canonical bits).
#[test]
fn two_seats_on_transatlantic_roll_back_at_most_8_ticks_and_confirm_the_lockstep_reference() {
    for delay in [1u16, 2, 3] {
        let mut s = LockstepScenario::new(named("transatlantic"), 2, DEFAULT_SEED, 60_000);
        s.config = rollback_config(delay);
        let r = run_lockstep(&s).expect("the run starts");
        summary(&format!("transatlantic delay {delay}"), &r);
        assert!(r.verdicts.is_empty(), "no desync: {:?}", r.verdicts);
        assert!(r.in_step, "every simulation agrees at the end");
        assert!(
            r.host
                .seats
                .iter()
                .all(|s| s.inputs_missing == 0 && s.inputs_late_merged == 0),
            "every input made its frame"
        );
        for seat in &r.seats {
            let st = &seat.status;
            assert!(
                seat.in_order,
                "seat {}: confirmed ticks in order, once",
                seat.seat
            );
            assert_eq!(st.rollback_window_ticks, MAX_ROLLBACK_TICKS);
            assert_eq!(
                st.local_input_lag_ticks,
                Some((u64::from(delay), u64::from(delay))),
                "seat {}: the local input is stepped {delay} ticks after it was sampled",
                seat.seat
            );
            assert!(st.rollback_ticks_max <= 8, "seat {}: {st:?}", seat.seat);
            assert!(
                st.rollbacks > 0 && st.ticks_predicted > 1_000,
                "seat {}: {st:?}",
                seat.seat
            );
        }
        let tick = r.common_tick.expect("a common tick");
        assert!(tick > 3_000, "the match ran: {tick} ticks");
        assert_eq!(
            r.common_hash,
            Some(reference_hash(2, DEFAULT_SEED, u64::from(delay), tick)),
            "delay {delay}: the confirmed state is the lockstep reference's at tick {tick}"
        );
    }
}

/// Spec 120 § 7.7 acceptance: four seats on bad-wifi for ten minutes, rollback lockstep with the
/// preset (the delay controller between 1 and 3 ticks): no desync, nobody resynced or removed,
/// every simulation in step at the end, no rollback deeper than the window.
#[test]
fn four_rollback_seats_on_bad_wifi_for_ten_minutes_never_desync() {
    let mut s = LockstepScenario::new(named("bad-wifi"), 4, DEFAULT_SEED, 600_000);
    s.config = LockstepConfig::rollback(60);
    let r = run_lockstep(&s).expect("the run starts");
    summary("bad-wifi 4 seats 10 min", &r);
    assert!(r.verdicts.is_empty(), "no desync: {:?}", r.verdicts);
    assert!(r.removed.is_empty() && !r.unresolved);
    assert_eq!(r.host.resyncs, 0);
    assert!(r.in_step, "every simulation agrees at the end");
    assert!(
        r.host.checkpoints_agreed > 1_000,
        "votes were taken: {}",
        r.host.checkpoints_agreed
    );
    assert!(r.delay_max_ticks <= 3);
    for seat in &r.seats {
        let st = &seat.status;
        assert!(seat.in_order, "seat {}", seat.seat);
        assert_eq!(seat.resyncs, 0);
        assert!(
            st.rollback_ticks_max <= u64::from(MAX_ROLLBACK_TICKS),
            "seat {}: {st:?}",
            seat.seat
        );
        assert!(
            seat.frames_stepped > 30_000,
            "seat {} confirmed {}",
            seat.seat,
            seat.frames_stepped
        );
    }
}
