// SPDX-License-Identifier: MPL-2.0
//! The double-ended and exact-size iterators of `math::geom` (rect, cuboid, quad, ellipse): reversing
//! them, mixing both ends, and reporting a length that matches what they yield. These ported
//! iterators used to underflow a `u8` on `.rev()` (rect), yield nothing from the back (cuboid
//! corners) and report a wrong `len()` after the first item (quad triangles).

use regia_core::math::geom::{Cuboid, Ellipse, Quad, Rect, Tri, vec2};

fn rect() -> Rect<f32> {
    Rect::from_corner_points([0.0, 0.0], [4.0, 2.0])
}

fn cuboid() -> Cuboid<f32> {
    Cuboid::from_x_y_z_w_h_d(0.0, 0.0, 0.0, 2.0, 2.0, 2.0)
}

#[test]
fn rect_subdivisions_reversed_are_the_forward_order_backwards() {
    let forward: Vec<_> = rect().subdivisions_iter().collect();
    let mut backward: Vec<_> = rect().subdivisions_iter().rev().collect();
    backward.reverse();
    assert_eq!(forward.len(), 4);
    assert_eq!(forward, backward);
    assert_eq!(forward, rect().subdivisions().to_vec());
}

#[test]
fn rect_subdivisions_meet_in_the_middle_when_both_ends_are_used() {
    let all = rect().subdivisions();
    let mut it = rect().subdivisions_iter();
    assert_eq!(it.next(), Some(all[0]));
    assert_eq!(it.next_back(), Some(all[3]));
    assert_eq!(it.len(), 2);
    assert_eq!(it.next_back(), Some(all[2]));
    assert_eq!(it.next(), Some(all[1]));
    assert_eq!(it.next(), None);
    assert_eq!(it.next_back(), None);
    assert_eq!(it.len(), 0);
}

#[test]
fn rect_subdivisions_stay_exhausted_from_either_end() {
    let mut it = rect().subdivisions_iter();
    assert_eq!(it.by_ref().count(), 4);
    assert_eq!(it.next_back(), None);
    assert_eq!(it.next(), None);
    let mut it = rect().subdivisions_iter();
    assert_eq!(it.by_ref().rev().count(), 4);
    assert_eq!(it.next_back(), None);
    assert_eq!(it.next(), None);
}

#[test]
fn rect_corners_reversed_and_mixed() {
    let forward: Vec<_> = rect().corners_iter().collect();
    assert_eq!(forward.len(), 4);
    let mut backward: Vec<_> = rect().corners_iter().rev().collect();
    backward.reverse();
    assert_eq!(forward, backward);
    let mut it = rect().corners_iter();
    assert_eq!(it.next(), Some(forward[0]));
    assert_eq!(it.next_back(), Some(forward[3]));
    assert_eq!(it.next_back(), Some(forward[2]));
    assert_eq!(it.next(), Some(forward[1]));
    assert_eq!(it.next(), None);
    assert_eq!(it.next_back(), None);
}

#[test]
fn rect_iterator_lengths_shrink_with_every_item_from_either_end() {
    let mut it = rect().corners_iter();
    for left in (0..4usize).rev() {
        it.next_back();
        assert_eq!(it.len(), left);
        assert_eq!(it.size_hint(), (left, Some(left)));
    }
}

#[test]
fn cuboid_corners_reversed_and_mixed() {
    let c = cuboid();
    let forward: Vec<_> = c.corners_iter().collect();
    assert_eq!(forward.len(), 8);
    let mut backward: Vec<_> = c.corners_iter().rev().collect();
    assert_eq!(backward.len(), 8, "corners_iter().rev() must yield every corner");
    backward.reverse();
    assert_eq!(forward, backward);
    let mut it = c.corners_iter();
    assert_eq!(it.next(), Some(forward[0]));
    assert_eq!(it.next_back(), Some(forward[7]));
    assert_eq!(it.len(), 6);
    assert_eq!(it.next_back(), Some(forward[6]));
    assert_eq!(it.next(), Some(forward[1]));
    assert_eq!(it.len(), 4);
}

#[test]
fn cuboid_subdivisions_reversed_and_mixed() {
    let c = cuboid();
    let forward: Vec<_> = c.subdivision_ranges().cuboids_iter().collect();
    assert_eq!(forward.len(), 8);
    let mut backward: Vec<_> = c.subdivision_ranges().cuboids_iter().rev().collect();
    backward.reverse();
    assert_eq!(forward, backward);
    let mut it = c.subdivision_ranges().cuboids_iter();
    assert_eq!(it.next(), Some(forward[0]));
    assert_eq!(it.next_back(), Some(forward[7]));
    assert_eq!(it.len(), 6);
    assert_eq!(it.next(), Some(forward[1]));
    assert_eq!(it.next_back(), Some(forward[6]));
    assert_eq!(it.len(), 4);
}

