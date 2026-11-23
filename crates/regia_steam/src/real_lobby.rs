// SPDX-License-Identifier: MPL-2.0
//! The real Steam lobby backend (ISteamMatchmaking) and peer-to-peer messages
//! (ISteamNetworkingMessages) over the `steamworks` 0.13 bindings (spec 89, ADR-1877). Desktop
//! only, feature `steamworks`.
//!
//! Callbacks only queue raw results; [`LobbyBackend::poll`] turns them into
//! [`LobbyEvent`]s on the game thread. Membership, ownership and the launched server are found by
//! comparing each joined lobby with the previous poll rather than through the member-state
//! callback, whose binding aborts on combined state flags (ADR-1877 (4)). Every key and value is
//! checked before it reaches the bindings, which abort on interior NUL bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_net::lobby::{
    Compare, Distance, LeaveReason, LobbyBackend, LobbyError, LobbyEvent, LobbyId, LobbyInfo, LobbyMember,
    LobbyQuery, LobbyServer, LobbySpec, LobbyVisibility, MAX_KEYS, MemberId, RequestId, check_chat,
    check_key, check_spec, check_value, keys,
};
use regia_net::socket::MAX_DATAGRAM_BYTES;
use steamworks::networking_types::{NetworkingIdentity, SendFlags};

use crate::backend::{SteamError, SteamId};
use crate::lobby::{Membership, membership_events};
use crate::real_lobby_info::{LobbyFacts, MAX_DATA_ENTRIES, lobby_info};
use crate::sockets::{P2pMessages, SteamAccept};

/// What a callback reports; turned into events by `poll`.
enum Raw {
    Created(RequestId, Result<u64, String>, LobbySpec),
    Entered(RequestId, Result<u64, ()>),
    /// The `LobbyEnter` callback for a join we initiated: the bindings' `join_lobby` collapses
    /// every failure to `Err(())`, this carries the actual response code and the locked flag.
    Enter {
        request: RequestId,
        lobby: u64,
        /// `LobbyEnter_t.m_bLocked`: "only invited users may join" — the joinable flag inverted.
        blocked: bool,
        response: steamworks::ChatRoomEnterResponse,
    },
    List(RequestId, Result<Vec<u64>, String>),
    Chat {
        lobby: u64,
        user: u64,
        chat_id: i32,
    },
    Invite {
        lobby: u64,
        friend: u64,
    },
    Data {
        lobby: u64,
        member: u64,
    },
}

/// What we last saw of a joined lobby.
#[derive(Clone)]
struct Seen {
    members: Vec<MemberId>,
    owner: MemberId,
    server: Option<LobbyServer>,
    visibility: LobbyVisibility,
    joinable: bool,
}

