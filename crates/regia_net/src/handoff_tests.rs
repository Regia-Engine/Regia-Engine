// SPDX-License-Identifier: MPL-2.0
//! The hand-off on a loopback net and on a scripted clock (spec 120 § 10.5, ADR-8901): no
//! impairment and no wall clock. The acceptance runs on impaired links (16 players, good-wifi and
//! bad-wifi) are in `crates/regia_net_soak/tests/migration_handoff.rs`.

use std::collections::BTreeMap;

use super::*;
use crate::client::{ClientEvent, NetClient};
use crate::server::ServerEvent;
use crate::socket::{LoopbackNet, LoopbackSocket};

#[derive(Debug, Clone, Copy)]
struct Mini;

impl NetGame for Mini {
    const GAME_ID: &'static str = "handoff-tests";
    const PROTOCOL_VERSION: u32 = 1;
    type JoinData = ();
    type Input = u8;
    type Message = Vec<u8>;
}

fn addr(last: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, last], port))
}

fn identity(seed: u8) -> ServerIdentity {
    ServerIdentity::derived(&[seed; 32]).expect("key pair")
}

fn link(last_heard_ms: u64) -> HostLink {
    HostLink {
        last_heard_ms,
        ended: false,
    }
}

/// A host with players 1 to `n` at 10.0.0.(10 + id), successors 1 (first) and 2, both announced,
/// and one client state machine per player holding its roster (and grant).
fn scripted(n: u32) -> (HandoffHost, BTreeMap<u32, HandoffClient>) {
    let mut host = HandoffHost::with_secret(HandoffConfig::default(), [7; 32]);
    let peers: Vec<(PeerId, SocketAddr)> = (1..=n).map(|i| (PeerId(i), addr(10 + i as u8, 5000))).collect();
    host.set_peers(&peers);
    host.set_successors(&[PeerId(1), PeerId(2)]);
    let mut clients: BTreeMap<u32, HandoffClient> = (1..=n)
        .map(|i| {
            let mut c = HandoffClient::new(HandoffConfig::default()).with_identity(identity(i as u8));
            c.set_peer(PeerId(i));
            (i, c)
        })
        .collect();
    for i in [1, 2] {
        let c = clients.get_mut(&i).expect("successor");
        assert!(c.set_appointed(true));
        let announce = c.poll_out().expect("an announcement");
        assert!(is_handoff_payload(&announce));
        host.on_payload(PeerId(i), &announce).expect("announce");
        assert_eq!(c.poll_out(), None, "one announcement per appointment");
    }
    for out in host.poll() {
        assert!(is_handoff_payload(&out.bytes));
        clients
            .get_mut(&out.peer.0)
            .expect("a player")
            .on_payload(&out.bytes)
            .expect("roster or grant");
    }
    assert!(host.poll().is_empty(), "nothing is sent twice");
    (host, clients)
}

/// Every client gets a roster that names both successors with the address the host sees them at
/// and their keys; each successor gets a grant; the ticket a client holds for a successor is the
/// one that successor's plan adopts it with, and it is not the ticket for the other successor.
#[test]
fn every_client_gets_a_roster_and_each_successor_a_grant() {
    let (host, mut clients) = scripted(5);
    assert_eq!(host.ready_successors(), vec![PeerId(1), PeerId(2)]);
    for (i, c) in &clients {
        let roster = c.roster().expect("a roster");
        assert_eq!((roster.epoch, roster.version), (1, 2), "client {i}");
        assert_eq!(roster.successors.len(), 2);
        assert_eq!(roster.successors[0].peer, PeerId(1));
        assert_eq!(roster.successors[0].addr, addr(11, 5000));
        assert_eq!(roster.successors[0].key, identity(1).public());
        assert_eq!(roster.successors[1].addr, addr(12, 5000));
        assert_eq!(roster.successors[1].key, identity(2).public());
        assert_eq!(c.rank(), [1, 2].iter().position(|s| s == i).map(|r| r as u8));
    }
    // Host lost: the first successor's plan.
    let first = clients.get_mut(&1).expect("first");
    let Some(HandoffAction::Promote(plan)) = first.poll(2_000, link(0), true) else {
        panic!("the first successor promotes");
    };
    assert_eq!((plan.rank, plan.own_peer, plan.epoch), (0, PeerId(1), 2));
    assert_eq!(plan.identity.public(), identity(1).public());
    assert_eq!(
        plan.adopt.iter().map(|(p, _)| p.0).collect::<Vec<_>>(),
        vec![2, 3, 4, 5],
        "every other player is held"
    );
    assert_eq!(
        plan.host_seat,
        PeerId(6),
        "the lost host's seat is the lowest id above every player"
    );
    for (peer, ticket) in &plan.adopt {
        let roster = clients[&peer.0].roster().expect("roster");
        assert_eq!(
            &roster.successors[0].ticket, ticket,
            "client {peer} holds the ticket the successor expects"
        );
        assert_ne!(
            roster.successors[0].ticket, roster.successors[1].ticket,
            "one ticket per successor"
        );
    }
    // The second successor's grant makes other tickets: it cannot make one for the first's server.
    let second = clients.get_mut(&2).expect("second");
    let mut plan2 = None;
    for now in [2_000, 2_001, 5_001] {
        if let Some(HandoffAction::Promote(p)) = second.poll(now, link(0), true) {
            plan2 = Some(p);
        }
    }
    let plan2 = plan2.expect("the second successor promotes after the first's 3 s");
    assert_eq!((plan2.rank, plan2.own_peer), (1, PeerId(2)));
    for (peer, ticket) in &plan2.adopt {
        if let Some((_, first_ticket)) = plan.adopt.iter().find(|(p, _)| p == peer) {
            assert_ne!(
                ticket, first_ticket,
                "peer {peer}: the successors' tickets differ"
            );
        }
    }
    assert_ne!(plan.host_ticket, plan2.host_ticket);
    // The lost host's own way back uses the tickets the plans hold for its seat.
    let mut back = host.comeback(9_000).expect("a way back");
    let Some(HandoffAction::Resume(target)) = back.poll(9_000, link(0), false) else {
        panic!("the lost host resumes to the first successor");
    };
    assert_eq!(
        (target.rank, target.peer, target.ticket),
        (0, PeerId(1), plan.host_ticket)
    );
    assert_eq!(back.status().role, "returning_host");
}

