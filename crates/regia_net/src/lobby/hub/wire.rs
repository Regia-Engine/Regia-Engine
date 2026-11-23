// SPDX-License-Identifier: MPL-2.0
//! The hub's messages and their bounded codec.
//!
//! Both directions ride one `regia_net` game, [`HubWire`] (`regia.lobby_hub`), on the reliable
//! ordered channel. Its protocol version is [`HUB_WIRE_VERSION`]; it feeds the protocol id like any
//! game's (spec 120 § 12.1), so a hub and a client of another hub wire version refuse each other at
//! the handshake with `protocol_mismatch` and the version probe names the older side. There is no
//! cross-version path.
//!
//! Decoding is bounded: every string has a byte limit, every list a count limit checked before any
//! element is read, nothing is allocated from a length the bytes do not back, and trailing bytes
//! are an error.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use super::join::{JoinPlan, MAX_TICKET_BYTES, NatClass, ReachKind, ReachOffer};
use super::{HubLobbyCard, HubPresence, InviteCard, InviteToken, PresenceRow};
use crate::crypto::{PLAYER_PROOF_BYTES, PlayerKey};
use crate::handshake::{decode_addr, encode_addr};
use crate::join_code::JOIN_CODE_LEN;
use crate::lobby::{
    LeaveReason, LobbyId, LobbyServer, LobbyVisibility, MAX_CHAT_BYTES, MAX_KEY_BYTES, MAX_KEYS,
    MAX_LOBBY_MEMBERS, MAX_VALUE_BYTES, MemberId,
};
use crate::nat::{decode_candidates, encode_candidates};
use crate::payload::{NetCodec, NetGame};
use crate::wire::{WireError, WireReader, WireWriter};

/// The hub's game id in the protocol id.
pub const HUB_GAME_ID: &str = "regia.lobby_hub";
/// The hub wire version (the [`HubWire`] protocol version). Bump it on any incompatible change
/// after a release. Version 1 is unreleased, so NL-05's fields (the optional player proof in
/// `Hello`, the player key in `Welcome`, friends by player key, the join broker's requests and
/// events) are part of version 1 (ADR-8633 amends ADR-8607's bump; ADR-7724 practice).
pub const HUB_WIRE_VERSION: u32 = 1;
/// Largest hub message, whatever the link allows.
pub const MAX_HUB_MESSAGE_BYTES: usize = 256 * 1024;
/// Most presence rows or removals in one directory page.
pub const MAX_PAGE_ROWS: usize = 4096;
/// Most lobby cards or removals in one directory page.
pub const MAX_PAGE_LOBBIES: usize = 1024;
/// Longest error code on the wire.
pub const MAX_CODE_BYTES: usize = 64;

const HANDLE_WIRE_BYTES: usize = super::HANDLE_LIMIT_BYTES;

/// The `regia_net` game the hub speaks.
#[derive(Debug, Clone, Copy)]
pub struct HubWire;

impl NetGame for HubWire {
    type Message = HubMessage;
    type Input = ();
    type JoinData = ();
    const GAME_ID: &'static str = HUB_GAME_ID;
    const PROTOCOL_VERSION: u32 = HUB_WIRE_VERSION;
}

/// One hub message: a client's request or the hub's event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubMessage {
    /// Client to hub.
    Request(HubRequest),
    /// Hub to client.
    Event(HubEvent),
}

/// Which chat room a line is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatScope {
    /// Every session on the hub.
    Global,
    /// The members of one lobby.
    Lobby(LobbyId),
}

/// What a client asks the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubRequest {
    /// The confirm after the handshake: the hub allocates a session for this handle, under the
    /// player key the proof binds to this sealed link (`crate::crypto::PlayerIdentity::prove`).
    /// The proof is optional: an empty one gives a session with no player key, which friends-only
    /// lobbies refuse (ADR-8633).
    Hello {
        /// The handle (a display and session name, never an identity).
        handle: String,
        /// The player proof ([`PLAYER_PROOF_BYTES`] bytes), or empty for no player key.
        proof: Vec<u8>,
    },
    /// Keeps an idle session alive.
    Heartbeat,
    /// Asks for the directory from these generations on; answered by directory pages and
    /// [`HubEvent::ListDone`].
    List {
        /// The client's request id.
        request: u64,
        /// The directory generation the client has.
        dir_gen: u64,
        /// The presence generation the client has.
        presence_gen: u64,
    },
    /// Sets the session's presence.
    SetPresence(HubPresence),
    /// Sets the session's friends by player key (who may join its friends-only lobbies; ADR-8633).
    SetFriends(Vec<PlayerKey>),
    /// Creates a lobby the session owns.
    Create {
        /// The client's request id.
        request: u64,
        /// Visibility.
        visibility: LobbyVisibility,
        /// Member limit.
        max_members: u32,
        /// Whether others may join.
        joinable: bool,
        /// Initial lobby data.
        data: BTreeMap<String, String>,
    },
    /// Joins a lobby, with an invite token when the session has one.
    Join {
        /// The client's request id.
        request: u64,
        /// The lobby.
        lobby: LobbyId,
        /// The invite token.
        token: Option<InviteToken>,
    },
    /// Leaves a lobby.
    Leave {
        /// The lobby.
        lobby: LobbyId,
    },
    /// Sets or deletes lobby data (owner).
    SetData {
        /// The lobby.
        lobby: LobbyId,
        /// The key.
        key: String,
        /// The value; `None` deletes.
        value: Option<String>,
    },
    /// Sets or deletes the session's member data.
    SetMemberData {
        /// The lobby.
        lobby: LobbyId,
        /// The key.
        key: String,
        /// The value; `None` deletes.
        value: Option<String>,
    },
    /// Opens or closes a lobby to joins (owner).
    SetJoinable {
        /// The lobby.
        lobby: LobbyId,
        /// Whether others may join.
        joinable: bool,
    },
    /// The owner confirms (`Some`) or withdraws (`None`) the game session's address; only members
    /// receive it.
    SetServer {
        /// The lobby.
        lobby: LobbyId,
        /// Where the session is.
        server: Option<LobbyServer>,
    },
    /// Invites a handle to a lobby (owner).
    Invite {
        /// The lobby.
        lobby: LobbyId,
        /// The invitee's handle.
        handle: String,
    },
    /// Mutes or unmutes a member in lobby chat (owner).
    Mute {
        /// The lobby.
        lobby: LobbyId,
        /// The member.
        member: MemberId,
        /// Mute or unmute.
        muted: bool,
    },
    /// A chat line.
    Chat {
        /// Where.
        scope: ChatScope,
        /// The text.
        text: String,
    },
    /// The owner offers how its game socket is reached (candidates, relay, join code, ticket).
    Offer {
        /// The lobby.
        lobby: LobbyId,
        /// The offer.
        offer: ReachOffer,
    },
    /// A member asks for a path to the owner's game session, with its own game socket's reach.
    AskReach {
        /// The lobby.
        lobby: LobbyId,
        /// The member's candidates and NAT behavior (relay, code and ticket are ignored).
        offer: ReachOffer,
    },
    /// The owner approves (or denies) a member's ask.
    Approve {
        /// The lobby.
        lobby: LobbyId,
        /// The member.
        member: MemberId,
        /// Approve or deny.
        approve: bool,
    },
    /// The member reports the path that carried its sealed handshake.
    ReachDone {
        /// The lobby.
        lobby: LobbyId,
        /// The path.
        path: ReachKind,
        /// Connectivity checks it sent before the path was chosen.
        punch_attempts: u32,
    },
}

