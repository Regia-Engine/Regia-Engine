// SPDX-License-Identifier: MPL-2.0
//! Spec 60 § 8 tests 1, 2 and 6: codec round trip and conformance, malformed input, coalescing and
//! back-pressure. No GPU, no sockets, no clock.
#![cfg(feature = "dcc-feed")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_agent::dcc_feed::inbox::{FeedInbox, InboxConfig, PushOutcome};
use regia_agent::dcc_feed::wire::*;

fn info(id: u64) -> FrameInfo {
    FrameInfo {
        frame_id: id,
        source_time_ns: id * 1000,
        timecode: None,
    }
}

fn xf() -> TransformBlock {
    TransformBlock {
        translation: [1.0, 2.0, 3.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0; 3],
        world_space: false,
    }
}

fn tri_mesh(handle: u32, id: u64, mode: u8) -> MeshUpdate {
    let pos: Vec<u8> = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]
        .iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    let mut sections = vec![MeshSection {
        section_id: section::POSITIONS,
        elem: Elem::F32,
        index: 0,
        count: 3,
        data: pos,
    }];
    if mode == 0 {
        let idx: Vec<u8> = [0u16, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect();
        sections.push(MeshSection {
            section_id: section::INDICES,
            elem: Elem::U16,
            index: 0,
            count: 3,
            data: idx,
        });
    }
    MeshUpdate {
        handle,
        info: info(id),
        update_mode: mode,
        sections,
    }
}

fn every_message() -> Vec<Message> {
    let tc = Timecode {
        hh: 1,
        mm: 2,
        ss: 3,
        ff: 4,
        rate_num: 24,
        rate_den: 1,
        drop_frame: false,
    };
    vec![
        Message::Hello(Hello {
            min_version: 1,
            max_version: 1,
            client_name: "ref".into(),
            client_version: "0.1".into(),
            dcc: "Blender 4.2".into(),
            space: "gltf".into(),
            unit_scale: 1.0,
            caps: 0b1111,
            token: String::new(),
        }),
        Message::HelloAck(HelloAck {
            version: 1,
            session_id: 7,
            accepted: true,
            caps: 3,
            limits: Limits::default(),
            window_bytes: 1 << 20,
            server_name: "regia".into(),
        }),
        Message::ItemDeclare(ItemDeclare {
            handle: 1,
            key: "Cube".into(),
            kind: FeedKind::Mesh,
            parent_handle: 0,
            display_name: "Cube".into(),
            variant: 0,
            channels: vec![],
        }),
        Message::ItemDeclare(ItemDeclare {
            handle: 2,
            key: "Rig".into(),
            kind: FeedKind::Generic,
            parent_handle: 1,
            display_name: "".into(),
            variant: 0,
            channels: vec!["a".into(), "b".into()],
        }),
        Message::ItemDelete(2),
        Message::Transform(TransformFrame {
            handle: 1,
            info: FrameInfo {
                timecode: Some(tc),
                ..info(5)
            },
            xf: xf(),
        }),
        Message::Camera(CameraFrame {
            handle: 3,
            info: info(1),
            xf: xf(),
            lens: Lens::Perspective {
                yfov_rad: 0.8,
                aspect: 0.0,
                near: 0.1,
                far: 0.0,
            },
            physical: Some(PhysicalCamera {
                focal_mm: 35.0,
                sensor_w_mm: 36.0,
                sensor_h_mm: 24.0,
                focus_m: 3.0,
                f_stop: 2.8,
            }),
        }),
        Message::Camera(CameraFrame {
            handle: 3,
            info: info(2),
            xf: xf(),
            lens: Lens::Orthographic {
                xmag: 2.0,
                ymag: 1.0,
                near: 0.1,
                far: 50.0,
            },
            physical: None,
        }),
        Message::Light(LightFrame {
            handle: 4,
            info: info(1),
            xf: xf(),
            color: [1.0, 0.9, 0.8],
            intensity: 10.0,
            range: 0.0,
            spot_inner_rad: 0.1,
            spot_outer_rad: 0.4,
        }),
        Message::Mesh(tri_mesh(1, 1, 0)),
        Message::Mesh(tri_mesh(1, 2, 1)),
        Message::SkeletonStatic(SkeletonStatic {
            handle: 5,
            joints: vec![
                JointStatic {
                    name: "root".into(),
                    parent: -1,
                    t: [0.0; 3],
                    r: [0.0, 0.0, 0.0, 1.0],
                    s: [1.0; 3],
                },
                JointStatic {
                    name: "arm".into(),
                    parent: 0,
                    t: [0.0, 1.0, 0.0],
                    r: [0.0, 0.0, 0.0, 1.0],
                    s: [1.0; 3],
                },
            ],
        }),
        Message::Pose(PoseFrame {
            handle: 5,
            info: info(3),
            layout: 0,
            count: 2,
            values: [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]
                .iter()
                .cycle()
                .take(20)
                .copied()
                .collect(),
        }),
        Message::Pose(PoseFrame {
            handle: 6,
            info: info(3),
            layout: 1,
            count: 3,
            values: vec![0.1, 0.2, 0.3],
        }),
        Message::ClipPush(ClipPush {
            name: "walk".into(),
            fps: 30.0,
            duration_s: 1.0,
            looping: true,
            bind_handle: 5,
            tracks: vec![ClipTrack {
                target: "arm".into(),
                property: 1,
                interp: 1,
                times: vec![0.0, 1.0],
                values: vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            }],
        }),
        Message::TexturePush(TexturePush {
            name: "t".into(),
            colorspace: 0,
            source: TextureSource::Path("tex/a.png".into()),
        }),
        Message::TexturePush(TexturePush {
            name: "t".into(),
            colorspace: 1,
            source: TextureSource::Encoded {
                container: 0,
                bytes: vec![1, 2, 3],
            },
        }),
        Message::TexturePush(TexturePush {
            name: "t".into(),
            colorspace: 2,
            source: TextureSource::Raw {
                pixel_format: 0,
                width: 2,
                height: 1,
                bytes: vec![0; 8],
            },
        }),
        Message::Ping(Ping {
            token: 9,
            sender_time_ns: 100,
            responder_time_ns: 0,
        }),
        Message::Pong(Ping {
            token: 9,
            sender_time_ns: 100,
            responder_time_ns: 150,
        }),
        Message::Ack(Ack {
            seq_applied: 4,
            credit_bytes: 4096,
        }),
        Message::SessionState(SessionState {
            state: 2,
            reason: "accepted".into(),
        }),
        Message::Resync(Resync { handle: 0 }),
        Message::Error(ErrorFrame {
            ref_seq: 3,
            handle: 1,
            code: "item_conflict".into(),
            message: "x".into(),
            fatal: false,
        }),
        Message::Goodbye("done".into()),
    ]
}

