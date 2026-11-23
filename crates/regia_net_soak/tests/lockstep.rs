// SPDX-License-Identifier: MPL-2.0
//! Lockstep acceptance over the impaired network (spec 120 § 7.1-7.3, § 7.5; ADR-8146): a real
//! `NetServer` and real `NetClient`s carry a `LockstepHost` and its seats in virtual time.

use regia_net_soak::DEFAULT_SEED;
use regia_net_soak::lockstep::{
    InputMix, InputSource, LockInput, LockstepRecord, LockstepScenario, SoakHost, UPLOAD_BAR_RUN_MS,
    UPLOAD_BAR_SEATS_ACTION, UPLOAD_BAR_SEATS_ORDERS, UPLOAD_P95_LIMIT_BPS, UPLOAD_PEAK_LIMIT_BPS,
    run_lockstep,
};
use regia_net_soak::profiles::Profile;

fn named(name: &str) -> Profile {
    Profile::named(name).unwrap_or_else(|| panic!("profile {name}"))
}

fn run(s: &LockstepScenario) -> LockstepRecord {
    run_lockstep(s).expect("the run starts")
}

fn summary(label: &str, r: &LockstepRecord) {
    let missing: Vec<u64> = r.host.seats.iter().map(|s| s.inputs_missing).collect();
    let single: Vec<(u64, u64)> = r
        .seats
        .iter()
        .map(|s| (s.status.single_losses, s.status.single_loss_repair_ticks_max))
        .collect();
    let holes: Vec<u64> = r.host.seats.iter().map(|s| s.hole_repairs).collect();
    let worst: Vec<u64> = r.seats.iter().map(|s| s.status.repair_ticks_max).collect();
    println!("{label}: hole repairs {holes:?} worst repair of any loss, ticks {worst:?}");
    println!(
        "{label}: frames {} delay {} (raises {}, lowers {}) upload {} bit/s {} dgram/s missing {missing:?} \
         single-loss (count, worst repair ticks) {single:?} desyncs {} in_step {}",
        r.frames_built,
        r.host.delay_ticks,
        r.host.delay_raises,
        r.host.delay_lowers,
        r.host_upload_bps,
        r.host_upload_datagrams_per_s,
        r.verdicts.len(),
        r.in_step
    );
    println!(
        "{label}: upload seconds {} median {} p95 {} bit/s, over 4.0 Mbit/s {}, stream frames {}",
        r.host_upload_seconds,
        r.host_upload_p50_bps,
        r.host_upload_p95_bps,
        r.host_upload_seconds_over,
        r.seats.iter().map(|s| s.stream_frames).sum::<u64>()
    );
    let link = r.host_link;
    let per_packet = |bytes: u64| bytes as f64 / link.packets.max(1) as f64;
    println!(
        "{label}: host transport {} packets, body {:.1} B per packet, of which ack frame {:.2} B (ADR-8423)",
        link.packets,
        per_packet(link.body_bytes),
        per_packet(link.ack_frame_bytes)
    );
    println!(
        "{label}: delay over the run: mean {}.{:03} ticks, largest {}",
        r.delay_mean_milliticks / 1000,
        r.delay_mean_milliticks % 1000,
        r.delay_max_ticks
    );
    println!(
        "{label}: busiest second {} bit/s, checkpoints agreed {}, transport refused {}, lagging {:?}, \
         host refused {} in the match and {} before its start {:?}",
        r.host_upload_peak_bps,
        r.host.checkpoints_agreed,
        r.transport_refused,
        r.lagging,
        r.host_refused,
        r.host_refused_before_start,
        r.host_refused_codes
    );
}

