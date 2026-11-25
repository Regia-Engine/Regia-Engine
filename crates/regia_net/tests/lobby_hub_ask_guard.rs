// SPDX-License-Identifier: MPL-2.0
//! AskReach guards (NL-09). Acceptance rows 4, 5, 11 and 12.
//!
//! Time is `now_ms` passed into the core. Nothing sleeps. Row 12 uses a loopback socket and
//! steps [`HubHost::update`] on that same clock.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use regia_net::crypto::binding::{prove_binding, prove_server_presence};
use regia_net::crypto::{PlayerIdentity, PlayerKey, ServerIdentity, ServerKey};
use regia_net::handshake::MAGIC;
use regia_net::lobby::hub::{
    BindingClaims, HubClientConfig, HubClock, HubError, HubEvent, HubHost, HubLobbyBackend, HubNotice,
    HubServer, HubSettings, ReachOffer, ServerPresence, encode_binding_payload,
};
use regia_net::lobby::{
    LobbyBackend, LobbyError, LobbyEvent, LobbyId, LobbyQuery, LobbyServer, LobbySpec, LobbyVisibility,
    MemberId, keys,
};
use regia_net::nat::{Candidate, CandidateKind};
use regia_net::relay::RELAY_MAGIC;
use regia_net::{DatagramSocket, LoopbackNet, LoopbackSocket};

const CANDIDATE: &str = "203.0.113.77:4242";
const CANDIDATE_OCTETS: [u8; 4] = [203, 0, 113, 77];

fn key(n: u8) -> PlayerKey {
    PlayerKey([n; 32])
}

fn candidate() -> SocketAddr {
    CANDIDATE.parse().expect("addr")
}

fn offer_with_candidate() -> ReachOffer {
    // `ReachOffer` is `non_exhaustive`. An integration test is another crate.
    let mut offer = ReachOffer::default();
    offer.candidates = vec![Candidate::new(CandidateKind::Host, candidate())];
    offer
}

fn hub() -> HubServer {
    HubServer::with_secret(HubSettings::default(), [9; 32])
}

fn hub_caps(ask_inflight_per_identity: u32, ask_inflight_hub: u32) -> HubServer {
    HubServer::with_secret(
        HubSettings {
            ask_inflight_per_identity,
            ask_inflight_hub,
            max_members: 64,
            ..HubSettings::default()
        },
        [9; 32],
    )
}

fn spec_members(max_members: u32, visibility: LobbyVisibility) -> LobbySpec {
    LobbySpec {
        visibility,
        max_members,
        ..LobbySpec::default()
    }
}

/// Owner offers, `member` has joined. Both are connected with player keys.
fn listen_with_offer(h: &mut HubServer, visibility: LobbyVisibility) -> (MemberId, MemberId, LobbyId) {
    let owner = h.connect_player(key(1), "owner", 0).expect("owner");
    let member = h.connect_player(key(2), "member", 0).expect("member");
    let lobby = h.create(owner, &spec_members(32, visibility), 0).expect("create");
    h.join(member, lobby, None, 0).expect("join");
    h.offer_reach(owner, lobby, offer_with_candidate(), 0)
        .expect("offer");
    (owner, member, lobby)
}

fn pushed(h: &mut HubServer, ids: &[MemberId]) -> Vec<HubEvent> {
    ids.iter().flat_map(|id| h.take_events(*id)).collect()
}

fn assert_no_candidate(events: &[HubEvent]) {
    let debug = format!("{events:?}");
    assert!(!debug.contains(CANDIDATE), "{debug}");
    assert!(!debug.contains("203.0.113.77"), "{debug}");
    for event in events {
        let bytes = event.to_bytes().expect("encodes");
        assert!(
            !bytes.windows(4).any(|w| w == CANDIDATE_OCTETS),
            "candidate bytes in {event:?}"
        );
    }
}

fn code(err: HubError) -> &'static str {
    err.code()
}

