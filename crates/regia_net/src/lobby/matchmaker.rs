// SPDX-License-Identifier: MPL-2.0
//! Matchmaking (spec 89 § 5): a pure state machine over a [`LobbyBackend`], stepped once per
//! frame with the backend's events. No clock and no randomness: waits are counted in steps, so the
//! same events in the same order give the same transitions and the same chosen lobby.

use std::collections::VecDeque;

use super::{
    Distance, LobbyBackend, LobbyError, LobbyEvent, LobbyId, LobbyQuery, LobbyServer, LobbySpec, RequestId,
};

/// What to look for and what to do when nothing fits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchmakingRequest {
    /// The game's filters (the engine compatibility filters are added by `super::Lobbies`).
    pub query: LobbyQuery,
    /// One list round per entry, widening the distance; empty means one `Default` round.
    pub rounds: Vec<Distance>,
    /// Create a lobby when every round came back empty.
    pub create_if_none: bool,
    /// The lobby to create (engine keys added by `super::Lobbies`).
    pub create: LobbySpec,
    /// Members (owner included) needed before [`Matchmaker::ready_to_launch`].
    pub min_players: u32,
    /// Steps to wait for a platform answer before treating the request as failed.
    pub step_timeout: u32,
}

impl Default for MatchmakingRequest {
    fn default() -> Self {
        Self {
            query: LobbyQuery::default(),
            rounds: vec![
                Distance::Close,
                Distance::Default,
                Distance::Far,
                Distance::Worldwide,
            ],
            create_if_none: true,
            create: LobbySpec::default(),
            min_players: 2,
            step_timeout: 600,
        }
    }
}

/// Why matchmaking stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchFailure {
    /// Every round was empty and `create_if_none` was off.
    NoMatch,
    /// Creating the fallback lobby failed.
    CreateFailed(LobbyError),
    /// The platform never answered a create.
    RequestTimeout,
    /// We were removed from the lobby (or it vanished) before launch.
    Left,
    /// No lobby platform.
    Unavailable,
}

impl MatchFailure {
    /// The stable code (spec 89 § 9).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoMatch => "no_match",
            Self::CreateFailed(_) => "create_failed",
            Self::RequestTimeout => "request_timeout",
            Self::Left => "left",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The matchmaker's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchState {
    /// Not matchmaking.
    Idle,
    /// Waiting for a list result.
    Listing {
        /// The pending request.
        request: RequestId,
    },
    /// Waiting for a join result.
    Joining {
        /// The lobby being joined.
        lobby: LobbyId,
        /// The pending request.
        request: RequestId,
    },
    /// Waiting for the fallback lobby.
    Creating {
        /// The pending request.
        request: RequestId,
    },
    /// In a lobby, before launch.
    InLobby {
        /// The lobby.
        lobby: LobbyId,
        /// Whether we own it.
        owner: bool,
    },
    /// The owner launched: connect to `server`.
    Launched {
        /// The lobby.
        lobby: LobbyId,
        /// Where the session is.
        server: LobbyServer,
    },
    /// Stopped.
    Failed(MatchFailure),
}

impl MatchState {
    /// The stable state name (spec 89 § 9).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Listing { .. } => "listing",
            Self::Joining { .. } => "joining",
            Self::Creating { .. } => "creating",
            Self::InLobby { .. } => "in_lobby",
            Self::Launched { .. } => "launched",
            Self::Failed(_) => "failed",
        }
    }

    /// The lobby we are in, if any.
    pub fn lobby(&self) -> Option<LobbyId> {
        match self {
            Self::InLobby { lobby, .. } | Self::Launched { lobby, .. } => Some(*lobby),
            _ => None,
        }
    }

    /// Whether a request is pending (listing, joining or creating).
    pub fn is_searching(&self) -> bool {
        matches!(
            self,
            Self::Listing { .. } | Self::Joining { .. } | Self::Creating { .. }
        )
    }
}

/// The matchmaking state machine (spec 89 § 5).
#[derive(Debug, Clone)]
pub struct Matchmaker {
    request: MatchmakingRequest,
    state: MatchState,
    round: usize,
    candidates: VecDeque<LobbyId>,
    waited: u32,
    abandoned: VecDeque<RequestId>,
}

