// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests of `regia_gen` (ADR-7240): per operation, watertightness, vertex and triangle
//! counts, UV ranges and byte pins (`mesh_hash`). The pins hold on every platform because the crate
//! uses only exact IEEE operations and `regia_core::math::det`; a pin that moves is a determinism
//! or behavior change, never something to re-pin silently.

use regia_gen::{
    Axis, Bevel, CpuMesh, ExtrudeOptions, GenError, LatheOptions, Plane, Profile, Recipe, RecipeLimits,
    build_recipe, clip, extrude, intersect_convex, lathe, merge, mesh_stats, rounded_rect, translate, uv_box,
    uv_planar,
};

fn square(side: f32) -> Vec<[f32; 2]> {
    vec![[0.0, 0.0], [side, 0.0], [side, side], [0.0, side]]
}

fn ngon(n: u32, r: f32) -> Vec<[f32; 2]> {
    (0..n)
        .map(|i| {
            let (s, c) = regia_core::math::det::sin_cos(std::f32::consts::TAU * i as f32 / n as f32);
            [r * c, r * s]
        })
        .collect()
}

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

fn sphere() -> CpuMesh {
    let profile: Vec<[f32; 2]> = (0..=8)
        .map(|j| {
            let (s, c) = regia_core::math::det::sin_cos(
                -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * j as f32 / 8.0,
            );
            [if j == 0 || j == 8 { 0.0 } else { c }, s]
        })
        .collect();
    lathe(&profile, &LatheOptions { segments: 12 }).expect("sphere")
}

fn assert_close(a: [f32; 3], b: [f32; 3]) {
    assert!((0..3).all(|k| (a[k] - b[k]).abs() <= 1e-6), "{a:?} vs {b:?}");
}

fn assert_valid(m: &CpuMesh) {
    m.validate().expect("CpuMesh::validate");
}

// ---------------------------------------------------------------------------------------------
// Extrude
// ---------------------------------------------------------------------------------------------

#[test]
fn extrude_square_is_a_watertight_unit_cube_with_6n_vertices() {
    let m = extrude(
        &Profile::new(square(1.0)),
        &ExtrudeOptions {
            depth: 1.0,
            bevel: None,
        },
    )
    .expect("extrude");
    assert_valid(&m);
    let s = mesh_stats(&m);
    assert!(s.watertight, "{s:?}");
    assert_eq!((s.vertices, s.triangles, s.welded_positions), (24, 12, 8));
    assert!((s.volume - 1.0).abs() < 1e-9, "{}", s.volume);
    assert_eq!(s.bounds, Some(([0.0; 3], [1.0; 3])));
}

#[test]
fn extrude_with_hole_and_either_winding_is_watertight_and_counts_follow_the_formula() {
    // n = 8 points, h = 1 hole: 6n vertices, 2n + 2(n - 2 + 2h) triangles.
    let mut hole = vec![[1.0, 1.0], [3.0, 1.0], [3.0, 3.0], [1.0, 3.0]];
    hole.reverse();
    let p = Profile::new(square(4.0)).with_hole(hole);
    let m = extrude(
        &p,
        &ExtrudeOptions {
            depth: 0.5,
            bevel: None,
        },
    )
    .expect("frame");
    assert_valid(&m);
    let s = mesh_stats(&m);
    assert!(s.watertight, "{s:?}");
    assert_eq!((s.vertices, s.triangles), (48, 16 + 2 * (8 - 2 + 2)));
    assert!((s.volume - 12.0 * 0.5).abs() < 1e-9, "{}", s.volume);
}