/// The Steam lobby backend.
pub(crate) struct RealLobbies {
    client: steamworks::Client,
    raw: Arc<Mutex<Vec<Raw>>>,
    next_request: AtomicU64,
    joined: Mutex<BTreeMap<LobbyId, Seen>>,
    member_keys: Mutex<BTreeSet<String>>,
    /// Last member rows per lobby, so a refresh reuses persona names whose ids did not change.
    /// A `PersonaStateChange` with a new name drops that member from it (`forget_member_name`).
    names: Arc<Mutex<BTreeMap<LobbyId, Vec<LobbyMember>>>>,
    /// Lobbies a `join` is waiting on (`LobbyEnter_t` carries no request id, so the callback
    /// attributes itself through this map; a stale entry is overwritten by the next join of the
    /// same lobby).
    pending: Arc<Mutex<BTreeMap<u64, RequestId>>>,
    _callbacks: Vec<steamworks::CallbackHandle>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Converts the bindings' response code to the feature-independent mirror (see
/// [`crate::lobby::EnterResponse`]).
fn enter_response(response: steamworks::ChatRoomEnterResponse) -> crate::lobby::EnterResponse {
    use steamworks::ChatRoomEnterResponse as S;
    match response {
        S::Success => crate::lobby::EnterResponse::Success,
        S::DoesntExist => crate::lobby::EnterResponse::DoesntExist,
        S::NotAllowed => crate::lobby::EnterResponse::NotAllowed,
        S::Full => crate::lobby::EnterResponse::Full,
        S::Error => crate::lobby::EnterResponse::Error,
        S::Banned => crate::lobby::EnterResponse::Banned,
        S::Limited => crate::lobby::EnterResponse::Limited,
        S::ClanDisabled => crate::lobby::EnterResponse::ClanDisabled,
        S::CommunityBan => crate::lobby::EnterResponse::CommunityBan,
        S::MemberBlockedYou => crate::lobby::EnterResponse::MemberBlockedYou,
        S::YouBlockedMember => crate::lobby::EnterResponse::YouBlockedMember,
        S::RatelimitExceeded => crate::lobby::EnterResponse::RatelimitExceeded,
    }
}

fn lobby_type(v: LobbyVisibility) -> steamworks::LobbyType {
    match v {
        LobbyVisibility::Public => steamworks::LobbyType::Public,
        LobbyVisibility::FriendsOnly => steamworks::LobbyType::FriendsOnly,
        LobbyVisibility::Private => steamworks::LobbyType::Private,
        LobbyVisibility::Invisible => steamworks::LobbyType::Invisible,
    }
}

fn string_kind(c: Compare) -> steamworks::StringFilterKind {
    match c {
        Compare::Eq => steamworks::StringFilterKind::Equal,
        Compare::Ne => steamworks::StringFilterKind::NotEqual,
        Compare::Lt => steamworks::StringFilterKind::LessThan,
        Compare::Le => steamworks::StringFilterKind::EqualToOrLessThan,
        Compare::Gt => steamworks::StringFilterKind::GreaterThan,
        Compare::Ge => steamworks::StringFilterKind::EqualToOrGreaterThan,
    }
}

fn number_kind(c: Compare) -> steamworks::ComparisonFilter {
    match c {
        Compare::Eq => steamworks::ComparisonFilter::Equal,
        Compare::Ne => steamworks::ComparisonFilter::NotEqual,
        Compare::Lt => steamworks::ComparisonFilter::LessThan,
        Compare::Le => steamworks::ComparisonFilter::LessThanEqualTo,
        Compare::Gt => steamworks::ComparisonFilter::GreaterThan,
        Compare::Ge => steamworks::ComparisonFilter::GreaterThanEqualTo,
    }
}

fn distance(d: Distance) -> steamworks::DistanceFilter {
    match d {
        Distance::Close => steamworks::DistanceFilter::Close,
        Distance::Default => steamworks::DistanceFilter::Default,
        Distance::Far => steamworks::DistanceFilter::Far,
        Distance::Worldwide => steamworks::DistanceFilter::Worldwide,
    }
}

fn key(k: &str) -> Result<steamworks::LobbyKey<'_>, LobbyError> {
    steamworks::LobbyKey::try_new(k).map_err(|_| LobbyError::KeyInvalid)
}

impl RealLobbies {
    pub(crate) fn new(client: steamworks::Client) -> Self {
        let raw: Arc<Mutex<Vec<Raw>>> = Arc::default();
        let mut callbacks = Vec::new();
        let q = raw.clone();
        callbacks.push(client.register_callback(move |cb: steamworks::LobbyChatMsg| {
            lock(&q).push(Raw::Chat {
                lobby: cb.lobby.raw(),
                user: cb.user.raw(),
                chat_id: cb.chat_id,
            });
        }));
        let q = raw.clone();
        callbacks.push(
            client.register_callback(move |cb: steamworks::GameLobbyJoinRequested| {
                lock(&q).push(Raw::Invite {
                    lobby: cb.lobby_steam_id.raw(),
                    friend: cb.friend_steam_id.raw(),
                });
            }),
        );
        let q = raw.clone();
        callbacks.push(client.register_callback(move |cb: steamworks::LobbyDataUpdate| {
            if cb.success {
                lock(&q).push(Raw::Data {
                    lobby: cb.lobby.raw(),
                    member: cb.member.raw(),
                });
            }
        }));
        // `join_lobby`'s call result collapses every failure to `Err(())`; the LobbyEnter
        // callback carries the response code and the locked flag. It also fires for lobbies we
        // create ourselves — those are not in `pending` and are ignored here (the Created path
        // reports them).
        let pending: Arc<Mutex<BTreeMap<u64, RequestId>>> = Arc::default();
        let q = raw.clone();
        let joins = pending.clone();
        callbacks.push(client.register_callback(move |cb: steamworks::LobbyEnter| {
            let lobby = cb.lobby.raw();
            let Some(request) = lock(&joins).remove(&lobby) else {
                return;
            };
            lock(&q).push(Raw::Enter {
                request,
                lobby,
                blocked: cb.blocked,
                response: cb.chat_room_enter_response,
            });
        }));
        // Steam fills in a non-friend's persona name after the lobby join; a cached name read
        // before that is a placeholder, so a name change forgets it and the next refresh reads it.
        let names: Arc<Mutex<BTreeMap<LobbyId, Vec<LobbyMember>>>> = Arc::default();
        let cached = names.clone();
        callbacks.push(
            client.register_callback(move |cb: steamworks::PersonaStateChange| {
                if cb.flags.contains(steamworks::PersonaChange::NAME) {
                    crate::lobby::forget_member_name(&mut lock(&cached), MemberId(cb.steam_id.raw()));
                }
            }),
        );
        Self {
            client,
            raw,
            next_request: AtomicU64::new(0),
            joined: Mutex::new(BTreeMap::new()),
            member_keys: Mutex::new([keys::READY.to_owned()].into_iter().collect()),
            names,
            pending,
            _callbacks: callbacks,
        }
    }

