// SPDX-License-Identifier: MPL-2.0
//! Session tests (spec 120 § 7.4, ADR-8212): DO27's lockstep session scenarios on the virtual-time
//! rig: silent seats, agreed leave and match end, the timeout policy and its start grace, late join
//! and reconnect through a catch-up. Seeded, virtual time, no wall clock.

use super::*;

/// The rig's presses: a capture whose tick is a multiple of 5 carries a press id (its capture
/// tick) in the blob; buttons mirror it.
fn pressing_input(seat: SeatId, capture: u64) -> Pad {
    let press = capture.is_multiple_of(5);
    Pad {
        move_x: (capture % 7) as i8,
        move_y: seat.0 as i8,
        aim: capture as u32,
        buttons: u16::from(press),
        blob: if press {
            (capture as u32).to_le_bytes().to_vec()
        } else {
            Vec::new()
        },
    }
}

/// Presses cleared, analog kept; a late input's presses are kept, the newer analog wins.
fn pad_rules() -> InputRules<Pad> {
    InputRules {
        clear_presses: |p| Pad {
            buttons: 0,
            blob: Vec::new(),
            ..p.clone()
        },
        merge_late: |late, next| {
            next.buttons |= late.buttons;
            let mut blob = late.blob.clone();
            blob.extend_from_slice(&next.blob);
            next.blob = blob;
        },
    }
}

fn observer_rig(config: LockstepConfig, seats: &[u16], link: LinkSpec) -> Rig {
    Rig::new(config, HostRole::Observer, seats, link)
}

/// Every frame the host built was built in the first millisecond it was due: nothing held it.
fn assert_frames_on_time(rig: &Rig) {
    let tick_us = rig.config.tick_us();
    let mut expected = 0;
    for (ms, tick) in &rig.build_times {
        assert_eq!(*tick, expected, "frames built in order, none skipped");
        expected += 1;
        let due_ms = (tick * tick_us).div_ceil(1000);
        assert_eq!(*ms, due_ms, "frame {tick} built at {ms} ms, due at {due_ms} ms");
    }
    if rig.host.ended_at().is_none() {
        let last_due = rig.now.saturating_sub(1) * 1000 / tick_us;
        assert!(
            expected > last_due,
            "the host built every due frame ({expected} of {})",
            last_due + 1
        );
    }
}

/// The rig's host sends every 16 ms and a tick is 16.7 ms, so two frames sometimes share a
/// datagram: a seat that is never held waits at most two ticks between frames.
const UNHELD_STEP_GAP_MS: u64 = 34;

/// The longest wait between two frames a seat stepped from `from_ms` on, in milliseconds.
fn longest_step_gap(node: &SeatNode, from_ms: u64) -> u64 {
    let times: Vec<u64> = node
        .step_times
        .iter()
        .filter(|(ms, _)| *ms >= from_ms)
        .map(|(ms, _)| *ms)
        .collect();
    times.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(u64::MAX)
}

fn removals(rig: &Rig) -> Vec<(u64, SeatId, &'static str)> {
    rig.host_timeline
        .iter()
        .filter_map(|(ms, e)| match e {
            HostEvent::SeatRemoved { seat, reason, .. } => Some((*ms, *seat, *reason)),
            _ => None,
        })
        .collect()
}

/// The frames' entry for `seat` at every tick it is listed.
fn entries_of(frames: &[LockstepFrame<Pad>], seat: SeatId) -> Vec<(u64, SeatInput<Pad>)> {
    frames
        .iter()
        .filter_map(|f| {
            f.inputs
                .iter()
                .find(|(s, _)| *s == seat)
                .map(|(_, input)| (f.tick, input.clone()))
        })
        .collect()
}

/// The host side of a resume: the token names the seat, a fresh seat process takes the host's
/// state at a tick 300 frames back (a recent checkpoint) plus every frame since. Returns the
/// frames sent.
fn reconnect(rig: &mut Rig, seat: SeatId, token: u64) -> u64 {
    let id = rig
        .host
        .resume(token, rig.now)
        .expect("the token names a seat that may return");
    assert_eq!(id, seat, "the resume token keeps the seat id");
    let sim = rig.host_sim.as_ref().expect("an observer host");
    let newest = *sim.stepped.last().expect("the host stepped");
    let state_tick = newest.saturating_sub(300);
    let state = sim.hashes[&state_tick];
    let config = rig.config.clone();
    let node = rig.seats.get_mut(&seat).expect("the seat's node");
    node.seat = LockstepSeat::catching_up(config, seat);
    node.sim = Sim::default();
    node.off = None;
    node.held_stream.clear();
    rig.host
        .catch_up(seat, state_tick, &state.to_le_bytes(), rig.now)
        .expect("the frame log holds every frame since the state")
}

