// SPDX-License-Identifier: MPL-2.0
//! Spec 60 § 2.1, § 2.2 and § 2.4 over real sockets: a scripted DCC peer drives the session state
//! machine and the item states end to end through a project's `Regia.toml`, the reader threads,
//! the `PreUpdate` systems and the agent methods, the way an add-on and an artist would.
//!
//! - an unknown client lands in `pending`; its data frames are dropped and counted, nothing spawns;
//! - `dcc_feed.accept` makes it `active` and sends `session_state` and a `resync`, the peer re-sends,
//!   the item maps onto one entity;
//! - fixed-step engine time with no frames makes the item `stalled`; `goodbye` makes it `detached`;
//! - a second, allow-listed client on TCP with the token rebinds the same key onto the same entity;
//! - a third client declaring that key meanwhile gets `item_conflict` and keeps its session;
//! - `dcc_feed.disconnect { despawn: true }` closes the second session and removes the entity.
//!
//! The session phases the systems publish only move forward. Waits are blocking socket reads or
//! ticks until a condition holds; the time limit only turns a hang into a failure.
#![cfg(all(feature = "dcc-feed", unix))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use regia_agent::dcc_feed::methods;
use regia_agent::dcc_feed::net::feed_dir;
use regia_agent::dcc_feed::registry::ItemState;
use regia_agent::dcc_feed::session::Phase;
use regia_agent::dcc_feed::wire::{
    self, FeedKind, FrameInfo, FrameReader, Hello, ItemDeclare, Limits, Message, Reassembler, TransformBlock,
    TransformFrame,
};
use regia_agent::dcc_feed::{
    DccFeedClock, DccFeedPlugin, DccFeedRegistry, DccFeedServer, FeedItem, FeedSessionChanged, item_guid,
};
use regia_app::prelude::*;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

/// Turns a hang into a failure; nothing is timed against it.
const SAFETY_LIMIT: Duration = Duration::from_secs(120);
const STEP: f64 = 1.0 / 60.0;
const KEY: &str = "Crate";

/// Every phase change the systems published, in order.
#[derive(Resource, Default)]
struct Phases(Vec<FeedSessionChanged>);

fn record_phases(mut changes: MessageReader<FeedSessionChanged>, mut seen: ResMut<Phases>) {
    seen.0.extend(changes.read().copied());
}

fn live_app(dir: &Path) -> App {
    let toml = "[dcc_feed]\nenabled = true\nlisten = [\"unix\", \"tcp:127.0.0.1:0\"]\nstall_after_ms = 100\n\
                auto_accept = [\"render-farm-tool\"]\n";
    std::fs::write(dir.join("Regia.toml"), toml).expect("write Regia.toml");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.add_plugins(DccFeedPlugin {
        project_dir: Some(dir.to_path_buf()),
    });
    app.init_resource::<Phases>();
    app.add_systems(Update, record_phases);
    app.finish_plugins();
    app.world_mut().resource_mut::<DccFeedClock>().fixed_step_s = Some(STEP);
    assert_eq!(
        app.world().resource::<DccFeedServer>().listeners().len(),
        2,
        "the Unix socket and loopback TCP listen"
    );
    app
}

/// One side of a fake DCC add-on: a stream, a frame reader and the outgoing `seq` counter.
struct Peer<S: Read + Write> {
    out: S,
    reader: FrameReader<S>,
    assembler: Reassembler,
    seq: u32,
}

impl<S: Read + Write> Peer<S> {
    fn send(&mut self, msg: &Message) {
        self.seq += 1;
        let mut frame = Vec::new();
        wire::encode(msg, self.seq, &mut frame).expect("encode");
        self.out.write_all(&frame).expect("write");
    }

    /// The next message, or `None` when the server closed the stream.
    fn recv(&mut self) -> Option<Message> {
        let limits = Limits::default();
        loop {
            let (header, payload) = self.reader.next(&limits).ok()??;
            if let Some((head, body)) = self.assembler.push(header, payload, &limits).expect("fragments") {
                return Some(wire::decode(head, &body, &limits).expect("decode"));
            }
        }
    }

    /// Reads until `want` matches, skipping acks, pings and anything else on the way.
    fn until(&mut self, what: &str, want: impl Fn(&Message) -> bool) -> Message {
        loop {
            match self.recv() {
                Some(msg) if want(&msg) => return msg,
                Some(_) => {}
                None => panic!("the stream closed before {what}"),
            }
        }
    }

    fn hello(&mut self, client: &str, token: &str) {
        self.send(&Message::Hello(Hello {
            min_version: 1,
            max_version: 1,
            client_name: client.to_owned(),
            client_version: "4.2".to_owned(),
            dcc: "scripted test peer".to_owned(),
            space: "regia".to_owned(),
            unit_scale: 1.0,
            caps: 0,
            token: token.to_owned(),
        }));
    }

