// SPDX-License-Identifier: MPL-2.0
//! Spec 89 § 11: two apps matched through a `LocalLobbyHub`; the owner launches a listen session
//! on a loopback transport and the member's `LobbyPlugin` auto-connects its `NetSession`, which
//! reaches `in_game`. Fixed steps and a manual session clock: no wall time.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_net::lobby::{
    Compare, LobbyMessage, LobbyQuery, LobbyServer, LoopbackLobbyTransport, MatchState, keys,
};
use regia_net::session::lifecycle::{ClientSessionState, HostOptions};
use regia_net::{Lobbies, LobbyPlugin, LocalLobbyHub, LoopbackNet, NetSession, NetSessionPlugin};

fn app(hub: &LocalLobbyHub, net: &LoopbackNet, id: u64, name: &str) -> App {
    let mut app = App::new();
    let mut lobbies = Lobbies::new(Arc::new(hub.member(id, name)));
    lobbies.set_transport(Arc::new(LoopbackLobbyTransport::new(net.clone())));
    let manifest = regia_net::session::ContentManifest::build("0.5.0", "1", 1, 0xfeed, &[]);
    lobbies.set_identity("example.arena", &manifest);
    app.insert_resource(lobbies);
    let mut session = NetSession::default();
    session.set_manifest(manifest);
    session.set_manual_clock(0);
    app.insert_resource(session);
    app.add_plugins((NetSessionPlugin, LobbyPlugin));
    app.init_resource::<Seen>().add_systems(Update, collect);
    app
}

fn step(apps: &mut [&mut App], n: usize) {
    for _ in 0..n {
        for app in apps.iter_mut() {
            app.world_mut()
                .resource_mut::<NetSession>()
                .advance_manual_clock(16);
            app.update();
        }
    }
}

/// Every `LobbyMessage` the app's systems could read, collected in `Update`.
#[derive(Resource, Default)]
struct Seen(Vec<LobbyMessage>);

fn collect(mut reader: MessageReader<LobbyMessage>, mut seen: ResMut<Seen>) {
    seen.0.extend(reader.read().cloned());
}

fn messages(app: &mut App) -> Vec<LobbyMessage> {
    std::mem::take(&mut app.world_mut().resource_mut::<Seen>().0)
}

#[test]
fn matchmake_launch_and_auto_connect() {
    let hub = LocalLobbyHub::new();
    let net = LoopbackNet::new();
    let mut host = app(&hub, &net, 1, "Ann");
    let mut guest = app(&hub, &net, 2, "Bob");

    // The host finds nothing and creates; the guest then finds and joins it.
    {
        let mut l = host.world_mut().resource_mut::<Lobbies>();
        let mut req = l.default_request();
        req.rounds.truncate(1);
        req.create.data.insert("mode".into(), "duel".into());
        l.start_matchmaking(req).expect("host matchmaking");
    }
    step(&mut [&mut host], 3);
    let lobby = match host.world().resource::<Lobbies>().matchmaker().state() {
        MatchState::InLobby { lobby, owner: true } => *lobby,
        other => panic!("host: {other:?}"),
    };
    {
        let mut l = guest.world_mut().resource_mut::<Lobbies>();
        let mut req = l.default_request();
        req.query = LobbyQuery::default().with_string("mode", Compare::Eq, "duel");
        l.start_matchmaking(req).expect("guest matchmaking");
    }
    step(&mut [&mut host, &mut guest], 3);
    assert_eq!(guest.world().resource::<Lobbies>().current(), Some(lobby));
    let joined = messages(&mut host)
        .into_iter()
        .any(|m| matches!(m.0, regia_net::lobby::LobbyEvent::MemberJoined { member, .. } if member.0 == 2));
    assert!(joined, "the host saw the guest join through LobbyMessage");

    // Both ready; the host hosts on the loopback transport and launches.
    for app in [&mut host, &mut guest] {
        app.world_mut()
            .resource_mut::<Lobbies>()
            .set_ready(true)
            .expect("ready");
    }
    step(&mut [&mut host, &mut guest], 1);
    assert!(host.world().resource::<Lobbies>().ready_to_launch());
    let server = host
        .world_mut()
        .resource_scope(|world, mut lobbies: Mut<Lobbies>| {
            let mut session = world.resource_mut::<NetSession>();
            lobbies.launch_hosting(
                &mut session,
                HostOptions {
                    level: "levels/arena.level.ron".into(),
                    ..HostOptions::default()
                },
            )
        })
        .expect("launch");
    assert!(matches!(server, LobbyServer::Address(_)));

    // The guest sees the server and connects by itself.
    step(&mut [&mut host, &mut guest], 120);
    let g = guest.world().resource::<Lobbies>();
    assert_eq!(g.connected_to(), Some(server));
    assert_eq!(*g.matchmaker().state(), MatchState::Launched { lobby, server });
    // The host saw the guest reach the game: the lobby says so to everyone (spec 89 § 4).
    assert_eq!(
        g.snapshot().map(|s| s.data[keys::STATE].clone()).as_deref(),
        Some("in_game")
    );
    assert_eq!(
        guest.world().resource::<NetSession>().state(),
        ClientSessionState::InGame
    );
    assert_eq!(
        host.world()
            .resource::<NetSession>()
            .server()
            .map(|s| s.peer_rows().len()),
        Some(1)
    );
    // The host does not connect to itself, and the lobby is closed to new joins.
    assert_eq!(host.world().resource::<Lobbies>().connected_to(), None);
    assert!(!hub.lobbies()[0].joinable);
}