#[test]
fn a_silent_seat_never_blocks_the_frame_and_holds_its_last_input() {
    let config = LockstepConfig {
        missing_input: MissingInputPolicy::Hold,
        ..config()
    };
    let mut rig = observer_rig(config, &[0, 1, 2], LinkSpec::latency(10));
    rig.run_ms(2_000);
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").off = Some((2_000, 5_000, true));
    rig.run_ms(9_000);
    assert_frames_on_time(&rig);
    for id in [0, 1] {
        let gap = longest_step_gap(&rig.seats[&SeatId(id)], 1_000);
        assert!(
            gap <= UNHELD_STEP_GAP_MS,
            "seat {id} waited {gap} ms between two frames while seat 2 was silent"
        );
    }
    // The silent seat is listed with the input it last sent, the same on every participant.
    let entries = entries_of(&rig.frames, SeatId(2));
    let held: Vec<&(u64, SeatInput<Pad>)> = entries
        .iter()
        .filter(|(_, i)| matches!(i, SeatInput::Held(_)))
        .collect();
    assert!(
        held.len() >= 150,
        "only {} held ticks during a 3 s silence",
        held.len()
    );
    let first_held = held[0].0;
    let last_sent = entries
        .iter()
        .rev()
        .find(|(t, i)| *t < first_held && i.is_present())
        .and_then(|(_, i)| i.as_ref().cloned())
        .expect("an input before the silence");
    // Ticks built during the silence (it ended at 5 s, tick 300).
    let during: Vec<_> = held.iter().filter(|(t, _)| *t < 300).collect();
    assert!(during.len() >= 150);
    assert!(
        during.iter().all(|(_, i)| i.as_ref() == Some(&last_sent)),
        "Hold repeats the last input"
    );
    assert!(removals(&rig).is_empty(), "a 3 s silence removes nobody");
    let row = rig
        .host
        .status(rig.now)
        .seats
        .into_iter()
        .find(|s| s.seat == 2)
        .expect("row");
    assert_eq!(row.inputs_held, held.len() as u64);
    // Back after the silence: every participant stepped the same frames, held inputs included.
    rig.assert_in_step();
    let seat_frames: Vec<_> = rig.seats[&SeatId(0)]
        .seat
        .frames_since(0)
        .filter_map(|f| {
            f.inputs
                .iter()
                .find(|(s, _)| *s == SeatId(2))
                .map(|(_, i)| (f.tick, i.clone()))
        })
        .collect();
    for (tick, input) in seat_frames {
        let host_input = &rig.frames[tick as usize]
            .inputs
            .iter()
            .find(|(s, _)| *s == SeatId(2))
            .expect("listed")
            .1;
        assert_eq!(
            &input, host_input,
            "seat 0 derived the held input of tick {tick} like the host"
        );
    }
}

#[test]
fn repeat_analog_clears_the_presses_and_empty_lists_the_seat_absent() {
    for (policy, rules) in [
        (MissingInputPolicy::RepeatAnalog, true),
        (MissingInputPolicy::RepeatAnalog, false),
        (MissingInputPolicy::Empty, true),
    ] {
        let config = LockstepConfig {
            missing_input: policy,
            ..config()
        };
        let mut rig = observer_rig(config, &[0, 1], LinkSpec::latency(10));
        rig.input_of = |seat, capture| Pad {
            buttons: 1,
            blob: vec![1, 2],
            ..pressing_input(seat, capture)
        };
        if rules {
            rig.host.set_input_rules(pad_rules());
            for node in rig.seats.values_mut() {
                node.seat.set_input_rules(pad_rules());
            }
        }
        rig.run_ms(1_000);
        rig.seats.get_mut(&SeatId(1)).expect("seat 1").off = Some((1_000, 2_500, true));
        rig.run_ms(3_000);
        let missing: Vec<SeatInput<Pad>> = entries_of(&rig.frames, SeatId(1))
            .into_iter()
            .filter(|(t, i)| *t > 10 && !i.is_present())
            .map(|(_, i)| i)
            .collect();
        assert!(missing.len() > 60, "{policy:?}: the seat was missing");
        for input in &missing {
            match (policy, rules, input) {
                (MissingInputPolicy::RepeatAnalog, true, SeatInput::Held(p)) => {
                    assert_eq!((p.buttons, p.blob.len()), (0, 0), "presses cleared");
                    assert_ne!(p.aim, 0, "analog kept");
                }
                (MissingInputPolicy::RepeatAnalog, false, SeatInput::Absent) => {}
                (MissingInputPolicy::Empty, _, SeatInput::Absent) => {}
                other => panic!("{other:?}"),
            }
        }
        rig.assert_in_step();
    }
}

