// SPDX-License-Identifier: MPL-2.0
//! Headless tests of the ECS side (`specs/60` § 8, tests 4 to 11): a world, a fixed feed clock and
//! messages submitted the way a reader thread submits them. No socket and no wall clock.
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use regia_animation::{FeedPoseBuffer, FeedPoseTarget, SkinnedMesh};
use regia_app::prelude::*;
use regia_render::{Camera, DirectionalLight, MeshRenderer, PointLight, SpotLight};
use regia_scene::prelude::{EntityGuid, FeedAttach, FeedOwned, Transform, snapshot_scene};
use regia_scene::spawn::SnapshotFilter;
use serde_json::{Value, json};

use super::core::{FeedCore, ReplayOptions};
use super::net::lock;
use super::plugin::{DccFeedClock, DccFeedRegistry, DccFeedServer, FeedChannels, FeedItem, FeedMeshPart};
use super::recording::Recording;
use super::registry::ItemState;
use super::session::{Phase, SessionId, Transport};
use super::settings::{DccFeedSettings, Playout};
use super::wire::{
    self, CameraFrame, Elem, FeedKind, FrameInfo, Hello, ItemDeclare, JointStatic, Lens, LightFrame,
    MeshSection, MeshUpdate, Message, PoseFrame, SkeletonStatic, TexturePush, TextureSource, TransformBlock,
    TransformFrame, section,
};
use super::{DccFeedPlugin, item_guid, methods};

const STEP: f64 = 1.0 / 60.0;

fn settings() -> DccFeedSettings {
    DccFeedSettings {
        auto_accept: vec!["tool".to_owned()],
        ..DccFeedSettings::default()
    }
}

fn app_with(settings: DccFeedSettings, project: Option<std::path::PathBuf>) -> (App, Arc<Mutex<FeedCore>>) {
    let mut app = App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    app.add_plugins(DccFeedPlugin {
        project_dir: project.clone(),
    });
    let server = DccFeedServer::new(&settings, project);
    let core = server.core().clone();
    app.insert_resource(settings);
    app.insert_resource(server);
    app.insert_resource(DccFeedClock {
        fixed_step_s: Some(STEP),
        now_s: 0.0,
    });
    (app, core)
}

fn hello(client: &str) -> Message {
    Message::Hello(Hello {
        min_version: 1,
        max_version: 1,
        client_name: client.to_owned(),
        client_version: "1".to_owned(),
        dcc: "test".to_owned(),
        space: "regia".to_owned(),
        unit_scale: 1.0,
        caps: 0,
        token: String::new(),
    })
}

/// An active session in `regia` space, so transforms arrive untouched.
fn session(core: &Mutex<FeedCore>) -> SessionId {
    let mut c = lock(core);
    let s = c.open(Transport::Unix, "unix", None);
    c.submit(s, 1, 40, hello("tool"));
    s
}

fn submit(core: &Mutex<FeedCore>, s: SessionId, msg: Message) {
    lock(core).submit(s, 2, 64, msg);
}

fn declare(handle: u32, key: &str, kind: FeedKind, parent: u32, variant: u8) -> Message {
    Message::ItemDeclare(ItemDeclare {
        handle,
        key: key.to_owned(),
        kind,
        parent_handle: parent,
        display_name: key.to_owned(),
        variant,
        channels: Vec::new(),
    })
}

fn info(frame_id: u64) -> FrameInfo {
    FrameInfo {
        frame_id,
        source_time_ns: frame_id * 16_666_667,
        timecode: None,
    }
}

fn block(x: f32) -> TransformBlock {
    TransformBlock {
        translation: [x, 0.0, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0; 3],
        world_space: false,
    }
}

fn transform(handle: u32, frame_id: u64, x: f32) -> Message {
    Message::Transform(TransformFrame {
        handle,
        info: info(frame_id),
        xf: block(x),
    })
}

