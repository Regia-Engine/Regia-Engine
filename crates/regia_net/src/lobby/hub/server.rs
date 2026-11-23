// SPDX-License-Identifier: MPL-2.0
//! [`HubServer`]: the hub's pure, bounded state core. No socket and no clock: every operation takes
//! the caller's `now_ms`, so tests drive it on a test clock and two runs of the same calls give the
//! same state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::join::{JoinPlan, JoinRow, ReachKind, ReachOffer, broker_reach};
use super::wire::{ChatScope, DirectoryPage, HubEvent, RoomState};
use super::{
    HubError, HubLobbyCard, HubPresence, HubReport, HubSettings, InviteCard, InviteToken, PresenceRow,
    clean_handle, fold, public_data,
};
use crate::crypto::PlayerKey;
use crate::lobby::{
    LeaveReason, LobbyError, LobbyId, LobbyInfo, LobbyMember, LobbyServer, LobbySpec, LobbyVisibility,
    MAX_KEYS, MemberId, check_chat, check_key, check_spec, check_value, keys,
};

/// Bytes one directory page aims to stay under (a page always holds at least one row).
const PAGE_BYTES: usize = 32 * 1024;
/// Rows and cards a [`HubReport`] lists.
const REPORT_ROWS: usize = 64;
/// Join rows a [`HubReport`] lists.
const REPORT_JOINS: usize = 256;

struct Session {
    handle: String,
    key: String,
    /// The player key the transport proved; `None` for a core-only session ([`HubServer::connect`]).
    identity: Option<PlayerKey>,
    presence: HubPresence,
    lobby: Option<LobbyId>,
    /// Friends by player key (ADR-8633): who may join this session's friends-only lobbies.
    friends: BTreeSet<PlayerKey>,
    last_ms: u64,
    /// `None` until the first directory update went out (it is a snapshot).
    dir_sent: Option<u64>,
    presence_sent: u64,
    chat_times: Vec<u64>,
    /// Data, session-address and joinable writes in the last second ([`HubSettings::data_per_sec`]).
    data_times: Vec<u64>,
    /// When (session clock) the cursor last moved backwards ([`HubSettings::list_rewind_ms`]).
    rewind_ms: Option<u64>,
    outbox: Vec<HubEvent>,
    /// Events were dropped at the outbox cap since the last drain: the drain resynchronizes, and
    /// a session still overflowed at [`HubServer::expire`] is a slow reader.
    overflowed: bool,
    /// The last drain carried a resync: if the link cannot take it, the session cannot keep up.
    drained_overflow: bool,
    /// The first lobby chat line (its sequence number) dropped at the outbox cap since the last
    /// resync; the resync replays it and every later kept line (KI-NT-045, ADR-10069).
    lobby_chat_gap: Option<u64>,
    /// The same for global chat.
    global_chat_gap: Option<u64>,
}

/// One kept chat line: sequence number, sender, the sender's handle then, text.
type ChatLine = (u64, MemberId, String, String);

/// Which chat history a queued event belongs to, so a drop at the outbox cap marks the gap.
#[derive(Debug, Clone, Copy)]
enum ChatMark {
    None,
    Lobby(u64),
    Global(u64),
}

struct RoomMember {
    id: MemberId,
    data: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinState {
    Asked,
    Planned,
    Reached,
    Denied,
}

impl JoinState {
    fn name(self) -> &'static str {
        match self {
            Self::Asked => "asked",
            Self::Planned => "planned",
            Self::Reached => "reached",
            Self::Denied => "denied",
        }
    }
}

struct Join {
    offer: ReachOffer,
    state: JoinState,
    planned: Option<ReachKind>,
    chosen: Option<ReachKind>,
    punch_attempts: u32,
    /// The member's plan, while it was dropped at the member's outbox cap: the resync sends it
    /// again (KI-NT-045).
    member_plan_lost: Option<JoinPlan>,
    /// The owner's plan for this member, while it was dropped at the owner's outbox cap.
    owner_plan_lost: Option<JoinPlan>,
}

struct Room {
    owner: MemberId,
    visibility: LobbyVisibility,
    max_members: u32,
    joinable: bool,
    data: BTreeMap<String, String>,
    members: Vec<RoomMember>,
    server: Option<LobbyServer>,
    muted: BTreeSet<MemberId>,
    /// Muted player keys: a mute holds when the member leaves and comes back, under any member id.
    muted_keys: BTreeSet<PlayerKey>,
    chat: VecDeque<ChatLine>,
    /// The sequence number of the last lobby chat line.
    chat_seq: u64,
    chat_times: BTreeMap<MemberId, Vec<u64>>,
    /// Writes the lobby took in the last second, from every member
    /// ([`HubSettings::lobby_data_per_sec`]).
    data_times: Vec<u64>,
    /// The owner's reach offer (join brokering).
    offer: Option<ReachOffer>,
    /// Members' asks and plans.
    joins: BTreeMap<MemberId, Join>,
}

impl Room {
    fn listed(&self) -> bool {
        self.visibility == LobbyVisibility::Public
    }

    fn has(&self, member: MemberId) -> bool {
        self.members.iter().any(|m| m.id == member)
    }
}

/// Who an invite is for: the invitee's player key when its session has one (the invite then
/// outlives that session and only that key redeems it), else its member id (ADR-8633).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Invitee {
    Member(MemberId),
    Player(PlayerKey),
}

struct Invite {
    token: InviteToken,
    expires_ms: u64,
    serial: u64,
    from: MemberId,
    from_handle: String,
}

/// The hub's state: sessions, presence, lobbies, the directory with its generations, invites and
/// chat. Bounded by [`HubSettings`]. See the module docs for the rules it keeps.
pub struct HubServer {
    settings: HubSettings,
    secret: [u8; 32],
    next_member: u64,
    next_lobby: u64,
    next_invite: u64,
    dir_generation: u64,
    presence_generation: u64,
    /// Directory changes: (generation, lobby). Every change after `dir_floor` is here.
    dir_log: VecDeque<(u64, LobbyId)>,
    dir_floor: u64,
    /// Presence changes: (generation, member). Every change after `presence_floor` is here.
    presence_log: VecDeque<(u64, MemberId)>,
    presence_floor: u64,
    sessions: BTreeMap<MemberId, Session>,
    by_handle: BTreeMap<String, MemberId>,
    by_identity: BTreeMap<PlayerKey, MemberId>,
    rooms: BTreeMap<LobbyId, Room>,
    /// Lobbies the directory lists (as of their last change).
    in_directory: BTreeSet<LobbyId>,
    invites: BTreeMap<(LobbyId, Invitee), Invite>,
    global_chat: VecDeque<ChatLine>,
    /// The sequence number of the last global chat line.
    global_chat_seq: u64,
    /// Global chat lines the hub took in the last second ([`HubSettings::global_chat_per_sec`]).
    global_chat_times: Vec<u64>,
    directory_bytes: u64,
    chat_bytes: u64,
    other_bytes: u64,
    expired: u64,
    refused: BTreeMap<String, u64>,
}

impl std::fmt::Debug for HubServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubServer")
            .field("sessions", &self.sessions.len())
            .field("lobbies", &self.rooms.len())
            .field("dir_generation", &self.dir_generation)
            .field("presence_generation", &self.presence_generation)
            .finish_non_exhaustive()
    }
}

impl HubServer {
    /// A hub with these settings (sanitized) and a fresh random invite secret.
    pub fn new(settings: HubSettings) -> Self {
        Self::with_secret(settings, crate::handshake::fresh_secret())
    }

    /// A hub whose invite tokens derive from `secret` (tests and replays).
    pub fn with_secret(settings: HubSettings, secret: [u8; 32]) -> Self {
        Self {
            settings: settings.sanitized(),
            secret,
            next_member: 0,
            next_lobby: 0,
            next_invite: 0,
            dir_generation: 0,
            presence_generation: 0,
            dir_log: VecDeque::new(),
            dir_floor: 0,
            presence_log: VecDeque::new(),
            presence_floor: 0,
            sessions: BTreeMap::new(),
            by_handle: BTreeMap::new(),
            by_identity: BTreeMap::new(),
            rooms: BTreeMap::new(),
            in_directory: BTreeSet::new(),
            invites: BTreeMap::new(),
            global_chat: VecDeque::new(),
            global_chat_seq: 0,
            global_chat_times: Vec::new(),
            directory_bytes: 0,
            chat_bytes: 0,
            other_bytes: 0,
            expired: 0,
            refused: BTreeMap::new(),
        }
    }