#[test]
fn late_inputs_merge_into_the_next_frame_and_no_press_is_lost_or_doubled() {
    let mut rig = observer_rig(config(), &[0, 1], LinkSpec::latency(8));
    rig.input_of = pressing_input;
    rig.host.set_input_rules(pad_rules());
    for node in rig.seats.values_mut() {
        node.seat.set_input_rules(pad_rules());
    }
    rig.run_ms(3_000);
    // Seat 1's uplink stalls: its inputs arrive after their frames were built.
    for (from, latency) in [(3_000, 400), (3_700, 8), (6_000, 250), (6_400, 8)] {
        rig.run_ms(from - rig.now);
        let node = rig.seats.get_mut(&SeatId(1)).expect("seat 1");
        node.up.spec = LinkSpec::latency(latency);
    }
    rig.run_ms(4_000);
    let status = rig.host.status(rig.now);
    let row = status.seats.iter().find(|s| s.seat == 1).expect("row");
    assert!(
        row.inputs_late > 20,
        "the stalls made inputs late ({})",
        row.inputs_late
    );
    assert!(
        row.inputs_late_merged > 20,
        "late inputs were merged ({})",
        row.inputs_late_merged
    );
    eprintln!(
        "late merge: {} inputs late, {} merged into a later frame",
        row.inputs_late, row.inputs_late_merged
    );
    // Every press seat 1 stamped onto a tick the host has built is in exactly one frame.
    let built = rig.host.tick();
    let sent: Vec<u32> = rig.seats[&SeatId(1)]
        .stamps
        .iter()
        .filter_map(|(capture, stamp)| match stamp {
            Stamp::Tick(t) if *t < built && capture.is_multiple_of(5) => Some(*capture as u32),
            _ => None,
        })
        .collect();
    let mut stepped: Vec<u32> = Vec::new();
    for (_, input) in entries_of(&rig.frames, SeatId(1)) {
        if let SeatInput::Present(p) = input {
            stepped.extend(p.blob.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)));
        }
    }
    let mut sorted = stepped.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), stepped.len(), "a press was stepped twice");
    let lost: Vec<u32> = sent.iter().copied().filter(|p| !sorted.contains(p)).collect();
    assert!(lost.is_empty(), "presses lost: {lost:?}");
    assert!(sent.len() > 100, "{} presses sent", sent.len());
    eprintln!(
        "late merge: {} presses sent, {} stepped, each once",
        sent.len(),
        stepped.len()
    );
    rig.assert_in_step();
}

#[test]
fn a_clean_leave_never_stalls_and_leave_and_match_end_land_on_the_same_tick_everywhere() {
    let mut rig = observer_rig(config(), &[0, 1, 2, 3], LinkSpec::latency(15));
    rig.run_ms(2_000);
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").seat.leave();
    rig.run_ms(2_000);
    rig.host.end_match();
    rig.run_ms(2_000);
    assert_frames_on_time(&rig);
    let (leave_ms, leave_seat, reason) = removals(&rig)[0];
    assert_eq!((leave_seat, reason), (SeatId(2), "left"));
    assert!(leave_ms < 2_100, "the leave was agreed at once ({leave_ms} ms)");
    let leave_tick = rig
        .frames
        .iter()
        .find(|f| {
            f.membership
                .iter()
                .any(|c| c.seat == SeatId(2) && c.kind == MembershipKind::Leave)
        })
        .expect("a frame lists the leave")
        .tick;
    let end = rig.host.ended_at().expect("the match ended");
    assert_eq!(
        rig.frames.last().map(|f| (f.tick, f.match_end)),
        Some((end, true)),
        "no frame after the end"
    );
    assert_eq!(rig.host.poll_frame(rig.now + 1_000), None);
    for (id, node) in &rig.seats {
        let left: Vec<u64> = node
            .events
            .iter()
            .filter_map(|e| match e {
                SeatEvent::Left { tick } => Some(*tick),
                _ => None,
            })
            .collect();
        let ended: Vec<u64> = node
            .events
            .iter()
            .filter_map(|e| match e {
                SeatEvent::MatchEnded { tick } => Some(*tick),
                _ => None,
            })
            .collect();
        if *id == SeatId(2) {
            assert_eq!(
                left,
                [leave_tick],
                "the leaving seat sees its leave on the agreed tick"
            );
            assert_eq!(node.seat.ended_at(), None, "it stops at its leave");
            continue;
        }
        assert!(left.is_empty());
        assert_eq!(ended, [end], "{id} saw the end on the agreed tick");
        assert_eq!(
            node.sim.stepped.last(),
            Some(&end),
            "{id} stepped exactly to the end"
        );
        assert!(node.seat.frames_since(0).all(|f| f.tick <= end));
        // The leave stalled no one: every remaining seat stepped on without a wait.
        let gap = longest_step_gap(node, 1_000);
        assert!(
            gap <= UNHELD_STEP_GAP_MS,
            "{id} waited {gap} ms between two frames around the leave"
        );
        let listed = node
            .seat
            .frames_since(leave_tick)
            .all(|f| f.inputs.iter().all(|(s, _)| *s != SeatId(2)));
        assert!(listed, "{id} lists seat 2 after its leave");
    }
    for (id, node) in rig.seats.iter_mut().filter(|(id, _)| **id != SeatId(2)) {
        assert_eq!(
            node.seat.submit_input(&Pad::default()),
            Err(LockstepError::MatchEnded),
            "{id} takes no input after the end"
        );
    }
    assert!(rig.desyncs().is_empty());
}

