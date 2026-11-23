// SPDX-License-Identifier: MPL-2.0
//! The headless lockstep server game type (ADR-8558): a dedicated host with up to 40 in-process
//! seats on the soak kit's [`LockSim`], an observer simulation on its own thread that votes in
//! the hash quorum, in virtual time (the test clock is a millisecond counter; no sleeps, no wall
//! clock).
//!
//! Built with `--no-default-features` (`cargo test -p regia_net_soak --no-default-features --test
//! lockstep_server_host`) these tests run on the ECS-free regia_net core, and the 40-seat test
//! checks that the build has no ECS. With the default features they run the same way.

use std::collections::BTreeMap;

use regia_net::lockstep::server::{
    LockstepObserver, LockstepServer, LockstepServerError, LockstepServerSettings, LockstepServerStatus,
};
use regia_net::lockstep::{HostEvent, LockstepFrame, LockstepSeat, SeatId, VerdictKind, Voter};
use regia_net_soak::lockstep::{InputMix, InputSource, LockGame, LockInput, LockSim};

/// One virtual millisecond step of the test clock per host pass would be wasteful; 4 ms keeps
/// every 60 Hz tick inside one or two passes.
const PASS_MS: u64 = 4;

/// One in-process seat: the lockstep seat, its simulation and its input script.
struct Seat {
    seat: LockstepSeat<LockGame>,
    sim: LockSim,
    inputs: InputSource,
}

/// A host and its seats on a perfect in-process link.
struct Match {
    server: LockstepServer<LockGame>,
    seats: Vec<Seat>,
    now_ms: u64,
    /// Sync the observer after each pass (its progress is then a function of the passes alone).
    sync: bool,
}

impl Match {
    fn new(seats: u16, settings: &LockstepServerSettings) -> Self {
        let observer: Option<Box<dyn LockstepObserver<_>>> = if settings.observer {
            Some(Box::new(LockSim::default()))
        } else {
            None
        };
        Self::with_observer(seats, settings, observer)
    }

    fn with_observer(
        seats: u16,
        settings: &LockstepServerSettings,
        observer: Option<Box<dyn LockstepObserver<LockInput>>>,
    ) -> Self {
        let mut server = LockstepServer::<LockGame>::new(settings, 60, observer).expect("server");
        let mut list = Vec::new();
        for n in 0..seats {
            server.add_seat(SeatId(n)).expect("a free seat");
            list.push(Seat {
                seat: LockstepSeat::new(server.config().clone(), SeatId(n)),
                sim: LockSim::default(),
                inputs: InputSource::new(0x5eed, n, InputMix::ACTION),
            });
        }
        server.start(0);
        Self {
            server,
            seats: list,
            now_ms: 0,
            sync: true,
        }
    }

    /// One host pass at the current virtual time, then the clock moves on.
    fn pass(&mut self, mut after_step: impl FnMut(usize, u64, &mut LockSim)) {
        let now = self.now_ms;
        for (i, s) in self.seats.iter_mut().enumerate() {
            let input = s.inputs.next_input();
            let _ = s.seat.submit_input(&input);
            for out in s.seat.outgoing(now) {
                self.server
                    .receive(SeatId(i as u16), out.lane, &out.bytes, now)
                    .expect("seat bytes");
            }
        }
        self.server.tick(now);
        if self.sync {
            self.server.sync_observer();
        }
        for (to, out) in self.server.outgoing(now) {
            let s = &mut self.seats[usize::from(to.0)];
            s.seat.receive(out.lane, &out.bytes, now).expect("host bytes");
        }
        for (i, s) in self.seats.iter_mut().enumerate() {
            while let Some(frame) = s.seat.next_frame() {
                s.sim.step(&frame);
                after_step(i, frame.tick, &mut s.sim);
                s.seat.report_hash(frame.tick, s.sim.hash());
            }
            while s.seat.poll_event().is_some() {}
        }
        self.now_ms += PASS_MS;
    }