/// The timeline of spec 120 § 10.5 on a scripted clock: nothing before 1.5 s of silence; then
/// every client resumes to the first successor; the first does not answer, so exactly 3 s later
/// the second successor promotes and the others resume to it; nobody answers there either, so
/// 3 s after that the clients are told to go back.
#[test]
fn the_second_successor_takes_over_when_the_first_does_not_answer_within_three_seconds() {
    let (_host, mut clients) = scripted(4);
    let heard = 10_000;
    for c in clients.values_mut() {
        assert!(
            c.poll(heard + 1_499, link(heard), true).is_none(),
            "1,499 ms of silence is not a loss"
        );
        assert!(!c.migrating());
    }
    // The first successor is dead: it never polls again. The others see the loss at 1.5 s.
    let lost = heard + 1_500;
    for i in [2, 3, 4] {
        let c = clients.get_mut(&i).expect("client");
        let Some(HandoffAction::Resume(t)) = c.poll(lost, link(heard), true) else {
            panic!("client {i} resumes to the first successor");
        };
        assert_eq!(
            (t.rank, t.peer, t.addr, t.takeover_ms),
            (0, PeerId(1), addr(11, 5000), 3_000)
        );
        assert_eq!(t.server_key, identity(1).public());
        let config = t.client_config(ClientConfig::default());
        assert_eq!(config.handshake_resend_ms, HANDOFF_HANDSHAKE_RESEND_MS);
        assert_eq!(config.resume, Some(t.ticket));
        assert_eq!(
            config.server_key,
            Some(identity(1).public()),
            "the successor's key is pinned"
        );
        assert_eq!(config.connect_timeout_ms, 3_000);
        assert_eq!(c.poll_event(), Some(HandoffEvent::HostLost { silent_ms: 1_500 }));
        assert_eq!(
            c.poll_event(),
            Some(HandoffEvent::Resuming {
                rank: 0,
                peer: PeerId(1)
            })
        );
        assert!(
            c.poll(lost + 2_999, link(heard), true).is_none(),
            "client {i}: the first has 3 s"
        );
    }
    let second = clients.get_mut(&2).expect("second");
    let Some(HandoffAction::Promote(plan)) = second.poll(lost + 3_000, link(heard), true) else {
        panic!("the second successor takes over at 3 s");
    };
    assert_eq!((plan.rank, plan.own_peer), (1, PeerId(2)));
    assert_eq!(
        second.poll_event(),
        Some(HandoffEvent::SuccessorSilent { rank: 0 })
    );
    assert_eq!(
        second.poll_event(),
        Some(HandoffEvent::Promoted {
            rank: 1,
            after_ms: 3_000
        })
    );
    assert!(second.is_host());
    for i in [3, 4] {
        let c = clients.get_mut(&i).expect("client");
        let Some(HandoffAction::Resume(t)) = c.poll(lost + 3_000, link(heard), true) else {
            panic!("client {i} resumes to the second successor");
        };
        assert_eq!((t.rank, t.peer, t.addr), (1, PeerId(2), addr(12, 5000)));
        assert_eq!(
            plan.adopt.iter().find(|(p, _)| p.0 == i).map(|(_, t)| *t),
            Some(t.ticket),
            "client {i} presents the ticket the second successor holds"
        );
    }
    // Client 3 connects 400 ms later; client 4 gets no answer and is sent back at 6 s.
    let c3 = clients.get_mut(&3).expect("client 3");
    c3.resumed(lost + 3_400, PeerId(3));
    let events: Vec<HandoffEvent> = std::iter::from_fn(|| c3.poll_event()).collect();
    assert!(
        events.contains(&HandoffEvent::Resumed {
            rank: 1,
            after_ms: 3_400
        }),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, HandoffEvent::PeerIdChanged { .. }))
    );
    assert!(!c3.migrating());
    assert_eq!(c3.status().state, "resumed");
    assert_eq!(c3.status().last_handoff_ms, Some(3_400));
    assert!(c3.roster().is_none(), "the lost host's roster is dropped");
    let c4 = clients.get_mut(&4).expect("client 4");
    assert!(c4.poll(lost + 5_999, link(heard), true).is_none());
    assert!(matches!(
        c4.poll(lost + 6_000, link(heard), true),
        Some(HandoffAction::ReturnToHost)
    ));
    assert_eq!(c4.status().state, "abandoned");
    assert!(c4.poll(lost + 60_000, link(heard), true).is_none(), "told once");
    // The host is heard again much later (client 4 kept that connection): it watches it again.
    assert!(c4.poll(lost + 61_000, link(lost + 60_500), true).is_none());
    let events: Vec<HandoffEvent> = std::iter::from_fn(|| c4.poll_event()).collect();
    assert_eq!(
        events.last(),
        Some(&HandoffEvent::HostRestored { after_ms: 61_000 })
    );
    assert_eq!(c4.status().state, "watching");
    // The promoted second successor counts who came: 3 of {1, 3, 4}; with itself that is 2 of 4,
    // no majority.
    let second = clients.get_mut(&2).expect("second");
    second.peer_resumed(lost + 3_400, PeerId(3));
    second.peer_resumed(lost + 3_400, PeerId(3));
    second.peer_resumed(lost + 3_400, PeerId(77));
    let events: Vec<HandoffEvent> = std::iter::from_fn(|| second.poll_event()).collect();
    assert_eq!(
        events,
        vec![HandoffEvent::PeerResumed {
            peer: PeerId(3),
            after_ms: 400
        }]
    );
    assert!(second.poll(lost + 12_999, link(heard), true).is_none());
    assert!(second.poll(lost + 13_000, link(heard), true).is_none());
    assert_eq!(
        second.poll_event(),
        Some(HandoffEvent::NoQuorum {
            resumed: 1,
            expected: 3
        })
    );
    second.peer_resumed(lost + 13_500, PeerId(4));
    let events: Vec<HandoffEvent> = std::iter::from_fn(|| second.poll_event()).collect();
    assert!(
        events.contains(&HandoffEvent::Quorum { after_ms: 10_500 }),
        "{events:?}"
    );
    let status = second.status();
    assert_eq!(
        (status.role.as_str(), status.resumed_peers, status.quorum),
        ("promoted", 2, true)
    );
}