/// Create and join requests given up on (cancel, timeout) that [`Matchmaker`] remembers, so a
/// lobby that lands for one later is left at once instead of lingering with us in it.
pub const MAX_ABANDONED: usize = 32;

impl Default for Matchmaker {
    fn default() -> Self {
        Self {
            request: MatchmakingRequest::default(),
            state: MatchState::Idle,
            round: 0,
            candidates: VecDeque::new(),
            waited: 0,
            abandoned: VecDeque::new(),
        }
    }
}

impl Matchmaker {
    /// The current state.
    pub fn state(&self) -> &MatchState {
        &self.state
    }

    /// The round index (0-based) of the current or last list.
    pub fn round(&self) -> usize {
        self.round
    }

    /// Lobbies left to try from the last list.
    pub fn candidates(&self) -> Vec<LobbyId> {
        self.candidates.iter().copied().collect()
    }

    /// The request in force.
    pub fn request(&self) -> &MatchmakingRequest {
        &self.request
    }

    fn rounds(&self) -> Vec<Distance> {
        if self.request.rounds.is_empty() {
            vec![Distance::Default]
        } else {
            self.request.rounds.clone()
        }
    }

    /// Starts (or restarts) matchmaking: the first list request goes out now.
    pub fn start(&mut self, backend: &dyn LobbyBackend, request: MatchmakingRequest) {
        self.request = request;
        self.round = 0;
        self.candidates.clear();
        self.list(backend);
    }

    /// Stops matchmaking and leaves the lobby it joined or created. A create or join still in
    /// flight is abandoned: the lobby it lands in is left when its event arrives.
    pub fn cancel(&mut self, backend: &dyn LobbyBackend) {
        if let Some(lobby) = self.state.lobby() {
            backend.leave(lobby);
        }
        match self.state {
            MatchState::Joining { lobby, request } => {
                // Leaving now keeps us out where the platform joins synchronously.
                backend.leave(lobby);
                self.abandon(request);
            }
            MatchState::Creating { request } => self.abandon(request),
            _ => {}
        }
        self.state = MatchState::Idle;
        self.candidates.clear();
    }

    /// Remembers `request` (a create or join) as given up on; see [`MAX_ABANDONED`].
    pub fn abandon(&mut self, request: RequestId) {
        if self.abandoned.len() >= MAX_ABANDONED {
            self.abandoned.pop_front();
        }
        self.abandoned.push_back(request);
    }

    /// Requests given up on whose answer has not arrived yet, oldest first.
    pub fn abandoned(&self) -> Vec<RequestId> {
        self.abandoned.iter().copied().collect()
    }

    /// Leaves every lobby that `events` show landing for an abandoned request and forgets the
    /// request. [`Matchmaker::step`] calls it; call it yourself on frames you do not step.
    pub fn settle_abandoned(&mut self, backend: &dyn LobbyBackend, events: &[LobbyEvent]) {
        if self.abandoned.is_empty() {
            return;
        }
        for event in events {
            let (LobbyEvent::Created { request, result } | LobbyEvent::Entered { request, result }) = event
            else {
                continue;
            };
            let Some(at) = self.abandoned.iter().position(|r| r == request) else {
                continue;
            };
            self.abandoned.remove(at);
            if let Ok(lobby) = result {
                backend.leave(*lobby);
            }
        }
    }

    /// Forgets every abandoned request (the backend they belong to is gone; request ids are per
    /// backend).
    pub fn forget_abandoned(&mut self) {
        self.abandoned.clear();
    }

    /// The lobby we were in vanished without a `Left` event: the same as being removed from it.
    pub fn lobby_lost(&mut self) {
        if self.state.lobby().is_some() {
            self.state = MatchState::Failed(MatchFailure::Left);
            self.candidates.clear();
        }
    }

    /// Forgets the state without touching the backend (the lobby was left another way).
    pub fn reset(&mut self) {
        self.state = MatchState::Idle;
        self.candidates.clear();
    }

