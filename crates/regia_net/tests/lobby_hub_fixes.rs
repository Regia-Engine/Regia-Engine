// SPDX-License-Identifier: MPL-2.0
//! The lobby hub's review fixes through `HubHost` (ADR-8633): an optional player proof in `Hello`,
//! friends by player key, an outbox burst resynchronized instead of dropping the host, a session
//! held across a dormant link, and a create and list flood from one and from many connections.
//! In-process loopback; time is the test clock (10 ms steps); nothing sleeps.

use std::net::SocketAddr;

use regia_net::crypto::{PlayerKey, ServerIdentity, ServerKey};
use regia_net::lobby::hub::{
    HubClientConfig, HubClock, HubEvent, HubHost, HubLimits, HubLinkState, HubLobbyBackend, HubMessage,
    HubRequest, HubSettings, HubWire,
};
use regia_net::lobby::{LobbyBackend, LobbyEvent, LobbyId, LobbyQuery, LobbySpec, LobbyVisibility, MemberId};
use regia_net::{
    Channel, ClientConfig, ClientEvent, CryptoMode, LinkConfig, LoopbackNet, LoopbackSocket, NetClient,
    RateConfig, ServerConfig,
};

const STEP_MS: u64 = 10;
const HUB_ADDR: ([u8; 4], u16) = ([10, 7, 0, 1], 7820);

type Raw = NetClient<HubWire, LoopbackSocket>;

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

/// A raw hub endpoint: the requests a hostile or buggy client can send, and what it was sent.
struct RawPeer {
    link: Raw,
    events: Vec<HubEvent>,
}