#[test]
fn a_different_build_never_matches() {
    let hub = LocalLobbyHub::new();
    let net = LoopbackNet::new();
    let mut host = app(&hub, &net, 1, "Ann");
    host.world_mut()
        .resource_mut::<Lobbies>()
        .create(regia_net::lobby::LobbySpec::default())
        .expect("create");
    step(&mut [&mut host], 1);
    let mut old = App::new();
    let mut lobbies = Lobbies::new(Arc::new(hub.member(3, "Old")));
    lobbies.set_identity(
        "example.arena",
        &regia_net::session::ContentManifest::build("0.4.0", "1", 1, 0xbeef, &[]),
    );
    let mut req = lobbies.default_request();
    req.rounds.truncate(1);
    req.create_if_none = false;
    lobbies.start_matchmaking(req).expect("matchmaking");
    old.insert_resource(lobbies);
    old.add_plugins(LobbyPlugin);
    for _ in 0..3 {
        old.update();
    }
    assert_eq!(
        old.world().resource::<Lobbies>().matchmaker().state().name(),
        "failed"
    );
}

/// Spec 89 § 13 measurement, not a gate: `Lobbies::pump` on the owner of a 16-member lobby
/// with no events (one poll, one snapshot). Prints the mean; asserts only the setup. Run with
/// `cargo test -p regia_net --test lobby -- --ignored --nocapture lobby_pump_cost`.
///
/// The enforcing companion is `a_quiet_pump_does_not_fan_out_with_members` below: the suite
/// carries no wall-clock asserts (lane rules), so the § 13 bound (≤ 0.05 ms/frame) is measured
/// here for the reference run and the structural bound is enforced in the suite.
#[test]
#[ignore = "measurement; prints the per-pump cost"]
fn lobby_pump_cost_16_members() {
    let hub = LocalLobbyHub::new();
    let mut owner = Lobbies::new(Arc::new(hub.member(1, "owner")));
    owner.allow_unidentified(true);
    let spec = regia_net::lobby::LobbySpec {
        max_members: 16,
        ..owner.default_spec()
    };
    assert!(owner.create(spec).is_ok());
    owner.pump();
    let Some(lobby) = owner.current() else {
        panic!("the local hub creates at once");
    };
    let mut members: Vec<Lobbies> = (2..=16)
        .map(|id| Lobbies::new(Arc::new(hub.member(id, &format!("member {id}")))))
        .collect();
    for m in &mut members {
        assert!(m.join(lobby).is_ok());
        m.pump();
        assert!(m.set_ready(true).is_ok());
    }
    for _ in 0..4 {
        owner.pump();
        for m in &mut members {
            m.pump();
        }
    }
    assert_eq!(owner.snapshot().map(|s| s.member_count), Some(16));
    assert!(owner.pump().is_empty(), "no events left before measuring");

    const PUMPS: u32 = 10_000;
    let start = std::time::Instant::now();
    for _ in 0..PUMPS {
        std::hint::black_box(owner.pump());
    }
    let per_pump = start.elapsed() / PUMPS;
    println!("lobby_pump (16 members, no events): {per_pump:?} per pump over {PUMPS} pumps (budget 50 us)");
}

