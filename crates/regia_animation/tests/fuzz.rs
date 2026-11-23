// SPDX-License-Identifier: MPL-2.0
//! M18 robustness: mutation fuzzing of the `.animgraph.ron` loader. See
//! `docs/evidence/m18/robustness.md`.

// `AnimGraphError` embeds `ron::SpannedError` (`result_large_err` on the test closure).
#![allow(clippy::result_large_err)]

use regia_animation::graph::AnimGraph;
use regia_test_utils::fuzz::{fuzz_loader, fuzz_text_loader};

const N: usize = 3_000;

#[test]
fn animgraph_ron_never_panics() {
    let fixture = include_str!("fixtures/locomotion.animgraph.ron");
    fuzz_text_loader(fixture, 0xE1, N, AnimGraph::from_ron_str);
}

/// A valid cooked clip file (FCKD envelope, one animated translation track).
fn cooked_clip_file() -> Vec<u8> {
    use regia_animation::AnimationClip;
    use regia_animation::clip::{JointTrack, TrackCurve};
    use regia_animation::cooked::clip_type_tag;
    use regia_asset::{
        AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file, pack_importer_version,
    };
    use regia_core::math::Vec3;
    let mut clip = AnimationClip::empty("fuzz", 1.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Default::default()),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let payload = clip.to_cooked_payload().expect("serialize clip");
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: clip_type_tag(),
        asset_id: AssetId::new_random(),
        importer_version: pack_importer_version(1, 1, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, &payload).expect("encode cooked clip")
}

#[test]
fn cooked_clip_never_panics() {
    use regia_animation::cooked::load_cooked_clip;
    let fixture = cooked_clip_file();
    load_cooked_clip(&fixture).expect("the fixture itself loads");
    fuzz_loader(&fixture, 0xE2, N, load_cooked_clip);
}

/// A compressed `cooked-clip` v3 body (ADR-7227) under mutation: the decompressor sees corrupt
/// quantized keys, ranges, key counts and slerp indices and must never panic.
#[test]
fn compressed_cooked_clip_never_panics() {
    use regia_animation::AnimationClip;
    use regia_animation::clip::{JointTrack, TrackCurve};
    use regia_animation::compress::CompressionSettings;
    use regia_animation::cooked::{clip_type_tag, load_cooked_clip};
    use regia_asset::{
        AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file, pack_importer_version,
    };
    use regia_core::math::{Quat, Vec3};
    let mut clip = AnimationClip::empty("fuzz", 1.0);
    let times: Vec<f32> = (0..31).map(|k| k as f32 / 30.0).collect();
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| Vec3::new(t.sin(), 0.0, t * t)).collect(),
            exact_rotation: false,
        },
        rotations: TrackCurve::Animated {
            times: times.clone(),
            values: times.iter().map(|&t| Quat::from_rotation_y(2.0 * t)).collect(),
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let payload = clip
        .to_compressed_cooked_payload(None, &CompressionSettings::default())
        .expect("compressed payload");
    assert!(AnimationClip::compressed_cooked_payload(&payload).is_some());
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: clip_type_tag(),
        asset_id: AssetId::new_random(),
        importer_version: pack_importer_version(1, 1, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    let fixture = encode_cooked_file(&header, &payload).expect("encode cooked clip");
    load_cooked_clip(&fixture).expect("the fixture itself loads");
    fuzz_loader(&fixture, 0xE3, N, load_cooked_clip);
    let fuzz_compressed = |bytes: &[u8]| {
        AnimationClip::compressed_cooked_payload(bytes).ok_or("not a compressed cooked clip payload")
    };
    fuzz_loader(&payload, 0xE4, N, fuzz_compressed);
}

/// Table-driven hostile clip payloads (the bytes after the FCKD envelope): clean errors only.
#[test]
fn cooked_clip_payload_hostile_inputs_are_errors() {
    use regia_animation::AnimationClip;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("magic only", b"FCLP".to_vec()),
        (
            "future version",
            [b"FCLP".as_slice(), &u32::MAX.to_le_bytes()].concat(),
        ),
        ("version zero", [b"FCLP".as_slice(), &0u32.to_le_bytes()].concat()),
        (
            "huge string length",
            [
                b"FCLP".as_slice(),
                &2u32.to_le_bytes(),
                &[0xFF, 0xFF, 0xFF, 0xFF, 0x0F],
            ]
            .concat(),
        ),
        ("huge vec length (v1)", vec![0xFF; 12]),
        (
            "v3 unknown body variant",
            [b"FCLP".as_slice(), &3u32.to_le_bytes(), &[7]].concat(),
        ),
        (
            "v3 compressed body, huge joint count",
            [
                b"FCLP".as_slice(),
                &3u32.to_le_bytes(),
                &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                &[0xFF, 0xFF, 0xFF, 0xFF, 0x0F],
            ]
            .concat(),
        ),
    ];
    for (what, bytes) in cases {
        assert!(AnimationClip::from_cooked_payload(&bytes).is_err(), "{what}");
    }
}