/// A successor whose shadow is not promotable, or that holds no grant for its key, does not
/// become the server: it passes its turn at once and resumes to the next successor. A client
/// with no roster is told to go back to its host. A connection that ended is a loss at once.
#[test]
fn a_successor_that_cannot_promote_passes_its_turn() {
    let (_host, mut clients) = scripted(3);
    let first = clients.get_mut(&1).expect("first");
    let Some(HandoffAction::Resume(t)) = first.poll(
        50,
        HostLink {
            last_heard_ms: 40,
            ended: true,
        },
        false,
    ) else {
        panic!("a cold first successor resumes to the second");
    };
    assert_eq!((t.rank, t.peer), (1, PeerId(2)));
    assert_eq!(first.poll_event(), Some(HandoffEvent::HostLost { silent_ms: 0 }));
    assert_eq!(first.poll_event(), Some(HandoffEvent::NotPromotable { rank: 0 }));
    // No grant: the roster lists this client with a key it did not announce.
    let mut c = HandoffClient::new(HandoffConfig::default()).with_identity(identity(9));
    c.set_peer(PeerId(1));
    let other = clients.get(&3).expect("client 3");
    let mut roster = other.roster().expect("roster").clone();
    roster.successors.truncate(1);
    c.on_payload(&roster.encode()).expect("roster");
    assert!(matches!(
        c.poll(2_000, link(0), true),
        Some(HandoffAction::ReturnToHost)
    ));
    assert!(!c.status().granted);
    // No roster at all.
    let mut bare = HandoffClient::new(HandoffConfig::default());
    assert!(
        bare.poll(99_000, link(0), true).is_none(),
        "never connected: nothing to lose"
    );
    bare.set_peer(PeerId(4));
    assert!(matches!(
        bare.poll(99_000, link(0), true),
        Some(HandoffAction::ReturnToHost)
    ));
    assert_eq!(
        bare.poll_event(),
        Some(HandoffEvent::HostLost { silent_ms: 99_000 })
    );
    assert_eq!(bare.poll_event(), Some(HandoffEvent::NoSuccessor));
}

/// The roster follows the journal's successors and their order: when the journal makes the second
/// successor the first, the roster does; a successor that leaves is dropped from it; a new
/// player gets the current roster and every grant lists it; a player that did not change is not
/// sent anything again.
#[test]
fn the_roster_follows_the_successors_and_the_players() {
    let (mut host, mut clients) = scripted(4);
    // The journal's order changes: the second successor is the first now.
    host.set_successors(&[PeerId(2), PeerId(1)]);
    assert_eq!(host.ready_successors(), vec![PeerId(2), PeerId(1)]);
    let outs = host.poll();
    assert_eq!(outs.len(), 4 + 2, "four rosters and two grants");
    for out in outs {
        clients
            .get_mut(&out.peer.0)
            .expect("player")
            .on_payload(&out.bytes)
            .expect("payload");
    }
    assert_eq!(
        clients[&3].roster().expect("roster").successors[0].peer,
        PeerId(2)
    );
    assert_eq!(clients[&2].rank(), Some(0));
    // A player joins: one roster for it, one grant per successor, nothing for the others.
    let peers: Vec<(PeerId, SocketAddr)> = (1..=5).map(|i| (PeerId(i), addr(10 + i as u8, 5000))).collect();
    host.set_peers(&peers);
    let outs = host.poll();
    let rosters: Vec<u32> = outs
        .iter()
        .filter(|o| o.bytes[0] == KIND_ROSTER)
        .map(|o| o.peer.0)
        .collect();
    let grants: Vec<u32> = outs
        .iter()
        .filter(|o| o.bytes[0] == KIND_GRANT)
        .map(|o| o.peer.0)
        .collect();
    assert_eq!((rosters, grants), (vec![5], vec![2, 1]));
    for out in outs {
        if let Some(c) = clients.get_mut(&out.peer.0) {
            c.on_payload(&out.bytes).expect("payload");
        }
    }
    let Some(HandoffAction::Promote(plan)) = clients.get_mut(&2).expect("second").poll(2_000, link(0), true)
    else {
        panic!("the journal's first successor promotes first");
    };
    assert_eq!(
        plan.adopt.iter().map(|(p, _)| p.0).collect::<Vec<_>>(),
        vec![1, 3, 4, 5]
    );
    assert_eq!(plan.host_seat, PeerId(6));
    // The first successor leaves: it is in no roster any more, and an older roster is refused.
    let old = clients[&3].roster().expect("roster").clone();
    let peers: Vec<(PeerId, SocketAddr)> = (2..=5).map(|i| (PeerId(i), addr(10 + i as u8, 5000))).collect();
    host.set_peers(&peers);
    host.set_successors(&[PeerId(2)]);
    assert_eq!(host.ready_successors(), vec![PeerId(2)]);
    for out in host.poll() {
        if let Some(c) = clients.get_mut(&out.peer.0) {
            c.on_payload(&out.bytes).expect("payload");
        }
    }
    let c3 = clients.get_mut(&3).expect("client 3");
    assert_eq!(c3.roster().expect("roster").successors.len(), 1);
    assert_eq!(c3.on_payload(&old.encode()), Err(HandoffError::Stale));
    assert_eq!(c3.status().last_refusal.as_deref(), Some("handoff_stale"));
    // An announcement from somebody who is not a player is refused; so is a client's kind sent
    // to the host and the host's kinds sent to a host.
    let announce = Announce {
        key: identity(40).public(),
        port: 7777,
    }
    .encode();
    assert_eq!(
        host.on_payload(PeerId(99), &announce),
        Err(HandoffError::NotAPlayer)
    );
    assert_eq!(
        host.on_payload(PeerId(2), &old.encode()),
        Err(HandoffError::WrongKind(KIND_ROSTER))
    );
    assert_eq!(
        c3.on_payload(&announce),
        Err(HandoffError::WrongKind(KIND_ANNOUNCE))
    );
    // An announced port replaces the port of the address the host sees.
    host.on_payload(PeerId(2), &announce).expect("a new announcement");
    assert_eq!(host.status().successors[0].addr, addr(12, 7777).to_string());
    assert_eq!(host.status().successors[0].key, identity(40).public().to_string());
    assert_eq!(host.status().state, "ready");
    assert!(host.deposed(true));
    assert!(!host.deposed(false));
}

