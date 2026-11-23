// SPDX-License-Identifier: MPL-2.0
//! A self-hosted lobby hub (NL-04, ADR-8559): presence, a lobby directory and invites, over
//! Regia's own endpoints and Noise, with no platform account.
//!
//! | Piece | What |
//! |---|---|
//! | [`HubServer`] | The pure, bounded state core: sessions, presence, the directory with generations and deltas, invites with a TTL, visibility, chat with rate limits. No socket, no clock: every call takes `now_ms`. |
//! | [`HubHost`] | Runs a [`HubServer`] behind a [`crate::NetServer`] that always seals with the hub's [`crate::ServerIdentity`]. |
//! | [`HubLobbyBackend`] | The client: a [`crate::lobby::LobbyBackend`] over a [`crate::NetClient`] pinned to the hub's [`crate::ServerKey`]. An unpinned or mismatched key is refused. |
//! | [`HubRequest`], [`HubEvent`] | The messages, carried as the `regia.lobby_hub` game on the reliable channel; bounded decode. |
//! | [`join`] | Join brokering (NL-05, ADR-8607): the owner's [`ReachOffer`], the member's ask, the owner's approval, and a [`JoinPlan`] in the order mapped, punch, relay. |
//! | `HubReach` (feature `ecs`) | Runs a member's [`JoinPlan`] on its game socket: the mapped address, or `server_host::JoinPath` (punch, then the relay). |
//! | [`HubFile`] | `hub.toml` for the `regia-lobby-hub` process (NL-06, ADR-8648): port, key path, caps, and per-address and per-identity rate limits. |
//!
//! A session's identity is a player key ([`crate::crypto::PlayerKey`]): the client's static X25519
//! key, proven inside the sealed hub link by a proof bound to that link's Noise handshake hash
//! ([`crate::crypto::PlayerIdentity::prove`]). A `Hello` whose proof does not match the transport it
//! came on, and a `Welcome` that names another key than the client's, are refused with
//! `hub_identity_mismatch`.
//!
//! Rules the hub keeps:
//! - A hub session is allocated only after the client's `Hello`, which can only arrive over a
//!   connection whose handshake (cookie and Noise) is complete.
//! - A game session's address exists on the hub only after the lobby owner confirms it
//!   ([`crate::lobby::LobbyBackend::set_server`]), and goes to the lobby's members only. Directory
//!   cards, presence rows and invite cards never carry it.
//! - An invite card carries a lobby id and an invite token, never a host address.
//! - Nothing is broadcast on a timer: a generation moves only when a row changes, and a session that
//!   already has a generation is sent nothing.
//!
//! The behavior (operations, caps and their defaults, presence states, open, invite and friends
//! visibility, generations) follows DO27's `ir-hub` as requirements only; no line is copied, and its
//! crypto and C ABI are not used (`docs/do27-port-review.md` § 5).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{LobbyError, LobbyId, LobbyServer, LobbyVisibility, MemberId};

mod client;
mod config;
mod host;
pub mod join;
mod limits;
mod process;
#[cfg(feature = "ecs")]
mod reach;
mod server;
mod soak;
mod wire;

pub use client::{HubClientConfig, HubClock, HubLinkState, HubLobbyBackend, ReachAsk};
pub use config::{
    DEFAULT_HUB_PORT, DEFAULT_HUB_TOML, DEFAULT_PER_ADDRESS_PER_SEC, DEFAULT_PER_IDENTITY_PER_SEC,
    DEFAULT_STATUS_PORT, HUB_TOML_FORMAT_VERSION, HUB_TOML_KEYS, HubCaps, HubFieldError, HubFile, HubLimits,
    HubStatusFile, default_key_path, hub_toml_migration_sources, hub_toml_schema_fingerprint,
    key_path_is_inside_project, validate_key_path,
};
pub use host::{HubHost, PROOF_FAILURE_LIMIT};
pub use join::{BrokerError, JoinPlan, JoinRow, NatClass, ReachKind, ReachOffer, broker_reach};
pub use limits::AdmitGate;
pub use process::{HUB_STATUS_LABEL, HubProcess};
#[cfg(feature = "ecs")]
pub use reach::{HubReach, ReachSocket, ReachState};
pub use server::HubServer;
pub use soak::{
    SOAK_ALLOC_BOUND, SOAK_CYCLES, SOAK_PRESENCES, SOAK_SIMULATED_MS, SOAK_STATE_BOUND, SoakReport,
    run_hub_soak_1000,
};
pub use wire::{
    ChatScope, DirectoryPage, HUB_GAME_ID, HUB_WIRE_VERSION, HubEvent, HubMessage, HubRequest, HubWire,
    MAX_CODE_BYTES, MAX_HUB_MESSAGE_BYTES, MAX_PAGE_LOBBIES, MAX_PAGE_ROWS, RoomState, sample_messages,
};

