// SPDX-License-Identifier: MPL-2.0
//! Wire 6 (spec 120 § 10.3; ADR-8733 (3) as amended by ADR-8858; ADR-8902): a ticket holder's
//! rejoin over the `asym-bufferbloat` path.
//!
//! The profile's downlink sits behind a one-second queue that cross traffic keeps full, so every
//! round trip of a handshake costs about a second. The full handshake is three round trips and
//! misses the soak's reconnect grace ([`RECONNECT_GRACE_MS`]); the one-round-trip resume makes
//! it with room to spare. Both run here on the impaired in-process network: seeded, virtual
//! time, no wall clock, no sleeps. The server ticks at 30 Hz and the client runs 60 frames per
//! second, as the soak's workload does.

use std::net::SocketAddr;

use regia_net::prelude::*;
use regia_net_soak::game::SoakGame;
use regia_net_soak::impair::{EndpointId, ImpairedNet, ImpairedSocket};
use regia_net_soak::profiles::Profile;
use regia_net_soak::report::{self, RECONNECT_GRACE_MS};
use regia_net_soak::workload::{self, Scenario, ServerMode};
use regia_net_soak::{DEFAULT_SEED, GATE_SECONDS};

type Server = NetServer<SoakGame, ImpairedSocket>;
type Client = NetClient<SoakGame, ImpairedSocket>;

/// The gate plan's seed and three more.
const SEEDS: [u64; 4] = [1_592_590_337, 11, 22, 33];
const SERVER_TICK_MS: u64 = 33;
const CLIENT_FRAME_MS: u64 = 16;
/// The soak's scripted leave stays away this long.
const AWAY_MS: u64 = 2_000;
const RUN_MS: u64 = 120_000;

fn server_addr() -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], 7777))
}

/// How the player went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leave {
    /// A goodbye, like the soak's scripted leave: the return uses the held slot of its address.
    Goodbye,
    /// The process just stops talking: the server still holds the connection when the player is
    /// back, and the return takes it over under the same peer id.
    Silent,
}

struct World {
    net: ImpairedNet,
    server: Server,
    client_ep: EndpointId,
    now: u64,
}

impl World {
    fn new(seed: u64) -> Self {
        let profile = Profile::named("asym-bufferbloat").expect("profile");
        let net = ImpairedNet::new(seed);
        let server_ep = net
            .add_endpoint(server_addr(), profile.server_link(false))
            .expect("server endpoint");
        let client_ep = net
            .add_endpoint(
                SocketAddr::from(([10, 1, 0, 1], 40_000)),
                profile.client_link(0, RUN_MS),
            )
            .expect("client endpoint");
        // A server that keeps tickets, as a `NetLink` server does (60 s).
        let config = ServerConfig {
            resume_grace_ms: 60_000,
            ..ServerConfig::default()
        };
        let server = NetServer::with_secret(net.socket(server_ep), config, [3; 32]);
        Self {
            net,
            server,
            client_ep,
            now: 0,
        }
    }

    fn client(&self, edit: impl FnOnce(&mut ClientConfig)) -> Client {
        let mut config = ClientConfig {
            name: "soak0".into(),
            ..ClientConfig::default()
        };
        edit(&mut config);
        NetClient::connect(self.net.socket(self.client_ep), server_addr(), config, &0u32).expect("connect")
    }

    /// One millisecond of virtual time: the network, then whichever end is due.
    fn tick(&mut self, client: Option<&mut Client>) {
        self.now += 1;
        self.net.advance_to_ms(self.now);
        if let Some(c) = client
            && self.now.is_multiple_of(CLIENT_FRAME_MS)
        {
            c.update(self.now);
        }
        if self.now.is_multiple_of(SERVER_TICK_MS) {
            self.server.update(self.now);
            while self.server.poll_event().is_some() {}
        }
    }

    /// Runs until `c` is connected or has ended, at most `limit_ms`. Returns the milliseconds it
    /// took and the datagrams the client had sent when it connected (resends included: on this
    /// path a handshake datagram is sent several times before its answer is through the queue).
    fn connect(&mut self, c: &mut Client, limit_ms: u64) -> (u64, u64) {
        let start = self.now;
        while self.now - start < limit_ms {
            self.tick(Some(c));
            if c.is_connected() || matches!(c.state(), ClientState::Ended(_)) {
                break;
            }
        }
        (self.now - start, c.totals().datagrams_out)
    }
}

