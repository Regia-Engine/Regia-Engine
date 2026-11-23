// SPDX-License-Identifier: MPL-2.0
//! The lobby hub process (NL-06, ADR-8648): `hub.toml`, the address and identity gate, the
//! in-process soak, and a drain that publishes `lobby.hub_status`.
//!
//! Its own test binary: it installs a counting global allocator so the soak can watch live bytes.

use std::net::{IpAddr, SocketAddr};

use regia_net::crypto::{PlayerKey, ServerIdentity};
use regia_net::handshake::DisconnectReason;
use regia_net::lobby::hub::{
    AdmitGate, DEFAULT_HUB_TOML, HubError, HubFile, HubHost, HubLimits, HubProcess, HubServer, HubSettings,
    SOAK_ALLOC_BOUND, SOAK_CYCLES, SOAK_PRESENCES, SOAK_SIMULATED_MS, SOAK_STATE_BOUND, run_hub_soak_1000,
    validate_key_path,
};
use regia_net::lobby::{LobbyError, LobbySpec};
use regia_net::status::NetAgentState;
use regia_net::{LoopbackNet, LoopbackSocket};
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

/// A bad field is named. An unknown key (including DO27's relay flag, which this file does not
/// carry) is a field error too.
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