#[test]
fn bevel_adds_rings_stays_watertight_and_keeps_the_outer_bounds() {
    let k = 3;
    let m = extrude(
        &Profile::new(square(2.0)),
        &ExtrudeOptions {
            depth: 1.0,
            bevel: Some(Bevel {
                width: 0.1,
                segments: k,
            }),
        },
    )
    .expect("bevel");
    assert_valid(&m);
    let s = mesh_stats(&m);
    assert!(s.watertight, "{s:?}");
    // 2k + 1 strips of 4 quads (4 vertices each) plus two 4-point caps.
    let strips = 2 * k as usize + 1;
    assert_eq!(s.vertices, strips * 4 * 4 + 2 * 4);
    assert_eq!(s.triangles, strips * 4 * 2 + 2 * 2);
    assert_eq!(s.bounds, Some(([0.0, 0.0, 0.0], [2.0, 2.0, 1.0])));
    // The bevel removes material: less than the 4 m³ prism, more than the inset prism.
    assert!(s.volume < 4.0 && s.volume > 1.8 * 1.8 * 1.0, "{}", s.volume);
    // The front cap is inset by exactly the width.
    let front: Vec<_> = m.positions.iter().filter(|p| p[2] == 1.0).collect();
    assert!(front.iter().all(|p| p[0] >= 0.1 && p[0] <= 1.9), "{front:?}");
}

#[test]
fn bevel_at_half_depth_drops_the_wall_and_a_too_wide_bevel_is_an_error() {
    let m = extrude(
        &Profile::new(square(2.0)),
        &ExtrudeOptions {
            depth: 0.4,
            bevel: Some(Bevel {
                width: 0.2,
                segments: 2,
            }),
        },
    )
    .expect("no wall");
    assert!(mesh_stats(&m).watertight);
    assert_eq!(mesh_stats(&m).vertices, 2 * 2 * 4 * 4 + 2 * 4);
    let thin = Profile::new(vec![[0.0, 0.0], [4.0, 0.0], [4.0, 0.2], [0.0, 0.2]]);
    let err = extrude(
        &thin,
        &ExtrudeOptions {
            depth: 2.0,
            bevel: Some(Bevel {
                width: 0.5,
                segments: 1,
            }),
        },
    )
    .expect_err("too wide");
    assert!(matches!(err, GenError::BevelTooWide { .. }), "{err:?}");
}

#[test]
fn a_rounded_box_profile_with_a_narrower_bevel_rounds_every_edge_watertight() {
    let outline = rounded_rect([2.0, 1.0], 0.2, 4).expect("outline");
    assert_eq!(outline.len(), 4 * 5);
    let m = extrude(
        &Profile::new(outline),
        &ExtrudeOptions {
            depth: 1.0,
            bevel: Some(Bevel {
                width: 0.1,
                segments: 4,
            }),
        },
    )
    .expect("rounded box");
    let s = mesh_stats(&m);
    assert!(s.watertight, "{s:?}");
    let (lo, hi) = s.bounds.expect("bounds");
    assert_close(lo, [-1.0, -0.5, 0.0]);
    assert_close(hi, [1.0, 0.5, 1.0]);
}

// ---------------------------------------------------------------------------------------------
// Lathe
// ---------------------------------------------------------------------------------------------

#[test]
fn lathe_closed_profile_is_watertight_with_the_documented_counts() {
    let m = sphere();
    assert_valid(&m);
    let s = mesh_stats(&m);
    assert!(s.watertight, "{s:?}");
    // 2 (m - 1)(segments + 1) vertices; 2 triangles per facet minus one at each pole facet.
    assert_eq!(s.vertices, 2 * 8 * 13);
    assert_eq!(s.triangles, 8 * 12 * 2 - 2 * 12);
    // Faces point outward: a positive volume a little under 4/3 π.
    assert!(
        s.volume > 3.5 && s.volume < 4.0 * std::f64::consts::PI / 3.0,
        "{}",
        s.volume
    );
    let (uv_lo, uv_hi) = s.uv_bounds.expect("uvs");
    assert_eq!((uv_lo, uv_hi), ([0.0, 0.0], [1.0, 1.0]));
}