    fn note_data(&self, lobby: LobbyId, member: u64) {
        lock(&self.raw).push(Raw::Data {
            lobby: lobby.0,
            member,
        });
    }

    fn request(&self) -> RequestId {
        RequestId(self.next_request.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn sid(id: u64) -> steamworks::SteamId {
        steamworks::SteamId::from_raw(id)
    }

    fn lid(id: LobbyId) -> steamworks::LobbyId {
        steamworks::LobbyId::from_raw(id.0)
    }

    fn me(&self) -> MemberId {
        MemberId(self.client.user().steam_id().raw())
    }

    /// The lobby's raw data pairs, at most `MAX_DATA_ENTRIES` indices.
    fn data_pairs(&self, lobby: LobbyId) -> Vec<(String, String)> {
        let mm = self.client.matchmaking();
        let cap = u32::try_from(MAX_DATA_ENTRIES).unwrap_or(u32::MAX);
        let n = mm.lobby_data_count(Self::lid(lobby)).min(cap);
        (0..n)
            .filter_map(|i| mm.lobby_data_by_index(Self::lid(lobby), i))
            .collect()
    }

    fn data_of(&self, lobby: LobbyId) -> BTreeMap<String, String> {
        self.data_pairs(lobby).into_iter().collect()
    }

    fn info(&self, lobby: LobbyId, with_members: bool) -> LobbyInfo {
        let mm = self.client.matchmaking();
        let l = Self::lid(lobby);
        let seen = lock(&self.joined).get(&lobby).cloned();
        let members = if with_members {
            let ids: Vec<MemberId> = mm
                .lobby_members(l)
                .into_iter()
                .map(|id| MemberId(id.raw()))
                .collect();
            let previous = lock(&self.names).remove(&lobby).unwrap_or_default();
            let member_keys = lock(&self.member_keys).clone();
            let friends = self.client.friends();
            let built = crate::lobby::rebuild_members(
                previous,
                &ids,
                true,
                |id| friends.get_friend(Self::sid(id.0)).name(),
                |id| {
                    member_keys
                        .iter()
                        .filter_map(|k| {
                            mm.get_lobby_member_data(l, Self::sid(id.0), k)
                                .filter(|v| !v.is_empty())
                                .map(|v| (k.clone(), v))
                        })
                        .collect()
                },
            );
            lock(&self.names).insert(lobby, built.clone());
            built
        } else {
            Vec::new()
        };
        lobby_info(
            LobbyFacts {
                id: lobby,
                owner: MemberId(mm.lobby_owner(l).raw()),
                visibility: seen.as_ref().map_or(LobbyVisibility::Public, |s| s.visibility),
                joinable: seen.as_ref().is_none_or(|s| s.joinable),
                max_members: mm.lobby_member_limit(l),
                member_count: mm.lobby_member_count(l),
                members,
            },
            self.data_pairs(lobby),
        )
    }

    fn owned(&self, lobby: LobbyId) -> Result<(), LobbyError> {
        if !lock(&self.joined).contains_key(&lobby) {
            return Err(LobbyError::NotInLobby);
        }
        if self.client.matchmaking().lobby_owner(Self::lid(lobby)).raw() != self.me().0 {
            return Err(LobbyError::NotOwner);
        }
        Ok(())
    }

    /// Records a successful enter (from the `LobbyEnter` callback, or from the collapsed call
    /// result when no callback detail arrived) and reports it.
    fn entered_lobby(&self, request: RequestId, id: u64, joinable: bool, out: &mut Vec<LobbyEvent>) {
        let lobby = LobbyId(id);
        let mm = self.client.matchmaking();
        let l = Self::lid(lobby);
        lock(&self.joined).insert(
            lobby,
            Seen {
                members: mm
                    .lobby_members(l)
                    .into_iter()
                    .map(|m| MemberId(m.raw()))
                    .collect(),
                owner: MemberId(mm.lobby_owner(l).raw()),
                server: mm
                    .lobby_data(l, keys::SERVER)
                    .and_then(|s| LobbyServer::from_key(&s)),
                // No binding exposes a lobby's type or joinable flag after the enter:
                // `m_bLocked` ("only invited users may join") is the joinable flag inverted,
                // and a locked, invite-only lobby reads as Private (never Public). The owner of
                // a created lobby overwrites both from the spec, and the owner's own
                // `set_joinable` keeps its copy fresh; guests cannot observe joinable changes
                // (a documented binding limit).
                visibility: crate::lobby::EnterResponse::entered_visibility(!joinable),
                joinable,
            },
        );
        out.push(LobbyEvent::Entered {
            request,
            result: Ok(lobby),
        });
    }

    /// Diffs a joined lobby against the last poll (`crate::lobby::membership_events`).
    fn diff(&self, lobby: LobbyId, out: &mut Vec<LobbyEvent>) {
        let mm = self.client.matchmaking();
        let l = Self::lid(lobby);
        let now = Membership {
            members: mm
                .lobby_members(l)
                .into_iter()
                .map(|m| MemberId(m.raw()))
                .collect(),
            owner: MemberId(mm.lobby_owner(l).raw()),
            server: mm
                .lobby_data(l, keys::SERVER)
                .and_then(|s| LobbyServer::from_key(&s)),
        };
        let me = self.me();
        let mut joined = lock(&self.joined);
        let Some(seen) = joined.get_mut(&lobby) else {
            return;
        };
        let before = Membership {
            members: std::mem::take(&mut seen.members),
            owner: seen.owner,
            server: seen.server,
        };
        let lost = membership_events(lobby, &before, &now, me, out);
        // No data this poll: keep what we saw.
        let keep = if lost.is_some() {
            crate::lobby::settle(&before, now)
        } else {
            before
        };
        seen.members = keep.members;
        seen.owner = keep.owner;
        seen.server = keep.server;
        if lost == Some(true) {
            joined.remove(&lobby);
            lock(&self.names).remove(&lobby);
        }
    }
}

impl LobbyBackend for RealLobbies {
    fn name(&self) -> &'static str {
        "steam"
    }

    fn local_member(&self) -> Option<MemberId> {
        Some(self.me())
    }

    fn create(&self, spec: &LobbySpec) -> Result<RequestId, LobbyError> {
        check_spec(spec)?;
        let request = self.request();
        let q = self.raw.clone();
        let spec2 = spec.clone();
        self.client
            .matchmaking()
            .create_lobby(lobby_type(spec.visibility), spec.max_members, move |r| {
                lock(&q).push(Raw::Created(
                    request,
                    r.map(|id| id.raw()).map_err(|e| e.to_string()),
                    spec2,
                ));
            });
        Ok(request)
    }

    fn join(&self, lobby: LobbyId) -> Result<RequestId, LobbyError> {
        let request = self.request();
        let q = self.raw.clone();
        lock(&self.pending).insert(lobby.0, request);
        self.client.matchmaking().join_lobby(Self::lid(lobby), move |r| {
            lock(&q).push(Raw::Entered(request, r.map(|id| id.raw())));
        });
        Ok(request)
    }

    fn leave(&self, lobby: LobbyId) {
        self.client.matchmaking().leave_lobby(Self::lid(lobby));
        lock(&self.names).remove(&lobby);
        if lock(&self.joined).remove(&lobby).is_some() {
            // Steam sends no callback for our own leave; report it like the local backend.
            lock(&self.raw).push(Raw::Data {
                lobby: lobby.0,
                member: u64::MAX,
            });
        }
    }

    fn request_list(&self, query: &LobbyQuery) -> Result<RequestId, LobbyError> {
        query.validate()?;
        let mut filter = steamworks::LobbyListFilter::default();
        let mut strings = Vec::new();
        for f in &query.strings {
            strings.push(steamworks::StringFilter(
                key(&f.key)?,
                &f.value,
                string_kind(f.op),
            ));
        }
        let mut numbers = Vec::new();
        for f in &query.numbers {
            numbers.push(steamworks::NumberFilter(key(&f.key)?, f.value, number_kind(f.op)));
        }
        let mut near = Vec::new();
        for f in &query.near {
            near.push(steamworks::NearFilter(key(&f.key)?, f.value));
        }
        filter.string = (!strings.is_empty()).then_some(strings);
        filter.number = (!numbers.is_empty()).then_some(numbers);
        filter.near_value = (!near.is_empty()).then_some(near);
        filter.open_slots = query.open_slots.map(|n| n.min(u32::from(u8::MAX)) as u8);
        filter.distance = Some(distance(query.distance));
        filter.count = Some(u64::from(query.max_results));
        let request = self.request();
        let q = self.raw.clone();
        let mm = self.client.matchmaking();
        mm.set_lobby_list_filter(filter);
        mm.request_lobby_list(move |r| {
            lock(&q).push(Raw::List(
                request,
                r.map(|ids| ids.into_iter().map(|id| id.raw()).collect())
                    .map_err(|e| e.to_string()),
            ));
        });
        Ok(request)
    }

    fn lobby(&self, lobby: LobbyId) -> Option<LobbyInfo> {
        if !lock(&self.joined).contains_key(&lobby) {
            return None;
        }
        Some(self.info(lobby, true))
    }

    fn still_in(&self, lobby: LobbyId) -> bool {
        lock(&self.joined).contains_key(&lobby)
    }

    fn set_data(&self, lobby: LobbyId, k: &str, value: Option<&str>) -> Result<(), LobbyError> {
        check_key(k)?;
        if let Some(v) = value {
            check_value(v)?;
        }
        self.owned(lobby)?;
        let mm = self.client.matchmaking();
        let ok = match value {
            Some(v) => {
                if mm.lobby_data(Self::lid(lobby), k).is_none() && self.data_of(lobby).len() >= MAX_KEYS {
                    return Err(LobbyError::TooManyKeys);
                }
                mm.set_lobby_data(Self::lid(lobby), k, v)
            }
            None => mm.delete_lobby_data(Self::lid(lobby), k),
        };
        if ok || value.is_none() {
            // The owner's write is visible immediately. Queue the same signal the
            // LobbyDataUpdate callback sends so the next poll refreshes the snapshot.
            self.note_data(lobby, lobby.0);
            Ok(())
        } else {
            Err(LobbyError::Platform("SetLobbyData refused".into()))
        }
    }

    fn set_member_data(&self, lobby: LobbyId, k: &str, value: Option<&str>) -> Result<(), LobbyError> {
        check_key(k)?;
        if let Some(v) = value {
            check_value(v)?;
        }
        if !lock(&self.joined).contains_key(&lobby) {
            return Err(LobbyError::NotInLobby);
        }
        {
            let mut member_keys = lock(&self.member_keys);
            if !member_keys.contains(k) {
                if member_keys.len() >= MAX_KEYS {
                    return Err(LobbyError::TooManyKeys);
                }
                member_keys.insert(k.to_owned());
            }
        }
        // Steam has no delete for member data: an empty value reads as unset.
        self.client
            .matchmaking()
            .set_lobby_member_data(Self::lid(lobby), k, value.unwrap_or(""));
        self.note_data(lobby, self.me().0);
        Ok(())
    }

    fn set_joinable(&self, lobby: LobbyId, joinable: bool) -> Result<(), LobbyError> {
        self.owned(lobby)?;
        if !self
            .client
            .matchmaking()
            .set_lobby_joinable(Self::lid(lobby), joinable)
        {
            return Err(LobbyError::Platform("SetLobbyJoinable refused".into()));
        }
        if let Some(seen) = lock(&self.joined).get_mut(&lobby) {
            seen.joinable = joinable;
        }
        self.note_data(lobby, lobby.0);
        Ok(())
    }

    fn set_server(&self, lobby: LobbyId, server: LobbyServer) -> Result<(), LobbyError> {
        self.owned(lobby)?;
        let mm = self.client.matchmaking();
        if !mm.set_lobby_data(Self::lid(lobby), keys::SERVER, &server.to_key()) {
            return Err(LobbyError::Platform("SetLobbyData refused".into()));
        }
        // Also fill Steam's own game-server field, which the overlay and friends list read.
        match server {
            LobbyServer::Address(SocketAddr::V4(v4)) => mm.set_lobby_game_server(Self::lid(lobby), v4, None),
            LobbyServer::Steam(id) => mm.set_lobby_game_server(
                Self::lid(lobby),
                SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0),
                Some(Self::sid(id)),
            ),
            LobbyServer::Address(SocketAddr::V6(_)) => {}
        }
        Ok(())
    }

