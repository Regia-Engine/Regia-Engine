// SPDX-License-Identifier: MPL-2.0
//! Lobbies and matchmaking (spec 89, ADR-1877).
//!
//! A lobby is a small platform-held room: key/value data, per-member data, a member limit, a
//! visibility and a joinable flag. It is where players gather and learn where the game session
//! is; the session itself is spec 77's `crate::NetSession`.
//!
//! | Module | What |
//! |---|---|
//! | [`query`] | [`LobbyQuery`] filters and the deterministic evaluation the local backend uses |
//! | [`local`] | [`LocalLobbyHub`]: an in-memory lobby directory (tests, play-in-editor, LAN builds) |
//! | [`matchmaker`] | [`Matchmaker`]: list, join, widen, create, ready, launch; stepped per frame |
//! | `plugin` | The `Lobbies` resource and `LobbyPlugin` |
//! | [`hub`] | A self-hosted lobby hub: [`hub::HubServer`], [`hub::HubHost`], [`hub::HubLobbyBackend`] (ADR-8559) |
//!
//! Games code against [`LobbyBackend`]; `regia_steam` provides the Steam implementation.
//! Operations that wait on the platform (`create`, `join`, `request_list`) return a
//! [`RequestId`] and report their result as a [`LobbyEvent`] on a later [`LobbyBackend::poll`].

use std::collections::BTreeMap;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

pub mod hub;
pub mod local;
pub mod matchmaker;
#[cfg(feature = "ecs")]
pub mod plugin;
pub mod query;

pub use local::{LocalLobbyBackend, LocalLobbyHub};
pub use matchmaker::{MatchFailure, MatchState, Matchmaker, MatchmakingRequest};
#[cfg(feature = "ecs")]
pub use plugin::LobbyIdentity;
#[cfg(feature = "ecs")]
pub use plugin::{
    LaunchError, Lobbies, LobbyMessage, LobbyPlugin, LobbyTransport, LoopbackLobbyTransport,
    lobby_follow_manifest, lobby_system,
};
mod settings;
pub use query::{Compare, Distance, LobbyQuery, NearFilter, NumberFilter, StringFilter};
pub use settings::LobbySettings;

/// Most members a lobby can hold (Steamworks `CreateLobby` limit).
pub const MAX_LOBBY_MEMBERS: u32 = 250;
/// Longest lobby data key in bytes (Steamworks `k_nMaxLobbyKeyLength`).
pub const MAX_KEY_BYTES: usize = 255;
/// Longest lobby data value in bytes (Steamworks `SetLobbyData`).
pub const MAX_VALUE_BYTES: usize = 8192;
/// Longest chat message in bytes (Steamworks `SendLobbyChatMsg`).
pub const MAX_CHAT_BYTES: usize = 4096;
/// Most data keys per lobby, and per member (a Regia limit that bounds memory and agent output).
pub const MAX_KEYS: usize = 64;
/// Most results one list request returns (Steamworks result-count filter limit).
pub const MAX_LIST_RESULTS: u32 = 50;
/// The prefix of the keys the engine owns (spec 89 § 4).
pub const RESERVED_PREFIX: &str = "regia.";

/// The engine's lobby keys (spec 89 § 4).
pub mod keys {
    /// Lobby key layout version (`2` since ADR-8897 added `regia.mode` and `regia.playlist`).
    pub const VERSION: &str = "regia.v";
    /// The game id.
    pub const GAME: &str = "regia.game";
    /// The protocol hash, 16 lowercase hex digits.
    pub const PROTOCOL: &str = "regia.protocol";
    /// The content hash, 16 lowercase hex digits.
    pub const CONTENT: &str = "regia.content";
    /// Where the launched session is (`udp:<addr>` or `steam:<id>`).
    pub const SERVER: &str = "regia.server";
    /// `open`, `launching` or `in_game`.
    pub const STATE: &str = "regia.state";
    /// Member data: `1` when the member is ready.
    pub const READY: &str = "regia.ready";
    /// The session's mode: the current playlist row's rule-set id or mode string, else the
    /// board's mode lobby id (ADR-8897). Optional: written by a host that publishes its session.
    pub const MODE: &str = "regia.mode";
    /// The session's playlist id (`Playlist::id`, ADR-8897). Optional, like [`MODE`].
    pub const PLAYLIST: &str = "regia.playlist";

