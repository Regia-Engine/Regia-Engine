// SPDX-License-Identifier: MPL-2.0
//! Spec 60 conformance: § 8 test 1's stored byte vectors for every wire message, and pins of the
//! numbers and access classes the spec names.
//!
//! The files under `tests/data/dcc_feed/` were laid out by hand from the byte tables of
//! `docs/formats/dcc-feed-protocol-v1.md`, one annotated line per field, without the Rust codec.
//! Each must decode to the message written out below and encode back to the same bytes, so the
//! codec and the published tables cannot drift apart unnoticed.
#![cfg(feature = "dcc-feed")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use regia_agent::dcc_feed::wire::*;

/// The bytes of an annotated vector: hex pairs, `#` starts a comment.
fn parse_hex(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for line in text.lines() {
        let data = line.split('#').next().unwrap_or("");
        for word in data.split_whitespace() {
            assert!(word.len().is_multiple_of(2), "odd hex word {word:?}");
            for i in (0..word.len()).step_by(2) {
                out.push(u8::from_str_radix(&word[i..i + 2], 16).expect("hex"));
            }
        }
    }
    out
}

fn info(frame_id: u64, source_time_ns: u64) -> FrameInfo {
    FrameInfo {
        frame_id,
        source_time_ns,
        timecode: None,
    }
}