/// One page of the directory: a snapshot (`reset` on its first page) or a delta. Generations are
/// the hub's after the change; a client moves its cursor when a page with `done` arrives.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DirectoryPage {
    /// The directory generation.
    pub dir_gen: u64,
    /// The presence generation.
    pub presence_gen: u64,
    /// First page of a snapshot: drop everything known before applying.
    pub reset: bool,
    /// Last page of this update.
    pub done: bool,
    /// New or changed presence rows.
    pub players: Vec<PresenceRow>,
    /// Sessions gone.
    pub removed_players: Vec<MemberId>,
    /// New or changed listed lobbies.
    pub lobbies: Vec<HubLobbyCard>,
    /// Lobbies no longer listed.
    pub removed_lobbies: Vec<LobbyId>,
}

/// A whole lobby, sent to a member when it enters. Members see the session address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomState {
    /// The lobby.
    pub lobby: LobbyId,
    /// Its owner.
    pub owner: MemberId,
    /// Visibility.
    pub visibility: LobbyVisibility,
    /// Member limit.
    pub max_members: u32,
    /// Whether others may join.
    pub joinable: bool,
    /// Lobby data.
    pub data: BTreeMap<String, String>,
    /// Members in join order: id, handle, member data.
    pub members: Vec<(MemberId, String, BTreeMap<String, String>)>,
    /// The confirmed session address.
    pub server: Option<LobbyServer>,
}

/// What the hub tells a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubEvent {
    /// The session exists: its member id, handle, the player key the hub bound it to, and the
    /// hub's idle timeout. A client refuses a `Welcome` whose key is not its own
    /// (`hub_identity_mismatch`).
    Welcome {
        /// The session's member id.
        member: MemberId,
        /// The handle as the hub stored it.
        handle: String,
        /// Silence after which the hub expires the session, in milliseconds.
        idle_ms: u64,
        /// The player key the hub recorded for the session.
        identity: PlayerKey,
    },
    /// A `Create` or `Join` finished.
    Reply {
        /// The client's request id.
        request: u64,
        /// The lobby, or the error code.
        result: Result<LobbyId, String>,
    },
    /// The directory pages of a `List` were sent.
    ListDone {
        /// The client's request id.
        request: u64,
    },
    /// A directory page.
    Directory(DirectoryPage),
    /// The lobby the session just entered, whole.
    Room(RoomState),
    /// Someone joined a lobby the session is in.
    MemberJoined {
        /// The lobby.
        lobby: LobbyId,
        /// Who.
        member: MemberId,
        /// Their handle.
        handle: String,
    },
    /// Someone left a lobby the session is in.
    MemberLeft {
        /// The lobby.
        lobby: LobbyId,
        /// Who.
        member: MemberId,
    },
    /// Lobby data changed.
    Data {
        /// The lobby.
        lobby: LobbyId,
        /// The key.
        key: String,
        /// The value; `None` deleted.
        value: Option<String>,
    },
    /// A member's data changed.
    MemberData {
        /// The lobby.
        lobby: LobbyId,
        /// The member.
        member: MemberId,
        /// The key.
        key: String,
        /// The value; `None` deleted.
        value: Option<String>,
    },
    /// The lobby opened or closed to joins.
    Joinable {
        /// The lobby.
        lobby: LobbyId,
        /// Whether others may join.
        joinable: bool,
    },
    /// The lobby has a new owner.
    OwnerChanged {
        /// The lobby.
        lobby: LobbyId,
        /// The new owner.
        owner: MemberId,
    },
    /// The owner confirmed or withdrew the session address (members only).
    ServerSet {
        /// The lobby.
        lobby: LobbyId,
        /// Where the session is; `None` withdrawn.
        server: Option<LobbyServer>,
    },
    /// A chat line.
    Chat {
        /// Where.
        scope: ChatScope,
        /// Who.
        from: MemberId,
        /// Their handle.
        from_handle: String,
        /// The text.
        text: String,
    },
    /// An invite for this session.
    Invite(InviteCard),
    /// The session is no longer in this lobby.
    Left {
        /// The lobby.
        lobby: LobbyId,
        /// Why.
        reason: LeaveReason,
    },
    /// An operation without a request id was refused.
    Refused {
        /// The error code.
        code: String,
    },
    /// A member asks for a path to the owner's session (to the owner only).
    ReachAsk {
        /// The lobby.
        lobby: LobbyId,
        /// Who asks.
        member: MemberId,
        /// Their handle.
        handle: String,
        /// Their player key.
        identity: PlayerKey,
        /// Their game socket's candidates and NAT behavior.
        offer: ReachOffer,
    },
    /// The approved plan, to the member and to the owner (each with the other's candidates).
    Plan(JoinPlan),
    /// The owner refused this session's ask.
    ReachDenied {
        /// The lobby.
        lobby: LobbyId,
    },
    /// A member reported the path that carried its handshake (to the owner).
    Reached {
        /// The lobby.
        lobby: LobbyId,
        /// The member.
        member: MemberId,
        /// The path.
        path: ReachKind,
        /// Connectivity checks it sent.
        punch_attempts: u32,
    },
}

impl HubEvent {
    /// The event's wire bytes (as [`HubMessage::Event`]).
    pub fn to_bytes(&self) -> Result<Vec<u8>, WireError> {
        HubMessage::Event(self.clone()).to_bytes()
    }

    /// Bytes this event takes on the wire (0 when it cannot be encoded).
    pub fn wire_len(&self) -> usize {
        let mut w = WireWriter::new();
        encode_event(&mut w, self);
        w.len()
    }
}

impl HubRequest {
    /// The request's wire bytes (as [`HubMessage::Request`]).
    pub fn to_bytes(&self) -> Result<Vec<u8>, WireError> {
        HubMessage::Request(self.clone()).to_bytes()
    }
}

impl HubMessage {
    /// Decodes a whole message after checking its size against [`MAX_HUB_MESSAGE_BYTES`].
    pub fn decode_bounded(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() > MAX_HUB_MESSAGE_BYTES {
            return Err(WireError::TooLong {
                len: bytes.len(),
                max: MAX_HUB_MESSAGE_BYTES,
            });
        }
        Self::from_bytes(bytes)
    }
}

impl NetCodec for HubMessage {
    fn encode(&self, w: &mut WireWriter) {
        match self {
            Self::Request(r) => encode_request(w, r),
            Self::Event(e) => encode_event(w, e),
        }
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        let tag = r.u8()?;
        if tag < EV_BASE {
            decode_request(tag, r).map(Self::Request)
        } else {
            decode_event(tag, r).map(Self::Event)
        }
    }
}