    /// Every engine key, in the order above (the lobby-keys layout `cargo xtask check-formats` hashes,
    /// ADR-2316; `regia.mode` and `regia.playlist` appended by ADR-8897).
    pub const ALL: &[&str] = &[
        VERSION, GAME, PROTOCOL, CONTENT, SERVER, STATE, READY, MODE, PLAYLIST,
    ];

    /// The values of [`STATE`].
    pub const STATES: &[&str] = &["open", "launching", "in_game"];

    /// The prefixes of a [`SERVER`] value (`udp:<ip:port>`, `steam:<id>`).
    pub const SERVER_PREFIXES: &[&str] = &["udp:", "steam:"];
}

/// The lobby key layout version written as `regia.v`: 2 since ADR-8897 (two optional keys; a v1 lobby
/// reads unchanged, it just has neither key).
pub const LOBBY_KEYS_VERSION: u32 = 2;

/// A lobby's id. Steam: the lobby's 64-bit Steam id. Local: hub-assigned, never 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LobbyId(pub u64);

impl std::fmt::Display for LobbyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A lobby member. Steam: the member's 64-bit Steam id. Local: the id given to
/// [`LocalLobbyHub::member`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MemberId(pub u64);

impl std::fmt::Display for MemberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The id of a pending `create`, `join` or `request_list`; its result arrives as a [`LobbyEvent`]
/// carrying the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RequestId(pub u64);

/// Who can find and join a lobby (Steam's lobby types).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LobbyVisibility {
    /// Listed and joinable by anyone.
    #[default]
    Public,
    /// Joinable by friends of members; not listed.
    FriendsOnly,
    /// Joinable by invite only; not listed.
    Private,
    /// Not listed, joinable by id (for a party that only ever joins by invite or code).
    Invisible,
}

impl LobbyVisibility {
    /// The stable name (`public`, `friends_only`, `private`, `invisible`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::FriendsOnly => "friends_only",
            Self::Private => "private",
            Self::Invisible => "invisible",
        }
    }

    /// Parses a stable name.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "public" => Self::Public,
            "friends_only" => Self::FriendsOnly,
            "private" => Self::Private,
            "invisible" => Self::Invisible,
            _ => return None,
        })
    }
}

/// What [`LobbyBackend::create`] asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LobbySpec {
    /// Who can find and join it.
    pub visibility: LobbyVisibility,
    /// Member limit, `1..=MAX_LOBBY_MEMBERS`.
    pub max_members: u32,
    /// Lobby data written right after creation, before `Created` is reported.
    pub data: BTreeMap<String, String>,
    /// Whether others may join.
    pub joinable: bool,
}

impl Default for LobbySpec {
    fn default() -> Self {
        Self {
            visibility: LobbyVisibility::Public,
            max_members: 8,
            data: BTreeMap::new(),
            joinable: true,
        }
    }
}

/// One member of a joined lobby.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LobbyMember {
    /// The member's id.
    pub id: MemberId,
    /// Display name (the platform persona name; may be empty).
    pub name: String,
    /// The member's own data.
    pub data: BTreeMap<String, String>,
}

/// Where a launched session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LobbyServer {
    /// A UDP (or loopback) address.
    Address(SocketAddr),
    /// A Steam user or game server id, reached through the Steam sockets transport.
    Steam(u64),
}

impl LobbyServer {
    /// The `regia.server` text: `udp:<ip:port>` or `steam:<id>`.
    pub fn to_key(&self) -> String {
        match self {
            Self::Address(a) => format!("udp:{a}"),
            Self::Steam(id) => format!("steam:{id}"),
        }
    }

    /// Parses the `regia.server` text.
    pub fn from_key(text: &str) -> Option<Self> {
        if let Some(addr) = text.strip_prefix("udp:") {
            return addr.parse().ok().map(Self::Address);
        }
        if let Some(id) = text.strip_prefix("steam:") {
            return id.parse().ok().filter(|id| *id != 0).map(Self::Steam);
        }
        None
    }
}

