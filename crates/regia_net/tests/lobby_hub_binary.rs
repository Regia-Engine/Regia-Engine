// SPDX-License-Identifier: MPL-2.0
//! The lobby hub process (NL-06, ADR-8648): `hub.toml`, the address and identity gate, the
//! in-process soak, and a drain that publishes `lobby.hub_status`.
//!
//! Its own test binary: it installs a counting global allocator so the soak can watch live bytes.

use std::net::{IpAddr, SocketAddr};

use regia_net::crypto::{PlayerKey, ServerIdentity};
use regia_net::handshake::DisconnectReason;
use regia_net::lobby::hub::{
    AdmitGate, DEFAULT_HUB_TOML, HUB_RELAY_DRAIN_MS, HubClientConfig, HubClock, HubDocument, HubError,
    HubFile, HubHost, HubLimits, HubLinkState, HubLobbyBackend, HubMessage, HubProcess, HubRequest,
    HubServer, HubSettings, HubWire, SOAK_ALLOC_BOUND, SOAK_CYCLES, SOAK_PRESENCES, SOAK_SIMULATED_MS,
    SOAK_STATE_BOUND, migrate_hub_toml, run_hub_soak_1000, validate_key_path,
};
use regia_net::lobby::{LobbyBackend, LobbyError, LobbySpec};
use regia_net::relay::{
    RelayConfig, RelayCore, RelayReply, RelayServer, encode_allocate, encode_lookup, encode_refresh,
    encode_register, encode_register_with_cookie,
};
#[cfg(feature = "ecs")]
use regia_net::server_host::HostClock;
use regia_net::status::NetAgentState;
use regia_net::{Channel, ClientConfig, CryptoMode, DatagramSocket, LoopbackNet, LoopbackSocket, NetClient};
use regia_test_utils::alloc_counter::{CountingAlloc, thread_live_bytes};
use regia_test_utils::unique_temp_dir;

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn live_alloc() -> usize {
    usize::try_from(thread_live_bytes().max(0)).unwrap_or(usize::MAX)
}

/// 1,000 presences connect, register, chat and drop for 10,000 simulated seconds. State and the
/// counting allocator stay under the stated bounds, and neither grows after the first cycle.
#[test]
fn hub_soak_1000_presences_bounded_memory() {
    let report = run_hub_soak_1000(&live_alloc).expect("soak");
    assert_eq!(report.cycles, SOAK_CYCLES);
    assert_eq!(report.simulated_ms, SOAK_SIMULATED_MS);
    assert_eq!(SOAK_PRESENCES, 1_000);
    assert!(
        report.peak_state_bytes <= SOAK_STATE_BOUND,
        "peak state {} over {SOAK_STATE_BOUND}",
        report.peak_state_bytes
    );
    assert!(
        report.state_at_end <= report.state_after_first_cycle,
        "state grew {} -> {}",
        report.state_after_first_cycle,
        report.state_at_end
    );
    let rise = report.peak_alloc_bytes.saturating_sub(report.alloc_at_start);
    assert!(
        rise <= SOAK_ALLOC_BOUND,
        "allocator rose {rise} over {SOAK_ALLOC_BOUND}"
    );
    assert!(
        report.alloc_at_end <= report.alloc_after_first_cycle,
        "allocator grew {} -> {}",
        report.alloc_after_first_cycle,
        report.alloc_at_end
    );
}

