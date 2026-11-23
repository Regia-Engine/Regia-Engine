// SPDX-License-Identifier: MPL-2.0
//! The self-hosted lobby hub end to end (NL-04, ADR-8559): an in-process loopback `NetServer`
//! running a `HubHost`, and `HubLobbyBackend` clients pinned to the hub's Noise key. Time is the
//! test clock (a shared manual clock stepped by 10 ms); nothing sleeps.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use regia_net::crypto::{ServerIdentity, ServerKey};
use regia_net::lobby::hub::HubWire;
use regia_net::lobby::hub::{
    HubClientConfig, HubClock, HubError, HubEvent, HubHost, HubLinkState, HubLobbyBackend, HubPresence,
    HubServer, HubSettings,
};
use regia_net::lobby::{
    LobbyBackend, LobbyError, LobbyEvent, LobbyId, LobbyQuery, LobbyServer, LobbySpec, LobbyVisibility, keys,
};
use regia_net::{ClientConfig, CryptoMode, LoopbackNet, LoopbackSocket, NetClient};

const STEP_MS: u64 = 10;
const HUB_ADDR: ([u8; 4], u16) = ([10, 9, 0, 1], 7810);

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
}

impl Rig {
    fn new() -> Self {
        let net = LoopbackNet::new();
        let identity = ServerIdentity::generate().expect("random");
        let key = identity.public();
        let host = HubHost::new(
            net.bind(SocketAddr::from(HUB_ADDR)).expect("bind"),
            identity,
            HubSettings::default(),
        );
        Self {
            net,
            host,
            key,
            clock: HubClock::manual(0),
            now: 0,
            next_port: 5000,
        }
    }

    fn connect_with(&mut self, handle: &str, key: Option<ServerKey>) -> Result<Peer, HubError> {
        self.next_port += 1;
        let socket = self
            .net
            .bind(SocketAddr::from(([10, 9, 1, 2], self.next_port)))
            .expect("bind");
        let backend = HubLobbyBackend::connect(
            socket,
            SocketAddr::from(HUB_ADDR),
            HubClientConfig {
                handle: handle.into(),
                server_key: key,
                clock: self.clock.clone(),
                ..HubClientConfig::default()
            },
        )?;
        Ok(Peer {
            backend,
            events: Vec::new(),
        })
    }

    fn client(&mut self, handle: &str) -> Peer {
        let key = self.key;
        let mut peer = self.connect_with(handle, Some(key)).expect("pinned");
        self.run(&mut [&mut peer], |p| p[0].backend.local_member().is_some());
        peer
    }

    fn step(&mut self, peers: &mut [&mut Peer]) {
        self.now += STEP_MS;
        self.clock.set_ms(self.now);
        for p in peers.iter_mut() {
            let events = p.backend.poll();
            p.events.extend(events);
        }
        self.host.update(self.now);
        for p in peers.iter_mut() {
            let events = p.backend.poll();
            p.events.extend(events);
        }
    }