// Request tags 1..=15, event tags 64..=79.
const RQ_HELLO: u8 = 1;
const RQ_HEARTBEAT: u8 = 2;
const RQ_LIST: u8 = 3;
const RQ_SET_PRESENCE: u8 = 4;
const RQ_SET_FRIENDS: u8 = 5;
const RQ_CREATE: u8 = 6;
const RQ_JOIN: u8 = 7;
const RQ_LEAVE: u8 = 8;
const RQ_SET_DATA: u8 = 9;
const RQ_SET_MEMBER_DATA: u8 = 10;
const RQ_SET_JOINABLE: u8 = 11;
const RQ_SET_SERVER: u8 = 12;
const RQ_INVITE: u8 = 13;
const RQ_MUTE: u8 = 14;
const RQ_CHAT: u8 = 15;
const RQ_OFFER: u8 = 16;
const RQ_ASK_REACH: u8 = 17;
const RQ_APPROVE: u8 = 18;
const RQ_REACH_DONE: u8 = 19;

const EV_BASE: u8 = 64;
const EV_WELCOME: u8 = 64;
const EV_REPLY: u8 = 65;
const EV_LIST_DONE: u8 = 66;
const EV_DIRECTORY: u8 = 67;
const EV_ROOM: u8 = 68;
const EV_MEMBER_JOINED: u8 = 69;
const EV_MEMBER_LEFT: u8 = 70;
const EV_DATA: u8 = 71;
const EV_MEMBER_DATA: u8 = 72;
const EV_JOINABLE: u8 = 73;
const EV_OWNER: u8 = 74;
const EV_SERVER: u8 = 75;
const EV_CHAT: u8 = 76;
const EV_INVITE: u8 = 77;
const EV_LEFT: u8 = 78;
const EV_REFUSED: u8 = 79;
const EV_REACH_ASK: u8 = 80;
const EV_PLAN: u8 = 81;
const EV_REACH_DENIED: u8 = 82;
const EV_REACHED: u8 = 83;

fn encode_request(w: &mut WireWriter, rq: &HubRequest) {
    match rq {
        HubRequest::Hello { handle, proof } => {
            w.u8(RQ_HELLO);
            put_str8(w, handle, HANDLE_WIRE_BYTES);
            let _ = w.bytes_u16(proof, PLAYER_PROOF_BYTES);
        }
        HubRequest::Heartbeat => w.u8(RQ_HEARTBEAT),
        HubRequest::List {
            request,
            dir_gen,
            presence_gen,
        } => {
            w.u8(RQ_LIST);
            w.u64(*request);
            w.u64(*dir_gen);
            w.u64(*presence_gen);
        }
        HubRequest::SetPresence(p) => {
            w.u8(RQ_SET_PRESENCE);
            w.u8(p.to_u8());
        }
        HubRequest::SetFriends(list) => {
            w.u8(RQ_SET_FRIENDS);
            put_count_u16(w, list.len(), super::FRIENDS_LIMIT);
            for f in list.iter().take(super::FRIENDS_LIMIT) {
                w.raw(&f.0);
            }
        }
        HubRequest::Create {
            request,
            visibility,
            max_members,
            joinable,
            data,
        } => {
            w.u8(RQ_CREATE);
            w.u64(*request);
            w.u8(visibility_u8(*visibility));
            w.u32(*max_members);
            w.u8(u8::from(*joinable));
            put_data(w, data);
        }
        HubRequest::Join {
            request,
            lobby,
            token,
        } => {
            w.u8(RQ_JOIN);
            w.u64(*request);
            w.u64(lobby.0);
            match token {
                Some(t) => {
                    w.u8(1);
                    w.raw(&t.0);
                }
                None => w.u8(0),
            }
        }
        HubRequest::Leave { lobby } => {
            w.u8(RQ_LEAVE);
            w.u64(lobby.0);
        }
        HubRequest::SetData { lobby, key, value } => {
            w.u8(RQ_SET_DATA);
            w.u64(lobby.0);
            put_str8(w, key, MAX_KEY_BYTES);
            put_opt_value(w, value.as_deref());
        }
        HubRequest::SetMemberData { lobby, key, value } => {
            w.u8(RQ_SET_MEMBER_DATA);
            w.u64(lobby.0);
            put_str8(w, key, MAX_KEY_BYTES);
            put_opt_value(w, value.as_deref());
        }
        HubRequest::SetJoinable { lobby, joinable } => {
            w.u8(RQ_SET_JOINABLE);
            w.u64(lobby.0);
            w.u8(u8::from(*joinable));
        }
        HubRequest::SetServer { lobby, server } => {
            w.u8(RQ_SET_SERVER);
            w.u64(lobby.0);
            put_opt_server(w, server.as_ref());
        }
        HubRequest::Invite { lobby, handle } => {
            w.u8(RQ_INVITE);
            w.u64(lobby.0);
            put_str8(w, handle, HANDLE_WIRE_BYTES);
        }
        HubRequest::Mute { lobby, member, muted } => {
            w.u8(RQ_MUTE);
            w.u64(lobby.0);
            w.u64(member.0);
            w.u8(u8::from(*muted));
        }
        HubRequest::Chat { scope, text } => {
            w.u8(RQ_CHAT);
            put_scope(w, *scope);
            put_str16(w, text, MAX_CHAT_BYTES);
        }
        HubRequest::Offer { lobby, offer } => {
            w.u8(RQ_OFFER);
            w.u64(lobby.0);
            put_offer(w, offer);
        }
        HubRequest::AskReach { lobby, offer } => {
            w.u8(RQ_ASK_REACH);
            w.u64(lobby.0);
            put_offer(w, offer);
        }
        HubRequest::Approve {
            lobby,
            member,
            approve,
        } => {
            w.u8(RQ_APPROVE);
            w.u64(lobby.0);
            w.u64(member.0);
            w.u8(u8::from(*approve));
        }
        HubRequest::ReachDone {
            lobby,
            path,
            punch_attempts,
        } => {
            w.u8(RQ_REACH_DONE);
            w.u64(lobby.0);
            w.u8(path.to_u8());
            w.u32(*punch_attempts);
        }
    }
}

