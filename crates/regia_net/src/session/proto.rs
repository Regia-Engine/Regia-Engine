// SPDX-License-Identifier: MPL-2.0
//! Session messages on the wire (spec 77 § 4, § 6, § 7, § 8; spec 76 § 7).
//!
//! Every session message is one transport message. A `Hello` starts with a fixed 16-byte prefix that
//! every protocol version keeps (`b"RGNH"`, `protocol_version: u32` LE, `protocol_hash: u64` LE), so
//! a server can answer `version_mismatch` or `protocol_mismatch` to a client whose later fields it
//! cannot decode. Every other message starts with a one-byte tag (never `b'R'`).

use crate::model::{MAX_LOCAL_PLAYERS, NetId, NetPlayerId, PeerId};
use crate::session::manifest::ContentManifest;
use crate::wire::{WireError, WireReader, WireWriter};

/// Lines of one `AdminOutput`; encoding cuts longer output, decoding refuses it.
pub const MAX_ADMIN_OUTPUT_LINES: usize = 1024;
/// The first four bytes of every `Hello`.
pub const HELLO_MAGIC: [u8; 4] = *b"RGNH";
/// Bytes of the `Hello` prefix every protocol version keeps.
pub const HELLO_PREFIX_BYTES: usize = 16;
/// Longest player name, in characters after trimming.
pub const MAX_PLAYER_NAME_CHARS: usize = 32;
/// Longest detail text carried by a reason.
pub const MAX_DETAIL_BYTES: usize = 4096;
/// Bytes of a reconnect token.
pub const RECONNECT_TOKEN_BYTES: usize = 16;
/// Largest opaque `auth` blob.
pub const MAX_AUTH_BYTES: usize = 4096;
/// Largest stream chunk on the wire.
pub const MAX_STREAM_CHUNK_BYTES: usize = 16 * 1024;
/// Largest game payload or join-record blob carried by one session message.
pub const MAX_SESSION_PAYLOAD_BYTES: usize = 60 * 1024;
/// Most persistent `NetId`s one `TravelBegin` carries; a server that keeps more sends no list and
/// its clients wait for the despawn records instead (ADR-7259).
pub const MAX_TRAVEL_KEEP_IDS: usize = 8192;

/// Revision of the session message layouts, mixed into the protocol hash (`runtime::protocol_hash`)
/// so builds whose session messages differ refuse each other at the `Hello` prefix with
/// `protocol_mismatch` instead of failing to decode mid-session. 1 is the layout before ADR-7259 (a
/// build with revision 1 has the protocol hashes of the builds before it); 2 adds the
/// [`TravelKeep`] of `TravelBegin`.
pub const SESSION_WIRE_REVISION: u64 = 2;

/// The replicated copies a client keeps at `TravelBegin` (spec 77 § 6.2 step 3, ADR-7259): every
/// copy whose `NetId` is below `below` (an entity of the old level) and not in `ids` goes at once;
/// copies of entities spawned since the travel (`NetId`s at or above `below`, which the server never
/// reuses) and the persistent ones stay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TravelKeep {
    /// The first `NetId` the server had not assigned when it despawned the old level.
    pub below: NetId,
    /// The `NetId`s of the `NetPersistAcrossTravel` entities the server kept, sorted, at most
    /// [`MAX_TRAVEL_KEEP_IDS`].
    pub ids: Vec<NetId>,
}

/// Why a connection ended (spec 77 § 7.1). Codes are stable: they are agent error codes and
/// analytics keys. Variants with a detail carry it as a `String`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// The player left.
    ClientQuit,
    /// The server is draining.
    ServerShuttingDown,
    /// No packets within the timeout of the current state.
    Timeout,
    /// An admin or a game rule kicked the player (reason text).
    Kicked(String),
    /// A ban entry matched (reason and expiry).
    Banned(String),
    /// No free player slots.
    ServerFull,
    /// More than 8 local players declared.
    TooManyLocalPlayers,
    /// Protocol versions differ (both versions).
    VersionMismatch(String),
    /// Protocol hashes differ (both hashes).
    ProtocolMismatch(String),
    /// Content manifests differ (up to 32 items, one per line).
    ContentMismatch(String),
    /// Wrong password.
    BadPassword,
    /// The authenticator refused or timed out (its text).
    AuthFailed(String),
    /// A to-server message failed validation with `Kick` (the reason).
    InvalidMessage(String),
    /// Bytes that did not decode.
    MalformedPacket,
    /// The peer sent faster than its flood limits.
    Flood,
    /// The reliable queue bound was hit.
    ReliableOverflow,
    /// The level could not be loaded, or loading took too long.
    LevelLoadFailed(String),
    /// The transport reported a failure (its text).
    TransportError(String),
    /// The app was suspended by the OS; enters the reconnect grace.
    Suspended,
}