#[test]
fn a_killed_seat_keeps_the_match_ticking_and_is_removed_at_the_timeout() {
    let config = config();
    let timeout = config.timeout.timeout_ms;
    let mut rig = observer_rig(config, &[0, 1, 2, 3], LinkSpec::latency(15));
    rig.run_ms(3_000);
    // The process of seat 3 dies: nothing more from it, ever.
    rig.seats.remove(&SeatId(3));
    rig.run_ms(27_000);
    assert_frames_on_time(&rig);
    for (id, node) in &rig.seats {
        let gap = longest_step_gap(node, 1_000);
        assert!(
            gap <= UNHELD_STEP_GAP_MS,
            "{id} waited {gap} ms between two frames after seat 3 died"
        );
    }
    let removed = removals(&rig);
    assert_eq!(removed.len(), 1);
    let (ms, seat, reason) = removed[0];
    assert_eq!((seat, reason), (SeatId(3), "timeout"));
    let last_heard = rig.last_up_ms[&SeatId(3)];
    let silence = ms - last_heard;
    eprintln!("killed seat: removed after {silence} ms of silence (timeout {timeout} ms)");
    let tick_ms = rig.config.tick_us().div_ceil(1000);
    assert!(
        (timeout..=timeout + tick_ms).contains(&silence),
        "removed after {silence} ms of silence (timeout {timeout} ms, +- 1 tick)"
    );
    assert!(
        rig.host_timeline
            .iter()
            .any(|(_, e)| *e == HostEvent::SeatInterrupted { seat: SeatId(3) })
    );
    assert!(rig.host.members().all(|s| s != SeatId(3)));
    assert_eq!(rig.host.status(rig.now).seats_timed_out, 1);
    rig.assert_in_step();
    assert!(rig.desyncs().is_empty());
}

#[test]
fn stalls_of_3_and_8_seconds_keep_the_seat_and_12_seconds_past_a_10_second_timeout_ejects_it() {
    let config = LockstepConfig {
        timeout: TimeoutPolicy {
            timeout_ms: 10_000,
            ..TimeoutPolicy::default()
        },
        ..config()
    };
    let mut rig = observer_rig(config, &[0, 1, 2, 3], LinkSpec::latency(12));
    rig.run_ms(2_000);
    for (seat, ms) in [(1, 3_000), (2, 8_000), (3, 12_000)] {
        rig.seats.get_mut(&SeatId(seat)).expect("seat").off = Some((2_000, 2_000 + ms, true));
    }
    rig.run_ms(10_500);
    let removed = removals(&rig);
    assert_eq!(removed.len(), 1, "{removed:?}");
    let (ms, seat, reason) = removed[0];
    assert_eq!((seat, reason), (SeatId(3), "timeout"));
    let silence = ms - rig.last_up_ms[&SeatId(3)];
    eprintln!("12 s stall past a 10 s timeout: ejected after {silence} ms of silence");
    let tick_ms = rig.config.tick_us().div_ceil(1000);
    assert!(
        (10_000..=10_000 + tick_ms).contains(&silence),
        "ejected after {silence} ms"
    );
    rig.run_ms(2_500);
    assert_eq!(removals(&rig).len(), 1, "the 8 s stall ended inside the timeout");
    // The ejected seat is out of the match; the stalled ones stepped back to the edge.
    rig.seats.remove(&SeatId(3));
    rig.run_ms(6_000);
    assert_frames_on_time(&rig);
    rig.assert_in_step();
    assert!(rig.desyncs().is_empty(), "the stalls kept the checksums");
    let host_tick = rig.host.tick();
    for id in [1, 2] {
        let node = &rig.seats[&SeatId(id)];
        let stepped = node.sim.stepped.len() as u64;
        assert!(
            stepped + 20 >= host_tick,
            "seat {id} is {} ticks behind",
            host_tick - stepped
        );
        assert!(rig.host.members().any(|s| s == SeatId(id)));
        // Its inputs count again after the stall.
        let late = entries_of(&rig.frames, SeatId(id));
        assert!(late.iter().rev().take(60).filter(|(_, i)| i.is_present()).count() > 50);
    }
}

#[test]
fn a_slow_loader_inside_the_start_grace_is_kept_and_one_that_never_comes_is_removed_at_the_grace() {
    let config = LockstepConfig {
        timeout: TimeoutPolicy {
            timeout_ms: 10_000,
            ..TimeoutPolicy::default()
        },
        ..config()
    };
    let grace = config.timeout.start_grace_ms;
    let mut rig = observer_rig(config, &[0, 1, 2, 3], LinkSpec::latency(10));
    let token = rig.host.resume_token(SeatId(2)).expect("a token");
    // Seats 2 and 3 load: they send nothing. Seat 2 is done after 25 s, seat 3 never.
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").off = Some((0, 25_000, false));
    rig.seats.get_mut(&SeatId(3)).expect("seat 3").off = Some((0, u64::MAX, false));
    rig.run_ms(25_000);
    assert!(
        removals(&rig).is_empty(),
        "nobody removed inside the start grace: {:?}",
        removals(&rig)
    );
    // The loader has missed more frames than the log holds: it starts from a catch-up.
    let frames = reconnect(&mut rig, SeatId(2), token);
    assert!(frames < rig.config.frame_log_ticks as u64);
    rig.run_ms(6_000);
    let removed = removals(&rig);
    assert_eq!(removed.len(), 1, "{removed:?}");
    let (ms, seat, reason) = removed[0];
    assert_eq!((seat, reason), (SeatId(3), "timeout"));
    let tick_ms = rig.config.tick_us().div_ceil(1000);
    eprintln!("loader that never comes: removed at {ms} ms (grace {grace} ms)");
    assert!(
        (grace..=grace + tick_ms).contains(&ms),
        "removed at {ms} ms, the grace ends at {grace} ms"
    );
    rig.seats.remove(&SeatId(3));
    rig.assert_in_step();
    let loader = &rig.seats[&SeatId(2)];
    assert!(
        loader
            .events
            .iter()
            .any(|e| matches!(e, SeatEvent::CaughtUp { .. }))
    );
    assert!(
        entries_of(&rig.frames, SeatId(2))
            .iter()
            .rev()
            .take(60)
            .filter(|(_, i)| i.is_present())
            .count()
            > 50
    );
}

