// SPDX-License-Identifier: MPL-2.0
//! Spec 60 § 8 test 2, the counting-allocator half: the wire layer's peak heap use is bounded by
//! the bytes that actually arrived and by `Limits`, never by a length or count a peer claims.
//!
//! `dcc_feed_hostile.rs` proves the same structurally (every reservation is sized from a count
//! first compared with the bytes present). This binary measures it: it installs a counting
//! global allocator (the test-target opt-out of `docs/04-conventions.md` § 5, ADR-1897) that
//! tracks live bytes and their peak per thread, and checks the peak of each call.
#![cfg(feature = "dcc-feed")]

use regia_agent::dcc_feed::wire::*;

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's live bytes and their peak.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// Runs `f` and returns its result with the peak of extra live bytes on this thread during it.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let (out, peak) = alloc_counter::peak_bytes(f);
    (out, peak as usize)
}

/// Allowance for the small fixed allocations of a call (error strings, a decoded struct).
const SLACK: usize = 64 * 1024;

fn header(msg_type: u16, payload_len: usize, flags: u16) -> FrameHeader {
    FrameHeader {
        len: u32::try_from(payload_len + 8).expect("test frame fits"),
        msg_type,
        flags,
        seq: 1,
    }
}

#[test]
fn a_claimed_length_reserves_only_what_arrives() {
    let limits = Limits::default();
    // A header claiming the full 64 MiB, then 100 bytes and the end of the stream.
    let mut stream = header(msg::MESH, limits.max_message_bytes as usize, 0)
        .to_bytes()
        .to_vec();
    stream.extend_from_slice(&[0u8; 100]);
    let (result, peak) = peak_of(|| FrameReader::new(stream.as_slice()).next(&limits));
    let err = result.expect_err("the stream ends inside the frame");
    assert_eq!(err.code(), "malformed_frame");
    assert!(
        peak < SLACK,
        "peak {peak} bytes for a 64 MiB claim with 100 bytes behind it"
    );

    // One byte over the limit: refused before any payload buffer.
    let over = header(msg::MESH, limits.max_message_bytes as usize + 1, 0).to_bytes();
    let (result, peak) = peak_of(|| FrameReader::new(over.as_slice()).next(&limits));
    assert_eq!(result.expect_err("too large").code(), "frame_too_large");
    assert!(peak < SLACK, "peak {peak}");
}

#[test]
fn hostile_counts_reserve_nothing_large() {
    let u32max = u32::MAX.to_le_bytes();
    let limits = Limits::default();
    let mut cases: Vec<(u16, Vec<u8>)> = Vec::new();

    // mesh: u16::MAX sections, the first claiming u32::MAX elements and bytes.
    let mut mesh = 1u32.to_le_bytes().to_vec();
    mesh.extend_from_slice(&[0u8; 17]);
    mesh.push(0);
    mesh.extend_from_slice(&u16::MAX.to_le_bytes());
    mesh.extend_from_slice(&[1, 0, 0, 0]);
    mesh.extend_from_slice(&u32max);
    mesh.extend_from_slice(&u32max);
    cases.push((msg::MESH, mesh));

    // clip: track count at the limit, nothing behind it.
    let mut clip = 0u16.to_le_bytes().to_vec();
    clip.extend_from_slice(&[0u8; 8]);
    clip.push(0);
    clip.extend_from_slice(&0u32.to_le_bytes());
    clip.extend_from_slice(&limits.max_clip_tracks.to_le_bytes());
    cases.push((msg::CLIP_PUSH, clip));

    // skeleton: u16::MAX joints.
    let mut skel = 1u32.to_le_bytes().to_vec();
    skel.extend_from_slice(&u16::MAX.to_le_bytes());
    cases.push((msg::SKELETON_STATIC, skel));

    // pose: u16::MAX joints.
    let mut pose = 1u32.to_le_bytes().to_vec();
    pose.extend_from_slice(&[0u8; 17]);
    pose.extend_from_slice(&u16::MAX.to_le_bytes());
    cases.push((msg::POSE, pose));

    // raw texture at the largest edge, byte_len u32::MAX.
    let mut tex = 0u16.to_le_bytes().to_vec();
    tex.extend_from_slice(&[0, 2, 0]);
    tex.extend_from_slice(&limits.max_texture_dim.to_le_bytes());
    tex.extend_from_slice(&limits.max_texture_dim.to_le_bytes());
    tex.extend_from_slice(&u32max);
    cases.push((msg::TEXTURE_PUSH, tex));

    // generic declare: u16::MAX channel names.
    let mut decl = 1u32.to_le_bytes().to_vec();
    decl.extend_from_slice(&[1, 0, b'k', 6]);
    decl.extend_from_slice(&0u32.to_le_bytes());
    decl.extend_from_slice(&[0, 0, 0]);
    decl.extend_from_slice(&u16::MAX.to_le_bytes());
    cases.push((msg::ITEM_DECLARE, decl));

    // A string claiming u16::MAX bytes in a hello.
    let mut hello = HELLO_MAGIC.to_vec();
    hello.extend_from_slice(&1u32.to_le_bytes());
    hello.extend_from_slice(&1u32.to_le_bytes());
    hello.extend_from_slice(&u16::MAX.to_le_bytes());
    cases.push((msg::HELLO, hello));

    for (ty, payload) in &cases {
        let h = header(*ty, payload.len(), 0);
        let (result, peak) = peak_of(|| decode(h, payload, &limits));
        assert!(result.is_err(), "type {ty:#x} must be refused");
        assert!(
            peak < SLACK,
            "type {ty:#x}: peak {peak} bytes for a {}-byte payload",
            payload.len()
        );
    }
}