/// Every code of [`DisconnectReason`], in declaration order.
pub const DISCONNECT_CODES: &[&str] = &[
    "client_quit",
    "server_shutting_down",
    "timeout",
    "kicked",
    "banned",
    "server_full",
    "too_many_local_players",
    "version_mismatch",
    "protocol_mismatch",
    "content_mismatch",
    "bad_password",
    "auth_failed",
    "invalid_message",
    "malformed_packet",
    "flood",
    "reliable_overflow",
    "level_load_failed",
    "transport_error",
    "suspended",
];

impl DisconnectReason {
    /// The stable lowercase code.
    pub fn code(&self) -> &'static str {
        DISCONNECT_CODES[usize::from(self.index())]
    }

    fn index(&self) -> u8 {
        match self {
            Self::ClientQuit => 0,
            Self::ServerShuttingDown => 1,
            Self::Timeout => 2,
            Self::Kicked(_) => 3,
            Self::Banned(_) => 4,
            Self::ServerFull => 5,
            Self::TooManyLocalPlayers => 6,
            Self::VersionMismatch(_) => 7,
            Self::ProtocolMismatch(_) => 8,
            Self::ContentMismatch(_) => 9,
            Self::BadPassword => 10,
            Self::AuthFailed(_) => 11,
            Self::InvalidMessage(_) => 12,
            Self::MalformedPacket => 13,
            Self::Flood => 14,
            Self::ReliableOverflow => 15,
            Self::LevelLoadFailed(_) => 16,
            Self::TransportError(_) => 17,
            Self::Suspended => 18,
        }
    }

    /// The detail text, for variants that carry one.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Kicked(d)
            | Self::Banned(d)
            | Self::VersionMismatch(d)
            | Self::ProtocolMismatch(d)
            | Self::ContentMismatch(d)
            | Self::AuthFailed(d)
            | Self::InvalidMessage(d)
            | Self::LevelLoadFailed(d)
            | Self::TransportError(d) => Some(d),
            _ => None,
        }
    }

    /// The reason for a code and detail; `None` for an unknown code.
    pub fn from_code(code: &str, detail: &str) -> Option<Self> {
        let d = detail.to_owned();
        Some(match code {
            "client_quit" => Self::ClientQuit,
            "server_shutting_down" => Self::ServerShuttingDown,
            "timeout" => Self::Timeout,
            "kicked" => Self::Kicked(d),
            "banned" => Self::Banned(d),
            "server_full" => Self::ServerFull,
            "too_many_local_players" => Self::TooManyLocalPlayers,
            "version_mismatch" => Self::VersionMismatch(d),
            "protocol_mismatch" => Self::ProtocolMismatch(d),
            "content_mismatch" => Self::ContentMismatch(d),
            "bad_password" => Self::BadPassword,
            "auth_failed" => Self::AuthFailed(d),
            "invalid_message" => Self::InvalidMessage(d),
            "malformed_packet" => Self::MalformedPacket,
            "flood" => Self::Flood,
            "reliable_overflow" => Self::ReliableOverflow,
            "level_load_failed" => Self::LevelLoadFailed(d),
            "transport_error" => Self::TransportError(d),
            "suspended" => Self::Suspended,
            _ => return None,
        })
    }

    /// True for drops that enter the reconnect grace (spec 77 § 7.4).
    pub fn enters_grace(&self) -> bool {
        matches!(self, Self::Timeout | Self::TransportError(_) | Self::Suspended)
    }

    fn encode(&self, w: &mut WireWriter) {
        w.u8(self.index());
        let detail = self.detail().unwrap_or("");
        let cut = truncate_utf8(detail, MAX_DETAIL_BYTES);
        // Cannot fail: `cut` is at most MAX_DETAIL_BYTES.
        let _ = w.bytes_u16(cut.as_bytes(), MAX_DETAIL_BYTES);
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let index = usize::from(r.u8()?);
        let detail = r.bytes_u16(MAX_DETAIL_BYTES)?;
        let detail = std::str::from_utf8(detail).map_err(|_| WireError::Utf8)?;
        let code = DISCONNECT_CODES
            .get(index)
            .ok_or(WireError::Invalid("disconnect reason"))?;
        Self::from_code(code, detail).ok_or(WireError::Invalid("disconnect reason"))
    }
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.detail() {
            Some(d) if !d.is_empty() => write!(f, "{}: {d}", self.code()),
            _ => f.write_str(self.code()),
        }
    }
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The client's first message (spec 77 § 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// The oldest protocol version the client speaks (`MIN_NET_PROTOCOL_VERSION` of its build). A
    /// server that speaks it takes the join; the wire is then negotiated per connection (ADR-1950).
    pub protocol_version: u32,
    /// The client's protocol hash.
    pub protocol_hash: u64,
    /// The client's content manifest (with its mounted mods).
    pub manifest: ContentManifest,
    /// One name per local player, 1..=8, each 1..=32 characters after trimming.
    pub player_names: Vec<String>,
    /// The password, if the player typed one.
    pub password: Option<String>,
    /// Opaque data for the session authenticator.
    pub auth: Option<Vec<u8>>,
    /// The client's own downstream limit, bytes per second.
    pub client_rate_bytes: u32,
    /// The token of a reconnect-grace slot, if resuming.
    pub reconnect_token: Option<[u8; RECONNECT_TOKEN_BYTES]>,
}