fn decode_request(tag: u8, r: &mut WireReader<'_>) -> Result<HubRequest, WireError> {
    Ok(match tag {
        RQ_HELLO => HubRequest::Hello {
            handle: get_str8(r, HANDLE_WIRE_BYTES)?,
            proof: r.bytes_u16(PLAYER_PROOF_BYTES)?.to_vec(),
        },
        RQ_HEARTBEAT => HubRequest::Heartbeat,
        RQ_LIST => HubRequest::List {
            request: r.u64()?,
            dir_gen: r.u64()?,
            presence_gen: r.u64()?,
        },
        RQ_SET_PRESENCE => {
            HubRequest::SetPresence(HubPresence::from_u8(r.u8()?).ok_or(WireError::Invalid("hub presence"))?)
        }
        RQ_SET_FRIENDS => {
            let n = get_count_u16(r, super::FRIENDS_LIMIT)?;
            let mut list = Vec::with_capacity(n.min(64));
            for _ in 0..n {
                list.push(PlayerKey(r.array()?));
            }
            HubRequest::SetFriends(list)
        }
        RQ_CREATE => HubRequest::Create {
            request: r.u64()?,
            visibility: get_visibility(r)?,
            max_members: r.u32()?,
            joinable: get_bool(r)?,
            data: get_data(r)?,
        },
        RQ_JOIN => HubRequest::Join {
            request: r.u64()?,
            lobby: LobbyId(r.u64()?),
            token: match r.u8()? {
                0 => None,
                1 => Some(InviteToken(r.array::<16>()?)),
                _ => return Err(WireError::Invalid("hub token flag")),
            },
        },
        RQ_LEAVE => HubRequest::Leave {
            lobby: LobbyId(r.u64()?),
        },
        RQ_SET_DATA => HubRequest::SetData {
            lobby: LobbyId(r.u64()?),
            key: get_str8(r, MAX_KEY_BYTES)?,
            value: get_opt_value(r)?,
        },
        RQ_SET_MEMBER_DATA => HubRequest::SetMemberData {
            lobby: LobbyId(r.u64()?),
            key: get_str8(r, MAX_KEY_BYTES)?,
            value: get_opt_value(r)?,
        },
        RQ_SET_JOINABLE => HubRequest::SetJoinable {
            lobby: LobbyId(r.u64()?),
            joinable: get_bool(r)?,
        },
        RQ_SET_SERVER => HubRequest::SetServer {
            lobby: LobbyId(r.u64()?),
            server: get_opt_server(r)?,
        },
        RQ_INVITE => HubRequest::Invite {
            lobby: LobbyId(r.u64()?),
            handle: get_str8(r, HANDLE_WIRE_BYTES)?,
        },
        RQ_MUTE => HubRequest::Mute {
            lobby: LobbyId(r.u64()?),
            member: MemberId(r.u64()?),
            muted: get_bool(r)?,
        },
        RQ_CHAT => HubRequest::Chat {
            scope: get_scope(r)?,
            text: get_str16(r, MAX_CHAT_BYTES)?,
        },
        RQ_OFFER => HubRequest::Offer {
            lobby: LobbyId(r.u64()?),
            offer: get_offer(r)?,
        },
        RQ_ASK_REACH => HubRequest::AskReach {
            lobby: LobbyId(r.u64()?),
            offer: get_offer(r)?,
        },
        RQ_APPROVE => HubRequest::Approve {
            lobby: LobbyId(r.u64()?),
            member: MemberId(r.u64()?),
            approve: get_bool(r)?,
        },
        RQ_REACH_DONE => HubRequest::ReachDone {
            lobby: LobbyId(r.u64()?),
            path: get_reach_kind(r)?,
            punch_attempts: r.u32()?,
        },
        _ => return Err(WireError::Invalid("hub request tag")),
    })
}

fn encode_event(w: &mut WireWriter, ev: &HubEvent) {
    match ev {
        HubEvent::Welcome {
            member,
            handle,
            idle_ms,
            identity,
        } => {
            w.u8(EV_WELCOME);
            w.u64(member.0);
            put_str8(w, handle, HANDLE_WIRE_BYTES);
            w.u64(*idle_ms);
            w.raw(&identity.0);
        }
        HubEvent::Reply { request, result } => {
            w.u8(EV_REPLY);
            w.u64(*request);
            match result {
                Ok(lobby) => {
                    w.u8(0);
                    w.u64(lobby.0);
                }
                Err(code) => {
                    w.u8(1);
                    put_str8(w, code, MAX_CODE_BYTES);
                }
            }
        }
        HubEvent::ListDone { request } => {
            w.u8(EV_LIST_DONE);
            w.u64(*request);
        }
        HubEvent::Directory(page) => {
            w.u8(EV_DIRECTORY);
            encode_page(w, page);
        }
        HubEvent::Room(room) => {
            w.u8(EV_ROOM);
            w.u64(room.lobby.0);
            w.u64(room.owner.0);
            w.u8(visibility_u8(room.visibility));
            w.u32(room.max_members);
            w.u8(u8::from(room.joinable));
            put_data(w, &room.data);
            put_count_u16(w, room.members.len(), MAX_LOBBY_MEMBERS as usize);
            for (id, handle, data) in room.members.iter().take(MAX_LOBBY_MEMBERS as usize) {
                w.u64(id.0);
                put_str8(w, handle, HANDLE_WIRE_BYTES);
                put_data(w, data);
            }
            put_opt_server(w, room.server.as_ref());
        }
        HubEvent::MemberJoined {
            lobby,
            member,
            handle,
        } => {
            w.u8(EV_MEMBER_JOINED);
            w.u64(lobby.0);
            w.u64(member.0);
            put_str8(w, handle, HANDLE_WIRE_BYTES);
        }
        HubEvent::MemberLeft { lobby, member } => {
            w.u8(EV_MEMBER_LEFT);
            w.u64(lobby.0);
            w.u64(member.0);
        }
        HubEvent::Data { lobby, key, value } => {
            w.u8(EV_DATA);
            w.u64(lobby.0);
            put_str8(w, key, MAX_KEY_BYTES);
            put_opt_value(w, value.as_deref());
        }
        HubEvent::MemberData {
            lobby,
            member,
            key,
            value,
        } => {
            w.u8(EV_MEMBER_DATA);
            w.u64(lobby.0);
            w.u64(member.0);
            put_str8(w, key, MAX_KEY_BYTES);
            put_opt_value(w, value.as_deref());
        }
        HubEvent::Joinable { lobby, joinable } => {
            w.u8(EV_JOINABLE);
            w.u64(lobby.0);
            w.u8(u8::from(*joinable));
        }
        HubEvent::OwnerChanged { lobby, owner } => {
            w.u8(EV_OWNER);
            w.u64(lobby.0);
            w.u64(owner.0);
        }
        HubEvent::ServerSet { lobby, server } => {
            w.u8(EV_SERVER);
            w.u64(lobby.0);
            put_opt_server(w, server.as_ref());
        }
        HubEvent::Chat {
            scope,
            from,
            from_handle,
            text,
        } => {
            w.u8(EV_CHAT);
            put_scope(w, *scope);
            w.u64(from.0);
            put_str8(w, from_handle, HANDLE_WIRE_BYTES);
            put_str16(w, text, MAX_CHAT_BYTES);
        }
        HubEvent::Invite(card) => {
            w.u8(EV_INVITE);
            encode_card(w, card);
        }
        HubEvent::Left { lobby, reason } => {
            w.u8(EV_LEFT);
            w.u64(lobby.0);
            w.u8(match reason {
                LeaveReason::Left => 0,
                LeaveReason::Kicked => 1,
                LeaveReason::Lost => 2,
            });
        }
        HubEvent::Refused { code } => {
            w.u8(EV_REFUSED);
            put_str8(w, code, MAX_CODE_BYTES);
        }
        HubEvent::ReachAsk {
            lobby,
            member,
            handle,
            identity,
            offer,
        } => {
            w.u8(EV_REACH_ASK);
            w.u64(lobby.0);
            w.u64(member.0);
            put_str8(w, handle, HANDLE_WIRE_BYTES);
            w.raw(&identity.0);
            put_offer(w, offer);
        }
        HubEvent::Plan(plan) => {
            w.u8(EV_PLAN);
            w.u64(plan.lobby.0);
            w.u64(plan.peer.0);
            w.raw(&plan.peer_identity.0);
            w.u8(plan.path.to_u8());
            put_opt_addr(w, plan.mapped);
            put_offer(
                w,
                &ReachOffer {
                    candidates: plan.candidates.clone(),
                    nat: None,
                    relay: plan.relay,
                    join_code: plan.join_code.clone(),
                    ticket: plan.ticket.clone(),
                },
            );
        }
        HubEvent::ReachDenied { lobby } => {
            w.u8(EV_REACH_DENIED);
            w.u64(lobby.0);
        }
        HubEvent::Reached {
            lobby,
            member,
            path,
            punch_attempts,
        } => {
            w.u8(EV_REACHED);
            w.u64(lobby.0);
            w.u64(member.0);
            w.u8(path.to_u8());
            w.u32(*punch_attempts);
        }
    }
}