    /// Steps until `done` holds; fails after 500 steps (5 s of test clock).
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

fn listed(p: &Peer) -> Option<Vec<regia_net::lobby::LobbyInfo>> {
    p.events.iter().rev().find_map(|e| match e {
        LobbyEvent::List { result: Ok(list), .. } => Some(list.clone()),
        _ => None,
    })
}

fn entered(p: &Peer) -> Option<Result<LobbyId, LobbyError>> {
    p.events.iter().rev().find_map(|e| match e {
        LobbyEvent::Entered { result, .. } => Some(result.clone()),
        _ => None,
    })
}

#[test]
fn two_clients_find_each_other_through_the_hub_without_steam() {
    let mut rig = Rig::new();
    let mut ann = rig.client("ann");
    let mut bob = rig.client("bob");
    assert_eq!(ann.backend.name(), "hub");
    let mut data = BTreeMap::new();
    data.insert("mode".to_owned(), "duel".to_owned());
    ann.backend
        .create(&LobbySpec {
            visibility: LobbyVisibility::Public,
            max_members: 4,
            data,
            joinable: true,
        })
        .expect("create");
    rig.run(&mut [&mut ann, &mut bob], |p| created(p[0]).is_some());
    let lobby = created(&ann).expect("created");

    bob.backend
        .request_list(&LobbyQuery::default().with_string("mode", regia_net::lobby::Compare::Eq, "duel"))
        .expect("list");
    rig.run(&mut [&mut ann, &mut bob], |p| listed(p[1]).is_some());
    let list = listed(&bob).expect("listed");
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0].id, lobby);
    assert_eq!(list[0].owner, ann.backend.local_member().expect("ann"));
    assert_eq!(list[0].member_count, 1);
    assert_eq!(list[0].server, None);

    bob.backend.join(lobby).expect("join");
    rig.run(&mut [&mut ann, &mut bob], |p| {
        entered(p[1]).is_some()
            && p[0]
                .events
                .iter()
                .any(|e| matches!(e, LobbyEvent::MemberJoined { .. }))
    });
    assert_eq!(entered(&bob), Some(Ok(lobby)));
    let bob_id = bob.backend.local_member().expect("bob");
    assert!(ann.events.contains(&LobbyEvent::MemberJoined {
        lobby,
        member: bob_id
    }));
    let seen_by_bob = bob.backend.lobby(lobby).expect("bob is in it");
    let names: Vec<&str> = seen_by_bob.members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["ann", "bob"]);
    assert_eq!(seen_by_bob.data["mode"], "duel");

    // Chat reaches the other member through the hub.
    bob.backend.send_chat(lobby, "hello").expect("chat");
    rig.run(&mut [&mut ann, &mut bob], |p| {
        p[0].events
            .iter()
            .any(|e| matches!(e, LobbyEvent::Chat { text, .. } if text == "hello"))
    });
    // The hub sees both presences in the lobby.
    let rows = rig.host.core().players();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.presence == HubPresence::Lobby));
}

#[test]
fn invite_card_carries_no_host_address() {
    let mut rig = Rig::new();
    let mut ann = rig.client("ann");
    let mut bob = rig.client("bob");
    let mut eve = rig.client("eve");
    ann.backend
        .create(&LobbySpec {
            visibility: LobbyVisibility::Private,
            ..LobbySpec::default()
        })
        .expect("create");
    rig.run(&mut [&mut ann, &mut bob, &mut eve], |p| created(p[0]).is_some());
    let lobby = created(&ann).expect("created");
    // The owner confirms the session before inviting: the address now exists on the hub.
    let session: SocketAddr = "10.20.30.40:7777".parse().expect("addr");
    ann.backend
        .set_server(lobby, LobbyServer::Address(session))
        .expect("confirm");
    ann.backend.invite(lobby, "bob").expect("invite");
    rig.run(&mut [&mut ann, &mut bob, &mut eve], |p| {
        !p[1].backend.invites().is_empty()
    });
    let card = bob.backend.invites().remove(0);
    assert_eq!(card.lobby, lobby);
    assert_eq!(card.from_handle, "ann");
    let bytes = card.to_bytes().expect("encodes");
    let needles: [&[u8]; 4] = [
        b"10.20.30.40",
        &[10, 20, 30, 40],
        &7777u16.to_le_bytes(),
        &7777u16.to_be_bytes(),
    ];
    for needle in needles {
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "the invite card carries {needle:?}: {bytes:?}"
        );
    }
    assert!(!format!("{card:?}").contains("10.20.30.40"));
    // The wire event the card came in carries nothing more.
    let event = HubEvent::Invite(card.clone()).to_bytes().expect("encodes");
    assert!(!event.windows(4).any(|w| w == [10, 20, 30, 40]));

    // Eve has no invite: a private lobby refuses her, and she never learns the address.
    eve.backend.join(lobby).expect("request");
    rig.run(&mut [&mut ann, &mut bob, &mut eve], |p| entered(p[2]).is_some());
    // KI-NT-046 (ADR-10069): she is told what a missing id answers, so she cannot probe for
    // private lobbies; the hub's refusal counter keeps the real reason.
    assert_eq!(
        entered(&eve).expect("answered").map_err(|e| e.code()),
        Err("lobby_not_found")
    );
    assert!(eve.backend.last_error().is_some_and(|e| e == "lobby_not_found"));

    // Bob joins with the card's token and, as an approved member, gets the session address.
    bob.backend.join(lobby).expect("request");
    rig.run(&mut [&mut ann, &mut bob, &mut eve], |p| {
        p[1].events
            .iter()
            .any(|e| matches!(e, LobbyEvent::ServerSet { .. }))
    });
    assert_eq!(entered(&bob), Some(Ok(lobby)));
    assert_eq!(
        bob.backend.lobby(lobby).and_then(|l| l.server),
        Some(LobbyServer::Address(session))
    );
    assert!(
        !eve.events
            .iter()
            .any(|e| matches!(e, LobbyEvent::ServerSet { .. }))
    );
    assert!(eve.backend.lobby(lobby).is_none());
}