fn decode_frame(bytes: &[u8]) -> Result<Message, WireError> {
    let head: [u8; HEADER_BYTES] = bytes[..HEADER_BYTES].try_into().unwrap();
    let h = FrameHeader::parse(&head);
    assert_eq!(
        h.len as usize,
        bytes.len() - 4,
        "len counts the bytes after the len field"
    );
    decode(h, &bytes[HEADER_BYTES..], &Limits::default())
}

#[test]
fn every_message_round_trips() {
    for (seq, m) in (1u32..).zip(every_message()) {
        let mut out = Vec::new();
        encode(&m, seq, &mut out).unwrap();
        assert_eq!(decode_frame(&out).unwrap(), m, "{m:?}");
        let h = FrameHeader::parse(out[..HEADER_BYTES].try_into().unwrap());
        assert_eq!(h.seq, seq);
        assert_eq!(Some(h.msg_type), m.msg_type());
    }
}

/// Independent decoder written from the § 4.4 table only (spec § 8 test 1): a `transform` frame
/// laid out by hand must decode to the expected values.
#[test]
fn hand_laid_out_transform_decodes() {
    let mut f: Vec<u8> = Vec::new();
    f.extend_from_slice(&[0u8; 12]); // header patched below
    f.extend_from_slice(&1u32.to_le_bytes()); // handle
    f.extend_from_slice(&9u64.to_le_bytes()); // frame_id
    f.extend_from_slice(&1000u64.to_le_bytes()); // source_time_ns
    f.push(0); // has_timecode
    for v in [1.0f32, 2.0, 3.0] {
        f.extend_from_slice(&v.to_le_bytes()); // translation
    }
    for v in [0.0f32, 0.0, 0.0, 1.0] {
        f.extend_from_slice(&v.to_le_bytes()); // rotation
    }
    for v in [1.0f32, 1.0, 1.0] {
        f.extend_from_slice(&v.to_le_bytes()); // scale
    }
    f.push(1); // space_flags: world
    let len = (f.len() - 4) as u32;
    f[0..4].copy_from_slice(&len.to_le_bytes());
    f[4..6].copy_from_slice(&0x0020u16.to_le_bytes());
    f[8..12].copy_from_slice(&1u32.to_le_bytes());
    let Message::Transform(t) = decode_frame(&f).unwrap() else {
        panic!("not a transform")
    };
    assert_eq!((t.handle, t.info.frame_id, t.info.source_time_ns), (1, 9, 1000));
    assert_eq!(t.xf.translation, [1.0, 2.0, 3.0]);
    assert!(t.xf.world_space);
    // And the encoder produces exactly those bytes.
    let mut out = Vec::new();
    encode(&Message::Transform(t), 1, &mut out).unwrap();
    assert_eq!(out, f);
}

