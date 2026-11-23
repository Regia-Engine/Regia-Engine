// SPDX-License-Identifier: MPL-2.0
//! Edge cases of the ported `math::geom` types: reversed and degenerate ranges, rect algebra under a
//! fixed-seed generator, polygon and triangle index helpers, angle and range helpers.

use regia_core::glam::Vec2;
use regia_core::math::Rng;
use regia_core::math::geom::{
    self, Align, Corner, Cuboid, Edge, Padding, Polygon, Range, Rect, Tri, clamp, deg_to_rad, fmod,
    map_range, partial_max, partial_min, rad_to_deg, rad_to_turns, tri, turns_to_rad, vec2,
};

fn random_rect(rng: &mut Rng) -> Rect<f32> {
    let mut v = || (rng.range(-20..20) as f32) * 0.5;
    let (a, b, c, d) = (v(), v(), v(), v());
    Rect::from_corner_points([a, b], [c, d])
}

#[test]
fn range_overlap_of_touching_ranges_is_a_single_point() {
    assert_eq!(
        Range::new(0.0, 1.0).overlap(Range::new(1.0, 2.0)),
        Some(Range::new(1.0, 1.0))
    );
    assert_eq!(Range::new(0.0, 1.0).overlap(Range::new(1.5, 2.0)), None);
    // Reversed ranges overlap like their absolute forms and the result is ascending.
    assert_eq!(
        Range::new(1.0, 0.0).overlap(Range::new(2.0, 1.0)),
        Some(Range::new(1.0, 1.0))
    );
}

#[test]
fn range_closest_edge_breaks_ties_towards_the_start() {
    let r = Range::new(0.0, 10.0);
    assert_eq!(r.closest_edge(5.0), Edge::Start);
    assert_eq!(r.closest_edge(-100.0), Edge::Start);
    assert_eq!(r.closest_edge(100.0), Edge::End);
    // A zero-length range has two equal edges: the start wins.
    assert_eq!(Range::new(3.0, 3.0).closest_edge(9.0), Edge::Start);
}

#[test]
fn range_align_to_agrees_with_the_named_alignments() {
    let a = Range::new(2.5, 7.5);
    let b = Range::new(0.0, 10.0);
    assert_eq!(a.align_to(Align::Start, b), a.align_start_of(b));
    assert_eq!(a.align_to(Align::Middle, b), a.align_middle_of(b));
    assert_eq!(a.align_to(Align::End, b), a.align_end_of(b));
    assert_eq!(a.align_middle_of(b).middle(), b.middle());
}

#[test]
fn range_pad_and_stretch_keep_a_reversed_range_reversed() {
    let r = Range::new(10.0, 0.0);
    let padded = r.pad(2.0);
    assert!(padded.start > padded.end, "{padded:?} must stay reversed");
    assert_eq!(padded.len(), 6.0);
    assert!(r.stretch_to_value(12.0).start == 12.0 && r.stretch_to_value(12.0).end == 0.0);
    assert_eq!(
        r.stretch_to_value(5.0),
        r,
        "a value inside leaves the range alone"
    );
    assert!(r.contains(5.0) && r.contains(0.0) && r.contains(10.0) && !r.contains(10.5));
}

#[test]
fn range_lerp_and_map_value_hit_their_end_points() {
    let r = Range::new(-5.0f32, 15.0);
    assert_eq!(r.lerp(0.0), -5.0);
    assert_eq!(r.lerp(1.0), 15.0);
    assert_eq!(r.lerp(2.0), 35.0, "lerp extrapolates");
    let zero = Range::new(4.0f32, 4.0);
    // A zero-width source range maps to the start of the target, not NaN.
    assert_eq!(zero.map_value(9.0, &r), -5.0);
}