    /// Passes until every seat has stepped `ticks` frames (bounded: a stalled match fails).
    fn run_to(&mut self, ticks: u64, mut after_step: impl FnMut(usize, u64, &mut LockSim)) {
        let limit = self.now_ms + ticks * 40 + 2_000;
        while self.seats.iter().any(|s| s.sim.tick < ticks) {
            assert!(self.now_ms < limit, "the seats stalled at {:?}", self.ticks());
            self.pass(&mut after_step);
        }
    }

    fn ticks(&self) -> Vec<u64> {
        self.seats.iter().map(|s| s.sim.tick).collect()
    }

    fn status(&mut self) -> LockstepServerStatus {
        self.server.status(self.now_ms)
    }
}

fn settings() -> LockstepServerSettings {
    LockstepServerSettings {
        enabled: true,
        ..LockstepServerSettings::default()
    }
}

/// Built with `--no-default-features`, regia_net has no ECS half: checked at compile time.
#[cfg(not(feature = "ecs"))]
const NO_ECS_IN_THIS_BUILD: () = assert!(
    !regia_net::ECS_FEATURE,
    "built with --no-default-features, regia_net has no ECS"
);

#[test]
fn headless_observer_host_runs_40_seats_without_ecs() {
    #[cfg(not(feature = "ecs"))]
    let () = NO_ECS_IN_THIS_BUILD;
    // The no-default build is the claim: `NO_ECS_IN_THIS_BUILD` above fails the build when the soak
    // kit's `ecs` feature is off and regia_net still has its ECS half.
    let settings = settings();
    assert_eq!(
        settings.max_seats, 40,
        "the default seat count is DO27's load-test cap"
    );
    let mut m = Match::new(40, &settings);
    // The 41st seat is refused with a named code.
    let full = m.server.add_seat(SeatId(40)).expect_err("41 seats");
    assert_eq!(full, LockstepServerError::SeatsFull { max: 40 });
    assert_eq!(full.code(), "lockstep_seats_full");

    m.run_to(600, |_, _, _| {});

    let s = m.status();
    assert_eq!(s.seats, 40);
    assert_eq!(s.seats_refused, 1);
    assert!(s.frames_built >= 600, "{}", s.frames_built);
    let obs = s.observer.as_ref().expect("an observer");
    assert!(obs.attached && obs.voting, "{obs:?}");
    assert!(obs.frames_stepped >= 600, "{obs:?}");
    assert_eq!((obs.dropouts, obs.rejoins), (0, 0), "{obs:?}");
    // Every checkpoint up to tick 570 closed agreed with 41 voters, and nobody diverged.
    assert_eq!(s.verdicts, 0, "{:?}", s.recent_verdicts);
    assert_eq!(s.lockstep.desyncs, 0);
    assert!(
        s.lockstep.checkpoints_agreed >= 18,
        "{}",
        s.lockstep.checkpoints_agreed
    );
    // All 40 simulations are the same at tick 600.
    let at_600: Vec<u64> = m
        .seats
        .iter()
        .map(|seat| {
            assert!(seat.sim.tick >= 600);
            seat.sim.hulls.len() as u64
        })
        .collect();
    assert!(
        at_600.iter().all(|n| *n == 40),
        "every seat sees 40 hulls: {at_600:?}"
    );
    let hashes: std::collections::BTreeSet<u64> = m
        .seats
        .iter()
        .filter(|seat| seat.sim.tick == m.seats[0].sim.tick)
        .map(|seat| seat.sim.hash())
        .collect();
    assert_eq!(hashes.len(), 1, "seats at the same tick hold the same state");
}

#[test]
fn the_observer_counts_in_the_quorum() {
    let settings = settings();
    let mut m = Match::new(2, &settings);
    // Seat 1's simulation goes wrong after it steps tick 100 (a bug or a flipped bit).
    m.run_to(400, |seat, tick, sim| {
        if seat == 1
            && tick == 100
            && let Some(hull) = sim.hulls.values_mut().next()
        {
            hull.x += 0.5;
        }
    });
    let s = m.status();
    let verdict = s
        .recent_verdicts
        .iter()
        .find(|v| v.kind == VerdictKind::Diverged)
        .unwrap_or_else(|| panic!("a 2-of-3 verdict: {:?}", s.recent_verdicts));
    assert_eq!(verdict.diverged, vec![Voter::Seat(SeatId(1))], "{verdict:?}");
    assert_eq!(
        verdict.agreeing,
        vec![Voter::Observer, Voter::Seat(SeatId(0))],
        "the observer and seat 0 are the majority: {verdict:?}"
    );
    assert!(!verdict.host_diverged);
    assert!(verdict.tick > 100 && verdict.tick <= 150, "{verdict:?}");
    let obs = s.observer.expect("observer");
    assert!(obs.voting && !obs.diverged, "{obs:?}");
}

