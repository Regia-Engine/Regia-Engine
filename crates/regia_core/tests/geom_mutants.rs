// SPDX-License-Identifier: MPL-2.0
//! Tests written from the mutation run of `math::geom` (ADR-3389): each one pins a behavior a mutant broke
//! while the older tests still passed.

use regia_core::glam::Vec3;
use regia_core::math::geom::{self, Cuboid, Range, cuboid};

#[test]
fn a_cuboid_has_twelve_triangles_two_per_face() {
    assert_eq!(cuboid::NUM_FACES, 6);
    assert_eq!(cuboid::NUM_TRIANGLES, 12);
    let c = Cuboid::from_x_y_z_w_h_d(0.0f32, 0.0, 0.0, 2.0, 2.0, 2.0);
    assert_eq!(c.triangles_iter().count(), usize::from(cuboid::NUM_TRIANGLES));
}

#[test]
fn bounding_cuboid_spans_every_vertex() {
    let points = [
        Vec3::new(1.0, -2.0, 3.0),
        Vec3::new(-4.0, 5.0, 0.5),
        Vec3::new(2.0, 0.0, -6.0),
    ];
    let b = geom::bounding_cuboid(points).expect("three points have bounds");
    assert_eq!(b.x, Range::new(-4.0, 2.0));
    assert_eq!(b.y, Range::new(-2.0, 5.0));
    assert_eq!(b.z, Range::new(-6.0, 3.0));
    let one = geom::bounding_cuboid([Vec3::new(1.0, 2.0, 3.0)]).expect("one point has bounds");
    assert_eq!(one.x, Range::new(1.0, 1.0), "one point bounds a zero-size cuboid");
}

fn sample() -> Cuboid<f32> {
    Cuboid::from_x_y_z_w_h_d(1.0, 2.0, 3.0, 4.0, 6.0, 8.0)
}

#[test]
fn shifting_one_axis_moves_that_axis_only() {
    let c = sample();
    let x = c.shift_x(3.0);
    assert_eq!((x.x, x.y, x.z), (c.x.shift(3.0), c.y, c.z));
    assert_ne!(x.x, c.x);
    let y = c.shift_y(-2.0);
    assert_eq!((y.x, y.y, y.z), (c.x, c.y.shift(-2.0), c.z));
    assert_ne!(y.y, c.y);
    let z = c.shift_z(0.5);
    assert_eq!((z.x, z.y, z.z), (c.x, c.y, c.z.shift(0.5)));
    assert_ne!(z.z, c.z);
}

#[test]
fn padding_one_side_changes_that_side_only() {
    let c = sample();
    let p = 0.5;
    let cases = [
        (
            c.pad_left(p),
            Cuboid {
                x: c.x.pad_start(p),
                ..c
            },
        ),
        (
            c.pad_right(p),
            Cuboid {
                x: c.x.pad_end(p),
                ..c
            },
        ),
        (
            c.pad_bottom(p),
            Cuboid {
                y: c.y.pad_start(p),
                ..c
            },
        ),
        (
            c.pad_top(p),
            Cuboid {
                y: c.y.pad_end(p),
                ..c
            },
        ),
        (
            c.pad_front(p),
            Cuboid {
                z: c.z.pad_start(p),
                ..c
            },
        ),
        (
            c.pad_back(p),
            Cuboid {
                z: c.z.pad_end(p),
                ..c
            },
        ),
    ];
    for (i, (padded, expected)) in cases.into_iter().enumerate() {
        assert_eq!(padded, expected, "side {i}");
        assert_ne!(padded, c, "side {i} moved");
    }
}

#[test]
fn f32_cuboid_round_trips_its_centre_and_size_and_contains_points() {
    let p = Vec3::new(1.0, -2.0, 3.0);
    let s = Vec3::new(4.0, 6.0, 2.0);
    let c = Cuboid::from_xyz_whd(p, s);
    assert_eq!(c.xyz(), p);
    assert_eq!(c.whd(), s);
    assert_eq!(c.xyz_whd(), (p, s));
    assert!(c.contains(p));
    assert!(c.contains(p + Vec3::new(1.5, 2.5, 0.5)));
    assert!(!c.contains(p + Vec3::new(2.5, 0.0, 0.0)));
    assert!(!c.contains(p + Vec3::new(0.0, 0.0, -1.5)));
}

#[test]
fn f64_cuboid_round_trips_its_centre_and_size() {
    use regia_core::glam::DVec3;
    let p = DVec3::new(-1.0, 2.0, 0.5);
    let s = DVec3::new(2.0, 8.0, 4.0);
    let c = Cuboid::from_xyz_whd_f64(p, s);
    assert_eq!(c.xyz(), p);
    assert_eq!(c.whd(), s);
    assert_eq!(c.xyz_whd(), (p, s));
    assert!(c.contains(p));
    assert!(c.contains(p + DVec3::new(0.5, 3.5, -1.5)));
    assert!(!c.contains(p + DVec3::new(0.0, 4.5, 0.0)));
}