/// Longest handle the wire carries, whatever the settings say (the handshake's name limit).
pub const HANDLE_LIMIT_BYTES: usize = 32;
/// Most friends one `SetFriends` carries, whatever the settings say.
pub const FRIENDS_LIMIT: usize = 256;
/// Longest chat line in characters, whatever the settings say (4 bytes each fit
/// [`crate::lobby::MAX_CHAT_BYTES`]).
pub const CHAT_LIMIT_CHARS: usize = 1024;
/// Most bytes of lobby data (keys plus values), whatever the settings say.
pub const LOBBY_DATA_LIMIT_BYTES: usize = 32 * 1024;
/// Most bytes of one member's data (keys plus values), whatever the settings say. With
/// [`LOBBY_DATA_LIMIT_BYTES`] and 250 members a room snapshot stays inside one reliable message.
pub const MEMBER_DATA_LIMIT_BYTES: usize = 256;

/// The hub's caps and timeouts (spec 89 § 11). Every DO27 cap is a setting whose default is
/// DO27's value; [`HubSettings::sanitized`] clamps each into what the wire can carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HubSettings {
    /// Longest handle in bytes (DO27 `HANDLE_MAX` 24), `1..=32`.
    pub handle_max_bytes: usize,
    /// Longest chat line in characters (DO27 `CHAT_MAX_CHARS` 160), `1..=1024`.
    pub chat_max_chars: usize,
    /// Chat lines one session may send per second, per scope (DO27 `CHAT_PER_SEC` 4).
    pub chat_per_sec: usize,
    /// Chat lines kept per lobby and for the global room (DO27 `CHAT_HISTORY` 100).
    pub chat_history: usize,
    /// Most lobbies the hub holds (DO27 `GAME_CAP` 256).
    pub lobby_cap: usize,
    /// Most sessions the hub holds (DO27 `SESSION_CAP` 1024).
    pub session_cap: usize,
    /// Most friends one session lists (DO27 `FRIEND_CAP` 64), at most 256.
    pub friend_cap: usize,
    /// Silence after which a session expires, in milliseconds (DO27 `IDLE_MS` 30 000).
    pub idle_ms: u64,
    /// How long an invite is good for, in milliseconds (DO27 `INVITE_TTL` 2 h).
    pub invite_ttl_ms: u64,
    /// Most open invites the hub holds; the oldest goes first.
    pub invite_cap: usize,
    /// Directory and presence changes kept for deltas (DO27 128); an older cursor gets a snapshot.
    pub changelog: usize,
    /// Largest member limit a lobby may ask for, `1..=250`.
    pub max_members: u32,
    /// Most bytes of lobby data (keys plus values), at most 32 KiB.
    pub lobby_data_bytes: usize,
    /// Most bytes of one member's data (keys plus values), at most 256.
    pub member_data_bytes: usize,
    /// Most events waiting for one session between drains. Events past it are dropped and the
    /// session is resynchronized when it drains (its room, the directory, its invites); a session
    /// that overflows and cannot drain (it never reads, or its link is backed up) is dropped as a
    /// slow reader at the next [`HubServer::expire`] (ADR-8633).
    pub outbox_cap: usize,
    /// Lobby-data, member-data, session-address and joinable writes one session may make per second
    /// (ADR-8633; default 20, at least 1).
    pub data_per_sec: usize,
    /// Those writes one lobby takes per second from all its members together (ADR-8633; default
    /// 160, at least 1). Bounds what a lobby's members can queue for each other.
    pub lobby_data_per_sec: usize,
    /// Global chat lines the whole hub takes per second (ADR-8633; default 16, at least 1): every
    /// session receives every global line.
    pub global_chat_per_sec: usize,
    /// How often one session may move its directory cursor backwards (a `List` with a generation
    /// behind what it was sent), in milliseconds (ADR-8633; default 10 000). A rewind inside the
    /// window is ignored, so it cannot ask for a snapshot rebuild more often than this.
    pub list_rewind_ms: u64,
    /// Reliable messages [`HubHost`] sends one peer per second (ADR-8633; default 1024,
    /// `64..=65536`). What the link refuses stays in the session's outbox for the next update.
    pub reliable_msgs_per_s: u32,
}