fn decode_event(tag: u8, r: &mut WireReader<'_>) -> Result<HubEvent, WireError> {
    Ok(match tag {
        EV_WELCOME => HubEvent::Welcome {
            member: MemberId(r.u64()?),
            handle: get_str8(r, HANDLE_WIRE_BYTES)?,
            idle_ms: r.u64()?,
            identity: PlayerKey(r.array()?),
        },
        EV_REPLY => {
            let request = r.u64()?;
            let result = match r.u8()? {
                0 => Ok(LobbyId(r.u64()?)),
                1 => Err(get_code(r)?),
                _ => return Err(WireError::Invalid("hub reply flag")),
            };
            HubEvent::Reply { request, result }
        }
        EV_LIST_DONE => HubEvent::ListDone { request: r.u64()? },
        EV_DIRECTORY => HubEvent::Directory(decode_page(r)?),
        EV_ROOM => {
            let lobby = LobbyId(r.u64()?);
            let owner = MemberId(r.u64()?);
            let visibility = get_visibility(r)?;
            let max_members = r.u32()?;
            let joinable = get_bool(r)?;
            let data = get_data(r)?;
            let n = get_count_u16(r, MAX_LOBBY_MEMBERS as usize)?;
            let mut members = Vec::with_capacity(n.min(16));
            for _ in 0..n {
                members.push((MemberId(r.u64()?), get_str8(r, HANDLE_WIRE_BYTES)?, get_data(r)?));
            }
            HubEvent::Room(RoomState {
                lobby,
                owner,
                visibility,
                max_members,
                joinable,
                data,
                members,
                server: get_opt_server(r)?,
            })
        }
        EV_MEMBER_JOINED => HubEvent::MemberJoined {
            lobby: LobbyId(r.u64()?),
            member: MemberId(r.u64()?),
            handle: get_str8(r, HANDLE_WIRE_BYTES)?,
        },
        EV_MEMBER_LEFT => HubEvent::MemberLeft {
            lobby: LobbyId(r.u64()?),
            member: MemberId(r.u64()?),
        },
        EV_DATA => HubEvent::Data {
            lobby: LobbyId(r.u64()?),
            key: get_str8(r, MAX_KEY_BYTES)?,
            value: get_opt_value(r)?,
        },
        EV_MEMBER_DATA => HubEvent::MemberData {
            lobby: LobbyId(r.u64()?),
            member: MemberId(r.u64()?),
            key: get_str8(r, MAX_KEY_BYTES)?,
            value: get_opt_value(r)?,
        },
        EV_JOINABLE => HubEvent::Joinable {
            lobby: LobbyId(r.u64()?),
            joinable: get_bool(r)?,
        },
        EV_OWNER => HubEvent::OwnerChanged {
            lobby: LobbyId(r.u64()?),
            owner: MemberId(r.u64()?),
        },
        EV_SERVER => HubEvent::ServerSet {
            lobby: LobbyId(r.u64()?),
            server: get_opt_server(r)?,
        },
        EV_CHAT => HubEvent::Chat {
            scope: get_scope(r)?,
            from: MemberId(r.u64()?),
            from_handle: get_str8(r, HANDLE_WIRE_BYTES)?,
            text: get_str16(r, MAX_CHAT_BYTES)?,
        },
        EV_INVITE => HubEvent::Invite(decode_card(r)?),
        EV_LEFT => HubEvent::Left {
            lobby: LobbyId(r.u64()?),
            reason: match r.u8()? {
                0 => LeaveReason::Left,
                1 => LeaveReason::Kicked,
                2 => LeaveReason::Lost,
                _ => return Err(WireError::Invalid("hub leave reason")),
            },
        },
        EV_REFUSED => HubEvent::Refused { code: get_code(r)? },
        EV_REACH_ASK => HubEvent::ReachAsk {
            lobby: LobbyId(r.u64()?),
            member: MemberId(r.u64()?),
            handle: get_str8(r, HANDLE_WIRE_BYTES)?,
            identity: PlayerKey(r.array()?),
            offer: get_offer(r)?,
        },
        EV_PLAN => {
            let lobby = LobbyId(r.u64()?);
            let peer = MemberId(r.u64()?);
            let peer_identity = PlayerKey(r.array()?);
            let path = get_reach_kind(r)?;
            let mapped = get_opt_addr(r)?;
            let offer = get_offer(r)?;
            HubEvent::Plan(JoinPlan {
                lobby,
                peer,
                peer_identity,
                path,
                mapped,
                candidates: offer.candidates,
                relay: offer.relay,
                join_code: offer.join_code,
                ticket: offer.ticket,
            })
        }
        EV_REACH_DENIED => HubEvent::ReachDenied {
            lobby: LobbyId(r.u64()?),
        },
        EV_REACHED => HubEvent::Reached {
            lobby: LobbyId(r.u64()?),
            member: MemberId(r.u64()?),
            path: get_reach_kind(r)?,
            punch_attempts: r.u32()?,
        },
        _ => return Err(WireError::Invalid("hub event tag")),
    })
}

/// Writes an invite card: lobby, inviter, inviter handle, token, expiry. No address field exists.
pub(crate) fn encode_card(w: &mut WireWriter, card: &InviteCard) {
    w.u64(card.lobby.0);
    w.u64(card.from.0);
    put_str8(w, &card.from_handle, HANDLE_WIRE_BYTES);
    w.raw(&card.token.0);
    w.u64(card.expires_ms);
}

fn decode_card(r: &mut WireReader<'_>) -> Result<InviteCard, WireError> {
    Ok(InviteCard {
        lobby: LobbyId(r.u64()?),
        from: MemberId(r.u64()?),
        from_handle: get_str8(r, HANDLE_WIRE_BYTES)?,
        token: InviteToken(r.array::<16>()?),
        expires_ms: r.u64()?,
    })
}

fn encode_page(w: &mut WireWriter, page: &DirectoryPage) {
    w.u64(page.dir_gen);
    w.u64(page.presence_gen);
    w.u8(u8::from(page.reset) | (u8::from(page.done) << 1));
    put_count_u16(w, page.players.len(), MAX_PAGE_ROWS);
    for row in page.players.iter().take(MAX_PAGE_ROWS) {
        w.u64(row.member.0);
        put_str8(w, &row.handle, HANDLE_WIRE_BYTES);
        w.u8(row.presence.to_u8());
        w.u64(row.lobby.map_or(0, |l| l.0));
    }
    put_count_u16(w, page.removed_players.len(), MAX_PAGE_ROWS);
    for id in page.removed_players.iter().take(MAX_PAGE_ROWS) {
        w.u64(id.0);
    }
    put_count_u16(w, page.lobbies.len(), MAX_PAGE_LOBBIES);
    for card in page.lobbies.iter().take(MAX_PAGE_LOBBIES) {
        w.u64(card.id.0);
        w.u64(card.owner.0);
        put_str8(w, &card.owner_handle, HANDLE_WIRE_BYTES);
        w.u8(visibility_u8(card.visibility));
        w.u32(card.max_members);
        w.u32(card.member_count);
        w.u8(u8::from(card.joinable));
        put_data(w, &card.data);
    }
    put_count_u16(w, page.removed_lobbies.len(), MAX_PAGE_LOBBIES);
    for id in page.removed_lobbies.iter().take(MAX_PAGE_LOBBIES) {
        w.u64(id.0);
    }
}

