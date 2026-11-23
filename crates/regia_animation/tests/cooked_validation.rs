// SPDX-License-Identifier: MPL-2.0
//! Cooked skeleton and clip files that are structurally valid postcard but semantically bad
//! must fail to load with an error, never reach the solvers. Byte-level corruption of the
//! cooked wrapper is covered by `fuzz.rs`; these are the payload checks.

use regia_animation::cooked::{
    clip_type_tag, cooked_clip_file_version, load_cooked_clip, load_cooked_skeleton,
    migrate_cooked_clip_file, skeleton_type_tag,
};
use regia_animation::pose::translated;
use regia_animation::{AnimationClip, Skeleton};
use regia_asset::{AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file, pack_importer_version};
use regia_core::id::TypePathHash;
use regia_core::math::Vec3;

fn cook(tag: TypePathHash, payload: &[u8]) -> Vec<u8> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: tag,
        asset_id: AssetId::new_random(),
        importer_version: pack_importer_version(1, 1, 0),
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    encode_cooked_file(&header, payload).expect("encode")
}

fn chain3() -> Skeleton {
    Skeleton::from_local_bind(
        vec!["a".into(), "b".into(), "c".into()],
        vec![None, Some(0), Some(1)],
        vec![translated(Vec3::ZERO), translated(Vec3::Y), translated(Vec3::Y)],
    )
    .expect("valid skeleton")
}

fn load_with(mutate: impl FnOnce(&mut Skeleton)) -> bool {
    let mut skeleton = chain3();
    mutate(&mut skeleton);
    let payload = postcard::to_allocvec(&skeleton).expect("serialize");
    load_cooked_skeleton(&cook(skeleton_type_tag(), &payload)).is_ok()
}

#[test]
fn a_valid_skeleton_loads() {
    assert!(load_with(|_| {}));
}

#[test]
fn a_corrupt_skeleton_payload_is_refused_at_load() {
    // A parent that is not an earlier joint (a cycle, a self-parent, a forward reference).
    assert!(!load_with(|s| s.parents[1] = Some(2)), "forward parent");
    assert!(!load_with(|s| s.parents[1] = Some(1)), "self parent");
    assert!(!load_with(|s| s.parents[2] = Some(77)), "parent past the end");
    // Arrays that disagree in length.
    assert!(!load_with(|s| {
        s.parents.pop();
    }));
    assert!(!load_with(|s| {
        s.bind_pose.pop();
    }));
    assert!(!load_with(|s| {
        s.inverse_bind_matrices.pop();
    }));
    // Names.
    assert!(!load_with(|s| s.joint_names[1] = String::new()), "empty name");
    assert!(!load_with(|s| s.joint_names[2] = "a".into()), "duplicate name");
}

#[test]
fn the_error_says_what_is_wrong() {
    let mut skeleton = chain3();
    skeleton.parents[1] = Some(2);
    let payload = postcard::to_allocvec(&skeleton).expect("serialize");
    let error = load_cooked_skeleton(&cook(skeleton_type_tag(), &payload)).expect_err("refused");
    let text = error.to_string();
    assert!(text.contains("invalid skeleton"), "{text}");
}

#[test]
fn clip_file_probes_reject_what_is_not_a_clip() {
    // Garbage and truncations are errors, not panics, at every prefix length.
    let clip = AnimationClip::empty("x", 1.0);
    let payload = clip.to_cooked_payload().expect("payload");
    let file = cook(clip_type_tag(), &payload);
    assert_eq!(
        cooked_clip_file_version(&file).expect("probe"),
        Some(regia_animation::clip::CLIP_FORMAT_VERSION)
    );
    for len in 0..file.len() {
        let cut = &file[..len];
        assert!(cooked_clip_file_version(cut).is_err(), "prefix {len} probed");
        assert!(migrate_cooked_clip_file(cut).is_err(), "prefix {len} migrated");
        assert!(load_cooked_clip(cut).is_err(), "prefix {len} loaded");
    }
    // A skeleton file is not a clip.
    let sk_payload = postcard::to_allocvec(&chain3()).expect("serialize");
    let sk_file = cook(skeleton_type_tag(), &sk_payload);
    assert_eq!(
        migrate_cooked_clip_file(&sk_file),
        Err("not a cooked clip".to_string())
    );
    assert!(load_cooked_clip(&sk_file).is_err());
    assert!(
        load_cooked_skeleton(&file).is_err(),
        "and a clip file is not a skeleton"
    );
    // A clip tag over a skeleton payload fails to decode as a clip.
    let wrong = cook(clip_type_tag(), &sk_payload);
    assert!(load_cooked_clip(&wrong).is_err());
    assert!(cooked_clip_file_version(&wrong).is_err());
    // Migration is idempotent: a current-version file rewrites to the same bytes' meaning.
    let once = migrate_cooked_clip_file(&file).expect("migrate");
    let twice = migrate_cooked_clip_file(&once).expect("migrate again");
    let (_, a) = load_cooked_clip(&once).expect("load");
    let (_, b) = load_cooked_clip(&twice).expect("load");
    assert_eq!(a, b);
    assert_eq!(a, clip);
}