    /// Deletes the `regia.server` key and writes Steam's "no game server" values (address
    /// 0.0.0.0, port 0, no Steam id) over the `SetLobbyGameServer` field [`Self::set_server`]
    /// filled, so a rolled-back launch leaves no dead server in the overlay or friends list
    /// (KI-NT-050, ADR-10069). Steam has no call that removes the field.
    fn clear_server(&self, lobby: LobbyId) -> Result<(), LobbyError> {
        self.owned(lobby)?;
        let mm = self.client.matchmaking();
        // A delete of a key that is not there answers false; `set_data(.., None)` treats it as
        // done too.
        let _ = mm.delete_lobby_data(Self::lid(lobby), keys::SERVER);
        mm.set_lobby_game_server(
            Self::lid(lobby),
            SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0),
            None,
        );
        self.note_data(lobby, lobby.0);
        Ok(())
    }

    fn send_chat(&self, lobby: LobbyId, text: &str) -> Result<(), LobbyError> {
        check_chat(text)?;
        if !lock(&self.joined).contains_key(&lobby) {
            return Err(LobbyError::NotInLobby);
        }
        self.client
            .matchmaking()
            .send_lobby_chat_message(Self::lid(lobby), text.as_bytes())
            .map_err(|e| LobbyError::Platform(e.to_string()))
    }

    fn poll(&self) -> Vec<LobbyEvent> {
        let raw = std::mem::take(&mut *lock(&self.raw));
        let mut out = Vec::new();
        // Report every join exactly once. Both the collapsed `join_lobby` call result
        // (`Raw::Entered`) and the `LobbyEnter` callback (`Raw::Enter`) fire in the same
        // run_callbacks round (an IO failure fires only the call result); the callback carries
        // the response code, so it decides whenever it is present.
        let mut done: BTreeSet<RequestId> = BTreeSet::new();
        for r in &raw {
            if let Raw::Enter {
                request,
                lobby,
                blocked,
                response,
            } = r
            {
                if !done.insert(*request) {
                    continue;
                }
                match enter_response(*response).join_error() {
                    None => self.entered_lobby(*request, *lobby, !*blocked, &mut out),
                    Some(error) => out.push(LobbyEvent::Entered {
                        request: *request,
                        result: Err(error),
                    }),
                }
            }
        }
        for r in raw {
            match r {
                Raw::Enter { .. } => {}
                Raw::Created(request, result, spec) => match result {
                    Ok(id) => {
                        let lobby = LobbyId(id);
                        let mm = self.client.matchmaking();
                        // Finding 1 (deep-zai net-lobby): Steam refuses SetLobbyData silently
                        // (rate limits, per-key limits surface only as `false`). A refused engine
                        // key or joinable flag must fail the create — the lobby would otherwise be
                        // invisible to this build's matchmaking, or joinable against the spec,
                        // with the owner never told. A refused non-engine key is logged; the
                        // lobby works without it.
                        let writes: Vec<(String, bool)> = spec
                            .data
                            .iter()
                            .map(|(k, v)| (k.clone(), mm.set_lobby_data(Self::lid(lobby), k, v)))
                            .collect();
                        let joinable_ok =
                            (!spec.joinable).then(|| mm.set_lobby_joinable(Self::lid(lobby), false));
                        match crate::lobby::created_verdict(&writes, joinable_ok) {
                            Ok(refused) => {
                                for key in &refused {
                                    tracing::warn!(target: "regia_steam", lobby = %lobby.0, key = %key, "SetLobbyData refused on create; the key is missing");
                                }
                                lock(&self.joined).insert(
                                    lobby,
                                    Seen {
                                        members: vec![self.me()],
                                        owner: self.me(),
                                        server: None,
                                        visibility: spec.visibility,
                                        joinable: spec.joinable,
                                    },
                                );
                                out.push(LobbyEvent::Created {
                                    request,
                                    result: Ok(lobby),
                                });
                            }
                            Err(reason) => {
                                // The created lobby would be broken: report the create failed
                                // and leave it, so no ghost lobby advertises to others.
                                mm.leave_lobby(Self::lid(lobby));
                                out.push(LobbyEvent::Created {
                                    request,
                                    result: Err(LobbyError::Platform(reason)),
                                });
                            }
                        }
                    }
                    Err(e) => out.push(LobbyEvent::Created {
                        request,
                        result: Err(LobbyError::Platform(e)),
                    }),
                },
                Raw::Entered(request, result) => {
                    if !done.insert(request) {
                        // The LobbyEnter callback of the same join already reported it.
                        continue;
                    }
                    match result {
                        Ok(id) => {
                            lock(&self.pending).remove(&id);
                            self.entered_lobby(request, id, true, &mut out);
                        }
                        // No callback detail (an IO failure fires no LobbyEnter): say so instead
                        // of guessing "the lobby is full".
                        Err(()) => out.push(LobbyEvent::Entered {
                            request,
                            result: Err(LobbyError::Platform(
                                "Steam did not answer the lobby join (IO failure)".into(),
                            )),
                        }),
                    }
                }
                Raw::List(request, result) => out.push(LobbyEvent::List {
                    request,
                    result: result
                        .map(|ids| ids.into_iter().map(|id| self.info(LobbyId(id), false)).collect())
                        .map_err(LobbyError::Platform),
                }),
                Raw::Chat { lobby, user, chat_id } => {
                    let mut buf = vec![0u8; regia_net::lobby::MAX_CHAT_BYTES];
                    let bytes = self.client.matchmaking().get_lobby_chat_entry(
                        steamworks::LobbyId::from_raw(lobby),
                        chat_id,
                        &mut buf,
                    );
                    let text = String::from_utf8_lossy(bytes).trim_end_matches('\0').to_owned();
                    out.push(LobbyEvent::Chat {
                        lobby: LobbyId(lobby),
                        from: MemberId(user),
                        text,
                    });
                }
                Raw::Invite { lobby, friend } => out.push(LobbyEvent::InviteAccepted {
                    lobby: LobbyId(lobby),
                    from: MemberId(friend),
                }),
                Raw::Data { lobby, member } if member == u64::MAX => out.push(LobbyEvent::Left {
                    lobby: LobbyId(lobby),
                    reason: LeaveReason::Left,
                }),
                Raw::Data { lobby, member } => out.push(LobbyEvent::DataChanged {
                    lobby: LobbyId(lobby),
                    member: (member != lobby).then_some(MemberId(member)),
                }),
            }
        }
        let ids: Vec<LobbyId> = lock(&self.joined).keys().copied().collect();
        for lobby in ids {
            self.diff(lobby, &mut out);
        }
        out
    }
}

