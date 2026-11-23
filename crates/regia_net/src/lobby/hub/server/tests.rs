// SPDX-License-Identifier: MPL-2.0
//! Unit tests of the state core. The cases are derived from DO27 `ir-hub` `hub.rs` and
//! `tests/server_cfg_limits.rs` as requirements (regia-absorb-4 = c9513623f); no line is copied.

use super::*;
use crate::lobby::LobbySpec;

fn hub() -> HubServer {
    HubServer::with_secret(HubSettings::default(), [3; 32])
}

fn spec(visibility: LobbyVisibility) -> LobbySpec {
    LobbySpec {
        visibility,
        ..LobbySpec::default()
    }
}

fn events_of(hub: &mut HubServer, m: MemberId) -> Vec<HubEvent> {
    hub.take_events(m)
}

/// A player key for tests (the transport's proof is `HubHost`'s; the core takes the proven key).
fn key(b: u8) -> PlayerKey {
    PlayerKey([b; 32])
}

/// What `join` answers for a private or friends-only lobby the caller may not enter: the missing
/// lobby's code (KI-NT-046, ADR-10069). The real reason is in the refusal counter.
const HIDDEN: HubError = HubError::Lobby(LobbyError::NotFound);

#[test]
fn handles_are_unique_case_insensitively_and_free_again_after_leaving() {
    let mut h = hub();
    let ann = h.connect("Ann", 0).expect("ann");
    assert_eq!(h.connect("ann", 0), Err(HubError::HandleTaken));
    assert_eq!(h.connect(" ANN ", 0), Err(HubError::HandleTaken));
    assert_eq!(h.connect("", 0), Err(HubError::HandleInvalid));
    assert_eq!(h.connect(&"x".repeat(25), 0), Err(HubError::HandleInvalid));
    h.disconnect(ann);
    assert!(h.connect("ann", 0).is_ok());
    assert_eq!(h.report().refused.get("hub_handle_taken"), Some(&2));
}

#[test]
fn the_session_cap_holds() {
    let mut h = HubServer::with_secret(
        HubSettings {
            session_cap: 2,
            ..HubSettings::default()
        },
        [0; 32],
    );
    h.connect("a", 0).expect("a");
    h.connect("b", 0).expect("b");
    assert_eq!(h.connect("c", 0), Err(HubError::SessionCap));
}

#[test]
fn private_lobbies_need_a_matching_unexpired_token() {
    let mut h = hub();
    let host = h.connect("host", 0).expect("host");
    let friend = h.connect("friend", 0).expect("friend");
    let other = h.connect("other", 0).expect("other");
    let lobby = h
        .create(host, &spec(LobbyVisibility::Private), 0)
        .expect("create");
    assert_eq!(h.join(friend, lobby, None, 10), Err(HIDDEN));
    let card = h.invite(host, lobby, "friend", 10).expect("invite");
    assert_eq!(card.from, host);
    assert!(matches!(events_of(&mut h, friend).last(), Some(HubEvent::Invite(c)) if *c == card));
    // Another session cannot use the friend's token.
    assert_eq!(h.join(other, lobby, Some(card.token), 20), Err(HIDDEN));
    // A forged token is refused.
    let forged = InviteToken([0; 16]);
    assert_eq!(h.join(friend, lobby, Some(forged), 20), Err(HIDDEN));
    // The real token works, once.
    h.join(friend, lobby, Some(card.token), 30).expect("join");
    h.leave(friend, lobby, 40);
    assert_eq!(h.join(friend, lobby, Some(card.token), 50), Err(HIDDEN));
    // The four refusals were invite-only ones (the answer hides it, the counter does not).
    assert_eq!(h.report().refused.get("hub_invite_only"), Some(&4));
}

#[test]
fn an_invite_expires_at_its_ttl_on_the_test_clock() {
    let mut h = HubServer::with_secret(
        HubSettings {
            invite_ttl_ms: 1_000,
            idle_ms: u64::MAX / 2,
            ..HubSettings::default()
        },
        [1; 32],
    );
    let host = h.connect("host", 0).expect("host");
    let guest = h.connect("guest", 0).expect("guest");
    let lobby = h
        .create(host, &spec(LobbyVisibility::Private), 0)
        .expect("create");
    let card = h.invite(host, lobby, "guest", 100).expect("invite");
    assert_eq!(card.expires_ms, 1_100);
    assert_eq!(h.join(guest, lobby, Some(card.token), 1_100), Err(HIDDEN));
    let card = h.invite(host, lobby, "guest", 2_000).expect("again");
    assert!(h.join(guest, lobby, Some(card.token), 2_999).is_ok());
    // Expiry drops invites from the table too.
    h.leave(guest, lobby, 3_000);
    h.invite(host, lobby, "guest", 3_000).expect("third");
    assert_eq!(h.report().invites, 1);
    h.expire(4_000);
    assert_eq!(h.report().invites, 0);
}

#[test]
fn friends_only_takes_the_owners_friends_or_an_invitee() {
    // ADR-8633: friends are player keys, not handles.
    let mut h = hub();
    let host = h.connect_player(key(1), "Host", 0).expect("host");
    let pal = h.connect_player(key(2), "Pal", 0).expect("pal");
    let stranger = h.connect_player(key(3), "Stranger", 0).expect("stranger");
    h.set_friends(host, &[key(2)], 0).expect("friends");
    let lobby = h
        .create(host, &spec(LobbyVisibility::FriendsOnly), 0)
        .expect("create");
    assert_eq!(h.join(stranger, lobby, None, 0), Err(HIDDEN));
    h.join(pal, lobby, None, 0).expect("a friend joins");
    let card = h.invite(host, lobby, "stranger", 0).expect("invite");
    h.join(stranger, lobby, Some(card.token), 0)
        .expect("an invitee joins");
    let too_many: Vec<PlayerKey> = (0..65u8).map(key).collect();
    assert_eq!(h.set_friends(host, &too_many, 0), Err(HubError::TooManyFriends));
}