#[test]
fn map_range_and_clamp_edge_cases() {
    // Reversed output, reversed input.
    assert_eq!(map_range(0.25f32, 0.0, 1.0, 10.0, 0.0), 7.5);
    assert_eq!(map_range(0.25f32, 1.0, 0.0, 0.0, 10.0), 7.5);
    // Float -> integer truncates toward zero.
    assert_eq!(map_range(0.99f32, 0.0, 1.0, 0i32, 10), 9);
    // clamp accepts a reversed pair.
    assert_eq!(clamp(5, 10, 0), 5);
    assert_eq!(clamp(-5, 10, 0), 0);
    assert_eq!(clamp(50, 10, 0), 10);
    // partial_max / partial_min keep the first argument on a tie and with NaN pick the second/first deterministically.
    assert_eq!(partial_max(1.0, 2.0), 2.0);
    assert_eq!(partial_min(1.0, 2.0), 1.0);
    assert_eq!(partial_max(f32::NAN, 1.0), 1.0);
    assert_eq!(partial_min(f32::NAN, 1.0), 1.0);
}

#[test]
fn angle_conversions_round_trip_and_match_known_values() {
    for deg in [-720.0f64, -90.0, 0.0, 1.0, 45.0, 180.0, 359.0, 1.0e6] {
        let back = rad_to_deg(deg_to_rad(deg));
        assert!((back - deg).abs() <= deg.abs() * 1e-12 + 1e-12, "{deg} -> {back}");
    }
    assert!((deg_to_rad(90.0f32) - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    assert!((turns_to_rad(0.25f64) - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
    assert!((rad_to_turns(std::f64::consts::PI) - 0.5).abs() < 1e-12);
    assert!(deg_to_rad(f32::NAN).is_nan());
}

#[test]
fn fmod_with_zero_or_infinite_operands_does_not_panic() {
    assert!(fmod(1.0f32, 0.0).is_nan());
    assert_eq!(fmod(3.0f32, f32::INFINITY), 3.0);
    assert!(fmod(f32::INFINITY, 3.0).is_nan());
    assert_eq!(fmod(-0.5f64, 1.0), -0.5);
    // 10^15 = 6 (mod 7), exactly.
    assert_eq!(fmod(1.0e15f64, 7.0), 6.0);
}

#[test]
fn rect_overlap_is_symmetric_and_contained_in_both() {
    let mut rng = Rng::new(0x0123_4567);
    for _ in 0..500 {
        let (a, b) = (random_rect(&mut rng), random_rect(&mut rng));
        assert_eq!(a.overlap(b), b.overlap(a), "{a:?} {b:?}");
        if let Some(o) = a.overlap(b) {
            for corner in o.corners_iter() {
                assert!(a.contains_point(corner) && b.contains_point(corner), "{corner:?}");
            }
        }
        let m = a.max(b);
        for corner in a.corners_iter().chain(b.corners_iter()) {
            assert!(m.contains_point(corner), "max of {a:?} {b:?} lost {corner:?}");
        }
    }
}

#[test]
fn rect_corners_come_in_the_documented_winding_and_are_contained() {
    let r = Rect::from_corner_points([-1.0f32, -2.0], [3.0, 4.0]);
    let corners: Vec<_> = r.corners_iter().collect();
    assert_eq!(corners, r.corners().0.to_vec());
    assert_eq!(r.corner_at_index(4), None);
    assert_eq!(r.corner_at_index(255), None);
    for c in &corners {
        assert!(r.contains_point(*c), "edges are inclusive");
    }
    assert!(!r.contains_point([3.001, 0.0]));
    assert!(!r.contains_point([f32::NAN, 0.0]));
}

#[test]
fn rect_closest_corner_covers_all_four() {
    let r = Rect::from_corner_points([0.0f32, 0.0], [10.0, 10.0]);
    assert_eq!(r.closest_corner([1.0, 1.0]), Corner::BottomLeft);
    assert_eq!(r.closest_corner([9.0, 1.0]), Corner::BottomRight);
    assert_eq!(r.closest_corner([1.0, 9.0]), Corner::TopLeft);
    assert_eq!(r.closest_corner([9.0, 9.0]), Corner::TopRight);
    assert_eq!(r.closest_corner([500.0, -500.0]), Corner::BottomRight);
}

#[test]
fn rect_subdivisions_tile_the_parent_without_overlap() {
    let mut rng = Rng::new(7);
    for _ in 0..200 {
        let r = random_rect(&mut rng);
        let parts = r.subdivisions();
        let area: f32 = parts.iter().map(|p| p.w() * p.h()).sum();
        assert!((area - r.w() * r.h()).abs() < 1e-3, "{r:?}");
        for (i, a) in parts.iter().enumerate() {
            assert_eq!(a.w(), r.w() / 2.0);
            assert_eq!(a.h(), r.h() / 2.0);
            for b in &parts[i + 1..] {
                if let Some(o) = a.overlap(*b) {
                    assert!(o.w() * o.h() == 0.0, "{a:?} and {b:?} share area");
                }
            }
        }
    }
}

#[test]
fn rect_triangles_cover_exactly_the_rect() {
    let r = Rect::from_corner_points([0.0f32, 0.0], [4.0, 2.0]);
    let (a, b) = r.triangles();
    let mut rng = Rng::new(99);
    for _ in 0..300 {
        let p = [rng.f32() * 6.0 - 1.0, rng.f32() * 4.0 - 1.0];
        let inside_rect = p[0] > 0.0 && p[0] < 4.0 && p[1] > 0.0 && p[1] < 2.0;
        let on_diagonal = (p[1] - p[0] * 0.5).abs() < 1e-5;
        if on_diagonal {
            continue;
        }
        let in_tris = a.contains(&p) || b.contains(&p);
        assert_eq!(in_tris, inside_rect, "{p:?}");
    }
}

#[test]
fn rect_padding_pad_and_relative_to() {
    let r = Rect::from_corner_points([0.0f32, 0.0], [10.0, 6.0]);
    let padded = r.pad(1.0);
    assert_eq!(padded.w_h(), (8.0, 4.0));
    assert_eq!(padded.pad(-1.0), r, "negative pad undoes pad");
    let p = Padding {
        x: Range::new(1.0, 2.0),
        y: Range::new(0.5, 0.0),
    };
    let q = r.padding(p);
    assert_eq!(q.l_r_b_t(), (1.0, 8.0, 0.5, 6.0));
    assert_eq!(r.relative_to([2.0, 1.0]).l_r_b_t(), (-2.0, 8.0, -1.0, 5.0));
    // Over-padding makes the rect inverted instead of panicking.
    let tiny = r.pad(100.0);
    assert!(tiny.w() < 0.0 || tiny.x.start > tiny.x.end || tiny.absolute().w() > 0.0);
}

#[test]
fn rect_inversion_keeps_the_covered_area() {
    let r = Rect::from_corner_points([0.0f32, 0.0], [4.0, 2.0]);
    let flipped = r.invert_x().invert_y();
    assert_ne!(flipped, r);
    assert_eq!(flipped.absolute(), r);
    assert!(flipped.contains_point([1.0, 1.0]));
}

#[test]
fn tri_contains_is_winding_independent_and_edge_inclusive_at_vertices() {
    let ccw = Tri([vec2(0.0, 0.0), vec2(4.0, 0.0), vec2(0.0, 4.0)]);
    let cw = Tri([vec2(0.0, 0.0), vec2(0.0, 4.0), vec2(4.0, 0.0)]);
    for p in [vec2(1.0, 1.0), vec2(0.5, 3.0), vec2(3.0, 0.5)] {
        assert!(ccw.contains(&p) && cw.contains(&p), "{p:?}");
    }
    for p in [vec2(-0.1, 1.0), vec2(3.0, 3.0), vec2(5.0, 5.0)] {
        assert!(!ccw.contains(&p) && !cw.contains(&p), "{p:?}");
    }
}

#[test]
fn tri_helpers_from_vertices_and_indices() {
    assert_eq!(tri::from_vertices([1, 2]), None::<Tri<i32>>);
    assert_eq!(
        tri::from_vertices([1, 2, 3, 4]),
        Some(Tri([1, 2, 3])),
        "extra vertices are ignored"
    );
    let verts = [10, 20, 30, 40];
    assert_eq!(tri::from_index_tri(&verts, &[3, 0, 3]), Tri([40, 10, 40]));
    let tris: Vec<_> = tri::iter_from_vertices(1..=7).collect();
    assert_eq!(
        tris,
        vec![Tri([1, 2, 3]), Tri([4, 5, 6])],
        "a trailing partial triangle is dropped"
    );
    let flat = tri::flatten_index_tris([[0, 1, 2], [2, 3, 0]]);
    assert_eq!(flat.len(), 6);
    assert_eq!(flat.collect::<Vec<_>>(), vec![0, 1, 2, 2, 3, 0]);
    let mut flat = tri::flatten_index_tris([[0, 1, 2], [2, 3, 0]]);
    flat.next();
    assert_eq!(flat.len(), 5);
    let t = |x: f32| Tri([vec2(x, 0.0), vec2(x, 1.0), vec2(x, 2.0)]);
    let mut vs = tri::vertices_from_iter(vec![t(1.0), t(2.0)]);
    assert_eq!(vs.len(), 6);
    vs.next();
    vs.next();
    assert_eq!(vs.len(), 4);
    assert_eq!(vs.map(|v| v.x).collect::<Vec<_>>(), vec![1.0, 2.0, 2.0, 2.0]);
    assert_eq!(tri::vertices_from_iter(Vec::<Tri<Vec2>>::new()).count(), 0);
}

#[test]
fn polygon_triangulation_counts_and_indices_agree() {
    for n in 0usize..12 {
        let pts: Vec<_> = (0..n).map(|i| vec2(i as f32, (i * i) as f32)).collect();
        let tris = Polygon::new(pts.iter().copied()).triangles();
        let count = tris.map_or(0, Iterator::count);
        let expected = n.saturating_sub(2);
        assert_eq!(count, expected, "{n} points");
        let idx: Vec<usize> = geom::polygon::triangle_indices(n).collect();
        assert_eq!(idx.len(), expected * 3, "{n} points");
        assert!(idx.iter().all(|&i| i < n.max(1)));
        // Fan around vertex 0, consecutive.
        for (t, chunk) in idx.as_chunks::<3>().0.iter().enumerate() {
            assert_eq!(*chunk, [0, t + 1, t + 2]);
        }
    }
}

#[test]
fn polygon_contains_and_bounds_with_degenerate_input() {
    let square = [vec2(0.0, 0.0), vec2(2.0, 0.0), vec2(2.0, 2.0), vec2(0.0, 2.0)];
    assert!(Polygon::new(square).contains(&vec2(1.0, 1.0)).is_some());
    assert!(Polygon::new(square).contains(&vec2(3.0, 1.0)).is_none());
    assert!(Polygon::new([vec2(0.0, 0.0)]).contains(&vec2(0.0, 0.0)).is_none());
    assert!(
        Polygon::new(Vec::<Vec2>::new())
            .contains(&vec2(0.0, 0.0))
            .is_none()
    );
    assert_eq!(Polygon::new(Vec::<Vec2>::new()).bounding_rect(), None);
    assert_eq!(
        Polygon::new([vec2(1.0, 5.0)])
            .bounding_rect()
            .map(|r| r.l_r_b_t()),
        Some((1.0, 1.0, 5.0, 5.0)),
        "one point bounds a zero-size rect"
    );
    let neg = geom::bounding_rect([vec2(-3.0, 4.0), vec2(2.0, -6.0), vec2(0.0, 0.0)]);
    assert_eq!(neg.map(|r| r.l_r_b_t()), Some((-3.0, 2.0, -6.0, 4.0)));
    assert_eq!(geom::bounding_cuboid(Vec::<geom::Point3>::new()), None);
}

#[test]
fn centroid_of_nothing_is_none_and_of_points_is_their_mean() {
    assert_eq!(geom::centroid(Vec::<Vec2>::new()), None);
    assert_eq!(
        geom::centroid([vec2(0.0, 0.0), vec2(4.0, 2.0)]),
        Some(vec2(2.0, 1.0))
    );
}

#[test]
fn cuboid_algebra_matches_per_axis_ranges() {
    let a = Cuboid::from_x_y_z_w_h_d(0.0f32, 0.0, 0.0, 4.0, 4.0, 4.0);
    let b = Cuboid::from_x_y_z_w_h_d(3.0f32, 0.0, 0.0, 4.0, 4.0, 4.0);
    assert_eq!(a.overlap(b).map(|o| o.w_h_d()), Some((1.0, 4.0, 4.0)));
    assert_eq!(a.overlap(b), b.overlap(a));
    let far = Cuboid::from_x_y_z_w_h_d(100.0f32, 0.0, 0.0, 1.0, 1.0, 1.0);
    assert_eq!(a.overlap(far), None);
    assert_eq!(a.volume(), 64.0);
    assert_eq!(a.pad(1.0).w_h_d(), (2.0, 2.0, 2.0));
    for c in a.corners() {
        assert!(a.contains_point(c));
    }
    assert!(!a.contains_point([0.0, 0.0, 4.5]));
    assert_eq!(a.corners().len(), 8);
    assert_eq!(a.faces().len(), 6);
    let stretched = a.stretch_to_point([10.0, -5.0, 2.0]);
    assert_eq!((stretched.right(), stretched.bottom()), (10.0, -5.0));
    let parts = a.subdivision_ranges().cuboids();
    let total: f32 = parts.iter().map(Cuboid::volume).sum();
    assert_eq!(total, 64.0);
}

#[test]
fn cuboid_max_unions_every_axis() {
    use regia_core::math::geom::{Cuboid, Range};
    let r = |a: f32, b: f32| Range { start: a, end: b };
    let a = Cuboid {
        x: r(0.0, 1.0),
        y: r(0.0, 1.0),
        z: r(0.0, 1.0),
    };
    let b = Cuboid {
        x: r(0.0, 1.0),
        y: r(0.0, 1.0),
        z: r(10.0, 11.0),
    };
    assert_eq!(a.max(b).z, r(0.0, 11.0));
}

#[test]
fn tri_contains_rejects_degenerate_and_is_edge_inclusive_for_both_windings() {
    let d = Tri([vec2(0.0, 0.0); 3]);
    assert!(!d.contains(&vec2(1000.0, 1000.0)));
    let line = Tri([vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(2.0, 0.0)]);
    assert!(!line.contains(&vec2(1.0, 0.0)) && !line.contains(&vec2(1.0, 5.0)));
    let ccw = Tri([vec2(0.0, 0.0), vec2(4.0, 0.0), vec2(0.0, 4.0)]);
    let cw = Tri([vec2(0.0, 0.0), vec2(0.0, 4.0), vec2(4.0, 0.0)]);
    let on_edge = vec2(2.0, 0.0);
    assert!(ccw.contains(&on_edge) && cw.contains(&on_edge));
}

#[test]
fn look_to_is_finite_for_parallel_or_zero_directions() {
    use regia_core::glam::{Mat4, Vec3};
    use regia_core::math::geom::Mat4LookTo;
    for dir in [Vec3::Y, Vec3::ZERO] {
        let m = <Mat4 as Mat4LookTo>::look_to_rh(Vec3::ZERO, dir, Vec3::Y);
        assert!(m.is_finite(), "{dir:?}");
        let l = <Mat4 as Mat4LookTo>::look_to_lh(Vec3::ONE, dir, Vec3::Y);
        assert!(l.is_finite(), "{dir:?}");
    }
}