#[test]
fn a_listed_lobby_never_shows_its_session_address() {
    let mut rig = Rig::new();
    let mut ann = rig.client("ann");
    let mut bob = rig.client("bob");
    ann.backend.create(&LobbySpec::default()).expect("create");
    rig.run(&mut [&mut ann, &mut bob], |p| created(p[0]).is_some());
    let lobby = created(&ann).expect("created");
    let session = LobbyServer::Address("10.20.30.40:7777".parse().expect("addr"));
    ann.backend.set_server(lobby, session).expect("confirm");
    ann.backend
        .set_data(lobby, keys::STATE, Some("in_game"))
        .expect("state");
    bob.backend.request_list(&LobbyQuery::default()).expect("list");
    rig.run(&mut [&mut ann, &mut bob], |p| listed(p[1]).is_some());
    let list = listed(&bob).expect("listed");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].server, None);
    assert!(!list[0].data.contains_key(keys::SERVER), "{:?}", list[0].data);
    assert_eq!(list[0].data[keys::STATE], "in_game");
    // The owner, a member, sees it.
    assert_eq!(ann.backend.lobby(lobby).and_then(|l| l.server), Some(session));
}

#[test]
fn the_hub_refuses_an_unpinned_key() {
    let mut rig = Rig::new();
    // No key: refused before a datagram leaves.
    let err = rig.connect_with("ann", None).err().expect("unpinned is refused");
    assert_eq!(err, HubError::KeyUnpinned);
    assert_eq!(err.code(), "hub_key_unpinned");
    // The wrong key: the Noise handshake fails the pin and no hub session is ever allocated.
    let wrong = ServerIdentity::generate().expect("random").public();
    let mut mallory = rig
        .connect_with("mallory", Some(wrong))
        .expect("pinned to the wrong key");
    rig.run(&mut [&mut mallory], |p| {
        matches!(p[0].backend.link_state(), HubLinkState::Ended(_))
    });
    assert_eq!(
        mallory.backend.link_state(),
        HubLinkState::Ended("hub_key_mismatch".into())
    );
    assert_eq!(mallory.backend.local_member(), None);
    assert!(rig.host.core().players().is_empty());
    assert_eq!(
        mallory.backend.create(&LobbySpec::default()),
        Err(LobbyError::Unavailable)
    );
    // The right key connects, sealed.
    let ann = rig.client("ann");
    assert!(ann.backend.is_sealed());
    assert_eq!(rig.host.server_key(), Some(rig.key));
}

#[test]
fn presence_deltas_follow_generations() {
    let mut hub = HubServer::new(HubSettings::default());
    let ann = hub.connect("ann", 0).expect("ann");
    hub.flush();
    let first = hub.take_events(ann);
    assert_eq!(first.len(), 2, "{first:?}");
    assert!(matches!(first[0], HubEvent::Welcome { member, .. } if member == ann));
    let HubEvent::Directory(page) = &first[1] else {
        panic!("a directory page first: {first:?}");
    };
    assert!(page.reset, "a new session gets a snapshot");
    assert!(page.done);
    let g1 = page.presence_gen;
    assert_eq!(page.players.len(), 1);

    // Nothing changed: nothing is sent.
    hub.flush();
    assert!(hub.take_events(ann).is_empty(), "idle bandwidth is zero");

    // One change: one delta, one generation on.
    let bob = hub.connect("bob", 0).expect("bob");
    hub.flush();
    let events = hub.take_events(ann);
    assert_eq!(events.len(), 1, "{events:?}");
    let HubEvent::Directory(delta) = &events[0] else {
        panic!("{events:?}");
    };
    assert!(!delta.reset, "a covered gap is a delta");
    assert_eq!(delta.presence_gen, g1 + 1);
    assert_eq!(delta.players.len(), 1);
    assert_eq!(delta.players[0].member, bob);
    assert_eq!(hub.generations().1, g1 + 1);

    // An invalid presence is refused and moves nothing.
    assert_eq!(
        hub.set_presence(ann, HubPresence::InGame, 0),
        Err(HubError::PresenceInvalid)
    );
    hub.flush();
    assert!(hub.take_events(ann).is_empty());

    // Leaving: a removal row in the next delta.
    hub.disconnect(bob);
    hub.flush();
    let events = hub.take_events(ann);
    let HubEvent::Directory(gone) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(gone.removed_players, vec![bob]);
    assert_eq!(gone.presence_gen, g1 + 2);

    // A cursor older than the change log gets a snapshot again.
    let mut small = HubServer::new(HubSettings {
        changelog: 4,
        ..HubSettings::default()
    });
    let watcher = small.connect("watcher", 0).expect("w");
    small.flush();
    small.take_events(watcher);
    let (_, cursor) = small.generations();
    for i in 0..8 {
        let id = small.connect(&format!("p{i}"), 0).expect("p");
        small.disconnect(id);
    }
    small.note_cursor(watcher, 0, cursor);
    small.flush();
    let events = small.take_events(watcher);
    let HubEvent::Directory(page) = &events[0] else {
        panic!("{events:?}");
    };
    assert!(page.reset, "a trimmed change log falls back to a snapshot");
    assert_eq!(page.players.len(), 1);
}

