// SPDX-License-Identifier: MPL-2.0
//! [`HubServer`]: the hub's pure, bounded state core. No socket and no clock: every operation takes
//! the caller's `now_ms`, so tests drive it on a test clock and two runs of the same calls give the
//! same state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::binding::{self, BindingClaims, ServerPresence};
use super::join::{JoinPlan, JoinRow, ReachKind, ReachOffer};
use super::wire::{
    BindingState, ChatScope, DirectoryPage, HubEvent, LobbyKind, RoomState, ServerCard, ServerNotice,
};
use super::{
    HubError, HubLobbyCard, HubPresence, HubReport, HubServerCard, HubSettings, InviteCard, InviteToken,
    PresenceRow, clean_handle, fold, public_data,
};
use crate::crypto::{PlayerKey, ServerIdentity, ServerKey};
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
/// Most player keys a hub operator may ban at once (NL-09).
const MAX_OPERATOR_BANS: usize = 4_096;

struct Session {
    handle: String,
    key: String,
    /// The player key the transport proved; `None` for a core-only session ([`HubServer::connect`]).
    identity: Option<PlayerKey>,
    /// The source address [`crate::lobby::hub::HubHost`] last noted. `None` skips the AskReach
    /// address window (a core-only test, or a peer whose socket has no address yet).
    source: Option<IpAddr>,
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
    /// Codec generation this peer speaks. `1` until `AnnounceWire`.
    wire: u32,
    /// Handshake hash of the sealed link, when the host noted it.
    link_hash: Option<[u8; 32]>,
    /// Set once `RegisterServer` succeeded on this session.
    server_key: Option<ServerKey>,
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
    /// Listen owner confirmed a session. The address itself is not stored (NL-09).
    confirmed: bool,
    /// Player keys the lobby owner refuses. At most [`binding::MAX_LOBBY_BANS`].
    bans: BTreeSet<PlayerKey>,
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
    /// Listen or dedicated. Listen is every lobby `create` makes for a player.
    kind: LobbyKind,
    /// The dedicated server, when `kind` is dedicated.
    server_key: Option<ServerKey>,
    /// The server's hub session. [`MemberId`]` (0)` when there is none.
    server_member: MemberId,
    /// Copied from the server at create. `0` on a listen room.
    capacity: u16,
    /// Copied from the server at create. `0` on a listen room.
    load: u16,
    /// Relay-only for every member. Copied from the server at create. `SetReachMode` stores
    /// it. `approve` passes it into the planner.
    relay_only: bool,
    /// Grace duration from the server. `0` means no deadline.
    grace_ms: u32,
    /// `None` when the owner is here. `Some(0)` is indefinite grace. `Some(n)` is an absolute deadline.
    grace: Option<u64>,
    /// The player who owns a delegated lobby. Stays set through grace.
    owner_key: Option<PlayerKey>,
    /// Handle shown as the owner, including while the member id is empty.
    owner_label: String,
    /// Created by `CreateDelegated` (a player owner, the server is not a member).
    delegated: bool,
    /// Created by an ownerless server session.
    ownerless: bool,
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
    /// The hub's static key, used to open binding and presence proofs.
    hub_identity: Option<ServerIdentity>,
    /// Live and expired bindings, keyed by (player, server).
    bindings: BTreeMap<([u8; 32], [u8; 32]), StoredBinding>,
    /// Server presence, keyed by the server's public key.
    servers: BTreeMap<[u8; 32], ServerSeat>,
    /// Revoked `(player, server, serial)` rows.
    revocations: Vec<StoredRevocation>,
    /// Directory that holds `revocations.ron`. `None` keeps the set in memory only.
    revocation_dir: Option<PathBuf>,
    /// The hub is draining: new server registration and new lobbies are refused.
    draining: bool,
    /// Server sessions a new link replaced. The host tells them `hub_server_in_use` and drops them.
    replaced: Vec<MemberId>,
    /// Dedicated lobbies, kept after close so a wire-1 directory delta does not remove a card that
    /// peer was never sent.
    dedicated_ids: BTreeSet<LobbyId>,
    /// AskReach windows: 10 seconds per source address and per player key (NL-09).
    asks: super::limits::AskWindows,
    /// Hub-operator player-key bans. Address bans stay in the operator's ban file.
    operator_bans: BTreeSet<PlayerKey>,
}

/// One owner binding the hub has verified.
struct StoredBinding {
    expiry_ms: u64,
    serial: u64,
    state: BindingState,
}

/// One dedicated server the hub has accepted.
struct ServerSeat {
    key: ServerKey,
    member: MemberId,
    online: bool,
    capacity: u16,
    load: u16,
    protocol: u32,
    game_version: String,
    relay_only: bool,
    grace_ms: u32,
    bans: Vec<PlayerKey>,
    owners: Vec<PlayerKey>,
    ownerless_lobbies: u32,
}

