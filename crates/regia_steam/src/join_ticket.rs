// SPDX-License-Identifier: MPL-2.0
//! Steam implementation of `regia_net`'s store-ticket hook.
//!
//! Tests use [`SteamJoinTicketCheck::allow`], an in-memory list, or [`SteamAuthSessions`] over a
//! fake [`SteamAuthApi`]. Neither opens a socket or links the Steamworks SDK. The live path is
//! `LiveSteamTickets`, compiled only with the `steamworks` feature on desktop: it is
//! [`SteamAuthSessions`] on the Steamworks client. `BeginAuthSession` succeeding means the ticket
//! parsed: the hook returns [`TicketVerdict::Pending`] and keeps the peer. Ownership, VAC, a
//! cancelled ticket and a login elsewhere arrive later on `ValidateAuthTicketResponse`. The host
//! must call `run_callbacks`. A refusal there is reported from [`JoinTicketCheck::take_refusals`],
//! and the server kicks the peer. The server calls [`JoinTicketCheck::end_ticket`] when the peer
//! leaves, which ends the Steam session. A `DuplicateRequest` for a session this hook holds is a
//! client that restarted on a new address: the old session ends, its peer is kicked, and the new
//! ticket begins again.

use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(all(feature = "steamworks", not(target_os = "android")))]
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, PoisonError};

use regia_net::ticket::{JoinTicketCheck, TicketRefused, TicketVerdict};

/// Decides whether a ticket blob is acceptable, without knowing about join admission.
pub trait SteamTicketSource: Send + Sync {
    /// `true` when `ticket` may enter. Pending counts as acceptable: the peer is in until a
    /// later refusal.
    fn accepts(&self, ticket: &[u8]) -> bool;

    /// The store's verdict. The default maps [`Self::accepts`] onto admit or refuse.
    ///
    /// A live source overrides this and does not call [`Self::accepts`] from it. [`Self::accepts`]
    /// may call this. Calling both for one join would open two store sessions.
    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        if self.accepts(ticket) {
            TicketVerdict::Admit
        } else {
            TicketVerdict::Refuse
        }
    }

    /// Ends the store session [`Self::verdict`] opened for `ticket`. The default does nothing.
    fn end_ticket(&self, _ticket: &[u8]) {}

    /// Tickets a late callback has refused. The default is empty. A source that returns
    /// [`TicketVerdict::Pending`] leaves the ticket here until [`Self::end_ticket`], so a refusal
    /// that arrives before the peer is connected is still applied on a later poll.
    fn take_refusals(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
}

/// In-memory allow-list. Equality is byte-for-byte. No network.
#[derive(Debug, Clone, Default)]
pub struct TableTickets {
    tickets: Vec<Vec<u8>>,
}

impl TableTickets {
    /// An allow-list of the given ticket blobs.
    pub fn new(tickets: impl IntoIterator<Item = impl Into<Vec<u8>>>) -> Self {
        Self {
            tickets: tickets.into_iter().map(Into::into).collect(),
        }
    }
}

impl SteamTicketSource for TableTickets {
    fn accepts(&self, ticket: &[u8]) -> bool {
        self.tickets.iter().any(|row| row.as_slice() == ticket)
    }
}

/// A [`JoinTicketCheck`] backed by a [`SteamTicketSource`].
#[derive(Debug, Clone)]
pub struct SteamJoinTicketCheck<S> {
    source: S,
}

impl SteamJoinTicketCheck<TableTickets> {
    /// A check that accepts only the listed ticket blobs.
    pub fn allow(tickets: impl IntoIterator<Item = impl Into<Vec<u8>>>) -> Self {
        Self {
            source: TableTickets::new(tickets),
        }
    }
}

impl<S: SteamTicketSource> SteamJoinTicketCheck<S> {
    /// A check that asks `source`.
    pub fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S: SteamTicketSource> JoinTicketCheck for SteamJoinTicketCheck<S> {
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused> {
        match self.source.verdict(ticket) {
            TicketVerdict::Refuse => Err(TicketRefused),
            TicketVerdict::Admit | TicketVerdict::Pending => Ok(()),
        }
    }

    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        self.source.verdict(ticket)
    }

    fn end_ticket(&self, ticket: &[u8]) {
        self.source.end_ticket(ticket);
    }

    fn take_refusals(&self) -> Vec<Vec<u8>> {
        self.source.take_refusals()
    }
}

/// Magic of the live Steam auth-ticket frame: `RGST`, then the Steam id, then the ticket.
pub const STEAM_AUTH_TICKET_MAGIC: [u8; 4] = *b"RGST";