#[test]
fn a_full_or_closed_lobby_refuses_and_public_lists_only_public() {
    let mut h = hub();
    let host = h.connect("host", 0).expect("host");
    let a = h.connect("a", 0).expect("a");
    let b = h.connect("b", 0).expect("b");
    let lobby = h
        .create(
            host,
            &LobbySpec {
                max_members: 2,
                ..LobbySpec::default()
            },
            0,
        )
        .expect("create");
    h.join(a, lobby, None, 0).expect("a joins");
    assert_eq!(h.join(b, lobby, None, 0), Err(LobbyError::Full.into()));
    h.leave(a, lobby, 0);
    h.set_joinable(host, lobby, false, 0).expect("close");
    assert_eq!(h.join(b, lobby, None, 0), Err(LobbyError::Full.into()));
    assert_eq!(h.lobbies().len(), 1);
    let other = h.connect("c", 0).expect("c");
    h.create(other, &spec(LobbyVisibility::Invisible), 0)
        .expect("invisible");
    assert_eq!(h.lobbies().len(), 1, "only public lobbies are listed");
    // Too many members for the hub's setting.
    let d = h.connect("d", 0).expect("d");
    let big = LobbySpec {
        max_members: 65,
        ..LobbySpec::default()
    };
    assert_eq!(h.create(d, &big, 0), Err(LobbyError::MembersInvalid.into()));
}

#[test]
fn the_lobby_cap_holds_at_the_setting() {
    let mut h = HubServer::with_secret(
        HubSettings {
            lobby_cap: 2,
            ..HubSettings::default()
        },
        [0; 32],
    );
    for i in 0..2 {
        let m = h.connect(&format!("h{i}"), 0).expect("host");
        h.create(m, &LobbySpec::default(), 0).expect("create");
    }
    let m = h.connect("late", 0).expect("late");
    assert_eq!(h.create(m, &LobbySpec::default(), 0), Err(HubError::LobbyCap));
}

#[test]
fn the_owner_leaving_hands_the_lobby_on_and_the_last_member_deletes_it() {
    let mut h = hub();
    let host = h.connect("host", 0).expect("host");
    let a = h.connect("a", 0).expect("a");
    let lobby = h.create(host, &LobbySpec::default(), 0).expect("create");
    h.join(a, lobby, None, 0).expect("join");
    h.take_events(a);
    h.disconnect(host);
    let ev = h.take_events(a);
    assert!(
        ev.contains(&HubEvent::MemberLeft { lobby, member: host }),
        "{ev:?}"
    );
    assert!(ev.contains(&HubEvent::OwnerChanged { lobby, owner: a }), "{ev:?}");
    assert_eq!(h.lobby_info(lobby).map(|l| l.owner), Some(a));
    h.leave(a, lobby, 0);
    assert!(h.lobby_info(lobby).is_none());
    assert!(h.lobbies().is_empty());
}

#[test]
fn the_session_address_goes_to_members_only_and_moves_presence() {
    let mut h = hub();
    let host = h.connect("host", 0).expect("host");
    let member = h.connect("member", 0).expect("member");
    let outsider = h.connect("outsider", 0).expect("outsider");
    let lobby = h.create(host, &LobbySpec::default(), 0).expect("create");
    h.join(member, lobby, None, 0).expect("join");
    for m in [host, member, outsider] {
        h.take_events(m);
    }
    let server = LobbyServer::Address("10.1.2.3:4000".parse().expect("addr"));
    assert_eq!(
        h.confirm_session(member, lobby, Some(server), 0),
        Err(LobbyError::NotOwner.into())
    );
    h.confirm_session(host, lobby, Some(server), 0).expect("confirm");
    h.flush();
    for m in [host, member] {
        assert!(h.take_events(m).contains(&HubEvent::ServerSet {
            lobby,
            server: Some(server)
        }));
    }
    let outside = h.take_events(outsider);
    for e in &outside {
        let bytes = e.to_bytes().expect("encodes");
        assert!(!bytes.windows(4).any(|w| w == [10, 1, 2, 3]), "{e:?}");
    }
    assert!(
        h.players()
            .iter()
            .filter(|r| r.member != outsider)
            .all(|r| r.presence == HubPresence::InGame)
    );
    assert!(!h.lobbies()[0].data.contains_key(keys::SERVER));
    // `regia.server` through set_data is the same confirm; a bad value is refused.
    assert_eq!(
        h.set_data(host, lobby, keys::SERVER, Some("tcp:nope"), 0),
        Err(LobbyError::ValueTooLong.into())
    );
    h.set_data(host, lobby, keys::SERVER, None, 0).expect("withdraw");
    assert_eq!(h.lobby_info(lobby).and_then(|l| l.server), None);
    assert!(
        h.players()
            .iter()
            .filter(|r| r.member != outsider)
            .all(|r| r.presence == HubPresence::Lobby)
    );
}

#[test]
fn chat_is_rate_limited_bounded_and_mutable_by_the_owner() {
    let mut h = hub();
    let host = h.connect("host", 0).expect("host");
    let a = h.connect("a", 0).expect("a");
    let lobby = h.create(host, &LobbySpec::default(), 0).expect("create");
    h.join(a, lobby, None, 0).expect("join");
    let scope = ChatScope::Lobby(lobby);
    for i in 0..4 {
        h.chat(a, scope, &format!("line {i}"), 100)
            .expect("within the limit");
    }
    assert_eq!(h.chat(a, scope, "fifth", 100), Err(HubError::ChatRateLimited));
    h.chat(a, scope, "next second", 1_100).expect("a new window");
    assert_eq!(
        h.chat(a, scope, &"x".repeat(161), 2_000),
        Err(HubError::ChatInvalid)
    );
    assert_eq!(h.chat(a, scope, "   ", 2_000), Err(HubError::ChatInvalid));
    assert_eq!(
        h.mute(a, lobby, host, true, 2_000),
        Err(LobbyError::NotOwner.into())
    );
    assert_eq!(
        h.mute(host, lobby, host, true, 2_000),
        Err(HubError::CannotMuteOwner)
    );
    h.mute(host, lobby, a, true, 2_000).expect("mute");
    assert_eq!(h.chat(a, scope, "hello?", 3_000), Err(HubError::Muted));
    // Global chat still works for a muted member.
    h.chat(a, ChatScope::Global, "lobby still works", 3_000)
        .expect("global");
    // History is bounded.
    for i in 0..300u64 {
        let _ = h.chat(host, ChatScope::Global, "spam", 10_000 + i * 1_000);
    }
    assert_eq!(h.global_chat.len(), 100);
}

#[test]
fn data_budgets_and_owner_rules_hold() {
    let mut h = hub();
    let host = h.connect("host", 0).expect("host");
    let a = h.connect("a", 0).expect("a");
    let lobby = h.create(host, &LobbySpec::default(), 0).expect("create");
    h.join(a, lobby, None, 0).expect("join");
    assert_eq!(
        h.set_data(a, lobby, "map", Some("x"), 0),
        Err(LobbyError::NotOwner.into())
    );
    h.set_data(host, lobby, "map", Some("harbor"), 0)
        .expect("owner sets");
    assert_eq!(
        h.set_data(host, lobby, "big", Some(&"v".repeat(8 * 1024)), 0),
        Err(HubError::DataTooLarge)
    );
    h.set_member_data(a, lobby, keys::READY, Some("1"), 0)
        .expect("ready");
    assert_eq!(
        h.set_member_data(a, lobby, "blob", Some(&"v".repeat(300)), 0),
        Err(HubError::DataTooLarge)
    );
    let info = h.lobby_info(lobby).expect("info");
    assert!(info.all_ready() || info.members.len() == 2);
    assert_eq!(info.data["map"], "harbor");
}