/// A bad field is named. `relay` as a string (the old DO27 flag) is a field error: version 2
/// carries `[relay]` as a table.
#[test]
fn hub_toml_bad_field_is_named() {
    let empty = HubFile::from_toml("").expect_err("empty");
    assert_eq!(empty[0].field, "hub.toml");
    assert!(
        empty[0].to_string().starts_with("hub.toml: expected"),
        "{empty:?}"
    );

    let bad_port = HubFile::from_toml("port = \"no\"\n").expect_err("bad port");
    assert_eq!(bad_port[0].field, "port");
    assert!(
        bad_port[0].to_string().starts_with("port: expected"),
        "{}",
        bad_port[0]
    );

    let unknown = HubFile::from_toml("relay = \"127.0.0.1:7800\"\n").expect_err("unknown");
    assert!(unknown.iter().any(|e| e.field == "relay"), "{unknown:?}");
    assert!(
        unknown
            .iter()
            .any(|e| e.to_string().starts_with("relay: expected")),
        "{unknown:?}"
    );

    let nested = HubFile::from_toml("[caps]\nnope = 1\n").expect_err("unknown cap");
    assert!(nested.iter().any(|e| e.field == "caps.nope"), "{nested:?}");

    let file = HubFile::from_toml(DEFAULT_HUB_TOML).expect("default");
    assert_eq!(file.port, 7810);
    assert_eq!(file.limits.per_address_per_sec, 30);
    assert_eq!(file.limits.per_identity_per_sec, 20);

    let dir = unique_temp_dir("hub-key-inside");
    std::fs::write(dir.join("Regia.toml"), b"[project]\nname = \"x\"\n").expect("manifest");
    let err = validate_key_path(&dir.join("lobby-hub.key")).expect_err("inside a project");
    assert_eq!(err.field, "key_path");
    assert!(err.to_string().starts_with("key_path: expected"), "{err}");
}

/// One address past its limit is refused. Another address is still admitted. The same player key
/// from a second address hits the identity limit. A later timestamp opens the window again.
#[test]
fn a_flood_from_one_address_is_rate_limited() {
    let net = LoopbackNet::new();
    let identity = ServerIdentity::generate().expect("key");
    let mut host = HubHost::new(
        net.bind(SocketAddr::from(([127, 0, 0, 1], 1))).expect("bind"),
        identity,
        HubSettings::default(),
    );
    host.set_limits(HubLimits {
        per_address_per_sec: 2,
        per_identity_per_sec: 3,
        ..HubLimits::default()
    });
    let home = IpAddr::from([10, 0, 0, 1]);
    let other = IpAddr::from([10, 0, 0, 2]);
    let player = PlayerKey([1u8; 32]);
    let other_player = PlayerKey([2u8; 32]);

    assert!(host.admit(Some(home), Some(&player), 0).is_ok());
    assert!(host.admit(Some(home), Some(&player), 0).is_ok());
    let flooded = host.admit(Some(home), Some(&other_player), 0).expect_err("flood");
    assert_eq!(flooded, HubError::AddressRateLimited);
    assert_eq!(flooded.code(), "hub_address_rate_limited");

    assert!(host.admit(Some(other), Some(&player), 0).is_ok());
    let identity_flood = host.admit(Some(other), Some(&player), 0).expect_err("identity");
    assert_eq!(identity_flood, HubError::IdentityRateLimited);
    assert_eq!(identity_flood.code(), "hub_identity_rate_limited");

    assert!(host.admit(Some(home), Some(&other_player), 1_000).is_ok());
}

/// Idle address buckets are dropped, and the map never keeps more than its cap.
#[test]
fn idle_addresses_are_dropped_and_the_map_is_capped() {
    let mut gate = AdmitGate::new(HubLimits {
        per_address_per_sec: 1,
        per_identity_per_sec: 1,
        ..HubLimits::default()
    });
    for n in 0..5_000u32 {
        let addr = IpAddr::from(n.to_be_bytes());
        let _ = gate.admit(Some(addr), None, 0);
    }
    assert!(gate.tracked_addresses() <= 4_096);
    let late = IpAddr::from([9, 9, 9, 9]);
    gate.admit(Some(late), None, 2_001).expect("window");
    assert_eq!(gate.tracked_addresses(), 1);
}

/// Seat caps from the requirements: 1, 2, 16, 22, 32 and 40 register; 0 is refused; a join stops
/// at a cap of 2.
#[test]
fn hub_seat_caps_follow_the_registered_limit() {
    let mut hub = HubServer::with_secret(HubSettings::default(), [9u8; 32]);
    for n in [1u32, 2, 16, 22, 32, 40] {
        let owner = hub.connect(&format!("h{n}"), 0).expect("owner");
        let spec = LobbySpec {
            max_members: n,
            ..LobbySpec::default()
        };
        hub.create(owner, &spec, 0).expect("register");
    }
    let owner = hub.connect("zero", 0).expect("owner");
    let spec = LobbySpec {
        max_members: 0,
        ..LobbySpec::default()
    };
    assert!(matches!(
        hub.create(owner, &spec, 0),
        Err(HubError::Lobby(LobbyError::MembersInvalid))
    ));

    let owner = hub.connect("cap", 1).expect("owner");
    let spec = LobbySpec {
        max_members: 2,
        ..LobbySpec::default()
    };
    let lobby = hub.create(owner, &spec, 1).expect("cap 2");
    let first = hub.connect("one", 1).expect("one");
    hub.join(first, lobby, None, 1).expect("second seat");
    let second = hub.connect("two", 1).expect("two");
    assert!(matches!(
        hub.join(second, lobby, None, 1),
        Err(HubError::Lobby(LobbyError::Full))
    ));
}