#[test]
fn a_seat_reconnects_after_15_seconds_and_is_in_step_within_5_seconds_with_the_same_seat_id() {
    let mut rig = observer_rig(config(), &[0, 1, 2], LinkSpec::BAD_WIFI);
    let token = rig.host.resume_token(SeatId(2)).expect("a token");
    rig.run_ms(3_000);
    // The connection is lost: nothing gets through for 15 s, then a fresh seat process returns.
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").off = Some((3_000, u64::MAX, false));
    rig.run_ms(15_000);
    assert!(removals(&rig).is_empty(), "15 s is inside the 20 s timeout");
    let back_ms = rig.now;
    reconnect(&mut rig, SeatId(2), token);
    rig.run_ms(8_000);
    let node = &rig.seats[&SeatId(2)];
    let caught_up = node
        .timeline
        .iter()
        .find_map(|(ms, e)| matches!(e, SeatEvent::CaughtUp { .. }).then_some(*ms))
        .expect("the seat caught up");
    eprintln!(
        "reconnect after 15 s on bad-wifi: in step {} ms after it came back, largest catch-up burst {} frames",
        caught_up - back_ms,
        node.catchup_burst_max
    );
    assert!(
        caught_up - back_ms <= 5_000,
        "in step {} ms after it came back",
        caught_up - back_ms
    );
    assert!(
        node.catchup_burst_max <= rig.config.catchup_steps_per_frame,
        "the per-frame budget held"
    );
    assert!(
        node.catchup_burst_max >= 2,
        "the catch-up ran faster than real time"
    );
    assert!(
        rig.host_timeline
            .iter()
            .any(|(_, e)| matches!(e, HostEvent::SeatCaughtUp { seat: SeatId(2), .. }))
    );
    // Same seat id all along: never removed, never joined again, and its inputs count again.
    assert!(removals(&rig).is_empty());
    assert!(
        rig.frames
            .iter()
            .all(|f| f.inputs.iter().any(|(s, _)| *s == SeatId(2)))
    );
    assert!(
        entries_of(&rig.frames, SeatId(2))
            .iter()
            .rev()
            .take(120)
            .filter(|(_, i)| i.is_present())
            .count()
            > 100
    );
    rig.assert_in_step();
    assert!(rig.desyncs().is_empty());
}

#[test]
fn a_seat_removed_past_the_timeout_returns_with_its_token_and_joins_on_an_agreed_tick() {
    let mut rig = observer_rig(config(), &[0, 1, 2], LinkSpec::latency(20));
    let token = rig.host.resume_token(SeatId(1)).expect("a token");
    rig.run_ms(2_000);
    rig.seats.get_mut(&SeatId(1)).expect("seat 1").off = Some((2_000, u64::MAX, false));
    rig.run_ms(23_000);
    assert_eq!(
        removals(&rig).first().map(|r| (r.1, r.2)),
        Some((SeatId(1), "timeout"))
    );
    assert_eq!(
        rig.host.resume(token ^ 1, rig.now),
        Err(LockstepError::ResumeRefused)
    );
    assert_eq!(LockstepError::ResumeRefused.code(), "lockstep_resume_refused");
    reconnect(&mut rig, SeatId(1), token);
    rig.run_ms(6_000);
    let joined: Vec<(SeatId, u64)> = rig
        .host_timeline
        .iter()
        .filter_map(|(_, e)| match e {
            HostEvent::SeatJoined { seat, tick } => Some((*seat, *tick)),
            _ => None,
        })
        .collect();
    assert_eq!(joined.len(), 1);
    let (seat, tick) = joined[0];
    assert_eq!(seat, SeatId(1), "the token kept the seat id");
    let frame = &rig.frames[tick as usize];
    assert!(frame.membership.contains(&MembershipChange {
        seat,
        kind: MembershipKind::Join
    }));
    for (id, node) in &rig.seats {
        let f = node.seat.frames_since(tick).next().expect("the join frame");
        assert_eq!(f.tick, tick);
        assert!(
            f.membership.contains(&MembershipChange {
                seat,
                kind: MembershipKind::Join
            }),
            "{id}"
        );
    }
    assert!(rig.seats[&seat].events.contains(&SeatEvent::Joined { tick }));
    assert!(
        entries_of(&rig.frames, seat)
            .iter()
            .rev()
            .take(60)
            .filter(|(_, i)| i.is_present())
            .count()
            > 50
    );
    rig.assert_in_step();
    // Past the resume window the token no longer works.
    let mut later = rig.host.resume(token, rig.now + rig.config.resume_window_ms * 2);
    assert_eq!(later, Ok(seat), "a member's token works while it is a member");
    rig.seats.get_mut(&seat).expect("seat").seat.leave();
    rig.run_ms(1_000);
    later = rig.host.resume(token, rig.now);
    assert_eq!(
        later,
        Err(LockstepError::ResumeRefused),
        "a seat that left cannot resume"
    );
}