#[test]
fn a_slow_reader_is_dropped_at_its_outbox_cap() {
    let mut h = HubServer::with_secret(
        HubSettings {
            outbox_cap: 16,
            chat_per_sec: 1_000,
            ..HubSettings::default()
        },
        [0; 32],
    );
    let quiet = h.connect("quiet", 0).expect("quiet");
    let loud = h.connect("loud", 0).expect("loud");
    for i in 0..40 {
        let _ = h.chat(loud, ChatScope::Global, "x", i);
        h.take_events(loud);
    }
    assert_eq!(h.expire(100), vec![quiet]);
}

#[test]
fn soak_500_sessions_idle_bandwidth_is_zero_and_state_is_bounded() {
    let mut h = hub();
    let ids: Vec<MemberId> = (0..500)
        .map(|i| h.connect(&format!("p{i}"), 0).expect("connect"))
        .collect();
    for (i, id) in ids.iter().enumerate().take(100) {
        let mut s = LobbySpec::default();
        s.data.insert("mode".into(), format!("m{i}"));
        h.create(*id, &s, 0).expect("create");
    }
    h.flush();
    for id in &ids {
        h.take_events(*id);
    }
    let before = h.report().directory_bytes;
    h.flush();
    for id in &ids {
        assert!(h.take_events(*id).is_empty());
    }
    assert_eq!(
        h.report().directory_bytes,
        before,
        "idle directory bandwidth is zero"
    );
    // One change: one small delta per session.
    let joiner = ids[200];
    h.join(joiner, LobbyId(1), None, 0).expect("join");
    h.flush();
    let delta = h.take_events(ids[499]);
    assert_eq!(delta.len(), 1);
    let HubEvent::Directory(page) = &delta[0] else {
        panic!("{delta:?}");
    };
    assert!(!page.reset);
    assert_eq!(page.lobbies.len(), 1);
    assert_eq!(page.players.len(), 1);
    let state = h.state_bytes();
    assert!(
        state < 2_000_000,
        "state {state} bytes for 500 sessions and 100 lobbies"
    );
}

#[test]
fn a_large_snapshot_is_paged_and_only_the_last_page_is_done() {
    let mut h = HubServer::with_secret(
        HubSettings {
            session_cap: 4096,
            ..HubSettings::default()
        },
        [0; 32],
    );
    for i in 0..3000 {
        h.connect(&format!("player-{i:05}-name"), 0).expect("connect");
    }
    let watcher = h.connect("watcher", 0).expect("watcher");
    h.flush_member(watcher);
    let pages: Vec<DirectoryPage> = h
        .take_events(watcher)
        .into_iter()
        .filter_map(|e| match e {
            HubEvent::Directory(p) => Some(p),
            _ => None,
        })
        .collect();
    assert!(pages.len() > 1, "{} pages", pages.len());
    assert!(pages[0].reset && pages[1..].iter().all(|p| !p.reset));
    assert!(pages.last().is_some_and(|p| p.done) && pages[..pages.len() - 1].iter().all(|p| !p.done));
    assert_eq!(pages.iter().map(|p| p.players.len()).sum::<usize>(), 3001);
    for p in &pages {
        let len = HubEvent::Directory(p.clone()).wire_len();
        assert!(len <= MAX_HUB_PAGE_TEST_BYTES, "page of {len} bytes");
    }
}

/// A page may run one row past the 32 KiB aim.
const MAX_HUB_PAGE_TEST_BYTES: usize = 40 * 1024;

#[test]
fn the_same_calls_give_the_same_state() {
    let run = || {
        let mut h = hub();
        let a = h.connect("a", 0).expect("a");
        let b = h.connect("b", 5).expect("b");
        let l = h.create(a, &spec(LobbyVisibility::Private), 10).expect("create");
        let card = h.invite(a, l, "b", 20).expect("invite");
        h.join(b, l, Some(card.token), 30).expect("join");
        h.flush();
        (
            h.take_events(a),
            h.take_events(b),
            h.generations(),
            h.state_bytes(),
        )
    };
    assert_eq!(run(), run());
}

// ---- Proof tests (lobby-hub-core review) -------------------------------------------------------
// These pin holes in 2bbaf9f03. The three that assert the safe behavior fail on that head; the
// friends-only one records the behavior ADR-8559 ships. Production code is unchanged.

/// Finding 4 (ADR-8633): the review's proof `proof_a_reused_handle_is_admitted_to_friends_only`
/// recorded that friends-only trusted the handle string, so the next session to claim a freed
/// handle was admitted. Friends are player keys now: the handle reuser (with another key, or with
/// none) is refused, and the real friend's key is admitted under any handle and member id.
#[test]
fn a_reused_handle_does_not_inherit_friends_only_access() {
    let mut h = hub();
    let host = h.connect_player(key(1), "Host", 0).expect("host");
    let pal = h.connect_player(key(2), "Pal", 0).expect("pal");
    h.set_friends(host, &[key(2)], 0).expect("friends");
    let lobby = h
        .create(host, &spec(LobbyVisibility::FriendsOnly), 0)
        .expect("create");
    h.disconnect(pal);
    // The review's own case: a key-less session claims the freed handle.
    let eve = h.connect("Pal", 1).expect("the handle is free as a display name");
    assert_ne!(eve, pal, "member ids are not reused");
    assert_eq!(h.join(eve, lobby, None, 1), Err(HIDDEN));
    h.disconnect(eve);
    // Another player's key under the same handle.
    let mallory = h.connect_player(key(9), "Pal", 2).expect("mallory");
    assert_eq!(h.join(mallory, lobby, None, 2), Err(HIDDEN));
    h.disconnect(mallory);
    // The friend's key comes back under another handle: admitted.
    let back = h.connect_player(key(2), "Pally", 3).expect("pal again");
    h.join(back, lobby, None, 3)
        .expect("the friend's key is admitted");
}