#[test]
fn an_idle_session_expires_on_the_test_clock() {
    let settings = HubSettings::default();
    assert_eq!(settings.idle_ms, 30_000);
    let mut hub = HubServer::new(settings);
    let ann = hub.connect("ann", 0).expect("ann");
    let bob = hub.connect("bob", 0).expect("bob");
    hub.heartbeat(ann, 20_000).expect("alive");
    assert!(hub.expire(29_999).is_empty());
    assert_eq!(hub.expire(30_000), vec![bob]);
    assert_eq!(hub.players().len(), 1);
    assert!(hub.expire(49_999).is_empty());
    assert_eq!(hub.expire(50_000), vec![ann]);
    assert!(hub.players().is_empty());
    // A handle is free again once its session expired.
    assert!(hub.connect("bob", 50_000).is_ok());

    // End to end: a client that stops pumping is expired by the hub on the hub's clock.
    let mut rig = Rig::new();
    let mut ann = rig.client("ann");
    let mut bob = rig.client("bob");
    assert_eq!(rig.host.core().players().len(), 2);
    // Ann keeps pumping (and heartbeating); bob goes quiet.
    let mut steps = 0u64;
    while rig.host.core().players().len() == 2 {
        rig.step(&mut [&mut ann]);
        steps += 1;
        assert!(steps < 10_000, "bob never expired");
    }
    let rows = rig.host.core().players();
    assert_eq!(rows.len(), 1);
    assert_eq!(Some(rows[0].member), ann.backend.local_member());
    assert!(
        steps * STEP_MS >= 3_000,
        "expiry follows the idle or link timeout, not the first quiet step"
    );
    let _ = &mut bob;
}

#[test]
fn a_connection_that_never_says_hello_is_dropped_at_the_idle_timeout() {
    let mut rig = Rig::new();
    // A raw endpoint of the hub's game, correctly pinned, that completes the handshake and then
    // sends nothing: it must not hold a peer slot forever.
    let mut raw: NetClient<HubWire, LoopbackSocket> = NetClient::connect(
        rig.net
            .bind(SocketAddr::from(([10, 9, 1, 9], 6000)))
            .expect("bind"),
        SocketAddr::from(HUB_ADDR),
        ClientConfig {
            crypto: CryptoMode::Required,
            server_key: Some(rig.key),
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("fits");
    let mut steps = 0u64;
    while !raw.is_connected() {
        rig.now += STEP_MS;
        raw.update(rig.now);
        rig.host.update(rig.now);
        steps += 1;
        assert!(steps < 500, "the handshake did not finish");
    }
    let joined = rig.now;
    while raw.is_connected() {
        rig.now += STEP_MS;
        raw.update(rig.now);
        rig.host.update(rig.now);
        assert!(rig.now - joined < 40_000, "a hello-less connection was kept");
    }
    assert!(rig.now - joined >= HubSettings::default().idle_ms);
    assert!(rig.host.core().players().is_empty());
    assert_eq!(rig.host.report().refused.get("hub_no_hello"), Some(&1));

    // A refused hello (a taken handle) ends the second client's link with the hub's code.
    let _ann = rig.client("ann");
    let mut copy = rig.connect_with("ANN", Some(rig.key)).expect("pinned");
    rig.run(&mut [&mut copy], |p| {
        matches!(p[0].backend.link_state(), HubLinkState::Ended(_))
    });
    assert_eq!(
        copy.backend.link_state(),
        HubLinkState::Ended("hub_handle_taken".into())
    );
}