/// Row 4: confirm is a flag. Directory, query, presence, room and `lobby()` hold no address.
#[test]
fn confirm_is_a_flag_and_no_view_carries_an_address() {
    let mut h = hub();
    let (owner, member, lobby) = listen_with_offer(&mut h, LobbyVisibility::Public);
    let outsider = h.connect("outsider", 0).expect("outsider");
    for id in [owner, member, outsider] {
        h.take_events(id);
    }
    let session = LobbyServer::Address("10.1.2.3:4000".parse().expect("addr"));
    h.confirm_session(owner, lobby, Some(session), 0)
        .expect("confirm");
    h.flush();
    let member_events = h.take_events(member);
    assert!(member_events.iter().any(|e| matches!(
        e,
        HubEvent::SessionConfirmed { lobby: id, confirmed: true } if *id == lobby
    )));
    for event in &member_events {
        let bytes = event.to_bytes().expect("encodes");
        assert!(!bytes.windows(4).any(|w| w == [10, 1, 2, 3]), "{event:?}");
    }
    let outside = h.take_events(outsider);
    for event in &outside {
        let bytes = event.to_bytes().expect("encodes");
        assert!(!bytes.windows(4).any(|w| w == [10, 1, 2, 3]), "{event:?}");
    }
    let info = h.lobby_info(lobby).expect("lobby");
    assert!(info.server.is_none());
    assert!(!format!("{info:?}").contains("10.1.2.3"));
    let found = LobbyQuery::default().evaluate(std::iter::once(&info));
    assert!(!format!("{found:?}").contains("10.1.2.3"));
    let presence = format!("{:?}", h.players());
    assert!(!presence.contains("10.1.2.3"), "{presence}");
    let cards = format!("{:?}", h.lobbies());
    assert!(!cards.contains("10.1.2.3"), "{cards}");
    let room = member_events.iter().find_map(|e| match e {
        HubEvent::Room(room) => Some(room),
        _ => None,
    });
    if let Some(room) = room {
        assert!(room.server.is_none());
    }
    assert_eq!(
        h.set_data(owner, lobby, "map", Some("10.9.8.7:1"), 0),
        Err(HubError::AddressInData)
    );
    assert_eq!(
        h.set_data(owner, lobby, "map", Some("udp:10.9.8.7:1"), 0),
        Err(HubError::AddressInData)
    );
    assert_eq!(
        h.set_data(owner, lobby, "map", Some("steam:12"), 0),
        Err(HubError::AddressInData)
    );
    assert_eq!(
        h.set_member_data(member, lobby, "platform", Some("steam:0"), 0),
        Err(HubError::AddressInData)
    );
    h.set_data(owner, lobby, "map", Some("harbor"), 0).expect("text");
    h.set_data(owner, lobby, keys::SERVER, None, 0).expect("withdraw");
    assert!(
        h.players()
            .iter()
            .any(|p| { p.member == member && p.presence == regia_net::lobby::hub::HubPresence::Lobby })
    );
}

/// Row 5: one key's 10-second window, and many keys sharing one address.
#[test]
fn one_key_and_one_address_each_have_an_ask_window() {
    let mut h = hub();
    let owner = h.connect_player(key(1), "owner", 0).expect("owner");
    let lobby = h
        .create(owner, &spec_members(32, LobbyVisibility::Public), 0)
        .expect("create");
    h.offer_reach(owner, lobby, offer_with_candidate(), 0)
        .expect("offer");
    let member = h.connect_player(key(2), "m0", 0).expect("member");
    h.join(member, lobby, None, 0).expect("join");
    h.note_source(member, Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 8))));
    for n in 0..4 {
        h.ask_reach(member, lobby, offer_with_candidate(), n)
            .expect("inside the key window");
    }
    assert_eq!(
        code(
            h.ask_reach(member, lobby, offer_with_candidate(), 4)
                .expect_err("fifth")
        ),
        "hub_ask_rate_limited"
    );
    h.ask_reach(member, lobby, offer_with_candidate(), 10_000)
        .expect("the window reopened");

    let full = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
    let quiet = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 40));
    let elsewhere = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 41));
    // A stamp leaves the window when `now - t` is no longer strictly under 10_000 ms.
    // The victim's three key stamps, the sixteen address stamps, the refusal and the
    // fourth key stamp all sit inside that window.
    let window = 40_000u64;
    let victim = h.connect_player(key(80), "victim", window).expect("victim");
    h.join(victim, lobby, None, window).expect("victim joins");
    h.note_source(victim, Some(quiet));
    for n in 0..3 {
        h.ask_reach(victim, lobby, ReachOffer::default(), window - 1_000 + n)
            .expect("victim key window");
    }
    for n in 0..16u8 {
        let id = h
            .connect_player(key(10 + n), &format!("f{n}"), window)
            .expect("filler");
        h.join(id, lobby, None, window).expect("join filler");
        h.note_source(id, Some(full));
        h.ask_reach(id, lobby, ReachOffer::default(), window)
            .expect("address window has room");
    }
    h.note_source(victim, Some(full));
    assert_eq!(
        code(
            h.ask_reach(victim, lobby, offer_with_candidate(), window)
                .expect_err("address full")
        ),
        "hub_ask_rate_limited"
    );
    h.note_source(victim, Some(elsewhere));
    h.ask_reach(victim, lobby, ReachOffer::default(), window)
        .expect("the address refusal did not stamp the key");
    assert_eq!(
        code(
            h.ask_reach(victim, lobby, ReachOffer::default(), window)
                .expect_err("key window full")
        ),
        "hub_ask_rate_limited"
    );
}

