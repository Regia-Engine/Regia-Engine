// SPDX-License-Identifier: MPL-2.0
//! Edge cases of `math::bounds`: empty and inverted boxes, rays that graze, start inside or point away,
//! denormal ray directions, and the 2D `Rect`.

use regia_core::glam::{Affine3A, Quat, Vec2, Vec3, vec2, vec3};
use regia_core::math::{Aabb, Plane, Ray, Rect, Sphere};

fn unit_box() -> Aabb {
    Aabb::new(Vec3::ZERO, Vec3::ONE)
}

#[test]
fn empty_boxes_intersect_and_contain_nothing() {
    let big = Aabb::new(Vec3::splat(-5.0), Vec3::splat(5.0));
    assert!(!Aabb::EMPTY.intersects(&big) && !big.intersects(&Aabb::EMPTY));
    assert!(!Aabb::EMPTY.intersects(&Aabb::EMPTY));
    // An inverted box that is not `EMPTY` is empty as well, even when it straddles a real box.
    let inverted = Aabb::new(Vec3::ONE, Vec3::ZERO);
    assert!(inverted.is_empty());
    assert!(!inverted.intersects(&big), "an empty box overlaps nothing");
    assert!(!big.intersects(&inverted));
    assert!(!inverted.contains_point(Vec3::splat(0.5)));
}

#[test]
fn empty_is_the_identity_of_union_and_extend() {
    let b = unit_box();
    assert_eq!(Aabb::EMPTY.union(&b), b);
    assert_eq!(b.union(&Aabb::EMPTY), b);
    assert!(Aabb::EMPTY.union(&Aabb::EMPTY).is_empty());
    let mut e = Aabb::EMPTY;
    e.extend(vec3(1.0, 2.0, 3.0));
    assert_eq!(e, Aabb::new(vec3(1.0, 2.0, 3.0), vec3(1.0, 2.0, 3.0)));
    assert!(!e.is_empty(), "a single point is a valid zero-volume box");
    assert_eq!(Aabb::from_points([]), None);
    assert_eq!(Aabb::default(), Aabb::EMPTY);
}

#[test]
fn touching_boxes_intersect_but_a_gap_does_not() {
    let a = unit_box();
    let touching = Aabb::new(vec3(1.0, 0.0, 0.0), vec3(2.0, 1.0, 1.0));
    let apart = Aabb::new(vec3(1.0001, 0.0, 0.0), vec3(2.0, 1.0, 1.0));
    assert!(a.intersects(&touching) && touching.intersects(&a));
    assert!(!a.intersects(&apart));
    assert!(a.contains(&a), "a box contains itself");
    assert!(!a.contains(&Aabb::EMPTY) && !Aabb::EMPTY.contains(&a));
}

#[test]
fn transformed_box_stays_empty_and_tracks_a_rotation() {
    assert!(
        Aabb::EMPTY
            .transformed(&Affine3A::from_rotation_y(1.0))
            .is_empty()
    );
    let turned = unit_box().transformed(&Affine3A::from_quat(Quat::from_rotation_z(
        std::f32::consts::FRAC_PI_2,
    )));
    assert!((turned.min - vec3(-1.0, 0.0, 0.0)).abs().max_element() < 1e-6);
    assert!((turned.max - vec3(0.0, 1.0, 1.0)).abs().max_element() < 1e-6);
    // A negative scale flips the corners but the box stays well ordered.
    let flipped = unit_box().transformed(&Affine3A::from_scale(vec3(-2.0, 1.0, 1.0)));
    assert_eq!((flipped.min.x, flipped.max.x), (-2.0, 0.0));
}

#[test]
fn sphere_from_an_empty_box_is_finite_and_touching_spheres_intersect() {
    let s = Sphere::from_aabb(&Aabb::EMPTY);
    assert!(s.center.is_finite() && s.radius == 0.0);
    let a = Sphere::new(Vec3::ZERO, 1.0);
    assert!(
        a.intersects_sphere(&Sphere::new(vec3(2.0, 0.0, 0.0), 1.0)),
        "touching counts"
    );
    assert!(!a.intersects_sphere(&Sphere::new(vec3(2.01, 0.0, 0.0), 1.0)));
    assert!(!a.intersects_aabb(&Aabb::EMPTY));
    assert!(
        a.intersects_aabb(&Aabb::new(vec3(1.0, -1.0, -1.0), vec3(3.0, 1.0, 1.0))),
        "touching counts"
    );
    let boxed = Sphere::from_aabb(&Aabb::new(Vec3::splat(-1.0), Vec3::splat(1.0)));
    assert!((boxed.radius - 3f32.sqrt()).abs() < 1e-6);
}