impl Default for HubSettings {
    fn default() -> Self {
        Self {
            handle_max_bytes: 24,
            chat_max_chars: 160,
            chat_per_sec: 4,
            chat_history: 100,
            lobby_cap: 256,
            session_cap: 1024,
            friend_cap: 64,
            idle_ms: 30_000,
            invite_ttl_ms: 2 * 60 * 60 * 1000,
            invite_cap: 4096,
            changelog: 128,
            max_members: 64,
            lobby_data_bytes: 8 * 1024,
            member_data_bytes: 256,
            outbox_cap: 1024,
            data_per_sec: 20,
            lobby_data_per_sec: 160,
            global_chat_per_sec: 16,
            list_rewind_ms: 10_000,
            reliable_msgs_per_s: 1024,
        }
    }
}

impl HubSettings {
    /// The settings with every value clamped into what the wire and the core can carry.
    pub fn sanitized(&self) -> Self {
        Self {
            handle_max_bytes: self.handle_max_bytes.clamp(1, HANDLE_LIMIT_BYTES),
            chat_max_chars: self.chat_max_chars.clamp(1, CHAT_LIMIT_CHARS),
            chat_per_sec: self.chat_per_sec.max(1),
            chat_history: self.chat_history.clamp(1, 10_000),
            lobby_cap: self.lobby_cap.clamp(1, 1 << 16),
            session_cap: self.session_cap.clamp(1, 1 << 20),
            friend_cap: self.friend_cap.min(FRIENDS_LIMIT),
            idle_ms: self.idle_ms.max(1),
            invite_ttl_ms: self.invite_ttl_ms.max(1),
            invite_cap: self.invite_cap.clamp(1, 1 << 20),
            changelog: self.changelog.clamp(1, 1 << 16),
            max_members: self.max_members.clamp(1, super::MAX_LOBBY_MEMBERS),
            lobby_data_bytes: self.lobby_data_bytes.min(LOBBY_DATA_LIMIT_BYTES),
            member_data_bytes: self.member_data_bytes.min(MEMBER_DATA_LIMIT_BYTES),
            outbox_cap: self.outbox_cap.clamp(16, 1 << 16),
            data_per_sec: self.data_per_sec.max(1),
            lobby_data_per_sec: self.lobby_data_per_sec.max(1),
            global_chat_per_sec: self.global_chat_per_sec.max(1),
            list_rewind_ms: self.list_rewind_ms,
            reliable_msgs_per_s: self.reliable_msgs_per_s.clamp(64, 1 << 16),
        }
    }
}

/// What a session is doing, as the directory shows it (DO27's menu, hosting and match states).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HubPresence {
    /// Online, in no lobby.
    #[default]
    Menu,
    /// In a lobby, before its session is confirmed.
    Lobby,
    /// In a lobby whose game session the owner confirmed.
    InGame,
}

impl HubPresence {
    /// The stable name (`menu`, `lobby`, `in_game`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Menu => "menu",
            Self::Lobby => "lobby",
            Self::InGame => "in_game",
        }
    }

    /// The wire value.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Menu => 0,
            Self::Lobby => 1,
            Self::InGame => 2,
        }
    }

    /// Parses a wire value.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Menu,
            1 => Self::Lobby,
            2 => Self::InGame,
            _ => return None,
        })
    }
}