#[test]
fn lathe_open_profile_has_boundary_edges_and_bad_input_is_an_error() {
    let tube = lathe(&[[1.0, 0.0], [1.0, 2.0]], &LatheOptions { segments: 6 }).expect("tube");
    let s = mesh_stats(&tube);
    assert!(!s.watertight);
    assert_eq!(s.boundary_edges, 12);
    assert_eq!((s.vertices, s.triangles), (2 * 7, 12));
    assert!(lathe(&[[1.0, 0.0], [1.0, 2.0]], &LatheOptions { segments: 2 }).is_err());
    assert!(lathe(&[[-1.0, 0.0], [1.0, 2.0]], &LatheOptions { segments: 6 }).is_err());
}

// ---------------------------------------------------------------------------------------------
// Boolean-lite
// ---------------------------------------------------------------------------------------------

#[test]
fn clipping_a_cube_in_half_with_a_cap_is_watertight_with_half_the_volume() {
    let half = clip(
        &cube(),
        Plane {
            normal: [0.0, 1.0, 0.0],
            offset: 0.0,
        },
        true,
    )
    .expect("clip");
    assert_valid(&half);
    let s = mesh_stats(&half);
    assert!(s.watertight, "{s:?}");
    assert!((s.volume - 4.0).abs() < 1e-9, "{}", s.volume);
    assert_eq!(s.bounds, Some(([-1.0; 3], [1.0, 0.0, 1.0])));
    let open = clip(
        &cube(),
        Plane {
            normal: [0.0, 1.0, 0.0],
            offset: 0.0,
        },
        false,
    )
    .expect("open clip");
    assert_eq!(mesh_stats(&open).boundary_edges, 4 * 2);
}

#[test]
fn an_oblique_clip_of_a_sphere_and_a_tube_with_a_hole_cap_stay_watertight() {
    let cut = clip(
        &sphere(),
        Plane {
            normal: [0.3, 1.0, -0.2],
            offset: 0.25,
        },
        true,
    )
    .expect("sphere cut");
    let s = mesh_stats(&cut);
    assert!(s.watertight, "{s:?}");
    // A thick ring (outer 2, inner 1) cut across its axis: the cap has a hole.
    let ring = extrude(
        &Profile::new(ngon(16, 2.0)).with_hole(ngon(16, 1.0)),
        &ExtrudeOptions {
            depth: 2.0,
            bevel: None,
        },
    )
    .expect("ring");
    let cut = clip(
        &ring,
        Plane {
            normal: [0.0, 0.0, 1.0],
            offset: 1.0,
        },
        true,
    )
    .expect("ring cut");
    let s = mesh_stats(&cut);
    assert!(s.watertight, "{s:?}");
    let full = mesh_stats(&ring).volume;
    assert!(
        (s.volume - full / 2.0).abs() < 1e-4,
        "{} vs {}",
        s.volume,
        full / 2.0
    );
}

#[test]
fn intersecting_a_cube_with_a_convex_wedge_is_watertight() {
    let planes = [
        Plane {
            normal: [1.0, 1.0, 0.0],
            offset: 0.5,
        },
        Plane {
            normal: [0.0, 0.0, 1.0],
            offset: 0.5,
        },
        Plane {
            normal: [-1.0, 0.0, 0.0],
            offset: 0.75,
        },
    ];
    let m = intersect_convex(&cube(), &planes).expect("intersect");
    assert_valid(&m);
    let s = mesh_stats(&m);
    assert!(s.watertight, "{s:?}");
    assert!(s.volume > 0.0 && s.volume < 8.0);
    let (lo, hi) = s.bounds.expect("bounds");
    assert!(hi[2] <= 0.5 && lo[0] >= -0.75, "{lo:?} {hi:?}");
    // Merging two disjoint solids keeps both closed.
    let both = merge(&[&m, &translate(&cube(), [5.0, 0.0, 0.0])]);
    let sb = mesh_stats(&both);
    assert!(sb.watertight);
    assert_eq!(sb.vertices, s.vertices + 24);
    assert!((sb.volume - (s.volume + 8.0)).abs() < 1e-9);
}