/// A lobby as a backend reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LobbyInfo {
    /// Its id.
    pub id: LobbyId,
    /// The current owner.
    pub owner: MemberId,
    /// Who can find and join it.
    pub visibility: LobbyVisibility,
    /// Member limit.
    pub max_members: u32,
    /// Whether others may join.
    pub joinable: bool,
    /// Members in join order. Listings leave this empty and carry [`LobbyInfo::member_count`].
    pub members: Vec<LobbyMember>,
    /// How many members are in it.
    pub member_count: u32,
    /// Lobby data.
    pub data: BTreeMap<String, String>,
    /// Where the game session is, once launched.
    pub server: Option<LobbyServer>,
}

impl LobbyInfo {
    /// Free member slots.
    pub fn open_slots(&self) -> u32 {
        self.max_members.saturating_sub(self.member_count)
    }

    /// Whether a member of this id is in the lobby (members listed only).
    pub fn has_member(&self, member: MemberId) -> bool {
        self.members.iter().any(|m| m.id == member)
    }

    /// Whether every listed member has `regia.ready` = `1`.
    pub fn all_ready(&self) -> bool {
        !self.members.is_empty()
            && self
                .members
                .iter()
                .all(|m| m.data.get(keys::READY).map(String::as_str) == Some("1"))
    }
}

/// Why we are no longer in a lobby.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveReason {
    /// We left.
    Left,
    /// The owner (or the platform) removed us.
    Kicked,
    /// The lobby is gone or the platform connection dropped.
    Lost,
}

impl LeaveReason {
    /// The stable code (`left`, `kicked`, `lost`).
    pub fn code(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Kicked => "kicked",
            Self::Lost => "lost",
        }
    }
}

/// Something happened to a lobby; drained by [`LobbyBackend::poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LobbyEvent {
    /// A `create` finished; on success we are its owner and only member.
    Created {
        /// The request.
        request: RequestId,
        /// The new lobby, or why not.
        result: Result<LobbyId, LobbyError>,
    },
    /// A `join` finished.
    Entered {
        /// The request.
        request: RequestId,
        /// The joined lobby, or why not.
        result: Result<LobbyId, LobbyError>,
    },
    /// A `request_list` finished.
    List {
        /// The request.
        request: RequestId,
        /// Matching lobbies in result order (members not listed), or why not.
        result: Result<Vec<LobbyInfo>, LobbyError>,
    },
    /// Someone joined a lobby we are in.
    MemberJoined {
        /// The lobby.
        lobby: LobbyId,
        /// Who.
        member: MemberId,
    },
    /// Someone left a lobby we are in.
    MemberLeft {
        /// The lobby.
        lobby: LobbyId,
        /// Who.
        member: MemberId,
    },
    /// Lobby data (`member: None`) or a member's data changed.
    DataChanged {
        /// The lobby.
        lobby: LobbyId,
        /// The member whose data changed; `None` for lobby data.
        member: Option<MemberId>,
    },
    /// The lobby has a new owner.
    OwnerChanged {
        /// The lobby.
        lobby: LobbyId,
        /// The new owner.
        owner: MemberId,
    },
    /// A chat message.
    Chat {
        /// The lobby.
        lobby: LobbyId,
        /// Who sent it.
        from: MemberId,
        /// The text.
        text: String,
    },
    /// The owner launched: the game session is at `server`.
    ServerSet {
        /// The lobby.
        lobby: LobbyId,
        /// Where to connect.
        server: LobbyServer,
    },
    /// The local user accepted an invite (Steam `GameLobbyJoinRequested`); join `lobby` to follow it.
    InviteAccepted {
        /// The lobby to join.
        lobby: LobbyId,
        /// The friend who invited.
        from: MemberId,
    },
    /// We are no longer in this lobby.
    Left {
        /// The lobby.
        lobby: LobbyId,
        /// Why.
        reason: LeaveReason,
    },
    /// A brokered join reached the owner's game session (the lobby hub, NL-05): `member` (we, or
    /// for the owner the member that reported) connected over `path`.
    Reached {
        /// The lobby.
        lobby: LobbyId,
        /// Who connected.
        member: MemberId,
        /// The path that carried the sealed handshake (mapped, punch, relay).
        path: hub::ReachKind,
    },
}

