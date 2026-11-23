// SPDX-License-Identifier: MPL-2.0
//! The lockstep game type of the dedicated-server host (ADR-8558): a [`HostGame`] around the
//! ECS-free [`LockstepServer`], so `server.toml`'s `[lockstep]` section turns a headless server
//! into the frame authority of a lockstep match, with an optional observer simulation that votes
//! in the hash quorum.
//!
//! The host steps it once per tick like any game: the session updates, what the seats sent comes
//! in through the game's [`LockstepSeatLink`], the due frames are built (the observer steps them
//! on its own thread) and the frames go out through the link. The host's health endpoint and the
//! agent's `server.lockstep_status` carry its status rows ([`LockstepServerStatus::health_json`]).

use super::{HostClock, HostGame, TestClock};
use crate::lockstep::net::SeatScores;
use crate::lockstep::server::{
    LockstepObserver, LockstepServer, LockstepServerError, LockstepServerSettings, LockstepServerStatus,
};
use crate::lockstep::{HostEvent, Lane, Outgoing, SeatId};
use crate::payload::NetGame;
use crate::session::NetSession;

/// How the seats' lockstep payloads reach the host: the game's transport (a `NetServer` with
/// `lockstep::net`, a relay, in-process seats).
pub trait LockstepSeatLink<G: NetGame> {
    /// Seats that connected (`true`) or went away (`false`) since the last call.
    fn membership(&mut self) -> Vec<(SeatId, bool)> {
        Vec::new()
    }
    /// What the seats sent since the last call, oldest first.
    fn receive(&mut self, now_ms: u64) -> Vec<(SeatId, Lane, Vec<u8>)>;
    /// Hands one payload to `to`'s connection.
    fn send(&mut self, to: SeatId, out: Outgoing, now_ms: u64);
    /// The host removed `seat` from the match for hostile traffic (ADR-8734): its malformed score
    /// reached the limit. `code` is the refusal that took it there (`lockstep_malformed`,
    /// `lockstep_input_invalid`, `lockstep_input_too_large`). Drop the seat's connection (a
    /// `NetServer` kicks its peer with `DisconnectReason::MalformedPacket`). The default does
    /// nothing; the seat is out of the match either way.
    fn refuse(&mut self, seat: SeatId, code: &'static str) {
        let _ = (seat, code);
    }
    /// The game's bytes of the majority seat's state after the frame at `tick`, once that seat
    /// sent it (it got [`crate::lockstep::SeatEvent::ProvideState`] for the observer): the
    /// outvoted observer takes it ([`LockstepServer::load_observer_state`], ADR-8855). Asked
    /// every step while a transfer is pending. The default has none, and the transfer is ordered
    /// again at its deadline.
    fn observer_state(&mut self, tick: u64) -> Option<Vec<u8>> {
        let _ = tick;
        None
    }
}

/// The dedicated server's lockstep game: a session (admin, bans, joins) plus a [`LockstepServer`].
pub struct LockstepHostGame<G: NetGame> {
    /// The session the host drives.
    pub session: NetSession,
    /// The lockstep match.
    pub server: LockstepServer<G>,
    /// The seats' transport (`None`: nobody can join yet).
    pub link: Option<Box<dyn LockstepSeatLink<G>>>,
    /// The session clock follows this host clock (milliseconds) when set.
    pub clock: Option<TestClock>,
    /// Host events the game has not read yet (the newest 256).
    pub events: std::collections::VecDeque<HostEvent>,
    /// Seats the link offered while the match was full.
    pub seats_refused: u64,
    /// Payloads from a seat that did not decode (counted; one is never fatal, a seat that keeps
    /// sending them is removed, ADR-8734).
    pub payloads_refused: u64,
    /// Seats removed because their malformed score reached the limit (ADR-8734).
    pub seats_removed: u64,
    /// The seats' malformed scores (ADR-8734).
    scores: SeatScores,
    /// Seats removed at the score limit whose leave the link has not reported yet: what they
    /// still send is refused unread (the host keeps a leaving seat until its leave frame).
    removed: std::collections::BTreeSet<SeatId>,
    started: bool,
}

