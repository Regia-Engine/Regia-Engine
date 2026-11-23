// SPDX-License-Identifier: MPL-2.0
//! Rollback lockstep tests (spec 120 § 7.7, ADR-8361): a fighting-style sample on the virtual-time
//! rig. Two fighters, a relay host, links shaped like the soak kit's transatlantic profile; the
//! confirmed state of every seat is compared with a lockstep reference stepped offline from the
//! same input scripts. Seeded, virtual time, no wall clock.

use super::*;

/// Fighter speed per tick, arena half-width and reach, in fixed-point units (1/256).
const SPEED: i32 = 96;
const ARENA: i32 = 40 * 256;
const REACH: i32 = 3 * 256;
const DAMAGE: i32 = 7;
const COOLDOWN: u32 = 18;

/// A fighting-style simulation: one-dimensional fighters, a punch press with a cooldown. Integer
/// arithmetic, so every participant steps it the same.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Fight {
    tick: u64,
    fighters: BTreeMap<u16, Fighter>,
    hits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fighter {
    x: i32,
    health: i32,
    cooldown: u32,
    facing: i32,
}

impl Fight {
    fn step_frame(&mut self, frame: &LockstepFrame<Pad>) {
        for (seat, _) in &frame.inputs {
            self.fighters.entry(seat.0).or_insert_with(|| Fighter {
                x: if seat.0 == 0 { -5 * 256 } else { 5 * 256 },
                health: 100,
                cooldown: 0,
                facing: if seat.0 == 0 { 1 } else { -1 },
            });
        }
        let mut punches = Vec::new();
        for (seat, input) in &frame.inputs {
            let Some(f) = self.fighters.get_mut(&seat.0) else {
                continue;
            };
            f.cooldown = f.cooldown.saturating_sub(1);
            if let Some(pad) = input.as_ref() {
                let dir = i32::from(pad.move_x.signum());
                f.x = (f.x + dir * SPEED).clamp(-ARENA, ARENA);
                if dir != 0 {
                    f.facing = dir;
                }
                if pad.buttons & 1 != 0 && f.cooldown == 0 {
                    f.cooldown = COOLDOWN;
                    punches.push(seat.0);
                }
            }
        }
        for attacker in punches {
            let Some(a) = self.fighters.get(&attacker).cloned() else {
                continue;
            };
            for (id, f) in &mut self.fighters {
                if *id != attacker && (f.x - a.x).abs() <= REACH && (f.x - a.x).signum() == a.facing {
                    f.health -= DAMAGE;
                    self.hits += 1;
                    if f.health <= 0 {
                        f.health = 100;
                    }
                }
            }
        }
        self.tick = frame.tick + 1;
    }

    fn state_hash(&self) -> u64 {
        let mut h = mix(0x0f19_4700, self.tick);
        h = mix(h, u64::from(self.hits));
        for (id, f) in &self.fighters {
            h = mix(h, u64::from(*id));
            h = mix(h, f.x as u32 as u64);
            h = mix(h, f.health as u32 as u64);
            h = mix(h, u64::from(f.cooldown));
            h = mix(h, f.facing as u32 as u64);
        }
        h
    }
}

impl RollbackGame<Pad> for Fight {
    fn step(&mut self, frame: &LockstepFrame<Pad>) {
        self.step_frame(frame);
    }
    fn hash(&self) -> u64 {
        self.state_hash()
    }
}

/// The fighter's input script: walk toward the other fighter and back, punch about every 9 ticks
/// (a press: cleared by the prediction, so every punch a remote seat throws is a misprediction).
fn script(seat: u16, capture: u64) -> Pad {
    let toward: i8 = if seat == 0 { 1 } else { -1 };
    let phase = (capture / 40 + u64::from(seat)) % 3;
    let move_x = match phase {
        0 => toward,
        1 => 0,
        _ => -toward,
    };
    let punch = (capture + u64::from(seat) * 4).is_multiple_of(9);
    Pad {
        move_x,
        move_y: 0,
        aim: 0,
        buttons: u16::from(punch),
        blob: Vec::new(),
    }
}