impl LobbyEvent {
    /// The stable event code (spec 89 § 9).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Created { .. } => "created",
            Self::Entered { .. } => "entered",
            Self::List { .. } => "list",
            Self::MemberJoined { .. } => "member_joined",
            Self::MemberLeft { .. } => "member_left",
            Self::DataChanged { .. } => "data_changed",
            Self::OwnerChanged { .. } => "owner_changed",
            Self::Chat { .. } => "chat",
            Self::ServerSet { .. } => "server_set",
            Self::InviteAccepted { .. } => "invite_accepted",
            Self::Left { .. } => "left",
            Self::Reached { .. } => "reached",
        }
    }

    /// The lobby the event is about, if any.
    pub fn lobby(&self) -> Option<LobbyId> {
        match self {
            Self::Created { result, .. } | Self::Entered { result, .. } => result.as_ref().ok().copied(),
            Self::List { .. } => None,
            Self::MemberJoined { lobby, .. }
            | Self::MemberLeft { lobby, .. }
            | Self::DataChanged { lobby, .. }
            | Self::OwnerChanged { lobby, .. }
            | Self::Chat { lobby, .. }
            | Self::ServerSet { lobby, .. }
            | Self::InviteAccepted { lobby, .. }
            | Self::Left { lobby, .. }
            | Self::Reached { lobby, .. } => Some(*lobby),
        }
    }
}

/// Why a lobby call failed. [`LobbyError::code`] is the stable agent `data.reason`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LobbyError {
    /// No lobby platform (Steam not running, feature off, Android).
    #[error("no lobby backend is available")]
    Unavailable,
    /// The call needs a lobby we are in.
    #[error("not in that lobby")]
    NotInLobby,
    /// Already in a lobby; leave first.
    #[error("already in a lobby")]
    AlreadyInLobby,
    /// Only the owner may do this.
    #[error("only the lobby owner may do this")]
    NotOwner,
    /// The lobby is full or not joinable.
    #[error("the lobby is full or not joinable")]
    Full,
    /// No such lobby.
    #[error("no such lobby")]
    NotFound,
    /// A key is empty, too long or holds a NUL.
    #[error("lobby key must be 1..=255 bytes without NUL")]
    KeyInvalid,
    /// A value is too long or holds a NUL.
    #[error("lobby value must be at most 8192 bytes without NUL")]
    ValueTooLong,
    /// A `regia.` key set through the game API.
    #[error("keys starting `regia.` belong to the engine")]
    ReservedKey,
    /// More than [`MAX_KEYS`] keys.
    #[error("at most 64 data keys per lobby or member")]
    TooManyKeys,
    /// A chat message is empty, too long or holds a NUL.
    #[error("chat text must be 1..=4096 bytes without NUL")]
    ChatInvalid,
    /// `max_members` out of `1..=250`.
    #[error("max_members must be 1..=250")]
    MembersInvalid,
    /// A query field is out of range.
    #[error("invalid lobby query: {0}")]
    QueryInvalid(String),
    /// The platform reported a failure.
    #[error("the lobby platform refused: {0}")]
    Platform(String),
    /// A create, list or matchmaking call with no lobby identity: no `NetSession` manifest to
    /// take it from, no `Lobbies::set_identity` or `Lobbies::set_game`, and no
    /// `Lobbies::allow_unidentified` opt-in (ADR-7881).
    #[error(
        "no lobby identity: add NetSessionPlugin, call Lobbies::set_identity or Lobbies::set_game, \
         or opt in with Lobbies::allow_unidentified"
    )]
    NoIdentity,
}

impl LobbyError {
    /// The stable code (spec 89 § 9).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable => "lobby_unavailable",
            Self::NotInLobby => "not_in_lobby",
            Self::AlreadyInLobby => "already_in_lobby",
            Self::NotOwner => "not_owner",
            Self::Full => "lobby_full",
            Self::NotFound => "lobby_not_found",
            Self::KeyInvalid => "key_invalid",
            Self::ValueTooLong => "value_too_long",
            Self::ReservedKey => "reserved_key",
            Self::TooManyKeys => "too_many_keys",
            Self::ChatInvalid => "chat_invalid",
            Self::MembersInvalid => "members_invalid",
            Self::QueryInvalid(_) => "query_invalid",
            Self::Platform(_) => "lobby_platform",
            Self::NoIdentity => "lobby_no_identity",
        }
    }
}