/// The confirming owner leaving must withdraw the game address. A player who joins afterwards
/// must not be handed that address, and the members who stayed must leave the in-game presence.
/// On 2bbaf9f03 `leave_inner` keeps `room.server`, the late joiner's `Room` carries it, and the
/// heir stays `InGame`.
#[test]
fn the_confirmed_address_is_withdrawn_when_its_owner_disconnects() {
    let mut h = hub();
    let host = h.connect("host", 0).expect("host");
    let heir = h.connect("heir", 0).expect("heir");
    let late = h.connect("late", 0).expect("late");
    let lobby = h.create(host, &LobbySpec::default(), 0).expect("create");
    h.join(heir, lobby, None, 0).expect("heir joins");
    let server = LobbyServer::Address("10.1.2.3:4000".parse().expect("addr"));
    h.confirm_session(host, lobby, Some(server), 0).expect("confirm");
    h.disconnect(host);
    let kept = h.lobby_info(lobby).and_then(|l| l.server);
    h.join(late, lobby, None, 0)
        .expect("late joins the lobby the host left");
    let handed = events_of(&mut h, late).into_iter().find_map(|e| match e {
        HubEvent::Room(room) => Some(room.server),
        _ => None,
    });
    let heir_presence = h.players().iter().find(|p| p.member == heir).map(|p| p.presence);
    assert_eq!(
        (kept, handed, heir_presence),
        (None, Some(None), Some(HubPresence::Lobby)),
        "host disconnect left the game address in place for a player who joins afterwards"
    );
}

/// A burst past `outbox_cap` sets a sticky `overflowed` flag. Draining the outbox does not clear
/// it, so the next `expire` drops a session that has already read. `HubHost::update` also expires
/// before it drains, so the same burst drops the session in that update.
#[test]
fn a_session_that_drained_a_burst_is_not_expired() {
    // `sanitized` raises `outbox_cap` to at least 16, so the burst has to clear that.
    let mut h = HubServer::with_secret(
        HubSettings {
            outbox_cap: 16,
            ..HubSettings::default()
        },
        [3; 32],
    );
    let host = h.connect("host", 1_000).expect("host");
    let a = h.connect("a", 1_000).expect("a");
    let b = h.connect("b", 1_000).expect("b");
    let lobby = h.create(host, &LobbySpec::default(), 1_000).expect("create");
    h.join(a, lobby, None, 1_000).expect("a joins");
    h.join(b, lobby, None, 1_000).expect("b joins");
    for m in [host, a, b] {
        h.take_events(m);
    }
    for i in 0..20 {
        h.set_member_data(a, lobby, "k", Some(&format!("a{i}")), 1_000)
            .expect("a writes");
        h.set_member_data(b, lobby, "k", Some(&format!("b{i}")), 1_000)
            .expect("b writes");
    }
    let _ = h.take_events(host);
    let expired = h.expire(1_000);
    assert!(
        !expired.contains(&host),
        "host was dropped for a burst it had already read: {expired:?}"
    );
}

/// `note_cursor` accepts a generation behind the one already sent. Each such list rebuilds a full
/// snapshot once the change log has moved on. A second rewind in the same second must not.
#[test]
fn a_rewound_list_cursor_does_not_rebuild_the_snapshot() {
    let mut h = HubServer::with_secret(
        HubSettings {
            changelog: 2,
            ..HubSettings::default()
        },
        [3; 32],
    );
    let watcher = h.connect("watcher", 0).expect("watcher");
    h.flush_member(watcher);
    h.take_events(watcher);
    for i in 0..3 {
        let m = h.connect(&format!("h{i}"), 0).expect("host");
        let id = h.create(m, &LobbySpec::default(), 0).expect("create");
        h.leave(m, id, 0);
        h.disconnect(m);
    }
    h.note_cursor(watcher, 0, 0);
    h.flush_member(watcher);
    let first = events_of(&mut h, watcher);
    assert!(
        first
            .iter()
            .any(|e| matches!(e, HubEvent::Directory(p) if p.reset)),
        "the setup did not force a snapshot: {first:?}"
    );
    h.note_cursor(watcher, 0, 0);
    h.flush_member(watcher);
    let second = events_of(&mut h, watcher);
    assert!(
        second.iter().all(|e| !matches!(e, HubEvent::Directory(_))),
        "rewinding the list cursor rebuilt the directory: {second:?}"
    );
}

// ---- Fixes (lobby-hub-fixes, ADR-8633) -------------------------------------------------------

/// Finding 1: members who stayed get the withdrawal, the new owner alone may confirm the next
/// address, and that confirm is checked like any other lobby data.
#[test]
fn after_the_owner_leaves_only_the_new_owner_confirms_and_it_is_checked() {
    let mut h = HubServer::with_secret(
        HubSettings {
            lobby_data_bytes: 64,
            ..HubSettings::default()
        },
        [3; 32],
    );
    let host = h.connect("host", 0).expect("host");
    let heir = h.connect("heir", 0).expect("heir");
    let other = h.connect("other", 0).expect("other");
    let lobby = h.create(host, &LobbySpec::default(), 0).expect("create");
    h.join(heir, lobby, None, 0).expect("heir");
    h.join(other, lobby, None, 0).expect("other");
    let first = LobbyServer::Address("10.1.2.3:4000".parse().expect("addr"));
    h.confirm_session(host, lobby, Some(first), 0).expect("confirm");
    for m in [heir, other] {
        h.take_events(m);
    }
    h.disconnect(host);
    for m in [heir, other] {
        let ev = h.take_events(m);
        assert!(
            ev.contains(&HubEvent::ServerSet { lobby, server: None }),
            "{m:?} was not told the address went: {ev:?}"
        );
    }
    let in_game = h
        .players()
        .iter()
        .filter(|p| p.presence == HubPresence::InGame)
        .count();
    assert_eq!(in_game, 0, "an in-game mark outlived the address");
    let second = LobbyServer::Address("10.9.9.9:5000".parse().expect("addr"));
    assert_eq!(
        h.confirm_session(other, lobby, Some(second), 0),
        Err(LobbyError::NotOwner.into())
    );
    // The confirm is lobby data: over the byte budget it is refused and nothing changes.
    h.set_data(heir, lobby, "pad", Some(&"x".repeat(50)), 0)
        .expect("pad");
    assert_eq!(
        h.confirm_session(heir, lobby, Some(second), 0),
        Err(HubError::DataTooLarge)
    );
    assert_eq!(h.lobby_info(lobby).and_then(|l| l.server), None);
    h.set_data(heir, lobby, "pad", None, 0).expect("unpad");
    h.confirm_session(heir, lobby, Some(second), 0)
        .expect("the new owner confirms its own game");
    assert_eq!(h.lobby_info(lobby).and_then(|l| l.server), Some(second));
}