#[test]
fn hello_hex_vector() {
    // 12 byte header, "RFDP", min 1, max 1, "a", "b", "c", "gltf", scale 1.0, caps 0, token "".
    let hex = "2d000000 0100 0000 01000000 52464450 01000000 01000000 0100 61 0100 62 0100 63 0400 676c7466 0000803f 00000000 0000";
    let bytes: Vec<u8> = hex
        .split_whitespace()
        .flat_map(|w| {
            (0..w.len())
                .step_by(2)
                .map(move |i| u8::from_str_radix(&w[i..i + 2], 16).unwrap())
        })
        .collect();
    let m = decode_frame(&bytes).unwrap();
    let Message::Hello(h) = m else { panic!("not hello") };
    assert_eq!(
        (h.client_name.as_str(), h.dcc.as_str(), h.space.as_str()),
        ("a", "c", "gltf")
    );
    let mut out = Vec::new();
    encode(&Message::Hello(h), 1, &mut out).unwrap();
    assert_eq!(out, bytes);
}

fn code_of(r: Result<Message, WireError>) -> &'static str {
    r.expect_err("must fail").code()
}

fn frame_of(msg_type: u16, flags: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = FrameHeader {
        len: payload.len() as u32 + 8,
        msg_type,
        flags,
        seq: 1,
    }
    .to_bytes()
    .to_vec();
    v.extend_from_slice(payload);
    v
}