/// `RGST` | steam id, little-endian u64 | raw auth-ticket bytes.
///
/// The live hook refuses a blob that does not have this shape. Tests can build one without
/// calling Steam.
pub fn encode_steam_auth_ticket(steam_id: u64, ticket: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 8 + ticket.len());
    out.extend_from_slice(&STEAM_AUTH_TICKET_MAGIC);
    out.extend_from_slice(&steam_id.to_le_bytes());
    out.extend_from_slice(ticket);
    out
}

/// Splits a frame from [`encode_steam_auth_ticket`]. `None` when the magic is wrong or the
/// steam id is missing. An empty ticket payload is `Some`, and the live hook still refuses it.
pub fn decode_steam_auth_ticket(bytes: &[u8]) -> Option<(u64, &[u8])> {
    if bytes.len() < 12 || bytes[..4] != STEAM_AUTH_TICKET_MAGIC {
        return None;
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[4..12]);
    Some((u64::from_le_bytes(id), &bytes[12..]))
}

/// Why `ISteamUser::BeginAuthSession` refused a ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeginAuthError {
    /// Steam already has an open auth session for this Steam id (`k_EBeginAuthSessionResultDuplicateRequest`).
    DuplicateRequest,
    /// Any other refusal: an invalid or expired ticket, the wrong game, an old interface version.
    Refused,
}

/// The two `ISteamUser` calls [`SteamAuthSessions`] makes.
///
/// `LiveSteamTickets` implements it on the Steamworks client. Tests implement it with a table
/// that behaves like Steam: one session per Steam id, and a second begin is
/// [`BeginAuthError::DuplicateRequest`].
pub trait SteamAuthApi: Send + Sync {
    /// `BeginAuthSession` for `steam_id` with the raw ticket bytes.
    fn begin_auth_session(&self, steam_id: u64, ticket: &[u8]) -> Result<(), BeginAuthError>;

    /// `EndAuthSession` for `steam_id`.
    fn end_auth_session(&self, steam_id: u64);
}

/// Stale tickets kept for [`SteamTicketSource::take_refusals`] at most; past it the oldest go first.
const MAX_STALE_TICKETS: usize = 1024;

/// Open auth sessions, the Steam ids a later callback refused, and replaced sessions' tickets.
#[derive(Debug, Default)]
struct AuthSessions {
    /// Steam id to the ticket frame of its open session.
    open: HashMap<u64, Vec<u8>>,
    /// Steam ids whose open session `ValidateAuthTicketResponse` refused.
    refused: HashSet<u64>,
    /// Ticket frames of sessions a restarted client replaced, reported once so the server kicks
    /// the stale peer.
    stale: VecDeque<Vec<u8>>,
    /// Per Steam id, the ticket frames begun whose first `ValidateAuthTicketResponse` has not
    /// arrived yet, oldest first (KI-NT-048, ADR-10069). Steam answers every begun session once,
    /// in begin order, and the callback names only the Steam id: the oldest waiting begin owns the
    /// next answer. An answer owned by a session that ended or was replaced is that session's and
    /// is dropped, so it cannot refuse the session that replaced it. Kept across an end.
    awaiting: HashMap<u64, VecDeque<Vec<u8>>>,
    /// Steam ids whose previous session (ended or replaced) already had its first answer while
    /// the session after it waits for its own: a ticket-cancelled answer now is the old ticket's
    /// (a crashed or closed client cancels its ticket; a ticket just issued is not cancelled before
    /// its first answer). Cleared by the new session's first answer.
    previous_answered: HashSet<u64>,
}

/// One `ValidateAuthTicketResponse`, as [`SteamAuthSessions::on_validate_answer`] takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteamAuthAnswer {
    /// `k_EAuthSessionResponseOK`.
    Ok,
    /// `k_EAuthSessionResponseAuthTicketCanceled`: the issuer cancelled the ticket.
    Cancelled,
    /// Any other error (no licence, VAC, logged in elsewhere, an invalid ticket, a ban).
    Refused,
}

/// Begins waiting for their first answer kept per Steam id at most; past it the oldest go first.
const MAX_AWAITING_PER_ID: usize = 8;

impl AuthSessions {
    /// A session opened for `steam_id`. A refusal left from an earlier session does not carry over.
    fn note_pending(&mut self, steam_id: u64, ticket: &[u8]) {
        self.refused.remove(&steam_id);
        self.open.insert(steam_id, ticket.to_vec());
        let waiting = self.awaiting.entry(steam_id).or_default();
        if waiting.len() >= MAX_AWAITING_PER_ID {
            waiting.pop_front();
        }
        waiting.push_back(ticket.to_vec());
    }