/// A drain refuses new work, the health body says so, and `lobby.hub_status` sees the report.
#[test]
fn hub_process_drains_and_publishes_status() {
    let net = LoopbackNet::new();
    let identity = ServerIdentity::generate().expect("key");
    let file = HubFile::from_toml(DEFAULT_HUB_TOML).expect("toml");
    let mut process = HubProcess::new(
        net.bind(SocketAddr::from(([127, 0, 0, 1], 2))).expect("bind"),
        identity,
        &file,
    );
    let mut state = NetAgentState::default();
    process.publish(&mut state);
    assert!(state.hubs().contains_key("hub"));

    process.begin_drain();
    assert!(process.is_draining());
    assert!(process.finished_draining());
    let body = process.health_json();
    assert!(body.contains("\"draining\":true"), "{body}");
    assert_eq!(
        HubProcess::<LoopbackSocket>::drain_reason(),
        DisconnectReason::ServerShutdown
    );

    #[cfg(feature = "ecs")]
    {
        let snapshot = process.health_snapshot();
        let (status, served) =
            regia_net::server_host::health::respond(b"GET /health HTTP/1.1\r\n\r\n", &snapshot);
        assert!(status.starts_with("503"), "{status}");
        assert!(served.contains("\"draining\":true"), "{served}");
    }
}

/// The number or word after `"key":` in the health body. The body has no spaces.
fn health_field<'a>(body: &'a str, key: &str) -> &'a str {
    let needle = format!("\"{key}\":");
    let start = body
        .find(&needle)
        .unwrap_or_else(|| panic!("missing {key} in {body}"))
        + needle.len();
    let rest = &body[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    &rest[..end]
}

fn health_usize(body: &str, key: &str) -> usize {
    health_field(body, key)
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number in {body}"))
}