/// Peer-to-peer messages over ISteamNetworkingMessages.
pub(crate) struct RealP2p {
    client: steamworks::Client,
    accept: Arc<Mutex<SteamAccept>>,
}

impl RealP2p {
    pub(crate) fn new(client: steamworks::Client) -> Self {
        let accept: Arc<Mutex<SteamAccept>> = Arc::default();
        let policy = accept.clone();
        client
            .networking_messages()
            .session_request_callback(move |request| {
                let allowed = request
                    .remote()
                    .steam_id()
                    .is_some_and(|id| lock(&policy).allows(SteamId(id.raw())));
                if allowed {
                    request.accept();
                } else {
                    request.reject();
                }
            });
        Self { client, accept }
    }
}

impl P2pMessages for RealP2p {
    fn local_id(&self) -> Option<SteamId> {
        Some(SteamId(self.client.user().steam_id().raw()))
    }

    fn send(&self, to: SteamId, channel: u16, bytes: &[u8]) -> Result<(), SteamError> {
        self.client
            .networking_messages()
            .send_message_to_user(
                NetworkingIdentity::new_steam_id(steamworks::SteamId::from_raw(to.0)),
                SendFlags::UNRELIABLE_NO_NAGLE | SendFlags::AUTO_RESTART_BROKEN_SESSION,
                bytes,
                u32::from(channel),
            )
            .map_err(|e| SteamError::Rejected(e.to_string()))
    }

