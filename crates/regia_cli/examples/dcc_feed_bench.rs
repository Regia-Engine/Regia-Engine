// SPDX-License-Identifier: MPL-2.0
//! Prints the numbers of `docs/specs/60-dcc-scene-feed.md` § 10, on this machine, in this build:
//! `cargo run --release -p regia_cli --example dcc_feed_bench`. Timing claims are benchmarks that
//! report (spec § 8), so nothing here fails on a budget. Rows the spec names but this tool cannot
//! reach (the 3-frame end to end needs a GPU) are left to `book/agents/dcc-feed.md`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::path::Path;
use std::time::Instant;

use regia_agent::dcc_feed::core::FeedCore;
use regia_agent::dcc_feed::mesh::FeedMeshData;
use regia_agent::dcc_feed::net::lock;
use regia_agent::dcc_feed::session::{SessionId, Transport};
use regia_agent::dcc_feed::settings::DccFeedSettings;
use regia_agent::dcc_feed::wire::{
    self, Elem, FeedKind, FrameHeader, FrameInfo, HEADER_BYTES, Hello, ItemDeclare, JointStatic, Limits,
    MeshSection, MeshUpdate, Message, PoseFrame, SkeletonStatic, TransformBlock, TransformFrame, section,
};
use regia_agent::dcc_feed::{DccFeedClock, DccFeedPlugin, DccFeedRegistry, DccFeedServer, item_guid};
use regia_app::prelude::*;
use regia_asset_pipeline::dcc_feed_bake::{
    BakeRequest, FeedSnapshot, SnapshotItem, SnapshotMesh, SnapshotSlot, plan_bake, write_glb,
};

const VERTICES: u32 = 100_000;
const RUNS: usize = 21;

fn floats(values: impl Iterator<Item = f32>) -> Vec<u8> {
    values.flat_map(f32::to_le_bytes).collect()
}

fn sec(section_id: u8, elem: Elem, count: u32, data: Vec<u8>) -> MeshSection {
    MeshSection {
        section_id,
        elem,
        index: 0,
        count,
        data,
    }
}

fn info(frame_id: u64) -> FrameInfo {
    FrameInfo {
        frame_id,
        source_time_ns: 0,
        timecode: None,
    }
}

/// Spec § 10's mesh: positions, normals, uv0 and 300k indices, about 4.4 MB.
fn sections(with_normals: bool) -> Vec<MeshSection> {
    let n = VERTICES;
    let wave = |i: u32| ((i % 317) as f32 * 0.1).sin();
    let positions = (0..n).flat_map(|i| [(i % 317) as f32, wave(i), (i / 317) as f32]);
    let normals = (0..n).flat_map(|_| [0.0, 1.0, 0.0]);
    let uv = (0..n).flat_map(|i| [(i % 317) as f32 / 317.0, (i / 317) as f32 / 316.0]);
    let indices: Vec<u8> = (0..n)
        .flat_map(|i| [i, (i + 1) % n, (i + 317) % n])
        .flat_map(u32::to_le_bytes)
        .collect();
    let mut out = vec![sec(section::POSITIONS, Elem::F32, n, floats(positions))];
    if with_normals {
        out.push(sec(section::NORMALS, Elem::F32, n, floats(normals)));
    }
    out.push(sec(section::UV, Elem::F32, n, floats(uv)));
    out.push(sec(section::INDICES, Elem::U32, n * 3, indices));
    out
}

fn mesh_update(handle: u32, frame_id: u64, with_normals: bool) -> MeshUpdate {
    MeshUpdate {
        handle,
        info: info(frame_id),
        update_mode: 0,
        sections: sections(with_normals),
    }
}