/// NL-11 (spec 89 § 16.8). One process, two ports, driven by `server_host::TestClock` when the
/// `ecs` feature is on (the dedicated host's test clock) and by the same `now_ms` the hub client
/// uses. A hub session and a relay allocation share that clock. `GET /health` matches
/// `lobby.hub_status`. SIGTERM drains. A v1 file migrates with the relay off. A wrong pin is
/// `hub_key_mismatch` and allocates no session.
#[test]
fn nl11_one_process_serves_a_hub_session_and_a_relay_allocation() {
    let v1 = "\
# Lobby hub (hub.toml v1). The key file is created on first run outside any project.
format_version = 1
bind = \"0.0.0.0\"
port = 7810
key_path = \"\"

[caps]
session_cap = 1024
lobby_cap = 256
max_members = 64
chat_per_sec = 4
handle_max_bytes = 24
idle_ms = 30000

[limits]
per_address_per_sec = 30
per_identity_per_sec = 20

[status]
enabled = true
bind = \"127.0.0.1\"
port = 7811
";
    let migrated = migrate_hub_toml(v1).expect("v1 migrates");
    let migrated_doc = HubDocument::from_toml(&migrated).expect("migrated loads");
    assert_eq!(migrated_doc.file.format_version, 2);
    assert!(!migrated_doc.relay.enabled);
    assert!(migrated_doc.relay_addr().expect("addr").is_none());
    assert_eq!(migrate_hub_toml(&migrated).expect("idempotent"), migrated);

    let mut clock = ProcessClock::new();
    let hub_clock = HubClock::manual(0);
    let net = LoopbackNet::new();
    let identity = ServerIdentity::generate().expect("key");
    let pinned = identity.public();
    let hub_addr = SocketAddr::from(([10, 9, 0, 1], 7810));
    let relay_addr = SocketAddr::from(([10, 9, 0, 1], 7812));
    assert_ne!(hub_addr.port(), relay_addr.port());
    let file = HubFile::from_toml(DEFAULT_HUB_TOML).expect("default");
    let mut process = HubProcess::new(net.bind(hub_addr).expect("hub port"), identity, &file);
    let alloc_socket = net
        .bind(SocketAddr::from(([10, 9, 0, 1], 0)))
        .expect("allocation port");
    let mut relay = RelayServer::from_core(
        net.bind(relay_addr).expect("relay port"),
        vec![alloc_socket],
        RelayCore::with_config(
            11,
            [7u8; 32],
            RelayConfig {
                allocation_ports: 1,
                max_allocations_per_ip: 8,
                ..RelayConfig::default()
            },
        ),
    );
    relay.set_source_rate(30);
    process.attach_relay(relay);

    let mut good = connect_peer(&net, &hub_clock, hub_addr, 5001, "ann", Some(pinned));
    drive_until(&mut clock, &hub_clock, &mut process, &mut [&mut good], |peers| {
        peers[0].local_member().is_some()
    });
    assert!(good.local_member().is_some());

    let host_addr = SocketAddr::from(([10, 9, 2, 1], 1));
    let mut host = net.bind(host_addr).expect("relay host");
    let now = clock.now_ms();
    let challenge = ask(
        &mut process,
        &mut host,
        relay_addr,
        &encode_register(3600, b"t").expect("register"),
        now,
    );
    let RelayReply::RegisterChallenge(cookie) = challenge else {
        panic!("expected a register challenge, got {challenge:?}");
    };
    let joined = ask(
        &mut process,
        &mut host,
        relay_addr,
        &encode_register_with_cookie(3600, b"t", &cookie).expect("register"),
        now,
    );
    let RelayReply::JoinCode(code) = joined else {
        panic!("expected a join code, got {joined:?}");
    };
    let mut looker = net.bind(SocketAddr::from(([10, 9, 2, 3], 1))).expect("looker");
    let info = ask(
        &mut process,
        &mut looker,
        relay_addr,
        &encode_lookup(&code).expect("lookup"),
        now,
    );
    let RelayReply::HostInfo { addr, bind_key, .. } = info else {
        panic!("expected host info, got {info:?}");
    };
    assert_eq!(addr, host_addr);

    let mut client = net
        .bind(SocketAddr::from(([10, 9, 2, 2], 1)))
        .expect("relay client");
    let client_key = [0xA1; 16];
    let first = ask(
        &mut process,
        &mut client,
        relay_addr,
        &encode_allocate(host_addr, &bind_key, &client_key, None),
        now,
    );
    let RelayReply::Challenge(alloc_cookie) = first else {
        panic!("expected an allocate challenge, got {first:?}");
    };
    let allocated = ask(
        &mut process,
        &mut client,
        relay_addr,
        &encode_allocate(host_addr, &bind_key, &client_key, Some(&alloc_cookie)),
        now,
    );
    let RelayReply::Allocated { port, peer } = allocated else {
        panic!("expected an allocation, got {allocated:?}");
    };
    assert_eq!(peer, host_addr);
    assert_eq!(process.relay().expect("relay").core().allocation_count(), 1);

    let mut state = NetAgentState::default();
    process.publish(&mut state);
    let report = state.hubs().get("hub").expect("lobby.hub_status");
    assert!(report.sessions >= 1, "{report:?}");
    assert_eq!(report.relay_allocations, 1);
    assert_eq!(report.servers_online, 0);
    assert!(!report.draining);
    let body = process.health_json();
    assert_eq!(health_usize(&body, "sessions"), report.sessions);
    assert_eq!(health_usize(&body, "lobbies"), report.lobbies);
    assert_eq!(health_usize(&body, "listed"), report.listed);
    assert_eq!(health_usize(&body, "state_bytes"), report.state_bytes);
    assert_eq!(health_usize(&body, "relay_allocations"), report.relay_allocations);
    assert_eq!(health_usize(&body, "servers_online"), report.servers_online);
    assert_eq!(health_field(&body, "draining") == "true", report.draining);
    let refused: u64 = report.refused.values().sum();
    assert_eq!(
        health_usize(&body, "refused"),
        usize::try_from(refused).unwrap_or(usize::MAX)
    );

    #[cfg(feature = "ecs")]
    {
        let snapshot = process.health_snapshot();
        let (status, served) =
            regia_net::server_host::health::respond(b"GET /health HTTP/1.1\r\n\r\n", &snapshot);
        assert!(status.starts_with("200"), "{status}");
        assert_eq!(served, body);
    }

    let wrong = ServerIdentity::generate().expect("other key").public();
    let mut bad = connect_peer(&net, &hub_clock, hub_addr, 5002, "eve", Some(wrong));
    drive_until(
        &mut clock,
        &hub_clock,
        &mut process,
        &mut [&mut good, &mut bad],
        |peers| matches!(peers[1].link_state(), HubLinkState::Ended(ref code) if code == "hub_key_mismatch"),
    );
    assert_eq!(bad.link_state(), HubLinkState::Ended("hub_key_mismatch".into()));
    assert!(bad.local_member().is_none());
    assert!(good.local_member().is_some());
    assert_eq!(process.report().sessions, 1);
    assert_eq!(process.report().relay_allocations, 1);

    #[cfg(feature = "ecs")]
    let signals = regia_net::server_host::HostSignals::new();
    #[cfg(feature = "ecs")]
    {
        signals.request_shutdown();
        assert!(signals.take_shutdown(), "SIGTERM sets the shutdown flag");
    }
    assert!(!process.note_shutdown(), "the first signal starts the drain");
    #[cfg(feature = "ecs")]
    {
        signals.mark_draining();
        assert!(signals.is_draining());
    }
    assert!(process.is_draining());
    assert!(
        !process.finished_draining(),
        "a live allocation waits for the process clock"
    );

    clock.advance(STEP_MS);
    let drain_at = clock.now_ms();
    hub_clock.set_ms(drain_at);
    process.update(drain_at);
    #[cfg(feature = "ecs")]
    {
        signals.request_shutdown();
        assert!(signals.take_shutdown());
    }
    assert!(
        process.note_shutdown(),
        "a second SIGTERM exits at once, before the window"
    );
    assert!(!process.finished_draining());

    let mut late = net
        .bind(SocketAddr::from(([10, 9, 2, 4], 1)))
        .expect("late client");
    let refused_alloc = ask(
        &mut process,
        &mut late,
        relay_addr,
        &encode_allocate(host_addr, &bind_key, &[0xB2; 16], None),
        drain_at,
    );
    assert!(
        matches!(refused_alloc, RelayReply::Full { .. }),
        "a new allocation during the drain is full, got {refused_alloc:?}"
    );
    assert_eq!(process.relay().expect("relay").core().allocation_count(), 1);
    let refresh = ask(
        &mut process,
        &mut client,
        SocketAddr::new(relay_addr.ip(), port),
        &encode_refresh(&client_key, None),
        drain_at,
    );
    assert!(
        matches!(
            refresh,
            RelayReply::Allocated { port: got, peer } if got == port && peer == host_addr
        ),
        "refresh from the holder still answers during the drain, got {refresh:?}"
    );

    clock.advance(HUB_RELAY_DRAIN_MS - 1);
    let before = clock.now_ms();
    hub_clock.set_ms(before);
    process.update(before);
    assert!(
        !process.finished_draining(),
        "the window is one millisecond short"
    );
    assert_eq!(process.relay().expect("relay").core().allocation_count(), 1);
    clock.advance(1);
    let done = clock.now_ms();
    assert_eq!(done.saturating_sub(drain_at), HUB_RELAY_DRAIN_MS);
    hub_clock.set_ms(done);
    process.update(done);
    assert!(process.finished_draining());
    assert_eq!(
        process.relay().expect("relay").core().allocation_count(),
        1,
        "the window ends the drain while the allocation is still held"
    );
    process.publish(&mut state);
    let draining = state.hubs().get("hub").expect("status");
    assert!(draining.draining);
    assert_eq!(draining.relay_allocations, 1);
    let drained = process.health_json();
    assert_eq!(health_field(&drained, "draining"), "true");
    assert_eq!(health_usize(&drained, "relay_allocations"), 1);

    #[cfg(feature = "ecs")]
    {
        let snapshot = process.health_snapshot();
        let (status, served) =
            regia_net::server_host::health::respond(b"GET /health HTTP/1.1\r\n\r\n", &snapshot);
        assert!(status.starts_with("503"), "{status}");
        assert_eq!(served, drained);
    }
}