/// Replacing an open ask does not take a second in-flight slot. The hub cap does.
#[test]
fn inflight_caps_count_open_asks_and_a_replacement_does_not() {
    let mut h = hub_caps(1, 2);
    let owner = h.connect_player(key(1), "owner", 0).expect("owner");
    let lobby = h
        .create(owner, &spec_members(8, LobbyVisibility::Public), 0)
        .expect("create");
    h.offer_reach(owner, lobby, offer_with_candidate(), 0)
        .expect("offer");
    let a = h.connect_player(key(2), "a", 0).expect("a");
    let b = h.connect_player(key(3), "b", 0).expect("b");
    let c = h.connect_player(key(4), "c", 0).expect("c");
    for id in [a, b, c] {
        h.join(id, lobby, None, 0).expect("join");
    }
    h.ask_reach(a, lobby, ReachOffer::default(), 0).expect("a");
    h.ask_reach(a, lobby, ReachOffer::default(), 1)
        .expect("replacing a's ask stays within the per-key cap of 1");
    h.ask_reach(b, lobby, ReachOffer::default(), 2).expect("b");
    assert_eq!(
        code(
            h.ask_reach(c, lobby, ReachOffer::default(), 3)
                .expect_err("hub cap")
        ),
        "hub_ask_inflight"
    );
    h.approve(owner, lobby, a, false, 4).expect("deny frees a slot");
    h.ask_reach(c, lobby, ReachOffer::default(), 5)
        .expect("slot freed");

    let mut keyless = hub_caps(1, 64);
    let host = keyless.connect("host", 0).expect("host");
    let lobby = keyless
        .create(host, &spec_members(4, LobbyVisibility::Public), 0)
        .expect("create");
    keyless
        .offer_reach(host, lobby, offer_with_candidate(), 0)
        .expect("offer");
    let one = keyless.connect("one", 0).expect("one");
    let two = keyless.connect("two", 0).expect("two");
    keyless.join(one, lobby, None, 0).expect("one joins");
    keyless.join(two, lobby, None, 0).expect("two joins");
    keyless
        .ask_reach(one, lobby, ReachOffer::default(), 0)
        .expect("keyless one");
    keyless
        .ask_reach(two, lobby, ReachOffer::default(), 0)
        .expect("a second keyless session does not share a key window");
}

