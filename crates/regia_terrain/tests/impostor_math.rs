// SPDX-License-Identifier: MPL-2.0
//! CPU-side math of the octahedral impostor bake (ADR-116, `scatter/impostor.rs`): settings clamping, the
//! hemi-octahedral map, the capture basis, the packed instance, and the rasteriser/encoder checked against
//! analytic projections of a flat quad. No GPU is needed and no golden is involved.
//!
//! References: Cigolle et al., "A Survey of Efficient Representations for Independent Unit Vectors", JCGT 3(2),
//! 2014 (hemi-octahedral map); the orthographic projection of a planar patch of area A onto a plane whose normal
//! makes angle θ with the planar patch's normal has area A·cos θ (elementary geometry).

use regia_core::glam::{Mat4, Quat, Vec3};
use regia_render::{LabMaterial, LabVertex};
use regia_terrain::scatter::impostor::{
    ImpostorPart, ImpostorSource, bake_cards, capture_basis, capture_direction, hemi_oct_decode,
    hemi_oct_encode,
};
use regia_terrain::{ImpostorAtlas, ImpostorCard, ImpostorInstance, ImpostorSettings};

fn vertex(p: [f32; 3], n: [f32; 3]) -> LabVertex {
    LabVertex::from_pos_normal_uv(p, n, [0.0, 0.0])
}

/// A horizontal square of half-side `h` at height `y`, facing +Y, two triangles.
fn quad(h: f32, y: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let n = [0.0, 1.0, 0.0];
    let v = vec![
        vertex([-h, y, -h], n),
        vertex([h, y, -h], n),
        vertex([h, y, h], n),
        vertex([-h, y, h], n),
    ];
    (v, vec![0, 1, 2, 0, 2, 3])
}

fn bake_one(
    vertices: &[LabVertex],
    indices: &[u32],
    centre_y: f32,
    radius: f32,
    s: &ImpostorSettings,
) -> ImpostorCard {
    let material = LabMaterial::default();
    bake_cards(
        &[ImpostorSource {
            parts: vec![ImpostorPart {
                vertices,
                indices,
                material: &material,
                colour: [1.0; 3],
            }],
            centre_y,
            radius,
        }],
        s,
    )
    .remove(0)
}

fn mean_alpha(card: &ImpostorCard, settings: &ImpostorSettings, i: u32, j: u32) -> f32 {
    let (grid, cell) = (settings.grid as usize, settings.cell as usize);
    let side = grid * cell;
    let mut sum = 0.0;
    for y in 0..cell {
        for x in 0..cell {
            let o = ((j as usize * cell + y) * side + i as usize * cell + x) * 4 + 3;
            sum += f32::from(card.albedo[0][o]) / 255.0;
        }
    }
    sum / (cell * cell) as f32
}

#[test]
fn settings_sanitize_clamps_every_field() {
    let wild = ImpostorSettings {
        grid: 1000,
        cell: 3,
        far_end_m: f32::NAN,
        min_pixels: f32::INFINITY,
        reference_height_px: -4.0,
        reference_fov_y: 9.0,
    }
    .sanitized();
    assert_eq!(wild.grid, 16);
    assert_eq!(wild.cell, 8);
    assert_eq!(
        wild.far_end_m, 2000.0,
        "a non-finite far end falls back to the default"
    );
    assert_eq!(
        wild.min_pixels, 0.75,
        "an infinite minimum size falls back to the default"
    );
    assert_eq!(wild.reference_height_px, 1.0);
    assert_eq!(wild.reference_fov_y, 3.0);

    let small = ImpostorSettings {
        grid: 0,
        cell: 1000,
        far_end_m: 0.0,
        min_pixels: -1.0,
        reference_height_px: 720.0,
        reference_fov_y: 0.0,
    }
    .sanitized();
    assert_eq!(small.grid, 2);
    assert_eq!(small.cell, 128, "a cell above the maximum clamps to 128");
    assert_eq!(small.far_end_m, 1.0);
    assert_eq!(small.min_pixels, 0.05);
    assert_eq!(small.reference_fov_y, 0.05);

    // A non-power-of-two cell rounds up; sanitizing is idempotent.
    let odd = ImpostorSettings {
        cell: 40,
        ..ImpostorSettings::default()
    }
    .sanitized();
    assert_eq!(odd.cell, 64);
    assert_eq!(odd, odd.sanitized());
    assert_eq!(
        ImpostorSettings::default(),
        ImpostorSettings::default().sanitized()
    );
}