/// Finding 2: the drain after a burst ends with a resync (the room as it stands), so a session
/// that read is whole again and is not expired; one that never reads still is.
#[test]
fn a_drained_burst_resyncs_the_room_and_a_session_that_never_reads_is_still_dropped() {
    let mut h = HubServer::with_secret(
        HubSettings {
            outbox_cap: 16,
            ..HubSettings::default()
        },
        [3; 32],
    );
    let host = h.connect("host", 1_000).expect("host");
    let a = h.connect("a", 1_000).expect("a");
    let sleeper = h.connect("sleeper", 1_000).expect("sleeper");
    let lobby = h.create(host, &LobbySpec::default(), 1_000).expect("create");
    h.join(a, lobby, None, 1_000).expect("a joins");
    h.join(sleeper, lobby, None, 1_000).expect("sleeper joins");
    h.take_events(host);
    for i in 0..20 {
        h.set_member_data(a, lobby, "k", Some(&format!("a{i}")), 1_000)
            .expect("a writes");
    }
    let drained = h.take_events(host);
    assert_eq!(
        drained.len(),
        17,
        "16 kept events and the resync room: {drained:?}"
    );
    let Some(HubEvent::Room(room)) = drained.last() else {
        panic!("the drain did not end with the room: {drained:?}");
    };
    let a_data = room
        .members
        .iter()
        .find(|(id, _, _)| *id == a)
        .map(|(_, _, d)| d.get("k").cloned());
    assert_eq!(
        a_data,
        Some(Some("a19".to_owned())),
        "the resync carries the latest data"
    );
    let expired = h.expire(1_000);
    assert!(!expired.contains(&host), "{expired:?}");
    assert!(
        expired.contains(&sleeper),
        "a session that never reads is a slow reader: {expired:?}"
    );
}

/// Finding 2: a drain that carried a resync and could not be sent (the host puts it back) marks a
/// session that cannot keep up; the next expiry drops it.
#[test]
fn a_resync_the_link_cannot_take_drops_the_session() {
    let mut h = HubServer::with_secret(
        HubSettings {
            outbox_cap: 16,
            ..HubSettings::default()
        },
        [3; 32],
    );
    let host = h.connect("host", 0).expect("host");
    let a = h.connect("a", 0).expect("a");
    let lobby = h.create(host, &LobbySpec::default(), 0).expect("create");
    h.join(a, lobby, None, 0).expect("a");
    h.take_events(host);
    for i in 0..20 {
        h.set_member_data(a, lobby, "k", Some(&i.to_string()), 0)
            .expect("write");
    }
    let drained = h.take_events(host);
    h.requeue(host, drained);
    assert!(h.expire(0).contains(&host));
    // Without an overflow, back-pressure alone only defers the events.
    let b = h.connect("b", 0).expect("b");
    let lobby_b = h.create(b, &LobbySpec::default(), 0).expect("create b");
    let c = h.connect("c", 0).expect("c");
    h.join(c, lobby_b, None, 0).expect("c");
    h.take_events(b);
    h.set_member_data(c, lobby_b, "k", Some("v"), 0).expect("write");
    let one = h.take_events(b);
    assert_eq!(one.len(), 1, "{one:?}");
    h.requeue(b, one.clone());
    assert!(!h.expire(0).contains(&b));
    assert_eq!(h.take_events(b), one, "the deferred event is sent next, in order");
}

/// Finding 2: lobby writes are rate limited per session and per lobby; a second later they are
/// admitted again, and a write that changes nothing costs nothing.
#[test]
fn lobby_writes_are_rate_limited_per_session_and_per_lobby() {
    let mut h = HubServer::with_secret(
        HubSettings {
            data_per_sec: 3,
            lobby_data_per_sec: 5,
            ..HubSettings::default()
        },
        [3; 32],
    );
    let host = h.connect("host", 0).expect("host");
    let a = h.connect("a", 0).expect("a");
    let b = h.connect("b", 0).expect("b");
    let lobby = h.create(host, &LobbySpec::default(), 0).expect("create");
    h.join(a, lobby, None, 0).expect("a");
    h.join(b, lobby, None, 0).expect("b");
    for i in 0..3 {
        h.set_member_data(a, lobby, "k", Some(&i.to_string()), 100)
            .expect("under the session rate");
    }
    assert_eq!(
        h.set_member_data(a, lobby, "k", Some("x"), 100),
        Err(HubError::DataRateLimited)
    );
    // A write that changes nothing is not counted.
    h.set_member_data(a, lobby, "k", Some("2"), 100).expect("a no-op");
    h.set_member_data(b, lobby, "k", Some("0"), 100).expect("b 1");
    h.set_data(host, lobby, "mode", Some("duel"), 100)
        .expect("host 1");
    assert_eq!(
        h.set_member_data(b, lobby, "k", Some("1"), 100),
        Err(HubError::DataRateLimited),
        "the lobby's rate is shared by its members"
    );
    assert_eq!(
        h.set_joinable(host, lobby, false, 100),
        Err(HubError::DataRateLimited)
    );
    h.set_member_data(a, lobby, "k", Some("later"), 1_100)
        .expect("a second later");
    assert_eq!(h.report().refused.get("hub_data_rate_limited"), Some(&3));
    // Two members writing as fast as they may cannot fill the host's default outbox in a second.
    let mut h = hub();
    let host = h.connect("host", 0).expect("host");
    let lobby = h.create(host, &LobbySpec::default(), 0).expect("create");
    let members: Vec<MemberId> = (0..2)
        .map(|i| {
            let m = h.connect(&format!("m{i}"), 0).expect("m");
            h.join(m, lobby, None, 0).expect("join");
            m
        })
        .collect();
    h.take_events(host);
    let mut admitted = 0;
    for i in 0..1_000 {
        for m in &members {
            if h.set_member_data(*m, lobby, "k", Some(&i.to_string()), 0).is_ok() {
                admitted += 1;
            }
        }
    }
    assert_eq!(admitted, 2 * HubSettings::default().data_per_sec);
    assert!(h.take_events(host).len() < HubSettings::default().outbox_cap);
    assert!(h.expire(0).is_empty());
}

