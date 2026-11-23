// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane: the top layer bit, query-mask intersection and the quaternion/axis-angle
//! bridge for rotations past half a turn.

use regia_core::math::{Quat, Vec3};
use regia_physics::convert::{from_rapier_pose, to_rapier_angvec, to_rapier_pose, to_rapier_rotation};
use regia_physics::{CollisionLayer, LayerConfigError, LayerFile, MAX_LAYERS, PhysicsLayers};

fn names(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("l{i}")).collect()
}

#[test]
fn the_32nd_layer_uses_bit_31_and_collides_only_as_configured() {
    let file = LayerFile {
        layers: names(MAX_LAYERS),
        collides: vec![("l31".into(), "l31".into()), ("l31".into(), "l0".into())],
        queries: vec![("l31".into(), "l0".into()), ("l0".into(), "l31".into())],
    };
    let layers = PhysicsLayers::from_file(&file).expect("32 layers fit");
    assert_eq!(layers.collision_rows[31], (1 << 31) | 1);
    assert_eq!(layers.collision_rows[0], 1 << 31, "symmetric from one direction");
    assert_eq!(layers.query_rows[31], 1);
    assert_eq!(layers.query_rows[0], 1 << 31);
    let top = layers.interaction_groups(&CollisionLayer::new(31));
    let bottom = layers.interaction_groups(&CollisionLayer::new(0));
    let middle = layers.interaction_groups(&CollisionLayer::new(15));
    assert!(top.test(top));
    assert!(top.test(bottom));
    assert!(!top.test(middle));
    // A query from l0 sees l31 only; one from l31 sees l0 only.
    assert!(layers.query_groups(Some(&CollisionLayer::new(0)), None).test(top));
    assert!(
        !layers
            .query_groups(Some(&CollisionLayer::new(0)), None)
            .test(bottom)
    );
    assert!(
        layers
            .query_groups(Some(&CollisionLayer::new(31)), None)
            .test(bottom)
    );
}

#[test]
fn a_requested_mask_is_intersected_with_the_query_row() {
    let file = LayerFile {
        layers: names(4),
        collides: vec![
            ("l0".into(), "l1".into()),
            ("l0".into(), "l2".into()),
            ("l0".into(), "l3".into()),
        ],
        queries: vec![("l0".into(), "l1".into()), ("l0".into(), "l2".into())],
    };
    let layers = PhysicsLayers::from_file(&file).expect("builds");
    let from = CollisionLayer::new(0);
    let l = |i| layers.interaction_groups(&CollisionLayer::new(i));
    // Request l2 and l3: l3 is not in the row, so only l2 is visible.
    let g = layers.query_groups(Some(&from), Some((1 << 2) | (1 << 3)));
    assert!(g.test(l(2)));
    assert!(!g.test(l(3)));
    assert!(!g.test(l(1)), "not requested");
    // An empty request sees nothing; no layer and no mask sees everything that collides.
    assert!(!layers.query_groups(Some(&from), Some(0)).test(l(1)));
    let all = layers.query_groups(None, None);
    assert!(all.test(l(1)) && all.test(l(3)));
    // No layer but a mask: the mask alone decides.
    let only3 = layers.query_groups(None, Some(1 << 3));
    assert!(only3.test(l(3)) && !only3.test(l(2)));
}

#[test]
fn duplicate_pairs_are_idempotent_and_an_empty_file_is_valid() {
    let file = LayerFile {
        layers: names(2),
        collides: vec![("l0".into(), "l1".into()); 5],
        queries: vec![("l0".into(), "l1".into()); 5],
    };
    let once = LayerFile {
        collides: vec![("l0".into(), "l1".into())],
        queries: vec![("l0".into(), "l1".into())],
        ..file.clone()
    };
    assert_eq!(
        PhysicsLayers::from_file(&file).expect("dup"),
        PhysicsLayers::from_file(&once).expect("once")
    );
    let empty = LayerFile {
        layers: Vec::new(),
        collides: Vec::new(),
        queries: Vec::new(),
    };
    let layers = PhysicsLayers::from_file(&empty).expect("no layers is valid");
    assert!(layers.names.is_empty());
    assert!(layers.collision_rows.iter().all(|r| *r == 0));
    assert_eq!(layers.index_of("anything"), None);
    // A pair naming a layer in an empty table is an unknown-layer error.
    let bad = LayerFile {
        collides: vec![("a".into(), "b".into())],
        ..empty
    };
    assert!(matches!(
        PhysicsLayers::from_file(&bad),
        Err(LayerConfigError::UnknownLayer(n)) if n == "a"
    ));
}

#[test]
fn layer_file_parse_rejects_unknown_and_missing_fields() {
    for text in [
        r#"(layers: [], collides: [], queries: [], extra: 1)"#,
        r#"(layers: [], collides: [])"#,
        r#"(layers: ["a"], collides: [("a")], queries: [])"#,
        "",
    ] {
        assert!(
            matches!(LayerFile::parse(text), Err(LayerConfigError::Parse(_))),
            "{text:?}"
        );
    }
}

/// `-q` and `q` are the same rotation; the bridge must produce the same rapier rotation either
/// way, and the scaled-axis vector must rotate vectors the same way (the angle may differ by a
/// full turn).
#[test]
fn double_cover_quaternions_give_the_same_rapier_rotation() {
    let q = Quat::from_axis_angle(Vec3::new(1.0, 2.0, -0.5).normalize(), 3.9); // past half a turn
    let v = Vec3::new(0.3, -1.2, 2.0);
    let expected = q * v;
    for q in [q, -q] {
        let r = to_rapier_rotation(q);
        let rotated = r * rapier3d::math::Vector::new(v.x, v.y, v.z);
        assert!(
            (Vec3::new(rotated.x, rotated.y, rotated.z) - expected).length() < 1e-4,
            "{q:?}"
        );
        let av = to_rapier_angvec(q);
        let back = Quat::from_scaled_axis(Vec3::new(av.x, av.y, av.z));
        assert!((back * v - expected).length() < 1e-4, "angvec of {q:?}");
    }
}

#[test]
fn identity_and_half_turn_angvecs_are_finite() {
    let id = to_rapier_angvec(Quat::IDENTITY);
    assert_eq!((id.x, id.y, id.z), (0.0, 0.0, 0.0));
    let half = to_rapier_angvec(Quat::from_rotation_x(std::f32::consts::PI));
    assert!(half.x.is_finite() && half.y.is_finite() && half.z.is_finite());
    assert!((half.length() - std::f32::consts::PI).abs() < 1e-3);
}

#[test]
fn pose_conversion_keeps_large_translations_exact() {
    let t = Vec3::new(16_777_216.0, -3.5e6, 0.125);
    let (t2, q2) = from_rapier_pose(&to_rapier_pose(t, Quat::IDENTITY));
    assert_eq!(t2, t, "no arithmetic, so bit-exact");
    assert_eq!(q2, Quat::IDENTITY);
}