    /// The sanitized settings.
    pub fn settings(&self) -> &HubSettings {
        &self.settings
    }

    /// Allocates a session for `handle` with no player key (a core-only session: tests, replays,
    /// operator tools). [`super::HubHost`] always uses [`HubServer::connect_player`]. The new
    /// session gets a directory snapshot at the next [`HubServer::flush`].
    pub fn connect(&mut self, handle: &str, now_ms: u64) -> Result<MemberId, HubError> {
        let result = self.connect_inner(None, handle, now_ms);
        self.count(result.as_ref().err());
        result
    }

    /// Allocates a session for `handle` under `identity`, the player key the client's transport
    /// proved. A key that already holds a session under the same handle gets that session back (a
    /// reconnect: a new `Welcome`, its room, and a directory snapshot); under another handle it is
    /// refused with [`HubError::IdentityInUse`]. The all-zero key is refused with
    /// [`HubError::IdentityMismatch`]: no proof can show it (ADR-8665), and a keyless session's
    /// `Welcome` reports it, so the hub never records it as a player.
    pub fn connect_player(
        &mut self,
        identity: PlayerKey,
        handle: &str,
        now_ms: u64,
    ) -> Result<MemberId, HubError> {
        let result = if identity == PlayerKey::default() {
            Err(HubError::IdentityMismatch)
        } else {
            self.connect_inner(Some(identity), handle, now_ms)
        };
        self.count(result.as_ref().err());
        result
    }

    /// The player key of `member`'s session (`None` for a core-only session).
    pub fn identity(&self, member: MemberId) -> Option<PlayerKey> {
        self.sessions.get(&member).and_then(|s| s.identity)
    }

    fn connect_inner(
        &mut self,
        identity: Option<PlayerKey>,
        handle: &str,
        now_ms: u64,
    ) -> Result<MemberId, HubError> {
        let handle = clean_handle(handle, self.settings.handle_max_bytes)?;
        let key = fold(&handle);
        if let Some(id) = identity.and_then(|i| self.by_identity.get(&i).copied()) {
            let Some(s) = self.sessions.get_mut(&id) else {
                return Err(HubError::NoSession);
            };
            if s.key != key {
                return Err(HubError::IdentityInUse);
            }
            s.last_ms = s.last_ms.max(now_ms);
            s.dir_sent = None;
            let (handle, lobby) = (s.handle.clone(), s.lobby);
            self.push(
                id,
                HubEvent::Welcome {
                    member: id,
                    handle,
                    idle_ms: self.settings.idle_ms,
                    identity: identity.unwrap_or_default(),
                },
            );
            if let Some(room) = lobby.and_then(|l| self.room_state(l)) {
                self.push(id, HubEvent::Room(room));
            }
            return Ok(id);
        }
        if self.by_handle.contains_key(&key) {
            return Err(HubError::HandleTaken);
        }
        if self.sessions.len() >= self.settings.session_cap {
            return Err(HubError::SessionCap);
        }
        self.next_member += 1;
        let id = MemberId(self.next_member);
        self.sessions.insert(
            id,
            Session {
                handle: handle.clone(),
                key: key.clone(),
                identity,
                presence: HubPresence::Menu,
                lobby: None,
                friends: BTreeSet::new(),
                data_times: Vec::new(),
                rewind_ms: None,
                drained_overflow: false,
                last_ms: now_ms,
                dir_sent: None,
                presence_sent: 0,
                chat_times: Vec::new(),
                outbox: Vec::new(),
                overflowed: false,
                lobby_chat_gap: None,
                global_chat_gap: None,
            },
        );
        self.by_handle.insert(key, id);
        if let Some(i) = identity {
            self.by_identity.insert(i, id);
        }
        self.push(
            id,
            HubEvent::Welcome {
                member: id,
                handle,
                idle_ms: self.settings.idle_ms,
                identity: identity.unwrap_or_default(),
            },
        );
        self.note_presence(id);
        Ok(id)
    }

    /// Ends a session: it leaves its lobby (members see it go) and its presence row is removed.
    pub fn disconnect(&mut self, member: MemberId) {
        if let Some(lobby) = self.sessions.get(&member).and_then(|s| s.lobby) {
            self.leave_inner(member, lobby, LeaveReason::Lost);
        }
        let Some(session) = self.sessions.remove(&member) else {
            return;
        };
        self.by_handle.remove(&session.key);
        if let Some(i) = session.identity {
            self.by_identity.remove(&i);
        }
        // An invite bound to a player key outlives the session: only that key redeems it.
        self.invites.retain(|(_, to), _| *to != Invitee::Member(member));
        self.note_presence(member);
    }