/// A relay that holds nothing does not wait out the 30 second window.
#[test]
fn a_drained_relay_with_no_allocation_finishes_without_the_window() {
    let net = LoopbackNet::new();
    let identity = ServerIdentity::generate().expect("key");
    let file = HubFile::from_toml(DEFAULT_HUB_TOML).expect("toml");
    let mut process = HubProcess::new(
        net.bind(SocketAddr::from(([10, 8, 0, 1], 7810))).expect("hub"),
        identity,
        &file,
    );
    let relay = RelayServer::from_core(
        net.bind(SocketAddr::from(([10, 8, 0, 1], 7812))).expect("relay"),
        vec![net.bind(SocketAddr::from(([10, 8, 0, 1], 0))).expect("port")],
        RelayCore::with_config(3, [4u8; 32], RelayConfig::default()),
    );
    process.attach_relay(relay);
    process.begin_drain();
    assert!(process.finished_draining());
    assert!(process.health_json().contains("\"draining\":true"));
    assert!(process.health_json().contains("\"relay_allocations\":0"));
}

/// Spec 89 § 16.8 and ADR-10397: `finished_draining` is true once hub sessions are gone and
/// 30 s of the process clock have passed. A handshake during the drain is refused and allocates
/// no session. [`HubLobbyBackend`] disconnects on that refusal. A client that stays on the link
/// leaves the peer unconfirmed. [`HubProcess::finished_draining`] reads the relay clock only
/// after [`HubHost::finished_draining`], and the host waits out `idle_ms` for that peer, so the
/// 30 s window is not an upper bound. Packets keep flowing so the transport silence timeout
/// (20 s) does not remove the peer and hide the hold.
#[test]
fn a_refused_handshake_does_not_hold_the_drain_past_the_window() {
    let net = LoopbackNet::new();
    let identity = ServerIdentity::generate().expect("key");
    let pinned = identity.public();
    let hub_addr = SocketAddr::from(([10, 9, 4, 1], 7810));
    let file = HubFile::from_toml(DEFAULT_HUB_TOML).expect("default");
    let mut process = HubProcess::new(net.bind(hub_addr).expect("hub"), identity, &file);
    let relay = RelayServer::from_core(
        net.bind(SocketAddr::from(([10, 9, 4, 1], 7812))).expect("relay"),
        vec![
            net.bind(SocketAddr::from(([10, 9, 4, 1], 0)))
                .expect("allocation port"),
        ],
        RelayCore::with_config(5, [5u8; 32], RelayConfig::default()),
    );
    process.attach_relay(relay);

    process.begin_drain();
    process.update(0);
    assert!(process.is_draining());

    let mut raw: NetClient<HubWire, LoopbackSocket> = NetClient::connect(
        net.bind(SocketAddr::from(([10, 9, 1, 2], 5401))).expect("client"),
        hub_addr,
        ClientConfig {
            crypto: CryptoMode::Required,
            server_key: Some(pinned),
            ..ClientConfig::default()
        },
        &(),
    )
    .expect("connect");
    let mut now = 0;
    while !raw.is_connected() {
        now += STEP_MS;
        raw.update(now);
        process.update(now);
        assert!(now < 5_000, "the handshake did not finish");
    }
    raw.send(
        Channel::GAME_RPC,
        &HubMessage::Request(HubRequest::Hello {
            handle: "late".into(),
            proof: Vec::new(),
        }),
    )
    .expect("hello");
    let mut refused = false;
    while now < 5_000 {
        now += STEP_MS;
        raw.update(now);
        process.update(now);
        if process.report().refused.get("hub_draining").copied().unwrap_or(0) > 0 {
            refused = true;
            break;
        }
    }
    assert!(refused, "the drain did not refuse the new hello");
    assert_eq!(
        process.report().sessions,
        0,
        "a refused hello allocates no session"
    );
    assert!(
        now < HUB_RELAY_DRAIN_MS,
        "the refusal itself ran past the drain window"
    );
    // Stay connected. Heartbeats are not a session. They keep the transport from timing the
    // peer out before the window, which would make this assertion pass for the wrong reason.
    while now < HUB_RELAY_DRAIN_MS {
        now = (now + 1_000).min(HUB_RELAY_DRAIN_MS);
        raw.send(Channel::GAME_RPC, &HubMessage::Request(HubRequest::Heartbeat))
            .expect("the link still accepts a packet");
        raw.update(now);
        process.update(now);
    }
    assert_eq!(process.report().sessions, 0);
    assert_eq!(process.report().relay_allocations, 0);
    assert!(
        process.finished_draining(),
        "an unconfirmed peer held the drain at +{HUB_RELAY_DRAIN_MS} ms (host finished: {})",
        process.host().finished_draining()
    );
}