#[test]
fn malformed_input_gets_documented_codes() {
    // Bad magic.
    let mut hello = Vec::new();
    encode(&every_message()[0], 1, &mut hello).unwrap();
    hello[HEADER_BYTES] = b'X';
    assert_eq!(code_of(decode_frame(&hello)), "bad_magic");

    // Unknown mandatory type is fatal-class; with OPTIONAL it is skipped.
    assert_eq!(
        code_of(decode_frame(&frame_of(0x7000, 0, &[]))),
        "unsupported_message"
    );
    assert_eq!(
        decode_frame(&frame_of(0x7000, FLAG_OPTIONAL, &[])).unwrap(),
        Message::Unknown {
            msg_type: 0x7000,
            optional: true
        }
    );

    // COMPRESSED is reserved.
    assert_eq!(
        code_of(decode_frame(&frame_of(
            msg::ITEM_DELETE,
            FLAG_COMPRESSED,
            &[0; 4]
        ))),
        "malformed_frame"
    );

    // NaN rejects the whole frame.
    let mut t = Vec::new();
    encode(
        &Message::Transform(TransformFrame {
            handle: 1,
            info: info(1),
            xf: xf(),
        }),
        1,
        &mut t,
    )
    .unwrap();
    let nan = f32::NAN.to_le_bytes();
    let at = HEADER_BYTES + 4 + 8 + 8 + 1;
    t[at..at + 4].copy_from_slice(&nan);
    assert_eq!(code_of(decode_frame(&t)), "invalid_value");

    // Zero quaternion is rejected; an off-length one is normalized and counted.
    let mut z = Vec::new();
    let mut bad = TransformFrame {
        handle: 1,
        info: info(1),
        xf: xf(),
    };
    bad.xf.rotation = [0.0; 4];
    encode(&Message::Transform(bad), 1, &mut z).unwrap();
    assert_eq!(code_of(decode_frame(&z)), "invalid_value");
    bad.xf.rotation = [0.0, 0.0, 0.0, 2.0];
    z.clear();
    encode(&Message::Transform(bad), 1, &mut z).unwrap();
    let h = FrameHeader::parse(z[..HEADER_BYTES].try_into().unwrap());
    let (m, warnings) = decode_with_warnings(h, &z[HEADER_BYTES..], &Limits::default()).unwrap();
    assert_eq!(warnings, 1);
    let Message::Transform(t) = m else { unreachable!() };
    assert_eq!(t.xf.rotation, [0.0, 0.0, 0.0, 1.0]);

    // Bad UTF-8 in a string.
    let payload = [1, 0, 0, 0, 2, 0, 0xff, 0xfe];
    assert_eq!(
        code_of(decode_frame(&frame_of(msg::ITEM_DECLARE, 0, &payload))),
        "malformed_frame"
    );

    // Index out of range.
    let mut mesh = tri_mesh(1, 1, 0);
    mesh.sections[1].data = [0u16, 1, 9].iter().flat_map(|i| i.to_le_bytes()).collect();
    let mut out = Vec::new();
    encode(&Message::Mesh(mesh), 1, &mut out).unwrap();
    assert_eq!(code_of(decode_frame(&out)), "invalid_value");

    // Mismatched section byte length.
    let mut mesh = tri_mesh(1, 1, 0);
    mesh.sections[0].count = 4;
    let mut out = Vec::new();
    encode(&Message::Mesh(mesh), 1, &mut out).unwrap();
    assert_eq!(code_of(decode_frame(&out)), "malformed_frame");

    // Vertex limit.
    let small = Limits {
        max_vertices: 2,
        ..Limits::default()
    };
    let mut out = Vec::new();
    encode(&Message::Mesh(tri_mesh(1, 1, 0)), 1, &mut out).unwrap();
    let h = FrameHeader::parse(out[..HEADER_BYTES].try_into().unwrap());
    assert_eq!(
        decode(h, &out[HEADER_BYTES..], &small).unwrap_err().code(),
        "limit_exceeded"
    );

    // Handle 0 is reserved.
    let mut d = Vec::new();
    let mut decl = match every_message()[2].clone() {
        Message::ItemDeclare(d) => d,
        _ => unreachable!(),
    };
    decl.handle = 0;
    encode(&Message::ItemDeclare(decl), 1, &mut d).unwrap();
    assert_eq!(code_of(decode_frame(&d)), "invalid_value");
}

#[test]
fn every_prefix_and_bit_flip_is_total() {
    // No panic for any truncation of any valid message, nor for single-byte corruption.
    let limits = Limits {
        max_message_bytes: 1 << 16,
        ..Limits::default()
    };
    for m in every_message() {
        let mut out = Vec::new();
        encode(&m, 1, &mut out).unwrap();
        let h = FrameHeader::parse(out[..HEADER_BYTES].try_into().unwrap());
        let payload = &out[HEADER_BYTES..];
        for n in 0..payload.len() {
            let r = decode(h, &payload[..n], &limits);
            if let Err(e) = r {
                assert!(ErrorCode::from_code(e.code()).is_some());
            }
        }
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..200 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut p = payload.to_vec();
            if p.is_empty() {
                break;
            }
            let i = (state % p.len() as u64) as usize;
            p[i] ^= (state >> 32) as u8 | 1;
            let _ = decode(h, &p, &limits);
        }
    }
}

#[test]
fn error_codes_are_protocol_vocabulary() {
    for c in ErrorCode::ALL {
        assert!(
            c.as_str()
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        );
        assert_eq!(ErrorCode::from_code(c.as_str()), Some(c));
    }
    assert!(
        ErrorCode::BadMagic.is_fatal()
            && !ErrorCode::UnknownItem.is_fatal()
            && !ErrorCode::Overloaded.is_fatal()
    );
}