#[test]
fn a_new_seat_joins_late_through_a_catch_up() {
    let mut rig = observer_rig(config(), &[0, 1], LinkSpec::BAD_WIFI);
    rig.run_ms(4_000);
    // A third player arrives mid-match.
    let config = rig.config.clone();
    rig.seats.insert(
        SeatId(7),
        SeatNode {
            seat: LockstepSeat::catching_up(config, SeatId(7)),
            sim: Sim::default(),
            down: Pipe::new(LinkSpec::BAD_WIFI, 0x1007),
            up: Pipe::new(LinkSpec::BAD_WIFI, 0x2007),
            stamps: Vec::new(),
            events: Vec::new(),
            off: None,
            held_stream: Vec::new(),
            hold_stream_until: 0,
            timeline: Vec::new(),
            step_times: Vec::new(),
            catchup_burst_max: 0,
            burst: 0,
            drift_at: None,
            drift_sticky: false,
            send_phase: 7,
            hold_inputs: None,
        },
    );
    let sim = rig.host_sim.as_ref().expect("observer");
    let (k, state) = (*sim.stepped.last().expect("stepped"), sim.state);
    let sent = rig
        .host
        .catch_up(SeatId(7), k, &state.to_le_bytes(), rig.now)
        .expect("catch-up");
    assert_eq!(sent, rig.host.tick() - k - 1);
    rig.run_ms(6_000);
    let join = rig
        .frames
        .iter()
        .find(|f| {
            f.membership.contains(&MembershipChange {
                seat: SeatId(7),
                kind: MembershipKind::Join,
            })
        })
        .expect("the join is agreed on a frame");
    assert!(join.tick > k);
    for node in rig.seats.values() {
        assert!(
            node.seat
                .frames_since(join.tick)
                .next()
                .is_some_and(|f| f.membership == join.membership)
        );
    }
    assert!(
        rig.frames
            .iter()
            .filter(|f| f.tick < join.tick)
            .all(|f| f.inputs.iter().all(|(s, _)| *s != SeatId(7)))
    );
    assert!(
        entries_of(&rig.frames, SeatId(7))
            .iter()
            .rev()
            .take(60)
            .filter(|(_, i)| i.is_present())
            .count()
            > 50
    );
    assert_eq!(rig.host.status(rig.now).catchups, 1);
    assert!(rig.seats[&SeatId(7)].seat.status().member);
    rig.assert_in_step();
    assert!(rig.desyncs().is_empty());
}

#[test]
fn a_gap_in_the_frame_log_refuses_the_catch_up() {
    let config = LockstepConfig {
        frame_log_ticks: 64,
        ..config()
    };
    let mut rig = observer_rig(config, &[0, 1], LinkSpec::latency(5));
    rig.run_ms(3_000);
    let next = rig.host.tick();
    let err = rig
        .host
        .catch_up(SeatId(1), 10, &[0; 8], rig.now)
        .expect_err("frames 11.. are gone");
    assert_eq!(err.code(), "lockstep_catchup_gap");
    assert!(
        matches!(err, LockstepError::CatchupGap { state_tick: 10, next_build, .. } if next_build == next)
    );
    let err = rig
        .host
        .catch_up(SeatId(1), next, &[0; 8], rig.now)
        .expect_err("not built yet");
    assert_eq!(err.code(), "lockstep_catchup_gap");
    // The oldest state the log can carry forward works.
    let oldest = rig.host.frames_since(0).next().expect("logged").tick;
    assert!(rig.host.catch_up(SeatId(1), oldest - 1, &[0; 8], rig.now).is_ok());
}

#[test]
fn the_catch_up_budget_bounds_the_frames_handed_out_per_application_frame() {
    let config = LockstepConfig {
        catchup_steps_per_frame: 3,
        ..config()
    };
    let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
    host.add_seat(SeatId(0));
    host.start(0);
    for now in 0..2_000 {
        while host.poll_frame(now).is_some() {}
    }
    let mut seat = LockstepSeat::<Game>::catching_up(config, SeatId(0));
    assert_eq!(seat.next_frame(), None, "nothing before the catch-up");
    host.catch_up(SeatId(0), 9, &[1, 2, 3], 2_000).expect("catch-up");
    for (_, out) in host.outgoing(2_000) {
        seat.receive(out.lane, &out.bytes, 2_000).expect("host payload");
    }
    assert_eq!(seat.poll_event(), Some(SeatEvent::StateReady { tick: 9 }));
    assert_eq!(seat.next_frame(), None, "no frame before the state is taken");
    assert_eq!(seat.take_state(), Some((9, vec![1, 2, 3])));
    let mut ticks = Vec::new();
    for _ in 0..4 {
        let mut handed = 0;
        while let Some(frame) = seat.next_frame() {
            ticks.push(frame.tick);
            handed += 1;
        }
        assert_eq!(handed, 3, "three frames per application frame");
        assert_eq!(
            seat.submit_input(&Pad::default()),
            Ok(Stamp::Skipped),
            "history takes no input"
        );
        let _ = seat.outgoing(2_000);
    }
    assert_eq!(ticks, (10..22).collect::<Vec<_>>());
}