/// Hostile payloads: every refusal has its code, lengths are checked before anything is stored,
/// and 60,000 seeded mutations of the three payloads decode without a panic.
#[test]
fn hostile_handoff_payloads_are_refused_with_codes_and_mutations_never_panic() {
    let (mut host, mut clients) = scripted(3);
    let roster = clients[&3].roster().expect("roster").clone();
    let c = clients.get_mut(&3).expect("client");
    fn code(c: &mut HandoffClient, bytes: &[u8]) -> Result<(), &'static str> {
        c.on_payload(bytes).map_err(|e| e.code())
    }
    assert_eq!(code(c, &[]), Err("handoff_malformed"));
    assert_eq!(code(c, &[0x47]), Err("handoff_wrong_kind"));
    assert_eq!(code(c, &[KIND_ROSTER, 1, 2, 3]), Err("handoff_malformed"));
    let mut bytes = roster.encode();
    bytes.push(0);
    assert_eq!(code(c, &bytes), Err("handoff_malformed"), "trailing bytes");
    let mut three = roster.clone();
    three.version += 1;
    three.successors.push(three.successors[0].clone());
    assert_eq!(code(c, &three.encode()), Err("handoff_too_many_successors"));
    let mut twice = roster.clone();
    twice.version += 1;
    twice.successors[1].peer = twice.successors[0].peer;
    assert_eq!(code(c, &twice.encode()), Err("handoff_duplicate_peer"));
    let mut zero = roster.clone();
    zero.successors[0].ticket = [0; RESUME_TICKET_BYTES];
    assert_eq!(code(c, &zero.encode()), Err("handoff_zero_ticket"));
    zero.successors[0].key = ServerKey([0; 32]);
    assert_eq!(code(c, &zero.encode()), Err("handoff_zero_key"));
    let mut slow = roster.clone();
    slow.takeover_ms = 60_001;
    assert_eq!(code(c, &slow.encode()), Err("handoff_bad_threshold"));
    slow.takeover_ms = 3_000;
    slow.host_lost_ms = 0;
    assert_eq!(code(c, &slow.encode()), Err("handoff_bad_threshold"));
    let grant = Grant {
        epoch: 1,
        version: 9,
        key: identity(1).public(),
        secret: [3; 32],
        peers: vec![PeerId(2), PeerId(3)],
    };
    let good = grant.encode();
    assert_eq!(code(c, &good), Ok(()));
    let mut many = good.clone();
    let at = 1 + 4 + 4 + 32 + 32;
    many[at..at + 2].copy_from_slice(&2_000u16.to_le_bytes());
    assert_eq!(code(c, &many), Err("handoff_too_many_peers"));
    many[at..at + 2].copy_from_slice(&1_000u16.to_le_bytes());
    assert_eq!(
        code(c, &many),
        Err("handoff_malformed"),
        "a count the bytes do not hold"
    );
    let mut dup = grant.clone();
    dup.version = 10;
    dup.peers = vec![PeerId(2), PeerId(2)];
    assert_eq!(code(c, &dup.encode()), Err("handoff_duplicate_peer"));
    let mut older = grant.clone();
    older.version = 8;
    assert_eq!(code(c, &older.encode()), Err("handoff_stale"));
    assert_eq!(
        host.on_payload(PeerId(1), &[KIND_ANNOUNCE; 35])
            .map_err(|e| e.code()),
        Ok(())
    );
    assert_eq!(
        host.on_payload(PeerId(1), &[KIND_ANNOUNCE, 0, 0])
            .map_err(|e| e.code()),
        Err("handoff_malformed")
    );
    let mut zero_key = vec![0u8; 35];
    zero_key[0] = KIND_ANNOUNCE;
    assert_eq!(
        host.on_payload(PeerId(1), &zero_key).map_err(|e| e.code()),
        Err("handoff_zero_key")
    );
    let refused = c.status().refused;
    assert!(refused >= 12, "{refused}");
    // Mutations: flip, cut and extend each payload.
    let announce = Announce {
        key: identity(5).public(),
        port: 9,
    }
    .encode();
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for source in [roster.encode(), good, announce] {
        for _ in 0..20_000 {
            let mut bytes = source.clone();
            for _ in 0..=(next() % 3) {
                let at = (next() as usize) % bytes.len();
                bytes[at] = next() as u8;
            }
            match next() % 4 {
                0 => bytes.truncate((next() as usize) % (bytes.len() + 1)),
                1 => bytes.extend((0..next() % 9).map(|i| i as u8)),
                _ => {}
            }
            let _ = c.on_payload(&bytes);
            let _ = host.on_payload(PeerId(1 + (next() % 4) as u32), &bytes);
            assert!(
                c.roster()
                    .is_none_or(|r| r.successors.len() <= MAX_HANDOFF_SUCCESSORS)
            );
            assert!(
                c.grant
                    .as_ref()
                    .is_none_or(|g| g.peers.len() <= MAX_HANDOFF_PEERS)
            );
            assert!(host.announces.len() <= 3);
        }
    }
}