impl Hello {
    /// Encodes the message (prefix first).
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = WireWriter::new();
        w.raw(&HELLO_MAGIC);
        w.u32(self.protocol_version);
        w.u64(self.protocol_hash);
        self.manifest.encode(&mut w)?;
        if self.player_names.len() > 255 {
            return Err(WireError::TooLong {
                len: self.player_names.len(),
                max: 255,
            });
        }
        w.u8(self.player_names.len() as u8);
        for n in &self.player_names {
            w.str_u8(n, 255)?;
        }
        opt_str(&mut w, self.password.as_deref())?;
        match &self.auth {
            Some(a) => {
                w.u8(1);
                w.bytes_u16(a, MAX_AUTH_BYTES)?;
            }
            None => w.u8(0),
        }
        w.u32(self.client_rate_bytes);
        match &self.reconnect_token {
            Some(t) => {
                w.u8(1);
                w.raw(t);
            }
            None => w.u8(0),
        }
        Ok(w.finish())
    }

    /// Reads only the fixed prefix: `(protocol_version, protocol_hash)`. `None` when the bytes are
    /// not a `Hello`.
    pub fn decode_prefix(bytes: &[u8]) -> Option<(u32, u64)> {
        if bytes.len() < HELLO_PREFIX_BYTES || bytes[..4] != HELLO_MAGIC {
            return None;
        }
        let mut r = WireReader::new(&bytes[4..HELLO_PREFIX_BYTES]);
        Some((r.u32().ok()?, r.u64().ok()?))
    }

    /// Decodes the whole message (only when the prefix matched this build).
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let (protocol_version, protocol_hash) =
            Self::decode_prefix(bytes).ok_or(WireError::Invalid("hello prefix"))?;
        let mut r = WireReader::new(&bytes[HELLO_PREFIX_BYTES..]);
        let manifest = ContentManifest::decode(&mut r)?;
        let count = usize::from(r.u8()?);
        let mut player_names = Vec::with_capacity(count);
        for _ in 0..count {
            player_names.push(r.str_u8(255)?.to_owned());
        }
        let password = read_opt_str(&mut r)?;
        let auth = match r.u8()? {
            0 => None,
            1 => Some(r.bytes_u16(MAX_AUTH_BYTES)?.to_vec()),
            _ => return Err(WireError::Invalid("auth flag")),
        };
        let client_rate_bytes = r.u32()?;
        let reconnect_token = match r.u8()? {
            0 => None,
            1 => Some(r.array::<RECONNECT_TOKEN_BYTES>()?),
            _ => return Err(WireError::Invalid("token flag")),
        };
        r.finish()?;
        Ok(Self {
            protocol_version,
            protocol_hash,
            manifest,
            player_names,
            password,
            auth,
            client_rate_bytes,
            reconnect_token,
        })
    }

    /// The names after trimming, or `None` when one is empty or longer than 32 characters.
    pub fn trimmed_names(&self) -> Option<Vec<String>> {
        if self.player_names.is_empty() {
            return None;
        }
        self.player_names
            .iter()
            .map(|n| {
                let t = n.trim();
                let chars = t.chars().count();
                (1..=MAX_PLAYER_NAME_CHARS).contains(&chars).then(|| t.to_owned())
            })
            .collect()
    }

    /// True when the player count is within 1..=8.
    pub fn local_player_count_ok(&self) -> bool {
        (1..=usize::from(MAX_LOCAL_PLAYERS)).contains(&self.player_names.len())
    }
}