#[test]
fn session_payloads_round_trip_and_hostile_ones_never_panic() {
    // The match end rides in the membership list.
    let mut enc = FrameState::new(4);
    let mut dec = FrameState::new(4);
    let frame = RawFrame {
        tick: 5,
        delay_change: None,
        membership: vec![MembershipChange {
            seat: SeatId(3),
            kind: MembershipKind::Leave,
        }],
        gap: true,
        entries: Vec::new(),
        match_end: true,
    };
    let body = wire::encode_frame_body(&frame, &mut enc);
    assert_eq!(
        wire::decode_frame_body(&body, 5, &mut dec, 64).expect("decodes"),
        frame
    );
    // The flags of an inputs datagram.
    let msg = wire::InputsMsg {
        leave: true,
        catching_up: true,
        hole: true,
        ..Default::default()
    };
    assert_eq!(
        wire::decode_inputs(&wire::encode_inputs(&msg), 64).expect("decodes"),
        msg
    );
    // A catch-up head.
    let mut base = FrameState::new(6);
    base.delay_from = 40;
    base.seats.insert(
        SeatId(2),
        wire::SeatDelta {
            present: false,
            bytes: vec![9, 9],
            seen: true,
        },
    );
    let head = wire::CatchupHead {
        state_tick: 99,
        base,
        state_len: 12,
    };
    let bytes = wire::encode_catchup(&head);
    assert_eq!(wire::decode_catchup(&bytes, 64).expect("decodes"), head);
    let mut rng = Rng(0x5e55);
    for _ in 0..20_000 {
        let mut b = bytes.clone();
        for _ in 0..1 + rng.below(4) {
            let i = rng.below(b.len() as u64) as usize;
            b[i] = rng.next() as u8;
        }
        b.truncate(rng.below(b.len() as u64 + 1) as usize);
        let _ = wire::decode_catchup(&b, 64);
        let mut seat = LockstepSeat::<Game>::catching_up(config(), SeatId(2));
        let _ = seat.receive(Lane::Stream, &b, 0);
        let _ = seat.receive(Lane::Stream, &wire::encode_state_chunk(&b), 0);
    }
}

/// The moment a seat's `CaughtUp` event fired, and that its `StateReady` came first.
fn caught_up_after_state(node: &SeatNode) -> u64 {
    let ready = node
        .timeline
        .iter()
        .rev()
        .find_map(|(ms, e)| matches!(e, SeatEvent::StateReady { .. }).then_some(*ms))
        .expect("the state arrived");
    let caught = node
        .timeline
        .iter()
        .rev()
        .find_map(|(ms, e)| matches!(e, SeatEvent::CaughtUp { .. }).then_some(*ms))
        .expect("the seat caught up");
    assert!(
        caught >= ready,
        "caught up at {caught} ms before its state arrived at {ready} ms"
    );
    caught
}

/// Review fix (ADR-8212): a member that comes back on a new connection starts its datagram
/// sequence again. The host must take its hole reports (they were classed as old datagrams of the
/// previous connection and ignored), so frames that reached it before the catch-up's head are
/// repaired.
#[test]
fn a_member_back_on_a_new_connection_gets_its_holes_repaired_when_the_stream_lags() {
    let mut rig = observer_rig(config(), &[0, 1, 2], LinkSpec::latency(10));
    let token = rig.host.resume_token(SeatId(2)).expect("a token");
    // The old connection sent for 9 s: its datagram sequence is far ahead of a new one's.
    rig.run_ms(9_000);
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").off = Some((9_000, u64::MAX, false));
    rig.run_ms(11_000);
    let back_ms = rig.now;
    reconnect(&mut rig, SeatId(2), token);
    // The catch-up's stream payloads arrive 300 ms after the datagrams that follow them.
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").hold_stream_until = rig.now + 300;
    rig.run_ms(6_000);
    let caught = caught_up_after_state(&rig.seats[&SeatId(2)]);
    assert!(
        caught - back_ms <= 5_000,
        "in step {} ms after it came back",
        caught - back_ms
    );
    assert!(
        !rig.host_timeline
            .iter()
            .any(|(ms, e)| *ms > back_ms && matches!(e, HostEvent::SeatLagging { .. })),
        "the seat was marked lagging again"
    );
    rig.assert_in_step();
}