/// One session as the directory shows it. No address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceRow {
    /// The session's member id (hub-assigned, never reused while the hub runs).
    pub member: MemberId,
    /// The handle.
    pub handle: String,
    /// What it is doing.
    pub presence: HubPresence,
    /// The lobby it is in, when that lobby is public; `None` otherwise.
    pub lobby: Option<LobbyId>,
}

/// One listed lobby as the directory shows it: never the session address (`regia.server` is
/// dropped from its data).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubLobbyCard {
    /// The lobby.
    pub id: LobbyId,
    /// Its owner.
    pub owner: MemberId,
    /// The owner's handle.
    pub owner_handle: String,
    /// Who can find and join it (listed cards are public).
    pub visibility: LobbyVisibility,
    /// Member limit.
    pub max_members: u32,
    /// Members in it.
    pub member_count: u32,
    /// Whether others may join.
    pub joinable: bool,
    /// Lobby data without `regia.server`.
    pub data: BTreeMap<String, String>,
}

impl HubLobbyCard {
    /// The card as a listing's [`crate::lobby::LobbyInfo`] (no members, no server).
    pub fn to_info(&self) -> super::LobbyInfo {
        super::LobbyInfo {
            id: self.id,
            owner: self.owner,
            visibility: self.visibility,
            max_members: self.max_members,
            joinable: self.joinable,
            members: Vec::new(),
            member_count: self.member_count,
            data: self.data.clone(),
            server: None,
        }
    }
}

/// An invite token: 128 bits the hub minted for one invitee and one lobby.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InviteToken(pub [u8; 16]);

impl std::fmt::Debug for InviteToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A token is a capability: logs show its first two bytes only.
        write!(f, "InviteToken({:02x}{:02x}..)", self.0[0], self.0[1])
    }
}

/// What an invitee receives: a lobby id and a token, never a host address. The session address
/// reaches the invitee only after the join is approved (the invitee is then a member).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCard {
    /// The lobby to join.
    pub lobby: LobbyId,
    /// Who invited.
    pub from: MemberId,
    /// The inviter's handle.
    pub from_handle: String,
    /// The token the join presents.
    pub token: InviteToken,
    /// When the invite stops being good, on the hub's clock in milliseconds.
    pub expires_ms: u64,
}

impl InviteCard {
    /// The card's wire bytes (what [`HubEvent::Invite`] carries after its tag).
    pub fn to_bytes(&self) -> Result<Vec<u8>, crate::WireError> {
        let mut w = crate::WireWriter::new();
        wire::encode_card(&mut w, self);
        w.finish_checked()
    }
}

/// What the hub reports to the agent protocol (`lobby.hub_status`) and logs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubReport {
    /// Sessions.
    pub sessions: usize,
    /// Lobbies, listed or not.
    pub lobbies: usize,
    /// Listed (public) lobbies.
    pub listed: usize,
    /// Open invites.
    pub invites: usize,
    /// The directory generation.
    pub directory_generation: u64,
    /// The presence generation.
    pub presence_generation: u64,
    /// The core's estimated memory, in bytes.
    pub state_bytes: usize,
    /// Directory bytes sent (snapshots and deltas).
    pub directory_bytes: u64,
    /// Chat bytes sent.
    pub chat_bytes: u64,
    /// Every other event's bytes.
    pub other_bytes: u64,
    /// Sessions expired (idle or slow readers).
    pub expired: u64,
    /// Refused operations by code.
    pub refused: BTreeMap<String, u64>,
    /// Presence rows, at most 64 (member order).
    pub players: Vec<PresenceRow>,
    /// Listed lobby cards, at most 64 (id order).
    pub games: Vec<HubLobbyCard>,
    /// Brokered joins, at most 256 (lobby, then member order): `lobby.hub_join_status`.
    #[serde(default)]
    pub joins: Vec<JoinRow>,
}