/// Row 5: a banned key is refused before its candidates are copied into an event.
#[test]
fn a_banned_key_receives_no_candidates() {
    let mut h = hub();
    let (owner, member, lobby) = listen_with_offer(&mut h, LobbyVisibility::Invisible);
    h.take_events(owner);
    h.set_operator_bans(&[key(2)]).expect("operator bans");
    let err = h
        .ask_reach(member, lobby, offer_with_candidate(), 0)
        .expect_err("operator ban");
    assert_eq!(code(err), "hub_banned");
    assert_no_candidate(&pushed(&mut h, &[owner, member]));

    h.set_operator_bans(&[]).expect("clear");
    let too_many = vec![key(3); binding_cap() + 1];
    assert_eq!(
        h.set_operator_bans(&too_many).expect_err("cap").code(),
        "hub_reach_invalid"
    );
    h.set_operator_bans(&[key(9)])
        .expect("kept after the refused replace");
    h.ask_reach(member, lobby, offer_with_candidate(), 1)
        .expect("a member of an invisible lobby is still admitted");

    let mut h = hub();
    let (owner, member, lobby) = listen_with_offer(&mut h, LobbyVisibility::Public);
    h.take_events(owner);
    h.set_bans(owner, lobby, &[key(2)], 0).expect("lobby bans");
    assert_eq!(
        code(
            h.ask_reach(member, lobby, offer_with_candidate(), 0)
                .expect_err("lobby ban")
        ),
        "hub_banned"
    );
    assert_no_candidate(&pushed(&mut h, &[owner, member]));
    let over = vec![key(4); 65];
    assert_eq!(
        h.set_bans(owner, lobby, &over, 0).expect_err("lobby cap").code(),
        "hub_reach_invalid"
    );
    assert_eq!(
        code(
            h.ask_reach(member, lobby, offer_with_candidate(), 1)
                .expect_err("old lobby ban kept")
        ),
        "hub_banned"
    );

    let (mut h, server, lobby) = dedicated_lobby(vec![key(2)], 8, 0, LobbyVisibility::Public);
    let member = h.connect_player(key(2), "bob", 0).expect("bob");
    h.set_wire(member, 2);
    h.offer_reach(server, lobby, offer_with_candidate(), 0)
        .expect("offer");
    h.take_events(server);
    assert_eq!(
        code(
            h.ask_reach(member, lobby, offer_with_candidate(), 0)
                .expect_err("server ban")
        ),
        "hub_banned"
    );
    assert_no_candidate(&pushed(&mut h, &[server, member]));
}

/// A ban is not an existence oracle: a missing id is `lobby_not_found` first.
#[test]
fn a_missing_lobby_is_not_found_before_the_ban_check() {
    let mut h = hub();
    let member = h.connect_player(key(2), "bob", 0).expect("bob");
    h.set_operator_bans(&[key(2)]).expect("ban");
    assert_eq!(
        h.ask_reach(member, LobbyId(9), offer_with_candidate(), 0),
        Err(LobbyError::NotFound.into())
    );
}

