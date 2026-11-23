// SPDX-License-Identifier: MPL-2.0
//! Clips compressed at cook: the `cooked-clip` v3 payload (`docs/specs/17-animation.md` § 1, ADR-7227).
//!
//! A v3 payload holds either the clip losslessly (what `to_cooked_payload` and the v2 migrator
//! write) or its joint tracks key-reduced and quantized by `compress` (what the cook writes). These
//! tests are the lane's acceptance: a compressed v3 clip decodes within the spec's error bound of its
//! source and at most 25 % of the raw payload, a slerp track keeps its flag, and a v2 clip still
//! loads, and migrates, unchanged.

use regia_animation::clip::{CLIP_FORMAT_VERSION, JointTrack, MorphTrack, TrackCurve};
use regia_animation::compress::{CompressionSettings, measure_compression_error};
use regia_animation::cooked::{
    clip_type_tag, cooked_clip_file_version, load_cooked_clip, migrate_cooked_clip_file,
};
use regia_animation::pose::translated;
use regia_animation::skeleton::Skeleton;
use regia_animation::{AnimationClip, Notify};
use regia_asset::{AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file, pack_importer_version};
use regia_core::math::{Quat, Vec3};

/// A 12-joint chain, 0.25 m per segment (a 2.75 m subtree under the root).
fn chain(n: usize) -> Skeleton {
    Skeleton::from_local_bind(
        (0..n).map(|i| format!("j{i}")).collect(),
        (0..n).map(|i| i.checked_sub(1).map(|p| p as u32)).collect(),
        (0..n)
            .map(|i| translated(Vec3::new(0.0, if i == 0 { 0.0 } else { 0.25 }, 0.0)))
            .collect(),
    )
    .expect("valid chain")
}