/// Why a hub call failed. [`HubError::code`] is the stable lowercase code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HubError {
    /// The client was given no hub key to pin.
    #[error("the hub's server key is not pinned")]
    KeyUnpinned,
    /// The hub answered under another key than the pinned one.
    #[error("the hub's key does not match the pinned key")]
    KeyMismatch,
    /// A handle is empty, too long or holds a control character.
    #[error("the handle is empty, too long or holds a control character")]
    HandleInvalid,
    /// Another session has this handle.
    #[error("the handle is taken")]
    HandleTaken,
    /// The hub is at its session cap.
    #[error("the hub is full")]
    SessionCap,
    /// No such session (expired, or never said hello).
    #[error("no hub session")]
    NoSession,
    /// The hub is at its lobby cap.
    #[error("the hub holds its most lobbies")]
    LobbyCap,
    /// A presence that does not match the session's lobby.
    #[error("that presence does not match the session's lobby")]
    PresenceInvalid,
    /// More friends than the cap.
    #[error("too many friends")]
    TooManyFriends,
    /// The lobby is private and there is no good invite.
    #[error("the lobby is invite only")]
    InviteOnly,
    /// The lobby is friends only, and the joiner has no player key the owner lists or an invite
    /// was bound to (a session without a proven player key is always refused: fail closed).
    #[error("the lobby is friends only")]
    FriendsOnly,
    /// No session has that handle.
    #[error("no such player")]
    NoSuchPlayer,
    /// A chat line over the rate limit.
    #[error("chat rate limited")]
    ChatRateLimited,
    /// A lobby-data, member-data, session-address or joinable write over
    /// [`HubSettings::data_per_sec`] or [`HubSettings::lobby_data_per_sec`].
    #[error("lobby data writes rate limited")]
    DataRateLimited,
    /// A chat line is empty or too long.
    #[error("chat line is empty or too long")]
    ChatInvalid,
    /// The lobby owner muted this member.
    #[error("muted in this lobby")]
    Muted,
    /// The owner cannot be muted.
    #[error("the lobby owner cannot be muted")]
    CannotMuteOwner,
    /// Lobby or member data over its byte budget.
    #[error("lobby or member data over its byte budget")]
    DataTooLarge,
    /// The link to the hub is down.
    #[error("not connected to the hub")]
    Disconnected,
    /// A `Hello` whose player proof does not match the sealed link it came on, or a `Welcome`
    /// that names another player key than the client's.
    #[error("the player identity does not match the transport")]
    IdentityMismatch,
    /// This player key already holds a session under another handle.
    #[error("this player key already holds another handle")]
    IdentityInUse,
    /// The owner offered nothing a member could reach (no mapping, candidate or relay).
    #[error("the lobby owner offered no reach")]
    NoReach,
    /// A reach offer with too many candidates, a bad join code or an over-long ticket.
    #[error("the reach offer is invalid")]
    ReachInvalid,
    /// The owner approved a member that has no open ask.
    #[error("that member has no open ask")]
    NoAsk,
    /// The owner denied the ask.
    #[error("the lobby owner denied the join")]
    ReachDenied,
    /// Too many requests from one address in one second.
    #[error("too many requests from this address")]
    AddressRateLimited,
    /// Too many requests from one player key in one second.
    #[error("too many requests from this player")]
    IdentityRateLimited,
    /// The hub is draining and takes no new session.
    #[error("the hub is shutting down")]
    Draining,
    /// A lobby rule (spec 89 codes).
    #[error(transparent)]
    Lobby(#[from] LobbyError),
}