fn rules() -> InputRules<Pad> {
    InputRules {
        clear_presses: |p| Pad {
            buttons: 0,
            ..p.clone()
        },
        merge_late: |late, next| next.buttons |= late.buttons,
    }
}

/// The lockstep reference's frame for `tick`: what the host builds when no input is ever late.
/// Ticks below the delay are the start's gap ticks.
fn reference_frame(seats: &[u16], delay: u64, tick: u64) -> LockstepFrame<Pad> {
    let inputs = seats
        .iter()
        .map(|s| {
            let input = if tick < delay {
                SeatInput::Absent
            } else {
                SeatInput::Present(script(*s, tick - delay))
            };
            (SeatId(*s), input)
        })
        .collect();
    LockstepFrame {
        tick,
        inputs,
        delay: delay as u16,
        delay_from: 0,
        membership: Vec::new(),
        match_end: false,
    }
}

/// The lockstep reference stepped offline through `ticks` ticks, with the hash after each.
fn reference(seats: &[u16], delay: u64, ticks: u64) -> (Fight, Vec<u64>) {
    let mut fight = Fight::default();
    let mut hashes = Vec::new();
    for tick in 0..ticks {
        fight.step_frame(&reference_frame(seats, delay, tick));
        hashes.push(fight.state_hash());
    }
    (fight, hashes)
}

struct Fighter2 {
    seat: LockstepSeat<Game>,
    runner: RollbackRunner<Game, Fight>,
    up: Pipe,
    down: Pipe,
    phase: u64,
    /// Confirmed hashes by tick, as the runner handed them out.
    confirmed: BTreeMap<u64, u64>,
    stalls: u64,
    max_ahead: u64,
}

struct FightRig {
    host: LockstepHost<Game>,
    seats: Vec<Fighter2>,
    now: u64,
    started: bool,
    /// `(ms, tick)` of every frame the host built.
    built: Vec<(u64, u64)>,
}

/// One application frame every this many milliseconds on a seat (60 Hz).
const FRAME_MS: u64 = 16;
/// When the host starts the frame clock, virtual milliseconds.
const START_MS: u64 = 200;

impl FightRig {
    fn new(config: LockstepConfig, link: LinkSpec, seed: u64) -> Self {
        let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
        host.set_input_rules(rules());
        let mut seats = Vec::new();
        for s in 0..2u16 {
            host.add_seat(SeatId(s));
            let mut seat = LockstepSeat::<Game>::new(config.clone(), SeatId(s));
            seat.set_input_rules(rules());
            seats.push(Fighter2 {
                runner: RollbackRunner::new(&config, SeatId(s), Fight::default()),
                seat,
                up: Pipe::new(link, seed ^ (0x100 + u64::from(s))),
                down: Pipe::new(link, seed ^ (0x200 + u64::from(s))),
                phase: u64::from(s) * 7 % FRAME_MS,
                confirmed: BTreeMap::new(),
                stalls: 0,
                max_ahead: 0,
            });
        }
        Self {
            host,
            seats,
            now: 0,
            started: false,
            built: Vec::new(),
        }
    }

    fn step_ms(&mut self) {
        let now = self.now;
        // The host starts once it has heard every seat, as a server starts a match once every
        // player is connected; inputs before that are refused.
        if !self.started && now >= START_MS {
            self.host.start(now);
            self.started = true;
        }
        for (i, node) in self.seats.iter_mut().enumerate() {
            for (lane, bytes) in node.up.due(now) {
                match self.host.receive(SeatId(i as u16), lane, &bytes, now) {
                    Ok(()) | Err(LockstepError::NotStarted) => {}
                    Err(e) => panic!("seat bytes decode: {e}"),
                }
            }
        }
        let mut built = false;
        while let Some(frame) = self.host.poll_frame(now) {
            self.built.push((now, frame.tick));
            built = true;
        }
        if built {
            for (to, out) in self.host.outgoing(now) {
                self.seats[usize::from(to.0)].down.send(now, out);
            }
        }
        for (i, node) in self.seats.iter_mut().enumerate() {
            if now % FRAME_MS != node.phase {
                continue;
            }
            for (lane, bytes) in node.down.due(now) {
                node.seat.receive(lane, &bytes, now).expect("host bytes decode");
            }
            let s = i as u16;
            let confirmed = &mut node.confirmed;
            let report = node.runner.advance(
                &mut node.seat,
                now,
                |capture| script(s, capture),
                |tick, state: &Fight| {
                    confirmed.insert(tick, state.state_hash());
                },
            );
            assert_eq!(report.inputs_refused, 0, "every local input is stamped");
            node.stalls += u64::from(report.stalled);
            node.max_ahead = node.max_ahead.max(node.runner.ahead_ticks());
            for out in node.seat.outgoing(now) {
                node.up.send(now, out);
            }
        }
        self.now += 1;
    }