/// What every run without an injected fault has to show, whatever else it measures. An upload
/// figure or a stall figure means nothing if the frames went out without the seats' inputs, if
/// a seat fell behind, or if no checkpoint was ever tallied.
fn assert_healthy(label: &str, s: &LockstepScenario, r: &LockstepRecord) {
    assert!(
        r.in_step,
        "{label}: every simulation agrees at the last common tick"
    );
    assert!(r.verdicts.is_empty(), "{label}: {:?}", r.verdicts);
    assert!(r.probes.is_empty(), "{label}: {:?}", r.probes);
    // No honest seat is flagged for holding its inputs back, whatever its link does (the host is
    // fed the transport's round trip, so the acknowledgement path is judged too).
    assert!(
        r.host.late_stamp_suspects.is_empty() && r.host.seats.iter().all(|row| row.late_stamp_flags == 0),
        "{label}: honest seats flagged for late stamping: {:?}",
        r.host
            .seats
            .iter()
            .filter(|row| row.late_stamp_flags > 0)
            .map(|row| (row.seat, row.input_hold_p10_us, row.input_margin_p50_us))
            .collect::<Vec<_>>()
    );
    // The host's own simulation stepped every tick in order (a relay host has none).
    assert_eq!(
        r.host_in_order,
        (s.host != SoakHost::Relay).then_some(true),
        "{label}: the host's simulation stepped in order"
    );
    assert!(
        !r.unresolved && r.removed.is_empty() && r.lagging.is_empty(),
        "{label}: unresolved {}, removed {:?}, lagging {:?}",
        r.unresolved,
        r.removed,
        r.lagging
    );
    assert_eq!(
        (
            r.host_refused,
            r.host_inputs_refused,
            r.transport_refused,
            r.transfers_skipped
        ),
        (0, 0, 0, 0),
        "{label}: payloads the host refused in the match, host inputs refused, sends the transport refused, \
         transfers skipped ({:?})",
        r.host_refused_codes
    );
    // Before the start the only refusal is "not started", and the seats send those inputs again.
    assert!(
        r.host_refused_codes
            .keys()
            .all(|code| code == "lockstep_not_started"),
        "{label}: {:?}",
        r.host_refused_codes
    );
    // One checkpoint every `hash_every` ticks, less the few whose deadline is still open at the end.
    let every = u64::from(s.config.hash_every.max(1));
    assert!(
        r.host.checkpoints_agreed + 5 >= r.frames_built / every,
        "{label}: {} checkpoints agreed over {} frames",
        r.host.checkpoints_agreed,
        r.frames_built
    );
    for seat in &r.seats {
        assert!(
            seat.in_order && seat.ended.is_none(),
            "{label}: seat {} in order {}, ended {:?}",
            seat.seat,
            seat.in_order,
            seat.ended
        );
        assert_eq!(
            (seat.refused, seat.inputs_refused),
            (0, 0),
            "{label}: seat {} refused host payloads or its own inputs",
            seat.seat
        );
        // Every seat hashed its subsystems after each step (the desync probe's 240-tick ring).
        assert_eq!(
            seat.subsystem_rows,
            regia_net::desync::ROW_RING_TICKS,
            "{label}: seat {} subsystem rows",
            seat.seat
        );
        assert!(
            seat.frames_stepped + 60 >= r.frames_built,
            "{label}: seat {} kept up: {} of {}",
            seat.seat,
            seat.frames_stepped,
            r.frames_built
        );
    }
    // The frames carried the seats' inputs: under 2 % of a seat's ticks went without one.
    for row in &r.host.seats {
        assert!(
            row.inputs_missing * 50 <= r.frames_built,
            "{label}: seat {}: {} of {} ticks without input",
            row.seat,
            row.inputs_missing,
            r.frames_built
        );
        assert!(!row.lagging, "{label}: seat {} is lagging", row.seat);
    }
}

