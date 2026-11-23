// SPDX-License-Identifier: MPL-2.0
//! The CPU ray geometry the probe placement and the leak scan run against (`placement.rs`): ray and segment hits,
//! front/back faces, the terrain, box queries and the Fibonacci direction set, each against a closed form on a cube
//! and a flat heightfield. CPU only, no GPU, no golden.
//!
//! Closed forms: a ray with unit direction d from the centre of an axis-aligned cube of half-size h leaves through
//! the face its largest |d| component points at, at distance h / max|dᵢ| (slab method); the points of a spherical
//! Fibonacci set have y = 1 − (2i + 1)/n (Keinert et al., "Spherical Fibonacci Mapping", 2015), so their mean is the
//! origin and each y-band holds the same number of points (equal area, Archimedes' hat-box theorem).

use regia_core::glam::Vec3;
use regia_pt::PtHeightfield;
use regia_pt::placement::{PlacementGeometry, fibonacci_directions};

/// A cube of half-size `h` about the origin, 12 triangles wound counter-clockwise seen from outside.
fn cube(h: f32) -> Vec<[Vec3; 3]> {
    let mut tris = Vec::new();
    for axis in 0..3 {
        for sign in [-1.0f32, 1.0] {
            let (u, v) = ((axis + 1) % 3, (axis + 2) % 3);
            let corner = |a: f32, b: f32| {
                let mut p = [0.0f32; 3];
                p[axis] = sign * h;
                p[u] = a * h;
                p[v] = b * h;
                Vec3::from_array(p)
            };
            let quad = [
                corner(-1.0, -1.0),
                corner(1.0, -1.0),
                corner(1.0, 1.0),
                corner(-1.0, 1.0),
            ];
            let mut outward = [0.0f32; 3];
            outward[axis] = sign;
            let facing = (quad[1] - quad[0])
                .cross(quad[2] - quad[0])
                .dot(Vec3::from_array(outward));
            let q = if facing > 0.0 {
                quad
            } else {
                [quad[0], quad[3], quad[2], quad[1]]
            };
            tris.push([q[0], q[1], q[2]]);
            tris.push([q[0], q[2], q[3]]);
        }
    }
    tris
}

fn flat_terrain(y: f32) -> PtHeightfield {
    PtHeightfield {
        width: 2,
        depth: 2,
        origin: [-10.0, -10.0],
        spacing: [20.0, 20.0],
        heights: vec![y; 4],
        material: 0,
    }
}

#[test]
fn rays_from_inside_a_cube_hit_back_faces_at_the_slab_distance() {
    let geo = PlacementGeometry::from_triangles(cube(2.0), None);
    assert_eq!(geo.triangle_count(), 12);
    let dirs = fibonacci_directions(128);
    let view = geo.view([0.0; 3], &dirs, 100.0);
    assert_eq!(view.rays, 128);
    assert_eq!(
        view.back_hits.len(),
        128,
        "every ray leaves a closed mesh through a back face"
    );
    assert!(view.closest_front.is_none());
    assert_eq!(view.back_face_fraction(), 1.0);
    assert!(view.inside(0.9) && geo.is_inside([0.0; 3], &dirs, 100.0, 0.5));
    // Distances: 2 / max|d|, sorted nearest first.
    for (d, t) in &view.back_hits {
        let m = d.iter().fold(0.0f32, |a, c| a.max(c.abs()));
        assert!((t - 2.0 / m).abs() < 1e-3, "dir {d:?}: t {t} vs {}", 2.0 / m);
    }
    assert!(
        view.back_hits.windows(2).all(|w| w[0].1 <= w[1].1),
        "nearest first"
    );
}

#[test]
fn rays_from_outside_hit_front_faces_and_the_nearest_wins() {
    let geo = PlacementGeometry::from_triangles(cube(2.0), None);
    let hit = geo
        .closest_hit([5.0, 0.3, -0.2], [-1.0, 0.0, 0.0], 100.0)
        .expect("hits the +x face");
    assert!((hit.t - 3.0).abs() < 1e-4 && !hit.back_face, "{hit:?}");
    // The same ray stops short of the face at t_max, and misses when pointed away.
    assert!(geo.closest_hit([5.0, 0.3, -0.2], [-1.0, 0.0, 0.0], 2.9).is_none());
    assert!(
        geo.closest_hit([5.0, 0.3, -0.2], [1.0, 0.0, 0.0], 100.0)
            .is_none()
    );
    // A miss beside the cube.
    assert!(
        geo.closest_hit([5.0, 3.0, 0.0], [-1.0, 0.0, 0.0], 100.0)
            .is_none()
    );
    // The direction need not be a unit vector.
    let hit = geo
        .closest_hit([5.0, 0.0, 0.0], [-7.0, 0.0, 0.0], 100.0)
        .expect("hit");
    assert!((hit.t - 3.0).abs() < 1e-4);
    // From outside, a view sees a front hit and no back hits.
    let view = geo.view([5.0, 0.0, 0.0], &[[-1.0, 0.0, 0.0], [1.0, 0.0, 0.0]], 100.0);
    assert!(view.back_hits.is_empty());
    let (d, t) = view.closest_front.expect("front hit");
    assert_eq!(d, [-1.0, 0.0, 0.0]);
    assert!((t - 3.0).abs() < 1e-4);
    assert_eq!(view.back_face_fraction(), 0.0);
}