/// A client in its own short outage: it sees the host lost and starts a resume to the first
/// successor (which, not being in the outage, never promoted). When the host is heard again the
/// hand-off is cancelled for this client: one `StayWithHost`, the roster is kept, and the next
/// loss starts from the first successor again. A connection that ended is never restored.
#[test]
fn a_host_heard_again_cancels_the_hand_off_for_that_client() {
    let (_host, mut clients) = scripted(4);
    let c = clients.get_mut(&3).expect("client 3");
    let heard = 20_000;
    assert!(matches!(
        c.poll(heard + 1_500, link(heard), false),
        Some(HandoffAction::Resume(ResumeTarget { rank: 0, .. }))
    ));
    assert!(c.migrating());
    // Still nothing from the host: no change.
    assert!(c.poll(heard + 2_000, link(heard), false).is_none());
    // A datagram from the host at 22.3 s.
    assert!(matches!(
        c.poll(heard + 2_310, link(heard + 2_300), false),
        Some(HandoffAction::StayWithHost)
    ));
    assert!(!c.migrating());
    let events: Vec<HandoffEvent> = std::iter::from_fn(|| c.poll_event()).collect();
    assert_eq!(events.last(), Some(&HandoffEvent::HostRestored { after_ms: 810 }));
    let status = c.status();
    assert_eq!(
        (status.state.as_str(), status.host_losses, status.last_handoff_ms),
        ("watching", 1, None)
    );
    assert_eq!(c.roster().expect("the roster is kept").successors.len(), 2);
    assert!(c.poll(heard + 3_000, link(heard + 2_990), false).is_none());
    // The next loss is a real one: the connection ended.
    assert!(matches!(
        c.poll(heard + 9_000, HostLink::gone(), false),
        Some(HandoffAction::Resume(ResumeTarget { rank: 0, .. }))
    ));
    assert!(
        c.poll(heard + 9_100, HostLink::gone(), false).is_none(),
        "an ended connection is not restored"
    );
    assert_eq!(c.status().host_losses, 2);
}