/// What one rejoin took.
#[derive(Debug, Clone, Copy)]
struct Rejoin {
    ms: u64,
    datagrams: u64,
    connected: bool,
    same_peer: bool,
    /// Resume flights the server sent to the full handshake (the client's count).
    fallbacks: u64,
    /// Resume flights the client started.
    flights: u64,
    /// Why the server did not resume the last flight it refused, if it refused one.
    refused: Option<&'static str>,
}

/// A player joins over `asym-bufferbloat`, plays for a second, leaves, stays away two seconds and
/// rejoins from a new port of its address with its ticket, with or without the resumption
/// credential of wire 6.
fn rejoin(seed: u64, leave: Leave, with_credential: bool) -> Rejoin {
    let mut w = World::new(seed);
    let mut first = w.client(|_| {});
    w.connect(&mut first, 20_000);
    assert!(first.is_connected(), "seed {seed}: {:?}", first.state());
    for _ in 0..1_000 {
        w.tick(Some(&mut first));
    }
    let peer = first.peer_id();
    let ticket = first.resume_ticket();
    let credential = first.resumption();
    assert!(
        credential.is_some(),
        "a server that keeps tickets seals one into its accept"
    );
    match leave {
        Leave::Goodbye => {
            first.disconnect();
            for _ in 0..100 {
                w.tick(Some(&mut first));
            }
        }
        Leave::Silent => {}
    }
    drop(first);
    for _ in 0..AWAY_MS {
        w.tick(None);
    }
    // A new socket of the same address: a new port, as after a restart behind a NAT.
    w.net.rebind_now(w.client_ep);
    let mut back = w.client(|config| {
        config.resume = ticket;
        config.resumption = if with_credential { credential } else { None };
    });
    let (ms, datagrams) = w.connect(&mut back, 20_000);
    Rejoin {
        ms,
        datagrams,
        connected: back.is_connected(),
        same_peer: back.peer_id() == peer,
        fallbacks: back.totals().resume_fallbacks,
        flights: back.totals().resume_inits,
        refused: w.server.last_resume_refusal().map(|why| why.as_str()),
    }
}

/// The acceptance row: with the credential the rejoin is one round trip: one flight, answered
/// with a `ResumeReply` (never sent to the full handshake), connected after about one pass of
/// the standing queue and inside the reconnect grace on every seed, for a goodbye's return and
/// for a takeover; the takeover keeps its peer id.
#[test]
fn a_ticket_resume_over_asym_bufferbloat_is_one_round_trip_inside_the_grace() {
    for seed in SEEDS {
        for leave in [Leave::Goodbye, Leave::Silent] {
            let r = rejoin(seed, leave, true);
            assert!(r.connected, "seed {seed} {leave:?}: {r:?}");
            assert_eq!(
                r.refused, None,
                "seed {seed} {leave:?}: the server refused the flight: {r:?}"
            );
            assert_eq!(
                (r.flights, r.fallbacks),
                (1, 0),
                "seed {seed} {leave:?}: one flight, resumed, not sent to the full handshake: {r:?}"
            );
            assert!(
                r.ms <= RECONNECT_GRACE_MS,
                "seed {seed} {leave:?}: {} ms is outside the {RECONNECT_GRACE_MS} ms grace: {r:?}",
                r.ms
            );
            // One pass of the one-second queue, not three.
            assert!(
                (900..=1_400).contains(&r.ms),
                "seed {seed} {leave:?}: {} ms: {r:?}",
                r.ms
            );
            assert_eq!(
                r.same_peer,
                leave == Leave::Silent,
                "seed {seed} {leave:?}: a takeover keeps its id, a goodbye's return is a new peer"
            );
        }
    }
}

/// The control: the same rejoin with the 16-byte ticket alone runs the full handshake, three
/// round trips through the same queue, and does not make the grace. This is the measured cause of
/// the two red `reconnect_within_grace` rows (ADR-8733 (c)), kept as a pin on the path's cost.
#[test]
fn the_three_round_trip_rejoin_over_asym_bufferbloat_misses_the_grace() {
    for seed in SEEDS {
        for leave in [Leave::Goodbye, Leave::Silent] {
            let r = rejoin(seed, leave, false);
            assert!(r.connected, "seed {seed} {leave:?}: {r:?}");
            assert_eq!(
                (r.flights, r.refused),
                (0, None),
                "seed {seed} {leave:?}: no credential, no flight: {r:?}"
            );
            assert!(r.datagrams >= 3, "seed {seed} {leave:?}: {r:?}");
            assert!(
                r.ms > RECONNECT_GRACE_MS,
                "seed {seed} {leave:?}: the full handshake took only {} ms",
                r.ms
            );
        }
    }
}