fn opt_str(w: &mut WireWriter, s: Option<&str>) -> Result<(), WireError> {
    match s {
        Some(s) => {
            w.u8(1);
            w.str_u8(s, 255)
        }
        None => {
            w.u8(0);
            Ok(())
        }
    }
}

fn read_opt_str(r: &mut WireReader<'_>) -> Result<Option<String>, WireError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(r.str_u8(255)?.to_owned())),
        _ => Err(WireError::Invalid("string flag")),
    }
}

/// The server's answer to an accepted `Hello` (spec 77 § 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    /// The client's session peer id (kept across a reconnect).
    pub peer_id: PeerId,
    /// Random 64-bit session id.
    pub session_id: u64,
    /// The server tick when the welcome was sent.
    pub server_tick: u64,
    /// Simulation rate.
    pub tick_hz: u32,
    /// Send cadence in ticks.
    pub send_every_ticks: u32,
    /// The level to load.
    pub level: String,
    /// The current travel id.
    pub travel_id: u32,
    /// Message of the day.
    pub motd: String,
    /// Server name.
    pub server_name: String,
    /// Token that resumes this slot after an unexpected drop.
    pub reconnect_token: [u8; RECONNECT_TOKEN_BYTES],
    /// The client's players.
    pub players: Vec<NetPlayerId>,
}

/// One session message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMsg {
    /// Client → server: the handshake.
    Hello(Box<Hello>),
    /// Server → client: accepted.
    Welcome(Box<Welcome>),
    /// Server → client: refused at a handshake check.
    Refused(DisconnectReason),
    /// Client → server: the level of this travel is loaded.
    LevelReady {
        /// The travel it answers.
        travel_id: u32,
    },
    /// Server → client, join snapshot item 1: level entities removed since the level loaded.
    JoinRemoved {
        /// Their `EntityGuid`s.
        guids: Vec<u128>,
    },
    /// Server → client, join snapshot item 2: spawn records (opaque to the session layer).
    JoinRecords {
        /// Encoded records.
        bytes: Vec<u8>,
    },
    /// Server → client, join snapshot item 3: the client enters `InGame`.
    InGame {
        /// The tick at which items 1 and 2 were captured.
        tick: u64,
    },
    /// Server → client: the session moves to another level.
    TravelBegin {
        /// Level path.
        level: String,
        /// The new travel id.
        travel_id: u32,
        /// The copies the client keeps; `None` when the server did not know them (a session
        /// core driven without a world): the client then waits for the despawn records.
        keep: Option<TravelKeep>,
    },
    /// Either way: the sender is leaving (sent three times, unreliable).
    Goodbye(DisconnectReason),
    /// Either way: nothing else was sent for a second.
    Keepalive,
    /// Server → client: one of the client's local players was removed.
    PlayerRemoved {
        /// Its local index.
        local: u8,
        /// Why.
        reason: DisconnectReason,
    },
    /// A stream starts (spec 77 § 8).
    StreamBegin {
        /// Stream id (per sender).
        id: u32,
        /// Game-defined kind.
        kind: String,
        /// Total bytes.
        total: u64,
    },
    /// The next chunk of a stream (reliable, in order).
    StreamChunk {
        /// Stream id.
        id: u32,
        /// The bytes.
        bytes: Vec<u8>,
    },
    /// Client → server: remote admin login (spec 76 § 7).
    AdminLogin {
        /// The token.
        token: String,
    },
    /// Client → server: one console line from an admin.
    AdminCommand {
        /// The line.
        line: String,
    },
    /// Server → client: the output of an admin command (or of a login).
    AdminOutput {
        /// Output lines.
        lines: Vec<String>,
    },
    /// A game-defined session message (broadcast chat, simulated-client input).
    Game {
        /// The payload.
        bytes: Vec<u8>,
    },
}