#[test]
fn frame_reader_is_bounded() {
    let mut out = Vec::new();
    encode(&Message::ItemDelete(1), 1, &mut out).unwrap();
    encode(&Message::ItemDelete(2), 2, &mut out).unwrap();
    let limits = Limits::default();
    let mut r = FrameReader::new(&out[..]);
    assert!(r.next(&limits).unwrap().is_some());
    assert!(r.next(&limits).unwrap().is_some());
    assert!(r.next(&limits).unwrap().is_none());
    // A length above the limit is refused before any payload is read.
    let huge = FrameHeader {
        len: u32::MAX,
        msg_type: msg::MESH,
        flags: 0,
        seq: 1,
    }
    .to_bytes();
    let mut r = FrameReader::new(&huge[..]);
    assert_eq!(r.next(&limits).unwrap_err().code(), "frame_too_large");
    // Truncated mid-frame.
    let mut r = FrameReader::new(&out[..out.len() - 2]);
    r.next(&limits).unwrap();
    assert_eq!(r.next(&limits).unwrap_err().code(), "malformed_frame");
    // Length below the header remainder.
    let short = FrameHeader {
        len: 3,
        msg_type: 1,
        flags: 0,
        seq: 1,
    }
    .to_bytes();
    assert_eq!(
        FrameReader::new(&short[..]).next(&limits).unwrap_err().code(),
        "malformed_frame"
    );
}

#[test]
fn fragments_reassemble_and_ping_interleaves() {
    let msg = Message::Mesh(tri_mesh(1, 1, 0));
    let mut out = Vec::new();
    let next = encode_fragments(&msg, 10, 16, &mut out).unwrap();
    assert!(next > 11);
    let mut ping = Vec::new();
    encode(
        &Message::Ping(Ping {
            token: 1,
            sender_time_ns: 2,
            responder_time_ns: 0,
        }),
        99,
        &mut ping,
    )
    .unwrap();

    let limits = Limits::default();
    let mut rd = FrameReader::new(&out[..]);
    let mut frames = Vec::new();
    while let Some(f) = rd.next(&limits).unwrap() {
        frames.push(f);
    }
    assert!(frames.len() > 2);
    let mut ping_rd = FrameReader::new(&ping[..]);
    let ping_frame = ping_rd.next(&limits).unwrap().unwrap();

    let mut re = Reassembler::new();
    let mut done = None;
    for (i, (h, p)) in frames.into_iter().enumerate() {
        if i == 1 {
            let got = re.push(ping_frame.0, ping_frame.1.clone(), &limits).unwrap();
            assert!(got.is_some(), "ping passes through a fragment run");
        }
        if let Some(c) = re.push(h, p, &limits).unwrap() {
            done = Some(c);
        }
    }
    let (h, payload) = done.expect("message completes");
    assert_eq!(h.seq, 10);
    assert_eq!(decode(h, &payload, &limits).unwrap(), msg);

    // A different type inside a run is malformed; an oversize accumulation is frame_too_large.
    let mut re = Reassembler::new();
    let first = FrameHeader {
        len: 12,
        msg_type: msg::MESH,
        flags: FLAG_FRAGMENT_MORE,
        seq: 1,
    };
    assert!(re.push(first, vec![0; 4], &limits).unwrap().is_none());
    let other = FrameHeader {
        msg_type: msg::TRANSFORM,
        ..first
    };
    assert_eq!(
        re.push(other, vec![0; 4], &limits).unwrap_err().code(),
        "malformed_frame"
    );
    let tiny = Limits {
        max_message_bytes: 6,
        ..Limits::default()
    };
    let mut re = Reassembler::new();
    assert!(re.push(first, vec![0; 4], &tiny).unwrap().is_none());
    assert_eq!(
        re.push(first, vec![0; 4], &tiny).unwrap_err().code(),
        "frame_too_large"
    );
}

fn tf(handle: u32, id: u64) -> Message {
    Message::Transform(TransformFrame {
        handle,
        info: info(id),
        xf: xf(),
    })
}

#[test]
fn hundred_transforms_coalesce_to_the_last() {
    let mut inbox = FeedInbox::new(InboxConfig::default());
    for i in 1..=100u64 {
        assert_eq!(inbox.push(1, i as u32, 100, tf(1, i)), PushOutcome::Accepted);
    }
    assert_eq!(inbox.len(), 1);
    let d = inbox.drain();
    assert_eq!(d.entries.len(), 1);
    let Message::Transform(t) = &d.entries[0].msg else {
        panic!()
    };
    assert_eq!(t.info.frame_id, 100);
    assert_eq!(inbox.counters(1).frames_coalesced, 99);
    // Credits returned equal the bytes accepted, in one ack.
    assert_eq!(d.credits.len(), 1);
    assert_eq!(d.credits[0].credit_bytes, 100 * 100);
    assert_eq!(d.credits[0].seq_applied, 100);
    assert_eq!(inbox.outstanding(1), 0);
}