impl HubError {
    /// The stable code: `hub_*`, or the [`LobbyError`] code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyUnpinned => "hub_key_unpinned",
            Self::KeyMismatch => "hub_key_mismatch",
            Self::HandleInvalid => "hub_handle_invalid",
            Self::HandleTaken => "hub_handle_taken",
            Self::SessionCap => "hub_session_cap",
            Self::NoSession => "hub_no_session",
            Self::LobbyCap => "hub_lobby_cap",
            Self::PresenceInvalid => "hub_presence_invalid",
            Self::TooManyFriends => "hub_too_many_friends",
            Self::InviteOnly => "hub_invite_only",
            Self::FriendsOnly => "hub_friends_only",
            Self::NoSuchPlayer => "hub_no_such_player",
            Self::ChatRateLimited => "hub_chat_rate_limited",
            Self::DataRateLimited => "hub_data_rate_limited",
            Self::ChatInvalid => "hub_chat_invalid",
            Self::Muted => "hub_muted",
            Self::CannotMuteOwner => "hub_cannot_mute_owner",
            Self::DataTooLarge => "hub_data_too_large",
            Self::Disconnected => "hub_disconnected",
            Self::IdentityMismatch => "hub_identity_mismatch",
            Self::IdentityInUse => "hub_identity_in_use",
            Self::NoReach => "hub_no_reach",
            Self::ReachInvalid => "hub_reach_invalid",
            Self::NoAsk => "hub_no_ask",
            Self::ReachDenied => "hub_reach_denied",
            Self::AddressRateLimited => "hub_address_rate_limited",
            Self::IdentityRateLimited => "hub_identity_rate_limited",
            Self::Draining => "hub_draining",
            Self::Lobby(e) => e.code(),
        }
    }

    /// Every `hub_*` code, for the book and the codes test.
    pub const HUB_CODES: &'static [&'static str] = &[
        "hub_key_unpinned",
        "hub_key_mismatch",
        "hub_handle_invalid",
        "hub_handle_taken",
        "hub_session_cap",
        "hub_no_session",
        "hub_lobby_cap",
        "hub_presence_invalid",
        "hub_too_many_friends",
        "hub_invite_only",
        "hub_friends_only",
        "hub_no_such_player",
        "hub_chat_rate_limited",
        "hub_data_rate_limited",
        "hub_chat_invalid",
        "hub_muted",
        "hub_cannot_mute_owner",
        "hub_data_too_large",
        "hub_disconnected",
        "hub_identity_mismatch",
        "hub_identity_in_use",
        "hub_no_reach",
        "hub_reach_invalid",
        "hub_no_ask",
        "hub_reach_denied",
        "hub_address_rate_limited",
        "hub_identity_rate_limited",
        "hub_draining",
    ];

    /// The error for a code the hub sent: a `hub_*` code, or a [`LobbyError`] code. Unknown codes
    /// become [`LobbyError::Platform`].
    pub fn from_code(code: &str) -> Self {
        match code {
            "hub_key_unpinned" => Self::KeyUnpinned,
            "hub_key_mismatch" => Self::KeyMismatch,
            "hub_handle_invalid" => Self::HandleInvalid,
            "hub_handle_taken" => Self::HandleTaken,
            "hub_session_cap" => Self::SessionCap,
            "hub_no_session" => Self::NoSession,
            "hub_lobby_cap" => Self::LobbyCap,
            "hub_presence_invalid" => Self::PresenceInvalid,
            "hub_too_many_friends" => Self::TooManyFriends,
            "hub_invite_only" => Self::InviteOnly,
            "hub_friends_only" => Self::FriendsOnly,
            "hub_no_such_player" => Self::NoSuchPlayer,
            "hub_chat_rate_limited" => Self::ChatRateLimited,
            "hub_data_rate_limited" => Self::DataRateLimited,
            "hub_chat_invalid" => Self::ChatInvalid,
            "hub_muted" => Self::Muted,
            "hub_cannot_mute_owner" => Self::CannotMuteOwner,
            "hub_data_too_large" => Self::DataTooLarge,
            "hub_disconnected" => Self::Disconnected,
            "hub_identity_mismatch" => Self::IdentityMismatch,
            "hub_identity_in_use" => Self::IdentityInUse,
            "hub_no_reach" => Self::NoReach,
            "hub_reach_invalid" => Self::ReachInvalid,
            "hub_no_ask" => Self::NoAsk,
            "hub_reach_denied" => Self::ReachDenied,
            "hub_address_rate_limited" => Self::AddressRateLimited,
            "hub_identity_rate_limited" => Self::IdentityRateLimited,
            "hub_draining" => Self::Draining,
            other => Self::Lobby(lobby_error_from_code(other)),
        }
    }

    /// The error as the [`crate::lobby::LobbyBackend`] reports it: lobby rules as themselves, hub
    /// rules as [`LobbyError::Platform`] carrying the hub code.
    pub fn to_lobby(&self) -> LobbyError {
        match self {
            Self::Lobby(e) => e.clone(),
            Self::Disconnected | Self::KeyMismatch | Self::KeyUnpinned | Self::IdentityMismatch => {
                LobbyError::Unavailable
            }
            other => LobbyError::Platform(other.code().to_owned()),
        }
    }
}