    /// A `ValidateAuthTicketResponse` refusal (see [`Self::note_response`]).
    #[cfg(test)]
    fn note_refused(&mut self, steam_id: u64) {
        self.note_response(steam_id, false);
    }

    /// A `ValidateAuthTicketResponse`. The oldest begin still waiting for its first answer owns
    /// it: when that begin is no longer the open session (it ended, or a restart replaced it), the
    /// answer is dropped (KI-NT-048). Otherwise a refusal is recorded while a session for that id
    /// is open, so a callback that arrives after `EndAuthSession` cannot kick a later rejoin.
    fn note_response(&mut self, steam_id: u64, ok: bool) {
        self.note_answer(
            steam_id,
            if ok {
                SteamAuthAnswer::Ok
            } else {
                SteamAuthAnswer::Refused
            },
        );
    }

    /// [`Self::note_response`] with the answer's kind. A cancel while the previous session (its
    /// first answer already in) has just been replaced or ended, and the new session still waits
    /// for its own first answer, is the old ticket's: dropped, and the new session's waiting begin
    /// keeps its place.
    fn note_answer(&mut self, steam_id: u64, answer: SteamAuthAnswer) {
        let open = self.open.get(&steam_id);
        let new_waits = self
            .awaiting
            .get(&steam_id)
            .and_then(VecDeque::front)
            .is_some_and(|front| open == Some(front));
        if answer == SteamAuthAnswer::Cancelled && new_waits && self.previous_answered.contains(&steam_id) {
            return;
        }
        let ok = answer == SteamAuthAnswer::Ok;
        let owner = match self.awaiting.get_mut(&steam_id) {
            Some(waiting) => {
                let front = waiting.pop_front();
                if waiting.is_empty() {
                    self.awaiting.remove(&steam_id);
                }
                front
            }
            None => None,
        };
        let open = self.open.get(&steam_id);
        if let Some(owner) = owner {
            if open != Some(&owner) {
                return;
            }
            // The open session's own first answer: the previous session is history now.
            self.previous_answered.remove(&steam_id);
        }
        if !ok && open.is_some() {
            self.refused.insert(steam_id);
        }
    }

    /// Notes that `steam_id`'s open session goes, for [`Self::previous_answered`]: whether its
    /// first answer already came (no begin of that ticket still waits).
    fn note_gone(&mut self, steam_id: u64) {
        if let Some(old) = self.open.get(&steam_id) {
            let waiting = self
                .awaiting
                .get(&steam_id)
                .is_some_and(|w| w.iter().any(|t| t == old));
            if waiting {
                self.previous_answered.remove(&steam_id);
            } else {
                self.previous_answered.insert(steam_id);
            }
        }
    }

    fn end(&mut self, steam_id: u64) {
        self.note_gone(steam_id);
        self.open.remove(&steam_id);
        self.refused.remove(&steam_id);
    }

    /// Ends the open session for `steam_id` because a new ticket replaces it. The old ticket is
    /// reported refused once, unless it is the same frame as the new one.
    fn replace(&mut self, steam_id: u64, new_ticket: &[u8]) {
        self.note_gone(steam_id);
        if let Some(old) = self.open.remove(&steam_id)
            && old != new_ticket
        {
            if self.stale.len() >= MAX_STALE_TICKETS {
                self.stale.pop_front();
            }
            self.stale.push_back(old);
        }
        self.refused.remove(&steam_id);
    }

    fn refusals(&mut self) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = self.stale.drain(..).collect();
        for (id, ticket) in &self.open {
            if self.refused.contains(id) {
                out.push(ticket.clone());
            }
        }
        out
    }
}

fn lock(sessions: &Mutex<AuthSessions>) -> MutexGuard<'_, AuthSessions> {
    sessions.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Steam auth sessions for join tickets, over any [`SteamAuthApi`].
///
/// [`SteamTicketSource::verdict`] decodes the frame from [`encode_steam_auth_ticket`] and begins
/// a session. A begin that succeeds is [`TicketVerdict::Pending`]: the ticket parsed, and
/// ownership, VAC, a cancel or a login elsewhere arrive later as `ValidateAuthTicketResponse`,
/// which the host forwards to [`Self::on_validate_response`]. An empty or undecodable frame is a
/// refusal and calls no API.
///
/// A client that crashed and joins again from a new port still has its old session open, so the
/// begin is [`BeginAuthError::DuplicateRequest`]. The old session is ended, its ticket is reported
/// from [`SteamTicketSource::take_refusals`] (the server kicks the stale peer), and the begin runs
/// again. [`SteamTicketSource::end_ticket`] ends a session only while that ticket is still the
/// open one for its Steam id, so the stale peer leaving does not end the new session.
pub struct SteamAuthSessions<A> {
    api: A,
    state: Mutex<AuthSessions>,
}