/// Acceptance: 4 seats on bad-wifi for 5 minutes. One lost frames datagram never stalls a seat
/// over 2 ticks (a reliable frame channel waits a whole resend timer), every seat steps every
/// tick in order, and no checkpoint disagrees.
#[test]
fn four_seats_on_bad_wifi_for_five_minutes_never_stall_over_two_ticks_from_one_lost_datagram() {
    let s = LockstepScenario::new(named("bad-wifi"), 4, DEFAULT_SEED, 300_000);
    let r = run(&s);
    summary("bad-wifi x4, 5 min", &r);
    assert_healthy("bad-wifi x4, 5 min", &s, &r);
    assert!(r.in_step, "every simulation agrees at the last common tick");
    assert!(r.verdicts.is_empty(), "{:?}", r.verdicts);
    assert!(!r.unresolved && r.removed.is_empty());
    assert!(
        r.frames_built >= 17_500,
        "{} frames in five minutes at 60 Hz",
        r.frames_built
    );
    assert_eq!(r.host_refused, 0);
    for seat in &r.seats {
        assert!(seat.in_order, "seat {} stepped out of order", seat.seat);
        assert!(seat.ended.is_none(), "seat {} ended: {:?}", seat.seat, seat.ended);
        assert_eq!(seat.refused, 0);
        assert!(
            seat.status.single_losses >= 100,
            "the link really lost single datagrams: {}",
            seat.status.single_losses
        );
        assert!(
            seat.status.single_loss_repair_ticks_max <= 2,
            "seat {}: a single lost datagram stalled {} ticks",
            seat.seat,
            seat.status.single_loss_repair_ticks_max
        );
        assert_eq!(
            seat.stream_frames, 0,
            "redundancy repaired every loss; nothing waited for a resend"
        );
        assert!(
            seat.frames_stepped + 60 >= r.frames_built,
            "seat {} kept up: {} of {}",
            seat.seat,
            seat.frames_stepped,
            r.frames_built
        );
    }
    assert!(r.host.delay_ticks <= s.config.max_delay_ticks);
    assert!(r.host.checkpoints_agreed >= 580, "{}", r.host.checkpoints_agreed);
    // The delay budget holds: under 2 % of a seat's ticks go without its input.
    for row in &r.host.seats {
        assert!(
            row.inputs_missing * 50 <= r.frames_built,
            "seat {}: {} of {} ticks without input",
            row.seat,
            row.inputs_missing,
            r.frames_built
        );
    }
}

/// Acceptance (spec 120 § 7.5, bar `lockstep_upload`): 40 seats on the lockstep mix, host upload
/// at most 4 Mbit/s, IP and UDP headers included.
#[test]
fn lockstep_mix_forty_seats_host_upload_is_at_most_four_megabits() {
    let mut s = LockstepScenario::new(named("lockstep-mix"), 40, DEFAULT_SEED, 60_000);
    s.mix = InputMix::ORDERS;
    let r = run(&s);
    summary("lockstep-mix x40, orders mix", &r);
    assert_healthy("lockstep-mix x40, orders mix", &s, &r);
    assert!(r.in_step);
    assert!(r.verdicts.is_empty(), "{:?}", r.verdicts);
    assert_eq!(r.seats.len(), 40);
    assert!(r.seats.iter().all(|s| s.in_order && s.ended.is_none()));
    assert!(
        r.host_upload_bps <= 4_000_000,
        "host upload {} bit/s at 40 seats",
        r.host_upload_bps
    );
    assert!(
        (2_300..=2_600).contains(&r.host_upload_datagrams_per_s),
        "one frames datagram per seat per tick: {}",
        r.host_upload_datagrams_per_s
    );
    // No seat needed the stream or stalled over two ticks on a single loss.
    for seat in &r.seats {
        assert_eq!(seat.stream_frames, 0, "seat {}", seat.seat);
        assert!(
            seat.status.single_loss_repair_ticks_max <= 2,
            "seat {}",
            seat.seat
        );
    }

    // For the record: the same run with every unacknowledged frame in every datagram (spec 120
    // § 7.2 as written) costs more the longer the round trips are.
    let mut all = s.clone();
    all.config.redundancy = regia_net::lockstep::FrameRedundancy::AllUnacked;
    let a = run(&all);
    summary("lockstep-mix x40, orders mix, every unacked frame", &a);
    assert_healthy("lockstep-mix x40, every unacked frame", &all, &a);
    assert!(a.in_step && a.verdicts.is_empty());
    assert!(a.host_upload_bps > r.host_upload_bps);
}

/// Acceptance (spec 120 § 7.6, ADR-8211): `lockstep-mix`, 8 seats for 60 s with every simulation
/// hashing its three subsystems after each step: zero desyncs, zero probes, every checkpoint
/// agreed and every seat's ring full. A minute, not a shorter slice: the shared health bar counts
/// the ticks a seat spent without an input while the delay was still rising, and that startup is
/// under 2 % of a minute on this profile.
#[test]
fn lockstep_mix_desync_zero() {
    let s = LockstepScenario::new(named("lockstep-mix"), 8, DEFAULT_SEED + 7, 60_000);
    let r = run(&s);
    summary("lockstep-mix x8 desync_zero", &r);
    assert_healthy("lockstep-mix x8 desync_zero", &s, &r);
    assert_eq!(
        (
            r.host.desyncs,
            r.host.resyncs,
            r.host.desync_reports,
            r.host.last_desync.clone()
        ),
        (0, 0, 0, None)
    );
}