/// Host events a [`LockstepHostGame`] keeps for the game.
const MAX_KEPT_EVENTS: usize = 256;

impl<G: NetGame> std::fmt::Debug for LockstepHostGame<G> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockstepHostGame")
            .field("server", &self.server)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl<G: NetGame> LockstepHostGame<G> {
    /// A lockstep game for `[lockstep]` at `tick_hz`. With `observer = true` the game gives the
    /// simulation the observer steps.
    pub fn new(
        session: NetSession,
        settings: &LockstepServerSettings,
        tick_hz: u32,
        observer: Option<Box<dyn LockstepObserver<G::Input>>>,
    ) -> Result<Self, LockstepServerError> {
        Ok(Self {
            session,
            server: LockstepServer::new(settings, tick_hz, observer)?,
            link: None,
            clock: None,
            events: std::collections::VecDeque::new(),
            seats_refused: 0,
            payloads_refused: 0,
            seats_removed: 0,
            scores: SeatScores::default(),
            removed: std::collections::BTreeSet::new(),
            started: false,
        })
    }

    /// The status rows at the session's clock.
    pub fn status(&mut self) -> LockstepServerStatus {
        let now = self.session.now_ms();
        self.server.status(now)
    }
}

impl<G: NetGame> HostGame for LockstepHostGame<G> {
    fn session(&mut self) -> &mut NetSession {
        &mut self.session
    }

    fn step(&mut self) {
        let _span = tracing::trace_span!("lockstep_host_game_step").entered();
        if let Some(clock) = &self.clock {
            self.session.set_manual_clock(clock.now().as_millis() as u64);
        }
        self.session.update();
        let now = self.session.now_ms();
        if !self.started {
            self.server.start(now);
            self.started = true;
        }
        if let Some(link) = self.link.as_mut() {
            for (seat, joined) in link.membership() {
                self.removed.remove(&seat);
                if !joined {
                    self.server.remove_seat(seat);
                    self.scores.remove(seat);
                } else if let Err(e) = self.server.add_seat(seat) {
                    self.seats_refused += 1;
                    tracing::info!(%seat, code = e.code(), "lockstep host: seat refused");
                }
            }
            for (seat, lane, bytes) in link.receive(now) {
                if self.removed.contains(&seat) {
                    self.payloads_refused += 1;
                    continue;
                }
                if let Err(e) = self.server.receive(seat, lane, &bytes, now) {
                    self.payloads_refused += 1;
                    tracing::debug!(%seat, code = e.code(), "lockstep host: payload refused");
                    // The seat's own connection delivered it: the transport's malformed rule
                    // (ADR-8606), so a seat that keeps sending them leaves (ADR-8734).
                    if self.scores.charge(seat, &e, now) {
                        self.scores.remove(seat);
                        self.removed.insert(seat);
                        self.server.remove_seat(seat);
                        self.seats_removed += 1;
                        tracing::info!(%seat, code = e.code(), "lockstep host: seat removed, malformed score at the limit");
                        link.refuse(seat, e.code());
                    }
                }
            }
        }
        self.server.tick(now);
        if let Some(tick) = self.server.observer_transfer()
            && let Some(state) = self.link.as_mut().and_then(|link| link.observer_state(tick))
            && let Err(e) = self.server.load_observer_state(tick, state)
        {
            tracing::info!(tick, code = e.code(), "lockstep host: observer state refused");
        }
        let out = self.server.outgoing(now);
        if let Some(link) = self.link.as_mut() {
            for (to, payload) in out {
                link.send(to, payload, now);
            }
        }
        while let Some(event) = self.server.poll_event() {
            if self.events.len() >= MAX_KEPT_EVENTS {
                self.events.pop_front();
            }
            self.events.push_back(event);
        }
    }