#[test]
fn plane_construction_rejects_degenerate_input() {
    assert_eq!(Plane::from_point_normal(Vec3::ONE, Vec3::ZERO), None);
    assert_eq!(Plane::from_point_normal(Vec3::ONE, Vec3::NAN), None);
    assert_eq!(Plane::from_vec4(regia_core::glam::Vec4::ZERO), None);
    assert_eq!(
        Plane::from_vec4(regia_core::glam::Vec4::new(f32::NAN, 0.0, 0.0, 1.0)),
        None
    );
    assert_eq!(
        Plane::from_vec4(regia_core::glam::Vec4::new(f32::INFINITY, 0.0, 0.0, 1.0)),
        None
    );
    // A scaled equation normalises to the same plane.
    let p = Plane::from_vec4(regia_core::glam::Vec4::new(0.0, 4.0, 0.0, -8.0)).expect("valid");
    assert_eq!((p.normal, p.d), (Vec3::Y, -2.0));
    assert_eq!(p.signed_distance(vec3(0.0, 5.0, 0.0)), 3.0);
}

#[test]
fn ray_from_a_zero_direction_hits_nothing() {
    let r = Ray::new(Vec3::ONE, Vec3::ZERO);
    assert_eq!(r.direction, Vec3::ZERO);
    assert_eq!(r.at(10.0), Vec3::ONE);
    assert_eq!(r.intersect_aabb(&unit_box()), None);
    assert_eq!(r.intersect_sphere(&Sphere::new(Vec3::ONE, 2.0)), None);
    assert_eq!(
        r.intersect_plane(&Plane {
            normal: Vec3::Y,
            d: 0.0
        }),
        None
    );
}

#[test]
fn ray_aabb_inside_behind_grazing_and_parallel() {
    let b = unit_box();
    assert_eq!(
        Ray::new(Vec3::splat(0.5), Vec3::X).intersect_aabb(&b),
        Some(0.0),
        "origin inside"
    );
    assert_eq!(
        Ray::new(vec3(-3.0, 0.5, 0.5), Vec3::X).intersect_aabb(&b),
        Some(3.0)
    );
    assert_eq!(
        Ray::new(vec3(-3.0, 0.5, 0.5), Vec3::NEG_X).intersect_aabb(&b),
        None,
        "box behind the origin"
    );
    assert_eq!(
        Ray::new(vec3(-3.0, 1.0, 0.5), Vec3::X).intersect_aabb(&b),
        Some(3.0),
        "grazing the top face"
    );
    assert_eq!(
        Ray::new(vec3(-3.0, 1.0001, 0.5), Vec3::X).intersect_aabb(&b),
        None,
        "just over it"
    );
    assert_eq!(
        Ray::new(vec3(-3.0, 0.5, 0.5), Vec3::Y).intersect_aabb(&b),
        None,
        "parallel to a slab, outside it"
    );
    assert_eq!(
        Ray::new(vec3(-3.0, 0.5, 0.5), Vec3::X).intersect_aabb(&Aabb::EMPTY),
        None
    );
    // A degenerate (flat) box is still hit.
    let flat = Aabb::new(vec3(0.0, 0.0, 0.5), vec3(1.0, 1.0, 0.5));
    assert_eq!(
        Ray::new(vec3(0.5, 0.5, -2.0), Vec3::Z).intersect_aabb(&flat),
        Some(2.5)
    );
}

#[test]
fn ray_aabb_with_a_denormal_direction_component_on_a_slab_plane() {
    // The y component is a denormal: its reciprocal overflows to infinity, and the origin sits exactly
    // on the y = 0 face, so `0 * inf` used to turn the slab distance into NaN and the hit into `inf`.
    let dir = vec3(1.0, 1.0e-40, 0.0);
    let r = Ray::new(vec3(-5.0, 0.0, 0.5), dir);
    let t = r
        .intersect_aabb(&unit_box())
        .expect("the ray runs along the bottom face");
    assert!(t.is_finite(), "t = {t}");
    assert!((t - 5.0).abs() < 1e-4, "t = {t}");
}