/// Acceptance (spec 120 § 7.5 as amended by ADR-8209, bar `lockstep_upload`): 40 seats on the
/// lockstep mix for 5 minutes. A player feels the worst seconds, not the mean: the host's upload
/// is at most 4.0 Mbit/s in the 95th-percentile second and at most 4.4 Mbit/s in the busiest
/// second, IP and UDP headers included.
#[test]
fn lockstep_mix_forty_seats_for_five_minutes_holds_the_upload_bar_in_its_worst_seconds() {
    let mut s = LockstepScenario::new(
        named("lockstep-mix"),
        UPLOAD_BAR_SEATS_ORDERS,
        DEFAULT_SEED,
        UPLOAD_BAR_RUN_MS,
    );
    s.mix = InputMix::ORDERS;
    let r = run(&s);
    summary("lockstep-mix x40, orders mix, 5 min", &r);
    assert_healthy("lockstep-mix x40, orders mix, 5 min", &s, &r);
    assert_eq!(r.seats.len(), 40);
    assert!(
        r.host_upload_seconds >= 290,
        "{} whole seconds judged",
        r.host_upload_seconds
    );
    assert!(
        r.host_upload_p95_bps <= UPLOAD_P95_LIMIT_BPS,
        "95th-percentile second {} bit/s at 40 seats ({} of {} seconds over {UPLOAD_P95_LIMIT_BPS})",
        r.host_upload_p95_bps,
        r.host_upload_seconds_over,
        r.host_upload_seconds
    );
    assert!(
        r.host_upload_peak_bps <= UPLOAD_PEAK_LIMIT_BPS,
        "busiest second {} bit/s at 40 seats",
        r.host_upload_peak_bps
    );
    assert!(
        r.host_upload_bps <= UPLOAD_P95_LIMIT_BPS,
        "the mean {}",
        r.host_upload_bps
    );
    for seat in &r.seats {
        assert_eq!(seat.stream_frames, 0, "seat {}", seat.seat);
        assert!(
            seat.status.single_loss_repair_ticks_max <= 2,
            "seat {}: a single lost datagram stalled {} ticks",
            seat.seat,
            seat.status.single_loss_repair_ticks_max
        );
    }
}

/// The second soak mix (ADR-8209): an action match, where every seat moves its aim on half of
/// its ticks, for 5 minutes at 16, 24 and 40 seats. The bar holds through
/// `UPLOAD_BAR_SEATS_ACTION` seats (spec 120 § 7.5 states the seat count per mix); 40 seats are
/// over it, and the run says by how much.
#[test]
fn the_action_mix_holds_the_upload_bar_through_twenty_four_seats_and_not_at_forty() {
    for seats in [16usize, 24, 40] {
        let mut s = LockstepScenario::new(named("lockstep-mix"), seats, DEFAULT_SEED, UPLOAD_BAR_RUN_MS);
        s.mix = InputMix::ACTION;
        let r = run(&s);
        let label = format!("lockstep-mix x{seats}, action mix, 5 min");
        summary(&label, &r);
        assert_healthy(&label, &s, &r);
        let holds =
            r.host_upload_p95_bps <= UPLOAD_P95_LIMIT_BPS && r.host_upload_peak_bps <= UPLOAD_PEAK_LIMIT_BPS;
        assert_eq!(
            holds,
            seats <= UPLOAD_BAR_SEATS_ACTION,
            "{label}: 95th-percentile second {} bit/s, busiest second {} bit/s",
            r.host_upload_p95_bps,
            r.host_upload_peak_bps
        );
        for seat in &r.seats {
            // Where the bar is stated nothing needs the stream. At 40 seats a frame is large
            // enough that the answer to a hole report (every unacknowledged frame) can pass the
            // datagram budget: those frames go on the stream, and the summary line counts them.
            if seats <= UPLOAD_BAR_SEATS_ACTION {
                assert_eq!(seat.stream_frames, 0, "{label}: seat {}", seat.seat);
            }
            assert!(
                seat.status.single_loss_repair_ticks_max <= 2,
                "{label}: seat {}: a single lost datagram stalled {} ticks",
                seat.seat,
                seat.status.single_loss_repair_ticks_max
            );
        }
    }
}