fn decode_page(r: &mut WireReader<'_>) -> Result<DirectoryPage, WireError> {
    let dir_gen = r.u64()?;
    let presence_gen = r.u64()?;
    let flags = r.u8()?;
    if flags > 3 {
        return Err(WireError::Invalid("hub page flags"));
    }
    let n = get_count_u16(r, MAX_PAGE_ROWS)?;
    let mut players = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let member = MemberId(r.u64()?);
        let handle = get_str8(r, HANDLE_WIRE_BYTES)?;
        let presence = HubPresence::from_u8(r.u8()?).ok_or(WireError::Invalid("hub presence"))?;
        let lobby = Some(r.u64()?).filter(|l| *l != 0).map(LobbyId);
        players.push(PresenceRow {
            member,
            handle,
            presence,
            lobby,
        });
    }
    let n = get_count_u16(r, MAX_PAGE_ROWS)?;
    let mut removed_players = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        removed_players.push(MemberId(r.u64()?));
    }
    let n = get_count_u16(r, MAX_PAGE_LOBBIES)?;
    let mut lobbies = Vec::with_capacity(n.min(16));
    for _ in 0..n {
        lobbies.push(HubLobbyCard {
            id: LobbyId(r.u64()?),
            owner: MemberId(r.u64()?),
            owner_handle: get_str8(r, HANDLE_WIRE_BYTES)?,
            visibility: get_visibility(r)?,
            max_members: r.u32()?,
            member_count: r.u32()?,
            joinable: get_bool(r)?,
            data: get_data(r)?,
        });
    }
    let n = get_count_u16(r, MAX_PAGE_LOBBIES)?;
    let mut removed_lobbies = Vec::with_capacity(n.min(16));
    for _ in 0..n {
        removed_lobbies.push(LobbyId(r.u64()?));
    }
    Ok(DirectoryPage {
        dir_gen,
        presence_gen,
        reset: flags & 1 != 0,
        done: flags & 2 != 0,
        players,
        removed_players,
        lobbies,
        removed_lobbies,
    })
}

fn get_reach_kind(r: &mut WireReader<'_>) -> Result<ReachKind, WireError> {
    ReachKind::from_u8(r.u8()?).ok_or(WireError::Invalid("hub reach kind"))
}

fn put_opt_addr(w: &mut WireWriter, addr: Option<SocketAddr>) {
    match addr {
        Some(a) => {
            w.u8(1);
            encode_addr(w, a);
        }
        None => w.u8(0),
    }
}

fn get_opt_addr(r: &mut WireReader<'_>) -> Result<Option<SocketAddr>, WireError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(decode_addr(r)?)),
        _ => Err(WireError::Invalid("hub address flag")),
    }
}

/// A reach offer: candidates, NAT byte (`0xFF` unknown), relay, join code, ticket.
fn put_offer(w: &mut WireWriter, offer: &ReachOffer) {
    if offer.candidates.len() > crate::nat::NAT_MAX_CANDIDATES {
        w.fail(WireError::TooLong {
            len: offer.candidates.len(),
            max: crate::nat::NAT_MAX_CANDIDATES,
        });
    }
    encode_candidates(w, &offer.candidates);
    w.u8(offer.nat.map_or(0xFF, NatClass::to_u8));
    put_opt_addr(w, offer.relay);
    match &offer.join_code {
        Some(code) => {
            w.u8(1);
            put_str8(w, code, JOIN_CODE_LEN * 2);
        }
        None => w.u8(0),
    }
    let _ = w.bytes_u16(&offer.ticket, MAX_TICKET_BYTES);
}

fn get_offer(r: &mut WireReader<'_>) -> Result<ReachOffer, WireError> {
    let candidates = decode_candidates(r)?;
    let nat = match r.u8()? {
        0xFF => None,
        v => Some(NatClass::from_u8(v).ok_or(WireError::Invalid("hub nat class"))?),
    };
    let relay = get_opt_addr(r)?;
    let join_code = match r.u8()? {
        0 => None,
        1 => Some(get_str8(r, JOIN_CODE_LEN * 2)?),
        _ => return Err(WireError::Invalid("hub code flag")),
    };
    let ticket = r.bytes_u16(MAX_TICKET_BYTES)?.to_vec();
    Ok(ReachOffer {
        candidates,
        nat,
        relay,
        join_code,
        ticket,
    })
}

fn put_str8(w: &mut WireWriter, s: &str, max: usize) {
    // A failure is recorded in the writer; `finish_checked` reports it.
    let _ = w.str_u8(s, max);
}

fn get_str8(r: &mut WireReader<'_>, max: usize) -> Result<String, WireError> {
    Ok(r.str_u8(max)?.to_owned())
}

fn put_str16(w: &mut WireWriter, s: &str, max: usize) {
    let _ = w.bytes_u16(s.as_bytes(), max);
}

fn get_str16(r: &mut WireReader<'_>, max: usize) -> Result<String, WireError> {
    let bytes = r.bytes_u16(max)?;
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| WireError::Utf8)
}

fn get_code(r: &mut WireReader<'_>) -> Result<String, WireError> {
    let code = get_str8(r, MAX_CODE_BYTES)?;
    if !code
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(WireError::Invalid("hub error code"));
    }
    Ok(code)
}

fn put_count_u16(w: &mut WireWriter, n: usize, max: usize) {
    if n > max {
        w.fail(WireError::TooLong { len: n, max });
    }
    w.u16(n.min(max) as u16);
}

fn get_count_u16(r: &mut WireReader<'_>, max: usize) -> Result<usize, WireError> {
    let n = usize::from(r.u16()?);
    if n > max {
        return Err(WireError::TooLong { len: n, max });
    }
    Ok(n)
}

fn get_bool(r: &mut WireReader<'_>) -> Result<bool, WireError> {
    match r.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(WireError::Invalid("hub bool")),
    }
}

fn put_opt_value(w: &mut WireWriter, value: Option<&str>) {
    match value {
        Some(v) => {
            w.u8(1);
            put_str16(w, v, MAX_VALUE_BYTES);
        }
        None => w.u8(0),
    }
}

fn get_opt_value(r: &mut WireReader<'_>) -> Result<Option<String>, WireError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(get_str16(r, MAX_VALUE_BYTES)?)),
        _ => Err(WireError::Invalid("hub option flag")),
    }
}

fn put_data(w: &mut WireWriter, data: &BTreeMap<String, String>) {
    if data.len() > MAX_KEYS {
        w.fail(WireError::TooLong {
            len: data.len(),
            max: MAX_KEYS,
        });
    }
    w.u8(data.len().min(MAX_KEYS) as u8);
    for (k, v) in data.iter().take(MAX_KEYS) {
        put_str8(w, k, MAX_KEY_BYTES);
        put_str16(w, v, MAX_VALUE_BYTES);
    }
}