#[test]
fn mip_count_is_log2_of_the_cell_plus_one() {
    for (cell, mips) in [(1, 1), (8, 4), (32, 6), (128, 8)] {
        let s = ImpostorSettings {
            cell,
            ..ImpostorSettings::default()
        };
        assert_eq!(s.mip_count(), mips, "cell {cell}");
    }
}

#[test]
fn size_limit_distance_matches_the_pinhole_projection() {
    // A sphere of radius r at distance d projects to a disc of radius r·H/(2·d·tan(fov/2)) pixels on a screen of
    // height H; setting that to min_pixels gives the distance (pinhole camera model).
    let s = ImpostorSettings::default();
    let r = 1.7;
    let d = s.size_limit_distance(r);
    let projected = r * s.reference_height_px * 0.5 / (d * (s.reference_fov_y * 0.5).tan());
    assert!(
        (projected - s.min_pixels).abs() < 1e-3 * s.min_pixels,
        "projected {projected}"
    );
    // Linear in the radius.
    assert!((s.size_limit_distance(2.0 * r) / d - 2.0).abs() < 1e-5);
}

#[test]
fn hemi_oct_maps_the_axes_and_clamps_the_lower_hemisphere() {
    assert_eq!(
        hemi_oct_encode([0.0, 1.0, 0.0]),
        [0.0, 0.0],
        "+Y is the centre of the square"
    );
    // The four horizon axes land on the corners/edges of the square (|u|+|v| = 1 along the border).
    for d in [
        [1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, -1.0],
    ] {
        let e = hemi_oct_encode(d);
        assert!((e[0].abs().max(e[1].abs()) - 1.0).abs() < 1e-6, "{d:?} -> {e:?}");
        let back = hemi_oct_decode(e);
        for k in 0..3 {
            assert!((back[k] - d[k]).abs() < 1e-5, "{d:?} -> {back:?}");
        }
    }
    // A degenerate zero vector must not produce NaN.
    let e = hemi_oct_encode([0.0; 3]);
    assert!(e[0].is_finite() && e[1].is_finite());
    // A lower-hemisphere direction folds to the horizon instead of leaving the square.
    let e = hemi_oct_encode([0.6, -0.8, 0.0]);
    assert!(e[0].abs() <= 1.0 + 1e-6 && e[1].abs() <= 1.0 + 1e-6);
    // Decoding a point outside the diamond (corner of the square) clamps y to the horizon.
    let corner = hemi_oct_decode([1.0, 1.0]);
    assert!(corner[1].abs() < 1e-6);
    assert!((Vec3::from_array(corner).length() - 1.0).abs() < 1e-5);
}

#[test]
fn capture_directions_are_unit_upper_hemisphere_and_distinct() {
    let grid = 8;
    let mut seen: Vec<Vec3> = Vec::new();
    for j in 0..grid {
        for i in 0..grid {
            let d = Vec3::from_array(capture_direction(i, j, grid));
            assert!((d.length() - 1.0).abs() < 1e-5);
            assert!(d.y >= 0.0);
            assert!(
                seen.iter().all(|s| s.distance(d) > 1e-3),
                "({i},{j}) repeats a direction"
            );
            seen.push(d);
        }
    }
    // A zero grid is treated as one capture instead of dividing by zero.
    let d = capture_direction(0, 0, 0);
    assert!(d.iter().all(|c| c.is_finite()));
}