/// The same mix at 8 and 16 seats, and a listen host: the upload grows with the seats and every
/// run stays in step.
#[test]
fn lockstep_mix_at_eight_and_sixteen_seats_stays_in_step() {
    let mut last = 0;
    for (seats, host) in [(8usize, SoakHost::Listen), (16, SoakHost::Observer)] {
        let mut s = LockstepScenario::new(named("lockstep-mix"), seats, DEFAULT_SEED + 1, 40_000);
        s.host = host;
        let r = run(&s);
        summary(&format!("lockstep-mix x{seats} {host:?}"), &r);
        assert_healthy(&format!("lockstep-mix x{seats} {host:?}"), &s, &r);
        assert!(r.in_step);
        assert!(r.verdicts.is_empty(), "{:?}", r.verdicts);
        assert!(r.seats.iter().all(|s| s.in_order && s.ended.is_none()));
        assert!(r.host_upload_bps > last, "more seats, more upload");
        last = r.host_upload_bps;
    }
}

/// ADR-8422 (integ-v06p-net "For the lead" 1): every `mobile-4g` seat's NAT rebinds once in the
/// run. The host moves it exactly once, refuses none of its records from the new port, and copies
/// the seat's frames to the new port while the move validates, so the seat keeps stepping and its
/// missed inputs stay under the bar. Base (ADR-8299 alone): the trial budget refused the records
/// that answered the probe (39 for seat 2), the seat stopped stepping for about a second, and its
/// missed inputs went from 20 to 69. Seats whose address never changes are never copied to.
#[test]
fn a_nat_rebinding_moves_a_lockstep_seat_once_and_refuses_none_of_its_records() {
    let mut s = LockstepScenario::new(named("lockstep-mix"), 8, DEFAULT_SEED + 1, 40_000);
    s.host = SoakHost::Listen;
    let r = run(&s);
    summary("lockstep-mix x8 rebinding", &r);
    assert_healthy("lockstep-mix x8 rebinding", &s, &r);
    let rows: Vec<_> = r
        .seats
        .iter()
        .map(|seat| {
            (
                seat.seat,
                seat.profile.clone(),
                seat.nat_rebindings,
                seat.rebinds,
                seat.rebinds_refused,
                seat.path_mirrored,
            )
        })
        .collect();
    println!("(seat, profile, rebindings, moves, refused, mirrored): {rows:?}");
    let moved: Vec<_> = r.seats.iter().filter(|seat| seat.nat_rebindings > 0).collect();
    assert!(moved.len() >= 2, "the mix has mobile seats that rebind: {rows:?}");
    for seat in &r.seats {
        assert_eq!(seat.rebinds, seat.nat_rebindings, "seat {}: {rows:?}", seat.seat);
        assert_eq!(seat.rebinds_refused, 0, "seat {}: {rows:?}", seat.seat);
        assert_eq!(
            seat.path_mirrored > 0,
            seat.nat_rebindings > 0,
            "seat {}: copies only to a seat that moved: {rows:?}",
            seat.seat
        );
    }
}

/// The upload envelope by input mix, printed for the report (`--nocapture`): an action match
/// where half of the seats move their aim on any tick costs more than the order-driven one. The
/// 4 Mbit/s bar is judged on the order-driven mix; this is the number for the other.
#[test]
fn the_upload_grows_with_how_often_inputs_change() {
    for seats in [16usize, 40] {
        let mut orders = LockstepScenario::new(named("lockstep-mix"), seats, DEFAULT_SEED + 2, 30_000);
        orders.mix = InputMix::ORDERS;
        let mut action = orders.clone();
        action.mix = InputMix::ACTION;
        let (o, a) = (run(&orders), run(&action));
        summary(&format!("lockstep-mix x{seats} orders"), &o);
        summary(&format!("lockstep-mix x{seats} action"), &a);
        assert_healthy(&format!("lockstep-mix x{seats} orders"), &orders, &o);
        assert_healthy(&format!("lockstep-mix x{seats} action"), &action, &a);
        assert!(o.in_step && a.in_step);
        assert!(o.verdicts.is_empty() && a.verdicts.is_empty());
        assert!(a.host_upload_bps > o.host_upload_bps);
    }
}