/// Review fix (ADR-8212): a catch-up for a seat object that is still running. Datagrams of the
/// frames after the catch-up reach it before the head and are applied to its old chain; the host
/// must not take those acknowledgements for the new chain, and must not count the seat caught up
/// (or join it) before it has the state.
#[test]
fn a_catch_up_in_place_with_datagrams_ahead_of_the_head_is_not_cut_short() {
    let mut rig = observer_rig(config(), &[0, 1, 2], LinkSpec::latency(10));
    rig.run_ms(3_000);
    let sim = rig.host_sim.as_ref().expect("observer");
    let newest = *sim.stepped.last().expect("stepped");
    let state_tick = newest - 120;
    let state = sim.hashes[&state_tick];
    let issued = rig.now;
    rig.host
        .catch_up(SeatId(2), state_tick, &state.to_le_bytes(), rig.now)
        .expect("catch-up");
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").hold_stream_until = rig.now + 300;
    rig.run_ms(5_000);
    let caught = caught_up_after_state(&rig.seats[&SeatId(2)]);
    let host_caught = rig
        .host_timeline
        .iter()
        .find_map(|(ms, e)| matches!(e, HostEvent::SeatCaughtUp { seat: SeatId(2), .. }).then_some(*ms))
        .expect("the host saw it catch up");
    assert!(
        host_caught >= caught,
        "the host counted it caught up at {host_caught} ms, the seat at {caught} ms"
    );
    assert!(caught - issued <= 2_000);
    assert!(
        !rig.host_timeline
            .iter()
            .any(|(_, e)| matches!(e, HostEvent::SeatLagging { .. })),
        "the seat was marked lagging"
    );
    rig.assert_in_step();
}

/// Review fix (ADR-8212): many late inputs between two frames all merge; none is dropped when the
/// late list is long.
#[test]
fn a_long_burst_of_late_inputs_merges_every_press() {
    let mut host = LockstepHost::<Game>::new(config(), HostRole::Relay);
    host.set_input_rules(pad_rules());
    host.add_seat(SeatId(1));
    host.start(0);
    // 200 ticks built without the seat's input.
    let mut now = 0;
    while host.tick() < 200 {
        while host.poll_frame(now).is_some() {}
        now += 1;
    }
    // Then 100 datagrams, one late input each, before the next frame.
    for (seq, tick) in (50..150u64).enumerate() {
        let msg = wire::InputsMsg {
            seq: seq as u64,
            first_tick: tick,
            inputs: vec![Some(
                pressing_input(SeatId(1), tick * 5).to_bytes().expect("encodes"),
            )],
            ..Default::default()
        };
        host.receive(SeatId(1), Lane::Datagram, &wire::encode_inputs(&msg), now)
            .expect("late input");
    }
    let frame = loop {
        if let Some(frame) = host.poll_frame(now) {
            break frame;
        }
        now += 1;
    };
    let Some((_, SeatInput::Present(merged))) = frame.inputs.iter().find(|(s, _)| *s == SeatId(1)) else {
        panic!("the late inputs are in the next frame");
    };
    let presses = merged.blob.as_chunks::<4>().0.len();
    assert_eq!(presses, 100, "every late press merged once");
}

/// Review fix (ADR-8212): a seat that left and comes back through a catch-up on the same seat
/// object joins and stays: its old leave request is not sent again.
#[test]
fn a_seat_that_left_and_returns_through_a_catch_up_stays() {
    let mut rig = observer_rig(config(), &[0, 1, 2], LinkSpec::latency(10));
    rig.run_ms(2_000);
    rig.seats.get_mut(&SeatId(1)).expect("seat 1").seat.leave();
    // The game takes the player back as soon as the leave is agreed, before the seat has
    // stepped the frame that lists it.
    let leave_built = |rig: &Rig| {
        rig.frames.iter().any(|f| {
            f.membership
                .iter()
                .any(|c| c.seat == SeatId(1) && c.kind == MembershipKind::Leave)
        })
    };
    while !leave_built(&rig) {
        rig.run_ms(1);
    }
    assert!(
        rig.seats[&SeatId(1)].seat.is_member(),
        "the seat has not stepped its leave yet"
    );
    let sim = rig.host_sim.as_ref().expect("observer");
    let state_tick = *sim.stepped.last().expect("stepped") - 30;
    let state = sim.hashes[&state_tick];
    rig.host
        .catch_up(SeatId(1), state_tick, &state.to_le_bytes(), rig.now)
        .expect("catch-up");
    rig.run_ms(4_000);
    assert_eq!(removals(&rig).len(), 1, "removed again: {:?}", removals(&rig));
    assert!(rig.host.members().any(|s| s == SeatId(1)), "it joined");
    assert!(rig.seats[&SeatId(1)].seat.is_member());
    rig.assert_in_step();
}

/// Review fix (ADR-8212): a seat id given to a new player does not answer the old player's token.
#[test]
fn a_token_does_not_survive_its_seat_being_given_to_a_new_player() {
    let mut rig = observer_rig(config(), &[0, 1, 2], LinkSpec::latency(10));
    let old = rig.host.resume_token(SeatId(2)).expect("a token");
    rig.run_ms(1_000);
    rig.seats.get_mut(&SeatId(2)).expect("seat 2").off = Some((1_000, u64::MAX, false));
    rig.run_ms(22_000);
    assert_eq!(removals(&rig).first().map(|r| r.2), Some("timeout"));
    assert_eq!(
        rig.host.resume(old, rig.now),
        Ok(SeatId(2)),
        "inside the resume window"
    );
    // The game gives seat 2 to someone else instead.
    rig.host.add_seat(SeatId(2));
    assert_eq!(rig.host.resume(old, rig.now), Err(LockstepError::ResumeRefused));
    let new = rig.host.resume_token(SeatId(2)).expect("the new player's token");
    assert_ne!(new, old);
    assert_eq!(rig.host.resume(new, rig.now), Ok(SeatId(2)));
}