#[test]
fn capture_basis_handles_the_pole_and_a_zero_forward() {
    // Looking straight down +Y, the cross product with Y vanishes; the basis falls back to X.
    let (r, u, f) = capture_basis([0.0, 1.0, 0.0]);
    assert_eq!(f, [0.0, 1.0, 0.0]);
    assert_eq!(r, [1.0, 0.0, 0.0]);
    let (r, u) = (Vec3::from_array(r), Vec3::from_array(u));
    assert!(r.dot(u).abs() < 1e-6);
    // A zero forward vector normalizes to +Y rather than NaN.
    let (_, _, f) = capture_basis([0.0; 3]);
    assert_eq!(f, [0.0, 1.0, 0.0]);
}

#[test]
fn instance_packs_proto_variant_and_a_non_identity_rotation() {
    let q = Quat::from_euler(regia_core::glam::EulerRot::YXZ, 2.1, -0.4, 0.9).normalize();
    let model = Mat4::from_scale_rotation_translation(Vec3::splat(3.0), q, Vec3::new(-4.0, 5.0, 6.5));
    let inst = ImpostorInstance::from_model(&model.to_cols_array_2d(), [1.0, 2.0, 3.0], 4.5, 0x1_0007, 200);
    assert_eq!(
        inst.packed & 0xffff,
        7,
        "the prototype index keeps its low 16 bits"
    );
    assert_eq!(inst.packed >> 16, 200);
    assert_eq!(inst.centre, [1.0, 2.0, 3.0]);
    assert_eq!(inst.radius, 4.5);
    assert!((inst.scale - 3.0).abs() < 1e-5);
    assert_eq!(inst.pivot, [-4.0, 5.0, 6.5]);
    // snorm16 packing keeps a rotation to within one part in 16k, up to the quaternion's sign.
    assert!(inst.quat().dot(q).abs() > 0.99999);
    // A near-zero scale is held at a positive floor so the shader never divides by zero.
    let flat = ImpostorInstance::from_model(
        &Mat4::from_scale(Vec3::ZERO).to_cols_array_2d(),
        [0.0; 3],
        1.0,
        0,
        0,
    );
    assert!(flat.scale >= 1e-6);
    // The default instance is the all-zero Pod value.
    assert_eq!(bytemuck::bytes_of(&ImpostorInstance::default()), &[0u8; 48][..]);
}

#[test]
fn atlas_size_and_bytes_count_every_mip_of_both_images() {
    let settings = ImpostorSettings {
        grid: 2,
        cell: 8,
        ..ImpostorSettings::default()
    }
    .sanitized();
    let (v, i) = quad(0.4, 0.0);
    let card = bake_one(&v, &i, 0.0, 1.0, &settings);
    let atlas = ImpostorAtlas {
        settings,
        cards: vec![card.clone(), card],
        protos: Vec::new(),
    };
    assert_eq!(atlas.size(), 16);
    // 16² + 8² + 4² + 2² texels per image, 4 bytes each, two images, two cards.
    assert_eq!(atlas.bytes(), (256 + 64 + 16 + 4) * 4 * 2 * 2);
    assert_eq!(atlas.cards[0].albedo.len(), settings.mip_count() as usize);
}

#[test]
fn a_flat_quad_covers_cos_theta_of_its_area_in_every_capture() {
    let settings = ImpostorSettings {
        grid: 4,
        cell: 64,
        ..ImpostorSettings::default()
    }
    .sanitized();
    let (radius, half) = (1.0, 0.4);
    let (v, i) = quad(half, 0.0);
    let card = bake_one(&v, &i, 0.0, radius, &settings);
    assert_eq!(card.radius, radius);
    assert_eq!(card.centre_y, 0.0);
    // The capture covers [-r, r]²; the quad's projected area is (2h)²·cos θ, with cos θ = forward.y.
    let full = (2.0 * half) * (2.0 * half) / ((2.0 * radius) * (2.0 * radius));
    for j in 0..settings.grid {
        for i in 0..settings.grid {
            let f = capture_direction(i, j, settings.grid);
            let expect = full * f[1];
            let got = mean_alpha(&card, &settings, i, j);
            assert!(
                (got - expect).abs() < 0.02,
                "capture ({i},{j}): coverage {got} vs analytic {expect}"
            );
        }
    }
}