    fn match_over(&mut self) -> bool {
        self.server.host().ended_at().is_some()
    }

    fn lockstep_status(&mut self) -> Option<LockstepServerStatus> {
        Some(self.status())
    }

    fn console_command(&mut self, cmd: &str, _rest: &str) -> Option<Vec<String>> {
        (cmd == "lockstep").then(|| self.status().console_lines())
    }

    fn console_help(&mut self) -> Vec<String> {
        vec!["lockstep                        seats, delay, observer lag, verdicts".to_owned()]
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::lockstep::server::LockstepObserver;
    use crate::lockstep::{LockstepFrame, LockstepSeat};
    use crate::server_host::config::CliOverrides;
    use crate::server_host::{HostTransport, ServerHost};

    struct Game;
    impl NetGame for Game {
        type Message = Vec<u8>;
        type Input = u8;
        type JoinData = ();
        const GAME_ID: &'static str = "test.lockstep.host";
        const PROTOCOL_VERSION: u32 = 1;
    }

    #[derive(Default)]
    struct Sum(u64);
    impl LockstepObserver<u8> for Sum {
        fn step(&mut self, frame: &LockstepFrame<u8>) {
            for (seat, input) in &frame.inputs {
                self.0 = self.0.wrapping_mul(31).wrapping_add(u64::from(seat.0) + 1);
                self.0 = self.0.wrapping_add(input.as_ref().map_or(0, |i| u64::from(*i)));
            }
        }
        fn state_hash(&self, tick: u64) -> u64 {
            self.0 ^ tick.rotate_left(17)
        }
    }

    /// In-process seats behind a shared queue: the link half the host holds.
    #[derive(Default)]
    struct Wire {
        joined: Vec<SeatId>,
        up: VecDeque<(SeatId, Lane, Vec<u8>)>,
        down: VecDeque<(SeatId, Outgoing)>,
        refused: Vec<(SeatId, &'static str)>,
    }
    struct Link(Arc<Mutex<Wire>>);
    impl LockstepSeatLink<Game> for Link {
        fn membership(&mut self) -> Vec<(SeatId, bool)> {
            let mut w = self.0.lock().expect("wire");
            w.joined.drain(..).map(|s| (s, true)).collect()
        }
        fn receive(&mut self, _now_ms: u64) -> Vec<(SeatId, Lane, Vec<u8>)> {
            self.0.lock().expect("wire").up.drain(..).collect()
        }
        fn send(&mut self, to: SeatId, out: Outgoing, _now_ms: u64) {
            self.0.lock().expect("wire").down.push_back((to, out));
        }
        fn refuse(&mut self, seat: SeatId, code: &'static str) {
            self.0.lock().expect("wire").refused.push((seat, code));
        }
    }

    fn host_dir() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-lockstep-host-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let text = "format_version = 4\n[server]\nmax_players = 8\ntick_hz = 60\n[levels]\nrotation = [\"levels/a.level.ron\"]\n[status]\nenabled = false\n[lockstep]\nenabled = true\nmax_seats = 3\n";
        std::fs::write(dir.join("server.toml.tmp"), text).expect("write");
        std::fs::rename(dir.join("server.toml.tmp"), dir.join("server.toml")).expect("rename");
        dir
    }

    /// The server host runs the lockstep game from `server.toml`: three seats in, the fourth
    /// refused, frames built on the host's test clock, the observer voting, the status rows in
    /// the health JSON and on the console.
    #[test]
    fn the_host_runs_a_lockstep_match_from_server_toml() {
        let dir = host_dir();
        let clock = TestClock::new();
        let mut host = ServerHost::new(
            &dir,
            CliOverrides::default(),
            Box::new(|_| None),
            Box::new(clock.clone()),
        )
        .expect("config");
        let settings = host.settings().clone();
        assert!(settings.lockstep.enabled);
        let mut game = LockstepHostGame::<Game>::new(
            NetSession::default(),
            &settings.lockstep,
            settings.tick_hz,
            Some(Box::new(Sum::default())),
        )
        .expect("game");
        game.clock = Some(clock.clone());
        let wire = Arc::new(Mutex::new(Wire::default()));
        game.link = Some(Box::new(Link(Arc::clone(&wire))));
        host.start(&mut game, HostTransport::Local).expect("start");

        let config = game.server.config().clone();
        let mut seats: Vec<LockstepSeat<Game>> = (0..3)
            .map(|n| LockstepSeat::new(config.clone(), SeatId(n)))
            .collect();
        let mut sims: Vec<Sum> = (0..3).map(|_| Sum::default()).collect();
        wire.lock()
            .expect("wire")
            .joined
            .extend([SeatId(0), SeatId(1), SeatId(2), SeatId(3)]);
        let period = Duration::from_micros(16_667);
        for _ in 0..300 {
            let now = clock.now().as_millis() as u64;
            for (i, seat) in seats.iter_mut().enumerate() {
                let _ = seat.submit_input(&(i as u8 + 1));
                let mut w = wire.lock().expect("wire");
                for out in seat.outgoing(now) {
                    w.up.push_back((SeatId(i as u16), out.lane, out.bytes));
                }
            }
            clock.advance(period);
            assert_eq!(host.run_once(&mut game), None);
            game.server.sync_observer();
            let down: Vec<_> = wire.lock().expect("wire").down.drain(..).collect();
            for (to, out) in down {
                if let Some(seat) = seats.get_mut(usize::from(to.0)) {
                    seat.receive(out.lane, &out.bytes, now).expect("frames");
                }
            }
            for (seat, sim) in seats.iter_mut().zip(sims.iter_mut()) {
                while let Some(frame) = seat.next_frame() {
                    sim.step(&frame);
                    seat.report_hash(frame.tick, sim.state_hash(frame.tick));
                }
            }
        }
        assert_eq!(game.seats_refused, 1, "max_seats = 3 refuses the fourth");
        let status = game.status();
        assert_eq!((status.seats, status.max_seats), (3, 3));
        assert!(status.frames_built >= 250, "{}", status.frames_built);
        assert!(status.lockstep.checkpoints_agreed >= 6, "{status:?}");
        assert_eq!(status.verdicts, 0);
        let obs = status.observer.as_ref().expect("observer");
        assert!(obs.voting && obs.frames_stepped >= 250, "{obs:?}");

        let health = host.health_json(&mut game);
        assert!(health.contains("\"lockstep\":{\"tick\":"), "{health}");
        assert!(health.contains("\"observer\":{\"voting\":true"), "{health}");
        let lines = host.execute(&mut game, "lockstep");
        assert!(lines[0].starts_with("lockstep tick="), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("observer ")), "{lines:?}");
        drop(host);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// v06-security-fixes-verification-c, next to ADR-8666's runtime voice fix: a lockstep
    /// payload that does not decode is the seat's own (it came over the seat's authenticated
    /// connection), yet `step` only counts it in `payloads_refused`. Nothing scores it and the
    /// link cannot be told, so a seat can send undecodable payloads for the whole match. ADR-8606's
    /// rule (16 points a refusal, a leak of 4 a second, out at 1,024) would remove this seat within
    /// about a second. Asserts the safe property: after five seconds of eight undecodable datagrams
    /// per tick, the seat is no longer in the match. Fails while every one is only counted.
    #[test]
    fn a_seat_that_keeps_sending_undecodable_payloads_is_removed() {
        let dir = host_dir();
        let clock = TestClock::new();
        let mut host = ServerHost::new(
            &dir,
            CliOverrides::default(),
            Box::new(|_| None),
            Box::new(clock.clone()),
        )
        .expect("config");
        let settings = host.settings().clone();
        let mut game = LockstepHostGame::<Game>::new(
            NetSession::default(),
            &settings.lockstep,
            settings.tick_hz,
            Some(Box::new(Sum::default())),
        )
        .expect("game");
        game.clock = Some(clock.clone());
        let wire = Arc::new(Mutex::new(Wire::default()));
        game.link = Some(Box::new(Link(Arc::clone(&wire))));
        host.start(&mut game, HostTransport::Local).expect("start");
        wire.lock().expect("wire").joined.extend([SeatId(0), SeatId(1)]);
        let period = Duration::from_micros(16_667);
        for _ in 0..300 {
            {
                let mut w = wire.lock().expect("wire");
                for _ in 0..8 {
                    // No lockstep payload kind: refused as `lockstep_malformed`.
                    w.up.push_back((SeatId(1), Lane::Datagram, vec![0xEE; 12]));
                }
            }
            clock.advance(period);
            assert_eq!(host.run_once(&mut game), None);
            wire.lock().expect("wire").down.clear();
        }
        assert!(
            game.payloads_refused >= 64,
            "setup: the junk was refused ({} refusals)",
            game.payloads_refused
        );
        let status = game.status();
        assert_eq!(
            status.seats, 1,
            "a seat that sent {} undecodable payloads in five seconds is still in the match ({status:?})",
            game.payloads_refused
        );
        drop(host);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// ADR-8734: a seat that sends one undecodable payload a second stays in the match (the score
    /// leaks faster than it grows), while seat 1's burst removes it, tells the link once
    /// (`lockstep_malformed`), and leaves the other seat playing.
    #[test]
    fn an_occasional_bad_payload_is_not_fatal_and_a_burst_removes_only_that_seat() {
        let dir = host_dir();
        let clock = TestClock::new();
        let mut host = ServerHost::new(
            &dir,
            CliOverrides::default(),
            Box::new(|_| None),
            Box::new(clock.clone()),
        )
        .expect("config");
        let settings = host.settings().clone();
        let mut game = LockstepHostGame::<Game>::new(
            NetSession::default(),
            &settings.lockstep,
            settings.tick_hz,
            Some(Box::new(Sum::default())),
        )
        .expect("game");
        game.clock = Some(clock.clone());
        let wire = Arc::new(Mutex::new(Wire::default()));
        game.link = Some(Box::new(Link(Arc::clone(&wire))));
        host.start(&mut game, HostTransport::Local).expect("start");
        wire.lock().expect("wire").joined.extend([SeatId(0), SeatId(1)]);
        let period = Duration::from_micros(16_667);
        // Ten seconds: seat 0 sends one junk payload every 60 ticks (one a second).
        for step in 0..600u32 {
            if step.is_multiple_of(60) {
                wire.lock()
                    .expect("wire")
                    .up
                    .push_back((SeatId(0), Lane::Datagram, vec![0xEE; 12]));
            }
            clock.advance(period);
            assert_eq!(host.run_once(&mut game), None);
            wire.lock().expect("wire").down.clear();
        }
        assert_eq!(game.payloads_refused, 10);
        assert_eq!(game.status().seats, 2, "one bad payload a second is not fatal");
        assert_eq!(game.seats_removed, 0);
        // Seat 1: 70 at one instant. The 64th reaches 1,024 points.
        {
            let mut w = wire.lock().expect("wire");
            for _ in 0..70 {
                w.up.push_back((SeatId(1), Lane::Datagram, vec![0xEE; 12]));
            }
        }
        clock.advance(period);
        assert_eq!(host.run_once(&mut game), None);
        assert_eq!(game.seats_removed, 1);
        assert_eq!(game.payloads_refused, 80, "the rest is refused unread");
        assert_eq!(
            wire.lock().expect("wire").refused,
            [(SeatId(1), "lockstep_malformed")],
            "the link is told once"
        );
        assert_eq!(game.status().seats, 1, "seat 0 plays on");
        drop(host);
        let _ = std::fs::remove_dir_all(dir);
    }
}