/// Row 11: full, closed and invisible asks return no candidates. A member of a closed listen
/// lobby still receives a plan.
#[test]
fn full_closed_and_invisible_asks_return_no_candidates() {
    let (mut h, server, lobby) = dedicated_lobby(Vec::new(), 1, 1, LobbyVisibility::Invisible);
    let member = h.connect_player(key(2), "bob", 0).expect("bob");
    h.set_wire(member, 2);
    h.join(member, lobby, None, 0).expect("already inside");
    h.offer_reach(server, lobby, offer_with_candidate(), 0)
        .expect("offer");
    h.take_events(server);
    assert_eq!(
        code(
            h.ask_reach(member, lobby, offer_with_candidate(), 0)
                .expect_err("full")
        ),
        "hub_server_full"
    );
    assert_no_candidate(&pushed(&mut h, &[server, member]));

    let stranger = h.connect_player(key(3), "cy", 0).expect("cy");
    h.set_wire(stranger, 2);
    assert_eq!(
        code(
            h.ask_reach(stranger, lobby, offer_with_candidate(), 0)
                .expect_err("full before visibility")
        ),
        "hub_server_full"
    );
    assert_no_candidate(&h.take_events(stranger));

    let mut h = hub();
    let owner = h.connect_player(key(1), "owner", 0).expect("owner");
    let lobby = h
        .create(owner, &spec_members(4, LobbyVisibility::Public), 0)
        .expect("create");
    h.offer_reach(owner, lobby, offer_with_candidate(), 0)
        .expect("offer");
    h.set_joinable(owner, lobby, false, 0).expect("close");
    let outsider = h.connect_player(key(4), "out", 0).expect("out");
    h.take_events(owner);
    assert_eq!(
        code(
            h.ask_reach(outsider, lobby, offer_with_candidate(), 0)
                .expect_err("closed")
        ),
        "hub_lobby_closed"
    );
    assert_no_candidate(&pushed(&mut h, &[owner, outsider]));

    let mut h = hub();
    let (owner, member, lobby) = listen_with_offer(&mut h, LobbyVisibility::Public);
    h.set_joinable(owner, lobby, false, 0).expect("close");
    h.take_events(owner);
    h.ask_reach(member, lobby, ReachOffer::default(), 0)
        .expect("a member of a closed listen lobby may still ask");
    let plan = h.approve(owner, lobby, member, true, 0).expect("approve");
    assert!(plan.is_some(), "approve yields a plan");

    let mut h = hub();
    let owner = h.connect_player(key(1), "owner", 0).expect("owner");
    let lobby = h
        .create(owner, &spec_members(4, LobbyVisibility::Invisible), 0)
        .expect("create");
    h.offer_reach(owner, lobby, offer_with_candidate(), 0)
        .expect("offer");
    let outsider = h.connect_player(key(5), "hid", 0).expect("hid");
    h.take_events(owner);
    assert_eq!(
        h.ask_reach(outsider, lobby, offer_with_candidate(), 0),
        Err(LobbyError::NotFound.into())
    );
    assert_no_candidate(&pushed(&mut h, &[owner, outsider]));

    let mut h = hub();
    let owner = h.connect_player(key(1), "owner", 0).expect("owner");
    let lobby = h
        .create(
            owner,
            &LobbySpec {
                max_members: 1,
                ..LobbySpec::default()
            },
            0,
        )
        .expect("create");
    h.offer_reach(owner, lobby, offer_with_candidate(), 0)
        .expect("offer");
    let outsider = h.connect_player(key(6), "full", 0).expect("full");
    assert_eq!(
        code(
            h.ask_reach(outsider, lobby, offer_with_candidate(), 0)
                .expect_err("member cap")
        ),
        "hub_lobby_full"
    );
}

/// Row 12: an `RGN1` datagram with a bad kind and an `RGNR` datagram on the hub socket are
/// dropped. Neither is answered and neither is forwarded.
#[test]
fn foreign_datagrams_on_the_hub_socket_are_dropped() {
    let net = LoopbackNet::new();
    let hub_addr = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 9), 7810));
    let sender_addr = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 10), 9000));
    let bystander_addr = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 11), 9001));
    let identity = ServerIdentity::generate().expect("key");
    let mut host = HubHost::new(net.bind(hub_addr).expect("hub"), identity, HubSettings::default());
    let mut sender = net.bind(sender_addr).expect("sender");
    let mut bystander = net.bind(bystander_addr).expect("bystander");
    let mut rgn1 = MAGIC.to_vec();
    rgn1.push(0xff);
    rgn1.extend_from_slice(b"not-a-packet");
    sender.send_to(&rgn1, hub_addr).expect("rgn1");
    let mut rgnr = RELAY_MAGIC.to_vec();
    rgnr.extend_from_slice(b"relay-datagram-not-for-hub");
    sender.send_to(&rgnr, hub_addr).expect("rgnr");
    host.update(0);
    let mut buf = [0u8; 256];
    assert!(sender.recv_from(&mut buf).expect("sender recv").is_none());
    assert!(bystander.recv_from(&mut buf).expect("bystander recv").is_none());
}