mod tag {
    pub const WELCOME: u8 = 1;
    pub const REFUSED: u8 = 2;
    pub const LEVEL_READY: u8 = 3;
    pub const JOIN_REMOVED: u8 = 4;
    pub const JOIN_RECORDS: u8 = 5;
    pub const IN_GAME: u8 = 6;
    pub const TRAVEL_BEGIN: u8 = 7;
    pub const GOODBYE: u8 = 8;
    pub const KEEPALIVE: u8 = 9;
    pub const PLAYER_REMOVED: u8 = 10;
    pub const STREAM_BEGIN: u8 = 11;
    pub const STREAM_CHUNK: u8 = 12;
    pub const ADMIN_LOGIN: u8 = 13;
    pub const ADMIN_COMMAND: u8 = 14;
    pub const ADMIN_OUTPUT: u8 = 15;
    pub const GAME: u8 = 16;
}

/// The connection channel an encoded session message rides (spec 120 § 5.1, ADR-8302): session
/// streams (`StreamBegin`, `StreamChunk`) go on the bulk stream, every other reliable session
/// message on the control stream, so a large stream never holds a join, travel or kick back.
/// Unreliable messages stay unreliable.
pub fn wire_channel(channel: crate::connection::Channel, bytes: &[u8]) -> crate::connection::Channel {
    use crate::connection::{Channel, StreamId};
    if !channel.is_reliable() {
        return channel;
    }
    match bytes.first() {
        Some(&tag::STREAM_BEGIN) | Some(&tag::STREAM_CHUNK) => Channel::Stream(StreamId::BULK),
        _ => Channel::Stream(StreamId::CONTROL),
    }
}

/// The kind name of an encoded session message (the first byte decides): `hello`, `welcome`,
/// `refused`, `level_ready`, `join_removed`, `join_records`, `in_game`, `travel_begin`, `goodbye`,
/// `keepalive`, `player_removed`, `stream_begin`, `stream_chunk`, `admin_login`, `admin_command`,
/// `admin_output`, `game`, or `unknown`. The bandwidth view (spec 77 § 10.2) groups bytes by it.
#[must_use]
pub fn kind_name(bytes: &[u8]) -> &'static str {
    match bytes.first() {
        Some(b) if *b == HELLO_MAGIC[0] => "hello",
        Some(&tag::WELCOME) => "welcome",
        Some(&tag::REFUSED) => "refused",
        Some(&tag::LEVEL_READY) => "level_ready",
        Some(&tag::JOIN_REMOVED) => "join_removed",
        Some(&tag::JOIN_RECORDS) => "join_records",
        Some(&tag::IN_GAME) => "in_game",
        Some(&tag::TRAVEL_BEGIN) => "travel_begin",
        Some(&tag::GOODBYE) => "goodbye",
        Some(&tag::KEEPALIVE) => "keepalive",
        Some(&tag::PLAYER_REMOVED) => "player_removed",
        Some(&tag::STREAM_BEGIN) => "stream_begin",
        Some(&tag::STREAM_CHUNK) => "stream_chunk",
        Some(&tag::ADMIN_LOGIN) => "admin_login",
        Some(&tag::ADMIN_COMMAND) => "admin_command",
        Some(&tag::ADMIN_OUTPUT) => "admin_output",
        Some(&tag::GAME) => "game",
        _ => "unknown",
    }
}

/// Most `level_removed` guids in one message.
pub const MAX_JOIN_REMOVED: usize = 4096;

