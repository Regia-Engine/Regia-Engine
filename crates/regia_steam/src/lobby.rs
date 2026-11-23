// SPDX-License-Identifier: MPL-2.0
//! Steam lobbies and the Steam sockets transport wired into `regia_net`'s lobby layer
//! (spec 89, ADR-1877).
//!
//! [`SteamLobbyPlugin`] adds `regia_net::LobbyPlugin` (when it is not there yet) and a
//! `PreUpdate` step that, once Steam is ready, gives `regia_net::Lobbies` the Steam lobby backend
//! (ISteamMatchmaking) and a [`SteamLobbyTransport`] (ISteamNetworkingMessages), so
//! `lobby.launch` hosts the session on a [`crate::SteamSocket`] and members join by Steam id.
//! Without Steam (feature off, client not running, Android) nothing is attached and every lobby
//! call reports `lobby_unavailable`.

use std::collections::BTreeSet;
use std::sync::Arc;

use regia_app::prelude::*;
use regia_net::lobby::{Lobbies, LobbyBackend, LobbyPlugin, UnavailableLobbies, lobby_system};

use crate::backend::{SteamId, SteamState};
use crate::resource::Steam;
use crate::sockets::{SteamAccept, SteamLobbyTransport};

/// The lobby backend of the running Steam client, or an unavailable one.
pub fn steam_lobbies(steam: &Steam) -> Arc<dyn LobbyBackend> {
    if steam.state != SteamState::Ready {
        return Arc::new(UnavailableLobbies);
    }
    steam
        .backend
        .lobbies()
        .unwrap_or_else(|| Arc::new(UnavailableLobbies))
}

/// Wires Steam lobbies and sockets into `regia_net::Lobbies` (spec 89 § 7-8). Add it after
/// [`crate::SteamPlugin`].
#[derive(Debug, Clone, Default)]
pub struct SteamLobbyPlugin {
    /// Accept peer-to-peer sessions only from members of the current lobby (default: anyone;
    /// the session handshake authenticates either way).
    pub members_only: bool,
}

/// Whether the Steam backend is attached, and the member set last given to the accept policy.
#[derive(Resource, Default)]
struct SteamLobbyState {
    /// The lobby backend the game's `Lobbies` poll — `None` before the attach and after a
    /// detach. Identified by pointer: `Steam::shutdown` drops the cached backend and a
    /// re-initialized client offers a new one, which must replace the dead attachment.
    attached: Option<Arc<dyn LobbyBackend>>,
    members_only: bool,
    accepted: Option<BTreeSet<SteamId>>,
}