#[test]
fn degenerate_rays_and_inputs_are_misses_not_panics() {
    let geo = PlacementGeometry::from_triangles(cube(1.0), None);
    assert!(
        geo.closest_hit([0.0; 3], [0.0; 3], 10.0).is_none(),
        "a zero direction"
    );
    assert!(
        geo.closest_hit([0.0; 3], [1.0, 0.0, 0.0], 0.0).is_none(),
        "a zero range"
    );
    assert!(
        geo.closest_hit([0.0; 3], [1.0, 0.0, 0.0], -1.0).is_none(),
        "a negative range"
    );
    assert!(
        geo.closest_hit([f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], 10.0)
            .is_none(),
        "a NaN origin"
    );
    // Non-finite triangles are dropped at build time; an empty geometry hits nothing.
    let nan = [Vec3::NAN, Vec3::X, Vec3::Y];
    let geo = PlacementGeometry::from_triangles(vec![nan], None);
    assert_eq!(geo.triangle_count(), 0);
    assert!(geo.closest_hit([0.0; 3], [1.0, 0.0, 0.0], 10.0).is_none());
    assert!(!geo.box_touches([-1.0; 3], [1.0; 3], false));
    // An empty view has no back-face fraction.
    let view = geo.view([0.0; 3], &[], 10.0);
    assert_eq!((view.rays, view.back_face_fraction()), (0, 0.0));
    assert!(!view.inside(0.0));
}

#[test]
fn segments_ignore_hits_at_their_ends_and_report_the_crossing() {
    let geo = PlacementGeometry::from_triangles(cube(2.0), None);
    // From the centre to well outside: crosses the +x face once, from inside (a back face).
    let hit = geo.segment_hit([0.0; 3], [4.0, 0.0, 0.1]).expect("crosses");
    assert!(hit.back_face && (hit.t - 2.0).abs() < 0.01, "{hit:?}");
    // A segment that stops short of the face, or ends exactly on it, does not cross.
    assert!(geo.segment_hit([0.0; 3], [1.9, 0.0, 0.0]).is_none());
    assert!(
        geo.segment_hit([0.0; 3], [2.0, 0.0, 0.0]).is_none(),
        "ending on the surface is not a crossing"
    );
    // A probe sitting on the surface does not see that surface.
    assert!(geo.segment_hit([2.0, 0.1, 0.1], [3.0, 0.1, 0.1]).is_none());
    // Zero-length, tiny and non-finite segments.
    assert!(geo.segment_hit([0.0; 3], [0.0; 3]).is_none());
    assert!(geo.segment_hit([0.0; 3], [1e-7, 0.0, 0.0]).is_none());
    assert!(geo.segment_hit([0.0; 3], [f32::INFINITY, 0.0, 0.0]).is_none());
}

#[test]
fn the_terrain_is_a_surface_seen_from_above_and_below() {
    let terrain = flat_terrain(-1.0);
    let geo = PlacementGeometry::from_triangles(cube(2.0), Some(&terrain));
    // Straight down from inside the cube: the terrain at y = -1 is nearer than the cube's floor at y = -2. The ray
    // starts above the ground, so it meets the terrain's front.
    let hit = geo
        .closest_hit([0.5, 0.0, 0.5], [0.0, -1.0, 0.0], 100.0)
        .expect("terrain");
    assert!((hit.t - 1.0).abs() < 1e-4 && !hit.back_face, "{hit:?}");
    // From beneath the ground, upward: the back of the terrain. The march brackets the crossing
    // in one 10 m step (half the 20 m grid spacing) and `TERRAIN_BISECT` (12) bisections refine
    // it to at most step/2^12 = 2.5e-3, so the tolerance is the algorithm's bound, not 1e-4.
    let hit = geo
        .closest_hit([5.0, -3.0, 5.0], [0.0, 1.0, 0.0], 100.0)
        .expect("terrain from below");
    assert!((hit.t - 2.0).abs() < 4e-3 && hit.back_face, "{hit:?}");
    // Off the grid there is no ground.
    assert!(
        geo.closest_hit([50.0, 0.0, 50.0], [0.0, -1.0, 0.0], 100.0)
            .is_none()
    );
    assert!(geo.under_terrain([5.0, -1.5, 5.0]));
    assert!(!geo.under_terrain([5.0, -0.5, 5.0]));
    assert!(
        !geo.under_terrain([50.0, -50.0, 50.0]),
        "off the grid is not under the terrain"
    );
    // Without a terrain nothing is under it.
    assert!(!PlacementGeometry::from_triangles(cube(2.0), None).under_terrain([0.0, -50.0, 0.0]));
    // A hole (NaN sample) removes the ground there.
    let mut holed = flat_terrain(-1.0);
    holed.heights[0] = f32::NAN;
    let geo = PlacementGeometry::from_triangles(Vec::new(), Some(&holed));
    assert!(!geo.under_terrain([0.0, -5.0, 0.0]));
    assert!(
        geo.closest_hit([0.0, 5.0, 0.0], [0.0, -1.0, 0.0], 100.0)
            .is_none()
    );
}