/// A successor that announced a port arms before it takes the match over: at its turn it opens
/// its server and waits. A majority of the players resuming to it commits it; hearing the host
/// again disarms it (its own outage: nothing happened); its own connection ending commits it
/// without a majority, and that is reported as `NoQuorum` after 10 s.
#[test]
fn a_successor_with_a_port_arms_and_takes_over_only_with_a_majority() {
    let armed = || {
        let mut host = HandoffHost::with_secret(HandoffConfig::default(), [7; 32]);
        let peers: Vec<(PeerId, SocketAddr)> =
            (1..=5).map(|i| (PeerId(i), addr(10 + i as u8, 5000))).collect();
        host.set_peers(&peers);
        host.set_successors(&[PeerId(1), PeerId(2)]);
        let mut c = HandoffClient::new(HandoffConfig::default())
            .with_identity(identity(1))
            .with_port(7001);
        c.set_peer(PeerId(1));
        assert!(c.set_appointed(true));
        host.on_payload(PeerId(1), &c.poll_out().expect("announcement"))
            .expect("announce");
        for out in host.poll() {
            if out.peer == PeerId(1) {
                c.on_payload(&out.bytes).expect("roster or grant");
            }
        }
        assert_eq!(
            c.roster().expect("roster").successors[0].addr,
            addr(11, 7001),
            "the roster names the announced port at the address the host sees"
        );
        let Some(HandoffAction::Arm(plan)) = c.poll(11_500, link(10_000), true) else {
            panic!("a successor with a port arms");
        };
        assert_eq!((plan.rank, plan.port, plan.adopt.len()), (0, 7001, 4));
        assert_eq!(c.poll_event(), Some(HandoffEvent::HostLost { silent_ms: 1_500 }));
        assert_eq!(c.poll_event(), Some(HandoffEvent::Armed { rank: 0 }));
        assert!(c.is_armed() && !c.is_host() && !c.migrating());
        assert_eq!(c.status().state, "armed");
        assert!(
            c.poll(11_600, link(10_000), true).is_none(),
            "armed, nobody here yet"
        );
        c
    };
    // A majority arrives: with the successor itself, 3 of the 5 clients of the lost host.
    let mut c = armed();
    c.peer_resumed(11_700, PeerId(3));
    assert!(
        c.poll(11_710, link(10_000), true).is_none(),
        "one player is not a majority"
    );
    assert_eq!(c.status().resumed_peers, 1);
    c.peer_resumed(11_800, PeerId(4));
    assert!(matches!(
        c.poll(11_810, link(10_000), true),
        Some(HandoffAction::Commit)
    ));
    assert!(c.is_host());
    let events: Vec<HandoffEvent> = std::iter::from_fn(|| c.poll_event()).collect();
    assert_eq!(
        events,
        vec![
            HandoffEvent::PeerResumed {
                peer: PeerId(3),
                after_ms: 200
            },
            HandoffEvent::PeerResumed {
                peer: PeerId(4),
                after_ms: 300
            },
            HandoffEvent::Promoted {
                rank: 0,
                after_ms: 310
            },
            HandoffEvent::Quorum { after_ms: 0 },
        ]
    );
    c.peer_resumed(12_000, PeerId(2));
    c.peer_resumed(12_100, PeerId(5));
    let events: Vec<HandoffEvent> = std::iter::from_fn(|| c.poll_event()).collect();
    assert!(
        events.contains(&HandoffEvent::AllResumed { after_ms: 290 }),
        "{events:?}"
    );
    let status = c.status();
    assert_eq!(
        (
            status.role.as_str(),
            status.resumed_peers,
            status.expected_peers,
            status.quorum
        ),
        ("promoted", 4, 5, true)
    );
    assert_eq!(status.last_handoff_ms, Some(310));
    // The host is heard again: the silence was the successor's own outage.
    let mut c = armed();
    c.peer_resumed(11_700, PeerId(3));
    assert!(matches!(
        c.poll(12_410, link(12_400), true),
        Some(HandoffAction::Disarm)
    ));
    assert!(!c.is_armed() && !c.is_host());
    let events: Vec<HandoffEvent> = std::iter::from_fn(|| c.poll_event()).collect();
    assert_eq!(events.last(), Some(&HandoffEvent::HostRestored { after_ms: 910 }));
    assert_eq!(c.status().state, "watching");
    assert!(c.poll(12_500, link(12_490), true).is_none());
    // The next loss arms again.
    assert!(matches!(
        c.poll(14_000, link(12_490), true),
        Some(HandoffAction::Arm(_))
    ));
    // The successor's own connection ends (the host said goodbye, or the transport gave up): it
    // takes over without a majority and says so after 10 s.
    let mut c = armed();
    assert!(matches!(
        c.poll(
            12_000,
            HostLink {
                last_heard_ms: 10_000,
                ended: true
            },
            true
        ),
        Some(HandoffAction::Commit)
    ));
    assert!(c.is_host());
    assert!(c.poll(21_999, HostLink::gone(), true).is_none());
    let events: Vec<HandoffEvent> = std::iter::from_fn(|| c.poll_event()).collect();
    assert!(
        !events.iter().any(|e| matches!(e, HandoffEvent::NoQuorum { .. })),
        "{events:?}"
    );
    assert!(c.poll(22_000, HostLink::gone(), true).is_none());
    assert_eq!(
        c.poll_event(),
        Some(HandoffEvent::NoQuorum {
            resumed: 0,
            expected: 4
        })
    );
}

struct Player {
    net: Option<NetClient<Mini, LoopbackSocket>>,
    handoff: HandoffClient,
    peer: Option<PeerId>,
    resumed_as: Option<PeerId>,
}