    fn run_ms(&mut self, ms: u64) {
        for _ in 0..ms {
            self.step_ms();
        }
    }
}

/// The soak kit's transatlantic profile as the rig's link: 45 ms + 0-6 ms jitter one way, 0.3 %
/// loss.
const TRANSATLANTIC: LinkSpec = LinkSpec {
    latency_ms: 45,
    jitter_ms: 6,
    loss_per_mille: 3,
    burst_start_per_mille: 0,
    burst_stay_per_mille: 0,
    dup_per_mille: 0,
};

fn fixed_delay(delay: u16) -> LockstepConfig {
    LockstepConfig {
        initial_delay_ticks: delay,
        min_delay_ticks: delay,
        max_delay_ticks: delay,
        ..LockstepConfig::rollback(60)
    }
}

/// Spec 120 § 7.7 acceptance on the rig: two fighters, each on a transatlantic link to a relay host
/// (seat to seat about 100 ms one way). The local input is stepped exactly the configured delay
/// after it was sampled, the seats run ahead of their frames and roll back, never deeper than the
/// window, and the confirmed state of both seats is the lockstep reference's, bit for bit.
#[test]
fn two_fighters_on_transatlantic_roll_back_within_the_window_and_confirm_the_lockstep_state() {
    for delay in [1u16, 2, 3] {
        let config = fixed_delay(delay);
        let mut rig = FightRig::new(config, TRANSATLANTIC, 0x5eed_7717 + u64::from(delay));
        rig.run_ms(20_000);
        let host = rig.host.status(rig.now);
        assert!(
            host.seats
                .iter()
                .all(|s| s.inputs_missing == 0 && s.inputs_late_merged == 0),
            "no input missed its frame (delay {delay}): {:?}",
            host.seats
                .iter()
                .map(|s| (s.inputs_missing, s.inputs_late_merged, s.lateness_p99_us))
                .collect::<Vec<_>>()
        );
        assert_eq!(host.desyncs, 0);
        for (i, node) in rig.seats.iter().enumerate() {
            let status = node.seat.status();
            assert_eq!(status.rollback_window_ticks, MAX_ROLLBACK_TICKS);
            assert_eq!(
                status.local_input_lag_ticks,
                Some((u64::from(delay), u64::from(delay))),
                "seat {i}: the local input is stepped {delay} ticks after it was sampled"
            );
            assert!(status.ticks_predicted > 1_000, "seat {i} ran ahead: {status:?}");
            assert!(
                status.rollbacks > 50,
                "seat {i}: remote punches are mispredicted: {status:?}"
            );
            assert!(
                status.rollback_ticks_max <= u64::from(MAX_ROLLBACK_TICKS),
                "seat {i}: rollback of {} ticks",
                status.rollback_ticks_max
            );
            assert!(node.max_ahead <= u64::from(MAX_ROLLBACK_TICKS));
            // Confirmed ticks are the frames, every one handed out once.
            let confirmed = node.runner.confirmed_tick();
            assert!(confirmed > 1_000, "seat {i} confirmed {confirmed} ticks");
            assert_eq!(node.confirmed.len() as u64, confirmed);
            let (want, hashes) = reference(&[0, 1], u64::from(delay), confirmed);
            assert_eq!(
                node.runner.confirmed_state(),
                &want,
                "seat {i} (delay {delay}): the confirmed state is the lockstep reference's"
            );
            // Every confirmed tick's hash, not just the last.
            for (tick, hash) in hashes.iter().enumerate() {
                assert_eq!(
                    node.confirmed.get(&(tick as u64)),
                    Some(hash),
                    "seat {i} tick {tick}"
                );
            }
            assert!(want.hits > 20, "the fighters hit each other: {}", want.hits);
        }
    }
}