/// The same match twice, the second with its observer held for 300 ticks: the seats' ticks and
/// states are the same pass for pass, the observer leaves the quorum and comes back.
#[test]
fn a_slow_observer_never_stalls_the_seats() {
    let settings = settings();
    let mut control = Match::new(4, &settings);
    let mut slow = Match::new(4, &settings);
    let throttle = slow.server.observer_throttle().expect("observer hook");

    let mut held_at = None;
    let mut released_at = None;
    let mut pass = 0u64;
    let mut agreed_while_out = None;
    while slow.seats.iter().any(|s| s.sim.tick < 900) {
        pass += 1;
        assert!(pass < 4_000, "stalled at {:?}", slow.ticks());
        control.pass(|_, _, _| {});
        slow.pass(|_, _, _| {});
        // Pass for pass, the held observer changes nothing the seats see.
        assert_eq!(slow.ticks(), control.ticks(), "pass {pass}");
        let tick = slow.server.host().tick();
        if held_at.is_none() && tick >= 120 {
            throttle.hold();
            held_at = Some(tick);
        }
        if let Some(at) = held_at
            && released_at.is_none()
            && tick >= at + 300
        {
            let s = slow.status();
            let obs = s.observer.as_ref().expect("observer");
            assert!(!obs.voting, "out of the quorum while held: {obs:?}");
            assert_eq!(obs.dropouts, 1, "{obs:?}");
            assert!(obs.lag_ticks > 60, "{obs:?}");
            agreed_while_out = Some(s.lockstep.checkpoints_agreed);
            throttle.release();
            released_at = Some(tick);
        }
    }
    let s = slow.status();
    let obs = s.observer.as_ref().expect("observer");
    assert!(obs.voting && obs.attached, "caught up and back in: {obs:?}");
    assert_eq!((obs.dropouts, obs.rejoins), (1, 1), "{obs:?}");
    assert!(obs.lag_max_ticks > 60, "{obs:?}");
    // Checkpoints closed while the observer was out (the seats' votes alone agreed).
    let held = held_at.expect("held");
    let out = agreed_while_out.expect("released");
    assert!(
        out >= held / 30 + 8,
        "agreed {out} by the release, held at {held}"
    );
    assert_eq!(s.verdicts, 0, "{:?}", s.recent_verdicts);
    // The two matches built the same frames and the seats hold the same states.
    let c = control.status();
    assert_eq!(s.frames_built, c.frames_built);
    let hash = |m: &Match| m.seats.iter().map(|s| s.sim.hash()).collect::<Vec<_>>();
    assert_eq!(hash(&slow), hash(&control));
}

#[test]
fn a_stalled_observer_is_detached_past_its_backlog() {
    let settings = LockstepServerSettings {
        observer_max_lag_ticks: 30,
        observer_max_backlog_ticks: 90,
        ..settings()
    };
    let mut m = Match::new(3, &settings);
    m.run_to(60, |_, _, _| {});
    m.server.observer_throttle().expect("hook").hold();
    m.run_to(300, |_, _, _| {});
    let s = m.status();
    let obs = s.observer.as_ref().expect("observer");
    assert!(obs.detached && !obs.attached && !obs.voting, "{obs:?}");
    assert_eq!(obs.dropouts, 1, "{obs:?}");
    // Detached, it is never fed again: the lag it shows stays at what it was.
    assert!(obs.frames_stepped <= 61, "{obs:?}");
    assert_eq!(s.verdicts, 0);
    assert!(m.seats.iter().all(|seat| seat.sim.tick >= 300));
}