/// Smooth motion on every joint at 60 keys/s for 3 s, one constant scale, a morph track and a
/// notify (which must survive unchanged).
fn dense_clip(joints: usize) -> AnimationClip {
    let keys = 181usize;
    let mut clip = AnimationClip::empty("walk", 3.0);
    clip.sample_rate = 60.0;
    for joint in 0..joints {
        let mut times = Vec::with_capacity(keys);
        let mut pos = Vec::with_capacity(keys);
        let mut rot = Vec::with_capacity(keys);
        for k in 0..keys {
            let t = k as f32 / 60.0;
            let phase = t * std::f32::consts::TAU / 1.5 + joint as f32 * 0.4;
            times.push(t);
            pos.push(Vec3::new(
                0.05 * phase.sin(),
                if joint == 0 { 0.0 } else { 0.25 },
                0.02 * (2.0 * phase).cos(),
            ));
            rot.push(Quat::from_rotation_x(0.5 * phase.sin()) * Quat::from_rotation_z(0.2 * phase.cos()));
        }
        clip.tracks.push(JointTrack {
            joint: joint as u32,
            translations: TrackCurve::Animated {
                times: times.clone(),
                values: pos,
                exact_rotation: false,
            },
            rotations: TrackCurve::Animated {
                times,
                values: rot,
                exact_rotation: false,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
    }
    clip.morph_tracks.push(MorphTrack::new(
        0,
        TrackCurve::Animated {
            times: vec![0.0, 1.5, 3.0],
            values: vec![0.0, 1.25, -0.1],
            exact_rotation: false,
        },
    ));
    clip.notifies.push(Notify {
        time: 0.75,
        name: "Footstep_L".into(),
        payload: Some("(volume: 0.5)".into()),
    });
    clip
}

fn cooked_file(payload: &[u8]) -> Vec<u8> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: clip_type_tag(),
        asset_id: AssetId::new_random(),
        importer_version: pack_importer_version(1, 1, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, payload).expect("encode")
}

/// The v2 payload byte for byte: magic, version 2, then the postcard clip.
fn v2_payload(clip: &AnimationClip) -> Vec<u8> {
    let mut out = b"FCLP".to_vec();
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&postcard::to_allocvec(clip).expect("postcard"));
    out
}

#[test]
fn the_cooked_clip_format_is_version_three() {
    assert_eq!(CLIP_FORMAT_VERSION, 3);
}

#[test]
fn a_compressed_v3_clip_decodes_within_the_spec_error_bound_of_its_source() {
    let skeleton = chain(12);
    let source = dense_clip(12);
    let settings = CompressionSettings::default();
    let payload = source
        .to_compressed_cooked_payload(Some(&skeleton), &settings)
        .expect("compressed payload");
    assert_eq!(AnimationClip::cooked_payload_version(&payload), 3);
    let compressed = AnimationClip::compressed_cooked_payload(&payload).expect("a compressed body");
    let (_, decoded) = load_cooked_clip(&cooked_file(&payload)).expect("the cooked file loads");

    // The error the decoded clip has against its source, per joint: inside the spec's tolerances
    // (`CompressionError::within`, the bound `compress`'s own tests hold it to).
    let error = measure_compression_error(&source, &compressed, Some(&skeleton));
    assert!(error.samples > 0);
    assert!(error.within(&settings, 5e-5), "{error:?} against {settings:?}");

    // The decoded clip samples exactly what the compressed body samples.
    let mut a = regia_animation::pose::Pose::from_identity(skeleton.len());
    let mut b = a.clone();
    for k in 0..=90 {
        let t = k as f32 / 30.0;
        decoded.sample_pose(t, &mut a);
        compressed.sample_pose(t, &mut b);
        for j in 0..skeleton.len() {
            assert!(
                (a.translations[j] - b.translations[j]).length() < 1e-6,
                "t {t} joint {j}"
            );
            assert!(
                a.rotations[j].dot(b.rotations[j]).abs() > 1.0 - 1e-6,
                "t {t} joint {j}"
            );
        }
    }
    // Everything that is not a joint track comes back exactly.
    assert_eq!(decoded.name, source.name);
    assert_eq!(decoded.duration, source.duration);
    assert_eq!(decoded.notifies, source.notifies);
    assert_eq!(
        decoded.morph_tracks, source.morph_tracks,
        "morph weights stay f32 (they may leave 0..=1)"
    );
    assert_eq!(decoded.tracks.len(), source.tracks.len());
}

#[test]
fn a_compressed_v3_clip_is_at_most_a_quarter_of_the_raw_payload() {
    let skeleton = chain(12);
    let source = dense_clip(12);
    let raw = source.to_cooked_payload().expect("raw payload");
    let packed = source
        .to_compressed_cooked_payload(Some(&skeleton), &CompressionSettings::default())
        .expect("compressed payload");
    assert!(
        packed.len() * 4 <= raw.len(),
        "compressed {} bytes vs raw {} bytes ({:.1} %)",
        packed.len(),
        raw.len(),
        100.0 * packed.len() as f64 / raw.len() as f64
    );
}

#[test]
fn compression_is_reproducible_byte_for_byte() {
    let skeleton = chain(12);
    let source = dense_clip(12);
    let settings = CompressionSettings::default();
    let a = source
        .to_compressed_cooked_payload(Some(&skeleton), &settings)
        .expect("a");
    let b = source
        .to_compressed_cooked_payload(Some(&skeleton), &settings)
        .expect("b");
    assert_eq!(a, b);
}

#[test]
fn a_slerp_rotation_track_keeps_its_flag_and_every_key() {
    let skeleton = chain(2);
    let mut source = dense_clip(2);
    let TrackCurve::Animated {
        exact_rotation,
        times,
        ..
    } = &mut source.tracks[1].rotations
    else {
        panic!("animated");
    };
    *exact_rotation = true;
    let key_count = times.len();
    let payload = source
        .to_compressed_cooked_payload(Some(&skeleton), &CompressionSettings::default())
        .expect("payload");
    let decoded = AnimationClip::from_cooked_payload(&payload).expect("decodes");
    match &decoded.tracks[1].rotations {
        TrackCurve::Animated {
            exact_rotation,
            times,
            ..
        } => {
            assert!(*exact_rotation, "the slerp flag survives the cook");
            assert_eq!(
                times.len(),
                key_count,
                "a slerp track is quantized, not key-reduced"
            );
        }
        TrackCurve::Static(_) => panic!("the slerp track was collapsed"),
    }
    match &decoded.tracks[0].rotations {
        TrackCurve::Animated { exact_rotation, .. } => assert!(!*exact_rotation),
        TrackCurve::Static(_) => {}
    }
}

#[test]
fn the_lossless_v3_payload_round_trips_exactly() {
    let source = dense_clip(3);
    let payload = source.to_cooked_payload().expect("payload");
    assert_eq!(AnimationClip::cooked_payload_version(&payload), 3);
    assert!(AnimationClip::compressed_cooked_payload(&payload).is_none());
    assert_eq!(
        AnimationClip::from_cooked_payload(&payload).expect("decodes"),
        source
    );
}

#[test]
fn a_v2_clip_still_loads_and_migrates_to_v3_unchanged() {
    let source = dense_clip(3);
    let file = cooked_file(&v2_payload(&source));
    assert_eq!(cooked_clip_file_version(&file).expect("probe"), Some(2));
    let (_, loaded) = load_cooked_clip(&file).expect("a v2 file loads");
    assert_eq!(loaded, source);

    let migrated = migrate_cooked_clip_file(&file).expect("migrate");
    assert_eq!(cooked_clip_file_version(&migrated).expect("probe"), Some(3));
    let (_, again) = load_cooked_clip(&migrated).expect("the migrated file loads");
    assert_eq!(
        again, source,
        "the migrator does not compress: old content plays exactly as before"
    );
}