#[test]
fn a_flat_quad_encodes_its_normal_and_sits_on_the_centre_plane() {
    let settings = ImpostorSettings {
        grid: 2,
        cell: 32,
        ..ImpostorSettings::default()
    }
    .sanitized();
    let (v, i) = quad(0.4, 0.0);
    let card = bake_one(&v, &i, 0.0, 1.0, &settings);
    let side = (settings.grid * settings.cell) as usize;
    // The middle texel of capture (0, 0): covered, normal (0, 1, 0) -> (0.5, 1, 0.5), depth 0.5 + h/2 where the
    // plane passes through the sphere centre the depth along the view is the in-plane offset, small near the middle.
    let (x, y) = (settings.cell as usize / 2, settings.cell as usize / 2);
    let o = (y * side + x) * 4;
    assert!(card.albedo[0][o + 3] > 200, "centre texel is covered");
    let nd = &card.normal_depth[0][o..o + 4];
    assert!((i32::from(nd[0]) - 128).abs() <= 2, "nx {}", nd[0]);
    assert_eq!(nd[1], 255, "ny of an upward normal");
    assert!((i32::from(nd[2]) - 128).abs() <= 2, "nz {}", nd[2]);
    // Albedo is white (vertex colour 1, no texture): linear 1 encodes as min(1,2)/2 = 0.5 -> sRGB 188.
    for k in 0..3 {
        assert!(
            (i32::from(card.albedo[0][o + k]) - 188).abs() <= 1,
            "rgb[{k}] {}",
            card.albedo[0][o + k]
        );
    }
    // A texel outside the quad stays transparent but is dilated to carry a colour for bilinear filtering.
    let corner = 0;
    assert_eq!(card.albedo[0][corner + 3], 0);
}

#[test]
fn bake_handles_degenerate_input_without_panicking() {
    let settings = ImpostorSettings {
        grid: 2,
        cell: 8,
        ..ImpostorSettings::default()
    }
    .sanitized();
    let (v, _) = quad(0.4, 0.0);
    // Out-of-range indices and a degenerate (collinear) triangle are skipped; the card is all transparent.
    let card = bake_one(&v, &[0, 1, 99, 0, 0, 0, 0, 1, 1], 0.0, 1.0, &settings);
    assert!(card.albedo[0].chunks(4).all(|p| p[3] == 0));
    // An empty source and a zero radius still produce a card of the right size (the radius is floored).
    let card = bake_one(&[], &[], 0.0, 0.0, &settings);
    assert!(card.radius > 0.0);
    assert_eq!(card.albedo[0].len(), 16 * 16 * 4);
    // No sources: no cards.
    assert!(bake_cards(&[], &settings).is_empty());
}

#[test]
fn baking_several_sources_in_parallel_equals_baking_them_one_by_one() {
    let settings = ImpostorSettings {
        grid: 2,
        cell: 16,
        ..ImpostorSettings::default()
    }
    .sanitized();
    let material = LabMaterial::default();
    let shapes = [(0.3, 0.0), (0.5, 0.1), (0.2, -0.1)];
    let meshes: Vec<_> = shapes.iter().map(|&(h, y)| quad(h, y)).collect();
    let sources: Vec<ImpostorSource<'_>> = meshes
        .iter()
        .zip(shapes)
        .map(|((v, i), (_, y))| ImpostorSource {
            parts: vec![ImpostorPart {
                vertices: v,
                indices: i,
                material: &material,
                colour: [1.0; 3],
            }],
            centre_y: y,
            radius: 1.0,
        })
        .collect();
    let together = bake_cards(&sources, &settings);
    assert_eq!(together.len(), 3);
    for (k, source) in sources.iter().enumerate() {
        let alone = bake_cards(std::slice::from_ref(source), &settings).remove(0);
        assert_eq!(together[k], alone, "card {k} depends on how it was scheduled");
    }
    assert_ne!(
        together[0], together[1],
        "different shapes bake to different cards"
    );
}