#[test]
fn delayed_playout_keeps_every_frame() {
    let mut inbox = FeedInbox::new(InboxConfig {
        coalesce_interpolated: false,
        ..InboxConfig::default()
    });
    for i in 1..=5u64 {
        inbox.push(1, i as u32, 10, tf(1, i));
    }
    assert_eq!(inbox.drain().entries.len(), 5);
}

#[test]
fn vertex_only_mesh_merges_into_pending_full_mesh() {
    let mut inbox = FeedInbox::new(InboxConfig::default());
    inbox.push(1, 1, 50, Message::Mesh(tri_mesh(1, 1, 0)));
    let mut vo = tri_mesh(1, 2, 1);
    vo.sections[0].data = [5.0f32; 9].iter().flat_map(|f| f.to_le_bytes()).collect();
    inbox.push(1, 2, 40, Message::Mesh(vo));
    assert_eq!(inbox.len(), 1);
    let d = inbox.drain();
    let Message::Mesh(m) = &d.entries[0].msg else {
        panic!()
    };
    assert_eq!(m.update_mode, 0, "stays a full update");
    assert_eq!(m.info.frame_id, 2);
    assert!(
        m.sections.iter().any(|s| s.section_id == section::INDICES),
        "indices kept"
    );
    let pos = m
        .sections
        .iter()
        .find(|s| s.section_id == section::POSITIONS)
        .unwrap();
    assert_eq!(&pos.data[..4], &5.0f32.to_le_bytes(), "positions replaced");
    assert_eq!(d.credits[0].credit_bytes, 90);

    // A newer full update supersedes an older vertex-only one.
    let mut inbox = FeedInbox::new(InboxConfig::default());
    inbox.push(1, 1, 10, Message::Mesh(tri_mesh(1, 1, 1)));
    inbox.push(1, 2, 10, Message::Mesh(tri_mesh(1, 2, 0)));
    assert_eq!(inbox.len(), 1);
}

/// Dcc audit H2: a vertex-only update whose counts differ from the pending full mesh is not merged
/// (the merged mesh would pair the old indices with the new vertices); it stays a separate entry
/// and `check_vertex_only` answers `topology_mismatch` for it.
#[test]
fn vertex_only_mesh_with_another_vertex_count_is_not_merged() {
    let mut inbox = FeedInbox::new(InboxConfig::default());
    inbox.push(1, 1, 50, Message::Mesh(tri_mesh(1, 1, 0)));
    let mut two = tri_mesh(1, 2, 1);
    two.sections[0].count = 2;
    two.sections[0].data.truncate(24);
    inbox.push(1, 2, 40, Message::Mesh(two.clone()));
    let mut normals = tri_mesh(1, 3, 1);
    normals.sections[0] = MeshSection {
        section_id: section::NORMALS,
        elem: Elem::F32,
        index: 0,
        count: 1_000_000,
        data: Vec::new(),
    };
    inbox.push(1, 3, 40, Message::Mesh(normals.clone()));
    assert_eq!(inbox.len(), 3, "neither update merges into the full mesh");
    let d = inbox.drain();
    let Message::Mesh(full) = &d.entries[0].msg else {
        panic!()
    };
    assert_eq!(full, &tri_mesh(1, 1, 0), "the full mesh is untouched");
    assert_eq!(
        check_vertex_only(&two, 3).unwrap_err().code(),
        "topology_mismatch"
    );
    assert_eq!(
        check_vertex_only(&normals, 3).unwrap_err().code(),
        "topology_mismatch"
    );
    assert!(check_vertex_only(&tri_mesh(1, 4, 1), 3).is_ok());
    assert!(
        check_vertex_only(&tri_mesh(1, 4, 0), 3).is_err(),
        "a full update is not vertex-only"
    );
}

#[test]
fn delete_discards_pending_data_of_the_handle() {
    let mut inbox = FeedInbox::new(InboxConfig::default());
    inbox.push(1, 1, 10, tf(1, 1));
    inbox.push(1, 2, 10, tf(2, 1));
    inbox.push(1, 3, 10, Message::ItemDelete(1));
    let d = inbox.drain();
    let handles: Vec<_> = d.entries.iter().map(|e| e.msg.handle()).collect();
    assert_eq!(handles, vec![Some(2), Some(1)]);
    assert!(matches!(d.entries[1].msg, Message::ItemDelete(1)));
    assert_eq!(
        d.credits[0].credit_bytes, 30,
        "the discarded frame's bytes come back too"
    );
}