fn xf(translation: [f32; 3], world_space: bool) -> TransformBlock {
    TransformBlock {
        translation,
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0, 1.0, 1.0],
        world_space,
    }
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Every stored vector and the message it holds.
fn vectors() -> Vec<(&'static str, &'static str, Message)> {
    vec![
        (
            "hello",
            include_str!("data/dcc_feed/hello.hex"),
            Message::Hello(Hello {
                min_version: 1,
                max_version: 1,
                client_name: "blend_feed".into(),
                client_version: "0.3".into(),
                dcc: "Blender 4.2".into(),
                space: "gltf".into(),
                unit_scale: 1.0,
                caps: 0x1f,
                token: String::new(),
            }),
        ),
        (
            "hello_ack",
            include_str!("data/dcc_feed/hello_ack.hex"),
            Message::HelloAck(HelloAck {
                version: 1,
                session_id: 7,
                accepted: true,
                caps: 0x1f,
                limits: Limits::default(),
                window_bytes: 32 << 20,
                server_name: "regia".into(),
            }),
        ),
        (
            "item_declare",
            include_str!("data/dcc_feed/item_declare.hex"),
            Message::ItemDeclare(ItemDeclare {
                handle: 2,
                key: "cube".into(),
                kind: FeedKind::Mesh,
                parent_handle: 1,
                display_name: "Cube".into(),
                variant: 0,
                channels: Vec::new(),
            }),
        ),
        (
            "item_declare_generic",
            include_str!("data/dcc_feed/item_declare_generic.hex"),
            Message::ItemDeclare(ItemDeclare {
                handle: 3,
                key: "rig_ch".into(),
                kind: FeedKind::Generic,
                parent_handle: 0,
                display_name: String::new(),
                variant: 0,
                channels: vec!["a".into(), "b".into()],
            }),
        ),
        (
            "item_delete",
            include_str!("data/dcc_feed/item_delete.hex"),
            Message::ItemDelete(2),
        ),
        (
            "transform",
            include_str!("data/dcc_feed/transform.hex"),
            Message::Transform(TransformFrame {
                handle: 1,
                info: info(9, 1000),
                xf: xf([1.0, 2.0, 3.0], true),
            }),
        ),
        (
            "camera",
            include_str!("data/dcc_feed/camera.hex"),
            Message::Camera(CameraFrame {
                handle: 4,
                info: FrameInfo {
                    frame_id: 1,
                    source_time_ns: 10,
                    timecode: Some(Timecode {
                        hh: 1,
                        mm: 2,
                        ss: 3,
                        ff: 4,
                        rate_num: 24,
                        rate_den: 1,
                        drop_frame: false,
                    }),
                },
                xf: xf([0.0, 1.0, 5.0], false),
                lens: Lens::Perspective {
                    yfov_rad: 0.5,
                    aspect: 0.0,
                    near: 0.25,
                    far: 0.0,
                },
                physical: Some(PhysicalCamera {
                    focal_mm: 35.0,
                    sensor_w_mm: 36.0,
                    sensor_h_mm: 24.0,
                    focus_m: 2.0,
                    f_stop: 2.5,
                }),
            }),
        ),
        (
            "camera_ortho",
            include_str!("data/dcc_feed/camera_ortho.hex"),
            Message::Camera(CameraFrame {
                handle: 4,
                info: info(2, 20),
                xf: xf([0.0, 0.0, 0.0], false),
                lens: Lens::Orthographic {
                    xmag: 2.0,
                    ymag: 1.5,
                    near: 0.5,
                    far: 100.0,
                },
                physical: None,
            }),
        ),
        (
            "light",
            include_str!("data/dcc_feed/light.hex"),
            Message::Light(LightFrame {
                handle: 5,
                info: info(2, 20),
                xf: xf([1.0, 2.0, 3.0], true),
                color: [1.0, 0.5, 0.25],
                intensity: 800.0,
                range: 10.0,
                spot_inner_rad: 0.25,
                spot_outer_rad: 0.5,
            }),
        ),
        (
            "mesh",
            include_str!("data/dcc_feed/mesh.hex"),
            Message::Mesh(MeshUpdate {
                handle: 2,
                info: info(3, 30),
                update_mode: 0,
                sections: vec![
                    MeshSection {
                        section_id: 1,
                        elem: Elem::F32,
                        index: 0,
                        count: 3,
                        data: f32_bytes(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
                    },
                    MeshSection {
                        section_id: 6,
                        elem: Elem::U16,
                        index: 0,
                        count: 3,
                        data: [0u16, 1, 2].iter().flat_map(|v| v.to_le_bytes()).collect(),
                    },
                ],
            }),
        ),
        (
            "skeleton_static",
            include_str!("data/dcc_feed/skeleton_static.hex"),
            Message::SkeletonStatic(SkeletonStatic {
                handle: 6,
                joints: vec![
                    JointStatic {
                        name: "root".into(),
                        parent: -1,
                        t: [0.0, 0.0, 0.0],
                        r: [0.0, 0.0, 0.0, 1.0],
                        s: [1.0, 1.0, 1.0],
                    },
                    JointStatic {
                        name: "arm".into(),
                        parent: 0,
                        t: [0.0, 1.0, 0.0],
                        r: [0.0, 0.0, 0.0, 1.0],
                        s: [1.0, 1.0, 1.0],
                    },
                ],
            }),
        ),
        (
            "pose",
            include_str!("data/dcc_feed/pose.hex"),
            Message::Pose(PoseFrame {
                handle: 6,
                info: info(4, 40),
                layout: 0,
                count: 2,
                values: vec![
                    0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, //
                    0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0,
                ],
            }),
        ),
        (
            "pose_scalar",
            include_str!("data/dcc_feed/pose_scalar.hex"),
            Message::Pose(PoseFrame {
                handle: 3,
                info: info(5, 50),
                layout: 1,
                count: 2,
                values: vec![0.5, 1.0],
            }),
        ),
        (
            "clip_push",
            include_str!("data/dcc_feed/clip_push.hex"),
            Message::ClipPush(ClipPush {
                name: "wave".into(),
                fps: 30.0,
                duration_s: 1.0,
                looping: true,
                bind_handle: 0,
                tracks: vec![ClipTrack {
                    target: "cube".into(),
                    property: 0,
                    interp: 1,
                    times: vec![0.0, 1.0],
                    values: vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
                }],
            }),
        ),
        (
            "texture_push_path",
            include_str!("data/dcc_feed/texture_push_path.hex"),
            Message::TexturePush(TexturePush {
                name: "brick".into(),
                colorspace: 1,
                source: TextureSource::Path("textures/brick.png".into()),
            }),
        ),
        (
            "texture_push_encoded",
            include_str!("data/dcc_feed/texture_push_encoded.hex"),
            Message::TexturePush(TexturePush {
                name: "tiny".into(),
                colorspace: 2,
                source: TextureSource::Encoded {
                    container: 0,
                    bytes: b"\x89PNG".to_vec(),
                },
            }),
        ),
        (
            "texture_push_raw",
            include_str!("data/dcc_feed/texture_push_raw.hex"),
            Message::TexturePush(TexturePush {
                name: "white".into(),
                colorspace: 0,
                source: TextureSource::Raw {
                    pixel_format: 0,
                    width: 1,
                    height: 1,
                    bytes: vec![0xff; 4],
                },
            }),
        ),
        (
            "ping",
            include_str!("data/dcc_feed/ping.hex"),
            Message::Ping(Ping {
                token: 42,
                sender_time_ns: 1000,
                responder_time_ns: 0,
            }),
        ),
        (
            "pong",
            include_str!("data/dcc_feed/pong.hex"),
            Message::Pong(Ping {
                token: 42,
                sender_time_ns: 1000,
                responder_time_ns: 2000,
            }),
        ),
        (
            "ack",
            include_str!("data/dcc_feed/ack.hex"),
            Message::Ack(Ack {
                seq_applied: 5,
                credit_bytes: 74,
            }),
        ),
        (
            "session_state",
            include_str!("data/dcc_feed/session_state.hex"),
            Message::SessionState(SessionState {
                state: 2,
                reason: "accepted".into(),
            }),
        ),
        (
            "resync",
            include_str!("data/dcc_feed/resync.hex"),
            Message::Resync(Resync { handle: 0 }),
        ),
        (
            "error",
            include_str!("data/dcc_feed/error.hex"),
            Message::Error(ErrorFrame {
                ref_seq: 3,
                handle: 2,
                code: "unknown_item".into(),
                message: "no item 2".into(),
                fatal: false,
            }),
        ),
        (
            "goodbye",
            include_str!("data/dcc_feed/goodbye.hex"),
            Message::Goodbye("done".into()),
        ),
    ]
}

#[test]
fn stored_vectors_decode_and_encode_byte_for_byte() {
    for (name, text, expected) in vectors() {
        let bytes = parse_hex(text);
        assert!(bytes.len() >= HEADER_BYTES, "{name}: shorter than a header");
        let head: [u8; HEADER_BYTES] = bytes[..HEADER_BYTES].try_into().expect("header");
        let header = FrameHeader::parse(&head);
        assert_eq!(
            header.len as usize,
            bytes.len() - 4,
            "{name}: len counts the bytes after it"
        );
        assert_eq!(header.flags, 0, "{name}: no flags");
        let decoded = decode(header, &bytes[HEADER_BYTES..], &Limits::default())
            .unwrap_or_else(|e| panic!("{name}: {} {}", e.code(), e.message));
        assert_eq!(decoded, expected, "{name}: decoded message");
        let mut out = Vec::new();
        encode(&expected, header.seq, &mut out).unwrap_or_else(|e| panic!("{name}: {}", e.code()));
        assert_eq!(out, bytes, "{name}: the encoder reproduces the stored bytes");
    }
}

/// Every message type of spec § 4.4 has at least one stored vector.
#[test]
fn every_message_type_has_a_stored_vector() {
    let covered: BTreeSet<u16> = vectors()
        .iter()
        .map(|(_, text, _)| {
            let bytes = parse_hex(text);
            u16::from_le_bytes([bytes[4], bytes[5]])
        })
        .collect();
    let spec_table: BTreeSet<u16> = [
        msg::HELLO,
        msg::HELLO_ACK,
        msg::ITEM_DECLARE,
        msg::ITEM_DELETE,
        msg::TRANSFORM,
        msg::CAMERA,
        msg::LIGHT,
        msg::MESH,
        msg::SKELETON_STATIC,
        msg::POSE,
        msg::CLIP_PUSH,
        msg::TEXTURE_PUSH,
        msg::PING,
        msg::PONG,
        msg::ACK,
        msg::SESSION_STATE,
        msg::RESYNC,
        msg::ERROR,
        msg::GOODBYE,
    ]
    .into_iter()
    .collect();
    // The ids are the spec's numbers, not just the codec's.
    let spec_numbers: BTreeSet<u16> = [
        0x0001, 0x0081, 0x0010, 0x0011, 0x0020, 0x0021, 0x0022, 0x0023, 0x0024, 0x0025, 0x0026, 0x0027,
        0x0030, 0x0031, 0x0082, 0x0083, 0x0084, 0x003F, 0x0032,
    ]
    .into_iter()
    .collect();
    assert_eq!(spec_table, spec_numbers, "message ids match spec 60 § 4.4");
    assert_eq!(covered, spec_numbers, "one stored vector per message type");
}

/// Each stored file is listed above, so a vector added to the folder is also checked.
#[test]
fn every_stored_file_is_checked() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/dcc_feed");
    let on_disk: BTreeSet<String> = std::fs::read_dir(&dir)
        .expect("vector folder")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter_map(|n| n.strip_suffix(".hex").map(str::to_owned))
        .collect();
    let listed: BTreeSet<String> = vectors().iter().map(|(n, _, _)| (*n).to_owned()).collect();
    assert_eq!(on_disk, listed);
}