    /// Expires sessions silent for [`HubSettings::idle_ms`] or more, and slow readers whose outbox
    /// overflowed; drops invites past their TTL. Returns the expired sessions.
    pub fn expire(&mut self, now_ms: u64) -> Vec<MemberId> {
        let idle = self.settings.idle_ms;
        let stale: Vec<MemberId> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.overflowed || now_ms.saturating_sub(s.last_ms) >= idle)
            .map(|(id, _)| *id)
            .collect();
        for id in &stale {
            self.disconnect(*id);
        }
        self.expired += stale.len() as u64;
        self.invites.retain(|_, i| i.expires_ms > now_ms);
        stale
    }

    /// Keeps a session alive.
    pub fn heartbeat(&mut self, member: MemberId, now_ms: u64) -> Result<(), HubError> {
        self.touch(member, now_ms)
    }

    /// Sets a session's presence. `Lobby` needs a lobby, `InGame` a lobby whose session was
    /// confirmed, `Menu` no lobby (leave first).
    pub fn set_presence(
        &mut self,
        member: MemberId,
        presence: HubPresence,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.set_presence_inner(member, presence, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn set_presence_inner(
        &mut self,
        member: MemberId,
        presence: HubPresence,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        let (current, lobby) = {
            let s = &self.sessions[&member];
            (s.presence, s.lobby)
        };
        if presence == current {
            return Ok(());
        }
        let allowed = match (presence, lobby) {
            (HubPresence::Menu, None) => true,
            (HubPresence::Lobby, Some(_)) => true,
            (HubPresence::InGame, Some(l)) => self.rooms.get(&l).is_some_and(|r| r.server.is_some()),
            _ => false,
        };
        if !allowed {
            return Err(HubError::PresenceInvalid);
        }
        if let Some(s) = self.sessions.get_mut(&member) {
            s.presence = presence;
        }
        self.note_presence(member);
        Ok(())
    }

    /// Sets a session's friends by player key (who may join its friends-only lobbies). Handles are
    /// display names, so a friend is the key its transport proved, never a handle (ADR-8633).
    pub fn set_friends(
        &mut self,
        member: MemberId,
        friends: &[PlayerKey],
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.set_friends_inner(member, friends, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn set_friends_inner(
        &mut self,
        member: MemberId,
        friends: &[PlayerKey],
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        if friends.len() > self.settings.friend_cap {
            return Err(HubError::TooManyFriends);
        }
        let set: BTreeSet<PlayerKey> = friends.iter().copied().collect();
        if let Some(s) = self.sessions.get_mut(&member) {
            s.friends = set;
        }
        Ok(())
    }

    /// Creates a lobby `member` owns and enters it.
    pub fn create(&mut self, member: MemberId, spec: &LobbySpec, now_ms: u64) -> Result<LobbyId, HubError> {
        let result = self.create_inner(member, spec, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn create_inner(&mut self, member: MemberId, spec: &LobbySpec, now_ms: u64) -> Result<LobbyId, HubError> {
        self.touch(member, now_ms)?;
        if self.sessions[&member].lobby.is_some() {
            return Err(LobbyError::AlreadyInLobby.into());
        }
        check_spec(spec)?;
        if spec.max_members > self.settings.max_members {
            return Err(LobbyError::MembersInvalid.into());
        }
        if data_bytes(&spec.data) > self.settings.lobby_data_bytes {
            return Err(HubError::DataTooLarge);
        }
        if self.rooms.len() >= self.settings.lobby_cap {
            return Err(HubError::LobbyCap);
        }
        self.next_lobby += 1;
        let id = LobbyId(self.next_lobby);
        let server = super::server_of(&spec.data);
        self.rooms.insert(
            id,
            Room {
                owner: member,
                visibility: spec.visibility,
                max_members: spec.max_members,
                joinable: spec.joinable,
                data: spec.data.clone(),
                members: vec![RoomMember {
                    id: member,
                    data: BTreeMap::new(),
                }],
                server,
                muted: BTreeSet::new(),
                muted_keys: BTreeSet::new(),
                chat: VecDeque::new(),
                chat_seq: 0,
                chat_times: BTreeMap::new(),
                data_times: Vec::new(),
                offer: None,
                joins: BTreeMap::new(),
            },
        );
        self.enter(member, id);
        Ok(id)
    }

    /// Joins `lobby`. Public and invisible lobbies take anyone, private lobbies an invitee whose
    /// token matches. Friends-only lobbies take a session with a proven player key that the owner
    /// lists as a friend or that an invite was bound to; a session with no player key is refused
    /// (fail closed, ADR-8633). A used invite is spent.
    ///
    /// A private or friends-only lobby the caller may not enter answers
    /// [`LobbyError::NotFound`], as a missing id does: lobby ids are sequential, so distinct codes
    /// would let anyone probe which private lobbies exist (KI-NT-046, ADR-10069). The hub's refusal
    /// counter ([`HubServer::report`]) keeps the real reason, `hub_invite_only` or
    /// `hub_friends_only`, for the operator.
    pub fn join(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        token: Option<InviteToken>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.join_inner(member, lobby, token, now_ms);
        self.count(result.as_ref().err());
        result.map_err(|e| match e {
            HubError::InviteOnly | HubError::FriendsOnly => LobbyError::NotFound.into(),
            other => other,
        })
    }

    fn join_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        token: Option<InviteToken>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        let current = self.sessions[&member].lobby;
        if current == Some(lobby) {
            // Already in: the room again, so a retried join still ends in a full view.
            let room = self.room_state(lobby);
            if let Some(room) = room {
                self.push(member, HubEvent::Room(room));
            }
            return Ok(());
        }
        if current.is_some() {
            return Err(LobbyError::AlreadyInLobby.into());
        }
        let room = self.rooms.get(&lobby).ok_or(LobbyError::NotFound)?;
        let player = self.sessions[&member].identity;
        let invitee = self.invitee_of(member);
        let invited = self
            .invites
            .get(&(lobby, invitee))
            .is_some_and(|i| i.expires_ms > now_ms && token == Some(i.token));
        match room.visibility {
            LobbyVisibility::Public | LobbyVisibility::Invisible => {}
            LobbyVisibility::FriendsOnly => {
                // Only a proven player key counts: a handle is a display name anyone may hold next.
                let Some(player) = player else {
                    return Err(HubError::FriendsOnly);
                };
                let owner_lists = self
                    .sessions
                    .get(&room.owner)
                    .is_some_and(|o| o.friends.contains(&player));
                if !owner_lists && !invited {
                    return Err(HubError::FriendsOnly);
                }
            }
            LobbyVisibility::Private => {
                if !invited {
                    return Err(HubError::InviteOnly);
                }
            }
        }
        if !room.joinable || room.members.len() >= room.max_members as usize {
            return Err(LobbyError::Full.into());
        }
        self.invites.remove(&(lobby, invitee));
        let handle = self.sessions[&member].handle.clone();
        let others: Vec<MemberId> = room.members.iter().map(|m| m.id).collect();
        if let Some(room) = self.rooms.get_mut(&lobby) {
            room.members.push(RoomMember {
                id: member,
                data: BTreeMap::new(),
            });
        }
        for other in others {
            self.push(
                other,
                HubEvent::MemberJoined {
                    lobby,
                    member,
                    handle: handle.clone(),
                },
            );
        }
        self.enter(member, lobby);
        Ok(())
    }

    /// Leaves `lobby` (no-op when not in it). The earliest remaining member owns it next; the last
    /// member out deletes it. When the owner leaves a lobby whose session address it confirmed, the
    /// address is withdrawn (members get `ServerSet` of `None` and go back to `Lobby` presence): it
    /// was the old owner's game, and a member who joins afterwards must not be sent it (ADR-8633).
    pub fn leave(&mut self, member: MemberId, lobby: LobbyId, now_ms: u64) {
        if self.touch(member, now_ms).is_ok() {
            self.leave_inner(member, lobby, LeaveReason::Left);
        }
    }

    fn leave_inner(&mut self, member: MemberId, lobby: LobbyId, reason: LeaveReason) {
        let Some(room) = self.rooms.get_mut(&lobby) else {
            return;
        };
        if !room.has(member) {
            return;
        }
        room.members.retain(|m| m.id != member);
        // A muted player key stays muted (`muted_keys`); the member id is gone with the member.
        room.muted.remove(&member);
        room.chat_times.remove(&member);
        room.joins.remove(&member);
        let remaining: Vec<MemberId> = room.members.iter().map(|m| m.id).collect();
        let new_owner = (room.owner == member)
            .then(|| remaining.first().copied())
            .flatten();
        if let Some(owner) = new_owner {
            room.owner = owner;
            // The old owner's game socket is not the new owner's: its offer and every plan go.
            room.offer = None;
            room.joins.clear();
        }
        let withdrawn = new_owner.is_some() && room.server.is_some();
        if withdrawn {
            room.server = None;
            room.data.remove(keys::SERVER);
        }
        if let Some(s) = self.sessions.get_mut(&member) {
            s.lobby = None;
            s.presence = HubPresence::Menu;
        }
        self.push(member, HubEvent::Left { lobby, reason });
        self.note_presence(member);
        if remaining.is_empty() {
            self.rooms.remove(&lobby);
            self.invites.retain(|(l, _), _| *l != lobby);
            self.note_lobby(lobby);
            return;
        }
        for other in &remaining {
            self.push(*other, HubEvent::MemberLeft { lobby, member });
            if let Some(owner) = new_owner {
                self.push(*other, HubEvent::OwnerChanged { lobby, owner });
            }
            if withdrawn {
                self.push(*other, HubEvent::ServerSet { lobby, server: None });
                let changed = self.sessions.get_mut(other).is_some_and(|s| {
                    let changed = s.presence == HubPresence::InGame;
                    if changed {
                        s.presence = HubPresence::Lobby;
                    }
                    changed
                });
                if changed {
                    self.note_presence(*other);
                }
            }
        }
        self.note_lobby(lobby);
    }

    /// Sets (`Some`) or deletes (`None`) lobby data; owner only. `regia.server` is the session
    /// address: setting it is [`HubServer::confirm_session`].
    pub fn set_data(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        key: &str,
        value: Option<&str>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.set_data_inner(member, lobby, key, value, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn set_data_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        key: &str,
        value: Option<&str>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        check_key(key)?;
        if let Some(v) = value {
            check_value(v)?;
        }
        if key == keys::SERVER {
            let server = match value {
                Some(v) => Some(LobbyServer::from_key(v).ok_or(LobbyError::ValueTooLong)?),
                None => None,
            };
            return self.confirm_inner(member, lobby, server, now_ms);
        }
        let budget = self.settings.lobby_data_bytes;
        let room = self.owned(member, lobby)?;
        let mut next = room.data.clone();
        match value {
            Some(v) => {
                next.insert(key.to_owned(), v.to_owned());
            }
            None => {
                next.remove(key);
            }
        }
        if next.len() > MAX_KEYS {
            return Err(LobbyError::TooManyKeys.into());
        }
        if data_bytes(&next) > budget {
            return Err(HubError::DataTooLarge);
        }
        if next == room.data {
            return Ok(());
        }
        self.admit_write(member, lobby, now_ms)?;
        if let Some(room) = self.rooms.get_mut(&lobby) {
            room.data = next;
        }
        let members = self.member_ids(lobby);
        for m in members {
            self.push(
                m,
                HubEvent::Data {
                    lobby,
                    key: key.to_owned(),
                    value: value.map(str::to_owned),
                },
            );
        }
        self.note_lobby(lobby);
        Ok(())
    }

    /// Sets or deletes `member`'s own data in `lobby`.
    pub fn set_member_data(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        key: &str,
        value: Option<&str>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.set_member_data_inner(member, lobby, key, value, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn set_member_data_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        key: &str,
        value: Option<&str>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        check_key(key)?;
        if let Some(v) = value {
            check_value(v)?;
        }
        let budget = self.settings.member_data_bytes;
        let room = self.rooms.get(&lobby).ok_or(LobbyError::NotInLobby)?;
        let entry = room
            .members
            .iter()
            .find(|m| m.id == member)
            .ok_or(LobbyError::NotInLobby)?;
        let mut next = entry.data.clone();
        match value {
            Some(v) => {
                next.insert(key.to_owned(), v.to_owned());
            }
            None => {
                next.remove(key);
            }
        }
        if next.len() > MAX_KEYS {
            return Err(LobbyError::TooManyKeys.into());
        }
        if data_bytes(&next) > budget {
            return Err(HubError::DataTooLarge);
        }
        if next == entry.data {
            return Ok(());
        }
        self.admit_write(member, lobby, now_ms)?;
        if let Some(entry) = self
            .rooms
            .get_mut(&lobby)
            .and_then(|r| r.members.iter_mut().find(|m| m.id == member))
        {
            entry.data = next;
        }
        for m in self.member_ids(lobby) {
            self.push(
                m,
                HubEvent::MemberData {
                    lobby,
                    member,
                    key: key.to_owned(),
                    value: value.map(str::to_owned),
                },
            );
        }
        Ok(())
    }

    /// Opens or closes a lobby to joins; owner only.
    pub fn set_joinable(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        joinable: bool,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.touch(member, now_ms).and_then(|()| {
            if self.owned(member, lobby)?.joinable == joinable {
                return Ok(false);
            }
            self.admit_write(member, lobby, now_ms)?;
            if let Some(room) = self.rooms.get_mut(&lobby) {
                room.joinable = joinable;
            }
            Ok(true)
        });
        self.count(result.as_ref().err());
        if result? {
            for m in self.member_ids(lobby) {
                self.push(m, HubEvent::Joinable { lobby, joinable });
            }
            self.note_lobby(lobby);
        }
        Ok(())
    }

    /// The owner confirms the lobby's game session at `server` (`None` withdraws it). This is the
    /// only way a session address enters the hub, and it goes to the lobby's members only; members
    /// move to [`HubPresence::InGame`] (back to `Lobby` on a withdrawal).
    pub fn confirm_session(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        server: Option<LobbyServer>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self
            .touch(member, now_ms)
            .and_then(|()| self.confirm_inner(member, lobby, server, now_ms));
        self.count(result.as_ref().err());
        result
    }

    fn confirm_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        server: Option<LobbyServer>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let budget = self.settings.lobby_data_bytes;
        let room = self.owned(member, lobby)?;
        if room.server == server {
            return Ok(());
        }
        // The address is lobby data like any other key: the same key-count and byte budget hold.
        let mut next = room.data.clone();
        match server {
            Some(s) => next.insert(keys::SERVER.to_owned(), s.to_key()),
            None => next.remove(keys::SERVER),
        };
        if next.len() > MAX_KEYS {
            return Err(LobbyError::TooManyKeys.into());
        }
        if data_bytes(&next) > budget {
            return Err(HubError::DataTooLarge);
        }
        self.admit_write(member, lobby, now_ms)?;
        if let Some(room) = self.rooms.get_mut(&lobby) {
            room.server = server;
            room.data = next;
        }
        let presence = if server.is_some() {
            HubPresence::InGame
        } else {
            HubPresence::Lobby
        };
        for m in self.member_ids(lobby) {
            self.push(m, HubEvent::ServerSet { lobby, server });
            let changed = self.sessions.get_mut(&m).is_some_and(|s| {
                let changed = s.presence != presence;
                s.presence = presence;
                changed
            });
            if changed {
                self.note_presence(m);
            }
        }
        Ok(())
    }

    /// Invites the session with `handle` to `lobby`; owner only. Returns the card the invitee was
    /// sent. A newer invite replaces an older one for the same invitee and lobby.
    pub fn invite(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        handle: &str,
        now_ms: u64,
    ) -> Result<InviteCard, HubError> {
        let result = self.invite_inner(member, lobby, handle, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn invite_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        handle: &str,
        now_ms: u64,
    ) -> Result<InviteCard, HubError> {
        self.touch(member, now_ms)?;
        let handle = clean_handle(handle, self.settings.handle_max_bytes)?;
        self.owned(member, lobby)?;
        let to = *self.by_handle.get(&fold(&handle)).ok_or(HubError::NoSuchPlayer)?;
        let invitee = self.invitee_of(to);
        if self.invites.len() >= self.settings.invite_cap && !self.invites.contains_key(&(lobby, invitee)) {
            self.evict_invite();
        }
        self.next_invite += 1;
        let token = self.mint_token(lobby, invitee, self.next_invite);
        let expires_ms = now_ms.saturating_add(self.settings.invite_ttl_ms);
        let from_handle = self.sessions[&member].handle.clone();
        self.invites.insert(
            (lobby, invitee),
            Invite {
                token,
                expires_ms,
                serial: self.next_invite,
                from: member,
                from_handle: from_handle.clone(),
            },
        );
        let card = InviteCard {
            lobby,
            from: member,
            from_handle,
            token,
            expires_ms,
        };
        self.push(to, HubEvent::Invite(card.clone()));
        Ok(card)
    }

    /// Mutes or unmutes `target` in `lobby` chat; owner only, never the owner. A mute holds on the
    /// target's player key too, so leaving and joining again (under any member id) keeps it.
    pub fn mute(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        target: MemberId,
        muted: bool,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let target_key = self.identity(target);
        let result = self.touch(member, now_ms).and_then(|()| {
            let room = self.owned(member, lobby)?;
            if target == room.owner {
                return Err(HubError::CannotMuteOwner);
            }
            if !room.has(target) {
                return Err(LobbyError::NotInLobby.into());
            }
            if muted {
                room.muted.insert(target);
                room.muted_keys.extend(target_key);
            } else {
                room.muted.remove(&target);
                if let Some(k) = target_key {
                    room.muted_keys.remove(&k);
                }
            }
            Ok(())
        });
        self.count(result.as_ref().err());
        result
    }

    /// Sends a chat line to every session (global) or to a lobby's members. Each session may send
    /// [`HubSettings::chat_per_sec`] lines per second per scope.
    pub fn chat(
        &mut self,
        member: MemberId,
        scope: ChatScope,
        text: &str,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.chat_inner(member, scope, text, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn chat_inner(
        &mut self,
        member: MemberId,
        scope: ChatScope,
        text: &str,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        let text = text.trim();
        if check_chat(text).is_err() || text.chars().count() > self.settings.chat_max_chars {
            return Err(HubError::ChatInvalid);
        }
        let per_sec = self.settings.chat_per_sec;
        let history = self.settings.chat_history;
        let from_handle = self
            .sessions
            .get(&member)
            .map(|s| s.handle.clone())
            .unwrap_or_default();
        let (recipients, mark): (Vec<MemberId>, ChatMark) = match scope {
            ChatScope::Global => {
                // Every session receives every global line: the hub as a whole takes a bounded rate.
                let hub_rate = self.settings.global_chat_per_sec;
                self.global_chat_times
                    .retain(|t| now_ms.saturating_sub(*t) < 1000);
                if self.global_chat_times.len() >= hub_rate {
                    return Err(HubError::ChatRateLimited);
                }
                let session = self.sessions.get_mut(&member).ok_or(HubError::NoSession)?;
                admit(&mut session.chat_times, now_ms, per_sec)?;
                self.global_chat_times.push(now_ms);
                self.global_chat_seq += 1;
                let seq = self.global_chat_seq;
                self.global_chat
                    .push_back((seq, member, from_handle.clone(), text.to_owned()));
                while self.global_chat.len() > history {
                    self.global_chat.pop_front();
                }
                (self.sessions.keys().copied().collect(), ChatMark::Global(seq))
            }
            ChatScope::Lobby(lobby) => {
                let key = self.sessions.get(&member).and_then(|s| s.identity);
                let room = self.rooms.get_mut(&lobby).ok_or(LobbyError::NotInLobby)?;
                if !room.has(member) {
                    return Err(LobbyError::NotInLobby.into());
                }
                if room.muted.contains(&member) || key.is_some_and(|k| room.muted_keys.contains(&k)) {
                    return Err(HubError::Muted);
                }
                admit(room.chat_times.entry(member).or_default(), now_ms, per_sec)?;
                room.chat_seq += 1;
                let seq = room.chat_seq;
                room.chat
                    .push_back((seq, member, from_handle.clone(), text.to_owned()));
                while room.chat.len() > history {
                    room.chat.pop_front();
                }
                (room.members.iter().map(|m| m.id).collect(), ChatMark::Lobby(seq))
            }
        };
        let event = HubEvent::Chat {
            scope,
            from: member,
            from_handle,
            text: text.to_owned(),
        };
        for r in recipients {
            self.push_marked(r, event.clone(), mark);
        }
        Ok(())
    }

    /// The lobby's kept chat lines, oldest first (at most [`HubSettings::chat_history`]).
    pub fn chat_history(&self, lobby: LobbyId) -> Vec<(MemberId, String)> {
        self.rooms
            .get(&lobby)
            .map(|r| r.chat.iter().map(|(_, m, _, t)| (*m, t.clone())).collect())
            .unwrap_or_default()
    }

    /// The owner offers how its game socket is reached (join brokering). A new offer replaces
    /// the old one; plans already sent stand.
    pub fn offer_reach(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        offer: ReachOffer,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.offer_inner(member, lobby, offer, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn offer_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        mut offer: ReachOffer,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        check_offer(&mut offer)?;
        self.owned(member, lobby)?.offer = Some(offer);
        Ok(())
    }

    /// A member asks for a path to the owner's game session, with its own game socket's
    /// candidates and NAT behavior. The owner is sent a [`HubEvent::ReachAsk`] and approves with
    /// [`HubServer::approve`].
    pub fn ask_reach(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        offer: ReachOffer,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.ask_inner(member, lobby, offer, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn ask_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        mut offer: ReachOffer,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        check_offer(&mut offer)?;
        // A member's ask carries its candidates and NAT only: the relay and ticket are the owner's.
        offer.relay = None;
        offer.join_code = None;
        offer.ticket.clear();
        let (handle, identity) = {
            let s = &self.sessions[&member];
            (s.handle.clone(), s.identity.unwrap_or_default())
        };
        let room = self.rooms.get_mut(&lobby).ok_or(LobbyError::NotInLobby)?;
        if !room.has(member) {
            return Err(LobbyError::NotInLobby.into());
        }
        if room.owner == member {
            return Err(LobbyError::NotOwner.into());
        }
        if room.offer.is_none() {
            return Err(HubError::NoReach);
        }
        let owner = room.owner;
        room.joins.insert(
            member,
            Join {
                offer: offer.clone(),
                state: JoinState::Asked,
                planned: None,
                chosen: None,
                punch_attempts: 0,
                member_plan_lost: None,
                owner_plan_lost: None,
            },
        );
        self.push(
            owner,
            HubEvent::ReachAsk {
                lobby,
                member,
                handle,
                identity,
                offer,
            },
        );
        Ok(())
    }

    /// The owner approves (or denies) `target`'s ask. On approval the broker chooses the path
    /// (mapped, punch, relay; [`super::join::broker_reach`]) and each side is sent a
    /// [`HubEvent::Plan`] carrying the other's candidates; the member's plan also carries the
    /// relay, its join code and the owner's ticket. Returns the member's plan.
    pub fn approve(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        target: MemberId,
        approve: bool,
        now_ms: u64,
    ) -> Result<Option<JoinPlan>, HubError> {
        let result = self.approve_inner(member, lobby, target, approve, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn approve_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        target: MemberId,
        approve: bool,
        now_ms: u64,
    ) -> Result<Option<JoinPlan>, HubError> {
        self.touch(member, now_ms)?;
        let owner_identity = self.identity(member).unwrap_or_default();
        let target_identity = self.identity(target).unwrap_or_default();
        let room = self.owned(member, lobby)?;
        let owner_offer = room.offer.clone().ok_or(HubError::NoReach)?;
        let join = room
            .joins
            .get_mut(&target)
            .filter(|j| j.state == JoinState::Asked)
            .ok_or(HubError::NoAsk)?;
        if !approve {
            join.state = JoinState::Denied;
            self.push(target, HubEvent::ReachDenied { lobby });
            return Ok(None);
        }
        let path = broker_reach(&owner_offer, join.offer.nat).map_err(|_| HubError::NoReach)?;
        join.state = JoinState::Planned;
        join.planned = Some(path);
        let member_candidates = join.offer.candidates.clone();
        let for_member = JoinPlan {
            lobby,
            peer: member,
            peer_identity: owner_identity,
            path,
            mapped: (path == ReachKind::Mapped)
                .then(|| owner_offer.mapped())
                .flatten(),
            candidates: owner_offer.candidates.clone(),
            relay: owner_offer.relay,
            join_code: owner_offer.join_code.clone(),
            ticket: owner_offer.ticket.clone(),
        };
        let for_owner = JoinPlan {
            peer: target,
            peer_identity: target_identity,
            mapped: None,
            candidates: member_candidates,
            ticket: Vec::new(),
            ..for_member.clone()
        };
        let member_lost = !self.push_marked(target, HubEvent::Plan(for_member.clone()), ChatMark::None);
        let owner_lost = !self.push_marked(member, HubEvent::Plan(for_owner.clone()), ChatMark::None);
        // A plan dropped at the outbox cap is kept for the resync, so nobody asks again
        // (KI-NT-045, ADR-10069).
        if let Some(join) = self.rooms.get_mut(&lobby).and_then(|r| r.joins.get_mut(&target)) {
            join.member_plan_lost = member_lost.then(|| for_member.clone());
            join.owner_plan_lost = owner_lost.then_some(for_owner);
        }
        Ok(Some(for_member))
    }

    /// The member reports the path that carried its sealed handshake; the owner is told.
    pub fn reach_done(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        path: ReachKind,
        punch_attempts: u32,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.touch(member, now_ms).and_then(|()| {
            let room = self.rooms.get_mut(&lobby).ok_or(LobbyError::NotInLobby)?;
            let owner = room.owner;
            let join = room
                .joins
                .get_mut(&member)
                .filter(|j| matches!(j.state, JoinState::Planned | JoinState::Reached))
                .ok_or(HubError::NoAsk)?;
            join.state = JoinState::Reached;
            join.chosen = Some(path);
            join.punch_attempts = punch_attempts;
            Ok(owner)
        });
        self.count(result.as_ref().err());
        let owner = result?;
        self.push(
            owner,
            HubEvent::Reached {
                lobby,
                member,
                path,
                punch_attempts,
            },
        );
        Ok(())
    }

    /// Every brokered join, in lobby then member order.
    pub fn joins(&self) -> Vec<JoinRow> {
        let mut out = Vec::new();
        for (lobby, room) in &self.rooms {
            for (member, j) in &room.joins {
                let session = self.sessions.get(member);
                out.push(JoinRow {
                    lobby: *lobby,
                    member: *member,
                    handle: session.map(|s| s.handle.clone()).unwrap_or_default(),
                    identity: session
                        .and_then(|s| s.identity)
                        .map(|k| k.short())
                        .unwrap_or_default(),
                    platform: room
                        .members
                        .iter()
                        .find(|m| m.id == *member)
                        .and_then(|m| m.data.get("platform").cloned()),
                    nat: j.offer.nat.map(super::join::NatClass::name),
                    state: j.state.name().to_owned(),
                    planned: j.planned,
                    chosen: j.chosen,
                    punch_attempts: j.punch_attempts,
                    relayed: j.chosen == Some(ReachKind::Relay),
                });
            }
        }
        out
    }

    /// Records the generations a client says it has (on a list request or a reconnect): the next
    /// flush sends it what changed since. A generation the hub never reached is ignored. A cursor
    /// behind what the session was sent (a rewind: it can force a snapshot rebuild) is taken at
    /// most once per [`HubSettings::list_rewind_ms`] of the session's clock (its last request); a
    /// rewind inside that window is ignored and counted as `hub_list_rewind` (ADR-8633).
    pub fn note_cursor(&mut self, member: MemberId, dir_gen: u64, presence_gen: u64) {
        let (dir_now, presence_now) = (self.dir_generation, self.presence_generation);
        let window = self.settings.list_rewind_ms;
        let Some(s) = self.sessions.get_mut(&member) else {
            return;
        };
        if dir_gen > dir_now || presence_gen > presence_now {
            return;
        }
        let rewinds = s
            .dir_sent
            .is_some_and(|d| dir_gen < d || presence_gen < s.presence_sent);
        if rewinds {
            if s.rewind_ms.is_some_and(|t| s.last_ms.saturating_sub(t) < window) {
                self.note_refused("hub_list_rewind");
                return;
            }
            s.rewind_ms = Some(s.last_ms);
        }
        s.dir_sent = Some(dir_gen);
        s.presence_sent = presence_gen;
    }

    /// Queues every session's directory update: a delta when the change logs cover its cursor, a
    /// snapshot otherwise, nothing when it is current. Pages stay near 32 KiB.
    pub fn flush(&mut self) {
        let ids: Vec<MemberId> = self.sessions.keys().copied().collect();
        for id in ids {
            self.flush_one(id);
        }
    }

    /// Queues `member`'s directory update now (as [`HubServer::flush`] does for every session).
    pub fn flush_member(&mut self, member: MemberId) {
        if self.sessions.contains_key(&member) {
            self.flush_one(member);
        }
    }

    /// Drains the events queued for `member`. When events were dropped at the outbox cap since the
    /// last drain, the drain ends with a resync ([`HubServer::resync`]'s events: its room, its
    /// invites, and a directory snapshot at the next flush) and the overflow is cleared: a burst
    /// the session has read does not drop it (ADR-8633).
    pub fn take_events(&mut self, member: MemberId) -> Vec<HubEvent> {
        let Some(s) = self.sessions.get_mut(&member) else {
            return Vec::new();
        };
        let overflowed = std::mem::take(&mut s.overflowed);
        s.drained_overflow = overflowed;
        let mut out = std::mem::take(&mut s.outbox);
        if overflowed {
            out.extend(self.resync_events(member).into_iter().map(|(e, _)| e));
        }
        out
    }

    /// Queues a resync for `member`: its room as it stands, every live invite addressed to it, the
    /// open asks of a lobby it owns, and a directory snapshot at the next flush. For a session whose
    /// event stream had a gap (an outbox overflow, or a link that resumed after dormancy).
    pub fn resync(&mut self, member: MemberId) {
        for (event, mark) in self.resync_events(member) {
            self.push_marked(member, event, mark);
        }
    }

    /// The resync's events. Besides the state above: the kept chat lines from the first one
    /// dropped at the outbox cap on (lobby and global, oldest first), and a join plan that was
    /// dropped (KI-NT-045, ADR-10069). The gaps and lost plans are cleared: the caller delivers
    /// these, and a drop on the way marks the gap again.
    fn resync_events(&mut self, member: MemberId) -> Vec<(HubEvent, ChatMark)> {
        let Some(s) = self.sessions.get_mut(&member) else {
            return Vec::new();
        };
        s.dir_sent = None;
        let lobby = s.lobby;
        let lobby_gap = s.lobby_chat_gap.take();
        let global_gap = s.global_chat_gap.take();
        let mut out = Vec::new();
        if let Some(lobby) = lobby {
            if let Some(room) = self.room_state(lobby) {
                out.push((HubEvent::Room(room), ChatMark::None));
            }
            if let Some(room) = self.rooms.get_mut(&lobby) {
                let owner = room.owner == member;
                for (asker, join) in room.joins.iter_mut() {
                    if *asker == member
                        && let Some(plan) = join.member_plan_lost.take()
                    {
                        out.push((HubEvent::Plan(plan), ChatMark::None));
                    }
                    if owner && let Some(plan) = join.owner_plan_lost.take() {
                        out.push((HubEvent::Plan(plan), ChatMark::None));
                    }
                }
            }
            if let Some(room) = self.rooms.get(&lobby).filter(|r| r.owner == member) {
                for (asker, join) in room.joins.iter().filter(|(_, j)| j.state == JoinState::Asked) {
                    if let Some(a) = self.sessions.get(asker) {
                        out.push((
                            HubEvent::ReachAsk {
                                lobby,
                                member: *asker,
                                handle: a.handle.clone(),
                                identity: a.identity.unwrap_or_default(),
                                offer: join.offer.clone(),
                            },
                            ChatMark::None,
                        ));
                    }
                }
            }
            if let (Some(gap), Some(room)) = (lobby_gap, self.rooms.get(&lobby)) {
                for (seq, from, handle, text) in room.chat.iter().filter(|l| l.0 >= gap) {
                    out.push((
                        HubEvent::Chat {
                            scope: ChatScope::Lobby(lobby),
                            from: *from,
                            from_handle: handle.clone(),
                            text: text.clone(),
                        },
                        ChatMark::Lobby(*seq),
                    ));
                }
            }
        }
        if let Some(gap) = global_gap {
            for (seq, from, handle, text) in self.global_chat.iter().filter(|l| l.0 >= gap) {
                out.push((
                    HubEvent::Chat {
                        scope: ChatScope::Global,
                        from: *from,
                        from_handle: handle.clone(),
                        text: text.clone(),
                    },
                    ChatMark::Global(*seq),
                ));
            }
        }
        let invitee = self.invitee_of(member);
        for ((l, to), i) in &self.invites {
            if *to == invitee {
                out.push((
                    HubEvent::Invite(InviteCard {
                        lobby: *l,
                        from: i.from,
                        from_handle: i.from_handle.clone(),
                        token: i.token,
                        expires_ms: i.expires_ms,
                    }),
                    ChatMark::None,
                ));
            }
        }
        out
    }

    /// Puts events the link would not take yet (back-pressure or the send rate) back at the front
    /// of `member`'s outbox, for the next drain. A session whose drain carried a resync and still
    /// could not send it, or whose outbox then passes its cap, is marked overflowed: it cannot keep
    /// up, and the next [`HubServer::expire`] drops it.
    pub(super) fn requeue(&mut self, member: MemberId, mut rest: Vec<HubEvent>) {
        let cap = self.settings.outbox_cap;
        let Some(s) = self.sessions.get_mut(&member) else {
            return;
        };
        if rest.is_empty() {
            return;
        }
        rest.append(&mut s.outbox);
        if rest.len() > cap || s.drained_overflow {
            rest.truncate(cap);
            s.overflowed = true;
        }
        s.outbox = rest;
    }

    /// Queues one answer for `member` (a reply or a list's end marker) after the events before it.
    /// Answers are not dropped at the outbox cap (a resync does not restore them); there is one per
    /// request the session sent, and a session whose link cannot take them is dropped at
    /// [`HubServer::expire`] through [`HubServer::requeue`].
    pub(super) fn queue(&mut self, member: MemberId, event: HubEvent) {
        if let Some(s) = self.sessions.get_mut(&member) {
            self.other_bytes += event.wire_len() as u64;
            s.outbox.push(event);
        }
    }

    /// Every session's presence row, in member order.
    pub fn players(&self) -> Vec<PresenceRow> {
        self.sessions
            .keys()
            .filter_map(|id| self.presence_row(*id))
            .collect()
    }

    /// Every listed lobby's card, in id order.
    pub fn lobbies(&self) -> Vec<HubLobbyCard> {
        self.rooms
            .iter()
            .filter(|(_, r)| r.listed())
            .filter_map(|(id, _)| self.card(*id))
            .collect()
    }

    /// A lobby with its members, as its members see it (session address included); `None` for an
    /// unknown lobby.
    pub fn lobby_info(&self, lobby: LobbyId) -> Option<LobbyInfo> {
        let room = self.rooms.get(&lobby)?;
        Some(LobbyInfo {
            id: lobby,
            owner: room.owner,
            visibility: room.visibility,
            max_members: room.max_members,
            joinable: room.joinable,
            members: room
                .members
                .iter()
                .map(|m| LobbyMember {
                    id: m.id,
                    name: self
                        .sessions
                        .get(&m.id)
                        .map(|s| s.handle.clone())
                        .unwrap_or_default(),
                    data: m.data.clone(),
                })
                .collect(),
            member_count: room.members.len() as u32,
            data: room.data.clone(),
            server: room.server,
        })
    }

    /// The member id of the session with `handle`.
    pub fn member_by_handle(&self, handle: &str) -> Option<MemberId> {
        self.by_handle.get(&fold(handle)).copied()
    }

    /// The directory and presence generations.
    pub fn generations(&self) -> (u64, u64) {
        (self.dir_generation, self.presence_generation)
    }

    /// The core's estimated memory in bytes: its structs, every string it holds, queued events and
    /// the change logs. Bounded by the settings' caps.
    pub fn state_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<Self>();
        for s in self.sessions.values() {
            total += std::mem::size_of::<Session>() + 2 * s.handle.len() + s.chat_times.len() * 8;
            total += s.friends.len() * 32 + s.data_times.len() * 8;
            total += s.outbox.iter().map(HubEvent::wire_len).sum::<usize>();
        }
        for r in self.rooms.values() {
            total += std::mem::size_of::<Room>() + data_bytes(&r.data);
            total += r
                .members
                .iter()
                .map(|m| std::mem::size_of::<RoomMember>() + data_bytes(&m.data))
                .sum::<usize>();
            total += r
                .chat
                .iter()
                .map(|(_, _, h, t)| h.len() + t.len() + 40)
                .sum::<usize>();
            total += r.muted.len() * 8 + r.chat_times.values().map(|v| v.len() * 8 + 16).sum::<usize>();
            total += r.muted_keys.len() * 32 + r.data_times.len() * 8;
            total += r
                .joins
                .values()
                .map(|j| std::mem::size_of::<Join>() + j.offer.candidates.len() * 32)
                .sum::<usize>();
            total += r
                .offer
                .as_ref()
                .map_or(0, |o| 64 + o.candidates.len() * 32 + o.ticket.len());
        }
        total += self
            .invites
            .values()
            .map(|i| std::mem::size_of::<Invite>() + 48 + i.from_handle.len())
            .sum::<usize>();
        total += self.global_chat_times.len() * 8;
        total += self
            .global_chat
            .iter()
            .map(|(_, _, h, t)| h.len() + t.len() + 40)
            .sum::<usize>();
        total += (self.dir_log.len() + self.presence_log.len()) * 16;
        total
    }

    /// The agent and log report.
    pub fn report(&self) -> HubReport {
        HubReport {
            sessions: self.sessions.len(),
            lobbies: self.rooms.len(),
            listed: self.rooms.values().filter(|r| r.listed()).count(),
            invites: self.invites.len(),
            directory_generation: self.dir_generation,
            presence_generation: self.presence_generation,
            state_bytes: self.state_bytes(),
            directory_bytes: self.directory_bytes,
            chat_bytes: self.chat_bytes,
            other_bytes: self.other_bytes,
            expired: self.expired,
            refused: self.refused.clone(),
            players: self
                .sessions
                .keys()
                .take(REPORT_ROWS)
                .filter_map(|id| self.presence_row(*id))
                .collect(),
            games: self.lobbies().into_iter().take(REPORT_ROWS).collect(),
            joins: self.joins().into_iter().take(REPORT_JOINS).collect(),
        }
    }

    /// Notes a refusal that happened outside the core (a request from a peer without a session, a
    /// message that did not decode).
    pub fn note_refused(&mut self, code: &str) {
        *self.refused.entry(code.to_owned()).or_default() += 1;
    }

    // ---- internals ------------------------------------------------------------------------

    fn count(&mut self, err: Option<&HubError>) {
        if let Some(e) = err {
            self.note_refused(e.code());
        }
    }

    /// Who an invite to `member` is for: its player key when it has one.
    fn invitee_of(&self, member: MemberId) -> Invitee {
        match self.sessions.get(&member).and_then(|s| s.identity) {
            Some(k) => Invitee::Player(k),
            None => Invitee::Member(member),
        }
    }

    /// At the hub's invite cap: the oldest invite of the lobby holding the most invites goes, so
    /// one lobby inviting in bulk discards its own invites, not another lobby's (ADR-8633).
    fn evict_invite(&mut self) {
        let mut per_lobby: BTreeMap<LobbyId, (usize, u64, Invitee)> = BTreeMap::new();
        for ((l, to), i) in &self.invites {
            let e = per_lobby.entry(*l).or_insert((0, i.serial, *to));
            e.0 += 1;
            if i.serial < e.1 {
                (e.1, e.2) = (i.serial, *to);
            }
        }
        let victim = per_lobby
            .iter()
            .max_by_key(|(l, (n, _, _))| (*n, std::cmp::Reverse(**l)))
            .map(|(l, (_, _, to))| (*l, *to));
        if let Some(key) = victim {
            self.invites.remove(&key);
        }
    }

    /// Admits one lobby write (data, member data, session address, joinable) by `member` in
    /// `lobby`: at most [`HubSettings::data_per_sec`] per session and
    /// [`HubSettings::lobby_data_per_sec`] per lobby, over the last second.
    fn admit_write(&mut self, member: MemberId, lobby: LobbyId, now_ms: u64) -> Result<(), HubError> {
        let (per_member, per_lobby) = (self.settings.data_per_sec, self.settings.lobby_data_per_sec);
        let s = self.sessions.get_mut(&member).ok_or(HubError::NoSession)?;
        let room = self.rooms.get_mut(&lobby).ok_or(LobbyError::NotInLobby)?;
        s.data_times.retain(|t| now_ms.saturating_sub(*t) < 1000);
        room.data_times.retain(|t| now_ms.saturating_sub(*t) < 1000);
        if s.data_times.len() >= per_member || room.data_times.len() >= per_lobby {
            return Err(HubError::DataRateLimited);
        }
        s.data_times.push(now_ms);
        room.data_times.push(now_ms);
        Ok(())
    }

    fn touch(&mut self, member: MemberId, now_ms: u64) -> Result<(), HubError> {
        let s = self.sessions.get_mut(&member).ok_or(HubError::NoSession)?;
        s.last_ms = s.last_ms.max(now_ms);
        Ok(())
    }

    fn owned(&mut self, member: MemberId, lobby: LobbyId) -> Result<&mut Room, HubError> {
        let room = self.rooms.get_mut(&lobby).ok_or(LobbyError::NotInLobby)?;
        if !room.has(member) {
            return Err(LobbyError::NotInLobby.into());
        }
        if room.owner != member {
            return Err(LobbyError::NotOwner.into());
        }
        Ok(room)
    }

    fn enter(&mut self, member: MemberId, lobby: LobbyId) {
        let in_game = self.rooms.get(&lobby).is_some_and(|r| r.server.is_some());
        if let Some(s) = self.sessions.get_mut(&member) {
            s.lobby = Some(lobby);
            // Lines dropped in an earlier lobby are not this lobby's.
            s.lobby_chat_gap = None;
            s.presence = if in_game {
                HubPresence::InGame
            } else {
                HubPresence::Lobby
            };
        }
        if let Some(room) = self.room_state(lobby) {
            self.push(member, HubEvent::Room(room));
        }
        self.note_presence(member);
        self.note_lobby(lobby);
    }

    fn room_state(&self, lobby: LobbyId) -> Option<RoomState> {
        let room = self.rooms.get(&lobby)?;
        Some(RoomState {
            lobby,
            owner: room.owner,
            visibility: room.visibility,
            max_members: room.max_members,
            joinable: room.joinable,
            data: room.data.clone(),
            members: room
                .members
                .iter()
                .map(|m| {
                    let handle = self
                        .sessions
                        .get(&m.id)
                        .map(|s| s.handle.clone())
                        .unwrap_or_default();
                    (m.id, handle, m.data.clone())
                })
                .collect(),
            server: room.server,
        })
    }

    fn member_ids(&self, lobby: LobbyId) -> Vec<MemberId> {
        self.rooms
            .get(&lobby)
            .map(|r| r.members.iter().map(|m| m.id).collect())
            .unwrap_or_default()
    }

    fn presence_row(&self, member: MemberId) -> Option<PresenceRow> {
        let s = self.sessions.get(&member)?;
        let lobby = s.lobby.filter(|l| self.rooms.get(l).is_some_and(Room::listed));
        Some(PresenceRow {
            member,
            handle: s.handle.clone(),
            presence: s.presence,
            lobby,
        })
    }

    fn card(&self, lobby: LobbyId) -> Option<HubLobbyCard> {
        let room = self.rooms.get(&lobby).filter(|r| r.listed())?;
        Some(HubLobbyCard {
            id: lobby,
            owner: room.owner,
            owner_handle: self
                .sessions
                .get(&room.owner)
                .map(|s| s.handle.clone())
                .unwrap_or_default(),
            visibility: room.visibility,
            max_members: room.max_members,
            member_count: room.members.len() as u32,
            joinable: room.joinable,
            data: public_data(&room.data),
        })
    }

    fn mint_token(&self, lobby: LobbyId, to: Invitee, serial: u64) -> InviteToken {
        let mut h = blake3::Hasher::new_keyed(&self.secret);
        h.update(b"regia.lobby_hub.invite");
        h.update(&lobby.0.to_le_bytes());
        match to {
            Invitee::Member(m) => {
                h.update(&[0]);
                h.update(&m.0.to_le_bytes());
            }
            Invitee::Player(k) => {
                h.update(&[1]);
                h.update(&k.0);
            }
        }
        h.update(&serial.to_le_bytes());
        let mut out = [0u8; 16];
        out.copy_from_slice(&h.finalize().as_bytes()[..16]);
        InviteToken(out)
    }

    fn note_lobby(&mut self, lobby: LobbyId) {
        // Only the directory's lobbies move its generation: a listed lobby, or one leaving it.
        let listed_now = self.rooms.get(&lobby).is_some_and(Room::listed);
        let was_listed = if listed_now {
            !self.in_directory.insert(lobby)
        } else {
            self.in_directory.remove(&lobby)
        };
        if !listed_now && !was_listed {
            return;
        }
        self.dir_generation += 1;
        self.dir_log.push_back((self.dir_generation, lobby));
        while self.dir_log.len() > self.settings.changelog {
            if let Some((g, _)) = self.dir_log.pop_front() {
                self.dir_floor = g;
            }
        }
    }

    fn note_presence(&mut self, member: MemberId) {
        self.presence_generation += 1;
        self.presence_log.push_back((self.presence_generation, member));
        while self.presence_log.len() > self.settings.changelog {
            if let Some((g, _)) = self.presence_log.pop_front() {
                self.presence_floor = g;
            }
        }
    }

    fn push(&mut self, member: MemberId, event: HubEvent) {
        self.push_marked(member, event, ChatMark::None);
    }

    /// Queues `event`; true when it was queued. At the outbox cap it is dropped, the session is
    /// marked overflowed, and a dropped chat line marks its history's gap (KI-NT-045).
    fn push_marked(&mut self, member: MemberId, event: HubEvent, mark: ChatMark) -> bool {
        let cap = self.settings.outbox_cap;
        let Some(s) = self.sessions.get_mut(&member) else {
            return false;
        };
        if s.outbox.len() >= cap {
            s.overflowed = true;
            match mark {
                ChatMark::None => {}
                ChatMark::Lobby(seq) => {
                    s.lobby_chat_gap = Some(s.lobby_chat_gap.map_or(seq, |g| g.min(seq)));
                }
                ChatMark::Global(seq) => {
                    s.global_chat_gap = Some(s.global_chat_gap.map_or(seq, |g| g.min(seq)));
                }
            }
            return false;
        }
        let n = event.wire_len() as u64;
        match &event {
            HubEvent::Directory(_) => self.directory_bytes += n,
            HubEvent::Chat { .. } => self.chat_bytes += n,
            _ => self.other_bytes += n,
        }
        s.outbox.push(event);
        true
    }

    fn flush_one(&mut self, member: MemberId) {
        let Some(s) = self.sessions.get(&member) else {
            return;
        };
        let (dir_now, presence_now) = (self.dir_generation, self.presence_generation);
        let (dir_sent, presence_sent) = (s.dir_sent, s.presence_sent);
        if dir_sent == Some(dir_now) && presence_sent == presence_now {
            return;
        }
        let covered = dir_sent.is_some_and(|d| d >= self.dir_floor) && presence_sent >= self.presence_floor;
        let pages = if covered {
            self.delta_pages(dir_sent.unwrap_or(0), presence_sent)
        } else {
            self.snapshot_pages()
        };
        for page in pages {
            self.push(member, HubEvent::Directory(page));
        }
        if let Some(s) = self.sessions.get_mut(&member) {
            s.dir_sent = Some(dir_now);
            s.presence_sent = presence_now;
        }
    }

    fn delta_pages(&self, dir_sent: u64, presence_sent: u64) -> Vec<DirectoryPage> {
        let lobbies: BTreeSet<LobbyId> = self
            .dir_log
            .iter()
            .filter(|(g, _)| *g > dir_sent)
            .map(|(_, l)| *l)
            .collect();
        let members: BTreeSet<MemberId> = self
            .presence_log
            .iter()
            .filter(|(g, _)| *g > presence_sent)
            .map(|(_, m)| *m)
            .collect();
        let mut rows = Vec::new();
        let mut removed_rows = Vec::new();
        for m in members {
            match self.presence_row(m) {
                Some(row) => rows.push(row),
                None => removed_rows.push(m),
            }
        }
        let mut cards = Vec::new();
        let mut removed_cards = Vec::new();
        for l in lobbies {
            match self.card(l) {
                Some(card) => cards.push(card),
                None => removed_cards.push(l),
            }
        }
        self.paginate(false, rows, removed_rows, cards, removed_cards)
    }

    fn snapshot_pages(&self) -> Vec<DirectoryPage> {
        self.paginate(true, self.players(), Vec::new(), self.lobbies(), Vec::new())
    }

    fn paginate(
        &self,
        reset: bool,
        rows: Vec<PresenceRow>,
        removed_rows: Vec<MemberId>,
        cards: Vec<HubLobbyCard>,
        removed_cards: Vec<LobbyId>,
    ) -> Vec<DirectoryPage> {
        let fresh = || DirectoryPage {
            dir_gen: self.dir_generation,
            presence_gen: self.presence_generation,
            ..DirectoryPage::default()
        };
        let mut pages = Vec::new();
        let mut page = fresh();
        let mut bytes = 0usize;
        let full = |bytes: usize, page: &DirectoryPage| {
            bytes >= PAGE_BYTES
                || page.players.len() + page.removed_players.len() >= super::wire::MAX_PAGE_ROWS
                || page.lobbies.len() + page.removed_lobbies.len() >= super::wire::MAX_PAGE_LOBBIES
        };
        macro_rules! add {
            ($field:ident, $item:expr, $size:expr) => {{
                if full(bytes, &page) {
                    pages.push(std::mem::replace(&mut page, fresh()));
                    bytes = 0;
                }
                bytes += $size;
                page.$field.push($item);
            }};
        }
        for r in rows {
            let size = 20 + r.handle.len();
            add!(players, r, size);
        }
        for m in removed_rows {
            add!(removed_players, m, 8);
        }
        for c in cards {
            let size = 40 + c.owner_handle.len() + data_bytes(&c.data) + 3 * c.data.len();
            add!(lobbies, c, size);
        }
        for l in removed_cards {
            add!(removed_lobbies, l, 8);
        }
        pages.push(page);
        if let Some(first) = pages.first_mut() {
            first.reset = reset;
        }
        if let Some(last) = pages.last_mut() {
            last.done = true;
        }
        pages
    }
}

/// Bounds an offer: at most [`crate::nat::NAT_MAX_CANDIDATES`] candidates, a join code that
/// normalizes (stored normalized), a ticket of at most [`super::join::MAX_TICKET_BYTES`].
fn check_offer(offer: &mut ReachOffer) -> Result<(), HubError> {
    if offer.candidates.len() > crate::nat::NAT_MAX_CANDIDATES
        || offer.ticket.len() > super::join::MAX_TICKET_BYTES
    {
        return Err(HubError::ReachInvalid);
    }
    if let Some(code) = &offer.join_code {
        offer.join_code = Some(crate::join_code::normalize_join_code(code).ok_or(HubError::ReachInvalid)?);
    }
    Ok(())
}

fn data_bytes(data: &BTreeMap<String, String>) -> usize {
    data.iter().map(|(k, v)| k.len() + v.len()).sum()
}

fn admit(window: &mut Vec<u64>, now_ms: u64, per_sec: usize) -> Result<(), HubError> {
    window.retain(|t| now_ms.saturating_sub(*t) < 1000);
    if window.len() >= per_sec {
        return Err(HubError::ChatRateLimited);
    }
    window.push(now_ms);
    Ok(())
}

#[cfg(test)]
mod tests;