#[test]
fn window_overflow_is_overloaded_then_closes() {
    let mut inbox = FeedInbox::new(InboxConfig {
        window_bytes: 100,
        coalesce_interpolated: false,
    });
    // Declarations are never coalesced, so they fill the window.
    let decl = |h| {
        Message::ItemDeclare(ItemDeclare {
            handle: h,
            key: format!("k{h}"),
            kind: FeedKind::Transform,
            parent_handle: 0,
            display_name: String::new(),
            variant: 0,
            channels: vec![],
        })
    };
    assert_eq!(inbox.push(1, 1, 80, decl(1)), PushOutcome::Accepted);
    // Crosses the line by one frame: allowed.
    assert_eq!(inbox.push(1, 2, 80, decl(2)), PushOutcome::Accepted);
    assert_eq!(inbox.push(1, 3, 80, decl(3)), PushOutcome::Overloaded);
    assert_eq!(inbox.push(1, 4, 80, decl(4)), PushOutcome::Overloaded);
    assert_eq!(inbox.push(1, 5, 80, decl(5)), PushOutcome::CloseSession);
    assert_eq!(inbox.counters(1).dropped_overload, 3);
    // Another session is unaffected, and credit reopens the window.
    assert_eq!(inbox.push(2, 1, 80, decl(1)), PushOutcome::Accepted);
    inbox.drain();
    assert_eq!(inbox.push(1, 6, 80, decl(6)), PushOutcome::Accepted);
    inbox.discard_session(1);
    assert_eq!(inbox.outstanding(1), 0);
}

/// The texture payload of a `texture_push` by path, built by hand so the encoder's own checks do not
/// stand in for the decoder's.
fn path_texture(path: &str) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&1u16.to_le_bytes());
    p.push(b't');
    p.push(0); // colorspace
    p.push(0); // by path
    p.extend_from_slice(&(path.len() as u16).to_le_bytes());
    p.extend_from_slice(path.as_bytes());
    frame_of(msg::TEXTURE_PUSH, 0, &p)
}

/// dcc audit M1: `path_denied` was documented and never returned.
#[test]
fn a_texture_path_that_can_never_be_opened_is_denied_at_decode() {
    assert!(decode_frame(&path_texture("tex/a.png")).is_ok());
    for bad in ["../../../etc/passwd", "tex/../../x.png", "a\0b", ""] {
        assert_eq!(
            code_of(decode_frame(&path_texture(bad))),
            "path_denied",
            "{bad:?}"
        );
    }
}

/// `C:/...` is relative (and refused at decode) off Windows; on Windows it is absolute, so decode
/// accepts its shape and `resolve_source_path` is what denies it.
#[cfg(windows)]
#[test]
fn a_drive_path_outside_the_project_is_denied_at_resolve() {
    let base = regia_test_utils::unique_temp_dir("dcc_drive_path");
    let (project, inbox) = (base.join("project"), base.join("inbox"));
    for d in [&project, &inbox] {
        std::fs::create_dir_all(d).unwrap();
    }
    let path = "C:/Windows/system32/config/SAM";
    assert!(decode_frame(&path_texture(path)).is_ok());
    let err = resolve_source_path(path, &project, &inbox).expect_err("denied");
    assert_eq!(err.code(), "path_denied");
}

#[cfg(not(windows))]
#[test]
fn a_drive_path_is_denied_at_decode() {
    assert_eq!(
        code_of(decode_frame(&path_texture("C:/Windows/system32/config/SAM"))),
        "path_denied"
    );
}