    fn receive(&self, channel: u16, max: usize) -> Vec<(SteamId, Vec<u8>)> {
        let mut scratch = Vec::new();
        self.receive_into(channel, max, &mut scratch)
            .into_iter()
            .scan(0usize, |start, (id, len)| {
                let end = *start + len;
                let bytes = scratch[*start..end].to_vec();
                *start = end;
                Some((id, bytes))
            })
            .collect()
    }

    /// Skips a message longer than `MAX_DATAGRAM_BYTES` without copying it (spec 120 § 10.8).
    fn receive_into(&self, channel: u16, max: usize, scratch: &mut Vec<u8>) -> Vec<(SteamId, usize)> {
        self.receive_bounded(channel, max, MAX_DATAGRAM_BYTES, scratch)
            .into_iter()
            .filter_map(|(id, len)| Some((id, len?)))
            .collect()
    }

    fn receive_bounded(
        &self,
        channel: u16,
        max: usize,
        max_len: usize,
        scratch: &mut Vec<u8>,
    ) -> Vec<(SteamId, Option<usize>)> {
        self.client
            .networking_messages()
            .receive_messages_on_channel(u32::from(channel), max)
            .into_iter()
            .filter_map(|m| {
                let from = m.identity_peer().steam_id()?;
                let data = m.data();
                // A long message stays in Steam's buffer and is released with `m` (spec 120
                // § 10.8, adversarial row 15): never copied.
                let kept = (data.len() <= max_len).then(|| {
                    scratch.extend_from_slice(data);
                    data.len()
                });
                Some((SteamId(from.raw()), kept))
            })
            .collect()
    }

