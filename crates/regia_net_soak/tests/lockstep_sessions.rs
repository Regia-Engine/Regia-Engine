// SPDX-License-Identifier: MPL-2.0
//! Lockstep sessions over the real transport (spec 120 § 7.4, ADR-8212): a `LockstepHost` on a
//! `NetServer` and seats on `NetClient`s over the impaired network, in virtual time. The `flappy`
//! link darkens every seat for 3 s every 20 s. One seat also takes a 12 s outage: longer than the
//! default frame log (600 ticks, 10 s at 60 Hz) and shorter than the 20 s removal timeout, so the
//! seat stays and returns through a catch-up (the host's serialized state plus every frame since)
//! with its seat id, and every simulation ends in step.
//!
//! The named `flappy` profile's long outage is the removal timeout plus 2 s (22 s, ADR-8209) so
//! that outage ends the connection. This test pins 12 s on that link and does not change the profile.

use regia_net_soak::lockstep::{LockstepScenario, SoakHost, run_lockstep};
use regia_net_soak::profiles::Profile;

/// Past the 10 s frame log, inside the 20 s removal timeout.
const CATCHUP_OUTAGE_MS: u64 = 12_000;

#[test]
fn flappy_outages_keep_every_seat_and_the_long_one_returns_through_a_catch_up() {
    let mut profile = Profile::named("flappy").expect("the flappy profile");
    profile.long_outage_ms = Some(CATCHUP_OUTAGE_MS);
    let mut s = LockstepScenario::new(profile, 4, 0x5e55_1075, 70_000);
    s.host = SoakHost::Observer;
    let rec = run_lockstep(&s).expect("run");
    for seat in &rec.seats {
        println!(
            "flappy x4: seat {} profile {} frames {} in order {} catch-ups {} interrupted {} silent_ms {} ended {:?}",
            seat.seat,
            seat.profile,
            seat.frames_stepped,
            seat.in_order,
            seat.catchups,
            rec.host
                .seats
                .iter()
                .find(|r| r.seat == seat.seat)
                .is_some_and(|r| r.interrupted),
            rec.host
                .seats
                .iter()
                .find(|r| r.seat == seat.seat)
                .map_or(0, |r| r.silent_ms),
            seat.ended,
        );
    }
    println!(
        "flappy x4: frames built {} lagging {:?} catch-ups {} refused {} removed {:?} desyncs {} in_step {}",
        rec.frames_built,
        rec.lagging,
        rec.catchups,
        rec.catchups_refused,
        rec.removed,
        rec.verdicts.len(),
        rec.in_step
    );
    assert!(
        rec.removed.is_empty(),
        "no seat is removed for a 3 s or a 12 s outage: {:?}",
        rec.removed
    );
    assert!(rec.verdicts.is_empty(), "the outages kept the checksums");
    assert!(
        !rec.lagging.is_empty(),
        "the 12 s outage is longer than the frame log"
    );
    assert_eq!(rec.catchups_refused, 0);
    assert_eq!(u64::from(rec.catchups), rec.lagging.len() as u64);
    let caught: Vec<_> = rec.seats.iter().filter(|r| r.catchups > 0).collect();
    assert_eq!(caught.len(), rec.lagging.len());
    for seat in &rec.seats {
        assert!(seat.ended.is_none(), "seat {} lost its connection", seat.seat);
        assert!(seat.in_order, "seat {} stepped out of order", seat.seat);
        assert!(
            seat.frames_stepped + 30 >= rec.frames_built,
            "seat {} is {} frames behind",
            seat.seat,
            rec.frames_built - seat.frames_stepped
        );
        assert!(
            !seat.status.catching_up,
            "seat {} is still catching up",
            seat.seat
        );
        assert!(seat.status.member, "seat {} kept its seat", seat.seat);
    }
    assert!(
        rec.host
            .seats
            .iter()
            .all(|r| r.member && !r.catching_up && !r.lagging)
    );
    assert!(rec.in_step, "every simulation agrees at the end");
}