/// Spec 89 § 13, the wall-clock-free half: a quiet `pump` must cost the same number of backend
/// round trips however many members the lobby has. An accidental per-member fan-out (the
/// realistic way a quiet pump blows the § 13 frame budget) changes these counts; the wall-clock
/// half is the ignored measurement above.
#[test]
fn a_quiet_pump_does_not_fan_out_with_members() {
    use regia_net::lobby::{
        LobbyBackend, LobbyError, LobbyEvent, LobbyId, LobbyInfo, LobbySpec, MemberId, RequestId,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts the backend round trips of one owner.
    #[derive(Default)]
    struct Calls {
        polls: AtomicUsize,
        still_ins: AtomicUsize,
    }

    struct Counted {
        inner: Arc<dyn LobbyBackend>,
        calls: Arc<Calls>,
    }

    impl LobbyBackend for Counted {
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn local_member(&self) -> Option<MemberId> {
            self.inner.local_member()
        }
        fn create(&self, spec: &LobbySpec) -> Result<RequestId, LobbyError> {
            self.inner.create(spec)
        }
        fn join(&self, lobby: LobbyId) -> Result<RequestId, LobbyError> {
            self.inner.join(lobby)
        }
        fn leave(&self, lobby: LobbyId) {
            self.inner.leave(lobby);
        }
        fn request_list(&self, query: &regia_net::lobby::LobbyQuery) -> Result<RequestId, LobbyError> {
            self.inner.request_list(query)
        }
        fn lobby(&self, lobby: LobbyId) -> Option<LobbyInfo> {
            self.inner.lobby(lobby)
        }
        fn still_in(&self, lobby: LobbyId) -> bool {
            self.calls.still_ins.fetch_add(1, Ordering::Relaxed);
            self.inner.still_in(lobby)
        }
        fn set_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
            self.inner.set_data(lobby, key, value)
        }
        fn set_member_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
            self.inner.set_member_data(lobby, key, value)
        }
        fn set_joinable(&self, lobby: LobbyId, joinable: bool) -> Result<(), LobbyError> {
            self.inner.set_joinable(lobby, joinable)
        }
        fn set_server(&self, lobby: LobbyId, server: LobbyServer) -> Result<(), LobbyError> {
            self.inner.set_server(lobby, server)
        }
        fn send_chat(&self, lobby: LobbyId, text: &str) -> Result<(), LobbyError> {
            self.inner.send_chat(lobby, text)
        }
        fn poll(&self) -> Vec<LobbyEvent> {
            self.calls.polls.fetch_add(1, Ordering::Relaxed);
            self.inner.poll()
        }
    }

    /// Builds a full lobby with `members` members (owner + joined), settled quiet.
    fn settled(hub: &LocalLobbyHub, members: u32) -> (Lobbies, Arc<Calls>) {
        let calls = Arc::new(Calls::default());
        let inner: Arc<dyn LobbyBackend> = Arc::new(hub.member(1, "owner"));
        let mut owner = Lobbies::new(Arc::new(Counted {
            inner,
            calls: calls.clone(),
        }));
        owner.allow_unidentified(true);
        let spec = regia_net::lobby::LobbySpec {
            max_members: members.max(1),
            ..owner.default_spec()
        };
        owner.create(spec).expect("create");
        owner.pump();
        let lobby = owner.current().expect("in lobby");
        for id in 2..=members {
            let mut m = Lobbies::new(Arc::new(hub.member(u64::from(id), "member")));
            m.join(lobby).expect("join");
            m.pump();
        }
        for _ in 0..4 {
            owner.pump();
        }
        assert_eq!(owner.snapshot().map(|s| s.member_count), Some(members));
        (owner, calls)
    }

    fn quiet_pump_cost(hub: &LocalLobbyHub, members: u32) -> (usize, usize) {
        let (mut owner, calls) = settled(hub, members);
        assert!(owner.pump().is_empty(), "no events before measuring");
        let polls = calls.polls.load(Ordering::Relaxed);
        let still_ins = calls.still_ins.load(Ordering::Relaxed);
        std::hint::black_box(owner.pump());
        (
            calls.polls.load(Ordering::Relaxed) - polls,
            calls.still_ins.load(Ordering::Relaxed) - still_ins,
        )
    }

    let solo = quiet_pump_cost(&LocalLobbyHub::new(), 1);
    let full = quiet_pump_cost(&LocalLobbyHub::new(), 16);
    assert_eq!(
        solo, full,
        "a quiet pump must make the same backend round trips at 1 and 16 members (one poll, one \
         still-in check), however the § 13 frame budget is blown: {solo:?} vs {full:?}"
    );
}