impl std::fmt::Debug for SteamLobbyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn LobbyBackend` is not `Debug`: name the attached backend instead.
        f.debug_struct("SteamLobbyState")
            .field("attached", &self.attached.as_ref().map(|backend| backend.name()))
            .field("members_only", &self.members_only)
            .field("accepted", &self.accepted)
            .finish()
    }
}

/// Steam sockets channel from `lobby.steam_channel` (`docs/formats/net-ron.md`).
///
/// The session channel is a `u16` (`0..=65535`). A value above 65535 does not fit and is
/// channel 0, the same channel an omitted setting uses.
pub(crate) fn steam_socket_channel(steam_channel: u32) -> u16 {
    u16::try_from(steam_channel).unwrap_or(0)
}

/// The verdict on a created lobby's initial writes (finding 1, deep-zai net-lobby): Steam
/// refuses `SetLobbyData`/`SetLobbyJoinable` silently (rate limits, per-key limits surface only
/// as `false`), and the create must not report `Ok` when a refused write leaves the lobby broken.
/// `writes` is each spec key with whether Steam accepted it; `joinable_ok` is the
/// `SetLobbyJoinable` result for a deliberately non-joinable create (`None` when not needed).
///
/// `Err` when an engine key (`regia.*`: the matchmaking filters, the state the launch and the
/// late-join `Launched` shortcut read) or the joinable flag was refused: the lobby would be
/// invisible to this build's matchmaking, or joinable against the spec. `Ok` carries the refused
/// non-engine keys — the lobby works without them, but the absence is logged.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn created_verdict(
    writes: &[(String, bool)],
    joinable_ok: Option<bool>,
) -> Result<Vec<String>, String> {
    let mut warned = Vec::new();
    for (key, ok) in writes {
        if *ok {
            continue;
        }
        if regia_net::lobby::keys::ALL.contains(&key.as_str()) {
            return Err(format!("SetLobbyData refused for `{key}`"));
        }
        warned.push(key.clone());
    }
    if joinable_ok == Some(false) {
        return Err("SetLobbyJoinable refused".to_owned());
    }
    Ok(warned)
}

fn attach_steam_lobbies(
    steam: Option<Res<Steam>>,
    mut lobbies: ResMut<Lobbies>,
    mut state: ResMut<SteamLobbyState>,
) {
    let Some(steam) = steam else {
        return;
    };
    // The backend the running client offers now: `None` when Steam is down.
    // `Steam::shutdown` drops the cached lobbies/messages (real.rs), and a re-initialized
    // client offers new ones — an attachment to the old client would silently poll a dead
    // backend (every call `Ok`, no event ever arriving), so the attachment follows the client.
    let offered: Option<Arc<dyn LobbyBackend>> = if steam.state == SteamState::Ready {
        steam.backend.lobbies()
    } else {
        None
    };
    let stale = match (&state.attached, &offered) {
        (Some(have), Some(now)) => !Arc::ptr_eq(have, now),
        (Some(_), None) => true,
        (None, _) => false,
    };
    if stale {
        // Steam went away or a new client replaced the one we attached to: detach, so lobby
        // calls report `lobby_unavailable` instead of polling the dead backend.
        lobbies.set_backend(Arc::new(UnavailableLobbies));
        state.attached = None;
        state.accepted = None;
        tracing::info!(target: "regia_steam", "Steam lobbies detached (Steam shut down or restarted)");
    }
    if state.attached.is_none()
        && let Some(backend) = offered
    {
        lobbies.set_backend(backend.clone());
        if let Some(messages) = steam.backend.p2p() {
            let channel = steam_socket_channel(lobbies.settings().steam_channel);
            lobbies.set_transport(Arc::new(SteamLobbyTransport::new(messages, channel)));
        }
        state.attached = Some(backend);
        tracing::info!(target: "regia_steam", "Steam lobbies and sockets attached");
    }
    if state.attached.is_some() && state.members_only {
        let members: BTreeSet<SteamId> = lobbies
            .snapshot()
            .map(|s| s.members.iter().map(|m| SteamId(m.id.0)).collect())
            .unwrap_or_default();
        if state.accepted.as_ref() != Some(&members) {
            if let Some(messages) = steam.backend.p2p() {
                messages.set_accept(SteamAccept::Listed(members.clone()));
            }
            state.accepted = Some(members);
        }
    }
}

impl Plugin for SteamLobbyPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<LobbyPlugin>()) {
            app.add_plugins(LobbyPlugin);
        }
        app.insert_resource(SteamLobbyState {
            members_only: self.members_only,
            ..SteamLobbyState::default()
        });
        app.add_systems(
            PreUpdate,
            attach_steam_lobbies
                .after(crate::pump::pump_steam)
                .before(lobby_system),
        );
    }
}

/// Why entering a Steam lobby failed, mirrored from `steamworks::ChatRoomEnterResponse` so the
/// mapping to [`regia_net::lobby::LobbyError`] is testable without the `steamworks` feature.
/// The bindings collapse every failed join to `Result<LobbyId, ()>`; the `LobbyEnter` callback
/// carries the actual code.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnterResponse {
    /// Joined.
    Success,
    /// The lobby no longer exists.
    DoesntExist,
    /// Joining is not allowed (locked, or the lobby is full of invites only).
    NotAllowed,
    /// The lobby is full.
    Full,
    /// An unspecified Steam-side error.
    Error,
    /// The local user is banned from the lobby.
    Banned,
    /// The local user cannot join lobbies (limited account).
    Limited,
    /// Clan lobbies are disabled.
    ClanDisabled,
    /// The local user is community-banned.
    CommunityBan,
    /// A member blocked the local user.
    MemberBlockedYou,
    /// The local user blocked a member.
    YouBlockedMember,
    /// The join was rate-limited; retry later.
    RatelimitExceeded,
}

#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
impl EnterResponse {
    /// The game-facing error for a failed enter (`None` = success). Full stays
    /// [`regia_net::lobby::LobbyError::Full`], a missing lobby becomes
    /// [`regia_net::lobby::LobbyError::NotFound`], every other refusal is platform-flavoured —
    /// not everything is "the lobby is full".
    pub(crate) fn join_error(self) -> Option<regia_net::lobby::LobbyError> {
        use regia_net::lobby::LobbyError;
        match self {
            Self::Success => None,
            Self::DoesntExist => Some(LobbyError::NotFound),
            Self::Full => Some(LobbyError::Full),
            other => Some(LobbyError::Platform(format!(
                "the lobby platform refused the join ({other:?})"
            ))),
        }
    }

    /// The visibility a freshly entered lobby is reported with, from `LobbyEnter_t.m_bLocked`
    /// ("only invited users may join"): a locked lobby is invite-only, which no listed lobby can
    /// be. The bindings expose no getter for a lobby's type, so an unlocked lobby reads as
    /// [`regia_net::lobby::LobbyVisibility::Public`].
    pub(crate) fn entered_visibility(blocked: bool) -> regia_net::lobby::LobbyVisibility {
        use regia_net::lobby::LobbyVisibility;
        if blocked {
            LobbyVisibility::Private
        } else {
            LobbyVisibility::Public
        }
    }
}

/// What one poll saw of a joined Steam lobby (ADR-1877 (4): membership is found by diffing).
// Used by the real client only (and its tests here, which run without the SDK).
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Membership {
    pub(crate) members: Vec<regia_net::lobby::MemberId>,
    pub(crate) owner: regia_net::lobby::MemberId,
    pub(crate) server: Option<regia_net::lobby::LobbyServer>,
}

/// What to remember after a poll: a server key that reads empty once (a missed `GetLobbyData`)
/// does not forget the one we saw, so it is not announced twice when it comes back.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn settle(before: &Membership, mut now: Membership) -> Membership {
    if now.owner.0 == 0 {
        now.owner = before.owner;
    }
    if now.server.is_none() {
        now.server = before.server;
    }
    now
}

/// The events between two polls of joined lobby `lobby`, appended to `out`, and whether `me` is
/// no longer in it (then the caller forgets the lobby; a `Left { kicked }` is appended). `None`
/// when `now` lists no members: a lobby we are in always lists us, so that is "no data yet", and
/// the caller keeps `before` for the next poll.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn membership_events(
    lobby: regia_net::lobby::LobbyId,
    before: &Membership,
    now: &Membership,
    me: regia_net::lobby::MemberId,
    out: &mut Vec<regia_net::lobby::LobbyEvent>,
) -> Option<bool> {
    use regia_net::lobby::{LeaveReason, LobbyEvent};
    if now.members.is_empty() {
        return None;
    }
    // Sets make the diff linear; the events keep list order.
    let before_set: std::collections::HashSet<_> = before.members.iter().copied().collect();
    let now_set: std::collections::HashSet<_> = now.members.iter().copied().collect();
    for m in &now.members {
        if !before_set.contains(m) {
            out.push(LobbyEvent::MemberJoined { lobby, member: *m });
        }
    }
    for m in &before.members {
        if !now_set.contains(m) {
            out.push(LobbyEvent::MemberLeft { lobby, member: *m });
        }
    }
    if now.owner != before.owner && now.owner.0 != 0 {
        out.push(LobbyEvent::OwnerChanged {
            lobby,
            owner: now.owner,
        });
    }
    if now.server != before.server
        && let Some(server) = now.server
    {
        out.push(LobbyEvent::ServerSet { lobby, server });
    }
    // Only a lobby we were seen in can be lost (the list right after entering may be empty).
    let lost = !before.members.is_empty() && !now_set.contains(&me);
    if lost {
        out.push(LobbyEvent::Left {
            lobby,
            reason: LeaveReason::Kicked,
        });
    }
    Some(lost)
}

/// Drops `member` from every cached member list, so the next [`rebuild_members`] fetches its
/// persona name (and data) again. Steam delivers a non-friend's persona name asynchronously after
/// a lobby join (`PersonaStateChange`); the name read on the first refresh can be empty or a
/// placeholder, and without this the cache kept it for the whole session.
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn forget_member_name(
    cache: &mut std::collections::BTreeMap<regia_net::lobby::LobbyId, Vec<regia_net::lobby::LobbyMember>>,
    member: regia_net::lobby::MemberId,
) {
    for members in cache.values_mut() {
        members.retain(|m| m.id != member);
    }
}

/// Rebuilds a lobby's member list. Names already known for an id are moved across; persona
/// names are fetched only for ids that were not in `previous`. Member data is fetched only when
/// `refresh_data` is set (a quiet poll passes `false` and returns `previous` unchanged when the
/// ids match).
#[cfg(any(test, all(feature = "steamworks", not(target_os = "android"))))]
pub(crate) fn rebuild_members(
    mut previous: Vec<regia_net::lobby::LobbyMember>,
    ids: &[regia_net::lobby::MemberId],
    refresh_data: bool,
    mut fetch_name: impl FnMut(regia_net::lobby::MemberId) -> String,
    mut fetch_data: impl FnMut(regia_net::lobby::MemberId) -> std::collections::BTreeMap<String, String>,
) -> Vec<regia_net::lobby::LobbyMember> {
    if !refresh_data
        && previous.len() == ids.len()
        && previous.iter().zip(ids).all(|(member, id)| member.id == *id)
    {
        return previous;
    }
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(pos) = previous.iter().position(|member| member.id == *id) {
            let mut old = previous.swap_remove(pos);
            if refresh_data {
                old.data = fetch_data(*id);
            }
            out.push(old);
        } else {
            out.push(regia_net::lobby::LobbyMember {
                id: *id,
                name: fetch_name(*id),
                data: fetch_data(*id),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use regia_net::lobby::{
        LeaveReason, LobbyError, LobbyEvent, LobbyId, LobbyMember, LobbyServer, LobbyVisibility, MemberId,
    };

    use super::{EnterResponse, Membership, forget_member_name, membership_events, rebuild_members, settle};
    use crate::backend::SteamId;

    /// A [`crate::Steam`] resource over the given backend, the way `SteamPlugin` builds it.
    fn steam_resource(backend: Arc<dyn crate::backend::SteamBackend>) -> crate::resource::Steam {
        crate::resource::Steam {
            backend,
            state: crate::backend::SteamState::Ready,
            achievements: Arc::new(crate::achievements::AchievementsConfig::default()),
            thresholds: Arc::new(crate::achievements::Thresholds::default()),
            events: Arc::new(crate::backend::EventQueue::default()),
            overlay_open: false,
            deck: false,
            paused_by_overlay: false,
            announced: Arc::default(),
            known_unlocked: Arc::default(),
            timeline: Arc::default(),
        }
    }

    /// Finding 3 (deep-zai net-lobby): the attach latched on the first ready backend forever.
    /// `Steam::shutdown` drops the cached lobbies and a re-initialized client offers new ones;
    /// without a re-attach the game's `Lobbies` keeps polling the dead client (every call `Ok`,
    /// no event ever arriving). The attachment must follow the client Steam currently offers.
    #[test]
    fn attach_recovers_when_steam_restarts() {
        use regia_app::prelude::*;

        let net1 = crate::mock_net::MockSteamNet::new();
        let net2 = crate::mock_net::MockSteamNet::new();
        let mock = crate::mock::MockSteam::shared();
        mock.join_net(&net1, SteamId(1), "one");
        let backend: Arc<dyn crate::backend::SteamBackend> = mock.clone();

        let mut app = App::new();
        app.insert_resource(steam_resource(backend));
        let mut lobbies = regia_net::lobby::Lobbies::new(Arc::new(regia_net::lobby::UnavailableLobbies));
        // Since ADR-7881 a create with no lobby identity answers `lobby_no_identity`; a game with
        // no `NetSession` names itself, as the net-game template does. The name survives every
        // re-attach below (`Lobbies::set_backend` keeps it).
        lobbies.set_game("steam-attach-test");
        app.insert_resource(lobbies);
        app.insert_resource(super::SteamLobbyState::default());
        app.add_systems(Update, super::attach_steam_lobbies);

        // The first ready client attaches; its lobbies land on the first network.
        app.update();
        {
            let mut lobbies = app.world_mut().resource_mut::<regia_net::lobby::Lobbies>();
            assert_eq!(lobbies.backend().name(), "local", "attached to the mock client");
            let spec = lobbies.default_spec();
            lobbies.create(spec).expect("create on net1");
            lobbies.pump();
        }
        assert_eq!(net1.hub().lobbies().len(), 1, "the lobby is on the first network");
        assert!(net2.hub().lobbies().is_empty());

        // Steam shuts down: the game's lobbies must detach (calls report `lobby_unavailable`),
        // not keep polling the dead client.
        app.world_mut()
            .resource_mut::<crate::resource::Steam>()
            .shutdown();
        app.update();
        {
            let mut lobbies = app.world_mut().resource_mut::<regia_net::lobby::Lobbies>();
            let spec = lobbies.default_spec();
            assert_eq!(
                lobbies.create(spec),
                Err(regia_net::lobby::LobbyError::Unavailable),
                "a shut-down Steam detaches the lobbies"
            );
        }

        // A new client initializes and joins a different network: the lobbies re-attach to it
        // and new lobbies land there.
        {
            let mut steam = app.world_mut().resource_mut::<crate::resource::Steam>();
            let state = steam.backend.initialize(None).expect("re-init");
            steam.state = state;
        }
        mock.join_net(&net2, SteamId(1), "one");
        app.update();
        {
            let mut lobbies = app.world_mut().resource_mut::<regia_net::lobby::Lobbies>();
            assert_eq!(lobbies.backend().name(), "local", "re-attached to the new client");
            let spec = lobbies.default_spec();
            lobbies.create(spec).expect("create on net2");
            lobbies.pump();
        }
        assert_eq!(
            net1.hub().lobbies().len(),
            0,
            "the detach left the old client's lobby (`Lobbies::set_backend` leaves the current \
             lobby), so nothing stays behind on the old network"
        );
        assert_eq!(
            net2.hub().lobbies().len(),
            1,
            "new lobbies land on the new client's network"
        );
    }

    fn member(id: u64, name: &str) -> LobbyMember {
        LobbyMember {
            id: MemberId(id),
            name: name.into(),
            data: BTreeMap::new(),
        }
    }

    #[test]
    fn unchanged_member_ids_reuse_names_and_skip_persona_fetches() {
        let names = Cell::new(0u32);
        let data_fetches = Cell::new(0u32);
        let ids = [MemberId(1), MemberId(2)];
        let previous = vec![member(1, "Ann"), member(2, "Bob")];
        let fetch_name = |_| {
            names.set(names.get() + 1);
            String::new()
        };
        let fetch_data = |_| {
            data_fetches.set(data_fetches.get() + 1);
            BTreeMap::from([("ready".into(), "1".into())])
        };
        let kept = rebuild_members(previous.clone(), &ids, false, fetch_name, fetch_data);
        assert_eq!(names.get(), 0, "a quiet id list fetches no persona names");
        assert_eq!(data_fetches.get(), 0, "a quiet id list fetches no member data");
        assert_eq!(kept, previous);

        let names = Cell::new(0u32);
        let data_fetches = Cell::new(0u32);
        let refreshed = rebuild_members(
            previous.clone(),
            &ids,
            true,
            |_| {
                names.set(names.get() + 1);
                "unused".into()
            },
            |_| {
                data_fetches.set(data_fetches.get() + 1);
                BTreeMap::from([("ready".into(), "1".into())])
            },
        );
        assert_eq!(names.get(), 0, "a data refresh keeps the names it already has");
        assert_eq!(data_fetches.get(), 2);
        assert_eq!(refreshed[0].name, "Ann");
        assert_eq!(refreshed[0].data.get("ready").map(String::as_str), Some("1"));

        let names = Cell::new(0u32);
        let data_fetches = Cell::new(0u32);
        let joined = rebuild_members(
            previous,
            &[MemberId(1), MemberId(2), MemberId(3)],
            false,
            |id| {
                names.set(names.get() + 1);
                assert_eq!(id, MemberId(3));
                "Cy".into()
            },
            |_| {
                data_fetches.set(data_fetches.get() + 1);
                BTreeMap::new()
            },
        );
        assert_eq!(names.get(), 1, "only the new id fetches a persona name");
        assert_eq!(data_fetches.get(), 1);
        assert_eq!(joined[0].name, "Ann");
        assert_eq!(joined[2].name, "Cy");
    }

    /// A persona name Steam delivers after the member was first listed must replace the cached
    /// one: the name read at the join can be a placeholder, and the cache reused it forever.
    #[test]
    fn a_persona_name_change_is_fetched_again() {
        let lobby = LobbyId(9);
        let ids = [MemberId(1), MemberId(2)];
        let mut cache = BTreeMap::from([(lobby, vec![member(1, "Ann"), member(2, "[unknown]")])]);
        forget_member_name(&mut cache, MemberId(2));
        let previous = cache.remove(&lobby).unwrap_or_default();
        let fetched = Cell::new(0u32);
        let rebuilt = rebuild_members(
            previous,
            &ids,
            true,
            |id| {
                fetched.set(fetched.get() + 1);
                assert_eq!(id, MemberId(2), "only the changed member is fetched");
                "Bob".into()
            },
            |_| BTreeMap::new(),
        );
        assert_eq!(fetched.get(), 1);
        assert_eq!(rebuilt[0].name, "Ann");
        assert_eq!(rebuilt[1].name, "Bob");
    }

    fn seen(members: &[u64], owner: u64, server: Option<LobbyServer>) -> Membership {
        Membership {
            members: members.iter().map(|m| MemberId(*m)).collect(),
            owner: MemberId(owner),
            server,
        }
    }

    #[test]
    fn a_steam_channel_above_u16_is_channel_zero() {
        assert_eq!(super::steam_socket_channel(0), 0);
        assert_eq!(super::steam_socket_channel(65535), 65535);
        assert_eq!(super::steam_socket_channel(65536), 0);
        assert_eq!(super::steam_socket_channel(100_000), 0);
    }

    /// Finding 1 (deep-zai net-lobby): a refused engine key (matchmaking filters, state) or a
    /// refused joinable flag must fail the create — the lobby would otherwise be invisible to
    /// this build's matchmaking, or joinable against the spec, with the owner never told. A
    /// refused non-engine key is a warning, not a failure.
    #[test]
    fn a_refused_engine_key_fails_the_create() {
        use regia_net::lobby::keys;

        let game = keys::GAME.to_owned();
        let all_ok = vec![
            (keys::VERSION.to_owned(), true),
            (game.clone(), true),
            (keys::PROTOCOL.to_owned(), true),
            (keys::STATE.to_owned(), true),
        ];
        assert_eq!(super::created_verdict(&all_ok, Some(true)), Ok(Vec::new()));
        assert_eq!(super::created_verdict(&all_ok, None), Ok(Vec::new()));

        // A refused engine key: the create fails, whichever key it was.
        for key in [
            keys::GAME,
            keys::PROTOCOL,
            keys::CONTENT,
            keys::STATE,
            keys::VERSION,
        ] {
            let writes = vec![(key.to_owned(), false)];
            let err = super::created_verdict(&writes, Some(true)).expect_err(key);
            assert!(err.contains("SetLobbyData refused"), "{err}");
        }

        // A refused joinable on a non-joinable spec: the create fails.
        let err = super::created_verdict(&all_ok, Some(false)).expect_err("joinable");
        assert!(err.contains("SetLobbyJoinable refused"), "{err}");

        // A refused game key (not an engine key): the create stands, the key is reported.
        let custom = "map".to_owned();
        assert_eq!(
            super::created_verdict(&[(custom.clone(), false)], Some(true)),
            Ok(vec![custom]),
            "a missing non-engine key is a warning, not a failed create"
        );
    }

    #[test]
    fn join_response_codes_are_not_all_full() {
        // The table the real backend maps LobbyEnter_t.m_EChatRoomEnterResponse through: a
        // missing lobby, a ban, a lock and a rate limit must not all read as "lobby full".
        let cases = [
            (EnterResponse::Success, None),
            (EnterResponse::DoesntExist, Some(LobbyError::NotFound)),
            (EnterResponse::Full, Some(LobbyError::Full)),
            (
                EnterResponse::NotAllowed,
                Some(LobbyError::Platform(String::new())),
            ),
            (EnterResponse::Error, Some(LobbyError::Platform(String::new()))),
            (EnterResponse::Banned, Some(LobbyError::Platform(String::new()))),
            (EnterResponse::Limited, Some(LobbyError::Platform(String::new()))),
            (
                EnterResponse::ClanDisabled,
                Some(LobbyError::Platform(String::new())),
            ),
            (
                EnterResponse::CommunityBan,
                Some(LobbyError::Platform(String::new())),
            ),
            (
                EnterResponse::MemberBlockedYou,
                Some(LobbyError::Platform(String::new())),
            ),
            (
                EnterResponse::YouBlockedMember,
                Some(LobbyError::Platform(String::new())),
            ),
            (
                EnterResponse::RatelimitExceeded,
                Some(LobbyError::Platform(String::new())),
            ),
        ];
        for (response, expected) in cases {
            let error = response.join_error();
            assert_eq!(
                error.is_some(),
                expected.is_some(),
                "{response:?}: success and failure must not be confused"
            );
            match (error, expected) {
                (None, None) => {}
                (Some(LobbyError::Platform(_)), Some(LobbyError::Platform(_))) => {}
                (a, b) => assert_eq!(a, b, "{response:?}"),
            }
        }
    }

    #[test]
    fn a_locked_enter_is_not_reported_public_and_joinable() {
        // LobbyEnter_t.m_bLocked ("only invited users may join") is the joinable flag inverted;
        // a locked lobby is invite-only, so it must not compare equal to `Public` + joinable.
        let visibility = EnterResponse::entered_visibility(true);
        assert_ne!(
            (visibility, true),
            (LobbyVisibility::Public, true),
            "a private, non-joinable enter must not read as a public joinable lobby"
        );
        assert_eq!(visibility, LobbyVisibility::Private);
        assert_eq!(EnterResponse::entered_visibility(false), LobbyVisibility::Public);
    }

    #[test]
    fn a_missing_server_key_does_not_forget_the_server() {
        let server = LobbyServer::Steam(9);
        let before = seen(&[1, 2], 1, Some(server));
        let kept = settle(&before, seen(&[1, 2], 1, None));
        assert_eq!(kept.server, Some(server));
        let mut out = Vec::new();
        membership_events(
            LobbyId(5),
            &kept,
            &seen(&[1, 2], 1, Some(server)),
            MemberId(1),
            &mut out,
        );
        assert!(out.is_empty(), "the key coming back raises no second ServerSet");
        let other = LobbyServer::Steam(10);
        assert_eq!(settle(&before, seen(&[1], 1, Some(other))).server, Some(other));
    }

    #[test]
    fn membership_diff_raises_joins_leaves_owner_server_and_kicks() {
        let lobby = LobbyId(5);
        let me = MemberId(1);
        let mut out = Vec::new();
        let before = seen(&[1, 2], 2, None);
        // Nothing changed: no events.
        assert_eq!(
            membership_events(lobby, &before, &before, me, &mut out),
            Some(false)
        );
        assert!(out.is_empty());

        // 3 joins, 2 leaves, ownership passes to us, the owner launched.
        let now = seen(&[1, 3], 1, Some(LobbyServer::Steam(1)));
        assert_eq!(membership_events(lobby, &before, &now, me, &mut out), Some(false));
        assert_eq!(
            out,
            [
                LobbyEvent::MemberJoined {
                    lobby,
                    member: MemberId(3)
                },
                LobbyEvent::MemberLeft {
                    lobby,
                    member: MemberId(2)
                },
                LobbyEvent::OwnerChanged { lobby, owner: me },
                LobbyEvent::ServerSet {
                    lobby,
                    server: LobbyServer::Steam(1)
                },
            ]
        );

        // An empty list is "no data yet", not "everyone left": no events, not lost.
        out.clear();
        assert_eq!(
            membership_events(lobby, &now, &seen(&[], 0, None), me, &mut out),
            None
        );
        assert!(out.is_empty());

        // A server key cleared and an unknown owner (0) raise nothing.
        assert_eq!(
            membership_events(lobby, &now, &seen(&[1, 3], 0, None), me, &mut out),
            Some(false)
        );
        assert!(out.is_empty());

        // We are no longer listed: kicked.
        assert_eq!(
            membership_events(lobby, &now, &seen(&[3], 3, None), me, &mut out),
            Some(true)
        );
        assert_eq!(
            out.last(),
            Some(&LobbyEvent::Left {
                lobby,
                reason: LeaveReason::Kicked
            })
        );
    }

    #[test]
    fn membership_diff_handles_large_lists_in_list_order() {
        let lobby = LobbyId(9);
        let me = MemberId(1);
        let before: Vec<u64> = (1..=250).collect();
        let now: Vec<u64> = (126..=375).rev().chain(std::iter::once(1)).collect();
        let mut out = Vec::new();
        let r = membership_events(lobby, &seen(&before, 1, None), &seen(&now, 1, None), me, &mut out);
        assert_eq!(r, Some(false));
        let joined: Vec<u64> = out
            .iter()
            .filter_map(|e| match e {
                LobbyEvent::MemberJoined { member, .. } => Some(member.0),
                _ => None,
            })
            .collect();
        let left: Vec<u64> = out
            .iter()
            .filter_map(|e| match e {
                LobbyEvent::MemberLeft { member, .. } => Some(member.0),
                _ => None,
            })
            .collect();
        assert_eq!(joined, (251..=375).rev().collect::<Vec<_>>());
        assert_eq!(left, (2..=125).collect::<Vec<_>>());
    }

    #[derive(Default)]
    struct PollHistory {
        remembered: BTreeMap<LobbyId, Membership>,
        events: Vec<LobbyEvent>,
    }

    impl PollHistory {
        fn remember(&mut self, lobby: LobbyId, initial: Membership) {
            self.remembered.insert(lobby, initial);
        }

        // Matches RealLobbies::diff: temporarily take the prior membership, ignore empty
        // member polls, settle usable polls, and forget a lobby only after caller removal.
        fn poll(&mut self, lobby: LobbyId, snapshot: Membership, me: MemberId) {
            let Some(before) = self.remembered.remove(&lobby) else {
                return;
            };
            let mut events = Vec::new();
            let lost = membership_events(lobby, &before, &snapshot, me, &mut events);
            let keep = if lost.is_some() {
                settle(&before, snapshot)
            } else {
                before
            };
            self.events.extend(events);
            if lost != Some(true) {
                self.remembered.insert(lobby, keep);
            }
        }
    }

    #[test]
    fn caller_poll_history_keeps_membership_across_empty_and_missing_data() {
        let lobby = LobbyId(41);
        let me = MemberId(1);
        let initial_server = LobbyServer::Steam(700);
        let mut history = PollHistory::default();
        history.remember(lobby, seen(&[1, 2], 1, Some(initial_server)));

        // Stable membership and backend enumeration order changes are not transitions.
        history.poll(lobby, seen(&[1, 2], 1, Some(initial_server)), me);
        assert!(history.events.is_empty());
        assert_eq!(history.remembered[&lobby], seen(&[1, 2], 1, Some(initial_server)));
        history.poll(lobby, seen(&[2, 1], 1, Some(initial_server)), me);
        assert!(history.events.is_empty());
        assert_eq!(history.remembered[&lobby], seen(&[2, 1], 1, Some(initial_server)));

        // An empty no-data snapshot retains the last usable enumeration and server key.
        history.poll(lobby, seen(&[], 0, None), me);
        assert!(history.events.is_empty());
        assert_eq!(history.remembered[&lobby], seen(&[2, 1], 1, Some(initial_server)));

        // Recovery without a real change does not duplicate joins or ServerSet.
        history.poll(lobby, seen(&[1, 2], 1, None), me);
        assert!(history.events.is_empty());
        assert_eq!(history.remembered[&lobby], seen(&[1, 2], 1, Some(initial_server)));

        // A known owner transfer is emitted once; owner zero is unknown, not a transfer.
        history.poll(lobby, seen(&[1, 2], 2, Some(initial_server)), me);
        assert_eq!(
            history.events,
            [LobbyEvent::OwnerChanged {
                lobby,
                owner: MemberId(2)
            }]
        );
        history.events.clear();
        history.poll(lobby, seen(&[1, 2], 0, None), me);
        assert!(history.events.is_empty());
        assert_eq!(history.remembered[&lobby], seen(&[1, 2], 2, Some(initial_server)));

        // Missing server data is remembered as the prior key; recovery is silent, while a
        // genuinely different key is observable exactly once.
        history.poll(lobby, seen(&[1, 2], 2, None), me);
        assert!(history.events.is_empty());
        let changed_server = LobbyServer::Steam(701);
        history.poll(lobby, seen(&[1, 2], 2, Some(changed_server)), me);
        assert_eq!(
            history.events,
            [LobbyEvent::ServerSet {
                lobby,
                server: changed_server
            }]
        );
        assert_eq!(history.remembered[&lobby], seen(&[1, 2], 2, Some(changed_server)));

        // Caller removal is a single final Left and drops only this remembered lobby.
        history.events.clear();
        history.poll(lobby, seen(&[2], 2, Some(changed_server)), me);
        assert_eq!(
            history.events,
            [
                LobbyEvent::MemberLeft { lobby, member: me },
                LobbyEvent::Left {
                    lobby,
                    reason: LeaveReason::Kicked
                },
            ]
        );
        assert!(!history.remembered.contains_key(&lobby));
        history.poll(lobby, seen(&[2], 2, Some(changed_server)), me);
        assert_eq!(
            history.events,
            [
                LobbyEvent::MemberLeft { lobby, member: me },
                LobbyEvent::Left {
                    lobby,
                    reason: LeaveReason::Kicked
                },
            ]
        );
    }

    #[test]
    fn caller_poll_histories_for_interleaved_lobbies_are_isolated() {
        let first = LobbyId(51);
        let second = LobbyId(52);
        let me = MemberId(1);
        let server_a = LobbyServer::Steam(801);
        let server_b = LobbyServer::Steam(802);
        let mut history = PollHistory::default();
        history.remember(first, seen(&[1, 2], 1, Some(server_a)));
        history.remember(second, seen(&[1, 3], 1, Some(server_b)));

        history.poll(first, seen(&[], 0, None), me);
        history.poll(second, seen(&[3, 1], 1, None), me);
        assert!(history.events.is_empty());
        assert_eq!(history.remembered[&first], seen(&[1, 2], 1, Some(server_a)));
        assert_eq!(history.remembered[&second], seen(&[3, 1], 1, Some(server_b)));

        history.poll(second, seen(&[1, 3, 4], 3, Some(LobbyServer::Steam(803))), me);
        assert_eq!(
            history.events,
            [
                LobbyEvent::MemberJoined {
                    lobby: second,
                    member: MemberId(4)
                },
                LobbyEvent::OwnerChanged {
                    lobby: second,
                    owner: MemberId(3)
                },
                LobbyEvent::ServerSet {
                    lobby: second,
                    server: LobbyServer::Steam(803)
                },
            ]
        );
        assert_eq!(history.remembered[&first], seen(&[1, 2], 1, Some(server_a)));
        assert_eq!(
            history.remembered[&second],
            seen(&[1, 3, 4], 3, Some(LobbyServer::Steam(803)))
        );

        history.events.clear();
        history.poll(first, seen(&[1, 2, 5], 1, Some(server_a)), me);
        assert_eq!(
            history.events,
            [LobbyEvent::MemberJoined {
                lobby: first,
                member: MemberId(5)
            }]
        );
        assert_eq!(
            history.remembered[&second],
            seen(&[1, 3, 4], 3, Some(LobbyServer::Steam(803)))
        );
    }
}