impl SessionMsg {
    /// Encodes the message.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut w = WireWriter::new();
        match self {
            Self::Hello(h) => return h.encode(),
            Self::Welcome(wel) => {
                w.u8(tag::WELCOME);
                w.u32(wel.peer_id.0);
                w.u64(wel.session_id);
                w.u64(wel.server_tick);
                w.u32(wel.tick_hz);
                w.u32(wel.send_every_ticks);
                w.str_u8(&wel.level, 255)?;
                w.u32(wel.travel_id);
                w.bytes_u16(wel.motd.as_bytes(), MAX_DETAIL_BYTES)?;
                w.str_u8(&wel.server_name, 255)?;
                w.raw(&wel.reconnect_token);
                w.u8(wel.players.len().min(255) as u8);
                for p in wel.players.iter().take(255) {
                    w.u32(p.peer.0);
                    w.u8(p.local);
                }
            }
            Self::Refused(r) => {
                w.u8(tag::REFUSED);
                r.encode(&mut w);
            }
            Self::LevelReady { travel_id } => {
                w.u8(tag::LEVEL_READY);
                w.u32(*travel_id);
            }
            Self::JoinRemoved { guids } => {
                if guids.len() > MAX_JOIN_REMOVED {
                    return Err(WireError::TooLong {
                        len: guids.len(),
                        max: MAX_JOIN_REMOVED,
                    });
                }
                w.u8(tag::JOIN_REMOVED);
                w.u16(guids.len() as u16);
                for g in guids {
                    w.raw(&g.to_le_bytes());
                }
            }
            Self::JoinRecords { bytes } => {
                w.u8(tag::JOIN_RECORDS);
                w.bytes_u16(bytes, MAX_SESSION_PAYLOAD_BYTES)?;
            }
            Self::InGame { tick } => {
                w.u8(tag::IN_GAME);
                w.u64(*tick);
            }
            Self::TravelBegin {
                level,
                travel_id,
                keep,
            } => {
                w.u8(tag::TRAVEL_BEGIN);
                w.str_u8(level, 255)?;
                w.u32(*travel_id);
                match keep {
                    None => w.u8(0),
                    Some(k) => {
                        if k.ids.len() > MAX_TRAVEL_KEEP_IDS {
                            return Err(WireError::TooLong {
                                len: k.ids.len(),
                                max: MAX_TRAVEL_KEEP_IDS,
                            });
                        }
                        w.u8(1);
                        w.u32(k.below.0);
                        w.u16(k.ids.len() as u16);
                        for id in &k.ids {
                            w.u32(id.0);
                        }
                    }
                }
            }
            Self::Goodbye(r) => {
                w.u8(tag::GOODBYE);
                r.encode(&mut w);
            }
            Self::Keepalive => w.u8(tag::KEEPALIVE),
            Self::PlayerRemoved { local, reason } => {
                w.u8(tag::PLAYER_REMOVED);
                w.u8(*local);
                reason.encode(&mut w);
            }
            Self::StreamBegin { id, kind, total } => {
                w.u8(tag::STREAM_BEGIN);
                w.u32(*id);
                w.str_u8(kind, 255)?;
                w.u64(*total);
            }
            Self::StreamChunk { id, bytes } => {
                w.u8(tag::STREAM_CHUNK);
                w.u32(*id);
                w.bytes_u16(bytes, MAX_STREAM_CHUNK_BYTES)?;
            }
            Self::AdminLogin { token } => {
                w.u8(tag::ADMIN_LOGIN);
                w.str_u8(token, 255)?;
            }
            Self::AdminCommand { line } => {
                w.u8(tag::ADMIN_COMMAND);
                w.bytes_u16(line.as_bytes(), MAX_DETAIL_BYTES)?;
            }
            Self::AdminOutput { lines } => {
                w.u8(tag::ADMIN_OUTPUT);
                w.u16(lines.len().min(MAX_ADMIN_OUTPUT_LINES) as u16);
                for l in lines.iter().take(MAX_ADMIN_OUTPUT_LINES) {
                    w.bytes_u16(truncate_utf8(l, MAX_DETAIL_BYTES).as_bytes(), MAX_DETAIL_BYTES)?;
                }
            }
            Self::Game { bytes } => {
                w.u8(tag::GAME);
                w.bytes_u16(bytes, MAX_SESSION_PAYLOAD_BYTES)?;
            }
        }
        Ok(w.finish())
    }

    /// Decodes one message. A `Hello` is decoded whole; callers that must answer a foreign protocol
    /// version read [`Hello::decode_prefix`] first.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.first() == Some(&HELLO_MAGIC[0]) {
            return Ok(Self::Hello(Box::new(Hello::decode(bytes)?)));
        }
        let mut r = WireReader::new(bytes);
        let msg = match r.u8()? {
            tag::WELCOME => {
                let peer_id = PeerId(r.u32()?);
                let session_id = r.u64()?;
                let server_tick = r.u64()?;
                let tick_hz = r.u32()?;
                let send_every_ticks = r.u32()?;
                let level = r.str_u8(255)?.to_owned();
                let travel_id = r.u32()?;
                let motd = std::str::from_utf8(r.bytes_u16(MAX_DETAIL_BYTES)?)
                    .map_err(|_| WireError::Utf8)?
                    .to_owned();
                let server_name = r.str_u8(255)?.to_owned();
                let reconnect_token = r.array::<RECONNECT_TOKEN_BYTES>()?;
                let n = usize::from(r.u8()?);
                let mut players = Vec::with_capacity(n);
                for _ in 0..n {
                    players.push(NetPlayerId {
                        peer: PeerId(r.u32()?),
                        local: r.u8()?,
                    });
                }
                Self::Welcome(Box::new(Welcome {
                    peer_id,
                    session_id,
                    server_tick,
                    tick_hz,
                    send_every_ticks,
                    level,
                    travel_id,
                    motd,
                    server_name,
                    reconnect_token,
                    players,
                }))
            }
            tag::REFUSED => Self::Refused(DisconnectReason::decode(&mut r)?),
            tag::LEVEL_READY => Self::LevelReady { travel_id: r.u32()? },
            tag::JOIN_REMOVED => {
                let n = usize::from(r.u16()?);
                if n > MAX_JOIN_REMOVED {
                    return Err(WireError::TooLong {
                        len: n,
                        max: MAX_JOIN_REMOVED,
                    });
                }
                let mut guids = Vec::with_capacity(n);
                for _ in 0..n {
                    guids.push(u128::from_le_bytes(r.array::<16>()?));
                }
                Self::JoinRemoved { guids }
            }
            tag::JOIN_RECORDS => Self::JoinRecords {
                bytes: r.bytes_u16(MAX_SESSION_PAYLOAD_BYTES)?.to_vec(),
            },
            tag::IN_GAME => Self::InGame { tick: r.u64()? },
            tag::TRAVEL_BEGIN => {
                let level = r.str_u8(255)?.to_owned();
                let travel_id = r.u32()?;
                let keep = match r.u8()? {
                    0 => None,
                    1 => {
                        let below = NetId(r.u32()?);
                        let n = usize::from(r.u16()?);
                        if n > MAX_TRAVEL_KEEP_IDS {
                            return Err(WireError::TooLong {
                                len: n,
                                max: MAX_TRAVEL_KEEP_IDS,
                            });
                        }
                        let mut ids = Vec::with_capacity(n);
                        for _ in 0..n {
                            ids.push(NetId(r.u32()?));
                        }
                        Some(TravelKeep { below, ids })
                    }
                    _ => return Err(WireError::Invalid("travel keep flag")),
                };
                Self::TravelBegin {
                    level,
                    travel_id,
                    keep,
                }
            }
            tag::GOODBYE => Self::Goodbye(DisconnectReason::decode(&mut r)?),
            tag::KEEPALIVE => Self::Keepalive,
            tag::PLAYER_REMOVED => Self::PlayerRemoved {
                local: r.u8()?,
                reason: DisconnectReason::decode(&mut r)?,
            },
            tag::STREAM_BEGIN => Self::StreamBegin {
                id: r.u32()?,
                kind: r.str_u8(255)?.to_owned(),
                total: r.u64()?,
            },
            tag::STREAM_CHUNK => Self::StreamChunk {
                id: r.u32()?,
                bytes: r.bytes_u16(MAX_STREAM_CHUNK_BYTES)?.to_vec(),
            },
            tag::ADMIN_LOGIN => Self::AdminLogin {
                token: r.str_u8(255)?.to_owned(),
            },
            tag::ADMIN_COMMAND => Self::AdminCommand {
                line: std::str::from_utf8(r.bytes_u16(MAX_DETAIL_BYTES)?)
                    .map_err(|_| WireError::Utf8)?
                    .to_owned(),
            },
            tag::ADMIN_OUTPUT => {
                let n = usize::from(r.u16()?);
                if n > MAX_ADMIN_OUTPUT_LINES {
                    return Err(WireError::Invalid("too many admin output lines"));
                }
                let mut lines = Vec::with_capacity(n);
                for _ in 0..n {
                    lines.push(
                        std::str::from_utf8(r.bytes_u16(MAX_DETAIL_BYTES)?)
                            .map_err(|_| WireError::Utf8)?
                            .to_owned(),
                    );
                }
                Self::AdminOutput { lines }
            }
            tag::GAME => Self::Game {
                bytes: r.bytes_u16(MAX_SESSION_PAYLOAD_BYTES)?.to_vec(),
            },
            _ => return Err(WireError::Invalid("session message tag")),
        };
        r.finish()?;
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit L1 (muse): the decoder holds the limit the encoder keeps.
    #[test]
    fn admin_output_line_count_is_bounded_on_decode() {
        let mut w = WireWriter::new();
        w.u8(tag::ADMIN_OUTPUT);
        w.u16(MAX_ADMIN_OUTPUT_LINES as u16 + 1);
        for _ in 0..=MAX_ADMIN_OUTPUT_LINES {
            w.u16(0);
        }
        assert!(SessionMsg::decode(&w.finish()).is_err());
        let lines = vec![String::new(); MAX_ADMIN_OUTPUT_LINES];
        let bytes = SessionMsg::AdminOutput { lines: lines.clone() }
            .encode()
            .expect("fits");
        assert_eq!(
            SessionMsg::decode(&bytes).expect("valid"),
            SessionMsg::AdminOutput { lines }
        );
    }

    fn manifest() -> ContentManifest {
        ContentManifest::build("0.5.0", "1.0.0", 1, 9, &[])
    }

    #[test]
    fn every_code_round_trips() {
        for code in DISCONNECT_CODES {
            let r = DisconnectReason::from_code(code, "d").expect("code");
            assert_eq!(r.code(), *code);
            let bytes = SessionMsg::Goodbye(r.clone()).encode().expect("encode");
            assert_eq!(
                SessionMsg::decode(&bytes).expect("decode"),
                SessionMsg::Goodbye(r)
            );
        }
    }

    #[test]
    fn hello_prefix_survives_an_unknown_body() {
        let hello = Hello {
            protocol_version: 1,
            protocol_hash: 0xabcd,
            manifest: manifest(),
            player_names: vec!["Ann".into()],
            password: Some("pw".into()),
            auth: Some(vec![1, 2]),
            client_rate_bytes: 1000,
            reconnect_token: Some([7; 16]),
        };
        let bytes = hello.encode().expect("encode");
        assert_eq!(&bytes[..4], b"RGNH");
        assert_eq!(
            SessionMsg::decode(&bytes).expect("decode"),
            SessionMsg::Hello(Box::new(hello))
        );
        // A future version: the prefix with a body this build cannot read.
        let mut future = Vec::from(HELLO_MAGIC);
        future.extend_from_slice(&99u32.to_le_bytes());
        future.extend_from_slice(&5u64.to_le_bytes());
        future.extend_from_slice(&[0xff; 7]);
        assert_eq!(Hello::decode_prefix(&future), Some((99, 5)));
        assert!(Hello::decode(&future).is_err());
    }

    #[test]
    fn hostile_bytes_never_panic() {
        let mut seed = 1u64;
        for len in 0..200usize {
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    seed = seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    (seed >> 56) as u8
                })
                .collect();
            let _ = SessionMsg::decode(&bytes);
            let _ = Hello::decode_prefix(&bytes);
        }
    }

    /// Spec 77 § 6.2 step 3 (ADR-7259): `TravelBegin` carries the copies a client keeps (the
    /// persistent `NetId`s and the first `NetId` of the new level), or nothing when the server
    /// does not know them; the decoder holds the encoder's bound.
    #[test]
    fn travel_begin_carries_the_persistent_net_ids() {
        use crate::model::NetId;
        for keep in [
            None,
            Some(TravelKeep {
                below: NetId(40),
                ids: vec![NetId(3), NetId(17)],
            }),
            Some(TravelKeep {
                below: NetId(1),
                ids: Vec::new(),
            }),
        ] {
            let msg = SessionMsg::TravelBegin {
                level: "levels/b.level.ron".into(),
                travel_id: 4,
                keep,
            };
            let bytes = msg.encode().expect("encode");
            assert_eq!(SessionMsg::decode(&bytes).expect("decode"), msg);
        }
        let too_many = SessionMsg::TravelBegin {
            level: "l".into(),
            travel_id: 1,
            keep: Some(TravelKeep {
                below: NetId(u32::MAX),
                ids: (0..=MAX_TRAVEL_KEEP_IDS as u32).map(NetId).collect(),
            }),
        };
        assert!(too_many.encode().is_err());
        let mut w = WireWriter::new();
        w.u8(tag::TRAVEL_BEGIN);
        w.str_u8("l", 255).expect("level");
        w.u32(1);
        w.u8(1);
        w.u32(9);
        w.u16(MAX_TRAVEL_KEEP_IDS as u16 + 1);
        assert!(SessionMsg::decode(&w.finish()).is_err(), "count over the bound");
        let mut w = WireWriter::new();
        w.u8(tag::TRAVEL_BEGIN);
        w.str_u8("l", 255).expect("level");
        w.u32(1);
        w.u8(2);
        assert!(SessionMsg::decode(&w.finish()).is_err(), "unknown keep flag");
    }
}