/// A transatlantic seat still feels only the configured delay at 3 ticks: the stalls at the window
/// (the seat waiting for a frame) stay rare.
#[test]
fn a_transatlantic_fighter_rarely_waits_at_the_window() {
    let mut rig = FightRig::new(fixed_delay(2), TRANSATLANTIC, 0x0a11);
    rig.run_ms(30_000);
    for node in &rig.seats {
        let frames = rig.now / FRAME_MS;
        assert!(
            node.stalls * 100 <= frames,
            "{} of {frames} application frames waited at the window",
            node.stalls
        );
    }
}

/// The strict rig as the control: the same fighters with the strict mode and the same delay never
/// predict, so their input is stepped only when the frame arrives (a round trip later).
#[test]
fn strict_mode_predicts_nothing_and_the_runner_steps_frames_as_they_arrive() {
    let config = LockstepConfig {
        mode: LockstepMode::Strict,
        ..fixed_delay(2)
    };
    let mut rig = FightRig::new(config, LinkSpec::latency(5), 3);
    rig.run_ms(5_000);
    for node in &rig.seats {
        let status = node.seat.status();
        assert_eq!(status.rollback_window_ticks, 0);
        assert!(status.rollback_ticks_max <= 1, "{status:?}");
        assert!(node.runner.ahead_ticks() <= 1);
    }
    let (a, b) = (&rig.seats[0].confirmed, &rig.seats[1].confirmed);
    let common = a.len().min(b.len()) as u64;
    assert!(common > 200);
    assert!(
        (0..common).all(|t| a.get(&t) == b.get(&t)),
        "both seats confirm the same states"
    );
}

/// The host holds a due frame for a live seat's input for up to twice the rollback window, and no
/// longer: a seat that went quiet is listed with its held input after that.
#[test]
fn the_host_holds_a_frame_for_a_live_seats_input_up_to_twice_the_window() {
    // The pure two-window hold (ADR-8361): with the slow-seat release on (the default since
    // ADR-8855), seat 1 here, whose median input misses the window, would be released after about a
    // second; `a_seat_whose_inputs_miss_the_window_is_not_held_for_and_the_other_seat_runs_free`
    // covers that.
    let config = LockstepConfig {
        release_slow_seat_hold: false,
        ..fixed_delay(2)
    };
    let tick_us = config.tick_us();
    let mut rig = FightRig::new(config, LinkSpec::latency(30), 9);
    // Seat 1's uplink is 200 ms: more than running its clock ahead by the window makes up, so its inputs
    // reach the host after their frames are due, every time.
    rig.seats[1].up = Pipe::new(LinkSpec::latency(200), 11);
    rig.run_ms(3_000);
    assert_eq!(
        rig.seats[1].runner.clock_lead_ticks(),
        i64::from(MAX_ROLLBACK_TICKS)
    );
    let late: Vec<_> = rig
        .built
        .iter()
        .filter(|(ms, tick)| *ms * 1000 > tick * tick_us + START_MS * 1000 + 1000)
        .collect();
    assert!(
        late.len() > 100,
        "frames waited for the seats' inputs: {}",
        late.len()
    );
    for (ms, tick) in &rig.built {
        assert!(
            ms * 1000
                <= START_MS * 1000 + tick * tick_us + 2 * u64::from(MAX_ROLLBACK_TICKS) * tick_us + 1000,
            "frame {tick} built at {ms} ms: held past twice the window"
        );
    }
    // Its round trip (230 ms) is more than the delay plus the window cover (10 ticks), so it waits
    // at the window too and some of its inputs come after the hold: those are held and merged
    // late. The seat on the short link loses none.
    let host = rig.host.status(rig.now);
    assert_eq!(host.seats[0].inputs_missing, 0, "{:?}", host.seats[0]);
    assert!(host.seats[1].inputs_missing > 0 && host.seats[1].inputs_late_merged > 0);
    assert!(rig.seats[1].stalls > 0);
    // Seat 1 goes quiet: its frames are built at the hold's end with its held input.
    rig.seats[1].up = Pipe::new(
        LinkSpec {
            loss_per_mille: 1000,
            ..LinkSpec::latency(200)
        },
        1,
    );
    let from = rig.built.len();
    rig.run_ms(1_000);
    let after = &rig.built[from + 2..];
    assert!(
        after.len() > 30,
        "frames keep coming without seat 1: {}",
        after.len()
    );
    for (ms, tick) in after {
        assert!(
            ms * 1000
                <= START_MS * 1000 + tick * tick_us + 2 * u64::from(MAX_ROLLBACK_TICKS) * tick_us + 1000,
            "frame {tick} built at {ms} ms"
        );
    }
    let host = rig.host.status(rig.now);
    assert!(host.seats[1].inputs_held > 20, "{:?}", host.seats[1]);
}

