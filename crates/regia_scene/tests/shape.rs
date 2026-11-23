// SPDX-License-Identifier: MPL-2.0
//! Spec 64 § 9 test 14: `Shape::signed_distance` of box, sphere and capsule matches independent
//! analytic forms at 1 000 seeded points within 1e-5 m.

use regia_core::math::Vec3;
use regia_scene::shape::{Shape, ShapeKind};

struct Lcg(u64);

impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / (1u64 << 24) as f32
    }
    fn point(&mut self, r: f32) -> Vec3 {
        Vec3::new(self.next_f32(), self.next_f32(), self.next_f32()) * (2.0 * r) - Vec3::splat(r)
    }
}

/// Box distance by clamping (outside) and the nearest face (inside).
fn box_analytic(p: Vec3, h: Vec3) -> f32 {
    let c = p.clamp(-h, h);
    if c != p {
        (p - c).length()
    } else {
        -(h.x - p.x.abs()).min(h.y - p.y.abs()).min(h.z - p.z.abs())
    }
}

/// Capsule distance: distance to the core segment by projection, minus the radius.
fn capsule_analytic(p: Vec3, r: f32, hh: f32) -> f32 {
    let (a, b) = (Vec3::new(0.0, -hh, 0.0), Vec3::new(0.0, hh, 0.0));
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.dot(ab)).clamp(0.0, 1.0);
    (p - (a + ab * t)).length() - r
}

#[test]
fn signed_distances_match_analytic_forms() {
    let h = Vec3::new(1.5, 0.75, 2.0);
    let bx = Shape::new(ShapeKind::Box { half_extents: h }).prepare();
    let sp = Shape::new(ShapeKind::Sphere { radius: 1.25 }).prepare();
    let cp = Shape::new(ShapeKind::Capsule {
        radius: 0.6,
        half_height: 1.1,
    })
    .prepare();
    let mut rng = Lcg(0x5eed_0014);
    for _ in 0..1000 {
        let p = rng.point(3.0);
        let (a, b) = (bx.signed_distance(p), box_analytic(p, h));
        assert!((a - b).abs() <= 1e-5, "box {p:?}: {a} vs {b}");
        let (a, b) = (sp.signed_distance(p), p.length() - 1.25);
        assert!((a - b).abs() <= 1e-5, "sphere {p:?}: {a} vs {b}");
        let (a, b) = (cp.signed_distance(p), capsule_analytic(p, 0.6, 1.1));
        assert!((a - b).abs() <= 1e-5, "capsule {p:?}: {a} vs {b}");
    }
}

/// Spec 64 § 2.4: a hull uses at most 256 points (the first 256 kept), so a 257th point far
/// outside does not grow the region.
#[test]
fn hulls_keep_the_first_256_points() {
    let mut pts: Vec<Vec3> = (0..256)
        .map(|i| {
            let a = i as f32 / 256.0 * std::f32::consts::TAU;
            Vec3::new(a.cos(), if i % 2 == 0 { -1.0 } else { 1.0 }, a.sin())
        })
        .collect();
    let base = Shape::new(ShapeKind::Hull { points: pts.clone() });
    pts.push(Vec3::new(50.0, 0.0, 0.0));
    let capped = Shape::new(ShapeKind::Hull { points: pts });
    let far = Vec3::new(20.0, 0.0, 0.0);
    assert!(!capped.contains(far), "the 257th point is dropped");
    assert_eq!(capped.signed_distance(far), base.signed_distance(far));
    assert!(capped.contains(Vec3::new(0.5, 0.0, 0.0)));
}