/// One revoked serial. `expiry_ms == 0` is never dropped to make room.
struct StoredRevocation {
    player: PlayerKey,
    server: [u8; 32],
    serial: u64,
    expiry_ms: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RevocationFile {
    format: u32,
    entries: Vec<RevocationRow>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct RevocationRow {
    player: String,
    server: String,
    serial: u64,
    expiry_ms: u64,
}

const REVOCATION_FORMAT: u32 = 1;

/// Most revoked serials kept.
const MAX_REVOCATIONS: usize = 4096;

static REVOCATION_TMP: AtomicU64 = AtomicU64::new(0);

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
            hub_identity: None,
            bindings: BTreeMap::new(),
            servers: BTreeMap::new(),
            revocations: Vec::new(),
            revocation_dir: None,
            draining: false,
            replaced: Vec::new(),
            dedicated_ids: BTreeSet::new(),
            asks: super::limits::AskWindows::default(),
            operator_bans: BTreeSet::new(),
        }
    }

    /// Records the source address the host saw for `member`.
    ///
    /// Core-only tests that never call this skip the AskReach address window. A `None` address
    /// skips it too. The player-key window still applies when the session has a key.
    pub fn note_source(&mut self, member: MemberId, addr: Option<IpAddr>) {
        if let Some(session) = self.sessions.get_mut(&member) {
            session.source = addr;
        }
    }

    /// Replaces the hub-operator player-key ban set.
    ///
    /// More than 4,096 keys is [`HubError::ReachInvalid`] and the previous set stays. Address
    /// bans are not stored here.
    ///
    /// # Errors
    ///
    /// [`HubError::ReachInvalid`] when `keys` is longer than the cap.
    pub fn set_operator_bans(&mut self, keys: &[PlayerKey]) -> Result<(), HubError> {
        if keys.len() > MAX_OPERATOR_BANS {
            return Err(HubError::ReachInvalid);
        }
        self.operator_bans = keys.iter().copied().collect();
        Ok(())
    }

    /// Replaces the lobby's player-key ban set. The owner only.
    ///
    /// More than [`MAX_LOBBY_BANS`](crate::lobby::hub::MAX_LOBBY_BANS) keys is
    /// [`HubError::ReachInvalid`] and the previous set stays.
    ///
    /// # Errors
    ///
    /// [`HubError::NoSession`], [`LobbyError::NotInLobby`], [`LobbyError::NotOwner`], or
    /// [`HubError::ReachInvalid`].
    pub fn set_bans(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        keys: &[PlayerKey],
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.set_bans_inner(member, lobby, keys, now_ms);
        self.count(result.as_ref().err());
        result
    }

    fn set_bans_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        keys: &[PlayerKey],
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        self.owned(member, lobby)?;
        if keys.len() > binding::MAX_LOBBY_BANS {
            return Err(HubError::ReachInvalid);
        }
        if let Some(room) = self.rooms.get_mut(&lobby) {
            room.bans = keys.iter().copied().collect();
        }
        Ok(())
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
                source: None,
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
                wire: 1,
                link_hash: None,
                server_key: None,
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
        if identity.is_some() {
            self.reattach_owner(id);
        }
        Ok(id)
    }

    /// Ends a session: it leaves its lobby (members see it go) and its presence row is removed.
    ///
    /// The grace clock is the session's last touch. [`HubServer::expire`] passes its own `now_ms`
    /// so a grace that starts on an idle timeout is measured from that call.
    pub fn disconnect(&mut self, member: MemberId) {
        let now_ms = self.sessions.get(&member).map(|s| s.last_ms).unwrap_or(0);
        self.disconnect_at(member, now_ms);
    }

    fn disconnect_at(&mut self, member: MemberId, now_ms: u64) {
        if let Some(key) = self.sessions.get(&member).and_then(|s| s.server_key) {
            let current = self.servers.get(&key.0).map(|s| s.member);
            if current == Some(member) {
                self.server_offline(key);
            }
        }
        if let Some(lobby) = self.sessions.get(&member).and_then(|s| s.lobby) {
            self.leave_inner(member, lobby, LeaveReason::Lost, now_ms);
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
            self.disconnect_at(*id, now_ms);
        }
        self.expire_grace(now_ms);
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
            (HubPresence::InGame, Some(l)) => self.rooms.get(&l).is_some_and(|r| r.confirmed),
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
        if self.draining {
            return Err(HubError::Draining);
        }
        if self.sessions[&member].server_key.is_some() {
            return self.create_ownerless(member, spec);
        }
        if self.sessions[&member].lobby.is_some() {
            return Err(LobbyError::AlreadyInLobby.into());
        }
        check_spec(spec)?;
        if spec.max_members > self.settings.max_members {
            return Err(LobbyError::MembersInvalid.into());
        }
        if super::data_has_address(&spec.data) {
            return Err(HubError::AddressInData);
        }
        if data_bytes(&spec.data) > self.settings.lobby_data_bytes {
            return Err(HubError::DataTooLarge);
        }
        if self.rooms.len() >= self.settings.lobby_cap {
            return Err(HubError::LobbyCap);
        }
        self.next_lobby += 1;
        let id = LobbyId(self.next_lobby);
        let owner_key = self.sessions[&member].identity;
        let owner_label = self.sessions[&member].handle.clone();
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
                confirmed: false,
                bans: BTreeSet::new(),
                muted: BTreeSet::new(),
                muted_keys: BTreeSet::new(),
                chat: VecDeque::new(),
                chat_seq: 0,
                chat_times: BTreeMap::new(),
                data_times: Vec::new(),
                offer: None,
                joins: BTreeMap::new(),
                kind: LobbyKind::Listen,
                server_key: None,
                server_member: MemberId(0),
                capacity: 0,
                load: 0,
                relay_only: false,
                grace_ms: 0,
                grace: None,
                owner_key,
                owner_label,
                delegated: false,
                ownerless: false,
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
        if room.kind == LobbyKind::Dedicated && self.sessions[&member].wire < 2 {
            return Err(HubError::NeedsWireV2);
        }
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
    /// member out deletes it. When the owner leaves a listen lobby whose session it confirmed, the
    /// flag is cleared (members get [`HubEvent::SessionConfirmed`] with `confirmed: false` and go
    /// back to `Lobby` presence): it was the old owner's game (ADR-8633, NL-09).
    pub fn leave(&mut self, member: MemberId, lobby: LobbyId, now_ms: u64) {
        if self.touch(member, now_ms).is_ok() {
            self.leave_inner(member, lobby, LeaveReason::Left, now_ms);
        }
    }

    fn leave_inner(&mut self, member: MemberId, lobby: LobbyId, reason: LeaveReason, now_ms: u64) {
        let Some(room) = self.rooms.get(&lobby) else {
            return;
        };
        if !room.has(member) {
            return;
        }
        // Delegated lobbies do not hand ownership off (spec 89 § 16.3). Listen lobbies still do.
        if room.delegated && room.owner == member {
            self.begin_grace(member, lobby, reason, now_ms);
            return;
        }
        if room.ownerless && room.owner == member {
            self.finish_dedicated(lobby);
            return;
        }
        let dedicated = room.kind == LobbyKind::Dedicated;
        let Some(room) = self.rooms.get_mut(&lobby) else {
            return;
        };
        room.members.retain(|m| m.id != member);
        // A muted player key stays muted (`muted_keys`); the member id is gone with the member.
        room.muted.remove(&member);
        room.chat_times.remove(&member);
        room.joins.remove(&member);
        let remaining: Vec<MemberId> = room.members.iter().map(|m| m.id).collect();
        let new_owner = (!dedicated && room.owner == member)
            .then(|| remaining.first().copied())
            .flatten();
        if let Some(owner) = new_owner {
            room.owner = owner;
            // The old owner's game socket is not the new owner's: its offer and every plan go.
            room.offer = None;
            room.joins.clear();
        }
        let withdrawn = new_owner.is_some() && room.confirmed;
        if withdrawn {
            room.confirmed = false;
            room.data.remove(keys::SERVER);
        }
        if let Some(s) = self.sessions.get_mut(&member) {
            s.lobby = None;
            s.presence = HubPresence::Menu;
        }
        self.push(member, HubEvent::Left { lobby, reason });
        self.note_presence(member);
        if remaining.is_empty() {
            if dedicated {
                // A delegated lobby in grace can have no player members and still stay open.
                self.note_lobby(lobby);
                return;
            }
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
                self.push(
                    *other,
                    HubEvent::SessionConfirmed {
                        lobby,
                        confirmed: false,
                    },
                );
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

    /// Sets (`Some`) or deletes (`None`) lobby data; owner only.
    ///
    /// A value that is an address spelling (`value_carries_address`) is
    /// [`HubError::AddressInData`]. `regia.server` is never
    /// stored: `Some` is the same refusal, and `None` withdraws the confirmed-session flag.
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
        // A non-owner cannot delete `regia.server` or learn that a value was refused as an address
        // in place of `not_owner`.
        self.owned(member, lobby)?;
        if key == keys::SERVER {
            if value.is_some() {
                return Err(HubError::AddressInData);
            }
            return self.confirm_inner(member, lobby, None);
        }
        if value.is_some_and(super::value_carries_address) {
            return Err(HubError::AddressInData);
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
    ///
    /// An address spelling, and any `Some` on `regia.server`, is [`HubError::AddressInData`].
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
        if value.is_some() && (key == keys::SERVER || value.is_some_and(super::value_carries_address)) {
            return Err(HubError::AddressInData);
        }
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

    /// The owner confirms (`Some`) or withdraws (`None`) a listen session.
    ///
    /// The address bytes are ignored. The hub stores a flag and tells members
    /// [`HubEvent::SessionConfirmed`] only. A dedicated lobby refuses every confirm with
    /// [`HubError::AddressInData`]. Members move to [`HubPresence::InGame`] while the flag is set
    /// (back to `Lobby` on a withdrawal). Confirming the flag the lobby already has is a no-op.
    pub fn confirm_session(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        server: Option<LobbyServer>,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self
            .touch(member, now_ms)
            .and_then(|()| self.confirm_inner(member, lobby, server));
        self.count(result.as_ref().err());
        result
    }

    fn confirm_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        server: Option<LobbyServer>,
    ) -> Result<(), HubError> {
        if self
            .rooms
            .get(&lobby)
            .is_some_and(|r| r.kind == LobbyKind::Dedicated)
        {
            return Err(HubError::AddressInData);
        }
        let confirmed = server.is_some();
        let room = self.owned(member, lobby)?;
        if room.confirmed == confirmed {
            return Ok(());
        }
        room.confirmed = confirmed;
        // A stored `regia.server` value would be an address. The flag is the whole record.
        room.data.remove(keys::SERVER);
        let presence = if confirmed {
            HubPresence::InGame
        } else {
            HubPresence::Lobby
        };
        for m in self.member_ids(lobby) {
            self.push(m, HubEvent::SessionConfirmed { lobby, confirmed });
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
        let room = self.rooms.get_mut(&lobby).ok_or(LobbyError::NotInLobby)?;
        if room.kind == LobbyKind::Dedicated {
            if member != room.server_member {
                return Err(HubError::NotSession);
            }
            room.offer = Some(offer);
            return Ok(());
        }
        if !room.has(member) {
            return Err(LobbyError::NotInLobby.into());
        }
        if room.owner != member {
            return Err(LobbyError::NotOwner.into());
        }
        room.offer = Some(offer);
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
        let (identity, source) = {
            let Some(session) = self.sessions.get(&member) else {
                return Err(HubError::NoSession);
            };
            (session.identity, session.source)
        };
        // A missing id is `lobby_not_found` before any ban, so a ban is not an existence oracle.
        let Some(room) = self.rooms.get(&lobby) else {
            return Err(LobbyError::NotFound.into());
        };
        let lobby_banned = identity.is_some_and(|key| room.bans.contains(&key));
        let dedicated = room.kind == LobbyKind::Dedicated;
        let server_key = room.server_key;
        let capacity = room.capacity;
        let load = room.load;
        let is_member = room.has(member);
        let is_listen_owner = !dedicated && room.owner == member;
        let visibility = room.visibility;
        let joinable = room.joinable;
        let lobby_full = room.members.len() >= room.max_members as usize;
        let server_member = room.server_member;
        let has_offer = room.offer.is_some();
        let replacing = room
            .joins
            .get(&member)
            .is_some_and(|j| matches!(j.state, JoinState::Asked | JoinState::Planned));
        let ask_owner = if dedicated { server_member } else { room.owner };
        let operator_banned = identity.is_some_and(|key| self.operator_bans.contains(&key));
        if lobby_banned || operator_banned {
            return Err(HubError::Banned);
        }
        let server_banned = identity.is_some_and(|key| {
            server_key.is_some_and(|sk| {
                self.servers
                    .get(&sk.0)
                    .is_some_and(|seat| seat.bans.contains(&key))
            })
        });
        if server_banned {
            return Err(HubError::Banned);
        }
        // A dedicated server at capacity refuses everyone, including a member, and before the
        // visibility check so a hidden full server is not an existence oracle the other way.
        if dedicated && capacity > 0 && load >= capacity {
            return Err(HubError::ServerFull);
        }
        if !is_member {
            if visibility != LobbyVisibility::Public {
                return Err(LobbyError::NotFound.into());
            }
            if !joinable {
                return Err(HubError::LobbyClosed);
            }
            if lobby_full {
                return Err(HubError::LobbyFull);
            }
            return Err(LobbyError::NotInLobby.into());
        }
        if is_listen_owner {
            return Err(LobbyError::NotOwner.into());
        }
        if dedicated && (server_member == MemberId(0) || !self.sessions.contains_key(&server_member)) {
            return Err(HubError::ServerOffline);
        }
        if !has_offer {
            return Err(HubError::NoReach);
        }
        // Invalid offers are not admitted and are not copied into an event.
        check_offer(&mut offer)?;
        // A member's ask carries its candidates and NAT only: the relay and ticket are the owner's.
        offer.relay = None;
        offer.join_code = None;
        offer.ticket.clear();
        let credit = usize::from(replacing);
        let (mine, hub_open) = if let Some(key) = identity {
            (self.inflight_for_key(key), self.inflight_hub())
        } else {
            (self.inflight_for_member(member), self.inflight_hub())
        };
        let mine = mine.saturating_sub(credit);
        let hub_open = hub_open.saturating_sub(credit);
        let per_key = usize::try_from(self.settings.ask_inflight_per_identity).unwrap_or(usize::MAX);
        let hub_cap = usize::try_from(self.settings.ask_inflight_hub).unwrap_or(usize::MAX);
        if mine >= per_key || hub_open >= hub_cap {
            return Err(HubError::AskInflight);
        }
        self.asks.admit(
            source,
            identity,
            now_ms,
            self.settings.ask_per_address_per_10s,
            self.settings.ask_per_identity_per_10s,
        )?;
        let (handle, shown) = {
            let session = &self.sessions[&member];
            (session.handle.clone(), session.identity.unwrap_or_default())
        };
        // A relay-only lobby's approver is not shown the asker's candidates (ADR-10399).
        let announced = match self.rooms.get(&lobby) {
            Some(room) => announced_reach_offer(room, &offer),
            None => offer.clone(),
        };
        if let Some(room) = self.rooms.get_mut(&lobby) {
            room.joins.insert(
                member,
                Join {
                    offer,
                    state: JoinState::Asked,
                    planned: None,
                    chosen: None,
                    punch_attempts: 0,
                    member_plan_lost: None,
                    owner_plan_lost: None,
                },
            );
        }
        self.push(
            ask_owner,
            HubEvent::ReachAsk {
                lobby,
                member,
                handle,
                identity: shown,
                offer: announced,
            },
        );
        Ok(())
    }

    /// Open asks (`Asked` or `Planned`) whose session carries `key`.
    fn inflight_for_key(&self, key: PlayerKey) -> usize {
        self.rooms
            .values()
            .map(|room| {
                room.joins
                    .iter()
                    .filter(|(id, join)| {
                        matches!(join.state, JoinState::Asked | JoinState::Planned)
                            && self.sessions.get(id).and_then(|s| s.identity) == Some(key)
                    })
                    .count()
            })
            .sum()
    }

    /// Open asks held by one member id. A keyless session is capped on its own joins.
    fn inflight_for_member(&self, member: MemberId) -> usize {
        self.rooms
            .values()
            .map(|room| {
                usize::from(
                    room.joins
                        .get(&member)
                        .is_some_and(|join| matches!(join.state, JoinState::Asked | JoinState::Planned)),
                )
            })
            .sum()
    }

    /// Open asks on the whole hub.
    fn inflight_hub(&self) -> usize {
        self.rooms
            .values()
            .map(|room| {
                room.joins
                    .values()
                    .filter(|join| matches!(join.state, JoinState::Asked | JoinState::Planned))
                    .count()
            })
            .sum()
    }

    /// The owner approves (or denies) `target`'s ask. On approval the broker chooses the path
    /// (mapped, punch, relay; [`super::join::broker_reach`]) and each side is sent a
    /// [`HubEvent::Plan`] carrying the other's candidates; the member's plan also carries the
    /// relay, its join code and the owner's ticket. Returns the member's plan.
    ///
    /// An operator ban, a lobby ban, a dedicated server-seat ban, or a dedicated server with
    /// `load >= capacity` forces a denial even when `approve` is true. The member is sent
    /// [`HubEvent::ReachDenied`], the result is `Ok(None)`, and no plan is built, so the owner's
    /// candidates are not copied. A keyless session matches none of the ban lists.
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
        let target_key = self.identity(target);
        let target_identity = target_key.unwrap_or_default();
        // A keyless session has no player key, so it matches none of the ban lists (ADR-10392).
        let operator_banned = target_key.is_some_and(|key| self.operator_bans.contains(&key));
        let (dedicated, server_key, capacity, load, server_member, lobby_relay_only, lobby_banned) = {
            let room = self.rooms.get(&lobby).ok_or(LobbyError::NotInLobby)?;
            (
                room.kind == LobbyKind::Dedicated,
                room.server_key,
                room.capacity,
                room.load,
                room.server_member,
                room.relay_only,
                target_key.is_some_and(|key| room.bans.contains(&key)),
            )
        };
        if dedicated && member != server_member {
            return Err(HubError::NotSession);
        }
        // Rechecked here, not only at ask time: a ban or a full server that lands while the ask
        // is open must not copy the owner's candidates into a plan.
        let server_banned = server_key
            .and_then(|k| self.servers.get(&k.0))
            .is_some_and(|s| s.bans.contains(&target_identity));
        let approve = approve
            && !operator_banned
            && !lobby_banned
            && !(dedicated && (server_banned || (capacity > 0 && load >= capacity)));
        let room = if dedicated {
            self.rooms.get_mut(&lobby).ok_or(LobbyError::NotInLobby)?
        } else {
            self.owned(member, lobby)?
        };
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
        let member_offer = join.offer.clone();
        let (for_member, for_owner) = super::join::plans_for(
            &owner_offer,
            &member_offer,
            lobby_relay_only,
            lobby,
            member,
            owner_identity,
            target,
            target_identity,
        )
        .map_err(|_| HubError::NoReach)?;
        join.state = JoinState::Planned;
        join.planned = Some(for_member.path);
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
                        .and_then(|m| m.data.get("platform"))
                        .filter(|v| !super::value_carries_address(v))
                        .cloned(),
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
    /// open asks it approves, and a directory snapshot at the next flush. A listen lobby's owner
    /// approves its asks. A dedicated lobby's server session approves its asks, even when that
    /// session is not a member. The owning player of a delegated lobby does not receive them
    /// (spec 89 § 16.3, ADR-10395). For a session whose event stream had a gap (an outbox
    /// overflow, or a link that resumed after dormancy).
    pub fn resync(&mut self, member: MemberId) {
        for (event, mark) in self.resync_events(member) {
            self.push_marked(member, event, mark);
        }
    }

    /// The resync's events. Besides the state above: the kept chat lines from the first one
    /// dropped at the outbox cap on (lobby and global, oldest first), and a join plan that was
    /// dropped (KI-NT-045, ADR-10069). Pending asks and the approver's dropped plan go to the
    /// session that approves (the listen owner, or the dedicated server), not to a delegated
    /// lobby's player owner. The gaps and lost plans are cleared: the caller delivers these, and
    /// a drop on the way marks the gap again.
    fn resync_events(&mut self, member: MemberId) -> Vec<(HubEvent, ChatMark)> {
        let Some(s) = self.sessions.get_mut(&member) else {
            return Vec::new();
        };
        s.dir_sent = None;
        let seated = s.lobby;
        let lobby_gap = s.lobby_chat_gap.take();
        let global_gap = s.global_chat_gap.take();
        let mut out = Vec::new();
        if let Some(lobby) = seated {
            if let Some(room) = self.room_state(lobby) {
                out.push((HubEvent::Room(room), ChatMark::None));
            }
            out.extend(self.reach_resync(member, lobby));
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
        // The dedicated server is not seated in the lobby. Pending asks and a plan dropped at
        // its outbox cap still belong to that session, not to the owning player.
        let server_lobbies: Vec<LobbyId> = self
            .rooms
            .iter()
            .filter(|(id, room)| {
                room.kind == LobbyKind::Dedicated && room.server_member == member && seated != Some(**id)
            })
            .map(|(id, _)| *id)
            .collect();
        for lobby in server_lobbies {
            out.extend(self.reach_resync(member, lobby));
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

    /// Who is sent a pending [`HubEvent::ReachAsk`] and the approver half of a dropped plan.
    fn reach_approver(room: &Room, member: MemberId) -> bool {
        if room.kind == LobbyKind::Dedicated {
            room.server_member == member
        } else {
            room.owner == member
        }
    }

    /// Open asks and dropped join plans `member` should see again for `lobby`.
    fn reach_resync(&mut self, member: MemberId, lobby: LobbyId) -> Vec<(HubEvent, ChatMark)> {
        let mut out = Vec::new();
        let approver = self
            .rooms
            .get(&lobby)
            .is_some_and(|room| Self::reach_approver(room, member));
        if let Some(room) = self.rooms.get_mut(&lobby) {
            for (asker, join) in room.joins.iter_mut() {
                if *asker == member
                    && let Some(plan) = join.member_plan_lost.take()
                {
                    out.push((HubEvent::Plan(plan), ChatMark::None));
                }
                if approver && let Some(plan) = join.owner_plan_lost.take() {
                    out.push((HubEvent::Plan(plan), ChatMark::None));
                }
            }
        }
        if approver && let Some(room) = self.rooms.get(&lobby) {
            for (asker, join) in room
                .joins
                .iter()
                .filter(|(_, join)| join.state == JoinState::Asked)
            {
                if let Some(asker_session) = self.sessions.get(asker) {
                    out.push((
                        HubEvent::ReachAsk {
                            lobby,
                            member: *asker,
                            handle: asker_session.handle.clone(),
                            identity: asker_session.identity.unwrap_or_default(),
                            offer: announced_reach_offer(room, &join.offer),
                        },
                        ChatMark::None,
                    ));
                }
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

    /// A lobby with its members, with no session address; `None` for an unknown lobby.
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
                    data: super::public_data(&m.data),
                })
                .collect(),
            member_count: room.members.len() as u32,
            data: super::public_data(&room.data),
            server: None,
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
            total += r.bans.len() * 32;
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
        total += self.bindings.len() * 80;
        total += self
            .servers
            .values()
            .map(|s| 96 + s.game_version.len() + s.bans.len() * 32)
            .sum::<usize>();
        total += self.revocations.len() * 80;
        total += self.dedicated_ids.len() * 8;
        total += self.operator_bans.len() * 32;
        total += self.asks.bytes_estimate();
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
            servers: self.server_cards().into_iter().take(REPORT_ROWS).collect(),
            servers_online: self.servers.len(),
            relay_allocations: 0,
            draining: self.draining,
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
        // Spec 89 § 16.3: ownership is never the server session. A delegated server
        // seat that calls an owner method is `not_owner`, including when that link
        // is not a lobby member (a replaced server must not pass this check).
        if room.delegated && room.server_member == member && room.owner != member {
            return Err(LobbyError::NotOwner.into());
        }
        if !room.has(member) {
            return Err(LobbyError::NotInLobby.into());
        }
        if room.owner != member {
            return Err(LobbyError::NotOwner.into());
        }
        Ok(room)
    }

    fn enter(&mut self, member: MemberId, lobby: LobbyId) {
        let in_game = self.rooms.get(&lobby).is_some_and(|r| r.confirmed);
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
            data: super::public_data(&room.data),
            members: room
                .members
                .iter()
                .map(|m| {
                    let handle = self
                        .sessions
                        .get(&m.id)
                        .map(|s| s.handle.clone())
                        .unwrap_or_default();
                    (m.id, handle, super::public_data(&m.data))
                })
                .collect(),
            server: None,
            kind: room.kind,
            relay_only: room.relay_only,
            capacity: room.capacity,
            load: room.load,
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
            owner_handle: self.owner_handle(room),
            visibility: room.visibility,
            max_members: room.max_members,
            member_count: room.members.len() as u32,
            joinable: room.joinable,
            data: public_data(&room.data),
            kind: room.kind,
            relay_only: room.relay_only,
            capacity: room.capacity,
            load: room.load,
        })
    }

    fn owner_handle(&self, room: &Room) -> String {
        if room.owner != MemberId(0)
            && let Some(s) = self.sessions.get(&room.owner)
        {
            return s.handle.clone();
        }
        room.owner_label.clone()
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
        if self
            .rooms
            .get(&lobby)
            .is_some_and(|r| r.kind == LobbyKind::Dedicated)
        {
            self.dedicated_ids.insert(lobby);
        }
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
        if self.dedicated_ids.len() > self.settings.changelog {
            let live: BTreeSet<LobbyId> = self.dir_log.iter().map(|(_, id)| *id).collect();
            self.dedicated_ids
                .retain(|id| self.rooms.contains_key(id) || live.contains(id));
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
        let (dir_sent, presence_sent, wire) = (s.dir_sent, s.presence_sent, s.wire);
        if dir_sent == Some(dir_now) && presence_sent == presence_now {
            return;
        }
        let covered = dir_sent.is_some_and(|d| d >= self.dir_floor) && presence_sent >= self.presence_floor;
        let mut pages = if covered {
            self.delta_pages(dir_sent.unwrap_or(0), presence_sent)
        } else {
            self.snapshot_pages()
        };
        if wire < 2 {
            for page in &mut pages {
                page.lobbies.retain(|c| c.kind == LobbyKind::Listen);
                page.removed_lobbies.retain(|id| !self.dedicated_ids.contains(id));
                page.servers.clear();
            }
        } else if let Some(last) = pages.last_mut() {
            last.servers = self
                .server_cards()
                .into_iter()
                .map(|c| ServerCard {
                    server_key: c.server_key,
                    capacity: c.capacity,
                    load: c.load,
                    protocol: c.protocol,
                    game_version: c.game_version,
                    relay_only: c.relay_only,
                    owner_count: c.owner_count,
                })
                .collect();
        }
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

impl HubServer {
    /// Stores the hub static key that opens binding and presence proofs.
    pub fn set_hub_identity(&mut self, identity: ServerIdentity) {
        self.hub_identity = Some(identity);
    }

    /// Records the sealed link's handshake hash for `member`. `RegisterServer` refuses a session
    /// that has none.
    pub fn note_link_hash(&mut self, member: MemberId, hash: [u8; 32]) {
        if let Some(s) = self.sessions.get_mut(&member) {
            s.link_hash = Some(hash);
        }
    }

    /// Sets the codec generation `member` speaks. Moving to generation 2 drops the directory
    /// cursor so the next flush is a snapshot that may carry dedicated cards.
    pub fn set_wire(&mut self, member: MemberId, wire: u32) {
        if let Some(s) = self.sessions.get_mut(&member)
            && s.wire != wire
        {
            s.wire = wire;
            s.dir_sent = None;
        }
    }

    /// The codec generation `member` speaks. `1` when the session is unknown.
    pub fn wire(&self, member: MemberId) -> u32 {
        self.sessions.get(&member).map(|s| s.wire).unwrap_or(1)
    }

    /// Refuses new server registration and new lobbies while set.
    pub fn set_draining(&mut self, draining: bool) {
        self.draining = draining;
    }

    /// Server sessions a newer link replaced since the last call. The host drops those links.
    pub fn take_replaced_servers(&mut self) -> Vec<MemberId> {
        std::mem::take(&mut self.replaced)
    }

    /// Loads `revocations.ron` from `dir` and keeps writing the set there.
    ///
    /// A missing file is an empty set. A file that does not parse is an error and leaves the
    /// in-memory set as it was.
    ///
    /// # Errors
    ///
    /// The path and the reason: unreadable, or not this file's layout.
    pub fn set_revocation_dir(&mut self, dir: PathBuf) -> Result<(), String> {
        self.revocation_dir = Some(dir);
        self.load_revocations()
    }

    /// Reads `revocations.ron` from the directory set by [`HubServer::set_revocation_dir`].
    ///
    /// # Errors
    ///
    /// The file is present and does not parse, or the directory was not set and a read was asked
    /// for. A missing file is success.
    pub fn load_revocations(&mut self) -> Result<(), String> {
        let Some(dir) = self.revocation_dir.clone() else {
            return Ok(());
        };
        let path = dir.join("revocations.ron");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let file: RevocationFile = ron::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if file.format != REVOCATION_FORMAT {
            return Err(format!("{}: revocation format {}", path.display(), file.format));
        }
        let mut next = Vec::with_capacity(file.entries.len().min(MAX_REVOCATIONS));
        for row in file.entries {
            let Some(player) = PlayerKey::parse(&row.player) else {
                return Err(format!("{}: bad player key", path.display()));
            };
            let Some(server) = ServerKey::parse(&row.server) else {
                return Err(format!("{}: bad server key", path.display()));
            };
            if next.len() >= MAX_REVOCATIONS {
                break;
            }
            next.push(StoredRevocation {
                player,
                server: server.0,
                serial: row.serial,
                expiry_ms: row.expiry_ms,
            });
        }
        self.revocations = next;
        Ok(())
    }

    /// Registers `member`'s link as the dedicated server in `presence`.
    ///
    /// # Errors
    ///
    /// The codes in spec 89 § 16.3: `hub_draining`, `hub_binding_invalid`, `hub_binding_expired`,
    /// `hub_server_invalid`. One bad owner proof fails the whole call and changes nothing.
    pub fn register_server(
        &mut self,
        member: MemberId,
        presence: &ServerPresence,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.register_inner(member, presence, now_ms);
        self.count(result.as_ref().err());
        result
    }

    /// Inserts `(member's player key, server_key, serial)` into the revocation set before returning.
    ///
    /// # Errors
    ///
    /// [`HubError::NotAPlayer`] when the session has no player key, [`HubError::RevokeFull`] when
    /// the set is at 4096 and no past-expiry row can be dropped.
    pub fn revoke_binding(
        &mut self,
        member: MemberId,
        server_key: ServerKey,
        serial: u64,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.revoke_inner(member, server_key, serial, now_ms);
        self.count(result.as_ref().err());
        result
    }

    /// Creates a delegated lobby on `server_key` owned by `member`.
    ///
    /// # Errors
    ///
    /// The create rows of spec 89 § 16.3.
    pub fn create_delegated(
        &mut self,
        member: MemberId,
        spec: &LobbySpec,
        server_key: ServerKey,
        now_ms: u64,
    ) -> Result<LobbyId, HubError> {
        let result = self.create_delegated_inner(member, spec, server_key, now_ms);
        self.count(result.as_ref().err());
        result
    }

    /// Sets whether `lobby` offers only the relay.
    ///
    /// The listen owner, a delegated lobby's owning player or its bound server, or an
    /// ownerless lobby's server session may set or clear the flag. Members are sent the
    /// room, and a listed lobby's directory card updates.
    ///
    /// # Errors
    ///
    /// [`HubError::NoSession`] when `member` has no session. A missing lobby or a listen
    /// caller who is not in it is [`crate::lobby::LobbyError::NotInLobby`]. A listen or
    /// ownerless caller who may not change it is [`crate::lobby::LobbyError::NotOwner`].
    /// A delegated caller who is neither the owning player nor the bound server is
    /// [`HubError::BindingNotOwner`].
    pub fn set_reach_mode(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        relay_only: bool,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.set_reach_mode_inner(member, lobby, relay_only, now_ms);
        self.count(result.as_ref().err());
        result
    }

    /// Closes a delegated or ownerless lobby. A listen lobby stays on [`HubServer::leave`].
    ///
    /// # Errors
    ///
    /// [`HubError::NotReady`] for a listen lobby, [`HubError::BindingNotOwner`] when a delegated
    /// lobby's caller is not the owning player, [`HubError::Lobby`] when the caller is not in a
    /// position to close an ownerless lobby.
    pub fn close_lobby(&mut self, member: MemberId, lobby: LobbyId, now_ms: u64) -> Result<(), HubError> {
        let result = self.close_inner(member, lobby, now_ms);
        self.count(result.as_ref().err());
        result
    }

    /// Moves a delegated lobby to another live owner of its server.
    ///
    /// Transfer to the server key, or to a key that is not a live owner, is
    /// [`HubError::BindingNotOwner`] and changes nothing.
    ///
    /// # Errors
    ///
    /// [`HubError::BindingNotOwner`], or a lobby error when `member` is not the owner.
    pub fn transfer_delegated(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        to: PlayerKey,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.transfer_inner(member, lobby, to, now_ms);
        self.count(result.as_ref().err());
        result
    }

    /// Removes `target` from `lobby`. The owner of a delegated lobby keeps this; the server does
    /// not become the owner.
    ///
    /// # Errors
    ///
    /// [`LobbyError::NotOwner`] when `member` is not the owner, [`HubError::CannotMuteOwner`] when
    /// `target` is the owner (the same refusal mute uses for the owner).
    pub fn kick(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        target: MemberId,
        now_ms: u64,
    ) -> Result<(), HubError> {
        let result = self.touch(member, now_ms).and_then(|()| {
            let room = self.rooms.get(&lobby).ok_or(LobbyError::NotInLobby)?;
            let allowed = if room.delegated {
                self.sessions.get(&member).and_then(|s| s.identity) == room.owner_key
            } else {
                room.owner == member && room.has(member)
            };
            if !allowed {
                return Err(LobbyError::NotOwner.into());
            }
            if target == room.owner || room.owner_key == self.identity(target) {
                return Err(HubError::CannotMuteOwner);
            }
            if !room.has(target) {
                return Err(LobbyError::NotInLobby.into());
            }
            Ok(())
        });
        self.count(result.as_ref().err());
        result?;
        self.leave_inner(target, lobby, LeaveReason::Kicked, now_ms);
        Ok(())
    }

    fn register_inner(
        &mut self,
        member: MemberId,
        presence: &ServerPresence,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        if self.draining {
            return Err(HubError::Draining);
        }
        if !(1..=4096).contains(&presence.capacity) || presence.load > presence.capacity {
            return Err(HubError::ServerInvalid);
        }
        if presence.proofs.len() > binding::MAX_BINDING_OWNERS {
            return Err(HubError::BindingInvalid);
        }
        if !binding::grace_in_range(presence.grace_ms)
            || presence.game_version.len() > binding::MAX_HUB_GAME_VERSION_BYTES
            || presence.bans.len() > binding::MAX_SERVER_BANS
        {
            return Err(HubError::ServerInvalid);
        }
        let hub = self.hub_identity.clone().ok_or(HubError::BindingInvalid)?;
        let hash = self
            .sessions
            .get(&member)
            .and_then(|s| s.link_hash)
            .ok_or(HubError::BindingInvalid)?;
        let server_key = binding::verify_server_presence(&hub, &hash, &presence.server_proof)?;
        let mut owners: Vec<(PlayerKey, BindingClaims)> = Vec::with_capacity(presence.proofs.len());
        for proof in &presence.proofs {
            let (player, claims) = binding::verify_binding(&hub, proof, now_ms)?;
            if claims.server_key != server_key {
                return Err(HubError::BindingInvalid);
            }
            if owners.iter().any(|(p, _)| *p == player) {
                return Err(HubError::BindingInvalid);
            }
            owners.push((player, claims));
        }
        if let Some(seat) = self.servers.get(&server_key.0) {
            if seat.ownerless_lobbies > 0 && !owners.is_empty() {
                return Err(HubError::BindingInvalid);
            }
            let same_owners = seat.owners.len() == owners.len()
                && seat.owners.iter().all(|k| owners.iter().any(|(p, _)| p == k));
            let named = self.rooms.values().any(|r| r.server_key == Some(server_key));
            if named && !same_owners && !(seat.owners.is_empty() && owners.is_empty()) {
                return Err(HubError::BindingInvalid);
            }
        }
        let old_member = self.servers.get(&server_key.0).map(|s| s.member);
        if let Some(old) = old_member.filter(|old| *old != member && *old != MemberId(0)) {
            // Move the server seat. An ownerless lobby's displayed owner is that seat, so it
            // moves too. A delegated lobby keeps its player owner (spec 89 § 16.3, ADR-10395):
            // the new link is not written in as owner, and the old link keeps its lobby so the
            // host's disconnect starts grace. A listen lobby is not a server seat; leaving
            // `s.lobby` set makes that same disconnect run listen hand-off.
            let old_lobby = self.sessions.get(&old).and_then(|s| s.lobby);
            let mut moved_lobby = false;
            for (id, room) in self.rooms.iter_mut() {
                if room.server_member != old {
                    continue;
                }
                room.server_member = member;
                if room.delegated {
                    continue;
                }
                if room.owner == old {
                    room.owner = member;
                    for person in &mut room.members {
                        if person.id == old {
                            person.id = member;
                        }
                    }
                    if old_lobby == Some(*id) {
                        moved_lobby = true;
                    }
                }
            }
            if let Some(s) = self.sessions.get_mut(&old) {
                s.server_key = None;
                if moved_lobby {
                    s.lobby = None;
                }
            }
            self.replaced.push(old);
        }
        let owner_keys: Vec<PlayerKey> = owners.iter().map(|(p, _)| *p).collect();
        let ownerless_lobbies = self
            .servers
            .get(&server_key.0)
            .map(|s| s.ownerless_lobbies)
            .unwrap_or(0);
        self.servers.insert(
            server_key.0,
            ServerSeat {
                key: server_key,
                member,
                online: true,
                capacity: presence.capacity,
                load: presence.load,
                protocol: presence.protocol,
                game_version: presence.game_version.clone(),
                relay_only: presence.relay_only,
                grace_ms: presence.grace_ms,
                bans: presence.bans.clone(),
                owners: owner_keys.clone(),
                ownerless_lobbies,
            },
        );
        if let Some(s) = self.sessions.get_mut(&member) {
            s.server_key = Some(server_key);
        }
        for room in self.rooms.values_mut() {
            if room.server_key == Some(server_key) {
                room.capacity = presence.capacity;
                room.load = presence.load;
                room.relay_only = presence.relay_only;
                room.grace_ms = presence.grace_ms;
            }
        }
        for (player, claims) in &owners {
            self.bindings.insert(
                (player.0, server_key.0),
                StoredBinding {
                    expiry_ms: claims.expiry_ms,
                    serial: claims.serial,
                    state: BindingState::Live,
                },
            );
        }
        self.emit_server(server_key, true);
        for (player, claims) in &owners {
            self.emit_binding(*player, server_key, claims.serial, BindingState::Live);
        }
        if self.sessions.get(&member).and_then(|s| s.lobby).is_none() {
            let inherited = self
                .rooms
                .iter()
                .find_map(|(id, room)| (room.ownerless && room.owner == member).then_some(*id));
            if let Some(lobby) = inherited {
                self.enter(member, lobby);
            }
        }
        Ok(())
    }

    fn revoke_inner(
        &mut self,
        member: MemberId,
        server_key: ServerKey,
        serial: u64,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        let player = self
            .sessions
            .get(&member)
            .and_then(|s| s.identity)
            .ok_or(HubError::NotAPlayer)?;
        if self
            .revocations
            .iter()
            .any(|r| r.player == player && r.server == server_key.0 && r.serial == serial)
        {
            return Ok(());
        }
        if self.revocations.len() >= MAX_REVOCATIONS {
            self.revocations
                .retain(|r| r.expiry_ms == 0 || r.expiry_ms > now_ms);
            if self.revocations.len() >= MAX_REVOCATIONS {
                return Err(HubError::RevokeFull);
            }
        }
        let expiry_ms = self
            .bindings
            .get(&(player.0, server_key.0))
            .filter(|b| b.serial == serial)
            .map(|b| b.expiry_ms)
            .unwrap_or(0);
        self.revocations.push(StoredRevocation {
            player,
            server: server_key.0,
            serial,
            expiry_ms,
        });
        if let Some(binding) = self.bindings.get_mut(&(player.0, server_key.0))
            && binding.serial == serial
        {
            binding.state = BindingState::Revoked;
        }
        if let Err(err) = self.persist_revocations() {
            tracing::warn!(error = %err, "revocations.ron was not written");
        }
        self.emit_binding(player, server_key, serial, BindingState::Revoked);
        Ok(())
    }

    fn create_delegated_inner(
        &mut self,
        member: MemberId,
        spec: &LobbySpec,
        server_key: ServerKey,
        now_ms: u64,
    ) -> Result<LobbyId, HubError> {
        self.touch(member, now_ms)?;
        let player = self
            .sessions
            .get(&member)
            .and_then(|s| s.identity)
            .ok_or(HubError::NotAPlayer)?;
        if self.draining {
            return Err(HubError::Draining);
        }
        let seat = self.servers.get(&server_key.0).ok_or(HubError::ServerOffline)?;
        if !seat.online || seat.member == MemberId(0) {
            return Err(HubError::ServerOffline);
        }
        if seat.owners.is_empty() {
            return Err(HubError::BindingUnbound);
        }
        let server_member = seat.member;
        let capacity = seat.capacity;
        let load = seat.load;
        let relay_only = seat.relay_only;
        let grace_ms = seat.grace_ms;
        let is_owner = seat.owners.contains(&player);
        let binding = self
            .bindings
            .get(&(player.0, server_key.0))
            .map(|b| (b.expiry_ms, b.serial, b.state));
        if !is_owner {
            return Err(HubError::BindingNotOwner);
        }
        let Some((expiry_ms, serial, state)) = binding else {
            return Err(HubError::BindingUnbound);
        };
        if state == BindingState::Revoked || self.revoked(player, server_key, serial) {
            return Err(HubError::BindingRevoked);
        }
        if expiry_ms != 0 && expiry_ms <= now_ms {
            if let Some(b) = self.bindings.get_mut(&(player.0, server_key.0)) {
                b.state = BindingState::Expired;
            }
            self.emit_binding(player, server_key, serial, BindingState::Expired);
            return Err(HubError::BindingExpired);
        }
        if self.sessions[&member].lobby.is_some() {
            return Err(LobbyError::AlreadyInLobby.into());
        }
        if super::data_has_address(&spec.data) {
            return Err(HubError::AddressInData);
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
        let owner_label = self.sessions[&member].handle.clone();
        self.insert_dedicated(
            id,
            member,
            spec,
            Some(player),
            owner_label,
            DedicatedSeat {
                server_key: Some(server_key),
                server_member,
                capacity,
                load,
                relay_only,
                grace_ms,
                delegated: true,
                ownerless: false,
            },
        );
        self.push(
            server_member,
            HubEvent::LobbyAssigned {
                lobby: id,
                owner: player,
            },
        );
        self.enter(member, id);
        Ok(id)
    }

    fn create_ownerless(&mut self, member: MemberId, spec: &LobbySpec) -> Result<LobbyId, HubError> {
        let server_key = self.sessions[&member].server_key.ok_or(HubError::NotAPlayer)?;
        let seat = self.servers.get(&server_key.0).ok_or(HubError::ServerOffline)?;
        if !seat.online {
            return Err(HubError::ServerOffline);
        }
        if !seat.owners.is_empty() {
            return Err(HubError::NotAPlayer);
        }
        if spec.visibility != LobbyVisibility::Public {
            return Err(HubError::ServerInvalid);
        }
        if self.sessions[&member].lobby.is_some() {
            return Err(LobbyError::AlreadyInLobby.into());
        }
        if super::data_has_address(&spec.data) {
            return Err(HubError::AddressInData);
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
        let capacity = seat.capacity;
        let load = seat.load;
        let relay_only = seat.relay_only;
        let grace_ms = seat.grace_ms;
        self.next_lobby += 1;
        let id = LobbyId(self.next_lobby);
        let owner_label = self.sessions[&member].handle.clone();
        self.insert_dedicated(
            id,
            member,
            spec,
            None,
            owner_label,
            DedicatedSeat {
                server_key: Some(server_key),
                server_member: member,
                capacity,
                load,
                relay_only,
                grace_ms,
                delegated: false,
                ownerless: true,
            },
        );
        if let Some(seat) = self.servers.get_mut(&server_key.0) {
            seat.ownerless_lobbies = seat.ownerless_lobbies.saturating_add(1);
        }
        self.enter(member, id);
        Ok(id)
    }

    fn set_reach_mode_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        relay_only: bool,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        let decision = {
            let room = self.rooms.get(&lobby).ok_or(LobbyError::NotInLobby)?;
            let player = self.sessions.get(&member).and_then(|s| s.identity);
            if room.kind == LobbyKind::Listen {
                if !room.has(member) {
                    Err(LobbyError::NotInLobby.into())
                } else if room.owner != member {
                    Err(LobbyError::NotOwner.into())
                } else {
                    Ok(())
                }
            } else if room.delegated {
                let owner = player.is_some() && player == room.owner_key;
                if owner || member == room.server_member {
                    Ok(())
                } else {
                    Err(HubError::BindingNotOwner)
                }
            } else if room.ownerless && member == room.server_member {
                Ok(())
            } else {
                Err(LobbyError::NotOwner.into())
            }
        };
        decision?;
        if let Some(room) = self.rooms.get_mut(&lobby) {
            room.relay_only = relay_only;
        }
        let ids = self.member_ids(lobby);
        if let Some(state) = self.room_state(lobby) {
            for id in ids {
                self.push(id, HubEvent::Room(state.clone()));
            }
        }
        self.note_lobby(lobby);
        Ok(())
    }

    fn close_inner(&mut self, member: MemberId, lobby: LobbyId, now_ms: u64) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        let room = self.rooms.get(&lobby).ok_or(LobbyError::NotFound)?;
        if room.kind != LobbyKind::Dedicated {
            return Err(HubError::NotReady);
        }
        let player = self.sessions.get(&member).and_then(|s| s.identity);
        let allowed = if room.delegated {
            player.is_some() && player == room.owner_key
        } else if room.ownerless {
            member == room.server_member
        } else {
            false
        };
        if !allowed {
            return Err(if room.delegated {
                HubError::BindingNotOwner
            } else {
                LobbyError::NotOwner.into()
            });
        }
        self.finish_dedicated(lobby);
        Ok(())
    }

    fn transfer_inner(
        &mut self,
        member: MemberId,
        lobby: LobbyId,
        to: PlayerKey,
        now_ms: u64,
    ) -> Result<(), HubError> {
        self.touch(member, now_ms)?;
        let room = self.rooms.get(&lobby).ok_or(LobbyError::NotFound)?;
        if !room.delegated {
            return Err(HubError::BindingNotOwner);
        }
        let player = self.sessions.get(&member).and_then(|s| s.identity);
        if player != room.owner_key {
            return Err(HubError::BindingNotOwner);
        }
        let server_key = room.server_key.ok_or(HubError::ServerOffline)?;
        if to.0 == server_key.0 {
            return Err(HubError::BindingNotOwner);
        }
        let live = self
            .servers
            .get(&server_key.0)
            .is_some_and(|s| s.owners.contains(&to))
            && self
                .bindings
                .get(&(to.0, server_key.0))
                .is_some_and(|b| b.state == BindingState::Live);
        if !live || self.revoked_player(to, server_key) {
            return Err(HubError::BindingNotOwner);
        }
        let target_member = self.by_identity.get(&to).copied();
        let label = target_member
            .and_then(|id| self.sessions.get(&id).map(|s| s.handle.clone()))
            .unwrap_or_default();
        let room = self.rooms.get_mut(&lobby).ok_or(LobbyError::NotFound)?;
        room.owner_key = Some(to);
        room.owner_label = label;
        room.grace = None;
        if let Some(id) = target_member {
            room.owner = id;
            if !room.members.iter().any(|m| m.id == id) {
                room.members.push(RoomMember {
                    id,
                    data: BTreeMap::new(),
                });
            }
            let others: Vec<MemberId> = room.members.iter().map(|m| m.id).filter(|m| *m != id).collect();
            for other in others {
                self.push(other, HubEvent::OwnerChanged { lobby, owner: id });
            }
            if self.sessions.get(&id).and_then(|s| s.lobby).is_none() {
                self.enter(id, lobby);
            }
        } else {
            room.owner = MemberId(0);
        }
        Ok(())
    }
}

/// The dedicated-server fields copied onto a new lobby.
struct DedicatedSeat {
    server_key: Option<ServerKey>,
    server_member: MemberId,
    capacity: u16,
    load: u16,
    relay_only: bool,
    grace_ms: u32,
    delegated: bool,
    ownerless: bool,
}

impl HubServer {
    fn insert_dedicated(
        &mut self,
        id: LobbyId,
        owner: MemberId,
        spec: &LobbySpec,
        owner_key: Option<PlayerKey>,
        owner_label: String,
        seat: DedicatedSeat,
    ) {
        self.dedicated_ids.insert(id);
        self.rooms.insert(
            id,
            Room {
                owner,
                visibility: spec.visibility,
                max_members: spec.max_members,
                joinable: spec.joinable,
                data: spec.data.clone(),
                members: vec![RoomMember {
                    id: owner,
                    data: BTreeMap::new(),
                }],
                confirmed: false,
                bans: BTreeSet::new(),
                muted: BTreeSet::new(),
                muted_keys: BTreeSet::new(),
                chat: VecDeque::new(),
                chat_seq: 0,
                chat_times: BTreeMap::new(),
                data_times: Vec::new(),
                offer: None,
                joins: BTreeMap::new(),
                kind: LobbyKind::Dedicated,
                server_key: seat.server_key,
                server_member: seat.server_member,
                capacity: seat.capacity,
                load: seat.load,
                relay_only: seat.relay_only,
                grace_ms: seat.grace_ms,
                grace: None,
                owner_key,
                owner_label,
                delegated: seat.delegated,
                ownerless: seat.ownerless,
            },
        );
    }

    fn begin_grace(&mut self, member: MemberId, lobby: LobbyId, reason: LeaveReason, now_ms: u64) {
        let Some(room) = self.rooms.get_mut(&lobby) else {
            return;
        };
        room.members.retain(|m| m.id != member);
        room.muted.remove(&member);
        room.chat_times.remove(&member);
        room.joins.remove(&member);
        let deadline = if room.grace_ms == 0 {
            0
        } else {
            now_ms.saturating_add(u64::from(room.grace_ms))
        };
        room.grace = Some(deadline);
        room.owner = MemberId(0);
        let server_member = room.server_member;
        let remaining: Vec<MemberId> = room.members.iter().map(|m| m.id).collect();
        if let Some(s) = self.sessions.get_mut(&member)
            && s.lobby == Some(lobby)
        {
            s.lobby = None;
            s.presence = HubPresence::Menu;
        }
        self.push(member, HubEvent::Left { lobby, reason });
        self.note_presence(member);
        for other in remaining {
            self.push(other, HubEvent::MemberLeft { lobby, member });
            self.push(
                other,
                HubEvent::GraceStarted {
                    lobby,
                    deadline_ms: deadline,
                },
            );
        }
        if server_member != MemberId(0) {
            self.push(server_member, HubEvent::MemberLeft { lobby, member });
            self.push(
                server_member,
                HubEvent::GraceStarted {
                    lobby,
                    deadline_ms: deadline,
                },
            );
        }
        self.note_lobby(lobby);
    }

    fn expire_grace(&mut self, now_ms: u64) {
        let due: Vec<LobbyId> = self
            .rooms
            .iter()
            .filter(|(_, room)| matches!(room.grace, Some(deadline) if deadline != 0 && now_ms >= deadline))
            .map(|(id, _)| *id)
            .collect();
        for lobby in due {
            self.finish_dedicated(lobby);
        }
    }

    fn finish_dedicated(&mut self, lobby: LobbyId) {
        let Some(room) = self.rooms.remove(&lobby) else {
            return;
        };
        self.invites.retain(|(id, _), _| *id != lobby);
        if room.ownerless
            && let Some(key) = room.server_key
            && let Some(seat) = self.servers.get_mut(&key.0)
        {
            seat.ownerless_lobbies = seat.ownerless_lobbies.saturating_sub(1);
        }
        let members: Vec<MemberId> = room.members.iter().map(|m| m.id).collect();
        let server_member = room.server_member;
        for m in &members {
            if let Some(s) = self.sessions.get_mut(m)
                && s.lobby == Some(lobby)
            {
                s.lobby = None;
                s.presence = HubPresence::Menu;
            }
            self.push(
                *m,
                HubEvent::Left {
                    lobby,
                    reason: LeaveReason::Left,
                },
            );
            self.push(*m, HubEvent::GraceEnded { lobby });
            self.note_presence(*m);
        }
        if server_member != MemberId(0) && !members.contains(&server_member) {
            self.push(server_member, HubEvent::GraceEnded { lobby });
        }
        self.note_lobby(lobby);
    }

    fn server_offline(&mut self, key: ServerKey) {
        let lobbies: Vec<LobbyId> = self
            .rooms
            .iter()
            .filter(|(_, room)| room.server_key == Some(key))
            .map(|(id, _)| *id)
            .collect();
        if let Some(seat) = self.servers.get_mut(&key.0) {
            seat.online = false;
        }
        for lobby in lobbies {
            if let Some(room) = self.rooms.get_mut(&lobby) {
                room.offer = None;
                room.joins.clear();
            }
            self.finish_dedicated(lobby);
        }
        if let Some(s) = self.sessions.iter_mut().find(|(_, s)| s.server_key == Some(key)) {
            // The session that went quiet is cleared by the caller after this returns. Mark the
            // seat's member empty so a late event does not target a session we are removing.
            let _ = s;
        }
        if let Some(seat) = self.servers.get_mut(&key.0) {
            seat.member = MemberId(0);
        }
        self.emit_server(key, false);
    }

    fn reattach_owner(&mut self, member: MemberId) {
        let Some(key) = self.sessions.get(&member).and_then(|s| s.identity) else {
            return;
        };
        let lobbies: Vec<LobbyId> = self
            .rooms
            .iter()
            .filter(|(_, room)| room.delegated && room.grace.is_some() && room.owner_key == Some(key))
            .map(|(id, _)| *id)
            .collect();
        let mut entered = self.sessions.get(&member).and_then(|s| s.lobby).is_some();
        let handle = self
            .sessions
            .get(&member)
            .map(|s| s.handle.clone())
            .unwrap_or_default();
        for lobby in lobbies {
            let others = {
                let Some(room) = self.rooms.get_mut(&lobby) else {
                    continue;
                };
                room.grace = None;
                room.owner = member;
                room.owner_label = handle.clone();
                if !room.members.iter().any(|m| m.id == member) {
                    room.members.push(RoomMember {
                        id: member,
                        data: BTreeMap::new(),
                    });
                }
                room.members
                    .iter()
                    .map(|m| m.id)
                    .filter(|id| *id != member)
                    .collect::<Vec<_>>()
            };
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
            if !entered {
                self.enter(member, lobby);
                entered = true;
            }
        }
    }

    fn revoked(&self, player: PlayerKey, server: ServerKey, serial: u64) -> bool {
        self.revocations
            .iter()
            .any(|r| r.player == player && r.server == server.0 && r.serial == serial)
    }

    fn revoked_player(&self, player: PlayerKey, server: ServerKey) -> bool {
        self.bindings
            .get(&(player.0, server.0))
            .is_some_and(|b| self.revoked(player, server, b.serial) || b.state == BindingState::Revoked)
    }

    fn emit_server(&mut self, key: ServerKey, online: bool) {
        let Some(seat) = self.servers.get(&key.0) else {
            return;
        };
        let notice = ServerNotice {
            server_key: key,
            capacity: seat.capacity,
            load: seat.load,
            protocol: seat.protocol,
            game_version: seat.game_version.clone(),
            relay_only: seat.relay_only,
            grace_ms: seat.grace_ms,
            owner_count: u8::try_from(seat.owners.len()).unwrap_or(u8::MAX),
        };
        let mut targets = Vec::new();
        if seat.member != MemberId(0) {
            targets.push(seat.member);
        }
        for owner in &seat.owners {
            if let Some(id) = self.by_identity.get(owner).copied() {
                targets.push(id);
            }
        }
        targets.sort_unstable();
        targets.dedup();
        let event = if online {
            HubEvent::ServerOnline(notice)
        } else {
            HubEvent::ServerOffline(notice)
        };
        for target in targets {
            self.push(target, event.clone());
        }
    }

    fn emit_binding(&mut self, player: PlayerKey, server: ServerKey, serial: u64, state: BindingState) {
        let event = HubEvent::BindingChanged {
            server_key: server,
            serial,
            state,
        };
        if let Some(id) = self.by_identity.get(&player).copied() {
            self.push(id, event.clone());
        }
        if let Some(seat) = self.servers.get(&server.0) {
            let member = seat.member;
            if member != MemberId(0) {
                self.push(member, event);
            }
        }
    }

    fn server_cards(&self) -> Vec<HubServerCard> {
        let mut cards: Vec<HubServerCard> = self
            .servers
            .values()
            .filter(|s| s.online)
            .map(|s| HubServerCard {
                server_key: s.key,
                capacity: s.capacity,
                load: s.load,
                protocol: s.protocol,
                game_version: s.game_version.clone(),
                relay_only: s.relay_only,
                owner_count: u8::try_from(s.owners.len()).unwrap_or(u8::MAX),
                grace_ms: s.grace_ms,
            })
            .collect();
        cards.sort_by_key(|a| a.server_key.0);
        cards
    }

    fn persist_revocations(&self) -> Result<(), String> {
        let Some(dir) = &self.revocation_dir else {
            return Ok(());
        };
        if let Err(err) = std::fs::create_dir_all(dir) {
            return Err(format!("{}: {err}", dir.display()));
        }
        let file = RevocationFile {
            format: REVOCATION_FORMAT,
            entries: self
                .revocations
                .iter()
                .map(|r| RevocationRow {
                    player: r.player.to_string(),
                    server: ServerKey(r.server).to_string(),
                    serial: r.serial,
                    expiry_ms: r.expiry_ms,
                })
                .collect(),
        };
        let text = ron::ser::to_string_pretty(&file, regia_core::ron_pretty())
            .map_err(|e| format!("revocations.ron: {e}"))?;
        let tmp = dir.join(format!(
            "revocations.ron.{}.{}.tmp",
            std::process::id(),
            REVOCATION_TMP.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(err) = std::fs::write(&tmp, text) {
            return Err(format!("{}: {err}", tmp.display()));
        }
        let path = dir.join("revocations.ron");
        if let Err(err) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("{}: {err}", path.display()));
        }
        Ok(())
    }
}

/// The offer a `ReachAsk` may carry. The stored ask keeps `offer` unchanged, so a later
/// clear of the flag still has the member's candidates. The lobby flag, this offer's flag,
/// or the owner's offer flag drops the candidates from the event.
fn announced_reach_offer(room: &Room, offer: &ReachOffer) -> ReachOffer {
    let hide = room.relay_only || offer.relay_only || room.offer.as_ref().is_some_and(|o| o.relay_only);
    if !hide {
        return offer.clone();
    }
    let mut announced = offer.clone();
    announced.candidates.clear();
    announced
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