/// Spec § 4.8: the default `Limits` the server sends in `hello_ack`.
#[test]
fn limits_defaults_are_the_spec_numbers() {
    let l = Limits::default();
    assert_eq!(l.max_message_bytes, 64 << 20);
    assert_eq!(l.max_vertices, 2_000_000);
    assert_eq!(l.max_indices, 12_000_000);
    assert_eq!(l.max_items, 2048);
    assert_eq!(l.max_joints, 1024);
    assert_eq!(l.max_clip_tracks, 4096);
    assert_eq!(l.max_keys_per_track, 100_000);
    assert_eq!(l.max_texture_dim, 8192);
    assert_eq!(l.max_texture_bytes, 256 << 20);
    assert_eq!(l.max_transient_texture_bytes, 512 << 20);
    assert_eq!(l.max_key_len, 128);
}

/// Spec § 2.1, § 2.2, § 2.6, § 3.2, § 3.4, § 4.3, § 4.7, § 4.11 and § 9.1: the numbers the prose
/// names, so a change to one of them is a visible spec change.
#[test]
fn session_playout_and_budget_defaults_are_the_spec_numbers() {
    use regia_agent::dcc_feed::settings::{MAX_DELAY_QUEUE_LEN, Playout};
    use regia_agent::dcc_feed::{inbox, plugin, recording, session, settings::DccFeedSettings};

    let s = DccFeedSettings::default();
    assert!(!s.enabled, "§ 9.1: off until the artist turns it on");
    assert!(!s.allow_remote);
    assert_eq!(s.max_sessions, 4);
    assert_eq!(s.playout, Playout::Snap);
    assert_eq!(s.playout_delay_ms, 50);
    assert_eq!(s.delay_queue_len, 64);
    assert_eq!(MAX_DELAY_QUEUE_LEN, 1024);
    assert_eq!(s.max_frame_age_ms, 2000);
    assert_eq!(s.stall_after_ms, 500);
    assert_eq!(s.window_bytes, 32 << 20);
    assert!(s.auto_accept.is_empty());
    // ADR-1891 (9): the WebSocket is opt-in, so the default listeners are the socket and loopback TCP.
    assert_eq!(s.listen, vec!["unix".to_owned(), "tcp:127.0.0.1:0".to_owned()]);

    assert_eq!(session::NEGOTIATE_TIMEOUT_S, 5.0);
    assert_eq!(session::PENDING_TIMEOUT_S, 120.0);
    assert_eq!(session::CLOSED_KEEP_S, 60.0);
    assert_eq!(session::AUTH_FAILURES_LOCKOUT, 5);
    assert_eq!(session::LOCKOUT_S, 60.0);
    assert_eq!(session::HELLO_INTERVAL_S, 1.0);

    assert_eq!(plugin::MAX_MESH_SWAPS_PER_FRAME, 2);
    assert_eq!(plugin::MAX_MESH_SWAP_BYTES, 8 << 20);
    assert_eq!(inbox::OVERLOAD_CLOSE_STREAK, 3);

    assert_eq!(PROTOCOL_VERSION, 1);
    assert_eq!(&HELLO_MAGIC, b"RFDP");
    assert_eq!(HEADER_BYTES, 12);
    assert_eq!((FLAG_FRAGMENT_MORE, FLAG_OPTIONAL, FLAG_COMPRESSED), (1, 2, 4));
    assert_eq!(&recording::MAGIC, b"RFDR");
    assert_eq!(recording::FORMAT_VERSION, 1);
}