fn get_data(r: &mut WireReader<'_>) -> Result<BTreeMap<String, String>, WireError> {
    let n = usize::from(r.u8()?);
    if n > MAX_KEYS {
        return Err(WireError::TooLong {
            len: n,
            max: MAX_KEYS,
        });
    }
    let mut data = BTreeMap::new();
    for _ in 0..n {
        let k = get_str8(r, MAX_KEY_BYTES)?;
        let v = get_str16(r, MAX_VALUE_BYTES)?;
        if data.insert(k, v).is_some() {
            return Err(WireError::Invalid("hub duplicate data key"));
        }
    }
    Ok(data)
}

fn visibility_u8(v: LobbyVisibility) -> u8 {
    match v {
        LobbyVisibility::Public => 0,
        LobbyVisibility::FriendsOnly => 1,
        LobbyVisibility::Private => 2,
        LobbyVisibility::Invisible => 3,
    }
}

fn get_visibility(r: &mut WireReader<'_>) -> Result<LobbyVisibility, WireError> {
    Ok(match r.u8()? {
        0 => LobbyVisibility::Public,
        1 => LobbyVisibility::FriendsOnly,
        2 => LobbyVisibility::Private,
        3 => LobbyVisibility::Invisible,
        _ => return Err(WireError::Invalid("hub visibility")),
    })
}

fn put_scope(w: &mut WireWriter, scope: ChatScope) {
    match scope {
        ChatScope::Global => w.u64(0),
        ChatScope::Lobby(l) => w.u64(l.0),
    }
}

fn get_scope(r: &mut WireReader<'_>) -> Result<ChatScope, WireError> {
    Ok(match r.u64()? {
        0 => ChatScope::Global,
        id => ChatScope::Lobby(LobbyId(id)),
    })
}

fn put_opt_server(w: &mut WireWriter, server: Option<&LobbyServer>) {
    match server {
        None => w.u8(0),
        Some(LobbyServer::Address(SocketAddr::V4(a))) => {
            w.u8(1);
            w.raw(&a.ip().octets());
            w.u16(a.port());
        }
        Some(LobbyServer::Address(SocketAddr::V6(a))) => {
            w.u8(2);
            w.raw(&a.ip().octets());
            w.u16(a.port());
        }
        Some(LobbyServer::Steam(id)) => {
            w.u8(3);
            w.u64(*id);
        }
    }
}

fn get_opt_server(r: &mut WireReader<'_>) -> Result<Option<LobbyServer>, WireError> {
    Ok(match r.u8()? {
        0 => None,
        1 => {
            let ip = Ipv4Addr::from(r.array::<4>()?);
            Some(LobbyServer::Address(SocketAddr::new(IpAddr::V4(ip), r.u16()?)))
        }
        2 => {
            let ip = Ipv6Addr::from(r.array::<16>()?);
            Some(LobbyServer::Address(SocketAddr::new(IpAddr::V6(ip), r.u16()?)))
        }
        3 => match r.u64()? {
            0 => return Err(WireError::Invalid("hub steam id 0")),
            id => Some(LobbyServer::Steam(id)),
        },
        _ => return Err(WireError::Invalid("hub server tag")),
    })
}