/// Checks a data key (any key, reserved or not).
pub fn check_key(key: &str) -> Result<(), LobbyError> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES || key.contains('\0') {
        return Err(LobbyError::KeyInvalid);
    }
    Ok(())
}

/// Checks a game-set key: valid and not reserved.
pub fn check_game_key(key: &str) -> Result<(), LobbyError> {
    check_key(key)?;
    if key.starts_with(RESERVED_PREFIX) {
        return Err(LobbyError::ReservedKey);
    }
    Ok(())
}

/// Checks a data value.
pub fn check_value(value: &str) -> Result<(), LobbyError> {
    if value.len() > MAX_VALUE_BYTES || value.contains('\0') {
        return Err(LobbyError::ValueTooLong);
    }
    Ok(())
}

/// Checks a chat message.
pub fn check_chat(text: &str) -> Result<(), LobbyError> {
    if text.is_empty() || text.len() > MAX_CHAT_BYTES || text.contains('\0') {
        return Err(LobbyError::ChatInvalid);
    }
    Ok(())
}

/// Checks a create request: member limit, data keys and values (reserved keys allowed: the engine
/// writes its own through here).
pub fn check_spec(spec: &LobbySpec) -> Result<(), LobbyError> {
    if spec.max_members == 0 || spec.max_members > MAX_LOBBY_MEMBERS {
        return Err(LobbyError::MembersInvalid);
    }
    if spec.data.len() > MAX_KEYS {
        return Err(LobbyError::TooManyKeys);
    }
    for (k, v) in &spec.data {
        check_key(k)?;
        check_value(v)?;
    }
    Ok(())
}

/// A 64-bit hash as the 16 lowercase hex digits the engine keys use.
pub fn hash_hex(hash: u64) -> String {
    format!("{hash:016x}")
}

/// The engine keys a new lobby carries (spec 89 § 4): version, game, protocol and content hash
/// (from the session's `ContentManifest`), state `open`.
pub fn engine_keys(game: &str, protocol_hash: u64, content_hash: u64) -> BTreeMap<String, String> {
    let mut data = BTreeMap::new();
    data.insert(keys::VERSION.into(), LOBBY_KEYS_VERSION.to_string());
    data.insert(keys::GAME.into(), game.into());
    data.insert(keys::PROTOCOL.into(), hash_hex(protocol_hash));
    data.insert(keys::CONTENT.into(), hash_hex(content_hash));
    data.insert(keys::STATE.into(), "open".into());
    data
}

/// The optional rotation keys (ADR-8897): `regia.mode` and `regia.playlist`, each only when its
/// value is not empty.
pub fn rotation_keys(mode: &str, playlist: &str) -> BTreeMap<String, String> {
    let mut data = BTreeMap::new();
    if !mode.is_empty() {
        data.insert(keys::MODE.into(), mode.into());
    }
    if !playlist.is_empty() {
        data.insert(keys::PLAYLIST.into(), playlist.into());
    }
    data
}

/// The rotation keys of `session` (ADR-8897): `regia.mode` is the current row's `mode` (a rule-set
/// id or a mode string), else the board's mode lobby id; `regia.playlist` is the playlist's id when
/// it has one.
pub fn session_rotation_keys(session: &regia_rules::session::MatchSession) -> BTreeMap<String, String> {
    let mode = session
        .current
        .as_ref()
        .map(|row| row.entry.mode.as_str())
        .filter(|mode| !mode.is_empty())
        .unwrap_or_else(|| session.board.mode.as_lobby_str());
    rotation_keys(mode, &session.playlist.id)
}