fn floats(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
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

fn slot_table(names: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    let string = |out: &mut Vec<u8>, t: &str| {
        out.extend_from_slice(&(t.len() as u16).to_le_bytes());
        out.extend_from_slice(t.as_bytes());
    };
    for name in names {
        string(&mut out, name);
        out.extend(floats(&[1.0, 1.0, 1.0, 1.0, 0.0, 0.5, 0.0, 0.0, 0.0]));
        for _ in 0..4 {
            string(&mut out, "");
        }
    }
    out
}

fn quad(handle: u32, frame_id: u64) -> Message {
    let positions = [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0, 0.0, 0.0];
    let indices: Vec<u8> = [0u32, 1, 2, 0, 2, 3]
        .iter()
        .flat_map(|i| i.to_le_bytes())
        .collect();
    let ranges: Vec<u8> = [0u32, 0, 3, 1, 3, 3]
        .iter()
        .flat_map(|i| i.to_le_bytes())
        .collect();
    Message::Mesh(MeshUpdate {
        handle,
        info: info(frame_id),
        update_mode: 0,
        sections: vec![
            sec(section::POSITIONS, Elem::F32, 4, floats(&positions)),
            sec(section::INDICES, Elem::U32, 6, indices),
            sec(section::SUBMESHES, Elem::U32, 2, ranges),
            sec(
                section::MATERIAL_SLOTS,
                Elem::U8,
                2,
                slot_table(&["Paint", "Trim"]),
            ),
        ],
    })
}

fn entity_of(app: &App, key: &str) -> Entity {
    app.world()
        .resource::<DccFeedRegistry>()
        .table
        .get(item_guid(key))
        .and_then(|i| i.entity)
        .unwrap_or_else(|| panic!("no entity for {key}"))
}

fn feed(app: &App, key: &str) -> FeedItem {
    app.world()
        .get::<FeedItem>(entity_of(app, key))
        .cloned()
        .unwrap_or_else(|| panic!("{key} has no FeedItem"))
}

fn errors(core: &Mutex<FeedCore>, s: SessionId) -> Vec<String> {
    lock(core)
        .take_outbox(s)
        .iter()
        .filter_map(|m| match m {
            Message::Error(e) => Some(e.code.clone()),
            _ => None,
        })
        .collect()
}

/// Spec § 8 test 4: one item of each kind maps to the expected components, ids and parents.
#[test]
fn every_kind_maps_onto_an_entity() {
    let (mut app, core) = app_with(settings(), None);
    let s = session(&core);
    submit(&core, s, declare(1, "Root", FeedKind::Transform, 0, 0));
    submit(&core, s, declare(2, "Cube", FeedKind::Mesh, 1, 0));
    submit(&core, s, declare(3, "Cam", FeedKind::Camera, 0, 0));
    submit(&core, s, declare(4, "Sun", FeedKind::Light, 0, 0));
    submit(&core, s, declare(5, "Lamp", FeedKind::Light, 0, 1));
    submit(&core, s, declare(6, "Spot", FeedKind::Light, 0, 2));
    submit(&core, s, declare(7, "Rig", FeedKind::Skeleton, 0, 0));
    submit(
        &core,
        s,
        Message::ItemDeclare(ItemDeclare {
            handle: 8,
            key: "Sliders".to_owned(),
            kind: FeedKind::Generic,
            parent_handle: 0,
            display_name: String::new(),
            variant: 0,
            channels: vec!["a".to_owned(), "b".to_owned()],
        }),
    );
    submit(&core, s, transform(1, 1, 3.0));
    submit(&core, s, quad(2, 1));
    submit(
        &core,
        s,
        Message::Camera(CameraFrame {
            handle: 3,
            info: info(1),
            xf: block(1.0),
            lens: Lens::Perspective {
                yfov_rad: 0.5,
                aspect: 0.0,
                near: 0.25,
                far: 0.0,
            },
            physical: None,
        }),
    );
    for (handle, intensity) in [(4, 800.0), (5, 10.0), (6, 20.0)] {
        submit(
            &core,
            s,
            Message::Light(LightFrame {
                handle,
                info: info(1),
                xf: block(0.0),
                color: [1.0, 0.5, 0.25],
                intensity,
                range: 0.0,
                spot_inner_rad: 0.25,
                spot_outer_rad: 0.5,
            }),
        );
    }
    submit(
        &core,
        s,
        Message::Pose(PoseFrame {
            handle: 8,
            info: info(1),
            layout: 1,
            count: 2,
            values: vec![0.25, 0.75],
        }),
    );
    app.update();
    let world = app.world();
    let root = entity_of(&app, "Root");
    assert_eq!(world.get::<Transform>(root).map(|t| t.translation.x), Some(3.0));
    assert_eq!(feed(&app, "Root").guid, item_guid("Root").as_u128(), "stable id");
    assert_eq!(
        world.get::<EntityGuid>(root).map(|g| g.0),
        Some(item_guid("Root"))
    );
    assert_eq!(feed(&app, "Root").frames_applied, 1);
    assert!(world.get::<FeedOwned>(root).is_some());
    let cube = entity_of(&app, "Cube");
    assert_eq!(
        world.get::<ChildOf>(cube).map(ChildOf::parent),
        Some(root),
        "parent_handle"
    );
    assert!(world.get::<MeshRenderer>(cube).is_some());
    let cam = world.get::<Camera>(entity_of(&app, "Cam")).expect("camera");
    assert_eq!((cam.fov_y, cam.near), (0.5, 0.25));
    assert!(cam.far.is_infinite(), "far 0 is infinite");
    let sun = world
        .get::<DirectionalLight>(entity_of(&app, "Sun"))
        .expect("sun");
    assert_eq!(sun.illuminance, 800.0);
    let lamp = world.get::<PointLight>(entity_of(&app, "Lamp")).expect("lamp");
    assert!(
        (lamp.intensity - 10.0 * 4.0 * std::f32::consts::PI).abs() < 1e-3,
        "candela to lumens"
    );
    let spot = world.get::<SpotLight>(entity_of(&app, "Spot")).expect("spot");
    assert_eq!(
        (spot.intensity, spot.inner_angle, spot.outer_angle),
        (20.0, 0.25, 0.5)
    );
    let channels = world
        .get::<FeedChannels>(entity_of(&app, "Sliders"))
        .expect("channels");
    assert_eq!(channels.values, vec![0.25, 0.75]);
    assert!(errors(&core, s).is_empty());
}

#[test]
fn a_glb_space_root_takes_the_forward_turn_and_children_do_not() {
    let (mut app, core) = app_with(settings(), None);
    let s = {
        let mut c = lock(&core);
        let s = c.open(Transport::Unix, "unix", None);
        let Message::Hello(mut h) = hello("tool") else {
            unreachable!("hello() builds a hello");
        };
        h.space = "gltf".to_owned();
        h.unit_scale = 2.0;
        c.submit(s, 1, 40, Message::Hello(h));
        s
    };
    submit(&core, s, declare(1, "Root", FeedKind::Transform, 0, 0));
    submit(&core, s, declare(2, "Child", FeedKind::Transform, 1, 0));
    submit(&core, s, transform(1, 1, 1.0));
    submit(&core, s, transform(2, 1, 1.0));
    app.update();
    let x = |key: &str| {
        app.world()
            .get::<Transform>(entity_of(&app, key))
            .map(|t| t.translation.x)
    };
    assert_eq!(x("Root"), Some(-2.0));
    assert_eq!(x("Child"), Some(2.0));
}

/// Spec § 8 test 5 and § 2.4: read-only entities, no save, `FeedAttach`, conflicts.
#[test]
fn ownership_rules_hold() {
    let (mut app, core) = app_with(settings(), None);
    let s = session(&core);
    submit(&core, s, declare(1, "Cube", FeedKind::Transform, 0, 0));
    app.update();
    let live = entity_of(&app, "Cube");
    let bits = live.to_bits();
    let world = app.world_mut();
    let reason = |e: crate::protocol::RpcError| e.data.expect("data")["reason"].clone();
    let params = json!({ "entity": bits, "component": "Transform", "path": "translation.x", "value": 9.0 });
    assert_eq!(
        reason(crate::scene::set_property(world, &params).expect_err("set")),
        json!("feed_owned")
    );
    let params = json!({ "entity": bits });
    assert_eq!(
        reason(crate::scene::despawn(world, &params).expect_err("despawn")),
        json!("feed_owned")
    );
    assert_eq!(
        reason(crate::scene::reparent(world, &params).expect_err("reparent")),
        json!("feed_owned")
    );
    let edits = json!({ "edits": [{ "op": "despawn", "entity": bits }] });
    assert!(
        crate::scene::apply_edits(world, &edits).is_err(),
        "apply_edits refuses before anything applies"
    );

    let child = world
        .spawn((
            EntityGuid(regia_core::id::Guid::from_u128(7)),
            Transform::default(),
        ))
        .id();
    let params = json!({ "entity": child.to_bits(), "parent": bits });
    crate::scene::reparent(world, &params).expect("adding a child stays allowed");

    let file = snapshot_scene(world, &SnapshotFilter::default()).expect("snapshot");
    assert_eq!(file.entities.len(), 1, "the live entity is not saved");
    assert_eq!(
        world.get::<FeedAttach>(child).map(|a| a.key.as_str()),
        Some("Cube")
    );

    // Another session may not take the key; the rest of its session goes on.
    let other = {
        let mut c = lock(&core);
        let id = c.open(Transport::Unix, "unix-2", None);
        c.submit(id, 1, 40, hello("tool"));
        id
    };
    lock(&core).take_outbox(other);
    submit(&core, other, declare(1, "Cube", FeedKind::Transform, 0, 0));
    submit(&core, other, declare(2, "Own", FeedKind::Transform, 0, 0));
    app.update();
    assert_eq!(errors(&core, other), vec!["item_conflict".to_owned()]);
    assert!(
        app.world()
            .resource::<DccFeedRegistry>()
            .table
            .get(item_guid("Own"))
            .is_some()
    );

    // A kind change on the key is refused.
    submit(&core, s, declare(1, "Cube", FeedKind::Camera, 0, 0));
    app.update();
    assert_eq!(errors(&core, s), vec!["kind_mismatch".to_owned()]);
}

/// Spec § 2.4 rule 3: a marker saved earlier finds its item when the key is declared again, and a
/// deleted item leaves its user children waiting at the root.
#[test]
fn feed_attach_rebinds_children_to_their_item() {
    let (mut app, core) = app_with(settings(), None);
    let waiting = app
        .world_mut()
        .spawn((
            Transform::from_translation(regia_core::math::Vec3::new(1.0, 2.0, 3.0)),
            FeedAttach {
                key: "Cube".to_owned(),
            },
        ))
        .id();
    let s = session(&core);
    submit(&core, s, declare(1, "Cube", FeedKind::Transform, 0, 0));
    app.update();
    let live = entity_of(&app, "Cube");
    assert_eq!(
        app.world().get::<ChildOf>(waiting).map(ChildOf::parent),
        Some(live)
    );
    assert_eq!(
        app.world().get::<Transform>(waiting).map(|t| t.translation.x),
        Some(1.0),
        "local transform preserved"
    );
    submit(&core, s, Message::ItemDelete(1));
    app.update();
    assert!(app.world().get_entity(live).is_err(), "the item entity is gone");
    assert!(app.world().get_entity(waiting).is_ok(), "the user entity is not");
    assert!(app.world().get::<ChildOf>(waiting).is_none());
    assert!(
        app.world().get::<FeedAttach>(waiting).is_some(),
        "still waiting for the key"
    );
}

/// Counts the "no DCC feed item" warnings that reach this thread's subscriber.
struct AttachWarnings(Arc<std::sync::atomic::AtomicUsize>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AttachWarnings {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        if message.0.contains("no DCC feed item") {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Spec § 2.4 rule 3: a saved child whose key has no item stays at the scene root, and the warning
/// is logged once per key, however many children wait for it and however many frames pass.
#[test]
fn a_missing_attach_key_warns_once_per_key() {
    use tracing_subscriber::layer::SubscriberExt;
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(AttachWarnings(count.clone()));
    tracing::subscriber::with_default(subscriber, || {
        let (mut app, _core) = app_with(settings(), None);
        // One thread runs every system, so their events reach this thread's subscriber.
        app.shuffle_schedules(1);
        let attach = |key: &str| FeedAttach { key: key.to_owned() };
        let first = app.world_mut().spawn(attach("Ghost")).id();
        app.world_mut().spawn(attach("Ghost"));
        app.world_mut().spawn(attach("Other"));
        app.update();
        app.update();
        app.world_mut().spawn(attach("Ghost"));
        app.update();
        assert!(app.world().get::<ChildOf>(first).is_none(), "stays at the root");
    });
    assert_eq!(
        count.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "one warning for Ghost, one for Other"
    );
}

/// Spec § 8 test 6 through the world: the 100th frame applies, 99 coalesce, credits match.
#[test]
fn a_hundred_frames_in_one_tick_apply_as_the_newest() {
    let (mut app, core) = app_with(settings(), None);
    let s = session(&core);
    submit(&core, s, declare(1, "A", FeedKind::Transform, 0, 0));
    for i in 0..100u32 {
        submit(&core, s, transform(1, u64::from(i) + 1, i as f32));
    }
    lock(&core).take_outbox(s);
    app.update();
    assert_eq!(
        app.world()
            .get::<Transform>(entity_of(&app, "A"))
            .map(|t| t.translation.x),
        Some(99.0)
    );
    assert_eq!(feed(&app, "A").last_frame_id, 100);
    assert_eq!(feed(&app, "A").frames_applied, 1);
    assert_eq!(lock(&core).counters(s).frames_coalesced, 99);
}

/// Spec § 8 test 7 at the entity: submesh ranges map to slots, slot names reach the item, a
/// vertex-only update with a wrong count is refused and leaves the mesh.
#[test]
fn meshes_build_split_and_refuse_a_wrong_vertex_count() {
    let (mut app, core) = app_with(settings(), None);
    let s = session(&core);
    submit(&core, s, declare(1, "Hull", FeedKind::Mesh, 0, 0));
    submit(&core, s, quad(1, 1));
    app.update();
    let world = app.world_mut();
    let mut parts = world.query::<(&FeedMeshPart, &MeshRenderer)>();
    let found: Vec<u32> = parts.iter(world).map(|(p, _)| p.slot).collect();
    assert_eq!(found, vec![1], "slot 0 sits on the item entity, slot 1 on a part");
    let registry = app.world().resource::<DccFeedRegistry>();
    let item = registry.table.get(item_guid("Hull")).expect("item");
    assert_eq!((item.detail.vertices, item.detail.triangles), (4, 2));
    let state = registry.meshes.get(&item_guid("Hull")).expect("mesh");
    let names: Vec<&str> = state.data.slots.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["Paint", "Trim"]);
    assert!(state.data.normals_generated);

    let wrong = Message::Mesh(MeshUpdate {
        handle: 1,
        info: info(2),
        update_mode: 1,
        sections: vec![sec(section::POSITIONS, Elem::F32, 3, floats(&[0.0; 9]))],
    });
    lock(&core).take_outbox(s);
    submit(&core, s, wrong);
    app.update();
    assert_eq!(errors(&core, s), vec!["topology_mismatch".to_owned()]);
    let registry = app.world().resource::<DccFeedRegistry>();
    assert_eq!(registry.meshes[&item_guid("Hull")].data.vertex_count(), 4);
}

fn decode_frame(msg: &Message) -> Result<Message, wire::WireError> {
    let mut frame = Vec::new();
    wire::encode(msg, 1, &mut frame).expect("encode");
    let header = wire::FrameHeader::parse(frame[..wire::HEADER_BYTES].try_into().expect("header"));
    wire::decode(header, &frame[wire::HEADER_BYTES..], &wire::Limits::default())
}

fn full_mesh(info_id: u64, sections: Vec<MeshSection>) -> Message {
    Message::Mesh(MeshUpdate {
        handle: 1,
        info: info(info_id),
        update_mode: 0,
        sections,
    })
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|i| i.to_le_bytes()).collect()
}

/// Audit: a repeated section would override the validated first one in `apply`.
#[test]
fn a_mesh_with_repeated_sections_is_refused_at_decode() {
    let positions = sec(section::POSITIONS, Elem::F32, 4, floats(&[0.0; 12]));
    let indices = sec(section::INDICES, Elem::U32, 3, u32s(&[0, 1, 2]));
    let ok = full_mesh(1, vec![positions.clone(), indices.clone()]);
    assert!(decode_frame(&ok).is_ok());
    let dup_positions = full_mesh(
        1,
        vec![
            positions.clone(),
            indices.clone(),
            sec(section::POSITIONS, Elem::F32, 1, floats(&[0.0; 3])),
        ],
    );
    assert!(decode_frame(&dup_positions).is_err());
    let dup_indices = full_mesh(
        1,
        vec![
            positions,
            indices,
            sec(section::INDICES, Elem::U32, 3, u32s(&[0, 9, 200])),
        ],
    );
    assert!(decode_frame(&dup_indices).is_err());
}

/// Audit item 7: a full update dropped as out of order must not change the vertex count that
/// later vertex-only updates are checked against.
#[test]
fn a_dropped_full_update_keeps_the_expected_vertex_count() {
    let (mut app, core) = app_with(settings(), None);
    let s = session(&core);
    submit(&core, s, declare(1, "Hull", FeedKind::Mesh, 0, 0));
    submit(&core, s, quad(1, 5));
    app.update();
    let three = full_mesh(
        2,
        vec![
            sec(section::POSITIONS, Elem::F32, 3, floats(&[0.0; 9])),
            sec(section::INDICES, Elem::U32, 3, u32s(&[0, 1, 2])),
        ],
    );
    submit(&core, s, three);
    app.update();
    lock(&core).take_outbox(s);
    submit(
        &core,
        s,
        Message::Mesh(MeshUpdate {
            handle: 1,
            info: info(6),
            update_mode: 1,
            sections: vec![sec(section::POSITIONS, Elem::F32, 4, floats(&[0.5; 12]))],
        }),
    );
    app.update();
    assert!(
        errors(&core, s).is_empty(),
        "four vertices still match the live mesh"
    );
}

fn skeleton(handle: u32, names: &[&str]) -> Message {
    Message::SkeletonStatic(SkeletonStatic {
        handle,
        joints: names
            .iter()
            .map(|n| JointStatic {
                name: (*n).to_owned(),
                parent: -1,
                t: [0.0; 3],
                r: [0.0, 0.0, 0.0, 1.0],
                s: [1.0; 3],
            })
            .collect(),
    })
}

fn pose(handle: u32, frame_id: u64, x: f32, joints: u16) -> Message {
    let mut values = Vec::new();
    for _ in 0..joints {
        values.extend([x, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
    }
    Message::Pose(PoseFrame {
        handle,
        info: info(frame_id),
        layout: 0,
        count: joints,
        values,
    })
}

/// Spec § 8 test 8, the feed half: pose frames reach the buffer under the item's key, and a count
/// that differs from the static one is refused.
#[test]
fn poses_reach_the_buffer_by_key() {
    let (mut app, core) = app_with(settings(), None);
    let s = session(&core);
    submit(&core, s, declare(1, "Rig", FeedKind::Skeleton, 0, 0));
    submit(&core, s, skeleton(1, &["root", "spine", "head"]));
    submit(&core, s, pose(1, 1, 2.0, 3));
    app.update();
    let buffer = app.world().resource::<FeedPoseBuffer>();
    let got = buffer.get("Rig").expect("pose");
    assert_eq!(got.joints.as_slice(), ["root", "spine", "head"]);
    assert_eq!(got.pose.translations[2].x, 2.0);
    lock(&core).take_outbox(s);
    submit(&core, s, pose(1, 2, 1.0, 2));
    app.update();
    assert_eq!(errors(&core, s), vec!["invalid_value".to_owned()]);
}

/// A mesh parented under a skeleton item binds to it: joint entities, a `SkinnedMesh`, and pose
/// frames moving the joints by name.
#[test]
fn a_skinned_mesh_binds_to_its_skeleton_item_whichever_arrives_first() {
    let (mut app, core) = app_with(settings(), None);
    app.add_plugins(regia_animation::AnimationPlugin);
    let id = session(&core);
    submit(&core, id, declare(1, "Rig", FeedKind::Skeleton, 0, 0));
    submit(&core, id, declare(2, "Body", FeedKind::Mesh, 1, 0));
    let Message::Mesh(mut mesh) = quad(2, 1) else {
        unreachable!("quad() builds a mesh");
    };
    let joints: Vec<u8> = [0u16, 1, 0, 0]
        .iter()
        .cycle()
        .take(16)
        .flat_map(|j| j.to_le_bytes())
        .collect();
    mesh.sections.push(sec(section::JOINTS, Elem::U16, 4, joints));
    mesh.sections.push(sec(
        section::WEIGHTS,
        Elem::F32,
        4,
        floats(&[0.5, 0.5, 0.0, 0.0].repeat(4)),
    ));
    submit(&core, id, Message::Mesh(mesh));
    app.update();
    let body = entity_of(&app, "Body");
    assert!(
        app.world().get::<SkinnedMesh>(body).is_none(),
        "rigid until the skeleton lands"
    );

    submit(&core, id, skeleton(1, &["root", "tip"]));
    app.update();
    let skinned = app.world().get::<SkinnedMesh>(body).expect("bound").clone();
    assert_eq!(skinned.joints.len(), 2);
    assert_eq!(
        app.world().get::<FeedPoseTarget>(body).map(|t| t.item.as_str()),
        Some("Rig")
    );

    submit(&core, id, pose(1, 1, 2.0, 2));
    app.update();
    let tip = app.world().get::<Transform>(skinned.joints[1]).expect("joint");
    assert_eq!(tip.translation.x, 2.0, "the pose moved the joint");
}

/// Spec § 8 test 9, through the world (the spec's numbers put the midpoint of frames 0 and 6 at
/// tick 6 with a delay of 3 ticks; ADR-1891). Rotation takes the slerp midpoint.
#[test]
fn delayed_playout_interpolates_behind_the_clock() {
    let mut s = settings();
    s.playout = Playout::Delayed;
    s.playout_delay_ms = 50; // three ticks at 60 Hz
    let (mut app, core) = app_with(s, None);
    let id = session(&core);
    submit(&core, id, declare(1, "A", FeedKind::Transform, 0, 0));
    let frame = |frame_id: u64, tick: u64, x: f32, angle: f32| {
        let half = angle * 0.5;
        Message::Transform(TransformFrame {
            handle: 1,
            info: FrameInfo {
                frame_id,
                source_time_ns: tick * 16_666_667,
                timecode: None,
            },
            xf: TransformBlock {
                translation: [x, 0.0, 0.0],
                rotation: [0.0, half.sin(), 0.0, half.cos()],
                scale: [1.0; 3],
                world_space: false,
            },
        })
    };
    submit(&core, id, frame(1, 0, 0.0, 0.0));
    submit(&core, id, frame(2, 6, 6.0, 1.0));
    // The first frame anchors the timeline to tick 1 (the first update): tick 0 of the source is
    // the clock's first step, so source tick 3 is due on update 7 with the delay of 3 ticks.
    for _ in 0..7 {
        app.update();
    }
    let t = *app
        .world()
        .get::<Transform>(entity_of(&app, "A"))
        .expect("transform");
    assert!((t.translation.x - 3.0).abs() < 1e-4, "{}", t.translation.x);
    let expected = regia_core::math::Quat::from_rotation_y(0.5);
    assert!(t.rotation.dot(expected).abs() > 1.0 - 1e-6, "{:?}", t.rotation);
}

/// A `goodbye` waiting to be applied between `DccFeedSet::Ingest` and `DccFeedSet::Apply`.
#[derive(Resource)]
struct GoodbyeMidTick {
    core: Arc<Mutex<FeedCore>>,
    session: SessionId,
    armed: bool,
}

/// Plays a reader thread that applies the `goodbye` after this tick's ingest and before its apply
/// and flush: the timing that loses the detach under load.
fn goodbye_mid_tick(mut pending: ResMut<GoodbyeMidTick>) {
    if std::mem::take(&mut pending.armed) {
        lock(&pending.core).submit(pending.session, 9, 16, Message::Goodbye("mid tick".to_owned()));
    }
}

/// ADR-8795: a `goodbye` applied between the tick and the flush still detaches the session's
/// items on the next tick; before, the flush cut the session unseen and the item stayed stalled.
#[test]
fn a_goodbye_between_ingest_and_apply_still_detaches_the_items() {
    let (mut app, core) = app_with(settings(), None);
    let id = session(&core);
    submit(&core, id, declare(1, "A", FeedKind::Transform, 0, 0));
    submit(&core, id, transform(1, 1, 1.0));
    app.update();
    assert_eq!(feed(&app, "A").state, ItemState::Active);
    app.insert_resource(GoodbyeMidTick {
        core: core.clone(),
        session: id,
        armed: true,
    });
    app.add_systems(
        PreUpdate,
        goodbye_mid_tick
            .after(super::DccFeedSet::Ingest)
            .before(super::DccFeedSet::Apply),
    );
    app.update();
    app.update();
    assert_eq!(feed(&app, "A").state, ItemState::Detached);
    assert_eq!(feed(&app, "A").session, None);
    assert_eq!(lock(&core).sessions.phase(id), Some(Phase::Closed));
}

/// Spec § 8 test 10: stall on fixed-step time, detach on close, rebind on the same entity.
#[test]
fn stall_detach_and_rebind_keep_the_entity() {
    let mut s = settings();
    s.stall_after_ms = 100;
    let (mut app, core) = app_with(s, None);
    let id = session(&core);
    submit(&core, id, declare(1, "A", FeedKind::Transform, 0, 0));
    submit(&core, id, transform(1, 1, 1.0));
    app.update();
    let entity = entity_of(&app, "A");
    assert_eq!(feed(&app, "A").state, ItemState::Active);
    for _ in 0..7 {
        app.update();
    }
    assert_eq!(feed(&app, "A").state, ItemState::Stalled);
    submit(&core, id, Message::Goodbye("done".to_owned()));
    app.update();
    assert_eq!(feed(&app, "A").state, ItemState::Detached);
    assert_eq!(feed(&app, "A").session, None);
    app.update();
    let next = {
        let mut c = lock(&core);
        let n = c.open(Transport::Unix, "unix-2", None);
        c.submit(n, 1, 40, hello("tool"));
        n
    };
    submit(&core, next, declare(9, "A", FeedKind::Transform, 0, 0));
    submit(&core, next, transform(9, 1, 5.0));
    app.update();
    assert_eq!(entity_of(&app, "A"), entity, "same entity");
    assert_eq!(feed(&app, "A").state, ItemState::Active);
    assert_eq!(feed(&app, "A").session, Some(next));
    assert_eq!(
        app.world().get::<Transform>(entity).map(|t| t.translation.x),
        Some(5.0)
    );
}

/// Key, translation, rotation, scale and frames applied of one item.
type ItemSnapshot = (String, [f32; 3], [f32; 4], [f32; 3], u64);

fn state_of(app: &App) -> Vec<ItemSnapshot> {
    let registry = app.world().resource::<DccFeedRegistry>();
    registry
        .table
        .iter()
        .map(|item| {
            let t = item
                .entity
                .and_then(|e| app.world().get::<Transform>(e))
                .copied()
                .unwrap_or_default();
            (
                item.key.clone(),
                t.translation.to_array(),
                t.rotation.to_array(),
                t.scale.to_array(),
                item.frames_applied,
            )
        })
        .collect()
}

fn take(ticks: u32) -> Recording {
    let mut rec = Recording::new("tool", "test");
    let mut push = |tick: u64, msg: &Message, seq: u32| {
        let mut frame = Vec::new();
        wire::encode(msg, seq, &mut frame).expect("encode");
        rec.push(tick, frame);
    };
    push(0, &declare(1, "A", FeedKind::Transform, 0, 0), 1);
    push(0, &declare(2, "Hull", FeedKind::Mesh, 0, 0), 2);
    push(0, &quad(2, 1), 3);
    for tick in 1..u64::from(ticks) {
        push(tick, &transform(1, tick, tick as f32 * 0.125), 3 + tick as u32);
    }
    rec
}

/// Spec § 8 test 11 and § 7: the same recording gives bit-equal state at every tick of two runs.
#[test]
fn replay_is_deterministic_tick_by_tick() {
    let run = || {
        let (mut app, core) = app_with(settings(), None);
        lock(&core).start_replay_recording(
            std::path::Path::new("mem.rfdrec"),
            take(20),
            &ReplayOptions::default(),
        );
        let mut states = Vec::new();
        for _ in 0..30 {
            app.update();
            states.push(state_of(&app));
        }
        states
    };
    let (a, b) = (run(), run());
    assert_eq!(a, b);
    assert_eq!(
        a[19].iter().find(|i| i.0 == "A").map(|i| i.4),
        Some(19),
        "frames applied"
    );
    assert_eq!(
        a.last()
            .and_then(|s| s.iter().find(|i| i.0 == "Hull"))
            .map(|i| i.4),
        Some(1)
    );
}

/// Spec § 8 test 11, the shuffle half (`docs/04-conventions.md` § 12): the replay scene under
/// seeded orders of the unconstrained systems gives the unshuffled run's state at every tick.
#[test]
fn replay_state_does_not_depend_on_system_order() {
    let run = |seed: Option<u64>| {
        let (mut app, core) = app_with(settings(), None);
        if let Some(seed) = seed {
            app.shuffle_schedules(seed);
        }
        lock(&core).start_replay_recording(
            std::path::Path::new("mem.rfdrec"),
            take(20),
            &ReplayOptions::default(),
        );
        let mut states = Vec::new();
        for _ in 0..30 {
            app.update();
            states.push(state_of(&app));
        }
        states
    };
    let reference = run(None);
    assert!(
        reference.last().is_some_and(|s| s.iter().any(|i| i.0 == "Hull")),
        "the take reached its mesh item"
    );
    for seed in 1..=4 {
        assert_eq!(run(Some(seed)), reference, "seed {seed}");
    }
}

fn call(result: Result<Value, crate::protocol::RpcError>) -> Value {
    result.unwrap_or_else(|e| panic!("{}", e.message))
}

fn refused_reason(result: Result<Value, crate::protocol::RpcError>) -> Value {
    result.expect_err("refused").data.expect("data")["reason"].clone()
}

/// Spec § 8 test 13: the methods work on a headless world, and refusals carry the listed codes.
#[test]
fn agent_methods_report_and_control_sessions() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_methods");
    let (mut app, core) = app_with(DccFeedSettings::default(), Some(dir.clone()));
    app.insert_resource(crate::AgentSettings {
        project_dir: Some(dir.clone()),
    });
    let s = {
        let mut c = lock(&core);
        let id = c.open(Transport::Unix, "unix", None);
        c.submit(id, 1, 40, hello("addon"));
        id
    };
    submit(&core, s, declare(1, "A", FeedKind::Transform, 0, 0));
    submit(&core, s, transform(1, 1, 1.0));
    app.update();
    let world = app.world_mut();
    let status = call(methods::status(world, &Value::Null));
    assert_eq!(status["sessions"][0]["state"], json!("pending"));
    assert_eq!(status["sessions"][0]["dropped_not_accepted"], json!(1));
    assert_eq!(
        refused_reason(methods::accept(world, &json!({ "session": 99 }))),
        json!("session_not_found")
    );

    let accepted = call(methods::accept(world, &json!({ "session": s, "remember": true })));
    assert_eq!(accepted["resync_sent"], json!(1));
    assert_eq!(
        super::settings::remembered_clients(&dir),
        vec!["addon".to_owned()]
    );
    assert_eq!(
        refused_reason(methods::accept(world, &json!({ "session": s }))),
        json!("already_accepted")
    );
    app.update();
    let world = app.world_mut();
    let items = call(methods::items(
        world,
        &json!({ "key_glob": "A*", "kind": "transform" }),
    ));
    assert_eq!(items["items"][0]["key"], json!("A"));
    assert_eq!(items["items"][0]["state"], json!("declared"));
    assert_eq!(
        call(methods::items(world, &json!({ "kind": "mesh" })))["items"],
        json!([])
    );
    assert_eq!(
        refused_reason(methods::items(world, &json!({ "session": 99 }))),
        json!("session_not_found")
    );

    let record = json!({ "action": "start", "path": "takes/one.rfdrec" });
    std::fs::create_dir_all(dir.join("takes")).expect("dir");
    call(methods::record(world, &record));
    assert_eq!(
        refused_reason(methods::record(world, &record)),
        json!("record_active")
    );
    let denied = json!({ "action": "start", "path": "../outside.rfdrec" });
    assert_eq!(
        refused_reason(methods::record(world, &denied)),
        json!("path_denied")
    );
    let stopped = call(methods::record(world, &json!({ "action": "stop" })));
    assert_eq!(stopped["recording"], json!(false));
    assert_eq!(
        refused_reason(methods::replay(world, &json!({ "path": "takes/none.rfdrec" }))),
        json!("replay_not_found")
    );
    std::fs::write(dir.join("takes/bad.rfdrec"), b"not a recording").expect("write");
    assert_eq!(
        refused_reason(methods::replay(world, &json!({ "path": "takes/bad.rfdrec" }))),
        json!("replay_corrupt")
    );
    assert_eq!(
        refused_reason(methods::replay(world, &json!({ "path": "../x.rfdrec" }))),
        json!("path_denied")
    );

    let closed = call(methods::disconnect(
        world,
        &json!({ "session": s, "despawn": true }),
    ));
    assert_eq!(
        closed,
        json!({ "closed": true, "items_detached": 1, "despawned": 1 })
    );
    assert_eq!(
        refused_reason(methods::disconnect(world, &json!({ "session": 99 }))),
        json!("session_not_found")
    );
    assert!(world.resource::<DccFeedRegistry>().table.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn accepting_a_replay_as_an_unknown_client_applies_it() {
    let (mut app, core) = app_with(DccFeedSettings::default(), None);
    let opts = ReplayOptions {
        as_client: Some("stranger".to_owned()),
        ticks_per_record: Some(1),
        ..ReplayOptions::default()
    };
    let (s, total) = lock(&core).start_replay_recording(std::path::Path::new("mem.rfdrec"), take(4), &opts);
    assert_eq!(total, 6);
    assert_eq!(lock(&core).sessions.phase(s), Some(Phase::Pending));
    app.update();
    call(methods::accept(app.world_mut(), &json!({ "session": s })));
    app.update();
    assert!(
        app.world()
            .resource::<DccFeedRegistry>()
            .table
            .get(item_guid("A"))
            .is_some()
    );
}

#[test]
fn texture_paths_are_refused_outside_the_project_and_read_inside() {
    let dir = regia_test_utils::unique_temp_dir("dcc_feed_texture");
    let (mut app, core) = app_with(settings(), Some(dir.clone()));
    let s = session(&core);
    let push = |path: &str| {
        Message::TexturePush(TexturePush {
            name: "t".to_owned(),
            colorspace: 0,
            source: TextureSource::Path(path.to_owned()),
        })
    };
    lock(&core).take_outbox(s);
    submit(&core, s, push("../../etc/passwd"));
    app.update();
    assert_eq!(errors(&core, s), vec!["path_denied".to_owned()]);
    std::fs::write(dir.join("t.png"), b"not really a png").expect("write");
    submit(&core, s, push("t.png"));
    app.update();
    assert!(errors(&core, s).is_empty());
    let registry = app.world().resource::<DccFeedRegistry>();
    let texture = registry.textures.get("t").expect("stored");
    assert!(texture.decoded.is_none(), "an undecodable file stays undecoded");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_idle_server_never_takes_the_lock() {
    let (mut app, core) = app_with(settings(), None);
    app.update();
    assert_eq!(lock(&core).tick, 0, "no session, no tick");
    assert!(!app.world().resource::<DccFeedServer>().is_live());
    session(&core);
    app.update();
    assert_eq!(lock(&core).tick, 1);
}