/// Spec § 5: the access class of every `dcc_feed.*` method. Dispatch refuses a token without the
/// class (`permission_denied`) by the generic scope check, so the class is what this pins.
#[test]
fn method_access_classes_follow_the_spec_table() {
    use regia_agent::schema::{Permission, method};
    let table = [
        ("dcc_feed.status", Permission::Read),
        ("dcc_feed.items", Permission::Read),
        ("dcc_feed.accept", Permission::EditScene),
        ("dcc_feed.bake", Permission::Assets),
        ("dcc_feed.disconnect", Permission::EditScene),
        ("dcc_feed.record", Permission::EditScene),
        ("dcc_feed.replay", Permission::EditScene),
        // ADR-1891 (6), (8): the bake job poll and the panel's two controls.
        ("dcc_feed.bake_status", Permission::Read),
        ("dcc_feed.listen", Permission::EditScene),
        ("dcc_feed.rotate_token", Permission::EditScene),
    ];
    for (name, permission) in table {
        let schema = method(name).unwrap_or_else(|| panic!("{name} has a schema row"));
        assert_eq!(schema.permission, permission, "{name}");
    }
    let mut feed_methods: Vec<&str> = regia_agent::schema::METHODS
        .iter()
        .map(|m| m.name)
        .filter(|n| n.starts_with("dcc_feed."))
        .collect();
    feed_methods.sort_unstable();
    let mut listed: Vec<&str> = table.iter().map(|(n, _)| *n).collect();
    listed.sort_unstable();
    assert_eq!(feed_methods, listed, "every dcc_feed method is in the table");
}