#[test]
fn clip_errors_on_a_zero_normal_and_on_a_cut_through_an_open_mesh() {
    let zero = Plane {
        normal: [0.0; 3],
        offset: 0.0,
    };
    assert!(clip(&cube(), zero, true).is_err());
    let tube = lathe(&[[1.0, 0.0], [1.0, 2.0]], &LatheOptions { segments: 6 }).expect("tube");
    // Cut lengthwise: the cut edges end at the tube's open ends.
    let err = clip(
        &tube,
        Plane {
            normal: [1.0, 0.0, 0.0],
            offset: 0.0,
        },
        true,
    )
    .expect_err("open cut");
    assert!(matches!(err, GenError::CapNotClosed { .. }), "{err:?}");
}

// ---------------------------------------------------------------------------------------------
// UV projection
// ---------------------------------------------------------------------------------------------

#[test]
fn box_projection_maps_every_cube_face_onto_the_unit_square() {
    let m = uv_box(&cube(), 0.5, [0.5, 0.5]).expect("uv box");
    assert_valid(&m);
    let s = mesh_stats(&m);
    assert!(s.watertight);
    assert_eq!(s.uv_bounds, Some(([0.0, 0.0], [1.0, 1.0])));
    assert_eq!(s.vertices, 24);
    // Every triangle spans half the unit square in UV.
    for t in m.indices.as_chunks::<3>().0 {
        let [a, b, c] = t.map(|i| m.uvs[i as usize]);
        let area = ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs() / 2.0;
        assert_eq!(area, 0.5);
    }
}

#[test]
fn box_projection_splits_shared_vertices_and_planar_keeps_the_layout() {
    // The sphere's smooth rings share vertices across faces of different dominant axes.
    let s0 = sphere();
    let boxed = uv_box(&s0, 1.0, [0.0, 0.0]).expect("uv box");
    assert!(boxed.positions.len() > s0.positions.len());
    assert_eq!(boxed.indices.len(), s0.indices.len());
    assert!(mesh_stats(&boxed).watertight);
    let (lo, hi) = mesh_stats(&boxed).uv_bounds.expect("uvs");
    assert!(lo[0] >= -1.0 && hi[0] <= 1.0 && lo[1] >= -1.0 && hi[1] <= 1.0);
    let planar = uv_planar(&s0, Axis::Y, 0.5, [0.5, 0.5]).expect("planar");
    assert_eq!(planar.positions, s0.positions);
    assert_eq!(planar.indices, s0.indices);
    let (lo, hi) = mesh_stats(&planar).uv_bounds.expect("uvs");
    assert!(
        lo[0] >= 0.0 && hi[0] <= 1.0 && lo[1] >= 0.0 && hi[1] <= 1.0,
        "{lo:?} {hi:?}"
    );
    assert!(uv_planar(&s0, Axis::X, f32::NAN, [0.0; 2]).is_err());
}

// ---------------------------------------------------------------------------------------------
// Recipes and byte pins
// ---------------------------------------------------------------------------------------------

fn recipe(json: &str) -> Recipe {
    serde_json::from_str(json).expect("recipe json")
}

const BEVELED_BOX: &str = r#"{
    "shape": { "box": { "size": [2, 1, 1], "bevel": { "width": 0.05, "segments": 3 } } },
    "ops": [ { "uv_box": { "scale": 0.5, "offset": [0.5, 0.5] } } ]
}"#;

const CUT_VASE: &str = r#"{
    "shape": { "lathe": { "profile": [[0, 0], [0.6, 0], [0.8, 0.5], [0.4, 1.4], [0.5, 2], [0, 2]], "segments": 24 } },
    "ops": [
        { "clip": { "plane": { "normal": [1, 0, 1], "offset": 0.3 } } },
        { "uv_planar": { "axis": "y", "scale": 0.5 } },
        { "merge": { "recipe": { "shape": { "extrude": { "profile": [[0,0],[1,0],[0.5,1]], "depth": 0.25 } },
                                 "ops": [ { "translate": { "by": [3, 0, 0] } } ] } } }
    ]
}"#;