    fn list(&mut self, backend: &dyn LobbyBackend) {
        self.waited = 0;
        let rounds = self.rounds();
        let mut query = self.request.query.clone();
        query.distance = rounds[self.round.min(rounds.len() - 1)];
        self.state = match backend.request_list(&query) {
            Ok(request) => MatchState::Listing { request },
            Err(LobbyError::Unavailable) => MatchState::Failed(MatchFailure::Unavailable),
            // A refused query cannot get better by widening: treat it as no match.
            Err(_) => MatchState::Failed(MatchFailure::NoMatch),
        };
    }

    fn next_round(&mut self, backend: &dyn LobbyBackend) {
        self.candidates.clear();
        if self.round + 1 < self.rounds().len() {
            self.round += 1;
            self.list(backend);
        } else if self.request.create_if_none {
            self.waited = 0;
            self.state = match backend.create(&self.request.create) {
                Ok(request) => MatchState::Creating { request },
                Err(LobbyError::Unavailable) => MatchState::Failed(MatchFailure::Unavailable),
                Err(e) => MatchState::Failed(MatchFailure::CreateFailed(e)),
            };
        } else {
            self.state = MatchState::Failed(MatchFailure::NoMatch);
        }
    }

    fn next_candidate(&mut self, backend: &dyn LobbyBackend) {
        while let Some(lobby) = self.candidates.pop_front() {
            self.waited = 0;
            match backend.join(lobby) {
                Ok(request) => {
                    self.state = MatchState::Joining { lobby, request };
                    return;
                }
                Err(LobbyError::Unavailable) => {
                    self.state = MatchState::Failed(MatchFailure::Unavailable);
                    return;
                }
                Err(_) => continue,
            }
        }
        self.next_round(backend);
    }

    fn entered(&mut self, backend: &dyn LobbyBackend, lobby: LobbyId, owner: bool) {
        // A lobby that launched before we arrived (joinable in game): go straight to launched.
        self.state = match backend.lobby(lobby).and_then(|l| l.server) {
            Some(server) if !owner => MatchState::Launched { lobby, server },
            _ => MatchState::InLobby { lobby, owner },
        };
    }