#[test]
fn a_relay_host_without_an_observer_runs_and_refuses_a_missing_simulation() {
    let missing = LockstepServer::<LockGame>::new(&settings(), 60, None).expect_err("no observer sim");
    assert_eq!(missing.code(), "lockstep_observer_missing");
    let relay = LockstepServerSettings {
        observer: false,
        ..settings()
    };
    let mut m = Match::new(3, &relay);
    m.run_to(200, |_, _, _| {});
    let s = m.status();
    assert!(s.observer.is_none());
    assert_eq!(s.verdicts, 0);
    assert!(s.lockstep.checkpoints_agreed >= 5);
}

/// The observer's simulation goes wrong once, after it steps `at` live (a bug or a flipped bit on
/// the server); `loads` says whether it can take a state transfer.
struct Drifting {
    sim: LockSim,
    at: u64,
    drifted: bool,
    loads: bool,
}

impl Drifting {
    fn boxed(at: u64, loads: bool) -> Option<Box<dyn LockstepObserver<LockInput>>> {
        Some(Box::new(Self {
            sim: LockSim::default(),
            at,
            drifted: false,
            loads,
        }))
    }
}

impl LockstepObserver<LockInput> for Drifting {
    fn step(&mut self, frame: &LockstepFrame<LockInput>) {
        self.sim.step(frame);
        if !self.drifted
            && frame.tick == self.at
            && let Some(hull) = self.sim.hulls.values_mut().next()
        {
            hull.x += 0.5;
            self.drifted = true;
        }
    }
    fn state_hash(&self, _tick: u64) -> u64 {
        self.sim.hash()
    }
    fn loads_state(&self) -> bool {
        self.loads
    }
    fn load_state(&mut self, tick: u64, state: &[u8]) -> bool {
        self.sim.load_state(tick, state)
    }
}

/// What a transfer run saw: the host transfers ordered (source seat, tick) and the ticks of the
/// states handed to the observer.
#[derive(Debug, Default)]
struct Transfers {
    ordered: Vec<(SeatId, u64)>,
    delivered: Vec<u64>,
}

/// Runs `m` until every seat stepped `ticks`, playing the game's side of a host transfer: each
/// seat keeps its state at every checkpoint (the bytes it would send for `ProvideState`), and the
/// state of the seat a `ResyncHost` names goes to the observer. `bytes` may replace the state
/// (a corrupt transfer). Seat 1 drifts after it steps `seat_drift` when set.
fn run_with_transfers(
    m: &mut Match,
    ticks: u64,
    seat_drift: Option<u64>,
    bytes: impl Fn(Vec<u8>) -> Vec<u8>,
) -> Transfers {
    let every = u64::from(m.server.config().hash_every.max(1));
    let mut snaps: BTreeMap<(usize, u64), Vec<u8>> = BTreeMap::new();
    let mut seen = Transfers::default();
    let limit = m.now_ms + ticks * 40 + 2_000;
    while m.seats.iter().any(|s| s.sim.tick < ticks) {
        assert!(m.now_ms < limit, "the seats stalled at {:?}", m.ticks());
        m.pass(|seat, tick, sim| {
            if seat == 1
                && Some(tick) == seat_drift
                && let Some(hull) = sim.hulls.values_mut().next()
            {
                hull.x += 0.5;
            }
            if tick.is_multiple_of(every) {
                snaps.insert((seat, tick), serde_json::to_vec(sim).expect("the sim serializes"));
            }
        });
        while let Some(event) = m.server.poll_event() {
            if let HostEvent::ResyncHost {
                source: Voter::Seat(seat),
                tick,
                ..
            } = event
            {
                seen.ordered.push((seat, tick));
            }
        }
        if let Some(tick) = m.server.observer_transfer()
            && let Some(&(seat, _)) = seen.ordered.iter().rev().find(|(_, t)| *t == tick)
            && let Some(state) = snaps.get(&(usize::from(seat.0), tick))
        {
            m.server
                .load_observer_state(tick, bytes(state.clone()))
                .expect("the transfer is pending");
            seen.delivered.push(tick);
        }
    }
    seen
}