#[test]
fn recipes_build_watertight_meshes() {
    for json in [BEVELED_BOX, CUT_VASE] {
        let m = build_recipe(&recipe(json), &RecipeLimits::default()).expect("build");
        assert_valid(&m);
        let s = mesh_stats(&m);
        assert!(s.watertight, "{json}: {s:?}");
    }
    let s = mesh_stats(&build_recipe(&recipe(BEVELED_BOX), &RecipeLimits::default()).expect("box"));
    let (lo, hi) = s.bounds.expect("bounds");
    assert_close(lo, [-1.0, -0.5, -0.5]);
    assert_close(hi, [1.0, 0.5, 0.5]);
    let (lo, hi) = s.uv_bounds.expect("uvs");
    assert!(
        lo[0] >= 0.0 && lo[1] >= 0.0 && hi[0] <= 1.0 && hi[1] <= 1.0,
        "{lo:?} {hi:?}"
    );
}

#[test]
fn recipe_limits_and_bad_fields_are_errors() {
    let small = RecipeLimits {
        segments: 16,
        ..RecipeLimits::default()
    };
    let err = build_recipe(&recipe(CUT_VASE), &small).expect_err("segments over the limit");
    assert!(
        matches!(err, GenError::TooLarge { what: "segments", .. }),
        "{err:?}"
    );
    assert!(serde_json::from_str::<Recipe>(r#"{ "shape": { "cone": {} } }"#).is_err());
    assert!(
        serde_json::from_str::<Recipe>(r#"{ "shape": { "box": { "size": [1,1,1] } }, "extra": 1 }"#).is_err()
    );
}

/// The byte pins: each output's `mesh_hash`, and a second build gives the same bits.
#[test]
fn byte_pins_hold() {
    let pins: [(&str, CpuMesh, &str); 5] = [
        (
            "extrude_bevel",
            extrude(
                &Profile::new(square(2.0)).with_hole(vec![[0.5, 0.5], [1.5, 0.5], [1.0, 1.5]]),
                &ExtrudeOptions {
                    depth: 1.0,
                    bevel: Some(Bevel {
                        width: 0.1,
                        segments: 3,
                    }),
                },
            )
            .expect("extrude"),
            "c3804cc04ff2b7373a9d83e65d35cf0f98a7f18d54dfc5c95593a98db369f87f",
        ),
        (
            "lathe_sphere",
            sphere(),
            "dd66ac95f1e24b68e787008ec9a4e23b61070f97cc3d861168176baeb064b762",
        ),
        (
            "clip_sphere",
            clip(
                &sphere(),
                Plane {
                    normal: [0.3, 1.0, -0.2],
                    offset: 0.25,
                },
                true,
            )
            .expect("clip"),
            "8fc5fa329bc78b856bdb9900565f91d96185ff660f173fcdf0b54964750f49f6",
        ),
        (
            "uv_box_sphere",
            uv_box(&sphere(), 1.0, [0.0, 0.0]).expect("uv"),
            "aa7d0a27061ebd849d057e42f9d38d31c20cef8000deaa5b44d1c401a3a2d9ab",
        ),
        (
            "recipe_vase",
            build_recipe(&recipe(CUT_VASE), &RecipeLimits::default()).expect("vase"),
            "b0c7bdd00d6db4c46876b3ed4060613e555d06d520a27331af6cdccc99ec4a7a",
        ),
    ];
    let mut wrong = Vec::new();
    for (name, mesh, pin) in &pins {
        let hash = regia_gen::mesh_hash(mesh);
        if hash != *pin {
            wrong.push(format!("{name}: {hash}"));
        }
    }
    assert!(wrong.is_empty(), "moved pins:\n{}", wrong.join("\n"));
    assert_eq!(
        regia_gen::mesh_hash(&sphere()),
        regia_gen::mesh_hash(&sphere()),
        "a rebuild gives the same bits"
    );
}
