// SPDX-License-Identifier: MPL-2.0
//! Net cutscenes end to end (ADR-2948): a server and two client apps with `NetPlugin` and
//! `NetCutscenePlugin`, datagrams moved by hand. The server announces a start tick; the server and
//! the first client start on that tick of their own session clock and play locally; the second
//! client joins later and lands where the tick says, with the markers before its join delivered as
//! skipped. Time is a step counter; nothing sleeps.

use regia_app::enable_fixed_step;
use regia_app::prelude::*;
use regia_framework::sequencer::cutscene::net::{NetCutscenePlugin, NetCutsceneServer};
use regia_framework::sequencer::{
    CutsceneSettings, MarkerReached, NetCutscenes, Sequence, SequencePlayer, SequencerPlugin,
};
use regia_net::NetPlugin;
use regia_net::model::{LocalPeer, NetMode, PeerId};
use regia_net::runtime::{NetNode, server_add_peer};

const HZ: f64 = 60.0;
const START: u64 = 20;

#[derive(Resource, Default)]
struct Seen(Vec<(String, bool)>);

#[derive(Resource, Default)]
struct Ended(usize);

fn count_ended(
    mut reader: MessageReader<regia_framework::sequencer::CutsceneEnded>,
    mut ended: ResMut<Ended>,
) {
    ended.0 += reader.read().count();
}

fn collect(mut reader: MessageReader<MarkerReached>, mut seen: ResMut<Seen>) {
    for m in reader.read() {
        seen.0.push((m.name.clone(), m.skipped));
    }
}

fn peer_app(mode: NetMode, peer: PeerId) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        NetPlugin { mode },
        SequencerPlugin,
        NetCutscenePlugin,
    ));
    enable_fixed_step(&mut app, HZ);
    app.insert_resource(LocalPeer(Some(peer)))
        .init_resource::<Seen>()
        .init_resource::<Ended>()
        .add_systems(Update, (collect, count_ended));
    let sequence = Sequence::from_ron(
        r#"(version: 5, name: "intro", markers: [(name: "a", time: 0.1), (name: "b", time: 0.5), (name: "c", time: 1.5), (name: "end", time: 2.0)])"#,
    )
    .expect("valid");
    let player = app.world_mut().spawn(SequencePlayer::new(sequence)).id();
    {
        let mut net = app.world_mut().resource_mut::<NetCutscenes>();
        net.tick_hz = HZ;
        net.register(
            "intro",
            player,
            CutsceneSettings {
                lock_input: false,
                letterbox: 0.0,
                ..CutsceneSettings::default()
            },
        );
    }
    (app, player)
}

/// One frame on every app; datagrams cross at once. `clients[i]` is peer `i + 1`; a client is in
/// the session only once `joined[i]`.
fn pump(server: &mut App, clients: &mut [(App, Entity)], joined: &[bool]) {
    server.update();
    let down = std::mem::take(&mut server.world_mut().resource_mut::<NetNode>().outbox);
    for d in down {
        let index = d.peer.0 as usize;
        if (1..=clients.len()).contains(&index) && joined[index - 1] {
            let mut d = d;
            d.peer = PeerId::SERVER;
            clients[index - 1]
                .0
                .world_mut()
                .resource_mut::<NetNode>()
                .inbox
                .push(d);
        }
    }
    for (i, (client, _)) in clients.iter_mut().enumerate() {
        if !joined[i] {
            continue;
        }
        client.update();
        let up = std::mem::take(&mut client.world_mut().resource_mut::<NetNode>().outbox);
        let mut node = server.world_mut().resource_mut::<NetNode>();
        node.inbox.extend(up.into_iter().map(|mut d| {
            d.peer = PeerId(i as u32 + 1);
            d
        }));
    }
}

fn tick(app: &App) -> u64 {
    u64::from(app.world().resource::<NetNode>().tick.0)
}

fn time(app: &App, player: Entity) -> f64 {
    app.world().get::<SequencePlayer>(player).expect("player").time()
}