/// The whole hand-off on real endpoints (a loopback net): four clients of a host, the host
/// stops, the first successor opens a server on its own socket under the announced key with
/// every player held dormant, the others resume with their tickets and keep their `PeerId`s, the
/// lost host comes back as a client on the seat the successor kept for it, a ticket made for the
/// other successor's server is not honored, and a stranger gets none of the held ids.
#[test]
fn clients_resume_to_the_promoted_successor_with_their_peer_ids() {
    let net = LoopbackNet::new();
    let host_addr = addr(1, 7000);
    let mut server: NetServer<Mini, _> = NetServer::with_secret(
        net.bind(host_addr).expect("bind"),
        ServerConfig {
            max_peers: 8,
            tick_rate_hz: 60,
            ..ServerConfig::default()
        },
        [9; 32],
    );
    let mut players: Vec<Player> = (0..4u8)
        .map(|i| Player {
            net: Some(
                NetClient::<Mini, _>::connect(
                    net.bind(addr(2 + i, 5000)).expect("bind"),
                    host_addr,
                    ClientConfig {
                        name: format!("player{i}"),
                        ..ClientConfig::default()
                    },
                    &(),
                )
                .expect("connect"),
            ),
            handoff: HandoffClient::new(HandoffConfig::default()).with_identity(identity(i + 1)),
            peer: None,
            resumed_as: None,
        })
        .collect();
    let mut host = HandoffHost::with_secret(HandoffConfig::default(), [7; 32]);
    let mut now = 0u64;
    // The match runs for two seconds; players 0 and 1 are appointed at 0.5 s.
    while now < 2_000 {
        now += 10;
        server.receive(now);
        while let Some(ev) = server.poll_event() {
            if let ServerEvent::Message { peer, message, .. } = ev
                && is_handoff_payload(&message)
            {
                host.on_payload(peer, &message).expect("announcement");
            }
        }
        // No journal here: the successors are set by hand.
        let peers: Vec<(PeerId, SocketAddr)> = server.session().peers().map(|p| (p.id, p.addr)).collect();
        host.set_peers(&peers);
        if now >= 500
            && let (Some(a), Some(b)) = (players[0].peer, players[1].peer)
        {
            host.set_successors(&[a, b]);
        }
        for out in host.poll() {
            server
                .send(out.peer, Channel::GAME_RPC, &out.bytes)
                .expect("send");
        }
        server.flush(now);
        for (i, p) in players.iter_mut().enumerate() {
            let c = p.net.as_mut().expect("connected");
            c.receive(now);
            while let Some(ev) = c.poll_event() {
                match ev {
                    ClientEvent::Connected { peer, .. } => {
                        p.peer = Some(peer);
                        p.handoff.set_peer(peer);
                    }
                    ClientEvent::Message { message, .. } if is_handoff_payload(&message) => {
                        p.handoff.on_payload(&message).expect("roster or grant");
                    }
                    _ => {}
                }
            }
            p.handoff.set_appointed(now >= 500 && i < 2);
            if let Some(announce) = p.handoff.poll_out() {
                c.send(Channel::GAME_RPC, &announce).expect("send");
            }
            let heard = HostLink {
                last_heard_ms: c.last_heard_ms(),
                ended: false,
            };
            assert!(p.handoff.poll(now, heard, i < 2).is_none(), "the host is alive");
            c.flush(now);
        }
    }
    let ids: Vec<PeerId> = players.iter().map(|p| p.peer.expect("joined")).collect();
    for p in &players {
        assert_eq!(p.handoff.roster().expect("a roster").successors.len(), 2);
    }
    assert!(players[0].handoff.status().granted);
    assert_eq!(players[3].handoff.status().role, "client");
    // The host stops: nothing more is received or sent by it.
    let lost_at = now;
    drop(server);
    let mut promoted: Option<NetServer<Mini, LoopbackSocket>> = None;
    let mut plan_kept: Option<PromotionPlan> = None;
    let mut stale_ticket = None;
    let mut resumed_events: Vec<PeerId> = Vec::new();
    while now < lost_at + 3_000 {
        now += 10;
        if let Some(server) = promoted.as_mut() {
            server.receive(now);
            while let Some(ev) = server.poll_event() {
                match ev {
                    ServerEvent::PeerResumed { peer, .. } => {
                        resumed_events.push(peer);
                        players[0].handoff.peer_resumed(now, peer);
                    }
                    ServerEvent::PeerJoined { peer, .. } => panic!("{peer} joined as a new peer"),
                    _ => {}
                }
            }
            server.advance_tick(|_, _| {});
            server.flush(now);
        }
        for (i, p) in players.iter_mut().enumerate() {
            if p.handoff.is_host() {
                let _ = p.handoff.poll(now, link(0), true);
                continue;
            }
            // One socket per player here: the connection to the lost host is given up for the
            // resume attempt, so from then on its link is gone.
            let heard = if p.handoff.migrating() {
                HostLink::gone()
            } else {
                HostLink {
                    last_heard_ms: p.net.as_ref().map_or(0, NetClient::last_heard_ms),
                    ended: false,
                }
            };
            match p.handoff.poll(now, heard, i < 2) {
                Some(HandoffAction::Promote(plan)) => {
                    assert_eq!(i, 0, "only the first successor promotes");
                    let socket = p.net.take().expect("client").into_socket();
                    let mut server = NetServer::<Mini, _>::new(
                        socket,
                        plan.server_config(ServerConfig {
                            max_peers: 8,
                            tick_rate_hz: 60,
                            ..ServerConfig::default()
                        }),
                    );
                    assert_eq!(
                        plan.apply(&mut server),
                        Ok(4),
                        "three players and the lost host's seat"
                    );
                    assert_eq!(plan.apply(&mut server), Err(AdoptError::IdInUse), "not twice");
                    assert!(server.start_at_tick(1_000));
                    assert_eq!(server.server_key(), Some(plan.identity.public()));
                    assert_eq!(server.dormant_peers().count(), 4);
                    promoted = Some(server);
                    plan_kept = Some(*plan);
                }
                Some(HandoffAction::Resume(target)) => {
                    assert_eq!(target.rank, 0);
                    assert_eq!(target.addr, addr(2, 5000), "the first successor's own address");
                    if i == 3 {
                        // Kept for later: client 3's ticket for the second successor's server.
                        stale_ticket = Some(p.handoff.roster().expect("roster").successors[1].ticket);
                    }
                    let socket = p.net.take().expect("client").into_socket();
                    p.net = Some(
                        NetClient::<Mini, _>::connect(
                            socket,
                            target.addr,
                            target.client_config(ClientConfig {
                                name: format!("player{i}"),
                                ..ClientConfig::default()
                            }),
                            &(),
                        )
                        .expect("connect"),
                    );
                }
                Some(HandoffAction::ReturnToHost) => panic!("player {i} found no successor"),
                Some(HandoffAction::StayWithHost) => panic!("player {i}: the host is gone"),
                Some(other) => panic!("player {i}: {other:?} without an announced port"),
                None => {}
            }
            if let Some(c) = p.net.as_mut() {
                c.receive(now);
                while let Some(ev) = c.poll_event() {
                    match ev {
                        ClientEvent::Connected { peer, .. } if p.handoff.migrating() => {
                            p.handoff.resumed(now, peer);
                            p.resumed_as = Some(peer);
                        }
                        ClientEvent::Ended(end) => panic!("player {i}: {end:?}"),
                        _ => {}
                    }
                }
                c.flush(now);
            }
        }
    }
    let plan = plan_kept.expect("a promotion");
    let mut server = promoted.expect("a promoted server");
    assert_eq!(plan.own_peer, ids[0]);
    for (i, (p, id)) in players.iter().zip(&ids).enumerate().skip(1) {
        assert_eq!(p.resumed_as, Some(*id), "player {i} keeps its PeerId");
        let c = p.net.as_ref().expect("client");
        assert!(c.is_sealed(), "the resumed link is sealed");
        assert_eq!(c.server_key(), Some(plan.identity.public()));
        assert!(p.handoff.status().last_handoff_ms.expect("measured") < 1_000);
    }
    resumed_events.sort();
    assert_eq!(resumed_events, ids[1..].to_vec());
    assert_eq!(server.totals().resumes, 3);
    assert!(
        server.driver().tick() >= 1_000,
        "the server carries on from the promoted tick"
    );
    let events: Vec<HandoffEvent> = std::iter::from_fn(|| players[0].handoff.poll_event()).collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HandoffEvent::Promoted { rank: 0, .. })),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, HandoffEvent::Quorum { .. })),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HandoffEvent::AllResumed { .. })),
        "{events:?}"
    );
    // The lost host comes back as a client and gets the seat the successor kept for it; a
    // ticket for the other successor's server is not honored (that join is a new peer); a
    // stranger gets none of the held ids.
    let mut back = host.comeback(now).expect("a way back");
    let Some(HandoffAction::Resume(target)) = back.poll(now, HostLink::gone(), false) else {
        panic!("the lost host resumes");
    };
    let connect = |from: SocketAddr, config: ClientConfig| {
        NetClient::<Mini, _>::connect(net.bind(from).expect("bind"), target.addr, config, &())
            .expect("connect")
    };
    let mut old_host = connect(addr(1, 7001), target.client_config(ClientConfig::default()));
    let mut wrong = connect(
        addr(9, 5000),
        ClientConfig {
            resume: stale_ticket,
            server_key: Some(target.server_key),
            ..ClientConfig::default()
        },
    );
    let mut stranger = connect(addr(8, 5000), ClientConfig::default());
    let mut seats: [Option<PeerId>; 3] = [None; 3];
    let until = now + 1_000;
    while now < until {
        now += 10;
        server.receive(now);
        while let Some(ev) = server.poll_event() {
            if let ServerEvent::PeerResumed { peer, .. } = ev {
                assert_eq!(peer, plan.host_seat, "only the lost host resumes now");
            }
        }
        server.flush(now);
        for (slot, c) in [&mut old_host, &mut wrong, &mut stranger].into_iter().enumerate() {
            c.receive(now);
            while let Some(ev) = c.poll_event() {
                if let ClientEvent::Connected { peer, .. } = ev {
                    seats[slot] = Some(peer);
                }
            }
            c.flush(now);
        }
    }
    assert_eq!(
        seats[0],
        Some(plan.host_seat),
        "the lost host is a client on the seat kept for it"
    );
    back.resumed(now, plan.host_seat);
    assert_eq!(back.status().state, "resumed");
    let held: Vec<PeerId> = ids.iter().copied().chain([plan.host_seat]).collect();
    for (slot, what) in [(1, "a ticket for the other successor"), (2, "a stranger")] {
        let seat = seats[slot].expect("joined as a new peer");
        assert!(!held.contains(&seat), "{what} got the held id {seat}");
    }
    assert_eq!(server.totals().resumes, 4);
    assert_eq!(server.totals().resumes_refused, 1);
    assert_eq!(server.dormant_peers().count(), 0, "every held player is back");
}