/// KI-NT-038 (ADR-8855): seat 1's uplink is 450 ms, so even with its clock a whole window ahead its
/// median input reaches the host about 19 ticks after its frame is due, past the rollback window.
/// Holding every frame for it (up to twice the window) made seat 0 wait at its window's edge on
/// nearly every application frame. Once the host measured that (half a second of arrivals), it
/// builds the frames without holding them for seat 1: seat 0 runs free, seat 1's inputs are merged
/// late, and both seats still confirm the same states.
#[test]
fn a_seat_whose_inputs_miss_the_window_is_not_held_for_and_the_other_seat_runs_free() {
    let config = fixed_delay(2);
    let tick_us = config.tick_us();
    let mut rig = FightRig::new(config, LinkSpec::latency(30), 21);
    rig.seats[1].up = Pipe::new(LinkSpec::latency(450), 23);
    rig.run_ms(4_000);
    let (stalls_then, from) = (rig.seats[0].stalls, rig.built.len());
    let missing_then = rig.host.status(rig.now).seats[0].inputs_missing;
    rig.run_ms(6_000);
    let frames = 6_000 / FRAME_MS;
    let stalls = rig.seats[0].stalls - stalls_then;
    assert!(
        stalls * 100 <= frames,
        "seat 0 waited at the window on {stalls} of {frames} application frames"
    );
    let wait_us = |(ms, tick): &(u64, u64)| (ms * 1000).saturating_sub(START_MS * 1000 + tick * tick_us);
    let worst = rig.built[from..].iter().map(wait_us).max().unwrap_or(0);
    assert!(worst <= 2 * tick_us + 1000, "a frame waited {worst} us");
    let host = rig.host.status(rig.now);
    assert!(host.seats[1].hold_released, "{:?}", host.seats[1]);
    assert!(!host.seats[0].hold_released, "{:?}", host.seats[0]);
    assert!(host.seats[1].inputs_late_merged > 100, "{:?}", host.seats[1]);
    assert_eq!(
        host.seats[0].inputs_missing, missing_then,
        "seat 0 lost no input after the release: {:?}",
        host.seats[0]
    );
    let (a, b) = (&rig.seats[0].confirmed, &rig.seats[1].confirmed);
    let common: Vec<u64> = a.keys().filter(|t| b.contains_key(t)).copied().collect();
    assert!(common.len() > 300, "{}", common.len());
    assert!(
        common.iter().all(|t| a.get(t) == b.get(t)),
        "both seats confirm the same states"
    );
}