/// KI-NT-036 (ADR-8855): an observer the two seats outvote takes the majority seat's state,
/// replays to the live edge and votes again. Proof that it holds the majority's state and is back
/// in the quorum: when seat 1 drifts later, the observer and seat 0 outvote it 2 of 3. At v0.6.0
/// the observer left the quorum for the match (`diverged`), so the later drift was a 1-1 split.
#[test]
fn an_outvoted_observer_takes_the_majority_state_and_votes_again() {
    let mut m = Match::with_observer(2, &settings(), Drifting::boxed(100, true));
    let seen = run_with_transfers(&mut m, 700, Some(400), |b| b);
    m.server.sync_observer();
    let s = m.status();
    assert_eq!(seen.ordered.len(), 1, "one host transfer: {seen:?}");
    let (source, tick) = seen.ordered[0];
    assert!(
        tick > 100 && tick <= 160,
        "the checkpoint after the drift: {seen:?}"
    );
    assert_eq!(seen.delivered, vec![tick], "{seen:?}");
    let first = s
        .recent_verdicts
        .iter()
        .find(|v| v.host_diverged)
        .unwrap_or_else(|| panic!("the observer was outvoted: {:?}", s.recent_verdicts));
    assert_eq!(first.diverged, vec![Voter::Observer], "{first:?}");
    assert!(first.agreeing.contains(&Voter::Seat(source)), "{first:?}");
    let obs = s.observer.as_ref().expect("observer");
    assert!(obs.voting && !obs.diverged, "back in the quorum: {obs:?}");
    assert_eq!((obs.resyncs, obs.transfer_tick), (1, None), "{obs:?}");
    let later = s
        .recent_verdicts
        .iter()
        .find(|v| v.tick > 400 && v.kind == VerdictKind::Diverged)
        .unwrap_or_else(|| panic!("seat 1 is outvoted 2 of 3: {:?}", s.recent_verdicts));
    assert_eq!(later.diverged, vec![Voter::Seat(SeatId(1))], "{later:?}");
    assert_eq!(
        later.agreeing,
        vec![Voter::Observer, Voter::Seat(SeatId(0))],
        "the observer holds the majority's state: {later:?}"
    );
    assert!(
        s.health_json().contains("\"resyncs\":1,\"transfer_tick\":null"),
        "{}",
        s.health_json()
    );
}

/// The fallbacks (ADR-8855): an observer whose simulation cannot load a state leaves the quorum
/// for the match as before; one that refuses the bytes it got does too; a state nobody asked for
/// is refused with a code.
#[test]
fn an_observer_that_cannot_load_or_refuses_the_state_leaves_the_quorum() {
    let mut cannot = Match::with_observer(2, &settings(), Drifting::boxed(100, false));
    let seen = run_with_transfers(&mut cannot, 300, None, |b| b);
    let s = cannot.status();
    let obs = s.observer.as_ref().expect("observer");
    assert!(obs.diverged && !obs.voting, "{obs:?}");
    assert_eq!((obs.resyncs, obs.transfer_tick), (0, None), "{obs:?}");
    assert!(
        seen.delivered.is_empty(),
        "nothing is asked of the game: {seen:?}"
    );
    let refused = cannot
        .server
        .load_observer_state(120, vec![1, 2, 3])
        .expect_err("no transfer pending");
    assert_eq!(refused.code(), "lockstep_observer_no_transfer");

    let mut corrupt = Match::with_observer(2, &settings(), Drifting::boxed(100, true));
    let seen = run_with_transfers(&mut corrupt, 300, None, |_| b"not a state".to_vec());
    corrupt.server.sync_observer();
    let s = corrupt.status();
    let obs = s.observer.as_ref().expect("observer");
    assert_eq!(seen.delivered.len(), 1, "{seen:?}");
    assert!(obs.diverged && !obs.voting, "{obs:?}");
    assert_eq!(obs.resyncs, 0, "{obs:?}");
}