/// The gate plan's row through the soak's own workload: `asym-bufferbloat`, 8 clients, the
/// gate's 60 s and seed. Client 5 says goodbye at 30 % of the run and rejoins two seconds later
/// with the ticket and the resumption credential its leave took; the profile's server keeps
/// tickets (ADR-8902 clause 9). The server resumes it in one round trip into the slot its address
/// held, and the row's `reconnect_within_grace` bar, red since ADR-8718 at 3,104 ms, passes.
#[test]
fn the_soak_rows_scripted_rejoin_is_a_one_round_trip_resume_inside_the_grace() {
    let mut profile = Profile::named("asym-bufferbloat").expect("profile");
    profile.fit_to(GATE_SECONDS * 1000);
    let scenario = Scenario {
        profile,
        clients: 8,
        mode: ServerMode::Dedicated,
        seed: DEFAULT_SEED,
        duration_ms: GATE_SECONDS * 1000,
        wire: Default::default(),
    };
    let record = workload::run(&scenario).expect("run");
    let scripted: Vec<_> = record
        .reconnects
        .iter()
        .filter(|r| r.kind == "scripted")
        .collect();
    assert_eq!(
        scripted.len(),
        1,
        "client 5 leaves and rejoins: {:?}",
        record.reconnects
    );
    let rejoin = scripted[0];
    let took = rejoin.connected_ms.expect("the rejoin connected") - rejoin.link_up_ms;
    let totals = record.server.totals;
    assert_eq!(
        (totals.resumed_one_rtt, totals.held_slot_returns),
        (1, 1),
        "the rejoin took {took} ms: {totals:?}"
    );
    assert!(
        (900..=1_400).contains(&took),
        "one pass of the queue, not {took} ms"
    );
    let result = report::evaluate(&record, 0);
    let bar = result
        .bars
        .iter()
        .find(|b| b.id == "reconnect_within_grace")
        .expect("the bar");
    assert!(
        bar.passed,
        "{}; ends {:?}; reconnects {:?}",
        bar.detail, record.ends, record.reconnects
    );
    let lost: Vec<_> = record.ends.iter().filter(|e| !e.voluntary).collect();
    assert!(lost.is_empty(), "no connection was lost: {lost:?}");
}

/// The rows that lost a client to `reassembly_stalled` when the scripted rejoin first presented
/// the credential (lane v07-wire6-resume-server's measurement, ADR-8902): 8 clients at the gate
/// seed (client 5, 21.8 s after its resume) and 32 clients at seeds 44 and 55 (clients 18 and 12,
/// which never left). In each the client waited 18.6 s of the server's traffic for the first
/// fragment of its join snapshot: the server repaired it once per round trip, in one burst with
/// its neighbors, and behind this profile's full drop-tail queue every repair met the same phase
/// of the cross traffic and was dropped (17 in a row at the gate seed). With repeat repairs
/// staggered (ADR-10088) no connection is lost and every scripted rejoin is a one-round-trip
/// resume inside the grace.
#[test]
fn no_join_snapshot_stalls_behind_the_full_queue() {
    for (clients, seed) in [(8usize, DEFAULT_SEED), (32, 44), (32, 55)] {
        let mut profile = Profile::named("asym-bufferbloat").expect("profile");
        profile.fit_to(GATE_SECONDS * 1000);
        let scenario = Scenario {
            profile,
            clients,
            mode: ServerMode::Dedicated,
            seed,
            duration_ms: GATE_SECONDS * 1000,
            wire: Default::default(),
        };
        let record = workload::run(&scenario).expect("run");
        let lost: Vec<_> = record
            .ends
            .iter()
            .filter(|e| !e.voluntary)
            .map(|e| (e.client, e.at_ms, e.side.clone(), e.cause.clone()))
            .collect();
        assert!(
            lost.is_empty(),
            "{clients} clients, seed {seed}: connections lost: {lost:?}"
        );
        let totals = record.server.totals;
        let scripted = record.reconnects.iter().filter(|r| r.kind == "scripted").count() as u64;
        assert_eq!(
            (totals.resumed_one_rtt, totals.held_slot_returns),
            (scripted, scripted),
            "{clients} clients, seed {seed}: every scripted rejoin resumed in one round trip"
        );
        let result = report::evaluate(&record, 0);
        for id in ["reconnect_within_grace", "involuntary_disconnects"] {
            let bar = result.bars.iter().find(|b| b.id == id).expect("a gating bar");
            assert!(bar.passed, "{clients} clients, seed {seed}: {id}: {}", bar.detail);
        }
    }
}
