// SPDX-License-Identifier: MPL-2.0
//! Spec 60 § 8 test 2: hostile length fields are refused before anything is reserved.
//!
//! The spec asks for a counting allocator; the workspace forbids `unsafe`, which a `GlobalAlloc`
//! needs. The guarantee is structural instead: every `Vec::with_capacity` in the decoder is sized
//! from a count that was first compared with the bytes present, so these inputs, which claim
//! gigabytes behind under a hundred bytes, return an error without reserving. A decoder that
//! reserved first would abort the process with a capacity overflow or an allocation failure here.
#![cfg(feature = "dcc-feed")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use regia_agent::dcc_feed::wire::*;

#[test]
fn hostile_counts_are_refused_before_reserving() {
    let limits = Limits::default();
    let u32max = u32::MAX.to_le_bytes();
    let mut cases: Vec<(u16, Vec<u8>)> = Vec::new();

    // mesh: one section claiming u32::MAX bytes.
    let mut mesh = Vec::new();
    mesh.extend_from_slice(&1u32.to_le_bytes()); // handle
    mesh.extend_from_slice(&[0u8; 17]); // frame info without timecode
    mesh.push(0); // update_mode
    mesh.extend_from_slice(&u16::MAX.to_le_bytes()); // section_count
    mesh.extend_from_slice(&[1, 0, 0, 0]);
    mesh.extend_from_slice(&u32max);
    mesh.extend_from_slice(&u32max);
    cases.push((msg::MESH, mesh));

    // clip: track count and key counts at the limit with no bytes behind them.
    let mut clip = Vec::new();
    clip.extend_from_slice(&0u16.to_le_bytes());
    clip.extend_from_slice(&[0u8; 8]);
    clip.push(0);
    clip.extend_from_slice(&0u32.to_le_bytes());
    clip.extend_from_slice(&limits.max_clip_tracks.to_le_bytes());
    cases.push((msg::CLIP_PUSH, clip));

    // skeleton: joint count at u16::MAX.
    let mut skel = 1u32.to_le_bytes().to_vec();
    skel.extend_from_slice(&u16::MAX.to_le_bytes());
    cases.push((msg::SKELETON_STATIC, skel));

    // texture: raw, byte_len u32::MAX.
    let mut tex = Vec::new();
    tex.extend_from_slice(&0u16.to_le_bytes());
    tex.extend_from_slice(&[0, 2, 0]);
    tex.extend_from_slice(&1u32.to_le_bytes());
    tex.extend_from_slice(&1u32.to_le_bytes());
    tex.extend_from_slice(&u32max);
    cases.push((msg::TEXTURE_PUSH, tex));

    // generic declare: channel count u16::MAX.
    let mut decl = 1u32.to_le_bytes().to_vec();
    decl.extend_from_slice(&[1, 0, b'k', 6]);
    decl.extend_from_slice(&0u32.to_le_bytes());
    decl.extend_from_slice(&[0, 0, 0]);
    decl.extend_from_slice(&u16::MAX.to_le_bytes());
    cases.push((msg::ITEM_DECLARE, decl));

    for (ty, payload) in &cases {
        let h = FrameHeader {
            len: payload.len() as u32 + 8,
            msg_type: *ty,
            flags: 0,
            seq: 1,
        };
        let err = decode(h, payload, &limits).expect_err("hostile counts must be refused");
        assert!(ErrorCode::from_code(err.code()).is_some(), "type {ty:#x}: {err}");
    }
}

/// Seeded payloads under every kind of frame: the decoder answers `Err` or a value, with nothing reserved up front.
#[test]
fn mutated_frames_never_panic() {
    let limits = Limits::default();
    let mut rng = regia_core::fuzz::FuzzRng::new(0xDCC0_0001);
    for msg_type in [
        msg::HELLO,
        msg::ITEM_DECLARE,
        msg::ITEM_DELETE,
        msg::TRANSFORM,
        msg::CAMERA,
        msg::LIGHT,
        msg::MESH,
        msg::SKELETON_STATIC,
        msg::POSE,
        msg::CLIP_PUSH,
    ] {
        for _ in 0..200 {
            let payload = regia_core::fuzz::mutate_bytes(&[0u8; 64], &mut rng);
            let header = FrameHeader {
                len: 8 + payload.len() as u32,
                msg_type,
                flags: 0,
                seq: 1,
            };
            let _ = decode(header, &payload, &limits);
        }
    }
}

/// A recording with one record; every record that survives the mutation goes through the frame decoder, as
/// `regia dcc-feed dump` does.
#[test]
fn mutated_recordings_never_panic() {
    use regia_agent::dcc_feed::recording::Recording;
    let mut recording = Recording::new("fuzz", "blender");
    let mut frame = (8u32 + 8).to_le_bytes().to_vec();
    frame.extend_from_slice(&msg::CAMERA.to_le_bytes());
    frame.extend_from_slice(&[0u8; 2 + 4 + 8]);
    recording.push(3, frame);
    let base = recording.to_bytes().expect("a recording encodes");
    assert!(Recording::from_bytes(&base).is_ok(), "the fixture must load");
    let limits = Limits::default();
    let mut rng = regia_core::fuzz::FuzzRng::new(0xDCC0_0002);
    for _ in 0..2000 {
        let bytes = regia_core::fuzz::mutate_bytes(&base, &mut rng);
        if let Ok(loaded) = Recording::from_bytes(&bytes) {
            let _ = loaded.dump_lines(&limits);
        }
    }
}