/// Everything a lobby platform provides (spec 89 § 3). Implementations are internally
/// synchronized; all methods take `&self`.
pub trait LobbyBackend: Send + Sync {
    /// `local` or `steam` (`none` for [`UnavailableLobbies`], the no-platform default).
    fn name(&self) -> &'static str;
    /// The local user's member id; `None` when the platform is not available.
    fn local_member(&self) -> Option<MemberId>;
    /// Creates a lobby we own; the result arrives as [`LobbyEvent::Created`].
    fn create(&self, spec: &LobbySpec) -> Result<RequestId, LobbyError>;
    /// Joins a lobby; the result arrives as [`LobbyEvent::Entered`].
    fn join(&self, lobby: LobbyId) -> Result<RequestId, LobbyError>;
    /// Leaves a lobby (no-op when not in it). A [`LobbyEvent::Left`] follows.
    fn leave(&self, lobby: LobbyId);
    /// Lists lobbies; the result arrives as [`LobbyEvent::List`].
    fn request_list(&self, query: &LobbyQuery) -> Result<RequestId, LobbyError>;
    /// A joined lobby with its members; `None` when we are not in it.
    fn lobby(&self, lobby: LobbyId) -> Option<LobbyInfo>;
    /// Whether this peer is still a member of `lobby`, without building the member list.
    ///
    /// The default asks [`Self::lobby`]. A backend that can answer from its join table overrides
    /// this so a quiet frame does not rebuild persona names. `false` is the lost-lobby signal
    /// `crate::lobby::Lobbies::pump` already treats as a missing `Left` event.
    fn still_in(&self, lobby: LobbyId) -> bool {
        self.lobby(lobby).is_some()
    }
    /// Sets (`Some`) or deletes (`None`) lobby data. Owner only. Reserved keys are allowed here:
    /// the game-facing `Lobbies` API checks them.
    fn set_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError>;
    /// Sets or deletes the local member's data.
    fn set_member_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError>;
    /// Opens or closes the lobby to joins. Owner only.
    fn set_joinable(&self, lobby: LobbyId, joinable: bool) -> Result<(), LobbyError>;
    /// Publishes where the game session is. Owner only.
    fn set_server(&self, lobby: LobbyId, server: LobbyServer) -> Result<(), LobbyError>;
    /// Withdraws what [`LobbyBackend::set_server`] published. Owner only. `Lobbies::launch` calls it
    /// when a launch is rolled back. The default deletes the `regia.server` key; a backend
    /// that also writes a platform field in `set_server` (Steam's lobby game server) clears that
    /// field here too (KI-NT-050, ADR-10069).
    fn clear_server(&self, lobby: LobbyId) -> Result<(), LobbyError> {
        self.set_data(lobby, keys::SERVER, None)
    }
    /// Sends a chat message to every member.
    fn send_chat(&self, lobby: LobbyId, text: &str) -> Result<(), LobbyError>;
    /// Drains the events since the last call. Called once per frame by `LobbyPlugin`.
    fn poll(&self) -> Vec<LobbyEvent>;
}

/// A backend that has no platform: every call is [`LobbyError::Unavailable`]. The default of
/// `Lobbies`.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableLobbies;

