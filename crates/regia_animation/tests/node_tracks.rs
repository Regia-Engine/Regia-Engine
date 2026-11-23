// SPDX-License-Identifier: MPL-2.0
//! Node-track format, validation and playback regressions (ADR-8157).

use regia_animation::AnimationClip;
use regia_animation::clip::MorphNode;
use regia_animation::node_tracks::{
    NODE_TRACKS_VERSION, NodeAnimation, NodeCurve, NodeInterpolation, NodeTrack,
};
use regia_core::math::{Mat4, Quat, Vec3};

fn fixture() -> NodeAnimation {
    NodeAnimation {
        version: NODE_TRACKS_VERSION,
        frame: Mat4::IDENTITY,
        start: 0.0,
        end: 1.0,
        nodes: vec![NodeTrack {
            node: MorphNode {
                index: 7,
                name: "door".into(),
            },
            parent: None,
            is_joint: false,
            bind: Default::default(),
            matrix: None,
            translations: Some(NodeCurve {
                times: vec![0.0, 1.0],
                values: vec![Vec3::ZERO, Vec3::X],
                interpolation: NodeInterpolation::Linear,
            }),
            rotations: None,
            scales: None,
        }],
    }
}

#[test]
fn node_payload_validates_versions_hierarchy_and_truncations() {
    let animation = fixture();
    let bytes = animation.to_payload().expect("encode");
    assert_eq!(NodeAnimation::from_payload(&bytes).expect("decode"), animation);
    for end in 0..bytes.len() {
        assert!(
            NodeAnimation::from_payload(&bytes[..end]).is_err(),
            "prefix {end}"
        );
    }
    let mut future = animation.clone();
    future.version += 1;
    let bytes = postcard::to_allocvec(&future).expect("future bytes");
    assert!(
        NodeAnimation::from_payload(&bytes)
            .expect_err("future")
            .contains("version")
    );
    let mut cycle = animation.clone();
    cycle.nodes[0].parent = Some(0);
    assert!(cycle.validate().is_err());
    let mut samples = Vec::new();
    assert!(cycle.sample(0.5, &mut samples).is_err());
    assert!(samples.is_empty());
    let mut duplicate = animation;
    duplicate.nodes.push(duplicate.nodes[0].clone());
    assert!(duplicate.validate().is_err());
}

#[test]
fn node_payload_rejects_bad_curves_and_animated_matrices() {
    let mut animation = fixture();
    animation.nodes[0].matrix = Some(Mat4::IDENTITY);
    assert!(animation.validate().is_err());
    animation.nodes[0].matrix = None;
    animation.nodes[0].translations.as_mut().expect("curve").times[1] = f32::INFINITY;
    assert!(animation.validate().is_err());
    let bad = NodeCurve {
        times: vec![0.0],
        values: vec![Quat::from_xyzw(0., 0., 0., 0.)],
        interpolation: NodeInterpolation::Linear,
    };
    assert!(bad.validate().is_err());
    assert_eq!(bad.sample(0.0), None);
}

#[test]
fn sampling_reuses_output_and_clamps_special_clocks() {
    let animation = fixture();
    let mut samples = Vec::new();
    animation.sample(0.5, &mut samples).expect("sample");
    let storage = samples.as_ptr();
    for (time, x) in [
        (f32::NAN, 0.),
        (f32::NEG_INFINITY, 0.),
        (f32::INFINITY, 1.),
        (0.75, 0.75),
    ] {
        animation.sample(time, &mut samples).expect("sample");
        assert_eq!(samples.as_ptr(), storage);
        assert_eq!(samples[0].world.w_axis.x, x);
    }
}

#[test]
fn a_slice_after_the_node_body_ends_keeps_its_held_pose() {
    let mut clip = AnimationClip::empty("long_clip", 2.0);
    let mut nodes = fixture();
    nodes.end = 0.5;
    clip.node_animation = Some(nodes);
    let sliced = clip.slice("held_tail", 1.0, 1.5);
    let mut original = Vec::new();
    let mut tail = Vec::new();
    for time in [0.0, 0.25, 0.5] {
        clip.node_animation
            .as_ref()
            .expect("body")
            .sample(1.0 + time, &mut original)
            .expect("original");
        sliced
            .node_animation
            .as_ref()
            .expect("sliced body")
            .sample(time, &mut tail)
            .expect("slice");
        assert_eq!(tail[0].world, original[0].world);
        assert_eq!(tail[0].world.w_axis.x, 0.5);
    }
}

#[test]
fn static_matrix_shear_survives_composition() {
    let mut animation = fixture();
    let mut parent = animation.nodes[0].clone();
    parent.node.index = 0;
    parent.translations = None;
    let mut shear = Mat4::IDENTITY;
    shear.y_axis.x = 0.5;
    parent.matrix = Some(shear);
    animation.nodes.insert(0, parent);
    animation.nodes[1].parent = Some(0);
    animation.validate().expect("valid");
    let mut samples = Vec::new();
    animation.sample(0.5, &mut samples).expect("sample");
    assert_eq!(samples[1].world, shear * Mat4::from_translation(Vec3::X * 0.5));
    assert_eq!(samples[1].bind_world, shear);
}

#[test]
fn cooked_migration_retains_node_samples_and_rejects_future_body() {
    use regia_asset::{AssetId, COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file};
    let mut clip = AnimationClip::empty("door", 1.0);
    clip.node_animation = Some(fixture());
    let payload = clip.to_cooked_payload().expect("payload");
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: regia_animation::cooked::clip_type_tag(),
        asset_id: AssetId::new_random(),
        importer_version: 1,
        dependencies: vec![],
        payload_len: payload.len() as u64,
    };
    let bytes = encode_cooked_file(&header, &payload).expect("file");
    let migrated = regia_animation::cooked::migrate_cooked_clip_file(&bytes).expect("migrate");
    assert_eq!(
        regia_animation::cooked::load_cooked_clip(&migrated)
            .expect("load")
            .1,
        clip
    );
    // The final node body is the standalone postcard body; change only its version.
    let nodes = clip
        .node_animation
        .as_ref()
        .expect("nodes")
        .to_payload()
        .expect("body");
    let mut future = payload.clone();
    let offset = future.len() - nodes.len();
    assert_eq!(&future[offset..], nodes);
    future[offset] = 2;
    assert!(AnimationClip::from_cooked_payload(&future).is_err());
    let mut invalid = clip;
    invalid.node_animation.as_mut().expect("nodes").version = 2;
    assert!(invalid.to_cooked_payload().is_err());
}

#[test]
fn node_tracks_never_panic_under_seeded_mutation() {
    let bytes = fixture().to_payload().expect("body");
    regia_test_utils::fuzz::fuzz_loader(&bytes, 0x8157, 3000, |bytes| {
        let nodes = NodeAnimation::from_payload(bytes)?;
        let mut out = Vec::new();
        let _ = nodes.sample(0.5, &mut out);
        nodes.to_payload()
    });
}

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

#[test]
fn node_tracks_corpus_loads_round_trips_and_refuses_future() {
    let load = |bytes: &[u8]| NodeAnimation::from_payload(bytes)?.to_payload();
    let read = |bytes: &[u8]| NodeAnimation::from_payload(bytes).ok().map(|a| a.version);
    let patch = |bytes: &[u8], version| {
        let mut node = NodeAnimation::from_payload(bytes).ok()?;
        node.version = version;
        postcard::to_allocvec(&node).ok()
    };
    let problems =
        format_corpus_support::check_format_with("node-tracks", NODE_TRACKS_VERSION, &load, &read, &patch);
    assert!(problems.is_empty(), "{problems:#?}");
}