/// A wire-1 peer is sent `ServerSet` of `None`, not `SessionConfirmed`. A wire-2 peer is sent
/// the flag and still no address.
#[test]
fn wire_1_confirm_is_rewritten_and_wire_2_gets_the_flag() {
    let mut rig = Rig::new();
    let mut v1 = rig.client("v1", false);
    v1.backend.create(&LobbySpec::default()).expect("create");
    rig.run(&mut [&mut v1], |p| created(p[0]).is_some());
    let lobby = created(&v1).expect("lobby");
    v1.events.clear();
    v1.notices.clear();
    let session = LobbyServer::Address("10.1.2.3:4000".parse().expect("addr"));
    v1.backend.set_server(lobby, session).expect("confirm");
    // `ServerSet` of `None` is applied as `DataChanged`. `SessionConfirmed` would not be.
    rig.run(&mut [&mut v1], |p| {
        p[0].events
            .iter()
            .any(|e| matches!(e, LobbyEvent::DataChanged { .. }))
    });
    assert_eq!(v1.backend.lobby(lobby).and_then(|l| l.server), None);
    assert!(
        v1.notices
            .iter()
            .all(|n| !matches!(n, HubNotice::SessionConfirmed { .. })),
        "{:?}",
        v1.notices
    );
    assert!(!format!("{:?} {:?}", v1.events, v1.notices).contains("10.1.2.3"));

    let mut v2 = rig.client("v2", true);
    v2.backend.create(&LobbySpec::default()).expect("create");
    rig.run(&mut [&mut v2], |p| created(p[0]).is_some());
    let lobby = created(&v2).expect("lobby");
    v2.notices.clear();
    let session = LobbyServer::Address("10.1.2.3:4000".parse().expect("addr"));
    v2.backend.set_server(lobby, session).expect("confirm");
    rig.run(&mut [&mut v2], |p| {
        p[0].notices.iter().any(|n| {
            matches!(
                n,
                HubNotice::SessionConfirmed {
                    lobby: id,
                    confirmed: true
                } if *id == lobby
            )
        })
    });
    assert_eq!(v2.backend.lobby(lobby).and_then(|l| l.server), None);
    assert!(!format!("{:?}", v2.notices).contains("10.1.2.3"));
}

/// Bracketed IPv4, a `udp://` URL, an uppercase prefix, a named IPv6 zone, and `Steam:` are
/// socket-address spellings. Lobby data, member data, and create data must refuse them, the same
/// as `10.9.8.7:1` and `udp:10.9.8.7:1`. The forms Rust's `SocketAddr` parser accepts are the
/// controls.
#[test]
fn socket_address_spellings_are_refused_in_lobby_and_member_data() {
    let mut h = hub();
    let (owner, member, lobby) = listen_with_offer(&mut h, LobbyVisibility::Public);
    for value in [
        "[2001:db8::1]:27015",
        "udp:[2001:db8::1]:27015",
        "[::ffff:203.0.113.50]:27015",
        "[fe80::1%1]:27015",
    ] {
        assert_eq!(
            h.set_data(owner, lobby, "map", Some(value), 0),
            Err(HubError::AddressInData),
            "control {value}"
        );
        assert_eq!(
            h.set_member_data(member, lobby, "platform", Some(value), 0),
            Err(HubError::AddressInData),
            "control member {value}"
        );
    }

    let mut tick = 0u64;
    let mut at = || {
        tick += 1_000;
        tick
    };
    let mut slipped = Vec::new();
    for value in [
        "[203.0.113.50]:27015",
        "udp:[203.0.113.50]:27015",
        "udp://203.0.113.50:27015",
        "udp://[2001:db8::1]:27015",
        "UDP:203.0.113.50:27015",
        "[fe80::1%eth0]:27015",
        "Steam:76561198000000000",
    ] {
        let data = h.set_data(owner, lobby, "map", Some(value), at());
        let member_data = h.set_member_data(member, lobby, "platform", Some(value), at());
        if data != Err(HubError::AddressInData) || member_data != Err(HubError::AddressInData) {
            let info = h.lobby_info(lobby);
            let cards = h.lobbies();
            slipped.push(format!(
                "{value}: set_data={data:?} set_member_data={member_data:?} lobby={info:?} cards={cards:?}"
            ));
            let _ = h.set_data(owner, lobby, "map", None, at());
            let _ = h.set_member_data(member, lobby, "platform", None, at());
        }
    }

    let loner = h.connect_player(key(9), "loner", at()).expect("loner");
    let mut spec = spec_members(4, LobbyVisibility::Public);
    spec.data
        .insert("endpoint".into(), "udp://203.0.113.50:27015".into());
    let created = h.create(loner, &spec, at());
    if created != Err(HubError::AddressInData) {
        slipped.push(format!(
            "create udp:// stored {created:?} cards={:?}",
            h.lobbies()
        ));
    }
    assert!(
        slipped.is_empty(),
        "address spellings were stored:\n{}",
        slipped.join("\n")
    );
}