    /// Advances with this frame's events (call once per frame, even with no events: waits are
    /// counted in steps).
    pub fn step(&mut self, backend: &dyn LobbyBackend, events: &[LobbyEvent]) {
        self.settle_abandoned(backend, events);
        let me = backend.local_member();
        for event in events {
            let state = self.state.clone();
            match (&state, event) {
                (MatchState::Listing { request }, LobbyEvent::List { request: r, result })
                    if r == request =>
                {
                    match result {
                        Ok(list) if !list.is_empty() => {
                            self.candidates = list.iter().map(|l| l.id).collect();
                            self.next_candidate(backend);
                        }
                        _ => self.next_round(backend),
                    }
                }
                (MatchState::Joining { request, .. }, LobbyEvent::Entered { request: r, result })
                    if r == request =>
                {
                    match result {
                        Ok(lobby) => self.entered(backend, *lobby, false),
                        Err(_) => self.next_candidate(backend),
                    }
                }
                (MatchState::Creating { request }, LobbyEvent::Created { request: r, result })
                    if r == request =>
                {
                    self.state = match result {
                        Ok(lobby) => MatchState::InLobby {
                            lobby: *lobby,
                            owner: true,
                        },
                        Err(e) => MatchState::Failed(MatchFailure::CreateFailed(e.clone())),
                    };
                }
                (MatchState::InLobby { lobby, .. }, LobbyEvent::ServerSet { lobby: l, server })
                    if l == lobby =>
                {
                    self.state = MatchState::Launched {
                        lobby: *lobby,
                        server: *server,
                    };
                }
                (MatchState::InLobby { lobby, .. }, LobbyEvent::OwnerChanged { lobby: l, owner })
                    if l == lobby =>
                {
                    self.state = MatchState::InLobby {
                        lobby: *lobby,
                        owner: Some(*owner) == me,
                    };
                }
                (
                    MatchState::InLobby { lobby, .. } | MatchState::Launched { lobby, .. },
                    LobbyEvent::Left { lobby: l, .. },
                ) if l == lobby => {
                    self.state = MatchState::Failed(MatchFailure::Left);
                }
                _ => {}
            }
        }
        if self.state.is_searching() {
            self.waited += 1;
            if self.waited > self.request.step_timeout {
                match self.state {
                    MatchState::Listing { .. } => self.next_round(backend),
                    MatchState::Joining { lobby, request } => {
                        backend.leave(lobby);
                        self.abandon(request);
                        self.next_candidate(backend);
                    }
                    MatchState::Creating { request } => {
                        self.abandon(request);
                        self.state = MatchState::Failed(MatchFailure::RequestTimeout);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Whether the owner may launch: we own the lobby, at least `min_players` members are in and
    /// every member is ready.
    pub fn ready_to_launch(&self, backend: &dyn LobbyBackend) -> bool {
        let MatchState::InLobby { lobby, owner: true } = self.state else {
            return false;
        };
        backend
            .lobby(lobby)
            .is_some_and(|l| l.member_count >= self.request.min_players.max(1) && l.all_ready())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lobby::{Compare, LocalLobbyHub, MemberId, keys};

    fn run(mm: &mut Matchmaker, backend: &dyn LobbyBackend, steps: usize) {
        for _ in 0..steps {
            let events = backend.poll();
            mm.step(backend, &events);
        }
    }

    fn open_lobby(hub: &LocalLobbyHub, owner: u64, mode: &str, max: u32) -> LobbyId {
        let b = hub.member(owner, "host");
        let mut spec = LobbySpec {
            max_members: max,
            ..LobbySpec::default()
        };
        spec.data.insert("mode".into(), mode.into());
        b.create(&spec).expect("create");
        match b.poll().first() {
            Some(LobbyEvent::Created { result: Ok(id), .. }) => *id,
            other => panic!("{other:?}"),
        }
    }

    fn duel() -> MatchmakingRequest {
        MatchmakingRequest {
            query: LobbyQuery::default().with_string("mode", Compare::Eq, "duel"),
            ..MatchmakingRequest::default()
        }
    }

    #[test]
    fn joins_the_best_match_and_skips_full_ones() {
        let hub = LocalLobbyHub::new();
        let full = open_lobby(&hub, 10, "duel", 1);
        let _other_mode = open_lobby(&hub, 11, "ffa", 4);
        let good = open_lobby(&hub, 12, "duel", 2);
        let me = hub.member(1, "me");
        let mut mm = Matchmaker::default();
        mm.start(&me, duel());
        assert_eq!(mm.state().name(), "listing");
        run(&mut mm, &me, 3);
        assert_eq!(
            *mm.state(),
            MatchState::InLobby {
                lobby: good,
                owner: false
            }
        );
        assert!(me.lobby(full).is_none(), "the full lobby was never listed");
        assert!(!mm.ready_to_launch(&me), "only owners launch");
    }

    #[test]
    fn a_join_refused_moves_to_the_next_candidate() {
        let hub = LocalLobbyHub::new();
        let first = open_lobby(&hub, 10, "duel", 2);
        let second = open_lobby(&hub, 11, "duel", 2);
        let me = hub.member(1, "me");
        let rival = hub.member(2, "rival");
        let mut mm = Matchmaker::default();
        mm.start(&me, duel());
        // The list comes back with both; before our join lands a rival takes the first slot.
        let list = me.poll();
        rival.join(first).expect("rival joins");
        mm.step(&me, &list);
        assert!(matches!(mm.state(), MatchState::Joining { lobby, .. } if *lobby == first));
        run(&mut mm, &me, 2);
        assert_eq!(
            *mm.state(),
            MatchState::InLobby {
                lobby: second,
                owner: false
            }
        );
    }

    #[test]
    fn widens_then_creates_and_launches_when_ready() {
        let hub = LocalLobbyHub::new();
        let me = hub.member(1, "me");
        let mut mm = Matchmaker::default();
        mm.start(&me, duel());
        run(&mut mm, &me, 4);
        assert_eq!(mm.round(), 3, "four rounds: close, default, far, worldwide");
        assert_eq!(mm.state().name(), "creating");
        run(&mut mm, &me, 1);
        let MatchState::InLobby { lobby, owner: true } = *mm.state() else {
            panic!("{:?}", mm.state());
        };
        // A second player matchmakes into the created lobby (the create spec has no `mode`; use
        // an empty query).
        let friend = hub.member(2, "friend");
        let mut fm = Matchmaker::default();
        fm.start(&friend, MatchmakingRequest::default());
        run(&mut fm, &friend, 2);
        assert_eq!(fm.state().lobby(), Some(lobby));
        run(&mut mm, &me, 1);
        assert!(!mm.ready_to_launch(&me), "nobody is ready");
        me.set_member_data(lobby, keys::READY, Some("1")).expect("ready");
        friend
            .set_member_data(lobby, keys::READY, Some("1"))
            .expect("ready");
        assert!(mm.ready_to_launch(&me));
        let server = LobbyServer::Steam(1);
        me.set_server(lobby, server).expect("launch");
        run(&mut mm, &me, 1);
        run(&mut fm, &friend, 1);
        assert_eq!(*mm.state(), MatchState::Launched { lobby, server });
        assert_eq!(*fm.state(), MatchState::Launched { lobby, server });
    }

    #[test]
    fn no_match_without_create_and_timeouts_by_steps() {
        let hub = LocalLobbyHub::new();
        let me = hub.member(1, "me");
        let mut mm = Matchmaker::default();
        mm.start(
            &me,
            MatchmakingRequest {
                create_if_none: false,
                rounds: vec![Distance::Close],
                ..duel()
            },
        );
        run(&mut mm, &me, 2);
        assert_eq!(*mm.state(), MatchState::Failed(MatchFailure::NoMatch));

        // A backend that never answers: each list round times out after `step_timeout` steps,
        // then the create times out too.
        struct Silent(LocalLobbyHub);
        impl LobbyBackend for Silent {
            fn name(&self) -> &'static str {
                "silent"
            }
            fn local_member(&self) -> Option<MemberId> {
                Some(MemberId(5))
            }
            fn create(&self, _: &LobbySpec) -> Result<RequestId, LobbyError> {
                Ok(RequestId(1))
            }
            fn join(&self, _: LobbyId) -> Result<RequestId, LobbyError> {
                Ok(RequestId(2))
            }
            fn leave(&self, _: LobbyId) {}
            fn request_list(&self, _: &LobbyQuery) -> Result<RequestId, LobbyError> {
                Ok(RequestId(3))
            }
            fn lobby(&self, _: LobbyId) -> Option<crate::lobby::LobbyInfo> {
                let _ = &self.0;
                None
            }
            fn set_data(&self, _: LobbyId, _: &str, _: Option<&str>) -> Result<(), LobbyError> {
                Ok(())
            }
            fn set_member_data(&self, _: LobbyId, _: &str, _: Option<&str>) -> Result<(), LobbyError> {
                Ok(())
            }
            fn set_joinable(&self, _: LobbyId, _: bool) -> Result<(), LobbyError> {
                Ok(())
            }
            fn set_server(&self, _: LobbyId, _: LobbyServer) -> Result<(), LobbyError> {
                Ok(())
            }
            fn send_chat(&self, _: LobbyId, _: &str) -> Result<(), LobbyError> {
                Ok(())
            }
            fn poll(&self) -> Vec<LobbyEvent> {
                Vec::new()
            }
        }
        let silent = Silent(LocalLobbyHub::new());
        let mut mm = Matchmaker::default();
        mm.start(
            &silent,
            MatchmakingRequest {
                rounds: vec![Distance::Close, Distance::Far],
                step_timeout: 3,
                ..MatchmakingRequest::default()
            },
        );
        run(&mut mm, &silent, 4);
        assert_eq!(mm.round(), 1, "the first round timed out after 3 steps");
        run(&mut mm, &silent, 4);
        assert_eq!(mm.state().name(), "creating");
        run(&mut mm, &silent, 4);
        assert_eq!(*mm.state(), MatchState::Failed(MatchFailure::RequestTimeout));
        assert_eq!(MatchFailure::RequestTimeout.code(), "request_timeout");
        // The timed-out create is remembered until its late answer arrives.
        assert_eq!(mm.abandoned(), vec![RequestId(1)]);
        let late = LobbyEvent::Created {
            request: RequestId(1),
            result: Ok(LobbyId(77)),
        };
        mm.step(&silent, &[late]);
        assert!(mm.abandoned().is_empty());
    }

    #[test]
    fn a_create_cancelled_in_flight_does_not_leave_an_orphan_lobby() {
        let hub = LocalLobbyHub::new();
        let me = hub.member(1, "me");
        let mut mm = Matchmaker::default();
        mm.start(
            &me,
            MatchmakingRequest {
                rounds: vec![Distance::Close],
                ..duel()
            },
        );
        // The empty list arrives and the fallback create goes out; the local hub makes the lobby
        // now and answers on the next poll.
        run(&mut mm, &me, 1);
        assert_eq!(mm.state().name(), "creating");
        assert_eq!(hub.lobbies().len(), 1);
        mm.cancel(&me);
        assert_eq!(*mm.state(), MatchState::Idle);
        run(&mut mm, &me, 1);
        assert!(
            hub.lobbies().is_empty(),
            "the lobby that landed after cancel was left"
        );
        assert!(mm.abandoned().is_empty());

        // The bound: the oldest abandoned request is forgotten first.
        for i in 0..(MAX_ABANDONED as u64 + 2) {
            mm.abandon(RequestId(1000 + i));
        }
        assert_eq!(mm.abandoned().len(), MAX_ABANDONED);
        assert_eq!(mm.abandoned()[0], RequestId(1002));
    }

    #[test]
    fn cancel_leaves_and_kicks_fail() {
        let hub = LocalLobbyHub::new();
        let lobby = open_lobby(&hub, 10, "duel", 4);
        let me = hub.member(1, "me");
        let mut mm = Matchmaker::default();
        mm.start(&me, duel());
        run(&mut mm, &me, 2);
        assert_eq!(mm.state().lobby(), Some(lobby));
        mm.cancel(&me);
        assert_eq!(*mm.state(), MatchState::Idle);
        assert!(me.lobby(lobby).is_none(), "cancel left the lobby");
        mm.start(&me, duel());
        run(&mut mm, &me, 2);
        hub.kick(lobby, MemberId(1));
        run(&mut mm, &me, 1);
        assert_eq!(*mm.state(), MatchState::Failed(MatchFailure::Left));
    }

    #[test]
    fn same_inputs_same_choice() {
        let pick = || {
            let hub = LocalLobbyHub::new();
            for owner in 10..16 {
                open_lobby(&hub, owner, "duel", 4);
            }
            let me = hub.member(1, "me");
            let mut mm = Matchmaker::default();
            mm.start(&me, duel());
            run(&mut mm, &me, 3);
            mm.state().lobby()
        };
        assert_eq!(pick(), pick());
        assert_eq!(pick(), Some(LobbyId(1)), "ties go to the lowest lobby id");
    }

    /// A local member the tests can watch and steer: it records the distance of every list
    /// request, can leave one lobby's join unanswered, and can answer creates with an error.
    struct Probe {
        inner: crate::lobby::LocalLobbyBackend,
        distances: std::sync::Mutex<Vec<Distance>>,
        silent_join: Option<LobbyId>,
        failing_create: bool,
        queued: std::sync::Mutex<Vec<LobbyEvent>>,
    }

    const PROBE_SILENT_JOIN: RequestId = RequestId(1 << 40);
    const PROBE_FAILED_CREATE: RequestId = RequestId((1 << 40) + 1);

    impl Probe {
        fn new(inner: crate::lobby::LocalLobbyBackend) -> Self {
            Self {
                inner,
                distances: std::sync::Mutex::default(),
                silent_join: None,
                failing_create: false,
                queued: std::sync::Mutex::default(),
            }
        }

        fn distances(&self) -> Vec<Distance> {
            self.distances.lock().map(|d| d.clone()).unwrap_or_default()
        }
    }

    impl LobbyBackend for Probe {
        fn name(&self) -> &'static str {
            "probe"
        }
        fn local_member(&self) -> Option<MemberId> {
            self.inner.local_member()
        }
        fn create(&self, spec: &LobbySpec) -> Result<RequestId, LobbyError> {
            if !self.failing_create {
                return self.inner.create(spec);
            }
            if let Ok(mut queued) = self.queued.lock() {
                queued.push(LobbyEvent::Created {
                    request: PROBE_FAILED_CREATE,
                    result: Err(LobbyError::Platform("refused by the test".into())),
                });
            }
            Ok(PROBE_FAILED_CREATE)
        }
        fn join(&self, lobby: LobbyId) -> Result<RequestId, LobbyError> {
            if self.silent_join == Some(lobby) {
                return Ok(PROBE_SILENT_JOIN);
            }
            self.inner.join(lobby)
        }
        fn leave(&self, lobby: LobbyId) {
            self.inner.leave(lobby);
        }
        fn request_list(&self, query: &LobbyQuery) -> Result<RequestId, LobbyError> {
            if let Ok(mut distances) = self.distances.lock() {
                distances.push(query.distance);
            }
            self.inner.request_list(query)
        }
        fn lobby(&self, lobby: LobbyId) -> Option<crate::lobby::LobbyInfo> {
            self.inner.lobby(lobby)
        }
        fn set_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
            self.inner.set_data(lobby, key, value)
        }
        fn set_member_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
            self.inner.set_member_data(lobby, key, value)
        }
        fn set_joinable(&self, lobby: LobbyId, joinable: bool) -> Result<(), LobbyError> {
            self.inner.set_joinable(lobby, joinable)
        }
        fn set_server(&self, lobby: LobbyId, server: LobbyServer) -> Result<(), LobbyError> {
            self.inner.set_server(lobby, server)
        }
        fn send_chat(&self, lobby: LobbyId, text: &str) -> Result<(), LobbyError> {
            self.inner.send_chat(lobby, text)
        }
        fn poll(&self) -> Vec<LobbyEvent> {
            let mut events = self.inner.poll();
            if let Ok(mut queued) = self.queued.lock() {
                events.append(&mut queued);
            }
            events
        }
    }

    /// Spec 89 § 5: each round's list request carries the next distance of the ladder.
    #[test]
    fn each_round_lists_at_the_next_distance() {
        let hub = LocalLobbyHub::new();
        let me = Probe::new(hub.member(1, "me"));
        let mut mm = Matchmaker::default();
        mm.start(&me, duel());
        run(&mut mm, &me, 4);
        assert_eq!(mm.state().name(), "creating");
        assert_eq!(
            me.distances(),
            [
                Distance::Close,
                Distance::Default,
                Distance::Far,
                Distance::Worldwide
            ]
        );
    }

    /// Spec 89 § 5: a join whose answer never comes times out after `step_timeout` steps: the
    /// lobby is left, the request abandoned, and the next candidate tried.
    #[test]
    fn a_join_that_never_answers_times_out_to_the_next_candidate() {
        let hub = LocalLobbyHub::new();
        let first = open_lobby(&hub, 10, "duel", 4);
        let second = open_lobby(&hub, 11, "duel", 4);
        let mut me = Probe::new(hub.member(1, "me"));
        me.silent_join = Some(first);
        let mut mm = Matchmaker::default();
        mm.start(
            &me,
            MatchmakingRequest {
                step_timeout: 3,
                ..duel()
            },
        );
        // The join goes out on the step the list lands and counts that step: the wait passes
        // `step_timeout` on the fourth.
        run(&mut mm, &me, 1);
        assert!(matches!(mm.state(), MatchState::Joining { lobby, .. } if *lobby == first));
        run(&mut mm, &me, 2);
        assert!(
            matches!(mm.state(), MatchState::Joining { lobby, .. } if *lobby == first),
            "still waiting at the timeout: {:?}",
            mm.state()
        );
        run(&mut mm, &me, 1);
        assert!(matches!(mm.state(), MatchState::Joining { lobby, .. } if *lobby == second));
        assert_eq!(mm.abandoned(), vec![PROBE_SILENT_JOIN]);
        run(&mut mm, &me, 1);
        assert_eq!(
            *mm.state(),
            MatchState::InLobby {
                lobby: second,
                owner: false
            }
        );
    }

    /// Spec 89 § 5: when every candidate refuses the join, matchmaking moves to the next round.
    #[test]
    fn refused_joins_exhaust_the_candidates_and_widen() {
        let hub = LocalLobbyHub::new();
        let first = open_lobby(&hub, 10, "duel", 2);
        let second = open_lobby(&hub, 11, "duel", 2);
        let me = hub.member(1, "me");
        let mut mm = Matchmaker::default();
        mm.start(&me, duel());
        let list = me.poll();
        // Rivals fill both lobbies after the list and before our joins land.
        hub.member(2, "rival").join(first).expect("rival joins");
        hub.member(3, "rival").join(second).expect("rival joins");
        mm.step(&me, &list);
        assert!(matches!(mm.state(), MatchState::Joining { lobby, .. } if *lobby == first));
        run(&mut mm, &me, 1);
        assert!(matches!(mm.state(), MatchState::Joining { lobby, .. } if *lobby == second));
        run(&mut mm, &me, 1);
        assert_eq!(mm.state().name(), "listing");
        assert_eq!(mm.round(), 1, "the next round lists wider");
        assert!(mm.candidates().is_empty());
    }

    /// Spec 89 § 5: `Created(err)` fails matchmaking with `create_failed`.
    #[test]
    fn a_create_answered_with_an_error_fails_with_create_failed() {
        let hub = LocalLobbyHub::new();
        let mut me = Probe::new(hub.member(1, "me"));
        me.failing_create = true;
        let mut mm = Matchmaker::default();
        mm.start(
            &me,
            MatchmakingRequest {
                rounds: vec![Distance::Close],
                ..duel()
            },
        );
        run(&mut mm, &me, 1);
        assert_eq!(mm.state().name(), "creating");
        run(&mut mm, &me, 1);
        let MatchState::Failed(failure) = mm.state() else {
            panic!("{:?}", mm.state());
        };
        assert_eq!(failure.code(), "create_failed");
        assert!(hub.lobbies().is_empty());
    }

    /// Spec 89 § 3 / § 5: when the owner leaves, the earliest remaining member becomes owner, its
    /// matchmaker says so, and it may launch.
    #[test]
    fn the_next_member_takes_over_as_owner_and_may_launch() {
        let hub = LocalLobbyHub::new();
        let me = hub.member(1, "me");
        let mut mm = Matchmaker::default();
        mm.start(
            &me,
            MatchmakingRequest {
                rounds: vec![Distance::Close],
                ..MatchmakingRequest::default()
            },
        );
        run(&mut mm, &me, 2);
        let MatchState::InLobby { lobby, owner: true } = *mm.state() else {
            panic!("{:?}", mm.state());
        };
        let friend = hub.member(2, "friend");
        let mut fm = Matchmaker::default();
        fm.start(
            &friend,
            MatchmakingRequest {
                min_players: 1,
                ..MatchmakingRequest::default()
            },
        );
        run(&mut fm, &friend, 2);
        assert_eq!(*fm.state(), MatchState::InLobby { lobby, owner: false });
        mm.cancel(&me);
        run(&mut fm, &friend, 1);
        assert_eq!(*fm.state(), MatchState::InLobby { lobby, owner: true });
        assert!(!fm.ready_to_launch(&friend), "not ready yet");
        friend
            .set_member_data(lobby, keys::READY, Some("1"))
            .expect("ready");
        assert!(fm.ready_to_launch(&friend));
    }

    /// Spec 89 § 5 (`joinable_in_game`): a lobby that launched before we joined takes us straight
    /// to `Launched`.
    #[test]
    fn joining_a_launched_lobby_goes_straight_to_launched() {
        let hub = LocalLobbyHub::new();
        let lobby = open_lobby(&hub, 10, "duel", 4);
        let server = LobbyServer::Steam(42);
        hub.member(10, "host").set_server(lobby, server).expect("launch");
        let me = hub.member(1, "me");
        let mut mm = Matchmaker::default();
        mm.start(&me, duel());
        run(&mut mm, &me, 2);
        assert_eq!(*mm.state(), MatchState::Launched { lobby, server });
    }
}