/// Finding 3: a session may rewind its cursor once per window; a flood of rewinds builds one
/// snapshot, and after the window a rewind is taken again.
#[test]
fn a_flood_of_rewinds_builds_one_snapshot_per_window() {
    let mut h = HubServer::with_secret(
        HubSettings {
            changelog: 2,
            ..HubSettings::default()
        },
        [3; 32],
    );
    let watcher = h.connect("watcher", 0).expect("watcher");
    h.flush_member(watcher);
    h.take_events(watcher);
    for i in 0..3 {
        let m = h.connect(&format!("h{i}"), 0).expect("host");
        let id = h.create(m, &LobbySpec::default(), 0).expect("create");
        h.leave(m, id, 0);
        h.disconnect(m);
    }
    let mut snapshots = 0;
    for t in 0..1_000u64 {
        h.heartbeat(watcher, t).expect("alive");
        h.note_cursor(watcher, 0, 0);
        h.flush_member(watcher);
        snapshots += h
            .take_events(watcher)
            .iter()
            .filter(|e| matches!(e, HubEvent::Directory(p) if p.reset))
            .count();
    }
    assert_eq!(snapshots, 1);
    assert_eq!(h.report().refused.get("hub_list_rewind"), Some(&999));
    h.heartbeat(watcher, 10_000).expect("alive");
    h.note_cursor(watcher, 0, 0);
    h.flush_member(watcher);
    assert!(
        h.take_events(watcher)
            .iter()
            .any(|e| matches!(e, HubEvent::Directory(p) if p.reset)),
        "a rewind after the window is taken"
    );
    // A cursor that does not move backwards is always taken.
    let (d, p) = h.generations();
    h.note_cursor(watcher, d, p);
    assert_eq!(h.report().refused.get("hub_list_rewind"), Some(&999));
}

/// Finding 4: an invite to a session with a player key is bound to the key: it outlives the
/// session, another key (or none) under the same handle cannot redeem it, and a friends-only
/// lobby refuses an invitee with no key.
#[test]
fn invites_key_on_the_player_and_friends_only_fails_closed_without_one() {
    let mut h = hub();
    let host = h.connect_player(key(1), "host", 0).expect("host");
    let guest = h.connect_player(key(2), "guest", 0).expect("guest");
    let lobby = h
        .create(host, &spec(LobbyVisibility::FriendsOnly), 0)
        .expect("create");
    let card = h.invite(host, lobby, "guest", 0).expect("invite");
    h.disconnect(guest);
    assert_eq!(h.report().invites, 1, "a key-bound invite outlives the session");
    let eve = h.connect_player(key(9), "guest", 1).expect("eve");
    assert_eq!(h.join(eve, lobby, Some(card.token), 1), Err(HIDDEN));
    h.disconnect(eve);
    let guest = h.connect_player(key(2), "guest", 2).expect("guest again");
    // The resync after a gap re-sends the card.
    h.resync(guest);
    assert!(
        h.take_events(guest)
            .iter()
            .any(|e| matches!(e, HubEvent::Invite(c) if c.token == card.token))
    );
    h.join(guest, lobby, Some(card.token), 2)
        .expect("the invited key redeems it");
    // A session with no key: invited, still refused by a friends-only lobby (fail closed).
    let anon = h.connect("anon", 3).expect("anon");
    let card = h.invite(host, lobby, "anon", 3).expect("invite anon");
    assert_eq!(h.join(anon, lobby, Some(card.token), 3), Err(HIDDEN));
    // A private lobby still takes a key-less invitee by its member-bound token.
    let other = h.connect("other", 3).expect("other");
    let private = h
        .create(other, &spec(LobbyVisibility::Private), 3)
        .expect("private");
    let card = h.invite(other, private, "anon", 3).expect("invite");
    h.join(anon, private, Some(card.token), 3)
        .expect("member-bound invite");
}

/// Nit: at the invite cap, the lobby holding the most invites loses its oldest; another lobby's
/// invite is not evicted.
#[test]
fn invite_eviction_takes_from_the_lobby_holding_the_most() {
    let mut h = HubServer::with_secret(
        HubSettings {
            invite_cap: 4,
            ..HubSettings::default()
        },
        [3; 32],
    );
    let flooder = h.connect("flooder", 0).expect("flooder");
    let quiet = h.connect("quiet", 0).expect("quiet");
    let big = h
        .create(flooder, &spec(LobbyVisibility::Private), 0)
        .expect("big");
    let small = h
        .create(quiet, &spec(LobbyVisibility::Private), 0)
        .expect("small");
    let targets: Vec<String> = (0..6).map(|i| format!("t{i}")).collect();
    let ids: Vec<MemberId> = targets.iter().map(|t| h.connect(t, 0).expect("target")).collect();
    let kept = h.invite(quiet, small, "t5", 0).expect("quiet invites");
    for t in &targets[..5] {
        h.invite(flooder, big, t, 0).expect("flood");
    }
    assert_eq!(h.report().invites, 4);
    h.join(ids[5], small, Some(kept.token), 0)
        .expect("the quiet lobby's invite survived the flood");
}

/// The hub as a whole takes a bounded rate of global chat (every session receives every line).
#[test]
fn global_chat_has_a_hub_wide_rate() {
    let mut h = HubServer::with_secret(
        HubSettings {
            global_chat_per_sec: 3,
            ..HubSettings::default()
        },
        [3; 32],
    );
    let talkers: Vec<MemberId> = (0..5)
        .map(|i| h.connect(&format!("t{i}"), 0).expect("talker"))
        .collect();
    let ok = talkers
        .iter()
        .filter(|t| h.chat(**t, ChatScope::Global, "hi", 0).is_ok())
        .count();
    assert_eq!(ok, 3);
    assert!(h.chat(talkers[4], ChatScope::Global, "hi", 1_000).is_ok());
}