    fn declare(&mut self, handle: u32) {
        self.send(&Message::ItemDeclare(ItemDeclare {
            handle,
            key: KEY.to_owned(),
            kind: FeedKind::Transform,
            parent_handle: 0,
            display_name: KEY.to_owned(),
            variant: 0,
            channels: Vec::new(),
        }));
    }

    fn transform(&mut self, handle: u32, frame_id: u64, x: f32) {
        self.send(&Message::Transform(TransformFrame {
            handle,
            info: FrameInfo {
                frame_id,
                source_time_ns: frame_id * 16_666_667,
                timecode: None,
            },
            xf: TransformBlock {
                translation: [x, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
                world_space: false,
            },
        }));
    }
}

fn unix_peer(path: &Path) -> Peer<UnixStream> {
    let stream = UnixStream::connect(path).expect("connect to the feed socket");
    stream.set_read_timeout(Some(SAFETY_LIMIT)).expect("timeout");
    Peer {
        reader: FrameReader::new(stream.try_clone().expect("clone")),
        out: stream,
        assembler: Reassembler::new(),
        seq: 0,
    }
}

fn tcp_peer(address: &str) -> Peer<TcpStream> {
    let stream = TcpStream::connect(address).expect("connect over TCP");
    stream.set_read_timeout(Some(SAFETY_LIMIT)).expect("timeout");
    Peer {
        reader: FrameReader::new(stream.try_clone().expect("clone")),
        out: stream,
        assembler: Reassembler::new(),
        seq: 0,
    }
}

fn tick_until(app: &mut App, what: &str, done: impl Fn(&mut App) -> bool) {
    let started = Instant::now();
    while !done(app) {
        app.update();
        assert!(started.elapsed() < SAFETY_LIMIT, "never reached: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn status(app: &mut App) -> Value {
    methods::status(app.world_mut(), &json!({})).expect("dcc_feed.status")
}

fn session_of(app: &mut App, client: &str) -> Option<(u64, String, Value)> {
    let status = status(app);
    status["sessions"].as_array()?.iter().find_map(|s| {
        (s["client"] == client && s["state"] != "closed").then(|| {
            (
                s["id"].as_u64().expect("id"),
                s["state"].as_str().expect("state").to_owned(),
                s.clone(),
            )
        })
    })
}

fn item(app: &App) -> Option<(Entity, ItemState, u64)> {
    let record = app
        .world()
        .resource::<DccFeedRegistry>()
        .table
        .get(item_guid(KEY))?;
    let entity = record.entity?;
    let feed = app.world().get::<FeedItem>(entity)?;
    Some((entity, feed.state, feed.frames_applied))
}

#[test]
fn a_scripted_peer_walks_the_session_and_item_states() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_peer");
    let mut app = live_app(&dir);
    let state = feed_dir(&dir);
    let token = std::fs::read_to_string(state.join("token")).expect("the token file");
    let endpoint: Value =
        serde_json::from_slice(&std::fs::read(state.join("endpoint.json")).expect("endpoint")).expect("json");
    let tcp_address = endpoint["transports"]
        .as_array()
        .expect("transports")
        .iter()
        .find(|t| t["transport"] == "tcp")
        .and_then(|t| t["address"].as_str())
        .expect("a tcp transport")
        .to_owned();

    // 1. An unknown client on the Unix socket: no token needed, but approval is.
    let mut artist = unix_peer(&state.join("feed.sock"));
    artist.hello("blender-addon", "");
    let Message::HelloAck(ack) = artist.until("hello_ack", |m| matches!(m, Message::HelloAck(_))) else {
        unreachable!()
    };
    assert!(!ack.accepted, "an unknown client waits for approval");
    artist.declare(1);
    artist.transform(1, 1, 2.0);
    tick_until(&mut app, "the dropped frame is counted", |app| {
        session_of(app, "blender-addon").is_some_and(|(_, state, s)| {
            state == "pending" && s["dropped_not_accepted"].as_u64().unwrap_or(0) >= 1
        })
    });
    for _ in 0..3 {
        app.update();
    }
    assert!(item(&app).is_none(), "nothing spawns before approval");
    let (first, _, _) = session_of(&mut app, "blender-addon").expect("pending session");

    // 2. Approval: state and resync go out at once; the peer re-sends its full state.
    let reply = methods::accept(app.world_mut(), &json!({ "session": first })).expect("dcc_feed.accept");
    assert_eq!(reply["accepted"], true);
    let Message::SessionState(active) =
        artist.until("session_state", |m| matches!(m, Message::SessionState(_)))
    else {
        unreachable!()
    };
    assert_eq!(active.state, Phase::Active as u8);
    let Message::Resync(resync) = artist.until("resync", |m| matches!(m, Message::Resync(_))) else {
        unreachable!()
    };
    assert_eq!(resync.handle, 0, "every item");
    artist.declare(1);
    artist.transform(1, 2, 2.0);
    tick_until(&mut app, "the item applies", |app| {
        item(app).is_some_and(|(_, state, frames)| state == ItemState::Active && frames == 1)
    });
    let (entity, _, _) = item(&app).expect("item");
    assert_eq!(
        app.world().get::<Transform>(entity).map(|t| t.translation.x),
        Some(2.0)
    );
    let again = methods::accept(app.world_mut(), &json!({ "session": first }));
    assert_eq!(
        again.expect_err("twice").data.expect("data")["reason"],
        "already_accepted"
    );

    // 3. Silence on the fixed step: 100 ms at 60 Hz is six ticks, so `stalled` by the eighth.
    let mut ticks = 0;
    while item(&app).map(|i| i.1) == Some(ItemState::Active) {
        app.update();
        ticks += 1;
        assert!(ticks <= 8, "still active after {ticks} silent ticks");
    }
    assert!(ticks >= 6, "stalled after only {ticks} ticks of 100 ms at 60 Hz");
    assert_eq!(item(&app).map(|i| i.1), Some(ItemState::Stalled));

    // 4. Goodbye: the session closes, the entity stays, frozen and still feed-owned.
    artist.send(&Message::Goodbye("artist closed the file".into()));
    tick_until(&mut app, "the detach", |app| {
        item(app).map(|i| i.1) == Some(ItemState::Detached)
    });
    assert_eq!(item(&app).map(|i| i.0), Some(entity));

    // 5. An allow-listed tool over TCP with the token rebinds the key onto the same entity.
    let mut tool = tcp_peer(&tcp_address);
    tool.hello("render-farm-tool", token.trim());
    let Message::HelloAck(ack) = tool.until("hello_ack", |m| matches!(m, Message::HelloAck(_))) else {
        unreachable!()
    };
    assert!(ack.accepted, "the allow list accepts it");
    tool.declare(7);
    tool.transform(7, 10, 5.0);
    // The declaration and the frame may land in different ticks: wait for the frame.
    tick_until(&mut app, "the rebind", |app| {
        app.world().get::<Transform>(entity).map(|t| t.translation.x) == Some(5.0)
    });
    assert_eq!(item(&app).map(|i| (i.0, i.1)), Some((entity, ItemState::Active)));

    // 6. A second live declaration of the key is refused for that handle only. One hello per peer
    // per second of engine time (spec § 9.1), and both TCP clients come from 127.0.0.1: let a
    // second of fixed steps pass first.
    for _ in 0..61 {
        app.update();
    }
    let mut rival = tcp_peer(&tcp_address);
    rival.hello("render-farm-tool", token.trim());
    rival.until("hello_ack", |m| matches!(m, Message::HelloAck(_)));
    rival.declare(3);
    // The error goes out when the declaration is applied; tick until the peer has it.
    let started = Instant::now();
    let conflict = loop {
        app.update();
        rival
            .out
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("timeout");
        if let Some(Message::Error(e)) = rival.recv() {
            break e;
        }
        assert!(started.elapsed() < SAFETY_LIMIT, "no item_conflict");
    };
    assert_eq!(conflict.code, "item_conflict");
    assert_eq!(conflict.handle, 3);
    assert!(!conflict.fatal, "the rest of its session continues");
    assert_eq!(item(&app).map(|i| i.0), Some(entity), "the first owner keeps it");

    // 7. The kill switch: disconnect with despawn removes the entity and closes the stream.
    let second = status(&mut app)["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|s| {
            s["client"] == "render-farm-tool" && s["state"] == "active" && s["items"].as_u64() == Some(1)
        })
        .and_then(|s| s["id"].as_u64())
        .expect("the owning session");
    let gone = methods::disconnect(app.world_mut(), &json!({ "session": second, "despawn": true }))
        .expect("dcc_feed.disconnect");
    assert_eq!(gone["closed"], true);
    assert_eq!(gone["despawned"], 1);
    app.update();
    assert!(app.world().get_entity(entity).is_err(), "despawned");
    tool.out.set_read_timeout(Some(SAFETY_LIMIT)).expect("timeout");
    while tool.recv().is_some() {}

    // The published phases of the first session only ever moved forward, from pending to closed.
    tick_until(&mut app, "the first session closes", |app| {
        app.world()
            .resource::<Phases>()
            .0
            .iter()
            .any(|c| c.session == first && c.to == Phase::Closed)
    });
    let path: Vec<(Option<Phase>, Phase)> = app
        .world()
        .resource::<Phases>()
        .0
        .iter()
        .filter(|c| c.session == first)
        .map(|c| (c.from, c.to))
        .collect();
    assert_eq!(path.first(), Some(&(None, Phase::Pending)), "{path:?}");
    assert!(path.contains(&(Some(Phase::Pending), Phase::Active)), "{path:?}");
    assert!(
        path.windows(2)
            .all(|w| (w[0].1 as u8) < (w[1].1 as u8) && w[1].0 == Some(w[0].1)),
        "phases only move forward: {path:?}"
    );
    assert_eq!(path.last().map(|p| p.1), Some(Phase::Closed));

    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}