impl LobbyBackend for UnavailableLobbies {
    fn name(&self) -> &'static str {
        "none"
    }
    fn local_member(&self) -> Option<MemberId> {
        None
    }
    fn create(&self, _spec: &LobbySpec) -> Result<RequestId, LobbyError> {
        Err(LobbyError::Unavailable)
    }
    fn join(&self, _lobby: LobbyId) -> Result<RequestId, LobbyError> {
        Err(LobbyError::Unavailable)
    }
    fn leave(&self, _lobby: LobbyId) {}
    fn request_list(&self, _query: &LobbyQuery) -> Result<RequestId, LobbyError> {
        Err(LobbyError::Unavailable)
    }
    fn lobby(&self, _lobby: LobbyId) -> Option<LobbyInfo> {
        None
    }
    fn set_data(&self, _lobby: LobbyId, _key: &str, _value: Option<&str>) -> Result<(), LobbyError> {
        Err(LobbyError::Unavailable)
    }
    fn set_member_data(&self, _lobby: LobbyId, _key: &str, _value: Option<&str>) -> Result<(), LobbyError> {
        Err(LobbyError::Unavailable)
    }
    fn set_joinable(&self, _lobby: LobbyId, _joinable: bool) -> Result<(), LobbyError> {
        Err(LobbyError::Unavailable)
    }
    fn set_server(&self, _lobby: LobbyId, _server: LobbyServer) -> Result<(), LobbyError> {
        Err(LobbyError::Unavailable)
    }
    fn send_chat(&self, _lobby: LobbyId, _text: &str) -> Result<(), LobbyError> {
        Err(LobbyError::Unavailable)
    }
    fn poll(&self) -> Vec<LobbyEvent> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_follow_the_platform_documentation() {
        assert!(check_key("mode").is_ok());
        assert_eq!(check_key(""), Err(LobbyError::KeyInvalid));
        assert!(check_key(&"k".repeat(255)).is_ok());
        assert_eq!(check_key(&"k".repeat(256)), Err(LobbyError::KeyInvalid));
        assert_eq!(check_key("a\0b"), Err(LobbyError::KeyInvalid));
        assert_eq!(check_game_key("regia.state"), Err(LobbyError::ReservedKey));
        assert!(check_value(&"v".repeat(8192)).is_ok());
        assert_eq!(check_value(&"v".repeat(8193)), Err(LobbyError::ValueTooLong));
        assert!(check_value("").is_ok(), "an empty value is allowed");
        assert_eq!(check_chat(""), Err(LobbyError::ChatInvalid));
        assert_eq!(check_chat(&"c".repeat(4097)), Err(LobbyError::ChatInvalid));
        let mut spec = LobbySpec {
            max_members: 0,
            ..LobbySpec::default()
        };
        assert_eq!(check_spec(&spec), Err(LobbyError::MembersInvalid));
        spec.max_members = 251;
        assert_eq!(check_spec(&spec), Err(LobbyError::MembersInvalid));
        spec.max_members = 250;
        assert!(check_spec(&spec).is_ok());
        spec.data = (0..65).map(|i| (format!("k{i}"), String::new())).collect();
        assert_eq!(check_spec(&spec), Err(LobbyError::TooManyKeys));
    }

    #[test]
    fn server_key_round_trips_and_rejects_garbage() {
        let a = LobbyServer::Address("10.0.0.2:7777".parse().expect("addr"));
        assert_eq!(a.to_key(), "udp:10.0.0.2:7777");
        assert_eq!(LobbyServer::from_key(&a.to_key()), Some(a));
        let s = LobbyServer::Steam(76_561_190_000_000_000);
        assert_eq!(LobbyServer::from_key(&s.to_key()), Some(s));
        assert_eq!(LobbyServer::from_key("steam:0"), None);
        assert_eq!(LobbyServer::from_key("udp:nope"), None);
        assert_eq!(LobbyServer::from_key("tcp:1.2.3.4:5"), None);
    }

    #[test]
    fn codes_are_stable_lowercase() {
        let all = [
            LobbyError::Unavailable,
            LobbyError::NotInLobby,
            LobbyError::AlreadyInLobby,
            LobbyError::NotOwner,
            LobbyError::Full,
            LobbyError::NotFound,
            LobbyError::KeyInvalid,
            LobbyError::ValueTooLong,
            LobbyError::ReservedKey,
            LobbyError::TooManyKeys,
            LobbyError::ChatInvalid,
            LobbyError::MembersInvalid,
            LobbyError::QueryInvalid(String::new()),
            LobbyError::Platform(String::new()),
            LobbyError::NoIdentity,
        ];
        for e in all {
            assert!(
                e.code()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                e.code()
            );
        }
        for v in [
            LobbyVisibility::Public,
            LobbyVisibility::FriendsOnly,
            LobbyVisibility::Private,
            LobbyVisibility::Invisible,
        ] {
            assert_eq!(LobbyVisibility::from_name(v.name()), Some(v));
        }
    }

    #[test]
    fn engine_keys_carry_the_manifest_hashes() {
        let k = engine_keys("example.pong", 0xabc, 1);
        assert_eq!(k[keys::PROTOCOL], "0000000000000abc");
        assert_eq!(k[keys::CONTENT], "0000000000000001");
        assert_eq!(k[keys::GAME], "example.pong");
        assert_eq!(k[keys::STATE], "open");
        assert_eq!(k[keys::VERSION], "2", "layout v2 since ADR-8897");
    }
}