/// A request and an event of every kind, for the round-trip tests and the fuzz fixture.
pub fn sample_messages() -> Vec<HubMessage> {
    let lobby = LobbyId(7);
    let mut data = BTreeMap::new();
    data.insert("mode".to_owned(), "duel".to_owned());
    let token = InviteToken([0xA5; 16]);
    let offer = ReachOffer {
        candidates: vec![
            crate::nat::Candidate::new(
                crate::nat::CandidateKind::Host,
                SocketAddr::from(([10, 0, 0, 2], 7777)),
            ),
            crate::nat::Candidate::new(
                crate::nat::CandidateKind::Mapped,
                SocketAddr::from(([203, 0, 113, 8], 7777)),
            ),
        ],
        nat: Some(NatClass::PORT_RESTRICTED),
        relay: Some(SocketAddr::from(([192, 0, 2, 1], 7800))),
        join_code: Some("ABCDEF".into()),
        ticket: b"RGJT\x01ticket".to_vec(),
    };
    let requests = vec![
        HubRequest::Hello {
            handle: "ann".into(),
            proof: vec![0x5A; PLAYER_PROOF_BYTES],
        },
        HubRequest::Heartbeat,
        HubRequest::List {
            request: 1,
            dir_gen: 2,
            presence_gen: 3,
        },
        HubRequest::SetPresence(HubPresence::Lobby),
        HubRequest::SetFriends(vec![PlayerKey([0x0B; 32]), PlayerKey([0x0C; 32])]),
        HubRequest::Create {
            request: 4,
            visibility: LobbyVisibility::FriendsOnly,
            max_members: 8,
            joinable: true,
            data: data.clone(),
        },
        HubRequest::Join {
            request: 5,
            lobby,
            token: Some(token),
        },
        HubRequest::Leave { lobby },
        HubRequest::SetData {
            lobby,
            key: "map".into(),
            value: Some("harbor".into()),
        },
        HubRequest::SetMemberData {
            lobby,
            key: "regia.ready".into(),
            value: None,
        },
        HubRequest::SetJoinable {
            lobby,
            joinable: false,
        },
        HubRequest::SetServer {
            lobby,
            server: Some(LobbyServer::Steam(42)),
        },
        HubRequest::Invite {
            lobby,
            handle: "bob".into(),
        },
        HubRequest::Mute {
            lobby,
            member: MemberId(2),
            muted: true,
        },
        HubRequest::Chat {
            scope: ChatScope::Global,
            text: "hi".into(),
        },
        HubRequest::Offer {
            lobby,
            offer: offer.clone(),
        },
        HubRequest::AskReach {
            lobby,
            offer: ReachOffer {
                candidates: offer.candidates.clone(),
                nat: Some(NatClass::SYMMETRIC),
                ..ReachOffer::default()
            },
        },
        HubRequest::Approve {
            lobby,
            member: MemberId(2),
            approve: true,
        },
        HubRequest::ReachDone {
            lobby,
            path: ReachKind::Punch,
            punch_attempts: 3,
        },
    ];
    let events = vec![
        HubEvent::Welcome {
            member: MemberId(1),
            handle: "ann".into(),
            idle_ms: 30_000,
            identity: PlayerKey([0x11; 32]),
        },
        HubEvent::Reply {
            request: 4,
            result: Ok(lobby),
        },
        HubEvent::Reply {
            request: 5,
            result: Err("hub_invite_only".into()),
        },
        HubEvent::ListDone { request: 1 },
        HubEvent::Directory(DirectoryPage {
            dir_gen: 3,
            presence_gen: 4,
            reset: true,
            done: true,
            players: vec![PresenceRow {
                member: MemberId(1),
                handle: "ann".into(),
                presence: HubPresence::Lobby,
                lobby: Some(lobby),
            }],
            removed_players: vec![MemberId(9)],
            lobbies: vec![HubLobbyCard {
                id: lobby,
                owner: MemberId(1),
                owner_handle: "ann".into(),
                visibility: LobbyVisibility::Public,
                max_members: 8,
                member_count: 1,
                joinable: true,
                data: data.clone(),
            }],
            removed_lobbies: vec![LobbyId(3)],
        }),
        HubEvent::Room(RoomState {
            lobby,
            owner: MemberId(1),
            visibility: LobbyVisibility::Private,
            max_members: 4,
            joinable: true,
            data: data.clone(),
            members: vec![(MemberId(1), "ann".into(), BTreeMap::new())],
            server: Some(LobbyServer::Address(SocketAddr::from(([10, 0, 0, 1], 7777)))),
        }),
        HubEvent::MemberJoined {
            lobby,
            member: MemberId(2),
            handle: "bob".into(),
        },
        HubEvent::MemberLeft {
            lobby,
            member: MemberId(2),
        },
        HubEvent::Data {
            lobby,
            key: "map".into(),
            value: None,
        },
        HubEvent::MemberData {
            lobby,
            member: MemberId(2),
            key: "regia.ready".into(),
            value: Some("1".into()),
        },
        HubEvent::Joinable {
            lobby,
            joinable: true,
        },
        HubEvent::OwnerChanged {
            lobby,
            owner: MemberId(2),
        },
        HubEvent::ServerSet {
            lobby,
            server: Some(LobbyServer::Address(SocketAddr::from((
                Ipv6Addr::LOCALHOST,
                7000,
            )))),
        },
        HubEvent::Chat {
            scope: ChatScope::Lobby(lobby),
            from: MemberId(1),
            from_handle: "ann".into(),
            text: "gl hf".into(),
        },
        HubEvent::Invite(InviteCard {
            lobby,
            from: MemberId(1),
            from_handle: "ann".into(),
            token,
            expires_ms: 7_200_000,
        }),
        HubEvent::Left {
            lobby,
            reason: LeaveReason::Kicked,
        },
        HubEvent::Refused {
            code: "hub_muted".into(),
        },
        HubEvent::ReachAsk {
            lobby,
            member: MemberId(2),
            handle: "bob".into(),
            identity: PlayerKey([0x22; 32]),
            offer: ReachOffer {
                candidates: offer.candidates.clone(),
                ..ReachOffer::default()
            },
        },
        HubEvent::Plan(JoinPlan {
            lobby,
            peer: MemberId(1),
            peer_identity: PlayerKey([0x11; 32]),
            path: ReachKind::Mapped,
            mapped: Some(SocketAddr::from(([203, 0, 113, 8], 7777))),
            candidates: offer.candidates.clone(),
            relay: offer.relay,
            join_code: offer.join_code.clone(),
            ticket: offer.ticket.clone(),
        }),
        HubEvent::ReachDenied { lobby },
        HubEvent::Reached {
            lobby,
            member: MemberId(2),
            path: ReachKind::Relay,
            punch_attempts: 0,
        },
    ];
    requests
        .into_iter()
        .map(HubMessage::Request)
        .chain(events.into_iter().map(HubMessage::Event))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        for m in sample_messages() {
            let bytes = m.to_bytes().expect("encodes");
            assert_eq!(HubMessage::decode_bounded(&bytes).expect("decodes"), m);
            // Every strict prefix is refused, never accepted as something else or a panic.
            for cut in 0..bytes.len() {
                assert!(
                    HubMessage::decode_bounded(&bytes[..cut]).is_err(),
                    "{m:?} cut at {cut}"
                );
            }
            let mut long = bytes.clone();
            long.push(0);
            assert!(HubMessage::decode_bounded(&long).is_err(), "trailing bytes");
        }
    }

    #[test]
    fn hostile_counts_are_refused_before_reading() {
        // A friend list claiming 65 535 entries in a 4-byte message.
        let bytes = [RQ_SET_FRIENDS, 0xFF, 0xFF, 0];
        assert!(matches!(
            HubMessage::decode_bounded(&bytes),
            Err(WireError::TooLong { .. })
        ));
        // A page claiming 4096 rows backed by nothing: truncated, not allocated.
        let mut w = WireWriter::new();
        w.u8(EV_DIRECTORY);
        w.u64(1);
        w.u64(1);
        w.u8(3);
        w.u16(MAX_PAGE_ROWS as u16);
        assert!(HubMessage::decode_bounded(&w.finish()).is_err());
        // Too big a message is refused before decoding.
        let huge = vec![RQ_HEARTBEAT; MAX_HUB_MESSAGE_BYTES + 1];
        assert!(matches!(
            HubMessage::decode_bounded(&huge),
            Err(WireError::TooLong { .. })
        ));
        // A code with characters outside the code alphabet.
        let mut w = WireWriter::new();
        w.u8(EV_REFUSED);
        let _ = w.str_u8("Hub Muted", 64);
        assert!(HubMessage::decode_bounded(&w.finish()).is_err());
    }

    #[test]
    fn an_over_long_field_fails_the_encode() {
        let m = HubMessage::Request(HubRequest::Hello {
            handle: "x".repeat(HANDLE_WIRE_BYTES + 1),
            proof: Vec::new(),
        });
        assert!(m.to_bytes().is_err());
        let m = HubMessage::Request(HubRequest::SetFriends(vec![
            PlayerKey::default();
            super::super::FRIENDS_LIMIT + 1
        ]));
        assert!(m.to_bytes().is_err());
    }

    /// Hostile hub-wire-v2 bytes, including proof lengths, are an error or a bounded message.
    /// Nothing here panics or allocates from a length the following bytes do not back.
    #[test]
    fn hostile_v2_messages_stay_bounded() {
        for b in 0..=255u8 {
            let _ = HubMessage::decode_bounded(&[b]);
            let _ = HubMessage::decode_bounded(&[b, 0xff, 0xff, 0, 0]);
        }
        let mut samples = Vec::new();
        // Hello, proof length 0, 127 and 128 (all legal on the wire; verification is later).
        for (claimed, body) in [(0u16, 0usize), (127, 127), (128, 128)] {
            let mut w = WireWriter::new();
            w.u8(1);
            w.str_u8("ann", 32).expect("handle");
            w.u16(claimed);
            w.raw(&vec![0xA5; body]);
            samples.push(w.finish());
        }
        // A proof length past the cap, and a u16::MAX length with no body: refused before the
        // claimed allocation.
        for claimed in [129u16, u16::MAX] {
            let mut w = WireWriter::new();
            w.u8(1);
            w.str_u8("ann", 32).expect("handle");
            w.u16(claimed);
            if claimed == 129 {
                w.raw(&[0u8; 129]);
            }
            assert!(
                matches!(
                    HubMessage::decode_bounded(&w.finish()),
                    Err(WireError::TooLong { .. })
                ),
                "claimed proof length {claimed}"
            );
        }
        for bytes in samples {
            if let Ok(message) = HubMessage::decode_bounded(&bytes) {
                let again = message.to_bytes().expect("a decoded message re-encodes");
                assert!(again.len() <= MAX_HUB_MESSAGE_BYTES);
            }
        }
        let oversized = HubMessage::Request(HubRequest::Hello {
            handle: "ann".into(),
            proof: vec![0; PLAYER_PROOF_BYTES + 1],
        });
        assert!(matches!(oversized.to_bytes(), Err(WireError::TooLong { .. })));
    }
}
