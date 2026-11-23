// SPDX-License-Identifier: MPL-2.0
//! The reference client (`specs/60` § 8, tests 1, 11 and 14): its output against a second decoder
//! written only from the published byte tables, `--watch` diffs, and record and replay through a
//! real world. A glTF written by the test is the fixture.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use regia_agent::dcc_feed::core::{FeedCore, ReplayOptions};
use regia_agent::dcc_feed::net::lock;
use regia_agent::dcc_feed::recording::Recording;
use regia_agent::dcc_feed::registry::ItemState;
use regia_agent::dcc_feed::session::Transport;
use regia_agent::dcc_feed::settings::DccFeedSettings;
use regia_agent::dcc_feed::wire::{self, Message};
use regia_agent::dcc_feed::{DccFeedClock, DccFeedPlugin, DccFeedRegistry, DccFeedServer, item_guid};
use regia_app::prelude::*;
use regia_cli::dcc_feed::{FeedScene, FrameIds, ItemData, SceneItem, adopt_handles, diff_scenes, load_scene};
use regia_scene::prelude::Transform;
use serde_json::json;

const NAMES: [&str; 2] = ["Hull", "Lamp"];

/// A file with a mesh node (a triangle), a plain child node and an animation of `Lamp`.
fn fixture(dir: &Path, name: &str, hull_y: f32, lamp_x: f32) -> PathBuf {
    let mut bin = Vec::new();
    for v in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, hull_y, 0.0] {
        bin.extend(v.to_le_bytes());
    }
    for i in [0u16, 1, 2, 0] {
        bin.extend(i.to_le_bytes()); // the fourth pads to 4 bytes
    }
    let times_at = bin.len();
    for t in [0.0f32, 1.0] {
        bin.extend(t.to_le_bytes());
    }
    let values_at = bin.len();
    for v in [0.0f32, 0.0, 0.0, 2.0, 0.0, 0.0] {
        bin.extend(v.to_le_bytes());
    }
    let uri = format!(
        "data:application/octet-stream;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bin)
    );
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [
            { "name": NAMES[0], "mesh": 0, "children": [1] },
            { "name": NAMES[1], "translation": [lamp_x, 1.0, 0.0] },
        ],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0 }, "indices": 1 }] }],
        "buffers": [{ "uri": uri, "byteLength": bin.len() }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": 36, "byteLength": 6 },
            { "buffer": 0, "byteOffset": times_at, "byteLength": 8 },
            { "buffer": 0, "byteOffset": values_at, "byteLength": 24 },
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
              "min": [0.0, 0.0, 0.0], "max": [1.0, hull_y, 0.0] },
            { "bufferView": 1, "componentType": 5123, "count": 3, "type": "SCALAR" },
            { "bufferView": 2, "componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [1.0] },
            { "bufferView": 3, "componentType": 5126, "count": 2, "type": "VEC3" },
        ],
        "animations": [{
            "name": "Slide",
            "samplers": [{ "input": 2, "output": 3 }],
            "channels": [{ "sampler": 0, "target": { "node": 1, "path": "translation" } }],
        }],
    });
    let path = dir.join(name);
    std::fs::write(&path, doc.to_string()).expect("write fixture");
    path
}

fn scene(dir: &Path, hull_y: f32, lamp_x: f32) -> FeedScene {
    load_scene(&fixture(dir, "scene.gltf", hull_y, lamp_x)).expect("loads")
}

fn frames_of(messages: &[Message]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        wire::encode(msg, i as u32 + 1, &mut out).expect("encode");
    }
    out
}

/// A second decoder, written only from the tables of `docs/formats/dcc-feed-protocol-v1.md`.
mod tables {
    pub struct Cursor<'a>(pub &'a [u8]);

    impl Cursor<'_> {
        pub fn take(&mut self, n: usize) -> Vec<u8> {
            let (head, tail) = self.0.split_at(n);
            self.0 = tail;
            head.to_vec()
        }
        pub fn u8(&mut self) -> u8 {
            self.take(1)[0]
        }
        pub fn u16(&mut self) -> u16 {
            let b = self.take(2);
            u16::from_le_bytes([b[0], b[1]])
        }
        pub fn u32(&mut self) -> u32 {
            let b = self.take(4);
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        }
        pub fn f32(&mut self) -> f32 {
            f32::from_bits(self.u32())
        }
        pub fn string(&mut self) -> String {
            let n = usize::from(self.u16());
            String::from_utf8(self.take(n)).expect("utf-8")
        }
        pub fn frame_info(&mut self) -> u64 {
            let id = u64::from_le_bytes(self.take(8).try_into().expect("8 bytes"));
            self.take(8); // source_time_ns
            assert_eq!(self.u8(), 0, "the client sends no timecode");
            id
        }
    }

    /// `(msg_type, payload)` of each frame: `u32 len` (8 + payload), `u16 type`, `u16 flags`,
    /// `u32 seq`.
    pub fn split_frames(mut bytes: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            let len = u32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")) as usize;
            let msg_type = u16::from_le_bytes([bytes[4], bytes[5]]);
            assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 0, "flags");
            out.push((msg_type, bytes[12..4 + len].to_vec()));
            bytes = &bytes[4 + len..];
        }
        out
    }
}