const STEP_MS: u64 = 10;

/// The process clock. With `ecs` this is `server_host::TestClock` (NL-11's test clock). Without
/// it, the same millisecond counter, so the test still builds.
struct ProcessClock {
    #[cfg(feature = "ecs")]
    inner: regia_net::server_host::TestClock,
    #[cfg(not(feature = "ecs"))]
    ms: u64,
}

impl ProcessClock {
    fn new() -> Self {
        Self {
            #[cfg(feature = "ecs")]
            inner: regia_net::server_host::TestClock::new(),
            #[cfg(not(feature = "ecs"))]
            ms: 0,
        }
    }

    fn advance(&mut self, ms: u64) {
        #[cfg(feature = "ecs")]
        self.inner.advance(std::time::Duration::from_millis(ms));
        #[cfg(not(feature = "ecs"))]
        {
            self.ms = self.ms.saturating_add(ms);
        }
    }

    fn now_ms(&self) -> u64 {
        #[cfg(feature = "ecs")]
        {
            u64::try_from(self.inner.now().as_millis()).unwrap_or(u64::MAX)
        }
        #[cfg(not(feature = "ecs"))]
        {
            self.ms
        }
    }
}

fn connect_peer(
    net: &LoopbackNet,
    clock: &HubClock,
    hub: SocketAddr,
    port: u16,
    handle: &str,
    key: Option<regia_net::crypto::ServerKey>,
) -> HubLobbyBackend<LoopbackSocket> {
    let socket = net.bind(SocketAddr::from(([10, 9, 1, 2], port))).expect("client");
    HubLobbyBackend::connect(
        socket,
        hub,
        HubClientConfig {
            handle: handle.into(),
            server_key: key,
            clock: clock.clone(),
            ..HubClientConfig::default()
        },
    )
    .expect("connect")
}