impl<A: SteamAuthApi> SteamAuthSessions<A> {
    /// Sessions that call `api`.
    pub fn new(api: A) -> Self {
        Self {
            api,
            state: Mutex::new(AuthSessions::default()),
        }
    }

    /// The API this tracker calls.
    pub fn api(&self) -> &A {
        &self.api
    }

    /// Records a `ValidateAuthTicketResponse`. `ok == false` reports that Steam id's open session
    /// from [`SteamTicketSource::take_refusals`] until it ends. A response for a Steam id with no
    /// open session (it already ended) is ignored.
    ///
    /// The callback names only the Steam id, so each response is matched to the oldest begin for
    /// that id still waiting for its first answer (Steam answers begins in order): a refusal that
    /// belongs to a session a restart replaced, or one that already ended, is dropped and does not
    /// kick the session after it (KI-NT-048, ADR-10069). Call it for every response, `ok` or not.
    pub fn on_validate_response(&self, steam_id: u64, ok: bool) {
        lock(&self.state).note_response(steam_id, ok);
    }

    /// [`Self::on_validate_response`] with the answer's kind (the live callback passes it). A
    /// ticket-cancelled answer that arrives after a restart replaced (or a leave ended) a session
    /// whose first answer was already in, while the new session waits for its own, is the old
    /// ticket's cancel (the crashed client's) and does not refuse the new session.
    pub fn on_validate_answer(&self, steam_id: u64, answer: SteamAuthAnswer) {
        lock(&self.state).note_answer(steam_id, answer);
    }

    /// Ends the auth session open for `steam_id`, whatever its ticket.
    pub fn end(&self, steam_id: u64) {
        let open = lock(&self.state).open.contains_key(&steam_id);
        if open {
            self.api.end_auth_session(steam_id);
        }
        lock(&self.state).end(steam_id);
    }

    /// True while a session is open for `steam_id`.
    pub fn is_open(&self, steam_id: u64) -> bool {
        lock(&self.state).open.contains_key(&steam_id)
    }

    fn decide(&self, ticket: &[u8]) -> TicketVerdict {
        let Some((steam_id, bytes)) = decode_steam_auth_ticket(ticket) else {
            return TicketVerdict::Refuse;
        };
        if bytes.is_empty() {
            return TicketVerdict::Refuse;
        }
        match self.api.begin_auth_session(steam_id, bytes) {
            Ok(()) => {}
            Err(BeginAuthError::DuplicateRequest) if self.is_open(steam_id) => {
                // A restart from a new address: the old peer's session goes, and its ticket is
                // reported so the server kicks that peer.
                self.api.end_auth_session(steam_id);
                lock(&self.state).replace(steam_id, ticket);
                if self.api.begin_auth_session(steam_id, bytes).is_err() {
                    return TicketVerdict::Refuse;
                }
            }
            Err(_) => return TicketVerdict::Refuse,
        }
        lock(&self.state).note_pending(steam_id, ticket);
        TicketVerdict::Pending
    }
}

impl<A: SteamAuthApi> SteamTicketSource for SteamAuthSessions<A> {
    fn accepts(&self, ticket: &[u8]) -> bool {
        !matches!(self.decide(ticket), TicketVerdict::Refuse)
    }

    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        self.decide(ticket)
    }

    fn end_ticket(&self, ticket: &[u8]) {
        let Some((steam_id, _)) = decode_steam_auth_ticket(ticket) else {
            return;
        };
        let current = lock(&self.state)
            .open
            .get(&steam_id)
            .is_some_and(|open| open == ticket);
        if current {
            self.end(steam_id);
        }
    }

    fn take_refusals(&self) -> Vec<Vec<u8>> {
        lock(&self.state).refusals()
    }
}

impl<A: SteamAuthApi> JoinTicketCheck for SteamAuthSessions<A> {
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused> {
        match self.decide(ticket) {
            TicketVerdict::Refuse => Err(TicketRefused),
            TicketVerdict::Admit | TicketVerdict::Pending => Ok(()),
        }
    }

    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        self.decide(ticket)
    }

    fn end_ticket(&self, ticket: &[u8]) {
        SteamTicketSource::end_ticket(self, ticket);
    }

    fn take_refusals(&self) -> Vec<Vec<u8>> {
        SteamTicketSource::take_refusals(self)
    }
}