/// A ban that lands while an ask is open must not copy the owner's candidates into a plan.
/// Server bans already force `ReachDenied`. Operator bans and lobby bans take the same path.
#[test]
fn an_operator_or_lobby_ban_after_the_ask_copies_no_plan() {
    let mut problems = Vec::new();

    let mut h = hub();
    let (owner, member, lobby) = listen_with_offer(&mut h, LobbyVisibility::Public);
    h.take_events(owner);
    h.take_events(member);
    h.ask_reach(member, lobby, ReachOffer::default(), 0).expect("ask");
    h.take_events(owner);
    h.set_operator_bans(&[key(2)]).expect("operator ban");
    let plan = h.approve(owner, lobby, member, true, 1).expect("approve");
    let member_events = h.take_events(member);
    if plan.is_some() || member_events.iter().any(|e| matches!(e, HubEvent::Plan(_))) {
        problems.push(format!("operator ban: plan={plan:?} member={member_events:?}"));
    }

    let mut h = hub();
    let (owner, member, lobby) = listen_with_offer(&mut h, LobbyVisibility::Public);
    h.take_events(owner);
    h.ask_reach(member, lobby, ReachOffer::default(), 0).expect("ask");
    h.take_events(owner);
    h.set_bans(owner, lobby, &[key(2)], 1).expect("lobby ban");
    let plan = h.approve(owner, lobby, member, true, 2).expect("approve");
    let member_events = h.take_events(member);
    if plan.is_some() || member_events.iter().any(|e| matches!(e, HubEvent::Plan(_))) {
        problems.push(format!("lobby ban: plan={plan:?} member={member_events:?}"));
    }

    let (mut h, server, lobby) = dedicated_lobby(Vec::new(), 8, 0, LobbyVisibility::Public);
    let member = h.connect_player(key(2), "bob", 0).expect("bob");
    h.set_wire(member, 2);
    h.join(member, lobby, None, 0).expect("join");
    h.offer_reach(server, lobby, offer_with_candidate(), 0)
        .expect("offer");
    h.take_events(server);
    h.ask_reach(member, lobby, ReachOffer::default(), 0).expect("ask");
    h.take_events(server);
    h.set_operator_bans(&[key(2)]).expect("operator ban");
    let plan = h.approve(server, lobby, member, true, 1).expect("approve");
    let member_events = h.take_events(member);
    if plan.is_some() || member_events.iter().any(|e| matches!(e, HubEvent::Plan(_))) {
        problems.push(format!(
            "dedicated operator ban: plan={plan:?} member={member_events:?}"
        ));
    }

    assert!(
        problems.is_empty(),
        "a ban after the ask still copied candidates:\n{}",
        problems.join("\n")
    );
}

/// Presence rows, directory cards, room snapshots, and invites carry the handle. A handle that
/// is a socket address is an address in those views.
#[test]
fn a_socket_address_handle_is_absent_from_directory_presence_room_and_invites() {
    let addr = "203.0.113.50:27015";
    let mut h = hub();
    let owner = h.connect_player(key(1), addr, 0).expect("owner");
    let guest = h.connect_player(key(2), "guest", 0).expect("guest");
    let lobby = h
        .create(owner, &spec_members(4, LobbyVisibility::Public), 0)
        .expect("create");
    h.join(guest, lobby, None, 0).expect("join");
    let card = h.invite(owner, lobby, "guest", 0).expect("invite");
    h.flush();
    let guest_events = h.take_events(guest);
    let views = format!(
        "{:?} {:?} {:?} {:?}",
        h.players(),
        h.lobbies(),
        h.lobby_info(lobby),
        card
    );
    assert!(
        !views.contains("203.0.113.50"),
        "handle published in a view: {views}"
    );
    for event in &guest_events {
        let bytes = event.to_bytes().expect("encodes");
        assert!(
            !bytes.windows(addr.len()).any(|w| w == addr.as_bytes()),
            "handle bytes in {event:?}"
        );
    }
}

fn binding_cap() -> usize {
    4_096
}