#[test]
fn cuboid_faces_reversed_and_mixed() {
    let c = cuboid();
    let forward: Vec<_> = c.faces_iter().collect();
    assert_eq!(forward.len(), 6);
    let mut backward: Vec<_> = c.faces_iter().rev().collect();
    backward.reverse();
    assert_eq!(forward, backward);
    let mut it = c.faces_iter();
    assert_eq!(it.next(), Some(forward[0]));
    assert_eq!(it.next_back(), Some(forward[5]));
    assert_eq!(it.len(), 4);
    assert_eq!(it.next_back(), Some(forward[4]));
    assert_eq!(it.next(), Some(forward[1]));
    assert_eq!(it.len(), 2);
}

#[test]
fn cuboid_triangles_cover_twelve_and_reverse_cleanly() {
    let c = cuboid();
    let forward: Vec<_> = c.triangles_iter().collect();
    assert_eq!(forward.len(), 12);
    let mut backward: Vec<_> = c.triangles_iter().rev().collect();
    backward.reverse();
    assert_eq!(forward, backward);
    let mut it = c.triangles_iter();
    for taken in 0..=12usize {
        assert_eq!(it.len(), 12 - taken, "len after {taken} items");
        assert_eq!(it.size_hint().0, 12 - taken);
        it.next();
    }
}

#[test]
fn quad_triangles_len_tracks_what_is_left() {
    let quad = Quad([vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0)]);
    let mut it = quad.triangles_iter();
    assert_eq!(it.len(), 2);
    assert!(it.next().is_some());
    assert_eq!(it.len(), 1, "one triangle is still to come");
    assert!(it.next().is_some());
    assert_eq!(it.len(), 0);
    assert!(it.next().is_none());
    assert_eq!(it.len(), 0, "an exhausted iterator has nothing left");

    let mut it = quad.triangles_iter();
    assert!(it.next_back().is_some());
    assert_eq!(it.len(), 1);
    assert!(it.next_back().is_some());
    assert_eq!(it.len(), 0);
    assert!(it.next_back().is_none());
    assert_eq!(it.len(), 0);
}

#[test]
fn quad_triangles_back_to_front_is_the_reverse_of_front_to_back() {
    let quad = Quad([vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0)]);
    let forward: Vec<Tri<_>> = quad.triangles_iter().collect();
    let mut backward: Vec<_> = quad.triangles_iter().rev().collect();
    backward.reverse();
    assert_eq!(forward, backward);
}

#[test]
fn ellipse_iterator_lengths_match_what_they_yield() {
    let ellipse = Ellipse::new(Rect::from_w_h(8.0f32, 4.0), 6.0);
    let mut circ = ellipse.circumference();
    let total = circ.len();
    assert_eq!(total, 7, "resolution 6 closes the outline on its first point");
    for yielded in 0..total {
        assert_eq!(circ.len(), total - yielded);
        circ.next();
    }
    assert_eq!(circ.len(), 0);

    let mut tris = ellipse.triangles();
    assert_eq!(tris.len(), 6);
    assert_eq!(tris.by_ref().count(), 6);

    let (vertices, indices) = ellipse.triangle_indices();
    assert_eq!(vertices.len(), 8, "the middle plus seven outline points");
    let expected = indices.len();
    assert_eq!(indices.count(), expected);
    assert_eq!(expected, 6 * 3);
}

#[test]
fn ellipse_resolution_below_one_is_clamped_instead_of_producing_nan() {
    for resolution in [0.0f32, -3.0, f32::NAN] {
        let points: Vec<[f32; 2]> = Ellipse::new(Rect::from_w_h(2.0f32, 2.0), resolution)
            .circumference()
            .collect();
        assert_eq!(points.len(), 2, "resolution {resolution} clamps to 1");
        assert!(points.iter().flatten().all(|v| v.is_finite()));
    }
}

#[test]
fn cuboid_triangles_meet_in_the_middle_when_both_ends_are_used() {
    let c = cuboid();
    let forward: Vec<_> = c.triangles_iter().collect();
    let mut it = c.triangles_iter();
    let mut seen = Vec::new();
    // Alternate the ends until nothing is left; the two halves must tile the forward order.
    let (mut lo, mut hi) = (0usize, forward.len());
    let mut from_front = true;
    while lo < hi {
        if from_front {
            assert_eq!(it.next().as_ref(), Some(&forward[lo]));
            lo += 1;
        } else {
            hi -= 1;
            assert_eq!(it.next_back().as_ref(), Some(&forward[hi]));
        }
        seen.push(from_front);
        assert_eq!(it.len(), hi - lo);
        from_front = !from_front;
    }
    assert_eq!(seen.len(), 12);
    assert!(it.next().is_none() && it.next_back().is_none());
}