impl Rig {
    fn new(settings: HubSettings, config: ServerConfig) -> Self {
        let net = LoopbackNet::new();
        let identity = ServerIdentity::generate().expect("random");
        let key = identity.public();
        let host = HubHost::with_config(
            net.bind(SocketAddr::from(HUB_ADDR)).expect("bind"),
            identity,
            settings,
            config,
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

    fn client(&mut self, handle: &str) -> Peer {
        self.next_port += 1;
        let socket = self
            .net
            .bind(SocketAddr::from(([10, 7, 1, 2], self.next_port)))
            .expect("bind");
        let backend = HubLobbyBackend::connect(
            socket,
            SocketAddr::from(HUB_ADDR),
            HubClientConfig {
                handle: handle.into(),
                server_key: Some(self.key),
                clock: self.clock.clone(),
                ..HubClientConfig::default()
            },
        )
        .expect("pinned");
        let mut peer = Peer {
            backend,
            events: Vec::new(),
        };
        self.run(&mut [&mut peer], &mut [], |p, _| {
            p[0].backend.local_member().is_some()
        });
        peer
    }

    /// A raw endpoint from `ip` (each address block has its own peer allowance).
    fn raw(
        &mut self,
        ip: [u8; 4],
        resume: Option<[u8; regia_net::handshake::RESUME_TICKET_BYTES]>,
    ) -> RawPeer {
        self.next_port += 1;
        let link = NetClient::connect(
            self.net
                .bind(SocketAddr::from((ip, self.next_port)))
                .expect("bind"),
            SocketAddr::from(HUB_ADDR),
            ClientConfig {
                crypto: CryptoMode::Required,
                server_key: Some(self.key),
                resume,
                // A raw connection is a scripted or hostile client: it sends at once, with no
                // rate controller and no per-stream message caps of its own (ADR-8407). With them
                // on, the flood test's flooder held itself back: its first second was nearly idle,
                // the app-limited bound (ADR-8592) set its target near 29 kbit/s, on this test
                // clock (RTT 0) no filled round ever grew it, and the reliable stream's message cap
                // spread the rest, so its own reliable queue reached `max_reliable_in_flight`
                // before the hub saw the flood.
                link: LinkConfig {
                    rate: RateConfig {
                        enabled: false,
                        ..RateConfig::default()
                    },
                    message_caps: false,
                    ..LinkConfig::default()
                },
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("fits");
        RawPeer {
            link,
            events: Vec::new(),
        }
    }

    fn step(&mut self, peers: &mut [&mut Peer], raws: &mut [&mut RawPeer]) {
        self.now += STEP_MS;
        self.clock.set_ms(self.now);
        for p in peers.iter_mut() {
            let events = p.backend.poll();
            p.events.extend(events);
        }
        for r in raws.iter_mut() {
            r.pump(self.now);
        }
        self.host.update(self.now);
        for p in peers.iter_mut() {
            let events = p.backend.poll();
            p.events.extend(events);
        }
        for r in raws.iter_mut() {
            r.pump(self.now);
        }
    }

    /// Steps `ms` of test clock.
    fn idle(&mut self, ms: u64, peers: &mut [&mut Peer], raws: &mut [&mut RawPeer]) {
        for _ in 0..ms / STEP_MS {
            self.step(peers, raws);
        }
    }

    /// Steps until `done` holds; fails after 2000 steps (20 s of test clock).
    fn run(
        &mut self,
        peers: &mut [&mut Peer],
        raws: &mut [&mut RawPeer],
        mut done: impl FnMut(&mut [&mut Peer], &mut [&mut RawPeer]) -> bool,
    ) {
        for _ in 0..2000 {
            if done(peers, raws) {
                return;
            }
            self.step(peers, raws);
        }
        panic!("the condition did not hold within 2000 steps");
    }
}

impl RawPeer {
    fn pump(&mut self, now: u64) {
        self.link.update(now);
        while let Some(e) = self.link.poll_event() {
            if let ClientEvent::Message {
                message: HubMessage::Event(ev),
                ..
            } = e
            {
                self.events.push(ev);
            }
        }
    }

    fn send(&mut self, rq: HubRequest) {
        self.link
            .send(Channel::GAME_RPC, &HubMessage::Request(rq))
            .expect("the raw link takes the request");
    }

    fn welcome(&self) -> Option<(MemberId, PlayerKey)> {
        self.events.iter().find_map(|e| match e {
            HubEvent::Welcome { member, identity, .. } => Some((*member, *identity)),
            _ => None,
        })
    }

    fn reply(&self, request: u64) -> Option<Result<LobbyId, String>> {
        self.events.iter().find_map(|e| match e {
            HubEvent::Reply { request: r, result } if *r == request => Some(result.clone()),
            _ => None,
        })
    }

    fn refused(&self) -> Option<String> {
        self.events.iter().find_map(|e| match e {
            HubEvent::Refused { code } => Some(code.clone()),
            _ => None,
        })
    }
}

fn created(p: &Peer) -> Option<LobbyId> {
    p.events.iter().find_map(|e| match e {
        LobbyEvent::Created { result: Ok(id), .. } => Some(*id),
        _ => None,
    })
}

fn friends_only() -> LobbySpec {
    LobbySpec {
        visibility: LobbyVisibility::FriendsOnly,
        ..LobbySpec::default()
    }
}

/// Finding 4 (ADR-8633): a `Hello` with no proof is a session with no player key; a friends-only
/// lobby refuses it (fail closed) even under the friend's handle, a friend is listed by key, and a
/// proof that does not bind to the link is refused.
#[test]
fn a_hello_without_a_proof_has_no_player_key_and_friends_only_refuses_it() {
    let mut rig = Rig::new(HubSettings::default(), ServerConfig::default());
    let mut owner = rig.client("owner");
    let mut pal = rig.client("pal");
    owner
        .backend
        .set_friends(&[pal.backend.identity()])
        .expect("friends");
    owner.backend.create(&friends_only()).expect("create");
    rig.run(&mut [&mut owner, &mut pal], &mut [], |p, _| {
        created(p[0]).is_some()
    });
    let lobby = created(&owner).expect("created");

    // A raw client says hello with no proof.
    let mut anon = rig.raw([10, 7, 2, 1], None);
    rig.run(&mut [], &mut [&mut anon], |_, r| r[0].link.is_connected());
    anon.send(HubRequest::Hello {
        handle: "anon".into(),
        proof: Vec::new(),
    });
    rig.run(&mut [], &mut [&mut anon], |_, r| r[0].welcome().is_some());
    let (member, identity) = anon.welcome().expect("welcome");
    assert_eq!(identity, PlayerKey::default(), "no key was proven");
    assert_eq!(rig.host.core().identity(member), None);
    anon.send(HubRequest::Join {
        request: 1,
        lobby,
        token: None,
    });
    rig.run(&mut [], &mut [&mut anon], |_, r| r[0].reply(1).is_some());
    assert_eq!(anon.reply(1), Some(Err("lobby_not_found".into())));

    // The friend's handle without the friend's key is refused too: pal's link goes away, the hub
    // drops its session, and a key-less session takes the handle.
    drop(pal);
    rig.idle(HubSettings::default().idle_ms + 1_000, &mut [&mut owner], &mut []);
    assert!(rig.host.core().member_by_handle("pal").is_none());
    let mut copy = rig.raw([10, 7, 2, 2], None);
    rig.run(&mut [], &mut [&mut copy], |_, r| r[0].link.is_connected());
    copy.send(HubRequest::Hello {
        handle: "pal".into(),
        proof: Vec::new(),
    });
    rig.run(&mut [], &mut [&mut copy], |_, r| {
        r[0].welcome().is_some() || r[0].refused().is_some()
    });
    assert!(
        copy.welcome().is_some(),
        "the handle is a display name, free again: {:?}",
        copy.events
    );
    copy.send(HubRequest::Join {
        request: 2,
        lobby,
        token: None,
    });
    rig.run(&mut [], &mut [&mut copy], |_, r| r[0].reply(2).is_some());
    assert_eq!(copy.reply(2), Some(Err("lobby_not_found".into())));
    // Both answers hide the lobby (KI-NT-046, ADR-10069); the operator's counter names the rule.
    assert_eq!(rig.host.report().refused.get("hub_friends_only"), Some(&2));

    // A proof that does not bind to this link (garbage of the right length) is refused.
    let mut forger = rig.raw([10, 7, 2, 3], None);
    rig.run(&mut [], &mut [&mut forger], |_, r| r[0].link.is_connected());
    forger.send(HubRequest::Hello {
        handle: "forger".into(),
        proof: vec![0x5A; regia_net::crypto::PLAYER_PROOF_BYTES],
    });
    rig.run(&mut [], &mut [&mut forger], |_, r| r[0].refused().is_some());
    assert_eq!(forger.refused(), Some("hub_identity_mismatch".into()));
    assert!(rig.host.core().member_by_handle("forger").is_none());
}

/// Finding 2: two members burst member data in one update past the owner's outbox cap. The owner
/// is resynchronized (it ends with both members' last values), not dropped.
#[test]
fn a_burst_past_the_outbox_cap_resyncs_the_owner_instead_of_dropping_it() {
    let mut rig = Rig::new(
        HubSettings {
            outbox_cap: 16,
            ..HubSettings::default()
        },
        ServerConfig::default(),
    );
    let mut owner = rig.client("owner");
    owner.backend.create(&LobbySpec::default()).expect("create");
    rig.run(&mut [&mut owner], &mut [], |p, _| created(p[0]).is_some());
    let lobby = created(&owner).expect("created");
    let mut a = rig.raw([10, 7, 3, 1], None);
    let mut b = rig.raw([10, 7, 3, 2], None);
    rig.run(&mut [&mut owner], &mut [&mut a, &mut b], |_, r| {
        r.iter().all(|x| x.link.is_connected())
    });
    for (raw, name) in [(&mut a, "a"), (&mut b, "b")] {
        raw.send(HubRequest::Hello {
            handle: name.into(),
            proof: Vec::new(),
        });
        raw.send(HubRequest::Join {
            request: 1,
            lobby,
            token: None,
        });
    }
    rig.run(&mut [&mut owner], &mut [&mut a, &mut b], |p, r| {
        r.iter().all(|x| x.reply(1) == Some(Ok(lobby)))
            && p[0].backend.lobby(lobby).is_some_and(|l| l.members.len() == 3)
    });
    // One update: 20 writes from each member.
    for (raw, name) in [(&mut a, "a"), (&mut b, "b")] {
        for i in 0..20 {
            raw.send(HubRequest::SetMemberData {
                lobby,
                key: "k".into(),
                value: Some(format!("{name}{i}")),
            });
        }
    }
    let expired_before = rig.host.report().expired;
    let mut raws = [&mut a, &mut b];
    rig.run(&mut [&mut owner], &mut raws, |p, _| {
        p[0].backend.lobby(lobby).is_some_and(|l| {
            let last = |n: &str| {
                l.members
                    .iter()
                    .find(|m| m.name == n)
                    .and_then(|m| m.data.get("k").cloned())
            };
            last("a") == Some("a19".into()) && last("b") == Some("b19".into())
        })
    });
    assert_eq!(owner.backend.link_state(), HubLinkState::Connected);
    assert!(
        owner
            .events
            .contains(&LobbyEvent::DataChanged { lobby, member: None }),
        "the owner was resynchronized (the case under test): {:?}",
        owner.events
    );
    assert_eq!(rig.host.report().expired, expired_before, "nobody was dropped");
    assert_eq!(rig.host.report().refused.get("hub_send_failed"), None);
    assert_eq!(rig.host.core().players().len(), 3);
}

/// Nit (ADR-8633): with `resume_grace_ms`, a dormant link keeps its hub session; the resumed link
/// is the same session and gets a resync (its room).
#[test]
fn a_dormant_link_keeps_its_session_and_the_resumed_link_is_resynced() {
    let mut rig = Rig::new(
        HubSettings::default(),
        ServerConfig {
            resume_grace_ms: 60_000,
            ..ServerConfig::default()
        },
    );
    let mut owner = rig.client("owner");
    owner.backend.create(&LobbySpec::default()).expect("create");
    rig.run(&mut [&mut owner], &mut [], |p, _| created(p[0]).is_some());
    let lobby = created(&owner).expect("created");
    let mut a = rig.raw([10, 7, 4, 1], None);
    rig.run(&mut [&mut owner], &mut [&mut a], |_, r| r[0].link.is_connected());
    a.send(HubRequest::Hello {
        handle: "a".into(),
        proof: Vec::new(),
    });
    a.send(HubRequest::Join {
        request: 1,
        lobby,
        token: None,
    });
    rig.run(&mut [&mut owner], &mut [&mut a], |_, r| r[0].reply(1).is_some());
    let (member, _) = a.welcome().expect("welcome");
    let ticket = a.link.resume_ticket().expect("a resume ticket");
    // A goes silent past the link timeout (it is not pumped): the hub keeps its session.
    rig.idle(HubSettings::default().idle_ms + 1_000, &mut [&mut owner], &mut []);
    assert_eq!(
        rig.host.server().dormant_peers().count(),
        1,
        "the silent link went dormant (the case under test)"
    );
    assert_eq!(
        rig.host.core().member_by_handle("a"),
        Some(member),
        "the dormant session was dropped"
    );
    // A comes back from a new address with its ticket: same session, and its room again.
    drop(a);
    let mut back = rig.raw([10, 7, 4, 2], Some(ticket));
    rig.run(&mut [&mut owner], &mut [&mut back], |_, r| {
        r[0].events
            .iter()
            .any(|e| matches!(e, HubEvent::Room(room) if room.lobby == lobby))
    });
    assert_eq!(rig.host.core().member_by_handle("a"), Some(member));
    back.send(HubRequest::SetMemberData {
        lobby,
        key: "k".into(),
        value: Some("back".into()),
    });
    rig.run(&mut [&mut owner], &mut [&mut back], |p, _| {
        p[0].backend
            .lobby(lobby)
            .and_then(|l| l.members.into_iter().find(|m| m.id == member))
            .and_then(|m| m.data.get("k").cloned())
            == Some("back".into())
    });
}

/// What one create-and-list flood left behind (see [`flood`]).
struct Flood {
    report: regia_net::lobby::hub::HubReport,
    /// Directory snapshots (reset pages) the flooder was sent.
    snapshots: usize,
    /// Requests the flooder sent: one hello, 500 creates or leaves, 2000 lists.
    sent: u64,
    /// Test clock from the flooder's hello to the end of the run, in ms.
    span_ms: u64,
    honest_connected: bool,
}

/// The lists the flooder sends from generation 0: four a step for 500 steps.
const FLOOD_LISTS: u64 = 2_000;

/// One connection floods creates, leaves and rewound lists while 64 connections create and list
/// at once and an honest client lists every 50 steps. `limits` replaces the hub's address and
/// identity gate (ADR-8648) when given.
fn flood(limits: Option<HubLimits>) -> Flood {
    let settings = HubSettings::default();
    let mut rig = Rig::new(settings.clone(), ServerConfig::default());
    if let Some(limits) = limits {
        rig.host.set_limits(limits);
    }
    let mut honest = rig.client("honest");

    let mut flooder = rig.raw([10, 7, 5, 1], None);
    let mut many: Vec<RawPeer> = (0..64u8).map(|i| rig.raw([10, 7, 6, i], None)).collect();
    {
        let mut raws: Vec<&mut RawPeer> = std::iter::once(&mut flooder).chain(many.iter_mut()).collect();
        rig.run(&mut [&mut honest], &mut raws, |_, r| {
            r.iter().all(|x| x.link.is_connected())
        });
    }
    let start_ms = rig.now;
    let mut sent = 1u64;
    flooder.send(HubRequest::Hello {
        handle: "flooder".into(),
        proof: Vec::new(),
    });
    for (i, m) in many.iter_mut().enumerate() {
        m.send(HubRequest::Hello {
            handle: format!("m{i}"),
            proof: Vec::new(),
        });
        m.send(HubRequest::Create {
            request: 1,
            visibility: LobbyVisibility::Public,
            max_members: 8,
            joinable: true,
            data: Default::default(),
        });
        m.send(HubRequest::List {
            request: 2,
            dir_gen: 0,
            presence_gen: 0,
        });
    }
    let mut request = 10u64;
    let mut last_lobby = None;
    let mut honest_lists = 0;
    // 5 s of test clock: every step, the flooder creates (or leaves its lobby) and sends four
    // lists from generation 0 (400 a second); the honest client lists every 50 steps.
    for step in 0..500u64 {
        if let Some(Ok(id)) = flooder.reply(request) {
            last_lobby = Some(id);
        }
        match last_lobby.take() {
            Some(lobby) => flooder.send(HubRequest::Leave { lobby }),
            None => {
                request += 1;
                flooder.send(HubRequest::Create {
                    request,
                    visibility: LobbyVisibility::Public,
                    max_members: 2,
                    joinable: true,
                    data: Default::default(),
                });
            }
        }
        sent += 1;
        for _ in 0..4 {
            flooder.send(HubRequest::List {
                request: 0,
                dir_gen: 0,
                presence_gen: 0,
            });
        }
        sent += 4;
        if step % 50 == 0 {
            honest.backend.request_list(&LobbyQuery::default()).expect("list");
            honest_lists += 1;
        }
        let mut raws: Vec<&mut RawPeer> = std::iter::once(&mut flooder).chain(many.iter_mut()).collect();
        rig.step(&mut [&mut honest], &mut raws);
    }
    {
        let mut raws: Vec<&mut RawPeer> = std::iter::once(&mut flooder).chain(many.iter_mut()).collect();
        rig.run(&mut [&mut honest], &mut raws, |p, _| {
            p[0].events
                .iter()
                .filter(|e| matches!(e, LobbyEvent::List { result: Ok(_), .. }))
                .count()
                == honest_lists
        });
    }
    let snapshots = flooder
        .events
        .iter()
        .filter(|e| matches!(e, HubEvent::Directory(p) if p.reset))
        .count();
    Flood {
        report: rig.host.report(),
        snapshots,
        sent,
        span_ms: rig.now - start_ms,
        honest_connected: honest.backend.link_state() == HubLinkState::Connected,
    }
}

/// Findings 2 and 3, end to end, on the hub core: one connection floods creates, leaves and
/// rewound lists, and many connections create and list at once. The lobby cap holds, the
/// flooder's rewinds build one snapshot per window, nobody is dropped, and an honest client's list
/// is still answered.
///
/// The address gate (NL-06) is opened here: one address is the cheapest flood, but an attacker
/// with many addresses and no player key passes that gate, so the core's rewind bound has to hold
/// on its own (ADR-8891). The gate's own bound is the next test.
#[test]
fn a_create_and_list_flood_from_one_and_many_connections_stays_bounded() {
    let open = HubLimits {
        per_address_per_sec: u32::MAX,
        per_identity_per_sec: u32::MAX,
    };
    let f = flood(Some(open));
    let report = &f.report;
    assert_eq!(report.sessions, 66, "nobody was dropped: {:?}", report.refused);
    assert!(report.lobbies <= HubSettings::default().lobby_cap);
    assert!(
        report.lobbies >= 64,
        "every one of the many created: {}",
        report.lobbies
    );
    assert_eq!(report.expired, 0);
    assert_eq!(
        report.refused.get("hub_address_rate_limited"),
        None,
        "the gate is open in this run"
    );
    // 2000 lists from generation 0 in 5 s: the first is the session's cursor, the next is its one
    // rewind in the window, and every other rewind is ignored.
    let rewinds = report.refused.get("hub_list_rewind").copied().unwrap_or(0);
    assert_eq!(rewinds, FLOOD_LISTS - 2);
    // The flooder's snapshots: its first, and one rewind. Every page is under 32 KiB plus a row,
    // and a snapshot of 66 rows and at most 65 cards is one page; the rest is deltas.
    assert!(f.snapshots <= 2, "{}", f.snapshots);
    assert!(f.honest_connected);
}

/// The same flood under the default address gate (NL-06, ADR-8891): the gate refuses the
/// flooder's requests before the core sees them, so the rewind counter no longer counts them. The
/// bound moves to the gate: of the flooder's requests, at most `per_address_per_sec` in any one
/// second of the run reach the core, and every rewind the core still counts is one of those.
/// Nobody else is refused, the snapshot bound holds, and the honest client is answered.
#[test]
fn the_address_gate_bounds_the_flood_in_front_of_the_rewind_window() {
    let limits = HubLimits::default();
    let f = flood(None);
    let report = &f.report;
    assert_eq!(report.sessions, 66, "nobody was dropped: {:?}", report.refused);
    assert!(report.lobbies <= HubSettings::default().lobby_cap);
    assert!(
        report.lobbies >= 64,
        "every one of the many created: {}",
        report.lobbies
    );
    assert_eq!(report.expired, 0);
    assert_eq!(
        report.refused.get("hub_identity_rate_limited"),
        None,
        "no flooder proved a key"
    );
    let by_address = report
        .refused
        .get("hub_address_rate_limited")
        .copied()
        .unwrap_or(0);
    // Only the flooder's address goes past the limit (the many send three requests each, the
    // honest client one list a half second), so every address refusal is one of its requests.
    let admitted = f.sent - by_address.min(f.sent);
    // A one-second sliding window admits at most the limit inside any second, so over a span of
    // `span_ms` it admits at most the limit once per started second.
    let windows = f.span_ms / 1_000 + 1;
    let bound = u64::from(limits.per_address_per_sec) * windows;
    assert!(
        admitted <= bound,
        "the gate let {admitted} of the flooder's {} requests through in {} ms (bound {bound})",
        f.sent,
        f.span_ms
    );
    let rewinds = report.refused.get("hub_list_rewind").copied().unwrap_or(0);
    assert!(
        rewinds + 2 <= admitted,
        "every counted rewind passed the gate: {rewinds} rewinds, {admitted} admitted"
    );
    assert!(f.snapshots <= 2, "{}", f.snapshots);
    assert!(f.honest_connected);
}