/// [`SteamAuthApi`] on the Steamworks client.
#[cfg(all(feature = "steamworks", not(target_os = "android")))]
struct ClientAuthApi {
    client: steamworks::Client,
}

#[cfg(all(feature = "steamworks", not(target_os = "android")))]
impl SteamAuthApi for ClientAuthApi {
    fn begin_auth_session(&self, steam_id: u64, ticket: &[u8]) -> Result<(), BeginAuthError> {
        self.client
            .user()
            .begin_authentication_session(steamworks::SteamId::from_raw(steam_id), ticket)
            .map_err(|err| match err {
                steamworks::AuthSessionError::DuplicateRequest => BeginAuthError::DuplicateRequest,
                _ => BeginAuthError::Refused,
            })
    }

    fn end_auth_session(&self, steam_id: u64) {
        self.client
            .user()
            .end_authentication_session(steamworks::SteamId::from_raw(steam_id));
    }
}

/// Live `BeginAuthSession` source. Desktop, and only with the `steamworks` feature.
///
/// [`SteamAuthSessions`] on the Steamworks client, with `ValidateAuthTicketResponse` registered:
/// see that type for the verdicts and the restart rule.
///
/// The host must keep calling the Steam client's `run_callbacks`. This crate does not. Until that
/// callback, the peer is in the game. An error callback is a kick (`kicked`), not `ticket_refused`.
/// [`JoinTicketCheck::end_ticket`] and [`Self::end`] both call `EndAuthSession`.
#[cfg(all(feature = "steamworks", not(target_os = "android")))]
pub struct LiveSteamTickets {
    sessions: Arc<SteamAuthSessions<ClientAuthApi>>,
    _callback: steamworks::CallbackHandle,
}

#[cfg(all(feature = "steamworks", not(target_os = "android")))]
impl LiveSteamTickets {
    /// Uses `client` for `BeginAuthSession`, `EndAuthSession` and `ValidateAuthTicketResponse`.
    /// Does not connect by itself. The callback stays registered until this value is dropped.
    pub fn new(client: steamworks::Client) -> Self {
        let sessions = Arc::new(SteamAuthSessions::new(ClientAuthApi {
            client: client.clone(),
        }));
        let shared = Arc::clone(&sessions);
        let callback = client.register_callback(move |event: steamworks::ValidateAuthTicketResponse| {
            let answer = match &event.response {
                Ok(()) => SteamAuthAnswer::Ok,
                Err(steamworks::AuthSessionValidateError::AuthTicketCancelled) => SteamAuthAnswer::Cancelled,
                Err(_) => SteamAuthAnswer::Refused,
            };
            shared.on_validate_answer(event.steam_id.raw(), answer);
        });
        Self {
            sessions,
            _callback: callback,
        }
    }

    /// Ends the auth session [`SteamTicketSource::verdict`] started for `steam_id`.
    pub fn end(&self, steam_id: u64) {
        self.sessions.end(steam_id);
    }
}

#[cfg(all(feature = "steamworks", not(target_os = "android")))]
impl SteamTicketSource for LiveSteamTickets {
    fn accepts(&self, ticket: &[u8]) -> bool {
        SteamTicketSource::accepts(&*self.sessions, ticket)
    }

    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        SteamTicketSource::verdict(&*self.sessions, ticket)
    }

    fn end_ticket(&self, ticket: &[u8]) {
        SteamTicketSource::end_ticket(&*self.sessions, ticket);
    }

    fn take_refusals(&self) -> Vec<Vec<u8>> {
        SteamTicketSource::take_refusals(&*self.sessions)
    }
}

#[cfg(all(feature = "steamworks", not(target_os = "android")))]
impl JoinTicketCheck for LiveSteamTickets {
    fn check_ticket(&self, ticket: &[u8]) -> Result<(), TicketRefused> {
        JoinTicketCheck::check_ticket(&*self.sessions, ticket)
    }

    fn verdict(&self, ticket: &[u8]) -> TicketVerdict {
        JoinTicketCheck::verdict(&*self.sessions, ticket)
    }

    fn end_ticket(&self, ticket: &[u8]) {
        JoinTicketCheck::end_ticket(&*self.sessions, ticket);
    }