/// `NetServer::adopt_dormant` refusals, and the end of an adopted hold: a player that never
/// comes back leaves with `Timeout` after the grace, and its id is free again.
#[test]
fn adopted_peers_are_refused_with_codes_and_expire_after_the_grace() {
    let net = LoopbackNet::new();
    let config = ServerConfig {
        resume_grace_ms: 5_000,
        ..ServerConfig::default()
    };
    let mut off: NetServer<Mini, _> =
        NetServer::new(net.bind(addr(1, 7000)).expect("bind"), ServerConfig::default());
    assert_eq!(
        off.adopt_dormant(PeerId(3), [1; 16]).map_err(AdoptError::code),
        Err("adopt_resume_off")
    );
    let mut server: NetServer<Mini, _> = NetServer::new(net.bind(addr(1, 7001)).expect("bind"), config);
    assert_eq!(
        server.adopt_dormant(PeerId(3), [0; 16]).map_err(AdoptError::code),
        Err("adopt_zero_ticket")
    );
    assert_eq!(
        server
            .adopt_dormant(PeerId::SERVER, [1; 16])
            .map_err(AdoptError::code),
        Err("adopt_server_id")
    );
    assert_eq!(server.adopt_dormant(PeerId(3), [1; 16]), Ok(()));
    assert_eq!(
        server.adopt_dormant(PeerId(3), [2; 16]).map_err(AdoptError::code),
        Err("adopt_id_in_use")
    );
    assert_eq!(
        server.adopt_dormant(PeerId(4), [1; 16]).map_err(AdoptError::code),
        Err("adopt_ticket_in_use")
    );
    assert!(server.keep_peer_id(PeerId(5)));
    assert!(!server.keep_peer_id(PeerId(5)));
    assert!(!server.keep_peer_id(PeerId(3)));
    assert!(!server.keep_peer_id(PeerId::SERVER));
    assert_eq!(
        server.adopt_dormant(PeerId(5), [3; 16]).map_err(AdoptError::code),
        Err("adopt_id_in_use")
    );
    assert!(server.start_at_tick(42));
    assert_eq!(server.driver().tick(), 42);
    server.advance_tick(|tick, _| assert_eq!(tick, 42));
    assert!(!server.start_at_tick(7), "only before the first tick");
    server.receive(4_999);
    assert!(server.poll_event().is_none());
    server.receive(5_000);
    assert!(matches!(
        server.poll_event(),
        Some(ServerEvent::PeerLeft {
            peer: PeerId(3),
            reason: crate::handshake::DisconnectReason::Timeout
        })
    ));
    assert_eq!(
        server.adopt_dormant(PeerId(3), [4; 16]),
        Ok(()),
        "the id is free again"
    );
    server.shutdown();
    assert_eq!(
        server.adopt_dormant(PeerId(6), [5; 16]).map_err(AdoptError::code),
        Err("adopt_closed")
    );
}