#[test]
fn a_valid_mesh_costs_in_proportion_to_its_bytes() {
    let limits = Limits::default();
    let vertices = 20_000u32;
    let positions: Vec<u8> = (0..vertices * 3).flat_map(|i| (i as f32).to_le_bytes()).collect();
    let indices: Vec<u8> = (0..vertices / 3 * 3).flat_map(u32::to_le_bytes).collect();
    let message = Message::Mesh(MeshUpdate {
        handle: 1,
        info: FrameInfo {
            frame_id: 1,
            source_time_ns: 0,
            timecode: None,
        },
        update_mode: 0,
        sections: vec![
            MeshSection {
                section_id: 1, // positions
                elem: Elem::F32,
                index: 0,
                count: vertices,
                data: positions,
            },
            MeshSection {
                section_id: 6, // indices
                elem: Elem::U32,
                index: 0,
                count: vertices / 3 * 3,
                data: indices,
            },
        ],
    });
    let mut frame = Vec::new();
    encode(&message, 1, &mut frame).expect("encodes");
    let (h, payload) = FrameReader::new(frame.as_slice())
        .next(&limits)
        .expect("reads")
        .expect("one frame");
    let (decoded, peak) = peak_of(|| decode(h, &payload, &limits));
    assert_eq!(decoded.expect("decodes"), message);
    // The decoded copy is one payload's worth; allow two plus the fixed slack.
    assert!(
        peak <= 2 * payload.len() + SLACK,
        "peak {peak} bytes for a {}-byte mesh",
        payload.len()
    );
}

#[test]
fn reassembly_stops_at_the_message_limit() {
    let limits = Limits {
        max_message_bytes: 1 << 20,
        ..Limits::default()
    };
    let chunk = vec![0u8; 64 * 1024];
    let mut reassembler = Reassembler::new();
    let mut refused = None;
    let (_, peak) = peak_of(|| {
        // 32 fragments of 64 KiB claim 2 MiB against a 1 MiB limit.
        for n in 0..32 {
            let h = header(msg::MESH, chunk.len(), FLAG_FRAGMENT_MORE);
            match reassembler.push(h, chunk.clone(), &limits) {
                Ok(_) => {}
                Err(e) => {
                    refused = Some((n, e.code()));
                    break;
                }
            }
        }
    });
    let (at, code) = refused.expect("the run is refused");
    assert_eq!(code, "frame_too_large");
    assert_eq!(at, 16, "the 17th fragment would pass 1 MiB");
    // The buffer (capacity doubling) plus the fragment in hand stay within about twice the limit.
    assert!(
        peak <= 2 * limits.max_message_bytes as usize + 2 * chunk.len() + SLACK,
        "peak {peak} bytes against a {} limit",
        limits.max_message_bytes
    );
}

#[test]
fn mutated_frames_stay_within_a_small_multiple_of_their_size() {
    let limits = Limits::default();
    let mut rng = regia_core::fuzz::FuzzRng::new(0xDCC0_A110);
    let mut worst = 0usize;
    for msg_type in [
        msg::HELLO,
        msg::ITEM_DECLARE,
        msg::TRANSFORM,
        msg::CAMERA,
        msg::LIGHT,
        msg::MESH,
        msg::SKELETON_STATIC,
        msg::POSE,
        msg::CLIP_PUSH,
        msg::TEXTURE_PUSH,
    ] {
        for _ in 0..300 {
            let payload = regia_core::fuzz::mutate_bytes(&[0xFFu8; 96], &mut rng);
            let h = header(msg_type, payload.len(), 0);
            let (_, peak) = peak_of(|| decode(h, &payload, &limits));
            worst = worst.max(peak.saturating_sub(8 * payload.len()));
        }
    }
    assert!(
        worst < SLACK,
        "a mutated frame reserved {worst} bytes beyond 8x its size"
    );
}