/// Acceptance (NL-09): the wander-fire bot mix is a pure function of its seed. The same seed
/// repeats the same frames; a different seed does not.
#[test]
fn wander_fire_mix_is_seed_stable() {
    let frames = |seed: u64| -> Vec<LockInput> {
        let mut src = InputSource::new(seed, 3, InputMix::WANDER_FIRE);
        (0..500).map(|_| src.next_input()).collect()
    };
    let same = frames(0xB07);
    assert_eq!(same, frames(0xB07));
    assert_ne!(same, frames(0xB08));
    assert!(same.iter().all(|f| f.throttle == 127), "the bot keeps moving");
    assert!(
        same.iter().any(|f| f.buttons & 1 != 0) && same.iter().any(|f| f.buttons & 1 == 0),
        "fire pulses, it does not stay down"
    );
}

/// KI-TD-010 (ADR-8855): the gate-sized variant of the 40-seat wander-fire run below, run by every
/// `cargo test`: the same profile, seed, seat count, input mix and checks over one virtual minute
/// (3,600 ticks) instead of ten. The ten-minute run stays the evidence run.
#[test]
fn lockstep_mix_40_seats_desync_zero_1_min() {
    // The same 5 s of start slack as the ten-minute run (the host starts after the seats connect).
    let mut s = LockstepScenario::new(named("lockstep-mix+bots"), 40, DEFAULT_SEED, 65_000);
    s.mix = InputMix::WANDER_FIRE;
    let r = run(&s);
    summary("lockstep-mix+bots x40, wander-fire, 1 min", &r);
    assert_wander_fire_40(&r, 3_600);
}

/// The checks of the 40-seat wander-fire runs: 40 seats, `frames` frames or more, no verdict, in
/// step, no desync or resync, every seat in order and still in, and the upload under its bars.
fn assert_wander_fire_40(r: &LockstepRecord, frames: u64) {
    assert_eq!(r.seats.len(), 40);
    assert!(
        r.frames_built >= frames,
        "{frames} frames at 60 Hz: {} built",
        r.frames_built
    );
    assert!(r.verdicts.is_empty(), "{:?}", r.verdicts);
    assert!(r.in_step, "the simulations agree");
    assert_eq!((r.host.desyncs, r.host.resyncs), (0, 0));
    assert!(r.seats.iter().all(|seat| seat.in_order && seat.ended.is_none()));
    assert!(
        r.host_upload_p95_bps <= UPLOAD_P95_LIMIT_BPS,
        "p95 {} over {UPLOAD_P95_LIMIT_BPS}",
        r.host_upload_p95_bps
    );
    assert!(
        r.host_upload_peak_bps <= UPLOAD_PEAK_LIMIT_BPS,
        "peak {} over {UPLOAD_PEAK_LIMIT_BPS}",
        r.host_upload_peak_bps
    );
}

/// Acceptance (NL-09, the plan's name): `lockstep-mix+bots`, 40 seats, 10 virtual minutes,
/// 36,000 ticks at 60 Hz, desync 0, inside the upload bar. Ignored by a normal `cargo test`
/// because it is longer than the crate's other runs; the lane runs it with `--ignored --exact`.
#[test]
#[ignore = "10 virtual minutes, 40 seats; run with --ignored --exact"]
fn lockstep_mix_40_seats_desync_zero_10_min() {
    // A 600_000 ms clock built 35,979 frames on the test machine (2026-11-18): the host
    // starts after the seats connect, about 350 ms late, so tick 36,000 is not due yet.
    // Five extra virtual seconds covers that. The gate row stays at 600_000 ms.
    let mut s = LockstepScenario::new(named("lockstep-mix+bots"), 40, DEFAULT_SEED, 605_000);
    s.mix = InputMix::WANDER_FIRE;
    let r = run(&s);
    summary("lockstep-mix+bots x40, wander-fire, 10 min", &r);
    assert_wander_fire_40(&r, 36_000);
}

#[cfg(feature = "desync-inject")]
mod forced_divergence {
    use regia_net::lockstep::{SeatId, VerdictKind, Voter};

    use super::*;

