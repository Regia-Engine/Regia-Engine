// SPDX-License-Identifier: MPL-2.0
//! Edge cases of `regia_gen` (ADR-7240): clips exactly through faces and vertices, a torus lathe,
//! a concave beveled star, a negative scale and empty results.

use regia_gen::{
    Bevel, CpuMesh, ExtrudeOptions, LatheOptions, Plane, Profile, clip, extrude, lathe, mesh_stats, scale,
    translate, uv_box,
};

fn cube() -> CpuMesh {
    let m = extrude(
        &Profile::new(vec![[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]]),
        &ExtrudeOptions {
            depth: 2.0,
            bevel: None,
        },
    )
    .expect("cube");
    translate(&m, [0.0, 0.0, -1.0])
}

fn y_plane(offset: f32) -> Plane {
    Plane {
        normal: [0.0, 1.0, 0.0],
        offset,
    }
}

#[test]
fn a_clip_at_the_top_face_keeps_the_cube_and_one_at_the_bottom_face_keeps_nothing() {
    let full = clip(&cube(), y_plane(1.0), true).expect("top");
    let s = mesh_stats(&full);
    assert!(s.watertight, "{s:?}");
    assert!((s.volume - 8.0).abs() < 1e-9, "{}", s.volume);
    assert_eq!(s.triangles, 12);
    // The bottom face lies in the plane but faces away from it: no zero-volume sheet is left.
    let none = clip(&cube(), y_plane(-1.0), true).expect("bottom");
    assert_eq!(mesh_stats(&none).triangles, 0, "{:?}", mesh_stats(&none));
    let beyond = clip(&cube(), y_plane(-5.0), true).expect("all removed");
    assert!(beyond.indices.is_empty());
    let all = mesh_stats(&clip(&cube(), y_plane(5.0), true).expect("all kept"));
    assert!(all.watertight);
    assert_eq!((all.triangles, all.welded_positions), (12, 8));
    assert!((all.volume - 8.0).abs() < 1e-9);
}

#[test]
fn a_diagonal_clip_through_four_cube_vertices_stays_watertight() {
    // x = y passes exactly through the vertical edges at (1, 1) and (-1, -1).
    let half = clip(
        &cube(),
        Plane {
            normal: [1.0, -1.0, 0.0],
            offset: 0.0,
        },
        true,
    )
    .expect("diagonal");
    let s = mesh_stats(&half);
    assert!(s.watertight, "{s:?}");
    assert!((s.volume - 4.0).abs() < 1e-6, "{}", s.volume);
}

#[test]
fn a_torus_lathe_from_a_closed_loop_profile_is_watertight() {
    let n = 12;
    let mut ring: Vec<[f32; 2]> = (0..n)
        .map(|i| {
            let (s, c) = regia_core::math::det::sin_cos(std::f32::consts::TAU * i as f32 / n as f32);
            // Counter-clockwise in (r, y) seen from +z, so the bottom-up-outside rule holds.
            [2.0 + 0.5 * c, 0.5 * s]
        })
        .collect();
    ring.push(ring[0]);
    let torus = lathe(&ring, &LatheOptions { segments: 16 }).expect("torus");
    let s = mesh_stats(&torus);
    assert!(s.watertight, "{s:?}");
    // Pappus: 2π R · π r² ≈ 9.87; the faceted torus is a little smaller.
    assert!(s.volume > 9.0 && s.volume < 9.87, "{}", s.volume);
}

#[test]
fn a_concave_star_with_a_bevel_and_box_uvs_is_watertight() {
    let star: Vec<[f32; 2]> = (0..10)
        .map(|i| {
            let r = if i % 2 == 0 { 2.0 } else { 0.9 };
            let (s, c) = regia_core::math::det::sin_cos(std::f32::consts::TAU * i as f32 / 10.0);
            [r * c, r * s]
        })
        .collect();
    let m = extrude(
        &Profile::new(star),
        &ExtrudeOptions {
            depth: 0.6,
            bevel: Some(Bevel {
                width: 0.08,
                segments: 2,
            }),
        },
    )
    .expect("star");
    let s = mesh_stats(&m);
    assert!(s.watertight, "{s:?}");
    assert!(s.volume > 0.0);
    let boxed = uv_box(&m, 0.25, [0.5, 0.5]).expect("uv");
    let sb = mesh_stats(&boxed);
    assert!(sb.watertight);
    let (lo, hi) = sb.uv_bounds.expect("uvs");
    assert!(
        lo[0] >= 0.0 && lo[1] >= 0.0 && hi[0] <= 1.0 && hi[1] <= 1.0,
        "{lo:?} {hi:?}"
    );
}

#[test]
fn a_negative_scale_keeps_faces_outward() {
    let mirrored = scale(&cube(), [-1.0, 2.0, 1.0]);
    let s = mesh_stats(&mirrored);
    assert!(s.watertight);
    assert!((s.volume - 16.0).abs() < 1e-9, "{}", s.volume);
}