/// KI-NT-038 (ADR-8855): once a seat is released, a loss burst that drops its sample count
/// does not arm the hold again. The median never comes back inside the window (the uplink
/// is still 450 ms), so the other seat must not wait out the two-window grace while the
/// host gathers a fresh half-second of arrivals.
#[test]
fn a_loss_burst_on_a_released_seat_does_not_arm_the_hold_again() {
    let config = fixed_delay(2);
    let mut rig = FightRig::new(config, LinkSpec::latency(30), 21);
    rig.seats[1].up = Pipe::new(LinkSpec::latency(450), 23);
    rig.run_ms(4_000);
    assert!(
        rig.host.status(rig.now).seats[1].hold_released,
        "warmup releases the slow seat: {:?}",
        rig.host.status(rig.now).seats[1]
    );
    rig.seats[1].up = Pipe::new(
        LinkSpec {
            loss_per_mille: 1000,
            ..LinkSpec::latency(0)
        },
        29,
    );
    rig.run_ms(1_700);
    rig.seats[1].up = Pipe::new(LinkSpec::latency(450), 31);
    let mut live_at = None;
    let mut cleared = 0u32;
    let mut sample = None;
    for _ in 0..2_500 {
        rig.step_ms();
        let host = rig.host.status(rig.now);
        let seat = &host.seats[1];
        if seat.silent_ms < 50 && !seat.interrupted {
            let start = *live_at.get_or_insert(rig.now);
            if rig.now.saturating_sub(start) <= 350 && !seat.hold_released {
                cleared += 1;
                if sample.is_none() {
                    sample = Some((
                        rig.now,
                        seat.silent_ms,
                        seat.lateness_p99_us,
                        seat.inputs_late_merged,
                    ));
                }
            }
        }
    }
    assert!(live_at.is_some(), "seat 1 was heard again after the loss burst");
    assert_eq!(
        cleared, 0,
        "hold_released cleared on {cleared} steps while seat 1 was heard again and its median was still past the window: {sample:?}"
    );
}

/// ADR-10147: the other side of the release. A released seat whose link comes back fast, after a
/// loss burst that emptied its feedback window, stays released while the window is thin and is
/// held for again once a full window of arrivals has its median back inside: the bit clears on
/// the evidence, never on the lack of it, and never stays stuck.
#[test]
fn a_released_seat_whose_link_recovers_is_held_for_again_once_its_median_is_back() {
    let config = fixed_delay(2);
    let mut rig = FightRig::new(config, LinkSpec::latency(30), 21);
    rig.seats[1].up = Pipe::new(LinkSpec::latency(450), 23);
    rig.run_ms(4_000);
    assert!(
        rig.host.status(rig.now).seats[1].hold_released,
        "warmup releases the slow seat"
    );
    rig.seats[1].up = Pipe::new(
        LinkSpec {
            loss_per_mille: 1000,
            ..LinkSpec::latency(0)
        },
        29,
    );
    rig.run_ms(1_700);
    assert!(
        rig.host.status(rig.now).seats[1].hold_released,
        "a thinned window keeps the release: {:?}",
        rig.host.status(rig.now).seats[1]
    );
    rig.seats[1].up = Pipe::new(LinkSpec::latency(30), 31);
    let mut held_again_at = None;
    for _ in 0..4_000 {
        rig.step_ms();
        if held_again_at.is_none() && !rig.host.status(rig.now).seats[1].hold_released {
            held_again_at = Some(rig.now);
        }
    }
    let host = rig.host.status(rig.now);
    assert!(
        held_again_at.is_some() && !host.seats[1].hold_released,
        "a seat back on a 30 ms link is held for again: {:?}",
        host.seats[1]
    );
    assert!(!host.seats[0].hold_released, "{:?}", host.seats[0]);
}

/// The clock sync: a seat's clock starts when its first frame arrives, a one-way trip late, and its
/// inputs then reach the host a round trip after their frames are due. The host's input margins
/// steer the clock ahead until the median input is on time: after that the host builds most frames
/// at their time and none more than two ticks late (without the sync every frame waits about
/// 60 ms here), so the hold is left for jitter.
#[test]
fn the_clock_sync_brings_the_median_input_in_before_its_frame_is_due() {
    let config = fixed_delay(2);
    let tick_us = config.tick_us();
    let mut rig = FightRig::new(config, LinkSpec::latency(30), 5);
    rig.run_ms(12_000);
    let settled_from = rig
        .built
        .iter()
        .position(|(ms, _)| *ms >= 7_000)
        .unwrap_or(rig.built.len());
    let settled = &rig.built[settled_from..];
    assert!(settled.len() > 250);
    let wait_us = |(ms, tick): &(u64, u64)| (ms * 1000).saturating_sub(START_MS * 1000 + tick * tick_us);
    let on_time = settled.iter().filter(|b| wait_us(b) <= 1000).count();
    assert!(
        on_time * 2 >= settled.len(),
        "{on_time} of {} frames built on time",
        settled.len()
    );
    let worst = settled.iter().map(wait_us).max().unwrap_or(0);
    assert!(worst <= 2 * tick_us + 1000, "a frame waited {worst} us");
    for node in &rig.seats {
        let lead = node.runner.clock_lead_ticks();
        assert!(
            (1..=i64::from(MAX_ROLLBACK_TICKS)).contains(&lead),
            "clock lead {lead}"
        );
        assert_eq!(node.seat.status().clock_lead_ticks, lead);
    }
}