#[test]
fn ray_sphere_inside_tangent_and_behind() {
    let s = Sphere::new(vec3(0.0, 0.0, 5.0), 1.0);
    assert_eq!(Ray::new(Vec3::ZERO, Vec3::Z).intersect_sphere(&s), Some(4.0));
    assert_eq!(
        Ray::new(vec3(0.0, 0.0, 5.0), Vec3::Z).intersect_sphere(&s),
        Some(0.0),
        "inside"
    );
    assert_eq!(
        Ray::new(Vec3::ZERO, Vec3::NEG_Z).intersect_sphere(&s),
        None,
        "behind"
    );
    assert_eq!(
        Ray::new(vec3(1.0, 0.0, 0.0), Vec3::Z).intersect_sphere(&s),
        Some(5.0),
        "tangent"
    );
    assert_eq!(Ray::new(vec3(1.01, 0.0, 0.0), Vec3::Z).intersect_sphere(&s), None);
    // A sphere the ray starts beyond (origin outside, sphere behind) is not hit.
    assert_eq!(Ray::new(vec3(0.0, 0.0, 7.0), Vec3::Z).intersect_sphere(&s), None);
}

#[test]
fn ray_plane_parallel_behind_and_front() {
    let floor = Plane {
        normal: Vec3::Y,
        d: 0.0,
    };
    assert_eq!(
        Ray::new(vec3(0.0, 3.0, 0.0), Vec3::NEG_Y).intersect_plane(&floor),
        Some(3.0)
    );
    assert_eq!(
        Ray::new(vec3(0.0, -3.0, 0.0), Vec3::NEG_Y).intersect_plane(&floor),
        None,
        "plane behind"
    );
    assert_eq!(
        Ray::new(vec3(0.0, 3.0, 0.0), Vec3::X).intersect_plane(&floor),
        None,
        "parallel"
    );
    assert_eq!(
        Ray::new(vec3(0.0, 0.0, 0.0), Vec3::Y).intersect_plane(&floor),
        Some(0.0),
        "starting on it"
    );
}

#[test]
fn rect_constructors_agree_and_inflate_shrinks_to_empty() {
    let a = Rect::from_min_size(vec2(1.0, 2.0), vec2(4.0, 2.0));
    let b = Rect::from_center_size(vec2(3.0, 3.0), vec2(4.0, 2.0));
    assert_eq!(a, b);
    assert_eq!(
        (a.width(), a.height(), a.size(), a.center()),
        (4.0, 2.0, vec2(4.0, 2.0), vec2(3.0, 3.0))
    );
    assert!(!a.is_empty());
    assert!(
        a.inflate(-1.0).is_empty(),
        "shrinking a height of 2 by 1 per side leaves a line"
    );
    assert!(a.inflate(-5.0).is_empty());
    assert!(a.inflate(1.0).contains(vec2(0.0, 1.0)));
    assert!(Rect::default().is_empty());
}

#[test]
fn rect_touching_does_not_intersect_but_contains_its_boundary() {
    let a = Rect::from_min_size(Vec2::ZERO, Vec2::ONE);
    let right = Rect::from_min_size(vec2(1.0, 0.0), Vec2::ONE);
    assert!(!a.intersects(&right), "shared edge has no area");
    assert_eq!(a.intersection(&right), None);
    assert!(a.contains(vec2(1.0, 1.0)) && a.contains(Vec2::ZERO));
    assert!(!a.contains(vec2(1.0, 1.0001)));
    assert!(!a.contains(vec2(f32::NAN, 0.5)));
    let overlap = a.intersection(&Rect::from_min_size(vec2(0.5, 0.5), Vec2::ONE));
    assert_eq!(overlap, Some(Rect::new(vec2(0.5, 0.5), Vec2::ONE)));
    assert_eq!(a.union(&right), Rect::new(Vec2::ZERO, vec2(2.0, 1.0)));
}

#[test]
fn sphere_does_not_hit_an_inverted_aabb() {
    use regia_core::glam::Vec3;
    use regia_core::math::{Aabb, Sphere};
    let inverted = Aabb {
        min: Vec3::new(2.0, 0.0, 0.0),
        max: Vec3::new(0.0, 1.0, 1.0),
    };
    assert!(inverted.is_empty());
    let s = Sphere::new(Vec3::new(0.0, 0.5, 0.5), 0.1);
    assert!(!s.intersects_aabb(&inverted));
}