/// The clock a peer should show after the frame that ran `tick`: the start tick's frame plays one
/// step.
fn expected(tick: u64) -> f64 {
    (tick + 1).saturating_sub(START) as f64 / HZ
}

#[test]
fn every_peer_starts_at_the_tick_and_a_late_joiner_lands_at_the_right_time() {
    let (mut server, server_player) = peer_app(NetMode::ListenServer, PeerId::SERVER);
    let mut clients = vec![
        peer_app(NetMode::Client, PeerId(1)),
        peer_app(NetMode::Client, PeerId(2)),
    ];
    server_add_peer(server.world_mut(), PeerId(1));
    let mut joined = [true, false];
    server
        .world_mut()
        .resource_mut::<NetCutsceneServer>()
        .announce("intro", START as u32);
    let mut client_started = None;
    let mut late_started = None;
    // Each client's clock on the frame before (a client plays on its own frames; its session tick
    // is the newest server tick it applied, which can stand still or jump between frames).
    let mut prev = [0.0f64; 2];
    for frame in 0..260 {
        if frame == 60 {
            server_add_peer(server.world_mut(), PeerId(2));
            joined[1] = true;
        }
        pump(&mut server, &mut clients, &joined);
        let st = tick(&server);
        let s_time = time(&server, server_player);
        if st >= START && s_time > 0.0 && s_time < 2.0 {
            assert!(
                (s_time - expected(st)).abs() < 1.0e-6,
                "server at tick {st}: {s_time} (expected {})",
                expected(st)
            );
        }
        for (i, (client, player)) in clients.iter().enumerate() {
            if !joined[i] {
                continue;
            }
            let ct = tick(client);
            let c_time = time(client, *player);
            if c_time > 0.0 && c_time < 2.0 {
                let slot = if i == 0 {
                    &mut client_started
                } else {
                    &mut late_started
                };
                if slot.is_none() {
                    // The start frame: the clock is where the tick seen on it says.
                    *slot = Some(ct);
                    assert!(
                        (c_time - expected(ct)).abs() < 1.0e-6,
                        "client {} started at its tick {ct}: {c_time} (expected {})",
                        i + 1,
                        expected(ct)
                    );
                } else {
                    // Then it plays locally, one step a frame.
                    assert!(
                        (c_time - prev[i] - 1.0 / HZ).abs() < 1.0e-6,
                        "client {} plays a step a frame: {} -> {c_time}",
                        i + 1,
                        prev[i]
                    );
                }
            }
            prev[i] = c_time;
        }
    }
    let client_started = client_started.expect("the first client started");
    assert!(
        (START..=START + 1).contains(&client_started),
        "the first client started on the announced tick (its tick may jump one): {client_started}"
    );
    let late_started = late_started.expect("the late joiner started");
    assert!(late_started > START + 30, "joined late: {late_started}");
    assert!(
        server.world().resource::<NetCutsceneServer>().sent() >= 2,
        "both peers got the start"
    );
    let on_time = clients[0].0.world().resource::<Seen>().0.clone();
    let late = clients[1].0.world().resource::<Seen>().0.clone();
    assert!(on_time.iter().all(|(_, skipped)| !skipped), "{on_time:?}");
    assert!(
        late.iter().any(|(n, s)| n == "a" && *s) && late.iter().any(|(n, s)| n == "b" && *s),
        "the markers before the join arrive skipped: {late:?}"
    );
    let names = |v: &[(String, bool)]| v.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    assert_eq!(
        names(&late),
        names(&on_time),
        "the same markers in the same order"
    );
    // The announcement stays up, yet every peer played the start once.
    let server_seen = server.world().resource::<Seen>().0.clone();
    assert_eq!(
        names(&server_seen),
        ["a", "b", "c", "end"],
        "played once: {server_seen:?}"
    );
    assert_eq!(
        server.world().resource::<Ended>().0,
        1,
        "the server's cutscene ended once"
    );
    for (client, _) in &clients {
        assert_eq!(
            client.world().resource::<Ended>().0,
            1,
            "each client's ended once"
        );
    }
    assert_eq!(names(&on_time), ["a", "b", "c", "end"]);
}