/// Under rollback a delay change starts a window after the frame that carries it: no seat has
/// sampled that capture tick yet, so every seat switches on the same capture.
#[test]
fn a_delay_change_under_rollback_starts_after_the_window() {
    let config = LockstepConfig {
        initial_delay_ticks: 1,
        min_delay_ticks: 1,
        max_delay_ticks: 3,
        ..LockstepConfig::rollback(60)
    };
    let mut rig = FightRig::new(config, TRANSATLANTIC, 21);
    rig.run_ms(6_000);
    let mut raised = false;
    while let Some(event) = rig.host.poll_event() {
        if let HostEvent::DelayChanged { tick, delay_from, .. } = event {
            raised = true;
            assert_eq!(delay_from, tick + 1 + u64::from(MAX_ROLLBACK_TICKS));
        }
    }
    assert!(raised, "the transatlantic lateness raises the delay");
    for node in &rig.seats {
        let status = node.seat.status();
        assert_eq!(status.delay_ticks, rig.host.delay_ticks());
        assert!(status.rollback_ticks_max <= u64::from(MAX_ROLLBACK_TICKS));
    }
    // Both seats' confirmed hashes agree on every tick both have.
    let (a, b) = (&rig.seats[0].confirmed, &rig.seats[1].confirmed);
    let common = a.len().min(b.len()) as u64;
    assert!(common > 200);
    for tick in 0..common {
        assert_eq!(a.get(&tick), b.get(&tick), "tick {tick}");
    }
}

/// The rollback window is a match setting inside 1..=8.
#[test]
fn the_rollback_window_is_sanitized_to_one_through_eight() {
    for (asked, kept) in [(0u16, 1u16), (3, 3), (8, 8), (40, 8)] {
        let config = LockstepConfig {
            mode: LockstepMode::Rollback {
                max_rollback_ticks: asked,
            },
            ..LockstepConfig::default()
        }
        .sanitized();
        assert_eq!(config.mode.window_ticks(), kept);
        assert!(config.input_buffer_ticks >= usize::from(config.max_delay_ticks + kept) + 2);
    }
    assert_eq!(LockstepConfig::default().mode, LockstepMode::Strict);
    let preset = LockstepConfig::rollback(60);
    assert_eq!(
        (
            preset.min_delay_ticks,
            preset.initial_delay_ticks,
            preset.max_delay_ticks
        ),
        (1, 2, 3)
    );
}

/// A strict seat still refuses a second input for its capture tick, and a rollback seat refuses
/// a capture past its window.
#[test]
fn a_capture_past_the_window_is_refused() {
    let mut seat = LockstepSeat::<Game>::new(fixed_delay(2), SeatId(0));
    for capture in 0..u64::from(MAX_ROLLBACK_TICKS) {
        assert!(
            seat.submit_capture(capture, &script(0, capture)).is_ok(),
            "capture {capture}"
        );
    }
    assert_eq!(
        seat.submit_capture(u64::from(MAX_ROLLBACK_TICKS), &script(0, 8)),
        Err(LockstepError::CaptureClosed)
    );
    let mut strict = LockstepSeat::<Game>::new(LockstepConfig::default(), SeatId(0));
    assert!(strict.submit_input(&script(0, 0)).is_ok());
    assert_eq!(
        strict.submit_input(&script(0, 1)),
        Err(LockstepError::CaptureClosed)
    );
}