fn dedicated_lobby(
    bans: Vec<PlayerKey>,
    capacity: u16,
    load: u16,
    visibility: LobbyVisibility,
) -> (HubServer, MemberId, LobbyId) {
    let hub_id = ServerIdentity::from_private([0x41; 32]).expect("hub key");
    let game = ServerIdentity::from_private([0x71; 32]).expect("game key");
    let mut h = HubServer::with_secret(HubSettings::default(), [9; 32]);
    h.set_hub_identity(hub_id.clone());
    let hash = [0x5A; 32];
    let server = h.connect("srv", 0).expect("server session");
    h.note_link_hash(server, hash);
    h.set_wire(server, 2);
    let owner_id = PlayerIdentity::from_private([0x12; 32]).expect("owner");
    let proof = {
        let claims = BindingClaims {
            version: 1,
            server_key: game.public(),
            expiry_ms: 0,
            serial: 1,
        };
        let payload = encode_binding_payload(&claims).expect("payload");
        prove_binding(&owner_id, &hub_id.public(), &payload).expect("binding")
    };
    let presence = ServerPresence {
        server_proof: prove_server_presence(&game, &hub_id.public(), &hash).expect("presence"),
        proofs: vec![proof],
        capacity,
        load,
        protocol: 1,
        game_version: "2".into(),
        relay_only: false,
        grace_ms: 0,
        bans,
    };
    h.register_server(server, &presence, 0).expect("register");
    let owner = h.connect_player(owner_id.public(), "ann", 0).expect("owner");
    h.set_wire(owner, 2);
    let lobby = h
        .create_delegated(owner, &spec_members(8, visibility), game.public(), 0)
        .expect("delegated");
    (h, server, lobby)
}

struct Rig {
    net: LoopbackNet,
    host: HubHost<LoopbackSocket>,
    key: ServerKey,
    clock: HubClock,
    now: u64,
    next_port: u16,
}

struct Peer {
    backend: HubLobbyBackend<LoopbackSocket>,
    events: Vec<LobbyEvent>,
    notices: Vec<HubNotice>,
}

impl Rig {
    fn new() -> Self {
        let net = LoopbackNet::new();
        let identity = ServerIdentity::generate().expect("key");
        let key = identity.public();
        let host = HubHost::new(
            net.bind(SocketAddr::from(([10, 8, 0, 1], 7810))).expect("bind"),
            identity,
            HubSettings::default(),
        );
        Self {
            net,
            host,
            key,
            clock: HubClock::manual(0),
            now: 0,
            next_port: 4000,
        }
    }

    fn client(&mut self, handle: &str, wire2: bool) -> Peer {
        self.next_port += 1;
        let socket = self
            .net
            .bind(SocketAddr::from(([10, 8, 1, 2], self.next_port)))
            .expect("bind");
        let backend = HubLobbyBackend::connect(
            socket,
            SocketAddr::from(([10, 8, 0, 1], 7810)),
            HubClientConfig {
                handle: handle.into(),
                server_key: Some(self.key),
                clock: self.clock.clone(),
                ..HubClientConfig::default()
            },
        )
        .expect("connect");
        let mut peer = Peer {
            backend,
            events: Vec::new(),
            notices: Vec::new(),
        };
        self.run(&mut [&mut peer], |p| p[0].backend.local_member().is_some());
        if wire2 {
            peer.backend.note_hub_game_version("2");
            for _ in 0..30 {
                self.step(&mut [&mut peer]);
            }
        }
        peer
    }

    fn step(&mut self, peers: &mut [&mut Peer]) {
        self.now += 10;
        self.clock.set_ms(self.now);
        for p in peers.iter_mut() {
            p.events.extend(p.backend.poll());
            p.notices.extend(p.backend.take_notices());
        }
        self.host.update(self.now);
        for p in peers.iter_mut() {
            p.events.extend(p.backend.poll());
            p.notices.extend(p.backend.take_notices());
        }
    }

    fn run(&mut self, peers: &mut [&mut Peer], mut done: impl FnMut(&mut [&mut Peer]) -> bool) {
        for _ in 0..500 {
            if done(peers) {
                return;
            }
            self.step(peers);
        }
        panic!("the condition did not hold within 500 steps");
    }
}

fn created(p: &Peer) -> Option<LobbyId> {
    p.events.iter().find_map(|e| match e {
        LobbyEvent::Created { result: Ok(id), .. } => Some(*id),
        _ => None,
    })
}
