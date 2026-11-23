// SPDX-License-Identifier: MPL-2.0
//! Compression of awkward clips: empty, one-key and malformed tracks, non-finite values,
//! zero and NaN tolerances, long chains, double-cover rotations, determinism. The error
//! contract is checked through `measure_compression_error`. Fixed-seed data only.

use regia_animation::clip::{JointTrack, MorphTrack, TrackCurve};
use regia_animation::compress::{
    CompressedClip, CompressedQuatTrack, CompressionSettings, QUAT_QUANTIZATION_ERROR, compress_clip,
    decompress_clip, measure_compression_error, pack_quat, rotation_error, unpack_quat,
    vec3_quantization_error,
};
use regia_animation::{AnimationClip, Pose};
use regia_core::math::{Quat, Vec3};

struct Lcg(u64);
impl Lcg {
    fn unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn walk_clip(keys: usize, seed: u64) -> AnimationClip {
    let mut rng = Lcg(seed);
    let times: Vec<f32> = (0..keys).map(|i| i as f32 / 30.0).collect();
    let mut clip = AnimationClip::empty("walk", times[keys - 1]);
    for joint in 0..3u32 {
        let phase = rng.unit() * 6.0;
        clip.tracks.push(JointTrack {
            joint,
            translations: TrackCurve::Animated {
                values: times
                    .iter()
                    .map(|t| Vec3::new((t * 2.0 + phase).sin() * 0.3, 1.0 + t * 0.1, 0.01 * rng.unit()))
                    .collect(),
                times: times.clone(),
                exact_rotation: false,
            },
            rotations: TrackCurve::Animated {
                values: times
                    .iter()
                    .map(|t| Quat::from_rotation_x((t * 3.0 + phase).sin() * 0.8))
                    .collect(),
                times: times.clone(),
                exact_rotation: false,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
    }
    clip
}

#[test]
fn compression_is_deterministic_and_within_tolerance_for_fixed_seeds() {
    let settings = CompressionSettings::default();
    for (keys, seed) in [(2, 1), (3, 2), (31, 3), (200, 4)] {
        let clip = walk_clip(keys, seed);
        let a = compress_clip(&clip, None, &settings);
        let b = compress_clip(&clip, None, &settings);
        assert_eq!(a, b, "{keys} keys: repeated compression is identical");
        assert_eq!(a.to_postcard().expect("bytes"), b.to_postcard().expect("bytes"));
        let error = measure_compression_error(&clip, &a, None);
        assert!(error.within(&settings, 1e-4), "{keys} keys: {error:?}");
        // Decompress, recompress: still within tolerance of the original (no drift blow-up).
        let again = compress_clip(&decompress_clip(&a), None, &settings);
        let drift = measure_compression_error(&clip, &again, None);
        assert!(
            drift.max_position <= 2.0 * settings.position_tolerance + 1e-3,
            "{keys}: {drift:?}"
        );
        assert!(
            drift.max_rotation <= 2.0 * settings.rotation_tolerance + 1e-3,
            "{keys}: {drift:?}"
        );
    }
}

/// ADR-4477 / audit animation1-determinism M7: the rounded f32 angle equals the tolerance,
/// while the relative quaternion's f64 angle is just above it. The key decision must use the
/// arithmetic side of that boundary, not whichever way the host's `atan2f` rounds.
#[test]
fn rotation_tolerance_boundary_uses_the_relative_quaternion() {
    let angle = f32::from_bits(0x3a83_126f);
    let tolerance = f32::from_bits(0x3a83_126f);
    let rotated = Quat::from_rotation_x(angle);
    let actual = 2.0 * f64::from(rotated.x).atan2(f64::from(rotated.w).abs());
    assert!(actual > f64::from(tolerance), "{actual} vs {tolerance}");

    let mut clip = AnimationClip::empty("threshold", 1.0);
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Static(Vec3::ZERO),
        rotations: TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![Quat::IDENTITY, rotated],
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let settings = CompressionSettings {
        rotation_tolerance: tolerance + QUAT_QUANTIZATION_ERROR,
        propagated_tolerance: 1.0,
        ..CompressionSettings::default()
    };
    let compressed = compress_clip(&clip, None, &settings);
    assert!(
        matches!(
            compressed.tracks[0].rotations,
            CompressedQuatTrack::Animated { .. }
        ),
        "the above-tolerance key must survive: {:?}",
        compressed.tracks[0].rotations
    );
}

#[test]
fn degenerate_clips_compress_and_sample_without_panicking() {
    let settings = CompressionSettings::default();
    // No tracks at all.
    let empty = AnimationClip::empty("e", 0.0);
    let c = compress_clip(&empty, None, &settings);
    assert!(c.tracks.is_empty());
    assert_eq!(decompress_clip(&c).tracks.len(), 0);
    let mut pose = Pose::from_identity(2);
    c.sample_pose(0.5, &mut pose);
    assert_eq!(pose, Pose::from_identity(2));
    assert_eq!(measure_compression_error(&empty, &c, None).worst_joint, None);
    // A single-key animated track and a zero-duration clip.
    let mut one = AnimationClip::empty("one", 0.0);
    one.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Animated {
            times: vec![0.0],
            values: vec![Vec3::new(1.0, 2.0, 3.0)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Animated {
            times: vec![0.0],
            values: vec![Quat::from_rotation_y(0.5)],
            exact_rotation: false,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let c = compress_clip(&one, None, &settings);
    let mut pose = Pose::from_identity(1);
    for t in [-1.0, 0.0, 5.0, f32::NAN] {
        c.sample_pose(t, &mut pose);
        assert!(
            (pose.translations[0] - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-3,
            "t {t}"
        );
        assert!(pose.rotations[0].is_finite());
    }
    // A track for a joint beyond the pose is skipped when sampling.
    let mut tiny = Pose::from_identity(0);
    c.sample_pose(0.0, &mut tiny);
    assert!(tiny.is_empty());
}

#[test]
fn extreme_tolerances_never_panic_and_keep_the_contract() {
    let clip = walk_clip(60, 9);
    let all = |p: f32, r: f32, s: f32, prop: f32| CompressionSettings {
        position_tolerance: p,
        rotation_tolerance: r,
        scale_tolerance: s,
        propagated_tolerance: prop,
    };
    for settings in [
        all(0.0, 0.0, 0.0, 0.0),
        all(-1.0, -1.0, -1.0, -1.0),
        all(f32::NAN, f32::NAN, f32::NAN, f32::NAN),
        all(f32::INFINITY, f32::INFINITY, f32::INFINITY, f32::INFINITY),
        all(1e-9, 1e-9, 1e-9, 1e-9),
        all(10.0, 10.0, 10.0, 10.0),
    ] {
        let compressed = compress_clip(&clip, None, &settings);
        let back = decompress_clip(&compressed);
        assert_eq!(back.tracks.len(), clip.tracks.len());
        let mut pose = Pose::from_identity(3);
        compressed.sample_pose(0.4, &mut pose);
        assert!(pose.rotations.iter().all(|q| q.is_finite()), "{settings:?}");
        assert!(pose.translations.iter().all(|t| t.is_finite()), "{settings:?}");
    }
    // Zero tolerances keep every key that is not exactly redundant, so the error is the
    // quantisation floor, far below the default tolerances.
    let tight = compress_clip(&clip, None, &all(0.0, 0.0, 0.0, 0.0));
    let error = measure_compression_error(&clip, &tight, None);
    assert!(
        error.max_position < 1e-3 && error.max_rotation < 1e-3,
        "{error:?}"
    );
}

#[test]
fn malformed_source_tracks_are_survivable() {
    let settings = CompressionSettings::default();
    let mut clip = AnimationClip::empty("bad", 1.0);
    // Fewer values than times, more values than times, empty, unsorted times, NaN values.
    let shapes: Vec<(Vec<f32>, Vec<Vec3>)> = vec![
        (vec![0.0, 0.5, 1.0], vec![Vec3::ZERO, Vec3::X]),
        (vec![0.0, 1.0], vec![Vec3::ZERO, Vec3::X, Vec3::Y]),
        (vec![], vec![]),
        (vec![1.0, 0.0, 0.5], vec![Vec3::ZERO, Vec3::X, Vec3::Y]),
        (
            vec![0.0, 0.5, 1.0],
            vec![Vec3::ZERO, Vec3::splat(f32::NAN), Vec3::Y],
        ),
    ];
    for (joint, (times, values)) in shapes.into_iter().enumerate() {
        clip.tracks.push(JointTrack {
            joint: joint as u32,
            translations: TrackCurve::Animated {
                times,
                values,
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
    }
    let compressed = compress_clip(&clip, None, &settings);
    assert_eq!(compressed.tracks.len(), 5);
    let mut pose = Pose::from_identity(5);
    compressed.sample_pose(0.5, &mut pose);
    let _ = decompress_clip(&compressed);
    let _ = measure_compression_error(&clip, &compressed, None);
}

#[test]
fn quaternion_packing_survives_hostile_input() {
    let mut rng = Lcg(404);
    for _ in 0..500 {
        let q = Quat::from_xyzw(
            rng.unit() - 0.5,
            rng.unit() - 0.5,
            rng.unit() - 0.5,
            rng.unit() - 0.5,
        )
        .normalize();
        let back = unpack_quat(pack_quat(q));
        assert!(rotation_error(q, back) < 2e-4, "{q:?} -> {back:?}");
        assert_eq!(pack_quat(q), pack_quat(q));
        // q and -q are the same rotation and pack to a rotation-equal result.
        assert!(rotation_error(unpack_quat(pack_quat(-q)), back) < 2e-4);
    }
    // Axis-aligned and identity-like quaternions have exact ties for the largest component.
    for q in [
        Quat::IDENTITY,
        -Quat::IDENTITY,
        Quat::from_xyzw(0.5, 0.5, 0.5, 0.5),
        Quat::from_xyzw(-0.5, 0.5, -0.5, 0.5),
    ] {
        assert!(rotation_error(q, unpack_quat(pack_quat(q))) < 2e-4, "{q:?}");
    }
    // Garbage words unpack to a unit quaternion (never NaN), whatever the bits.
    for bits in [
        0u64,
        u64::MAX,
        0x8000_8000_8000_0000,
        0xDEAD_BEEF_CAFE_F00D,
        3 << 48,
    ] {
        let q = unpack_quat(bits);
        assert!(
            q.is_finite() && (q.length() - 1.0).abs() < 1e-3,
            "{bits:#x}: {q:?}"
        );
    }
    assert!(rotation_error(Quat::IDENTITY, Quat::IDENTITY) == 0.0);
    assert!((rotation_error(Quat::IDENTITY, Quat::from_rotation_x(1.0)) - 1.0).abs() < 1e-5);
    assert!(
        rotation_error(Quat::IDENTITY, -Quat::IDENTITY) < 1e-6,
        "double cover"
    );
}

#[test]
fn vec3_quantization_bound_edges() {
    assert_eq!(vec3_quantization_error(&[]), 0.0);
    let flat = vec3_quantization_error(&[Vec3::ONE, Vec3::ONE]);
    assert!(
        flat > 0.0 && flat < 1e-5,
        "a constant track still has a floor: {flat}"
    );
    let wide = vec3_quantization_error(&[Vec3::splat(-100.0), Vec3::splat(100.0)]);
    assert!(wide > 1.0e-3, "a 200 m range quantises to millimetres: {wide}");
}

#[test]
fn morph_weights_clamp_and_sample() {
    let mut clip = AnimationClip::empty("m", 1.0);
    clip.morph_tracks.push(MorphTrack::new(
        0,
        TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![-3.0, 7.0],
            exact_rotation: false,
        },
    ));
    clip.morph_tracks
        .push(MorphTrack::new(1, TrackCurve::Static(f32::NAN)));
    clip.morph_tracks
        .push(MorphTrack::new(2, TrackCurve::Static(0.25)));
    let compressed = compress_clip(&clip, None, &CompressionSettings::default());
    let mut out = Vec::new();
    compressed.sample_morphs(0.0, &mut out);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0], (0, 0.0), "weights clamp to 0..=1");
    assert!(
        out.iter()
            .all(|&(_, w)| w.is_finite() && (0.0..=1.0).contains(&w)),
        "{out:?}"
    );
    assert!((out[2].1 - 0.25).abs() < 1e-4);
    compressed.sample_morphs(f32::NAN, &mut out);
    assert!(out.iter().all(|&(_, w)| w.is_finite()));
    compressed.sample_morphs(1.0, &mut out);
    assert_eq!(out[0], (0, 1.0));
}

#[test]
fn postcard_round_trip_and_garbage() {
    let compressed = compress_clip(&walk_clip(40, 5), None, &CompressionSettings::default());
    let bytes = compressed.to_postcard().expect("bytes");
    assert_eq!(CompressedClip::from_postcard(&bytes).expect("decode"), compressed);
    for len in [0, 1, bytes.len() / 2, bytes.len() - 1] {
        assert!(
            CompressedClip::from_postcard(&bytes[..len]).is_err(),
            "prefix {len}"
        );
    }
    assert!(compressed.compressed_size() < compressed.raw_equivalent_size());
}

#[test]
fn a_corrupt_compressed_clip_samples_without_panicking() {
    use regia_animation::compress::{CompressedJointTrack, CompressedQuatTrack, CompressedVec3Track};
    // Key and time counts that disagree, and animated tracks with no keys at all, as a damaged
    // file could decode to.
    let corrupt = |translations, rotations, scales| CompressedClip {
        name: "c".into(),
        duration: 1.0,
        sample_rate: 30.0,
        tracks: vec![CompressedJointTrack {
            joint: 0,
            translations,
            rotations,
            scales,
        }],
        morph_tracks: vec![],
        notifies: vec![],
        curves: vec![],
        property_tracks: vec![],
        root_motion: None,
        node_animation: None,
    };
    let vec3s = [
        CompressedVec3Track::Animated {
            times: vec![],
            keys: vec![],
            min: Vec3::ZERO,
            max: Vec3::ONE,
        },
        CompressedVec3Track::Animated {
            times: vec![],
            keys: vec![[0, 0, 0]],
            min: Vec3::ZERO,
            max: Vec3::ONE,
        },
        CompressedVec3Track::Animated {
            times: vec![0.0, 1.0, 2.0],
            keys: vec![[0; 3], [9; 3]],
            min: Vec3::ZERO,
            max: Vec3::ONE,
        },
        CompressedVec3Track::Animated {
            times: vec![0.0],
            keys: vec![[0; 3], [9; 3], [7; 3]],
            min: Vec3::ZERO,
            max: Vec3::ONE,
        },
    ];
    let quats = [
        CompressedQuatTrack::Animated {
            times: vec![],
            keys: vec![],
        },
        CompressedQuatTrack::Animated {
            times: vec![0.0],
            keys: vec![1, 2, 3],
        },
        CompressedQuatTrack::Animated {
            times: vec![0.0, 1.0, 2.0],
            keys: vec![1, 2],
        },
    ];
    for v in &vec3s {
        for q in &quats {
            let clip = corrupt(v.clone(), q.clone(), v.clone());
            for time in [-1.0, 0.0, 0.5, 5.0, f32::NAN] {
                let mut pose = Pose::from_identity(1);
                clip.sample_pose(time, &mut pose);
                assert!(pose.rotations[0].is_finite() && pose.translations[0].is_finite());
            }
            let _ = decompress_clip(&clip);
        }
    }
}