/// dcc audit M1: the stateful half of spec § 4.8, after symlink resolution.
#[test]
fn a_texture_path_must_land_in_the_project_or_the_inbox() {
    let base = regia_test_utils::unique_temp_dir("dcc_source_path");
    let (project, inbox, outside) = (base.join("project"), base.join("inbox"), base.join("outside"));
    for d in [&project, &inbox, &outside] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("a.png"), b"x").unwrap();
    }
    let in_inbox = inbox.join("a.png");
    assert!(resolve_source_path("a.png", &project, &inbox).is_ok());
    assert!(resolve_source_path(in_inbox.to_str().unwrap(), &project, &inbox).is_ok());
    let denied = |p: &str| resolve_source_path(p, &project, &inbox).unwrap_err().code();
    assert_eq!(denied(outside.join("a.png").to_str().unwrap()), "path_denied");
    assert_eq!(denied("../outside/a.png"), "path_denied");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, project.join("link")).unwrap();
        assert_eq!(denied("link/a.png"), "path_denied", "a symlink cannot lead out");
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// dcc audit M2: with a limit a hello may advertise, the product left `u64`: a debug build panicked
/// on the hostile frame and a release build accepted the wrapped size.
#[test]
fn a_raw_texture_size_that_leaves_u64_is_malformed() {
    let limits = Limits {
        max_texture_dim: u32::MAX,
        ..Limits::default()
    };
    let mut p = Vec::new();
    p.extend_from_slice(&1u16.to_le_bytes());
    p.push(b't');
    p.push(0); // colorspace
    p.push(2); // raw
    p.push(2); // rgba32f: 16 bytes per pixel
    p.extend_from_slice(&u32::MAX.to_le_bytes());
    p.extend_from_slice(&u32::MAX.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    let frame = frame_of(msg::TEXTURE_PUSH, 0, &p);
    let (header, payload) = FrameReader::new(&frame[..]).next(&limits).unwrap().unwrap();
    assert_eq!(
        decode(header, &payload, &limits).unwrap_err().code(),
        "malformed_frame"
    );
}

/// A stream that gives the bytes it holds, then fails with `end`; records the largest read asked of it.
struct Stalling {
    data: Vec<u8>,
    at: usize,
    end: std::io::ErrorKind,
    largest_read: usize,
}

impl std::io::Read for Stalling {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.largest_read = self.largest_read.max(buf.len());
        if self.at == self.data.len() {
            return Err(self.end.into());
        }
        let n = buf.len().min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// dcc audit M3: a header claiming 64 MiB followed by silence used to hold a zeroed 64 MiB buffer.
#[test]
fn a_stalled_frame_holds_what_arrived_and_reports_the_deadline() {
    let limits = Limits::default();
    let head = FrameHeader {
        len: limits.max_message_bytes,
        msg_type: msg::MESH,
        flags: 0,
        seq: 1,
    }
    .to_bytes();
    let mut data = head.to_vec();
    data.extend_from_slice(&[0u8; 100]);
    for (end, code) in [
        (std::io::ErrorKind::TimedOut, "timeout"),
        (std::io::ErrorKind::WouldBlock, "timeout"),
        (std::io::ErrorKind::ConnectionReset, "internal"),
    ] {
        let mut stream = Stalling {
            data: data.clone(),
            at: 0,
            end,
            largest_read: 0,
        };
        let code_got = FrameReader::new(&mut stream).next(&limits).unwrap_err().code();
        assert_eq!(code_got, code);
        assert!(
            stream.largest_read < 1 << 20,
            "a read of {} bytes means the buffer followed the claim, not the stream",
            stream.largest_read
        );
    }
}

/// dcc audit L2: merged updates are charged wider than one frame's `u32`, and each byte comes back.
#[test]
fn merged_updates_give_back_every_byte_of_credit() {
    let mut inbox = FeedInbox::new(InboxConfig {
        window_bytes: u32::MAX,
        coalesce_interpolated: true,
    });
    let s = 1;
    let full = Message::Mesh(tri_mesh(7, 1, 0));
    let vertex_only = Message::Mesh(tri_mesh(7, 2, 1));
    let frame = u32::MAX / 2;
    assert_eq!(inbox.push(s, 1, frame, full), PushOutcome::Accepted);
    assert_eq!(
        inbox.push(s, 2, frame, vertex_only.clone()),
        PushOutcome::Accepted
    );
    assert_eq!(inbox.push(s, 3, 3, vertex_only), PushOutcome::Accepted);
    let first = inbox.drain();
    let charged = 2 * u64::from(frame) + 3;
    assert_eq!(first.entries.len(), 1, "the updates merged");
    assert_eq!(first.entries[0].bytes, charged);
    let mut back: u64 = first.credits.iter().map(|c| u64::from(c.credit_bytes)).sum();
    back += inbox
        .drain()
        .credits
        .iter()
        .map(|c| u64::from(c.credit_bytes))
        .sum::<u64>();
    assert_eq!(back, charged);
    assert_eq!(inbox.outstanding(s), 0);
}