/// One session flooding creates, lists and rewinds, and many sessions creating at once: the
/// hub's state, lobbies and directory traffic stay inside their caps.
#[test]
fn a_create_and_list_flood_stays_inside_the_caps() {
    let settings = HubSettings::default();
    let mut h = hub();
    let watcher = h.connect("watcher", 0).expect("watcher");
    let flooder = h.connect("flooder", 0).expect("flooder");
    h.flush();
    h.take_events(watcher);
    h.take_events(flooder);
    // One connection: create, leave, list with a rewound cursor, 10 000 times in 10 s.
    for i in 0..10_000u64 {
        let t = i;
        if let Ok(id) = h.create(flooder, &LobbySpec::default(), t) {
            h.leave(flooder, id, t);
        }
        h.note_cursor(flooder, 0, 0);
        h.flush_member(flooder);
        h.take_events(flooder);
        h.flush();
        h.take_events(watcher);
    }
    assert!(
        h.lobbies().is_empty(),
        "one connection does not accumulate lobbies"
    );
    let one = h.report();
    assert_eq!(
        one.refused.get("hub_list_rewind").copied(),
        Some(10_000 - 1),
        "one rewind snapshot in the 10 s"
    );
    // Many connections: every session creates; the lobby cap holds.
    let mut made = 0;
    let mut capped = 0;
    for i in 0..(settings.session_cap - 2) {
        let m = h.connect(&format!("s{i}"), 20_000).expect("session");
        match h.create(m, &LobbySpec::default(), 20_000) {
            Ok(_) => made += 1,
            Err(HubError::LobbyCap) => capped += 1,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!(made, settings.lobby_cap);
    assert_eq!(capped, settings.session_cap - 2 - settings.lobby_cap);
    assert_eq!(h.connect("one-more", 20_000), Err(HubError::SessionCap));
    // Every session lists at once: each gets one snapshot, and the snapshot fits the outbox.
    h.flush();
    let ids: Vec<MemberId> = h.players().iter().map(|p| p.member).collect();
    for id in &ids {
        let ev = h.take_events(*id);
        assert!(ev.len() < settings.outbox_cap, "{}", ev.len());
    }
    assert!(h.expire(20_000).is_empty(), "nobody overflowed");
    let report = h.report();
    assert_eq!(report.lobbies, settings.lobby_cap);
    assert!(report.state_bytes < 16 * 1024 * 1024, "{}", report.state_bytes);
}

/// A core-only session has no player key, so it cannot take a handle a key still holds.
/// After that session disconnects the handle is free again; that is a release, not a theft.
#[test]
fn a_keyless_session_cannot_take_a_live_keyed_handle() {
    let key = crate::crypto::PlayerIdentity::from_private([9; 32])
        .expect("curve")
        .public();
    let mut h = hub();
    let ann = h.connect_player(key, "ann", 0).expect("ann");
    assert_eq!(h.connect("ann", 1), Err(HubError::HandleTaken));
    assert_eq!(h.connect(" ANN ", 1), Err(HubError::HandleTaken));
    assert_eq!(h.identity(ann), Some(key));
    h.disconnect(ann);
    let keyless = h.connect("ann", 2).expect("the handle is free after disconnect");
    assert_eq!(h.identity(keyless), None);
}

/// AskReach tells the owner and strips the owner's relay, join code and ticket. A member's
/// events carry none of them, and a member cannot approve its own ask.
#[test]
fn a_member_cannot_read_the_owners_ticket_before_approval() {
    let key = |b: u8| {
        crate::crypto::PlayerIdentity::from_private([b; 32])
            .expect("curve")
            .public()
    };
    let mut h = hub();
    let host = h.connect_player(key(1), "host", 0).expect("host");
    let bob = h.connect_player(key(2), "bob", 0).expect("bob");
    let lobby = h.create(host, &spec(LobbyVisibility::Public), 0).expect("create");
    h.join(bob, lobby, None, 0).expect("join");
    let offer = ReachOffer {
        candidates: vec![crate::nat::Candidate::new(
            crate::nat::CandidateKind::Mapped,
            "198.51.100.7:7777".parse().expect("addr"),
        )],
        relay: Some("192.0.2.1:7800".parse().expect("addr")),
        join_code: Some("ABCDEF".into()),
        ticket: b"RGJT-owner-ticket".to_vec(),
        nat: None,
    };
    h.offer_reach(host, lobby, offer, 0).expect("offer");
    h.ask_reach(bob, lobby, ReachOffer::default(), 0).expect("ask");
    assert!(
        events_of(&mut h, bob)
            .iter()
            .all(|e| !matches!(e, HubEvent::Plan(_))),
        "the asker is not sent a plan"
    );
    let ask = events_of(&mut h, host).into_iter().find_map(|e| match e {
        HubEvent::ReachAsk { offer, .. } => Some(offer),
        _ => None,
    });
    let ask = ask.expect("the owner is asked");
    assert!(ask.ticket.is_empty(), "the ask drops the owner's ticket");
    assert!(ask.relay.is_none());
    assert!(ask.join_code.is_none());
    assert!(
        ask.candidates.is_empty(),
        "a member ask carries its own candidates only"
    );
    assert_eq!(
        h.approve(bob, lobby, bob, true, 0),
        Err(LobbyError::NotOwner.into())
    );
    assert!(
        events_of(&mut h, bob)
            .iter()
            .all(|e| !matches!(e, HubEvent::Plan(_))),
        "a member approve does not release the plan"
    );
}

/// Ask and approve of a lobby the caller is not in use one code, whether or not that id exists.
#[test]
fn ask_and_approve_do_not_tell_a_missing_lobby_from_another() {
    let mut h = hub();
    let host = h.connect("host", 0).expect("host");
    let stranger = h.connect("cy", 0).expect("cy");
    let lobby = h
        .create(host, &spec(LobbyVisibility::Private), 0)
        .expect("create");
    let missing = LobbyId(9_000);
    assert_eq!(
        h.ask_reach(stranger, missing, ReachOffer::default(), 0)
            .map(|_| ()),
        Err(LobbyError::NotInLobby.into())
    );
    assert_eq!(
        h.ask_reach(stranger, lobby, ReachOffer::default(), 0).map(|_| ()),
        Err(LobbyError::NotInLobby.into())
    );
    assert_eq!(
        h.approve(stranger, missing, host, true, 0).map(|_| ()),
        Err(LobbyError::NotInLobby.into())
    );
    assert_eq!(
        h.approve(stranger, lobby, host, true, 0).map(|_| ()),
        Err(LobbyError::NotInLobby.into())
    );
}

/// `join` must not say whether an id exists. A missing lobby, a private lobby and a friends-only
/// lobby the caller cannot enter are the same refusal. Asserts the safe property (KI-NT-046,
/// ADR-10069: the shared code is spec 89's `lobby_not_found`, the code a missing lobby already
/// answered; the review draft named `not_in_lobby`, the answer of the other lobby requests).
#[test]
fn join_does_not_tell_a_missing_lobby_from_a_closed_one() {
    let mut h = hub();
    let owner = h.connect("owner", 0).expect("owner");
    let pal = h.connect("pal", 0).expect("pal");
    let bob = h.connect("bob", 0).expect("bob");
    let private_lobby = h
        .create(owner, &spec(LobbyVisibility::Private), 0)
        .expect("private");
    let friends = h
        .create(pal, &spec(LobbyVisibility::FriendsOnly), 0)
        .expect("friends");
    let code = |r: Result<(), HubError>| r.map_or_else(|e| e.code().to_owned(), |_| "ok".to_owned());
    let missing = code(h.join(bob, LobbyId(9_000), None, 0));
    let hidden = code(h.join(bob, private_lobby, None, 0));
    let friends_only = code(h.join(bob, friends, None, 0));
    assert_eq!(
        (missing, hidden, friends_only),
        (
            "lobby_not_found".to_owned(),
            "lobby_not_found".to_owned(),
            "lobby_not_found".to_owned()
        ),
        "join tells these ids apart"
    );
    // The operator still sees why: the refusal counter keeps the real reasons.
    let refused = h.report().refused;
    assert_eq!(refused.get("lobby_not_found"), Some(&1), "{refused:?}");
    assert_eq!(refused.get("hub_invite_only"), Some(&1), "{refused:?}");
    assert_eq!(refused.get("hub_friends_only"), Some(&1), "{refused:?}");
}

/// A thousand lines at one timestamp stay inside the per-second cap and the history cap, and a
/// muted key's lobby burst is refused in full.
#[test]
fn a_chat_burst_stays_inside_the_rate_the_history_and_a_mute() {
    let settings = HubSettings {
        chat_per_sec: 4,
        chat_history: 8,
        ..HubSettings::default()
    };
    let mut h = HubServer::with_secret(settings, [1; 32]);
    let owner_key = crate::crypto::PlayerIdentity::from_private([1; 32])
        .expect("curve")
        .public();
    let bob_key = crate::crypto::PlayerIdentity::from_private([2; 32])
        .expect("curve")
        .public();
    let owner = h.connect_player(owner_key, "owner", 0).expect("owner");
    let bob = h.connect_player(bob_key, "bob", 0).expect("bob");
    let lobby = h.create(owner, &LobbySpec::default(), 0).expect("lobby");
    h.join(bob, lobby, None, 0).expect("join");
    let scope = ChatScope::Lobby(lobby);
    let mut sent = 0usize;
    let mut limited = 0usize;
    for _ in 0..1000 {
        match h.chat(bob, scope, "x", 0) {
            Ok(()) => sent += 1,
            Err(HubError::ChatRateLimited) => limited += 1,
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    assert_eq!((sent, limited), (4, 996));
    assert!(h.chat_history(lobby).len() <= 8);
    for window in 1..5 {
        for n in 0..4 {
            h.chat(bob, scope, "y", window * 1000 + n)
                .expect("the next window admits its cap");
        }
    }
    assert!(h.chat_history(lobby).len() <= 8);
    h.mute(owner, lobby, bob, true, 10_000).expect("mute");
    for _ in 0..1000 {
        assert_eq!(h.chat(bob, scope, "z", 10_000), Err(HubError::Muted));
    }
    assert!(h.chat_history(lobby).len() <= 8);
}

fn chat_texts(events: &[HubEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            HubEvent::Chat { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// KI-NT-045 (ADR-10069): lobby and global chat lines dropped at a member's outbox cap come back
/// with the resync, in order and once each; a later drain does not repeat them.
#[test]
fn a_resync_replays_the_chat_lines_dropped_at_the_outbox_cap() {
    // `sanitized` raises `outbox_cap` to at least 16: the bursts below clear that.
    let settings = HubSettings {
        outbox_cap: 16,
        chat_per_sec: 1_000,
        global_chat_per_sec: 1_000,
        chat_history: 64,
        ..HubSettings::default()
    };
    let mut h = HubServer::with_secret(settings, [5; 32]);
    let host = h.connect("host", 0).expect("host");
    let bob = h.connect("bob", 0).expect("bob");
    let lobby = h.create(host, &spec(LobbyVisibility::Public), 0).expect("create");
    h.chat(host, ChatScope::Lobby(lobby), "before bob", 0)
        .expect("chat");
    h.join(bob, lobby, None, 0).expect("join");
    let _ = h.take_events(bob);
    for i in 0..40 {
        h.chat(host, ChatScope::Lobby(lobby), &format!("line {i}"), 0)
            .expect("chat");
    }
    let want: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
    assert_eq!(
        chat_texts(&h.take_events(bob)),
        want,
        "the lines past the outbox cap were lost (or the line before the join was sent)"
    );
    h.chat(host, ChatScope::Lobby(lobby), "after", 0).expect("chat");
    assert_eq!(
        chat_texts(&h.take_events(bob)),
        vec!["after".to_owned()],
        "a line came twice"
    );
    for i in 0..30 {
        h.chat(host, ChatScope::Global, &format!("global {i}"), 0)
            .expect("global chat");
    }
    let want: Vec<String> = (0..30).map(|i| format!("global {i}")).collect();
    assert_eq!(
        chat_texts(&h.take_events(bob)),
        want,
        "global lines past the cap were lost"
    );
}

/// KI-NT-045 (ADR-10069): a join plan dropped at the outbox cap is sent again by the resync, to
/// the member and to the owner, so neither has to ask again.
#[test]
fn a_resync_sends_a_dropped_join_plan_again() {
    let settings = HubSettings {
        outbox_cap: 16,
        chat_per_sec: 1_000,
        ..HubSettings::default()
    };
    let mut h = HubServer::with_secret(settings, [6; 32]);
    let host = h.connect("host", 0).expect("host");
    let bob = h.connect("bob", 0).expect("bob");
    let lobby = h.create(host, &spec(LobbyVisibility::Public), 0).expect("create");
    h.join(bob, lobby, None, 0).expect("join");
    let offer = ReachOffer {
        candidates: vec![crate::nat::Candidate::new(
            crate::nat::CandidateKind::Mapped,
            "198.51.100.7:7777".parse().expect("addr"),
        )],
        ..ReachOffer::default()
    };
    h.offer_reach(host, lobby, offer, 0).expect("offer");
    h.ask_reach(bob, lobby, ReachOffer::default(), 0).expect("ask");
    let _ = h.take_events(bob);
    let _ = h.take_events(host);
    // Both outboxes full: the plans are dropped.
    for i in 0..16 {
        h.chat(host, ChatScope::Lobby(lobby), &format!("fill {i}"), 0)
            .expect("chat");
    }
    let plan = h
        .approve(host, lobby, bob, true, 0)
        .expect("approve")
        .expect("a plan");
    let plans = |events: Vec<HubEvent>| -> Vec<JoinPlan> {
        events
            .into_iter()
            .filter_map(|e| match e {
                HubEvent::Plan(p) => Some(p),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        plans(h.take_events(bob)),
        vec![plan],
        "the member's dropped plan is not resent"
    );
    let owner_plans = plans(h.take_events(host));
    assert_eq!(owner_plans.len(), 1, "the owner's dropped plan is not resent");
    assert_eq!(owner_plans[0].peer, bob);
    // Delivered once: the next drain is quiet.
    assert!(plans(h.take_events(bob)).is_empty());
}