fn lobby_error_from_code(code: &str) -> LobbyError {
    match code {
        "lobby_unavailable" => LobbyError::Unavailable,
        "not_in_lobby" => LobbyError::NotInLobby,
        "already_in_lobby" => LobbyError::AlreadyInLobby,
        "not_owner" => LobbyError::NotOwner,
        "lobby_full" => LobbyError::Full,
        "lobby_not_found" => LobbyError::NotFound,
        "key_invalid" => LobbyError::KeyInvalid,
        "value_too_long" => LobbyError::ValueTooLong,
        "reserved_key" => LobbyError::ReservedKey,
        "too_many_keys" => LobbyError::TooManyKeys,
        "chat_invalid" => LobbyError::ChatInvalid,
        "members_invalid" => LobbyError::MembersInvalid,
        "query_invalid" => LobbyError::QueryInvalid(String::new()),
        other => LobbyError::Platform(other.to_owned()),
    }
}

/// Folds a handle for comparison: trimmed, lowercase (ASCII and Unicode simple case).
pub(crate) fn fold(handle: &str) -> String {
    handle.trim().to_lowercase()
}

/// Checks a handle against `max_bytes`; returns it trimmed.
pub(crate) fn clean_handle(handle: &str, max_bytes: usize) -> Result<String, HubError> {
    let handle = handle.trim();
    if handle.is_empty() || handle.len() > max_bytes || handle.chars().any(char::is_control) {
        return Err(HubError::HandleInvalid);
    }
    Ok(handle.to_owned())
}

/// Drops `regia.server` from lobby data (what a directory card may show).
pub(crate) fn public_data(data: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    data.iter()
        .filter(|(k, _)| k.as_str() != super::keys::SERVER)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// A server address cannot ride in a listed lobby's data: [`LobbyServer`] lives beside the data.
pub(crate) fn server_of(data: &BTreeMap<String, String>) -> Option<LobbyServer> {
    data.get(super::keys::SERVER)
        .and_then(|s| LobbyServer::from_key(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_lowercase_and_round_trip() {
        for code in HubError::HUB_CODES {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
            assert_eq!(HubError::from_code(code).code(), *code);
        }
        assert_eq!(
            HubError::from_code("lobby_full"),
            HubError::Lobby(LobbyError::Full)
        );
        assert_eq!(
            HubError::InviteOnly.to_lobby(),
            LobbyError::Platform("hub_invite_only".into())
        );
    }

    #[test]
    fn settings_default_to_the_do27_caps_and_clamp() {
        let s = HubSettings::default();
        assert_eq!(
            (
                s.handle_max_bytes,
                s.chat_max_chars,
                s.chat_per_sec,
                s.chat_history
            ),
            (24, 160, 4, 100)
        );
        assert_eq!((s.lobby_cap, s.session_cap, s.friend_cap), (256, 1024, 64));
        assert_eq!((s.idle_ms, s.invite_ttl_ms), (30_000, 7_200_000));
        assert_eq!(s.sanitized(), s);
        let wild = HubSettings {
            handle_max_bytes: 0,
            friend_cap: 10_000,
            max_members: 9_999,
            member_data_bytes: 1 << 20,
            ..HubSettings::default()
        }
        .sanitized();
        assert_eq!(wild.handle_max_bytes, 1);
        assert_eq!(wild.friend_cap, FRIENDS_LIMIT);
        assert_eq!(wild.max_members, crate::lobby::MAX_LOBBY_MEMBERS);
        assert_eq!(wild.member_data_bytes, MEMBER_DATA_LIMIT_BYTES);
    }

    #[test]
    fn handles_are_trimmed_bounded_and_folded() {
        assert_eq!(clean_handle("  ann ", 24).as_deref(), Ok("ann"));
        assert_eq!(clean_handle("", 24), Err(HubError::HandleInvalid));
        assert_eq!(clean_handle("a\u{7}", 24), Err(HubError::HandleInvalid));
        assert_eq!(clean_handle(&"x".repeat(25), 24), Err(HubError::HandleInvalid));
        assert_eq!(fold(" Ann "), fold("aNN"));
    }
}