#[test]
fn corners_from_both_ends_meet_without_repeating() {
    let c = sample();
    let mut it = c.corners_iter();
    let last = it.next_back().expect("eight corners");
    let rest: Vec<[f32; 3]> = it.collect();
    assert_eq!(
        rest.len(),
        7,
        "the corner taken from the back is not yielded again"
    );
    assert!(!rest.contains(&last));
    let all = c.corners();
    assert_eq!(&all[..7], rest.as_slice());
    assert_eq!(all[7], last);
}

/// Every step of `it` reports the exact number of items still to come.
fn assert_exact_size_hints<I: Iterator + Clone>(mut it: I, what: &str) {
    loop {
        let rest = it.clone().count();
        assert_eq!(it.size_hint(), (rest, Some(rest)), "{what} with {rest} left");
        if it.next().is_none() {
            break;
        }
    }
}

fn close(a: [f32; 2], b: [f32; 2]) -> bool {
    (a[0] - b[0]).abs() < 1e-5 && (a[1] - b[1]).abs() < 1e-5
}

fn assert_points(got: &[[f32; 2]], want: &[[f32; 2]], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: {got:?}");
    for (g, w) in got.iter().zip(want) {
        assert!(close(*g, *w), "{what}: got {got:?}, want {want:?}");
    }
}

fn ellipse_rect() -> geom::Rect<f32> {
    // Centred on (10, 5), 4 wide and 2 high.
    geom::Rect::from_x_y_w_h(10.0, 5.0, 4.0, 2.0)
}

#[test]
fn circumference_points_go_round_the_middle() {
    let points: Vec<[f32; 2]> = geom::ellipse::Circumference::new(ellipse_rect(), 4.0).collect();
    let want = [[12.0, 5.0], [10.0, 6.0], [8.0, 5.0], [10.0, 4.0], [12.0, 5.0]];
    assert_points(&points, &want, "full circumference");
    let offset: Vec<[f32; 2]> = geom::ellipse::Circumference::new(ellipse_rect(), 4.0)
        .offset_radians(std::f32::consts::FRAC_PI_2)
        .collect();
    assert_points(&offset[..1], &[[10.0, 6.0]], "offset by a quarter turn");
}

#[test]
fn circumference_sections_split_the_angle_by_the_resolution() {
    let half = std::f32::consts::PI;
    let want = [
        [12.0, 5.0],
        [
            10.0 + 2.0 * std::f32::consts::FRAC_1_SQRT_2,
            5.0 + std::f32::consts::FRAC_1_SQRT_2,
        ],
        [10.0, 6.0],
        [
            10.0 - 2.0 * std::f32::consts::FRAC_1_SQRT_2,
            5.0 + std::f32::consts::FRAC_1_SQRT_2,
        ],
        [8.0, 5.0],
    ];
    let new_section: Vec<[f32; 2]> =
        geom::ellipse::Circumference::new_section(ellipse_rect(), 4.0, half).collect();
    assert_points(&new_section, &want, "new_section");
    let section: Vec<[f32; 2]> = geom::ellipse::Circumference::new(ellipse_rect(), 4.0)
        .section(half)
        .collect();
    assert_points(&section, &want, "section");
}

#[test]
fn ellipse_iterators_report_exact_sizes() {
    let e = geom::Ellipse::new(ellipse_rect(), 6.0);
    assert_exact_size_hints(e.circumference(), "circumference");
    assert_exact_size_hints(e.triangles(), "triangles");
    let (vertices, indices) = e.triangle_indices();
    assert_eq!(
        vertices.clone().count(),
        8,
        "the middle and seven circumference points"
    );
    assert_eq!(
        vertices.clone().next(),
        Some([10.0, 5.0]),
        "the middle comes first"
    );
    assert_eq!(indices.clone().count(), 18, "six triangles");
    assert_exact_size_hints(vertices, "triangle vertices");
    assert_exact_size_hints(indices, "triangle indices");
}

#[test]
fn point_constructors_keep_their_components() {
    use regia_core::glam::{Vec2, Vec4};
    assert_eq!(geom::pt2(1.0, 2.0), Vec2::new(1.0, 2.0));
    assert_eq!(geom::pt3(1.0, 2.0, 3.0), Vec3::new(1.0, 2.0, 3.0));
    assert_eq!(geom::pt4(1.0, 2.0, 3.0, 4.0), Vec4::new(1.0, 2.0, 3.0, 4.0));
}

#[test]
fn polygon_bounding_cuboid_spans_its_points() {
    let poly = geom::Polygon::new([Vec3::new(0.0, 1.0, 2.0), Vec3::new(-1.0, 3.0, 0.0)]);
    let b = poly.bounding_cuboid().expect("two points have bounds");
    assert_eq!(
        (b.x, b.y, b.z),
        (Range::new(-1.0, 0.0), Range::new(1.0, 3.0), Range::new(0.0, 2.0))
    );
}