/// A state transfer that waits while the observer keeps stepping must not move the next
/// seat desync. The observer drifts at frame 100; the game delivers seat 0's checkpoint
/// only once the host has built frame 199, so the host ring still holds the observer's
/// diverged hashes through that span. Seat 1 then drifts at frame 190. The probe's
/// reference is the recovered observer (ADR-8855), and `first_diverged_tick` has to be
/// 190: the first tick seat 1's hash differs from the majority, not an earlier tick whose
/// ring entry was recorded before the load.
#[test]
fn a_late_observer_load_does_not_move_the_next_seats_first_diverged_tick() {
    let mut m = Match::with_observer(2, &settings(), Drifting::boxed(100, true));
    let every = u64::from(m.server.config().hash_every.max(1));
    let mut snaps: BTreeMap<(usize, u64), Vec<u8>> = BTreeMap::new();
    let mut transfer: Option<(SeatId, u64)> = None;
    let mut loaded_at: Option<u64> = None;
    let mut located: Vec<(Voter, u64, u64)> = Vec::new();
    let seat_drift = 190;
    let ticks = 520;
    let limit = m.now_ms + ticks * 40 + 2_000;
    while m.seats.iter().any(|s| s.sim.tick < ticks) {
        assert!(m.now_ms < limit, "the seats stalled at {:?}", m.ticks());
        m.pass(|seat, tick, sim| {
            if seat == 1
                && tick == seat_drift
                && let Some(hull) = sim.hulls.values_mut().next()
            {
                hull.x += 0.5;
            }
            if tick.is_multiple_of(every) {
                snaps.insert((seat, tick), serde_json::to_vec(sim).expect("the sim serializes"));
            }
        });
        while let Some(event) = m.server.poll_event() {
            match event {
                HostEvent::ResyncHost {
                    source: Voter::Seat(seat),
                    tick,
                    ..
                } => transfer = Some((seat, tick)),
                HostEvent::DesyncLocated {
                    tick,
                    voter,
                    first_diverged_tick,
                } => located.push((voter, tick, first_diverged_tick)),
                _ => {}
            }
        }
        if loaded_at.is_none()
            && m.server.host().tick() >= 200
            && let Some(tick) = m.server.observer_transfer()
            && let Some((seat, ordered)) = transfer
            && ordered == tick
            && let Some(state) = snaps.get(&(usize::from(seat.0), tick)).cloned()
        {
            let next = m.server.host().tick();
            assert!(
                next < 210,
                "the late load has to land before checkpoint 210 opens (next {next})"
            );
            m.server
                .load_observer_state(tick, state)
                .expect("the transfer is pending");
            m.server.sync_observer();
            loaded_at = Some(next);
        }
    }
    m.server.sync_observer();
    while let Some(event) = m.server.poll_event() {
        if let HostEvent::DesyncLocated {
            tick,
            voter,
            first_diverged_tick,
        } = event
        {
            located.push((voter, tick, first_diverged_tick));
        }
    }
    let s = m.status();
    let obs = s.observer.as_ref().expect("observer");
    assert!(loaded_at.is_some(), "the late load ran: transfer={transfer:?}");
    assert!(
        obs.voting && !obs.diverged,
        "the observer recovered from the delayed load: {obs:?} loaded_at={loaded_at:?}"
    );
    assert_eq!(obs.resyncs, 1, "{obs:?}");
    let later = s
        .recent_verdicts
        .iter()
        .find(|v| v.tick > seat_drift && v.kind == VerdictKind::Diverged)
        .unwrap_or_else(|| panic!("seat 1 is outvoted: {:?}", s.recent_verdicts));
    assert_eq!(
        later.diverged,
        vec![Voter::Seat(SeatId(1))],
        "the quorum still blames only seat 1: {later:?}"
    );
    assert_eq!(
        later.source,
        Some(Voter::Observer),
        "the recovered observer is the probe reference: {later:?}"
    );
    let seat1 = located
        .iter()
        .find(|(voter, _, _)| *voter == Voter::Seat(SeatId(1)))
        .unwrap_or_else(|| panic!("seat 1 was located: {located:?} verdict={later:?}"));
    assert_eq!(
        seat1.2, seat_drift,
        "the probe names seat 1's drift, not a tick the ring recorded before the load: \
         located={located:?} verdict={later:?} loaded_at={loaded_at:?}"
    );
}