#[test]
fn box_queries_follow_the_mesh_and_the_terrain() {
    let terrain = flat_terrain(-1.0);
    let geo = PlacementGeometry::from_triangles(cube(2.0), Some(&terrain));
    // Wholly inside the cube's volume and clear of its faces and of the terrain: no triangle touches it.
    assert!(!geo.box_touches([-0.5, 0.0, -0.5], [0.5, 1.0, 0.5], true));
    // Straddling a face.
    assert!(geo.box_touches([1.5, 0.0, 0.0], [2.5, 1.0, 1.0], false));
    // Far away.
    assert!(!geo.box_touches([20.0, 20.0, 20.0], [21.0, 21.0, 21.0], true));
    // The terrain counts only when asked: this box crosses y = -1 inside the cube.
    let across = ([-0.5, -1.5, -0.5], [0.5, -0.5, 0.5]);
    assert!(geo.box_touches(across.0, across.1, true));
    assert!(!geo.box_touches(across.0, across.1, false));
    // Wholly under the terrain: on the grid, top below the lowest ground.
    assert!(geo.box_under_terrain([-1.0, -5.0, -1.0], [1.0, -3.0, 1.0]));
    assert!(
        !geo.box_under_terrain([-1.0, -5.0, -1.0], [1.0, -0.5, 1.0]),
        "pokes through the ground"
    );
    assert!(
        !geo.box_under_terrain([8.0, -5.0, 8.0], [12.0, -3.0, 9.0]),
        "leaves the grid"
    );
    assert!(!PlacementGeometry::from_triangles(cube(2.0), None).box_under_terrain([0.0; 3], [1.0; 3]));
}

#[test]
fn from_scene_places_instances_by_their_transform() {
    use regia_core::glam::Mat4;
    use regia_pt::{PtCamera, PtMaterial, PtMesh, PtScene};
    let tris = cube(1.0);
    let mesh = PtMesh {
        positions: tris.iter().flatten().map(|v| v.to_array()).collect(),
        normals: Vec::new(),
        uvs: Vec::new(),
        colors: Vec::new(),
        tangents: Vec::new(),
        indices: (0..tris.len() as u32 * 3).collect(),
    };
    let mut scene = PtScene::new(PtCamera::pinhole([0.0, 0.0, 9.0], [0.0; 3], 0.8, 4, 4));
    let mat = scene.add_material(PtMaterial::default());
    let m = scene.add_mesh(mesh);
    scene.add_instance(m, mat, Mat4::from_translation(Vec3::new(10.0, 0.0, 0.0)));
    // An instance naming a mesh that does not exist is skipped, not a panic.
    scene.add_instance(99, mat, Mat4::IDENTITY);
    let geo = PlacementGeometry::from_scene(&scene);
    assert_eq!(geo.triangle_count(), 12);
    let hit = geo
        .closest_hit([0.0; 3], [1.0, 0.0, 0.0], 100.0)
        .expect("the translated cube");
    assert!((hit.t - 9.0).abs() < 1e-4 && !hit.back_face, "{hit:?}");
}

#[test]
fn fibonacci_directions_are_unit_uniform_and_off_the_axes() {
    for n in [4u32, 17, 64, 1024] {
        let dirs = fibonacci_directions(n);
        assert_eq!(dirs.len(), n as usize);
        let mut mean = Vec3::ZERO;
        for (i, d) in dirs.iter().enumerate() {
            let v = Vec3::from_array(*d);
            assert!((v.length() - 1.0).abs() < 1e-5, "n {n} point {i}");
            // y = 1 - (2i + 1)/n exactly: equal-area bands.
            assert!((d[1] - (1.0 - (2 * i + 1) as f32 / n as f32)).abs() < 1e-5);
            // Never on a coordinate axis (an axis-aligned wall would be grazed).
            assert!(
                v.abs().max_element() < 0.999_999,
                "n {n} point {i} on an axis: {d:?}"
            );
            mean += v;
        }
        // y sums to zero exactly; with enough points the spiral's x and z cancel too.
        assert!(mean.y.abs() < 1e-3, "n {n}: mean {mean:?}");
        if n >= 64 {
            assert!((mean / n as f32).length() < 0.05, "n {n}: mean {mean:?}");
        }
    }
    // The count clamps to 4..=1024; the set is fixed per n.
    assert_eq!(fibonacci_directions(0).len(), 4);
    assert_eq!(fibonacci_directions(1).len(), 4);
    assert_eq!(fibonacci_directions(5000).len(), 1024);
    assert_eq!(fibonacci_directions(100), fibonacci_directions(100));
    // The northern and southern halves hold the same number of points (equal area).
    let dirs = fibonacci_directions(200);
    assert_eq!(dirs.iter().filter(|d| d[1] > 0.0).count(), 100);
}