    /// A seat's simulation is perturbed at tick 437 on an impaired link, for 2, 3 and 4 seats:
    /// the detector names that seat, the checkpoint and the first diverged tick, the seat is
    /// resynced from the majority, and the match goes on in step.
    #[test]
    fn a_perturbed_seat_on_the_impaired_link_is_named_with_its_tick_and_resynced() {
        for seats in [2usize, 3, 4] {
            let mut s = LockstepScenario::new(named("bad-wifi"), seats, DEFAULT_SEED + 3, 30_000);
            s.config.self_check = false;
            s.drift = Some((1, 437));
            let r = run(&s);
            summary(&format!("forced divergence x{seats}"), &r);
            assert_eq!(r.verdicts.len(), 1, "{:?}", r.verdicts);
            let v = &r.verdicts[0];
            // The observer host is the third voter of a two-seat match: a strict majority exists.
            assert_eq!(v.kind, VerdictKind::Diverged);
            assert_eq!(v.diverged, [Voter::Seat(SeatId(1))], "the right seat");
            assert_eq!((v.tick, v.last_agreed), (450, Some(420)), "the right checkpoint");
            assert!(!v.host_diverged);
            assert_eq!(r.located, [(450, "seat1".to_owned(), 437)], "the right tick");
            assert_eq!(r.probes.len(), 1, "{:?}", r.probes);
            let p = &r.probes[0];
            assert_eq!(
                (
                    p.voter,
                    p.first_diverged_tick,
                    p.last_agreed_tick,
                    p.subsystem.as_deref()
                ),
                (Voter::Seat(SeatId(1)), Some(437), Some(436), Some("motion")),
                "the probe names the subsystem: {p:?}"
            );
            assert!(r.removed.is_empty() && !r.unresolved);
            let resynced: Vec<u16> = r.seats.iter().filter(|s| s.resyncs > 0).map(|s| s.seat).collect();
            assert_eq!(resynced, [1], "only that seat");
            assert_eq!(r.seats.iter().map(|s| s.resyncs).sum::<u32>(), 1, "one transfer");
            assert_eq!(
                (r.host_resyncs, r.transfers_skipped),
                (0, 0),
                "the host took no state and no transfer was skipped"
            );
            assert!(r.in_step, "back in step after the transfer");
        }
    }

    /// The host's own simulation is perturbed: it is outvoted and resynced from a seat; no seat is
    /// resynced or removed.
    #[test]
    fn a_perturbed_host_on_the_impaired_link_is_outvoted_and_resynced() {
        let mut s = LockstepScenario::new(named("bad-wifi"), 3, DEFAULT_SEED + 4, 30_000);
        s.config.self_check = false;
        s.drift = Some((u16::MAX, 437));
        let r = run(&s);
        summary("forced host divergence x3", &r);
        assert_eq!(r.verdicts.len(), 1, "{:?}", r.verdicts);
        assert!(r.verdicts[0].host_diverged);
        assert_eq!(r.verdicts[0].diverged, [Voter::Observer]);
        assert_eq!(r.located, [(450, "observer".to_owned(), 437)]);
        assert_eq!(r.probes.len(), 1, "{:?}", r.probes);
        assert_eq!(
            (
                r.probes[0].voter,
                r.probes[0].subsystem.as_deref(),
                r.probes[0].first_diverged_tick
            ),
            (Voter::Observer, Some("motion"), Some(437))
        );
        assert_eq!((r.host_resyncs, r.transfers_skipped), (1, 0));
        assert_eq!(r.seats.iter().map(|s| s.resyncs).sum::<u32>(), 0);
        assert!(r.removed.is_empty() && r.in_step);
    }

    /// With the self-check on, the perturbed seat replays from the last agreed checkpoint and
    /// repairs itself: no state transfer.
    #[test]
    fn a_perturbed_seat_repairs_itself_by_replay_when_the_self_check_is_on() {
        let mut s = LockstepScenario::new(named("good-wifi"), 3, DEFAULT_SEED + 5, 20_000);
        s.drift = Some((2, 437));
        let r = run(&s);
        assert_eq!(r.verdicts.len(), 1, "{:?}", r.verdicts);
        assert_eq!(r.verdicts[0].kind, VerdictKind::SelfHealed);
        assert_eq!(r.verdicts[0].self_healed, [Voter::Seat(SeatId(2))]);
        let replayed: Vec<u16> = r
            .seats
            .iter()
            .filter(|s| s.self_checks > 0)
            .map(|s| s.seat)
            .collect();
        assert_eq!(replayed, [2], "only the perturbed seat replayed");
        assert_eq!(r.seats.iter().map(|s| s.self_checks).sum::<u32>(), 1, "once");
        assert!(
            r.seats.iter().all(|s| s.resyncs == 0) && r.host_resyncs == 0,
            "no state transfer"
        );
        assert!(r.in_step);
    }
}