/// Spec § 8 test 1: the full state of the fixture, read back from the byte tables alone.
#[test]
fn the_client_output_matches_the_published_tables() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_tables");
    let scene = scene(&dir, 1.0, 3.0);
    let mut ids = FrameIds::default();
    let frames = tables::split_frames(&frames_of(&scene.full_state(&mut ids)));
    let types: Vec<u16> = frames.iter().map(|f| f.0).collect();
    assert_eq!(
        types,
        vec![0x0010, 0x0020, 0x0023, 0x0010, 0x0020, 0x0026],
        "declare, transform, mesh, declare, transform, clip_push"
    );

    let mut c = tables::Cursor(&frames[0].1);
    assert_eq!(c.u32(), 1, "handle");
    assert_eq!(c.string(), "Hull");
    assert_eq!(c.u8(), 2, "kind mesh");
    assert_eq!(c.u32(), 0, "no parent");

    let mut c = tables::Cursor(&frames[3].1);
    assert_eq!(c.u32(), 2);
    assert_eq!(c.string(), "Lamp");
    assert_eq!(c.u8(), 1, "kind transform");
    assert_eq!(c.u32(), 1, "parent handle");

    let mut c = tables::Cursor(&frames[4].1);
    assert_eq!(c.u32(), 2);
    assert_eq!(c.frame_info(), 1);
    let t = [c.f32(), c.f32(), c.f32()];
    assert_eq!(t, [3.0, 1.0, 0.0]);
    let r = [c.f32(), c.f32(), c.f32(), c.f32()];
    assert_eq!(r, [0.0, 0.0, 0.0, 1.0]);

    let mut c = tables::Cursor(&frames[2].1);
    assert_eq!(c.u32(), 1);
    assert_eq!(c.frame_info(), 2, "the second frame of the item");
    assert_eq!(c.u8(), 0, "a full update");
    let sections = c.u16();
    let mut seen = Vec::new();
    for _ in 0..sections {
        let id = c.u8();
        let _elem = c.u8();
        let _index = c.u16();
        let count = c.u32();
        let bytes = c.u32() as usize;
        c.take(bytes);
        seen.push((id, count));
    }
    assert!(seen.contains(&(1, 3)), "three positions: {seen:?}");
    assert!(seen.contains(&(6, 3)), "three indices: {seen:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec § 8 test 14: a moved node is one `transform`; an edited mesh is one `mesh`.
#[test]
fn watch_diffs_send_only_what_changed() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_watch");
    let base = scene(&dir, 1.0, 3.0);
    let mut ids = FrameIds::default();
    let _ = base.full_state(&mut ids);

    let moved = diff_scenes(&base, &scene(&dir, 1.0, 4.0), &mut ids);
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert!(matches!(&moved[0], Message::Transform(t) if t.handle == 2 && t.xf.translation[0] == 4.0));

    let edited = diff_scenes(&base, &scene(&dir, 2.0, 3.0), &mut ids);
    assert_eq!(edited.len(), 1, "{edited:?}");
    assert!(matches!(&edited[0], Message::Mesh(m) if m.handle == 1 && m.update_mode == 1));

    assert!(
        diff_scenes(&base, &scene(&dir, 1.0, 3.0), &mut ids).is_empty(),
        "no change, no traffic"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clips_sample_linearly_as_transform_frames() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_clip");
    let scene = scene(&dir, 1.0, 3.0);
    let clip = scene.clips.iter().find(|c| c.name == "Slide").expect("clip");
    assert_eq!(clip.duration_s, 1.0);
    let mut ids = FrameIds::default();
    let frames = clip.frames_at(&scene, 0.5, &mut ids);
    assert_eq!(frames.len(), 1);
    assert!(matches!(&frames[0], Message::Transform(t) if t.handle == 2 && t.xf.translation[0] == 1.0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// What `send --animate --record` writes: the full state, then one tick per clip step.
fn recording(scene: &FeedScene) -> Recording {
    let mut rec = Recording::new("tool", "regia dcc-feed");
    let mut ids = FrameIds::default();
    let mut seq = 1;
    let mut push = |rec: &mut Recording, tick: u64, msg: &Message| {
        let mut frame = Vec::new();
        wire::encode(msg, seq, &mut frame).expect("encode");
        seq += 1;
        rec.push(tick, frame);
    };
    for msg in scene.full_state(&mut ids) {
        push(&mut rec, 0, &msg);
    }
    let clip = &scene.clips[0];
    for i in 1..=6u32 {
        ids.time_ns = u64::from(i) * 33_333_333;
        for msg in clip.frames_at(scene, i as f32 / 6.0, &mut ids) {
            push(&mut rec, u64::from(i), &msg);
        }
    }
    rec
}

fn world_state(app: &App) -> Vec<(String, [f32; 3], u64)> {
    let registry = app.world().resource::<DccFeedRegistry>();
    registry
        .table
        .iter()
        .map(|i| {
            let t = i
                .entity
                .and_then(|e| app.world().get::<Transform>(e))
                .copied()
                .unwrap_or_default();
            (i.key.clone(), t.translation.to_array(), i.frames_applied)
        })
        .collect()
}

/// Spec § 8 test 11: the client's take replayed twice into fresh worlds, bit-equal at ticks 1, 10
/// and 60, and a file round trip on the way.
#[test]
fn the_same_take_replays_identically() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_replay");
    let scene = scene(&dir, 1.0, 3.0);
    let path = dir.join("take.rfdrec");
    recording(&scene).write_file(&path).expect("write");
    let run = || {
        let settings = DccFeedSettings {
            auto_accept: vec!["tool".to_owned()],
            ..DccFeedSettings::default()
        };
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.add_plugins(DccFeedPlugin::default());
        let server = DccFeedServer::new(&settings, None);
        let core: Arc<Mutex<FeedCore>> = server.core().clone();
        app.insert_resource(settings);
        app.insert_resource(server);
        app.insert_resource(DccFeedClock {
            fixed_step_s: Some(1.0 / 60.0),
            now_s: 0.0,
        });
        lock(&core)
            .start_replay(&path, &ReplayOptions::default())
            .expect("replay");
        let mut states = Vec::new();
        for tick in 1..=60 {
            app.update();
            if matches!(tick, 1 | 10 | 60) {
                states.push(world_state(&app));
            }
        }
        (states, app)
    };
    let (a, app) = run();
    let (b, _) = run();
    assert_eq!(a, b);
    let lamp = a[2].iter().find(|i| i.0 == "Lamp").expect("lamp");
    assert!(
        (lamp.1[0] - 2.0).abs() < 1e-5,
        "the clip reached its end: {lamp:?}"
    );
    assert!(
        app.world()
            .resource::<DccFeedRegistry>()
            .table
            .get(item_guid("Hull"))
            .is_some(),
        "the mesh item applied"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn renumbered_with(scene: &FeedScene, key: &str) -> FeedScene {
    // A new first item shifts every later position in the file, as an exporter would.
    let mut next = scene.clone();
    for item in &mut next.items {
        item.declare.handle += 1;
        if item.declare.parent_handle != 0 {
            item.declare.parent_handle += 1;
        }
    }
    let mut first: SceneItem = next.items[1].clone();
    first.declare.handle = 1;
    first.declare.parent_handle = 0;
    first.declare.key = key.to_owned();
    first.declare.display_name = key.to_owned();
    next.items.insert(0, first);
    next
}

/// Handles belong to the session: an item added early in the file must not renumber the rest.
#[test]
fn an_added_item_keeps_the_sessions_handles() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_handles");
    let base = scene(&dir, 1.0, 3.0);
    let mut ids = FrameIds::default();
    let _ = base.full_state(&mut ids);
    let next = adopt_handles(&base, &renumbered_with(&base, "Added"));
    let handle = |key: &str| {
        let item = next.items.iter().find(|i| i.declare.key == key).expect("item");
        (item.declare.handle, item.declare.parent_handle)
    };
    assert_eq!(handle("Hull"), (1, 0));
    assert_eq!(handle("Lamp"), (2, 1), "the parent link follows");
    assert_eq!(handle("Added"), (3, 0));

    let changes = diff_scenes(&base, &next, &mut ids);
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert!(matches!(&changes[0], Message::ItemDeclare(d) if d.handle == 3 && d.key == "Added"));
    assert!(matches!(&changes[1], Message::Transform(t) if t.handle == 3));

    // The next change after the swap still addresses the session's numbers.
    let mut moved = next.clone();
    let lamp = moved
        .items
        .iter_mut()
        .find(|i| i.declare.key == "Lamp")
        .expect("lamp");
    lamp.xf.translation[0] = 9.0;
    let changes = diff_scenes(&next, &moved, &mut ids);
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert!(matches!(&changes[0], Message::Transform(t) if t.handle == 2));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_key_that_changes_kind_is_deleted_first() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_kind");
    let base = scene(&dir, 1.0, 3.0);
    let mut ids = FrameIds::default();
    let mut next = base.clone();
    let lamp = next
        .items
        .iter_mut()
        .find(|i| i.declare.key == "Lamp")
        .expect("lamp");
    lamp.declare.kind = regia_agent::dcc_feed::wire::FeedKind::Light;
    lamp.declare.variant = 1;
    lamp.data = ItemData::Light {
        color: [1.0; 3],
        intensity: 10.0,
        range: 0.0,
        inner: 0.0,
        outer: 0.0,
    };
    let changes = diff_scenes(&base, &next, &mut ids);
    assert!(matches!(&changes[0], Message::ItemDelete(2)), "{changes:?}");
    assert!(matches!(&changes[1], Message::ItemDeclare(d) if d.handle == 2));
    assert!(matches!(&changes[2], Message::Light(l) if l.handle == 2));
    assert_eq!(changes.len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_changed_clip_or_parent_is_sent_and_an_unchanged_one_is_not() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_clip_diff");
    let base = scene(&dir, 1.0, 3.0);
    let mut ids = FrameIds::default();
    let mut next = base.clone();
    next.clips[0].duration_s = 2.0;
    next.items[1].declare.parent_handle = 0;
    let changes = diff_scenes(&base, &next, &mut ids);
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert!(matches!(&changes[0], Message::ItemDeclare(d) if d.handle == 2 && d.parent_handle == 0));
    assert!(matches!(&changes[1], Message::ClipPush(c) if c.name == "Slide"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// The binary against a running world
// ---------------------------------------------------------------------------------------------

/// Only so a broken client fails the test instead of hanging it; nothing is timed against it.
const SAFETY_LIMIT: Duration = Duration::from_secs(120);

/// A world with the feed on, set up the way a project does it: through `Regia.toml`.
fn live_app(dir: &Path) -> App {
    let toml = "[dcc_feed]\nenabled = true\nauto_accept = [\"regia-dcc-feed\"]\n";
    std::fs::write(dir.join("Regia.toml"), toml).expect("write Regia.toml");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.add_plugins(DccFeedPlugin {
        project_dir: Some(dir.to_path_buf()),
    });
    app.finish_plugins();
    assert!(
        !app.world().resource::<DccFeedServer>().listeners().is_empty(),
        "the feed listens"
    );
    app
}

fn client(args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .arg("dcc-feed")
        .args(args)
        .stdout(Stdio::null())
        .spawn()
        .expect("start regia")
}

/// Ticks until the client exits, then twice more so its goodbye is handled.
fn tick_until_exit(app: &mut App, child: &mut Child) -> ExitStatus {
    let started = Instant::now();
    loop {
        app.update();
        if let Some(status) = child.try_wait().expect("wait") {
            app.update();
            app.update();
            return status;
        }
        assert!(started.elapsed() < SAFETY_LIMIT, "the client did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn tick_until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
    let started = Instant::now();
    while !done(app) {
        app.update();
        assert!(started.elapsed() < SAFETY_LIMIT, "never reached: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn translation_of(app: &App, key: &str) -> Option<[f32; 3]> {
    let entity = app
        .world()
        .resource::<DccFeedRegistry>()
        .table
        .get(item_guid(key))?
        .entity?;
    Some(app.world().get::<Transform>(entity)?.translation.to_array())
}

fn state_of(app: &App, key: &str) -> Option<ItemState> {
    Some(
        app.world()
            .resource::<DccFeedRegistry>()
            .table
            .get(item_guid(key))?
            .state,
    )
}

fn vertices_of(app: &App, key: &str) -> u32 {
    app.world()
        .resource::<DccFeedRegistry>()
        .meshes
        .get(&item_guid(key))
        .map_or(0, |m| m.data.vertex_count())
}

/// `send` finds the project's transport through `endpoint.json` with no flags (the Unix socket
/// where there is one), the mesh arrives through the live build path, and the session ends in
/// `detached`.
#[test]
fn the_binary_streams_a_file_into_a_running_project() {
    let dir = regia_test_utils::unique_temp_dir("dccx_unix");
    let file = fixture(&dir, "scene.gltf", 1.0, 3.0);
    let mut app = live_app(&dir);
    let mut child = client(&[
        "send",
        file.to_str().expect("utf-8"),
        "--project",
        dir.to_str().expect("utf-8"),
    ]);
    let status = tick_until_exit(&mut app, &mut child);
    assert!(status.success(), "{status}");
    assert_eq!(translation_of(&app, "Lamp"), Some([3.0, 1.0, 0.0]));
    tick_until(&mut app, "the mesh build", |app| vertices_of(app, "Hull") == 3);
    tick_until(&mut app, "the detach", |app| {
        state_of(app, "Lamp") == Some(ItemState::Detached)
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// TCP needs the token, `--record` keeps it out of the file, `dump` reads the file, and the take
/// of a real run replays into a fresh world.
#[test]
fn the_binary_records_a_tcp_session_that_replays() {
    let dir = regia_test_utils::unique_temp_dir("dccx_tcp");
    let file = fixture(&dir, "scene.gltf", 1.0, 3.0);
    let mut app = live_app(&dir);
    let server = app.world().resource::<DccFeedServer>();
    let token = lock(server.core()).token().to_owned();
    let endpoint = server
        .listeners()
        .iter()
        .find(|l| l.transport == "tcp")
        .map(|l| format!("tcp:{}", l.address))
        .expect("tcp listener");
    let take = dir.join("take.rfdrec");
    let token_file = dir.join(".regia/dcc_feed/token");
    let mut child = client(&[
        "send",
        file.to_str().expect("utf-8"),
        "--endpoint",
        &endpoint,
        "--token-file",
        token_file.to_str().expect("utf-8"),
        "--animate",
        "Slide",
        "--fps",
        "10",
        "--record",
        take.to_str().expect("utf-8"),
    ]);
    let status = tick_until_exit(&mut app, &mut child);
    assert!(status.success(), "{status}");
    assert_eq!(
        translation_of(&app, "Lamp").map(|t| t[0]),
        Some(2.0),
        "the clip reached its end"
    );

    let bytes = std::fs::read(&take).expect("the take");
    assert!(
        !bytes.windows(token.len()).any(|w| w == token.as_bytes()),
        "the recording must not hold the session token"
    );
    let dump = Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(["dcc-feed", "dump", take.to_str().expect("utf-8")])
        .output()
        .expect("dump");
    assert!(dump.status.success());
    let lines = String::from_utf8_lossy(&dump.stdout).lines().count();
    let recording = Recording::read_file(&take).expect("reads");
    assert_eq!(lines, recording.records.len());
    let last_tick = recording.records.iter().map(|r| r.sim_tick).max();
    assert_eq!(last_tick, Some(11), "one tick per clip step, 0 to 1 s at 10 fps");

    let mut replayed = App::new();
    replayed.add_plugins(regia_scene::ScenePlugin);
    replayed.add_plugins(DccFeedPlugin::default());
    let settings = DccFeedSettings {
        auto_accept: vec!["regia-dcc-feed".to_owned()],
        ..DccFeedSettings::default()
    };
    let server = DccFeedServer::new(&settings, None);
    lock(server.core())
        .start_replay(&take, &ReplayOptions::default())
        .expect("replay");
    replayed.insert_resource(settings);
    replayed.insert_resource(server);
    replayed.insert_resource(DccFeedClock {
        fixed_step_s: Some(1.0 / 60.0),
        now_s: 0.0,
    });
    for _ in 0..16 {
        replayed.update();
    }
    assert_eq!(translation_of(&replayed, "Lamp").map(|t| t[0]), Some(2.0));
    assert_eq!(vertices_of(&replayed, "Hull"), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec § 8 test 14 through the file system: `--watch` sees the exporter replace the file, and
/// killing the client detaches the items.
#[test]
fn the_binary_watches_the_file_and_sends_the_change() {
    let dir = regia_test_utils::unique_temp_dir("dccx_watch");
    let file = fixture(&dir, "scene.gltf", 1.0, 3.0);
    let mut app = live_app(&dir);
    let mut child = client(&[
        "send",
        file.to_str().expect("utf-8"),
        "--project",
        dir.to_str().expect("utf-8"),
        "--watch",
    ]);
    tick_until(&mut app, "the first state", |app| {
        translation_of(app, "Lamp") == Some([3.0, 1.0, 0.0])
    });
    let before = app
        .world()
        .resource::<DccFeedRegistry>()
        .table
        .get(item_guid("Hull"))
        .map(|i| i.frames_applied);

    let edited = fixture(&dir, "edited.gltf.tmp", 1.0, 4.0);
    std::fs::rename(&edited, &file).expect("replace the file");
    tick_until(&mut app, "the edit", |app| {
        translation_of(app, "Lamp") == Some([4.0, 1.0, 0.0])
    });
    let after = app
        .world()
        .resource::<DccFeedRegistry>()
        .table
        .get(item_guid("Hull"))
        .map(|i| i.frames_applied);
    assert_eq!(before, after, "the mesh item saw no traffic");

    child.kill().expect("kill");
    child.wait().expect("wait");
    tick_until(&mut app, "the detach", |app| {
        state_of(app, "Lamp") == Some(ItemState::Detached)
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// The wrappers around the agent methods, through the real protocol: `record`, `record --stop` and
/// `replay` against a running app, with a `--watch` client as the session being recorded.
#[test]
fn the_binary_records_and_replays_through_the_agent() {
    let dir = regia_test_utils::unique_temp_dir("dccx_agent");
    let file = fixture(&dir, "scene.gltf", 1.0, 3.0);
    let toml = "[dcc_feed]\nenabled = true\nauto_accept = [\"regia-dcc-feed\"]\n";
    std::fs::write(dir.join("Regia.toml"), toml).expect("write Regia.toml");
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(dir.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    app.finish_plugins();

    let project = dir.to_str().expect("utf-8");
    let path = file.to_str().expect("utf-8");
    let mut watching = client(&["send", path, "--project", project, "--watch"]);
    tick_until(&mut app, "the first state", |app| {
        translation_of(app, "Lamp") == Some([3.0, 1.0, 0.0])
    });

    let mut start = client(&["record", "take.rfdrec", "--project", project]);
    let status = tick_until_exit(&mut app, &mut start);
    assert!(status.success(), "record: {status}");
    let edited = fixture(&dir, "edited.gltf.tmp", 1.0, 4.0);
    std::fs::rename(&edited, &file).expect("replace the file");
    tick_until(&mut app, "the edit", |app| {
        translation_of(app, "Lamp") == Some([4.0, 1.0, 0.0])
    });
    let mut stop = client(&["record", "--stop", "--project", project]);
    let status = tick_until_exit(&mut app, &mut stop);
    assert!(status.success(), "record --stop: {status}");
    let take = Recording::read_file(&dir.join("take.rfdrec")).expect("the take");
    assert!(take.records.len() > 1, "a hello and the frames");

    watching.kill().expect("kill");
    watching.wait().expect("wait");
    tick_until(&mut app, "the detach", |app| {
        state_of(app, "Lamp") == Some(ItemState::Detached)
    });

    let mut replay = client(&["replay", "take.rfdrec", "--project", project]);
    let status = tick_until_exit(&mut app, &mut replay);
    assert!(status.success(), "replay: {status}");
    // Every record but the hello is applied or coalesced, none discarded, whenever the replay ended.
    let delivered = |app: &App| {
        let core = lock(app.world().resource::<DccFeedServer>().core());
        let session = core.sessions.iter().find(|s| s.transport == Transport::Replay);
        session.map_or(0, |s| {
            let c = core.counters(s.id);
            c.frames_drained + c.frames_coalesced
        })
    };
    let expected = take.records.len() as u64 - 1;
    tick_until(&mut app, "the replayed frames", |app| delivered(app) == expected);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `load_scene` on hostile glTF files returns an error instead of panicking (a panic aborts the
/// shipping binary).
#[test]
fn load_scene_rejects_hostile_gltf_without_panicking() {
    let dir = regia_test_utils::unique_temp_dir("dcc-feed-hostile");
    let cases = [
        r#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[]}],"images":[{}]}"#,
        r#"{"asset":{"version":"2.0"},"meshes":[{"primitives":[{"attributes":{"POSITION":0}}]}]}"#,
        r#"{"asset":{"version":"2.0"},"buffers":[{"uri":"%80","byteLength":4}]}"#,
    ];
    for (i, json) in cases.iter().enumerate() {
        let path = dir.join(format!("hostile{i}.gltf"));
        std::fs::write(&path, json).expect("write");
        assert!(load_scene(&path).is_err(), "case {i} must be an error");
    }
}

fn item<'a>(scene: &'a FeedScene, key: &str) -> &'a SceneItem {
    scene
        .items
        .iter()
        .find(|i| i.declare.key == key)
        .unwrap_or_else(|| panic!("no item {key:?} in {:?}", keys(scene)))
}

fn keys(scene: &FeedScene) -> Vec<&str> {
    scene.items.iter().map(|i| i.declare.key.as_str()).collect()
}

fn skeleton_joints(scene: &FeedScene) -> &[wire::JointStatic] {
    scene
        .items
        .iter()
        .find_map(|i| match &i.data {
            ItemData::Skeleton(joints, _) => Some(joints.as_slice()),
            _ => None,
        })
        .expect("a skeleton item")
}

fn near(a: [f32; 3], b: [f32; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
}

/// Audit finding 1: a node under a joint follows that joint, and a joint that owns a camera (or
/// mesh, or light) is an item of its own. `Root` (y = 2) → joint `Hand` (x = 1) → plain `Sword`,
/// and joint `Eye` with a camera under `Hand`.
#[test]
fn nodes_under_a_joint_follow_the_joint() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_joint_children");
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [
            { "name": "Root", "translation": [0.0, 2.0, 0.0], "children": [1] },
            { "name": "Hand", "translation": [1.0, 0.0, 0.0], "children": [2, 3] },
            { "name": "Sword", "translation": [0.0, 0.0, 1.0] },
            { "name": "Eye", "camera": 0 },
        ],
        "cameras": [{ "type": "perspective", "perspective": { "yfov": 1.0, "znear": 0.1 } }],
        "skins": [{ "joints": [1, 3] }],
    });
    let path = dir.join("rig.gltf");
    std::fs::write(&path, doc.to_string()).expect("write");
    let scene = load_scene(&path).expect("loads");

    let root = item(&scene, "Root");
    let hand = item(&scene, "Hand");
    assert_eq!(
        hand.declare.parent_handle, root.declare.handle,
        "Hand sits under Root"
    );
    assert_eq!(
        hand.xf.translation,
        [1.0, 0.0, 0.0],
        "with its own local placement"
    );
    let sword = item(&scene, "Sword");
    assert_eq!(
        sword.declare.parent_handle, hand.declare.handle,
        "Sword follows the Hand joint, not Root"
    );
    let eye = item(&scene, "Eye");
    assert!(
        matches!(eye.data, ItemData::Camera(_)),
        "a joint with a camera is emitted"
    );
    assert_eq!(eye.declare.parent_handle, hand.declare.handle);

    // The skeleton itself is unchanged in shape: Hand is the root joint (Root folded in), Eye its child.
    let joints = skeleton_joints(&scene);
    assert_eq!(joints.len(), 2);
    assert_eq!(joints[0].parent, -1);
    assert!(near(joints[0].t, [1.0, 2.0, 0.0]), "{:?}", joints[0].t);
    assert_eq!(joints[1].parent, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rig without attachments keeps its old item list: joints with only joint children are not
/// items.
#[test]
fn a_plain_rig_declares_no_joint_items() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_plain_rig");
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [
            { "name": "Hips", "children": [1] },
            { "name": "Spine", "translation": [0.0, 1.0, 0.0] },
        ],
        "skins": [{ "joints": [0, 1] }],
    });
    let path = dir.join("rig.gltf");
    std::fs::write(&path, doc.to_string()).expect("write");
    let scene = load_scene(&path).expect("loads");
    assert_eq!(keys(&scene), vec!["skin_0"], "only the skeleton");
    let joints = skeleton_joints(&scene);
    assert_eq!((joints[0].parent, joints[1].parent), (-1, 0));
    assert_eq!(joints[1].t, [0.0, 1.0, 0.0], "no fold, bit-identical");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Audit finding 2: a joint whose parent node is not a joint takes the nearest joint ancestor as
/// its parent, with the skipped nodes folded into its local transform, at rest and when animated.
/// `Armature` (z = 5) → joint `Hips` (y = 1) → `Offset` (z = 3, 90° about +Y) → joint `Spine` (x = 1).
#[test]
fn a_joint_under_a_plain_node_keeps_its_joint_parent() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_joint_fold");
    let mut bin = Vec::new();
    for v in [0.0f32, 1.0, 1.0, 0.0, 0.0, 2.0, 0.0, 0.0] {
        bin.extend(v.to_le_bytes());
    }
    let uri = format!(
        "data:application/octet-stream;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bin)
    );
    let half = std::f32::consts::FRAC_1_SQRT_2;
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [
            { "name": "Armature", "translation": [0.0, 0.0, 5.0], "children": [1] },
            { "name": "Hips", "translation": [0.0, 1.0, 0.0], "children": [2] },
            { "name": "Offset", "translation": [0.0, 0.0, 3.0], "rotation": [0.0, half, 0.0, half],
              "children": [3] },
            { "name": "Spine", "translation": [1.0, 0.0, 0.0] },
        ],
        "skins": [{ "joints": [1, 3] }],
        "buffers": [{ "uri": uri, "byteLength": bin.len() }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 8 },
            { "buffer": 0, "byteOffset": 8, "byteLength": 24 },
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [1.0] },
            { "bufferView": 1, "componentType": 5126, "count": 2, "type": "VEC3" },
        ],
        "animations": [{
            "name": "Stretch",
            "samplers": [{ "input": 0, "output": 1 }],
            "channels": [{ "sampler": 0, "target": { "node": 3, "path": "translation" } }],
        }],
    });
    let path = dir.join("rig.gltf");
    std::fs::write(&path, doc.to_string()).expect("write");
    let scene = load_scene(&path).expect("loads");

    let joints = skeleton_joints(&scene);
    assert_eq!(joints.len(), 2);
    assert_eq!(joints[0].name, "Hips");
    assert_eq!(joints[0].parent, -1);
    assert!(
        near(joints[0].t, [0.0, 1.0, 5.0]),
        "Armature folded into the root: {:?}",
        joints[0].t
    );
    assert_eq!(joints[1].name, "Spine");
    assert_eq!(joints[1].parent, 0, "Spine's parent is Hips, not a second root");
    // Offset's rotation turns Spine's +X onto -Z, then Offset's translation adds z = 3.
    assert!(
        near(joints[1].t, [0.0, 0.0, 2.0]),
        "Offset folded in: {:?}",
        joints[1].t
    );
    assert!(
        joints[1]
            .r
            .iter()
            .zip([0.0, half, 0.0, half])
            .all(|(a, b)| (a - b).abs() < 1e-5),
        "{:?}",
        joints[1].r
    );

    // Animated: Spine's local x = 2 at t = 1 goes through the same fold.
    let clip = scene.clips.iter().find(|c| c.name == "Stretch").expect("clip");
    let mut ids = FrameIds::default();
    let frames = clip.frames_at(&scene, 1.0, &mut ids);
    let pose = frames
        .iter()
        .find_map(|m| match m {
            Message::Pose(p) => Some(p),
            _ => None,
        })
        .expect("a pose frame");
    let spine_t = [pose.values[10], pose.values[11], pose.values[12]];
    assert!(near(spine_t, [0.0, 0.0, 1.0]), "{spine_t:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `skeleton_static` needs parents before children (the server treats a parent index at or after
/// the joint as a root). A skin that lists `Spine` before its parent `Hips` is sent `Hips` first,
/// with poses in the same order.
#[test]
fn joints_listed_child_first_are_sent_parents_first() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_joint_order");
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [
            { "name": "Hips", "translation": [0.0, 1.0, 0.0], "children": [1] },
            { "name": "Spine", "translation": [0.0, 2.0, 0.0], "children": [2] },
            { "name": "Head", "translation": [0.0, 3.0, 0.0] },
        ],
        "skins": [{ "joints": [2, 1, 0] }],
    });
    let path = dir.join("rig.gltf");
    std::fs::write(&path, doc.to_string()).expect("write");
    let scene = load_scene(&path).expect("loads");
    let joints = skeleton_joints(&scene);
    let order: Vec<(&str, i32)> = joints.iter().map(|j| (j.name.as_str(), j.parent)).collect();
    assert_eq!(order, vec![("Hips", -1), ("Spine", 0), ("Head", 1)]);
    let sources = scene
        .items
        .iter()
        .find_map(|i| match &i.data {
            ItemData::Skeleton(_, sources) => Some(sources),
            _ => None,
        })
        .expect("skeleton");
    assert_eq!(sources.iter().map(|s| s.node).collect::<Vec<_>>(), vec![0, 1, 2]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A skinned mesh's `JOINTS_0` values follow the joints when the skin is reordered parents first:
/// a vertex bound to file slot 0 (`Head`) is sent bound to `Head`'s wire index (2), not to `Hips`.
#[test]
fn audit_reordered_skin_joints_remap_the_mesh_joint_indices() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_joint_remap");
    let mut bin = Vec::new();
    for p in [[0.0_f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        bin.extend(p.iter().flat_map(|v| v.to_le_bytes()));
    }
    // JOINTS_0 per vertex in file skin slots: slot 0 = Head, 1 = Spine, 2 = Hips.
    for j in [[0_u16, 1, 2, 0], [1, 1, 1, 1], [2, 0, 0, 0]] {
        bin.extend(j.iter().flat_map(|v| v.to_le_bytes()));
    }
    for _ in 0..3 {
        bin.extend([1.0_f32, 0.0, 0.0, 0.0].iter().flat_map(|v| v.to_le_bytes()));
    }
    let uri = format!(
        "data:application/octet-stream;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bin)
    );
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0, 3] }],
        "nodes": [
            { "name": "Hips", "translation": [0.0, 1.0, 0.0], "children": [1] },
            { "name": "Spine", "translation": [0.0, 2.0, 0.0], "children": [2] },
            { "name": "Head", "translation": [0.0, 3.0, 0.0] },
            { "name": "Body", "mesh": 0, "skin": 0 },
        ],
        "skins": [{ "joints": [2, 1, 0] }],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0, "JOINTS_0": 1, "WEIGHTS_0": 2 } }] }],
        "buffers": [{ "byteLength": bin.len(), "uri": uri }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": 36, "byteLength": 24 },
            { "buffer": 0, "byteOffset": 60, "byteLength": 48 },
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
              "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0] },
            { "bufferView": 1, "componentType": 5123, "count": 3, "type": "VEC4" },
            { "bufferView": 2, "componentType": 5126, "count": 3, "type": "VEC4" },
        ],
    });
    let path = dir.join("skinned.gltf");
    std::fs::write(&path, doc.to_string()).expect("write");
    let scene = load_scene(&path).expect("loads");
    let names: Vec<&str> = skeleton_joints(&scene).iter().map(|j| j.name.as_str()).collect();
    assert_eq!(names, vec!["Hips", "Spine", "Head"]);
    let ItemData::Mesh(sections) = &item(&scene, "Body").data else {
        panic!("Body is not a mesh item");
    };
    let joints = sections
        .iter()
        .find(|s| s.section_id == wire::section::JOINTS)
        .expect("a joints section");
    let (pairs, _) = joints.data.as_chunks::<2>();
    let sent: Vec<u16> = pairs.iter().map(|b| u16::from_le_bytes(*b)).collect();
    // Head -> 2, Spine -> 1, Hips -> 0.
    assert_eq!(sent, vec![2, 1, 0, 2, 1, 1, 1, 1, 0, 2, 2, 2]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A base64 `data:` URI holding `floats` as little-endian f32, then `shorts` as little-endian u16.
fn data_uri(floats: &[f32], shorts: &[u16]) -> (String, usize) {
    let mut bin = Vec::new();
    for v in floats {
        bin.extend(v.to_le_bytes());
    }
    for v in shorts {
        bin.extend(v.to_le_bytes());
    }
    let uri = format!(
        "data:application/octet-stream;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bin)
    );
    (uri, bin.len())
}

/// The skeleton joint names of the skeleton item `mesh` is parented under.
fn joint_names_over(scene: &FeedScene, mesh: &SceneItem) -> Vec<String> {
    scene
        .items
        .iter()
        .find(|i| i.declare.handle == mesh.declare.parent_handle)
        .and_then(|i| match &i.data {
            ItemData::Skeleton(joints, _) => Some(joints.iter().map(|j| j.name.clone()).collect()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{} sits under no skeleton", mesh.declare.key))
}

/// The first joint index of every vertex in a mesh item's `JOINTS` section.
fn first_joints(mesh: &SceneItem) -> Vec<u16> {
    let ItemData::Mesh(sections) = &mesh.data else {
        panic!("{} is not a mesh", mesh.declare.key);
    };
    let joints = sections
        .iter()
        .find(|s| s.section_id == wire::section::JOINTS)
        .expect("a JOINTS section");
    joints
        .data
        .as_chunks::<8>()
        .0
        .iter()
        .map(|v| u16::from_le_bytes([v[0], v[1]]))
        .collect()
}

/// Review finding 1: `JOINTS_0` indexes the skin's `joints` list in file order, and the server
/// binds vertex joint `i` to wire joint `i`. A skin listed child first is sent parents first, so the
/// vertex joints are remapped with it, per node: `Body` and `Body2` share one mesh under two skins
/// (child first and parents first). Each vertex is weighted to file slot 0, 1, 2 of its skin, and
/// must name the same joint on the wire as in the file.
#[test]
fn skinned_vertices_keep_their_bones_when_joints_are_reordered() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_joint_remap");
    let positions = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
    let weights = [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
    let floats: Vec<f32> = positions.iter().chain(&weights).copied().collect();
    let joints = [0u16, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0];
    let (uri, len) = data_uri(&floats, &joints);
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0, 3, 4] }],
        "nodes": [
            { "name": "Hips", "translation": [0.0, 1.0, 0.0], "children": [1] },
            { "name": "Spine", "translation": [0.0, 2.0, 0.0], "children": [2] },
            { "name": "Head", "translation": [0.0, 3.0, 0.0] },
            { "name": "Body", "mesh": 0, "skin": 0 },
            { "name": "Body2", "mesh": 0, "skin": 1 },
        ],
        "skins": [{ "name": "ChildFirst", "joints": [2, 1, 0] }, { "name": "ParentsFirst", "joints": [0, 1, 2] }],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0, "WEIGHTS_0": 1, "JOINTS_0": 2 } }] }],
        "buffers": [{ "uri": uri, "byteLength": len }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": 36, "byteLength": 48 },
            { "buffer": 0, "byteOffset": 84, "byteLength": 24 },
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
              "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0] },
            { "bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC4" },
            { "bufferView": 2, "componentType": 5123, "count": 3, "type": "VEC4" },
        ],
    });
    let path = dir.join("rig.gltf");
    std::fs::write(&path, doc.to_string()).expect("write");
    let scene = load_scene(&path).expect("loads");

    for (key, file_names) in [
        ("Body", ["Head", "Spine", "Hips"]),
        ("Body2", ["Hips", "Spine", "Head"]),
    ] {
        let mesh = item(&scene, key);
        let wire_names = joint_names_over(&scene, mesh);
        assert_eq!(
            wire_names,
            ["Hips", "Spine", "Head"],
            "{key}: parents first on the wire"
        );
        let on_wire: Vec<&str> = first_joints(mesh)
            .iter()
            .map(|j| wire_names[usize::from(*j)].as_str())
            .collect();
        assert_eq!(on_wire, file_names, "{key}: each vertex keeps its file joint");
    }
    // A list that already is parents first is sent as read.
    assert_eq!(first_joints(item(&scene, "Body2")), vec![0, 1, 2]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `clip_push` messages of a scene's full state, by clip name.
fn clip_pushes(scene: &FeedScene) -> Vec<wire::ClipPush> {
    scene
        .full_state(&mut FrameIds::default())
        .into_iter()
        .filter_map(|m| match m {
            Message::ClipPush(c) => Some(c),
            _ => None,
        })
        .collect()
}

fn close(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
}

/// Review finding 2: a joint with skipped nodes above it carries `chain * local` in its bind, so
/// its pushed keys carry the chain too; a baked clip replaces the joint's rest value with them. The
/// rig of `a_joint_under_a_plain_node_keeps_its_joint_parent`: Spine's raw key x = 2 at t = 1 is the
/// pose value [0, 0, 1], and x = 1 at t = 0 is its bind [0, 0, 2].
#[test]
fn pushed_clips_fold_the_skipped_nodes_into_joint_keys() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_clip_fold");
    let (uri, len) = data_uri(&[0.0, 1.0, 1.0, 0.0, 0.0, 2.0, 0.0, 0.0], &[]);
    let half = std::f32::consts::FRAC_1_SQRT_2;
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [
            { "name": "Armature", "translation": [0.0, 0.0, 5.0], "children": [1] },
            { "name": "Hips", "translation": [0.0, 1.0, 0.0], "children": [2] },
            { "name": "Offset", "translation": [0.0, 0.0, 3.0], "rotation": [0.0, half, 0.0, half],
              "children": [3] },
            { "name": "Spine", "translation": [1.0, 0.0, 0.0] },
        ],
        "skins": [{ "joints": [1, 3] }],
        "buffers": [{ "uri": uri, "byteLength": len }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 8 },
            { "buffer": 0, "byteOffset": 8, "byteLength": 24 },
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [1.0] },
            { "bufferView": 1, "componentType": 5126, "count": 2, "type": "VEC3" },
        ],
        "animations": [{
            "name": "Stretch",
            "samplers": [{ "input": 0, "output": 1 }],
            "channels": [{ "sampler": 0, "target": { "node": 3, "path": "translation" } }],
        }],
    });
    let path = dir.join("rig.gltf");
    std::fs::write(&path, doc.to_string()).expect("write");
    let scene = load_scene(&path).expect("loads");
    let pushes = clip_pushes(&scene);
    let stretch = pushes.iter().find(|c| c.name == "Stretch").expect("clip");
    let spine: Vec<&wire::ClipTrack> = stretch.tracks.iter().filter(|t| t.target == "Spine").collect();
    assert_eq!(spine.len(), 1, "{:?}", stretch.tracks);
    let spine = spine[0];
    assert_eq!((spine.property, spine.interp), (0, 1));
    assert_eq!(spine.times, vec![0.0, 1.0]);
    assert!(
        close(&spine.values[3..6], &[0.0, 0.0, 1.0]),
        "t = 1: {:?}",
        spine.values
    );
    assert!(
        close(&spine.values[0..3], &[0.0, 0.0, 2.0]),
        "t = 0 is the bind: {:?}",
        spine.values
    );
    assert!(close(&spine.values[0..3], &skeleton_joints(&scene)[1].t));
    let _ = std::fs::remove_dir_all(&dir);
}