fn drive_until(
    clock: &mut ProcessClock,
    hub_clock: &HubClock,
    process: &mut HubProcess<LoopbackSocket>,
    peers: &mut [&mut HubLobbyBackend<LoopbackSocket>],
    mut done: impl FnMut(&mut [&mut HubLobbyBackend<LoopbackSocket>]) -> bool,
) {
    for _ in 0..500 {
        if done(peers) {
            return;
        }
        clock.advance(STEP_MS);
        let now = clock.now_ms();
        hub_clock.set_ms(now);
        for peer in peers.iter() {
            let _ = peer.poll();
        }
        process.update(now);
        for peer in peers.iter() {
            let _ = peer.poll();
        }
    }
    panic!("the condition did not hold within 500 steps");
}

fn ask(
    process: &mut HubProcess<LoopbackSocket>,
    socket: &mut LoopbackSocket,
    to: SocketAddr,
    request: &[u8],
    now: u64,
) -> RelayReply {
    socket.send_to(request, to).expect("send");
    let mut buf = [0u8; 2048];
    for _ in 0..8 {
        process.update(now);
        if let Some((n, _)) = socket.recv_from(&mut buf).expect("io") {
            return RelayReply::decode(&buf[..n])
                .expect("decode")
                .expect("a relay reply");
        }
    }
    panic!("the relay did not answer");
}