    fn set_accept(&self, accept: SteamAccept) {
        *lock(&self.accept) = accept;
    }
}

#[cfg(test)]
mod tests {
    //! The query mapping onto the bindings' filter types (spec 24 § 2 "list with filters"). Pure
    //! functions: no Steam client is needed, only the `steamworks` feature that compiles them.
    use super::*;

    /// Steam reads every comparison as "the lobby's value <op> the filter value"
    /// (`ELobbyComparison`, Steamworks ISteamMatchmaking docs), the same side `regia_net`'s
    /// local hub compares on (`LobbyQuery::matches`), so each `Compare` maps to the same-named
    /// kind and never to its mirror.
    #[test]
    fn every_comparison_maps_to_the_same_sided_steam_filter() {
        use steamworks::{ComparisonFilter as N, StringFilterKind as S};
        let table = [
            (Compare::Eq, S::Equal, N::Equal),
            (Compare::Ne, S::NotEqual, N::NotEqual),
            (Compare::Lt, S::LessThan, N::LessThan),
            (Compare::Le, S::EqualToOrLessThan, N::LessThanEqualTo),
            (Compare::Gt, S::GreaterThan, N::GreaterThan),
            (Compare::Ge, S::EqualToOrGreaterThan, N::GreaterThanEqualTo),
        ];
        for (compare, string, number) in table {
            assert_eq!(string_kind(compare), string, "{compare:?} on strings");
            assert_eq!(number_kind(compare), number, "{compare:?} on numbers");
        }
    }

    #[test]
    fn every_distance_maps_to_its_steam_radius() {
        use steamworks::DistanceFilter as D;
        let table = [
            (Distance::Close, D::Close),
            (Distance::Default, D::Default),
            (Distance::Far, D::Far),
            (Distance::Worldwide, D::Worldwide),
        ];
        for (ours, steam) in table {
            assert_eq!(distance(ours), steam, "{ours:?}");
        }
    }

    /// A key longer than Steam's `k_nMaxLobbyKeyLength` (255) is `KeyInvalid` before the
    /// bindings see it; 255 bytes still pass.
    #[test]
    fn a_key_over_the_steam_limit_is_invalid() {
        assert!(key(&"k".repeat(255)).is_ok());
        assert!(matches!(key(&"k".repeat(256)), Err(LobbyError::KeyInvalid)));
    }
}