/// Median milliseconds of `RUNS` calls.
fn median_ms(mut run: impl FnMut()) -> f64 {
    let mut samples: Vec<f64> = (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            run();
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    samples[RUNS / 2]
}

/// A headless world on a fixed clock with one accepted session.
fn world() -> (App, std::sync::Arc<std::sync::Mutex<FeedCore>>, SessionId) {
    let settings = DccFeedSettings {
        auto_accept: vec!["bench".to_owned()],
        ..DccFeedSettings::default()
    };
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.add_plugins(DccFeedPlugin::default());
    let server = DccFeedServer::new(&settings, None);
    let core = server.core().clone();
    app.insert_resource(settings);
    app.insert_resource(server);
    app.insert_resource(DccFeedClock {
        fixed_step_s: Some(1.0 / 60.0),
        now_s: 0.0,
    });
    let session = {
        let mut c = lock(&core);
        let s = c.open(Transport::Unix, "unix", None);
        c.submit(
            s,
            1,
            40,
            Message::Hello(Hello {
                min_version: 1,
                max_version: 1,
                client_name: "bench".to_owned(),
                client_version: "1".to_owned(),
                dcc: "bench".to_owned(),
                space: "regia".to_owned(),
                unit_scale: 1.0,
                caps: 0,
                token: String::new(),
            }),
        );
        s
    };
    (app, core, session)
}

fn declare(core: &std::sync::Mutex<FeedCore>, s: SessionId, handle: u32, kind: FeedKind) {
    lock(core).submit(
        s,
        2,
        40,
        Message::ItemDeclare(ItemDeclare {
            handle,
            key: format!("item_{handle}"),
            kind,
            parent_handle: 0,
            display_name: String::new(),
            variant: 0,
            channels: Vec::new(),
        }),
    );
}

fn frame_of(msg: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    wire::encode(msg, 1, &mut out).expect("encode");
    out
}

fn decoding(limits: &Limits) {
    let frame = frame_of(&Message::Mesh(mesh_update(1, 1, true)));
    let head = frame.first_chunk::<HEADER_BYTES>().expect("header");
    let header = FrameHeader::parse(head);
    let payload = &frame[HEADER_BYTES..];
    let ms = median_ms(|| {
        wire::decode(header, payload, limits).expect("decode");
    });
    println!(
        "decode + validate, {:.1} MB mesh: {ms:.2} ms ({:.0} MB/s; budget 4 ms, 150 MB/s)",
        frame.len() as f64 / 1e6,
        frame.len() as f64 / 1e6 / (ms / 1e3)
    );
}

fn building() {
    let sent = mesh_update(1, 1, true);
    let ms = median_ms(|| {
        FeedMeshData::default().apply(&sent, 1.0).expect("build");
    });
    println!("mesh build with sent normals: {ms:.2} ms (budget 12 ms)");
    let sent = mesh_update(1, 1, false);
    let generated = median_ms(|| {
        FeedMeshData::default().apply(&sent, 1.0).expect("build");
    });
    println!("mesh build generating normals: {generated:.2} ms");
}

fn transforms() {
    let (mut app, core, s) = world();
    for handle in 1..=200 {
        declare(&core, s, handle, FeedKind::Transform);
    }
    app.world_mut().run_schedule(PreUpdate);
    let mut frame_id = 0;
    let ms = median_ms(|| {
        frame_id += 1;
        let mut c = lock(&core);
        for handle in 1..=200 {
            c.submit(
                s,
                3,
                64,
                Message::Transform(TransformFrame {
                    handle,
                    info: info(frame_id),
                    xf: TransformBlock {
                        translation: [frame_id as f32, 0.0, 0.0],
                        rotation: [0.0, 0.0, 0.0, 1.0],
                        scale: [1.0; 3],
                        world_space: false,
                    },
                }),
            );
        }
        c.take_outbox(s);
        drop(c);
        app.world_mut().run_schedule(PreUpdate);
    });
    println!("ingest + apply, 200 transforms per tick: {ms:.3} ms (budget 0.2 ms)");
}

fn poses() {
    let (mut app, core, s) = world();
    declare(&core, s, 1, FeedKind::Skeleton);
    let joints: Vec<JointStatic> = (0..100)
        .map(|i| JointStatic {
            name: format!("joint_{i}"),
            parent: i - 1,
            t: [0.0, 1.0, 0.0],
            r: [0.0, 0.0, 0.0, 1.0],
            s: [1.0; 3],
        })
        .collect();
    lock(&core).submit(
        s,
        3,
        4096,
        Message::SkeletonStatic(SkeletonStatic { handle: 1, joints }),
    );
    app.world_mut().run_schedule(PreUpdate);
    let mut frame_id = 0;
    let ms = median_ms(|| {
        frame_id += 1;
        let mut values = Vec::with_capacity(1000);
        for _ in 0..100 {
            values.extend([0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
        }
        let mut c = lock(&core);
        c.submit(
            s,
            4,
            4096,
            Message::Pose(PoseFrame {
                handle: 1,
                info: info(frame_id),
                layout: 0,
                count: 100,
                values,
            }),
        );
        c.take_outbox(s);
        drop(c);
        app.world_mut().run_schedule(PreUpdate);
    });
    println!("pose frame, 100 joints: {ms:.3} ms (budget 0.05 ms)");
}

/// The live path: the build runs on the pool, so the tick that picks it up holds only the swap.
fn swapping() {
    let (mut app, core, s) = world();
    app.insert_resource(DccFeedClock::default());
    declare(&core, s, 1, FeedKind::Mesh);
    app.world_mut().run_schedule(PreUpdate);
    let guid = item_guid("item_1");
    let current = |app: &App| {
        let meshes = &app.world().resource::<DccFeedRegistry>().meshes;
        meshes.get(&guid).map(|m| std::sync::Arc::as_ptr(&m.data))
    };
    let update = mesh_update(1, 0, true);
    let mut samples = Vec::new();
    for run in 1..=RUNS as u64 {
        let before = current(&app);
        let mut m = update.clone();
        m.info = info(run);
        let mut c = lock(&core);
        c.submit(s, 5, 4096, Message::Mesh(m));
        c.take_outbox(s);
        drop(c);
        app.world_mut().run_schedule(PreUpdate);
        while current(&app) == before {
            std::thread::yield_now();
            let start = Instant::now();
            app.world_mut().run_schedule(PreUpdate);
            let ms = start.elapsed().as_secs_f64() * 1e3;
            if current(&app) != before {
                samples.push(ms);
            }
        }
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "main-thread swap: {:.3} ms (budget 1 ms)",
        samples[samples.len() / 2]
    );
}

fn baking(dir: &Path) {
    let data = {
        let mut data = FeedMeshData::default();
        data.apply(&mesh_update(1, 1, true), 1.0).expect("build");
        data
    };
    let snapshot = FeedSnapshot {
        items: vec![SnapshotItem {
            key: "Bench".to_owned(),
            guid: item_guid("Bench"),
            kind: "mesh".to_owned(),
            mesh: Some(SnapshotMesh {
                positions: data.positions.clone(),
                normals: data.normals.clone(),
                uv0: data.uv0.clone(),
                indices: data.indices.clone(),
                submeshes: data.submeshes.clone(),
                slots: vec![SnapshotSlot {
                    name: "Default".to_owned(),
                    base_color: [1.0; 4],
                    ..SnapshotSlot::default()
                }],
                ..SnapshotMesh::default()
            }),
        }],
        ..FeedSnapshot::default()
    };
    let request = BakeRequest {
        items: vec![item_guid("Bench")],
        folder: "live".to_owned(),
        ..BakeRequest::default()
    };
    let target = dir.join("assets/live/Bench.glb");
    std::fs::create_dir_all(dir.join("assets/live")).expect("assets");
    let start = Instant::now();
    let plan = plan_bake(&snapshot, &request).expect("plan");
    let mut bytes = Vec::new();
    write_glb(&plan.files[0], &mut bytes).expect("write");
    std::fs::write(&target, &bytes).expect("file");
    let written = start.elapsed().as_secs_f64();
    let options = regia_asset_pipeline::CookOptions::for_host(dir.to_path_buf());
    let report = regia_asset_pipeline::cook_project(options).expect("cook");
    let total = start.elapsed().as_secs_f64();
    println!(
        "bake: {written:.3} s to write, {total:.3} s with the import, {} errors (budget 1.5 s)",
        report.errors.len()
    );
}

fn main() {
    let dir = std::env::temp_dir().join(format!("dcc_feed_bench_{}", std::process::id()));
    let limits = Limits::default();
    decoding(&limits);
    building();
    transforms();
    poses();
    swapping();
    baking(&dir);
    let _ = std::fs::remove_dir_all(&dir);
}