    fn take_refusals(&self) -> Vec<Vec<u8>> {
        JoinTicketCheck::take_refusals(&*self.sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list_accepts_good_and_refuses_bad_without_a_network_call() {
        let check = SteamJoinTicketCheck::allow([b"good".to_vec()]);
        assert!(check.check_ticket(b"good").is_ok());
        let refused = check.check_ticket(b"bad").expect_err("bad ticket");
        assert_eq!(TicketRefused::CODE, "ticket_refused");
        assert_eq!(refused.to_string(), TicketRefused::CODE);
        assert_eq!(check.verdict(b"good"), TicketVerdict::Admit);
        assert_eq!(check.verdict(b"bad"), TicketVerdict::Refuse);
    }

    #[test]
    fn steam_auth_ticket_frame_round_trips() {
        let bytes = encode_steam_auth_ticket(42, b"abc");
        let (id, ticket) = decode_steam_auth_ticket(&bytes).expect("frame");
        assert_eq!(id, 42);
        assert_eq!(ticket, b"abc");
        assert!(decode_steam_auth_ticket(b"nope").is_none());
        assert!(decode_steam_auth_ticket(b"RGST").is_none());
        let empty = encode_steam_auth_ticket(7, b"");
        assert_eq!(decode_steam_auth_ticket(&empty), Some((7, &[][..])));
    }

    #[test]
    fn a_late_refusal_stays_until_the_session_ends() {
        let mut sessions = AuthSessions::default();
        let ticket = encode_steam_auth_ticket(9, b"ticket");
        assert!(sessions.refusals().is_empty());
        // Steam's order: the begin opens the session, the callback refuses it later. A refusal
        // with no open session is ignored (net-rev-b #12, the test below).
        sessions.note_pending(9, &ticket);
        sessions.note_refused(9);
        assert_eq!(sessions.refusals(), vec![ticket.clone()]);
        assert_eq!(
            sessions.refusals(),
            vec![ticket.clone()],
            "reading the refusal does not drop it"
        );
        sessions.end(9);
        assert!(
            sessions.refusals().is_empty(),
            "end drops the session and the refusal"
        );
    }

    /// Review net-rev-b #12: a `ValidateAuthTicketResponse` refusal queued before the player left
    /// arrives after `EndAuthSession`. The player rejoins with a new ticket: no kick.
    #[test]
    fn a_refusal_after_the_session_ended_does_not_kick_the_rejoin() {
        let mut sessions = AuthSessions::default();
        let first = encode_steam_auth_ticket(9, b"first");
        sessions.note_pending(9, &first);
        sessions.end(9);
        sessions.note_refused(9);
        let rejoin = encode_steam_auth_ticket(9, b"rejoin");
        sessions.note_pending(9, &rejoin);
        assert!(
            sessions.refusals().is_empty(),
            "a refusal for an ended session must not kick the rejoin, got {:?}",
            sessions.refusals()
        );
    }

    /// Steam's session table: one session per Steam id; a second begin is `DuplicateRequest`.
    #[derive(Default)]
    struct FakeSteam {
        open: Mutex<HashMap<u64, Vec<u8>>>,
        begins: Mutex<u32>,
    }

    impl SteamAuthApi for FakeSteam {
        fn begin_auth_session(&self, steam_id: u64, ticket: &[u8]) -> Result<(), BeginAuthError> {
            *self.begins.lock().unwrap_or_else(PoisonError::into_inner) += 1;
            let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
            if open.contains_key(&steam_id) {
                return Err(BeginAuthError::DuplicateRequest);
            }
            if ticket == b"bad" {
                return Err(BeginAuthError::Refused);
            }
            open.insert(steam_id, ticket.to_vec());
            Ok(())
        }

        fn end_auth_session(&self, steam_id: u64) {
            self.open
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&steam_id);
        }
    }

    fn steam_open(sessions: &SteamAuthSessions<FakeSteam>) -> HashMap<u64, Vec<u8>> {
        sessions
            .api()
            .open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Review net-rev-b #13: a client that crashed joins again from a new port while its old
    /// session is open. The old session ends and its ticket is reported (the stale peer is
    /// kicked); the new ticket is a pending admit, not a `ticket_refused` lock-out.
    #[test]
    fn a_restart_on_a_new_port_replaces_the_open_session() {
        let sessions = SteamAuthSessions::new(FakeSteam::default());
        let old = encode_steam_auth_ticket(5, b"old");
        let new = encode_steam_auth_ticket(5, b"new");
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &old),
            TicketVerdict::Pending
        );
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &new),
            TicketVerdict::Pending,
            "a DuplicateRequest for a held session is a restart, not a refusal"
        );
        assert_eq!(steam_open(&sessions).get(&5), Some(&b"new".to_vec()));
        assert_eq!(
            SteamTicketSource::take_refusals(&sessions),
            vec![old.clone()],
            "the old ticket is reported once so the server kicks the stale peer"
        );
        assert!(SteamTicketSource::take_refusals(&sessions).is_empty());
        SteamTicketSource::end_ticket(&sessions, &old);
        assert!(
            steam_open(&sessions).contains_key(&5),
            "the stale peer leaving does not end the new session"
        );
        SteamTicketSource::end_ticket(&sessions, &new);
        assert!(steam_open(&sessions).is_empty(), "the new peer leaving ends it");
        assert!(!sessions.is_open(5));
    }

    /// A `DuplicateRequest` for a Steam id this hook never opened is someone else's session: refused.
    #[test]
    fn a_duplicate_for_a_session_this_hook_does_not_hold_is_refused() {
        let api = FakeSteam::default();
        api.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(6, b"elsewhere".to_vec());
        let sessions = SteamAuthSessions::new(api);
        let ticket = encode_steam_auth_ticket(6, b"mine");
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &ticket),
            TicketVerdict::Refuse
        );
        assert_eq!(steam_open(&sessions).get(&6), Some(&b"elsewhere".to_vec()));
        assert!(SteamTicketSource::take_refusals(&sessions).is_empty());
    }

    /// Begin errors other than a duplicate, an empty ticket and a bad frame are refusals; the last
    /// two never call Steam.
    #[test]
    fn begin_errors_and_bad_frames_refuse() {
        let sessions = SteamAuthSessions::new(FakeSteam::default());
        let bad = encode_steam_auth_ticket(7, b"bad");
        assert_eq!(SteamTicketSource::verdict(&sessions, &bad), TicketVerdict::Refuse);
        let empty = encode_steam_auth_ticket(7, b"");
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &empty),
            TicketVerdict::Refuse
        );
        assert_eq!(
            SteamTicketSource::verdict(&sessions, b"nope"),
            TicketVerdict::Refuse
        );
        assert_eq!(
            *sessions
                .api()
                .begins
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            1
        );
        assert!(!sessions.is_open(7));
    }

    /// `ValidateAuthTicketResponse` refusals: reported while the session is open, dropped by the
    /// end, and ignored once it ended (net-rev-b #12 through the public API).
    #[test]
    fn a_validate_refusal_kicks_only_the_open_session() {
        let sessions = SteamAuthSessions::new(FakeSteam::default());
        let first = encode_steam_auth_ticket(8, b"first");
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &first),
            TicketVerdict::Pending
        );
        sessions.on_validate_response(8, true);
        assert!(SteamTicketSource::take_refusals(&sessions).is_empty());
        sessions.on_validate_response(8, false);
        assert_eq!(SteamTicketSource::take_refusals(&sessions), vec![first.clone()]);
        SteamTicketSource::end_ticket(&sessions, &first);
        assert!(steam_open(&sessions).is_empty());
        sessions.on_validate_response(8, false);
        let rejoin = encode_steam_auth_ticket(8, b"rejoin");
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &rejoin),
            TicketVerdict::Pending
        );
        assert!(
            SteamTicketSource::take_refusals(&sessions).is_empty(),
            "a refusal after the end does not kick the rejoin"
        );
    }

    /// KI-NT-048 (ADR-10069): Steam's refusal for a session a restart replaced, or one that ended
    /// before its answer came, arrives after the new begin. It is the old session's answer and must
    /// not kick the new session; the new session's own later refusal still does.
    #[test]
    fn a_refusal_for_a_replaced_session_does_not_kick_the_new_one() {
        let sessions = SteamAuthSessions::new(FakeSteam::default());
        let old = encode_steam_auth_ticket(5, b"old");
        let new = encode_steam_auth_ticket(5, b"new");
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &old),
            TicketVerdict::Pending
        );
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &new),
            TicketVerdict::Pending
        );
        assert_eq!(SteamTicketSource::take_refusals(&sessions), vec![old.clone()]);
        // The old session's first answer, queued before the restart: a refusal.
        sessions.on_validate_response(5, false);
        assert!(
            SteamTicketSource::take_refusals(&sessions).is_empty(),
            "the replaced session's refusal kicked the new session"
        );
        // The new session's first answer, then a later refusal (a VAC ban, a cancel).
        sessions.on_validate_response(5, true);
        assert!(SteamTicketSource::take_refusals(&sessions).is_empty());
        sessions.on_validate_response(5, false);
        assert_eq!(SteamTicketSource::take_refusals(&sessions), vec![new.clone()]);

        // An ordinary leave and rejoin before the first session's answer came.
        let first = encode_steam_auth_ticket(6, b"first");
        let rejoin = encode_steam_auth_ticket(6, b"rejoin");
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &first),
            TicketVerdict::Pending
        );
        SteamTicketSource::end_ticket(&sessions, &first);
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &rejoin),
            TicketVerdict::Pending
        );
        sessions.on_validate_response(6, false);
        assert!(
            SteamTicketSource::take_refusals(&sessions)
                .iter()
                .all(|t| *t != rejoin),
            "the ended session's refusal kicked the rejoin"
        );
        sessions.on_validate_response(6, false);
        assert!(
            SteamTicketSource::take_refusals(&sessions).contains(&rejoin),
            "the rejoin's own refusal is reported"
        );
    }

    /// KI-NT-048 (ADR-10069): the crashed client's ticket is cancelled after its session's first
    /// answer and after the restart's begin. The cancel is the old ticket's; the new session's own
    /// later refusal (and its own cancel after its first answer) still kick.
    #[test]
    fn a_cancel_of_the_replaced_ticket_does_not_kick_the_restarted_session() {
        let sessions = SteamAuthSessions::new(FakeSteam::default());
        let old = encode_steam_auth_ticket(9, b"old");
        let new = encode_steam_auth_ticket(9, b"new");
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &old),
            TicketVerdict::Pending
        );
        sessions.on_validate_answer(9, SteamAuthAnswer::Ok);
        assert_eq!(
            SteamTicketSource::verdict(&sessions, &new),
            TicketVerdict::Pending
        );
        assert_eq!(SteamTicketSource::take_refusals(&sessions), vec![old.clone()]);
        sessions.on_validate_answer(9, SteamAuthAnswer::Cancelled);
        assert!(
            SteamTicketSource::take_refusals(&sessions).is_empty(),
            "the old ticket's cancel kicked the restarted session"
        );
        sessions.on_validate_answer(9, SteamAuthAnswer::Ok);
        sessions.on_validate_answer(9, SteamAuthAnswer::Cancelled);
        assert_eq!(
            SteamTicketSource::take_refusals(&sessions),
            vec![new.clone()],
            "the new session's own cancel kicks"
        );
        // A refusal (not a cancel) right after the restart still belongs to the waiting session.
        let other = SteamAuthSessions::new(FakeSteam::default());
        let a = encode_steam_auth_ticket(4, b"a");
        let b = encode_steam_auth_ticket(4, b"b");
        assert_eq!(SteamTicketSource::verdict(&other, &a), TicketVerdict::Pending);
        other.on_validate_answer(4, SteamAuthAnswer::Ok);
        assert_eq!(SteamTicketSource::verdict(&other, &b), TicketVerdict::Pending);
        let _ = SteamTicketSource::take_refusals(&other);
        other.on_validate_answer(4, SteamAuthAnswer::Refused);
        assert_eq!(SteamTicketSource::take_refusals(&other), vec![b.clone()]);
    }

    /// Forwards verdict, end and late refusals. `accepts` is not the path the server calls.
    struct Scripted {
        inner: Mutex<ScriptedInner>,
    }

    struct ScriptedInner {
        refused: Vec<Vec<u8>>,
        ended: Vec<Vec<u8>>,
    }

    impl SteamTicketSource for Scripted {
        fn accepts(&self, _ticket: &[u8]) -> bool {
            true
        }

        fn verdict(&self, _ticket: &[u8]) -> TicketVerdict {
            TicketVerdict::Pending
        }

        fn end_ticket(&self, ticket: &[u8]) {
            lock_scripted(&self.inner).ended.push(ticket.to_vec());
        }

        fn take_refusals(&self) -> Vec<Vec<u8>> {
            std::mem::take(&mut lock_scripted(&self.inner).refused)
        }
    }

    fn lock_scripted(inner: &Mutex<ScriptedInner>) -> MutexGuard<'_, ScriptedInner> {
        inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[test]
    fn the_wrapper_forwards_a_pending_verdict_and_a_late_refusal() {
        let source = Scripted {
            inner: Mutex::new(ScriptedInner {
                refused: vec![b"pending".to_vec()],
                ended: Vec::new(),
            }),
        };
        let check = SteamJoinTicketCheck::new(source);
        assert_eq!(check.verdict(b"pending"), TicketVerdict::Pending);
        assert!(check.check_ticket(b"pending").is_ok());
        assert_eq!(check.take_refusals(), vec![b"pending".to_vec()]);
        assert!(check.take_refusals().is_empty());
        check.end_ticket(b"pending");
        assert_eq!(
            lock_scripted(&check.source.inner).ended,
            vec![b"pending".to_vec()]
        );
    }
}