/// When a skipped node is animated too, the folded joint is resampled at the key times of every
/// track it depends on, and those keys equal what `--animate` sends at the same times. `Lift`
/// moves `Armature` (above the root joint `Hips`, which has no track of its own), turns `Offset`
/// (between `Hips` and `Spine`) and stretches `Spine`.
#[test]
fn pushed_clips_resample_joints_under_animated_skipped_nodes() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_client_clip_resample");
    let half = std::f32::consts::FRAC_1_SQRT_2;
    #[rustfmt::skip]
    let floats = [
        0.0f32, 2.0, // times A
        0.0, 1.0, // times B
        0.0, 0.0, 5.0, 0.0, 0.0, 7.0, // Armature translation
        0.0, 0.0, 0.0, 1.0, 0.0, half, 0.0, half, // Offset rotation
        1.0, 0.0, 0.0, 2.0, 0.0, 0.0, // Spine translation
    ];
    let (uri, len) = data_uri(&floats, &[]);
    let doc = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [
            { "name": "Armature", "translation": [0.0, 0.0, 5.0], "scale": [2.0, 2.0, 2.0], "children": [1] },
            { "name": "Hips", "translation": [0.0, 1.0, 0.0], "children": [2] },
            { "name": "Offset", "translation": [0.0, 0.0, 3.0], "children": [3] },
            { "name": "Spine", "translation": [1.0, 0.0, 0.0] },
        ],
        "skins": [{ "joints": [1, 3] }],
        "buffers": [{ "uri": uri, "byteLength": len }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 8 },
            { "buffer": 0, "byteOffset": 8, "byteLength": 8 },
            { "buffer": 0, "byteOffset": 16, "byteLength": 24 },
            { "buffer": 0, "byteOffset": 40, "byteLength": 32 },
            { "buffer": 0, "byteOffset": 72, "byteLength": 24 },
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [2.0] },
            { "bufferView": 1, "componentType": 5126, "count": 2, "type": "SCALAR", "min": [0.0], "max": [1.0] },
            { "bufferView": 2, "componentType": 5126, "count": 2, "type": "VEC3" },
            { "bufferView": 3, "componentType": 5126, "count": 2, "type": "VEC4" },
            { "bufferView": 4, "componentType": 5126, "count": 2, "type": "VEC3" },
        ],
        "animations": [{
            "name": "Lift",
            "samplers": [
                { "input": 0, "output": 2 },
                { "input": 1, "output": 3 },
                { "input": 1, "output": 4 },
            ],
            "channels": [
                { "sampler": 0, "target": { "node": 0, "path": "translation" } },
                { "sampler": 1, "target": { "node": 2, "path": "rotation" } },
                { "sampler": 2, "target": { "node": 3, "path": "translation" } },
            ],
        }],
    });
    let path = dir.join("rig.gltf");
    std::fs::write(&path, doc.to_string()).expect("write");
    let scene = load_scene(&path).expect("loads");
    let clip = scene.clips.iter().find(|c| c.name == "Lift").expect("clip");
    let pushes = clip_pushes(&scene);
    let lift = pushes.iter().find(|c| c.name == "Lift").expect("pushed");
    let joints = skeleton_joints(&scene);
    let track = |name: &str, property: u8| {
        lift.tracks
            .iter()
            .find(|t| t.target == name && t.property == property)
            .unwrap_or_else(|| panic!("no {name} track {property} in {:?}", lift.tracks))
    };
    // (joint index, name, property, expected key times)
    let cases = [
        (0usize, "Hips", 0u8, vec![0.0f32, 2.0]),
        (1, "Spine", 0, vec![0.0, 1.0]),
        (1, "Spine", 1, vec![0.0, 1.0]),
    ];
    for (index, name, property, times) in cases {
        let pushed = track(name, property);
        assert_eq!(pushed.times, times, "{name} {property}");
        let width = if property == 1 { 4 } else { 3 };
        for (k, t) in times.iter().enumerate() {
            let pose = clip
                .frames_at(&scene, *t, &mut FrameIds::default())
                .into_iter()
                .find_map(|m| match m {
                    Message::Pose(p) => Some(p),
                    _ => None,
                })
                .expect("a pose frame");
            let base = index * 10 + if property == 1 { 3 } else { 0 };
            let animated = &pose.values[base..base + width];
            let key = &pushed.values[k * width..(k + 1) * width];
            let same = close(key, animated)
                || (width == 4 && close(&key.iter().map(|x| -x).collect::<Vec<_>>(), animated));
            assert!(
                same,
                "{name} {property} at t = {t}: pushed {key:?}, --animate {animated:?}"
            );
        }
    }
    // Hips' first key is its folded bind (Armature at rest at t = 0).
    assert!(close(&track("Hips", 0).values[0..3], &joints[0].t));
    let _ = std::fs::remove_dir_all(&dir);
}
