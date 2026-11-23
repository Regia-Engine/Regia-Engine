// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests of the texture half of `regia_gen` (ADR-7280): seeded noise (value, gradient,
//! cellular, fBm and ridged octaves, tileable on request), image masks (threshold, levels, blend,
//! blur), mesh-derived edge and cavity masks baked into UV space, and the edge-wear / grime recipe.
//! The byte pins hold on every platform because the code uses integer hashing and exact IEEE
//! operations only; a pin that moves is a determinism or behavior change, never something to
//! re-pin silently.

use regia_gen::{
    BlendMode, CpuMesh, CurvatureOptions, FloatImage, Fractal, Levels, NoiseKind, NoiseParams, NoiseSampler,
    TextureLimits, TextureRecipe, WearOptions, blend, blur, build_texture, curvature_masks, flat_normals,
    image_hash, levels, noise_image, rgba8_hash, threshold, wear_masks,
};

fn noise(kind: NoiseKind, fractal: Fractal, octaves: u32, tile: bool) -> NoiseParams {
    NoiseParams {
        kind,
        seed: 0x5eed,
        cells: 4,
        octaves,
        fractal,
        lacunarity: 2,
        gain: 0.5,
        tile,
    }
}

const KINDS: [NoiseKind; 4] = [
    NoiseKind::Value,
    NoiseKind::Gradient,
    NoiseKind::Cellular,
    NoiseKind::CellularEdge,
];

// ---------------------------------------------------------------------------------------------
// Noise
// ---------------------------------------------------------------------------------------------

/// A tileable image continues across its edges: the sample one pixel past the right edge is the
/// left column, bit for bit, for every kind and both fractals, and the same for rows.
#[test]
fn tileable_noise_edges_match_bit_for_bit() {
    let (w, h) = (48, 40);
    for kind in KINDS {
        for fractal in [Fractal::Fbm, Fractal::Ridged] {
            let s = NoiseSampler::new(&noise(kind, fractal, 4, true), w, h).expect("sampler");
            for y in 0..h as i64 {
                assert_eq!(
                    s.sample(-1, y).to_bits(),
                    s.sample(w as i64 - 1, y).to_bits(),
                    "{kind:?} {fractal:?} row {y}"
                );
                assert_eq!(
                    s.sample(w as i64, y).to_bits(),
                    s.sample(0, y).to_bits(),
                    "{kind:?} {fractal:?} row {y}"
                );
            }
            for x in 0..w as i64 {
                assert_eq!(
                    s.sample(x, -1).to_bits(),
                    s.sample(x, h as i64 - 1).to_bits(),
                    "{kind:?} col {x}"
                );
                assert_eq!(
                    s.sample(x, h as i64 + 3).to_bits(),
                    s.sample(x, 3).to_bits(),
                    "{kind:?} col {x}"
                );
            }
        }
    }
}

/// Without `tile` the field does not repeat at the image size (it is not tileable by accident).
#[test]
fn untiled_noise_does_not_repeat_at_the_image_size() {
    let s = NoiseSampler::new(&noise(NoiseKind::Gradient, Fractal::Fbm, 3, false), 32, 32).expect("sampler");
    let differs = (0..32).filter(|&y| s.sample(-1, y) != s.sample(31, y)).count();
    assert!(differs > 16, "only {differs} rows differ");
}

/// fBm and ridged output stays in [0, 1] and uses most of the range; every kind is non-constant.
#[test]
fn fractal_noise_stays_in_the_unit_range_and_spreads() {
    for kind in KINDS {
        for fractal in [Fractal::Fbm, Fractal::Ridged] {
            let img = noise_image(&noise(kind, fractal, 6, false), 128, 128).expect("noise");
            let s = img.stats();
            assert!(s.min >= 0.0 && s.max <= 1.0, "{kind:?} {fractal:?}: {s:?}");
            assert!(
                s.max - s.min > 0.25,
                "{kind:?} {fractal:?} spread too small: {s:?}"
            );
            assert!(s.mean > 0.05 && s.mean < 0.95, "{kind:?} {fractal:?}: {s:?}");
        }
    }
}

/// The seed changes the field; the same seed gives the same bits.
#[test]
fn noise_is_seeded_and_repeatable() {
    let a = noise_image(&noise(NoiseKind::Value, Fractal::Fbm, 3, true), 32, 32).expect("a");
    let b = noise_image(&noise(NoiseKind::Value, Fractal::Fbm, 3, true), 32, 32).expect("b");
    let mut other = noise(NoiseKind::Value, Fractal::Fbm, 3, true);
    other.seed = 1;
    let c = noise_image(&other, 32, 32).expect("c");
    assert_eq!(image_hash(&a), image_hash(&b));
    assert_ne!(image_hash(&a), image_hash(&c));
}

/// Out-of-range parameters are errors with the protocol's codes.
#[test]
fn bad_noise_parameters_are_errors() {
    let mut p = noise(NoiseKind::Value, Fractal::Fbm, 0, false);
    assert_eq!(
        NoiseSampler::new(&p, 8, 8).err().map(|e| e.code()),
        Some("gen_invalid_input")
    );
    p.octaves = 2;
    p.gain = f32::NAN;
    assert_eq!(
        NoiseSampler::new(&p, 8, 8).err().map(|e| e.code()),
        Some("gen_invalid_input")
    );
    p.gain = 0.5;
    p.cells = 0;
    assert_eq!(
        NoiseSampler::new(&p, 8, 8).err().map(|e| e.code()),
        Some("gen_invalid_input")
    );
    p.cells = 1 << 20;
    p.octaves = 12;
    assert_eq!(
        NoiseSampler::new(&p, 8, 8).err().map(|e| e.code()),
        Some("gen_too_large")
    );
    assert_eq!(
        noise_image(&noise(NoiseKind::Value, Fractal::Fbm, 1, false), 0, 8)
            .err()
            .map(|e| e.code()),
        Some("gen_invalid_input")
    );
}

// ---------------------------------------------------------------------------------------------
// Masks
// ---------------------------------------------------------------------------------------------

fn ramp(w: u32) -> FloatImage {
    FloatImage::from_fn(w, 1, |x, _| x as f32 / (w - 1) as f32).expect("ramp")
}

#[test]
fn threshold_levels_blend_and_blur_have_exact_values() {
    let r = ramp(11);
    let t = threshold(&r, 0.5, 0.0).expect("threshold");
    assert_eq!(t.pixels, vec![0., 0., 0., 0., 0., 1., 1., 1., 1., 1., 1.]);
    let soft = threshold(&r, 0.5, 0.2).expect("soft");
    assert!(
        soft.pixels[2] == 0.0 && soft.pixels[3] < 1e-6,
        "{:?}",
        soft.pixels
    );
    assert!((soft.pixels[5] - 0.5).abs() < 1e-6, "{}", soft.pixels[5]);
    assert!(
        soft.pixels[7] > 1.0 - 1e-6 && soft.pixels[8] == 1.0,
        "{:?}",
        soft.pixels
    );

    let l = levels(
        &r,
        &Levels {
            in_low: 0.2,
            in_high: 0.6,
            gamma: 1.0,
            out_low: 0.0,
            out_high: 1.0,
        },
    )
    .expect("levels");
    assert_eq!(l.pixels[0], 0.0);
    assert_eq!(l.pixels[6], 1.0);
    assert_eq!(l.pixels[10], 1.0);
    assert!((l.pixels[4] - 0.5).abs() < 1e-6, "{}", l.pixels[4]);
    let g = levels(
        &r,
        &Levels {
            gamma: 2.0,
            ..Levels::default()
        },
    )
    .expect("gamma");
    assert!(
        (g.pixels[5] - 0.25).abs() < 1e-6,
        "gamma 2 squares: {}",
        g.pixels[5]
    );

    let half = FloatImage::new(11, 1, 0.5).expect("half");
    let m = blend(&r, &half, BlendMode::Multiply, 1.0).expect("multiply");
    assert_eq!(m.pixels[10], 0.5);
    let mix = blend(&r, &half, BlendMode::Mix, 0.5).expect("mix");
    assert_eq!(mix.pixels[0], 0.25);
    let mx = blend(&r, &half, BlendMode::Max, 1.0).expect("max");
    assert_eq!((mx.pixels[0], mx.pixels[10]), (0.5, 1.0));
    let add = blend(&r, &half, BlendMode::Add, 1.0).expect("add");
    assert_eq!(add.pixels[10], 1.0, "add clamps to 1");
    assert!(blend(&r, &FloatImage::new(10, 1, 0.0).expect("10"), BlendMode::Mix, 1.0).is_err());

    // A box blur of a single bright pixel spreads it evenly over 2r+1 pixels per axis and keeps
    // the total (wrap mode: nothing falls off an edge).
    let mut dot = FloatImage::new(9, 9, 0.0).expect("dot");
    dot.pixels[4 * 9 + 4] = 1.0;
    let b = blur(&dot, 1, 1, true).expect("blur");
    assert!((b.pixels[4 * 9 + 4] - 1.0 / 9.0).abs() < 1e-7);
    assert!((b.pixels[3 * 9 + 3] - 1.0 / 9.0).abs() < 1e-7);
    assert_eq!(b.pixels[2 * 9 + 4], 0.0);
    let total: f64 = b.pixels.iter().map(|&v| f64::from(v)).sum();
    assert!((total - 1.0).abs() < 1e-6, "{total}");
    // Clamp mode at a corner: the bright pixel counts for the clamped samples outside.
    let mut corner = FloatImage::new(4, 4, 0.0).expect("corner");
    corner.pixels[0] = 1.0;
    let bc = blur(&corner, 1, 1, false).expect("clamp blur");
    assert!((bc.pixels[0] - 4.0 / 9.0).abs() < 1e-7, "{}", bc.pixels[0]);
}

/// A unit cube whose six faces each own one tile of a 3 × 2 UV atlas (no overlap), with an inset
/// margin per tile; returns the mesh and, per face, the UV rectangle `(u0, v0, size_u, size_v)`.
fn atlas_cube() -> (CpuMesh, Vec<[f32; 4]>) {
    let boxed = regia_gen::build_recipe(
        &serde_json::from_value(serde_json::json!({ "shape": { "box": { "size": [1, 1, 1] } } }))
            .expect("recipe"),
        &regia_gen::RecipeLimits::default(),
    )
    .expect("box");
    let mut mesh = flat_normals(&boxed);
    let margin = 0.0625f32;
    let mut tiles = vec![[0.0f32; 4]; 6];
    for (i, (p, n)) in mesh.positions.iter().zip(&mesh.normals).enumerate() {
        let axis = (0..3)
            .max_by(|&a, &b| n[a].abs().total_cmp(&n[b].abs()))
            .unwrap_or(0);
        let face = axis * 2 + usize::from(n[axis] < 0.0);
        let (a, b) = match axis {
            0 => (p[2], p[1]),
            1 => (p[0], p[2]),
            _ => (p[0], p[1]),
        };
        let (col, row) = ((face % 3) as f32, (face / 3) as f32);
        let size = [(1.0 - 2.0 * margin) / 3.0, (1.0 - 2.0 * margin) / 2.0];
        let origin = [(col + margin) / 3.0, (row + margin) / 2.0];
        tiles[face] = [origin[0], origin[1], size[0], size[1]];
        mesh.uvs[i] = [origin[0] + (a + 0.5) * size[0], origin[1] + (b + 0.5) * size[1]];
    }
    (mesh, tiles)
}

fn at(img: &FloatImage, uv: [f32; 2]) -> f32 {
    let x = ((uv[0] * img.width as f32) as u32).min(img.width - 1);
    let y = ((uv[1] * img.height as f32) as u32).min(img.height - 1);
    img.pixels[(y * img.width + x) as usize]
}

/// The edge mask of a cube marks all 12 edges (each seen from both of its faces: 24 tile sides)
/// and leaves every face centre at exactly zero; a cube has no concave edge, so its cavity mask
/// is zero everywhere.
#[test]
fn edge_mask_on_a_cube_marks_its_12_edges_and_not_the_face_centres() {
    let (mesh, tiles) = atlas_cube();
    let opts = CurvatureOptions {
        width: 0.1,
        ..CurvatureOptions::default()
    };
    let m = curvature_masks(&mesh, 192, 128, &opts).expect("masks");
    let mut marked_sides = 0;
    for t in &tiles {
        let local = |a: f32, b: f32| [t[0] + a * t[2], t[1] + b * t[3]];
        for (a, b) in [(0.5, 0.02), (0.5, 0.98), (0.02, 0.5), (0.98, 0.5)] {
            let v = at(&m.edge, local(a, b));
            assert!(v > 0.8, "tile {t:?} side ({a}, {b}): {v}");
            marked_sides += 1;
        }
        for a in [0.25f32, 0.4, 0.5, 0.6, 0.75] {
            for b in [0.25f32, 0.4, 0.5, 0.6, 0.75] {
                assert_eq!(at(&m.edge, local(a, b)), 0.0, "face interior ({a}, {b}) of {t:?}");
            }
        }
    }
    assert_eq!(marked_sides, 24, "12 edges, each on two faces");
    assert_eq!(m.cavity.stats().max, 0.0, "a cube has no concave edge");
    assert!(m.edge.stats().max <= 1.0);
}

/// Turned inside out, every cube edge is concave: the cavity mask marks the 12 edges and the edge
/// mask is empty.
#[test]
fn cavity_mask_marks_the_edges_of_an_inside_out_cube() {
    let (mut mesh, tiles) = atlas_cube();
    for t in mesh.indices.as_chunks_mut::<3>().0 {
        t.swap(1, 2);
    }
    for n in &mut mesh.normals {
        *n = n.map(|c| -c);
    }
    let m = curvature_masks(
        &mesh,
        192,
        128,
        &CurvatureOptions {
            width: 0.1,
            ..Default::default()
        },
    )
    .expect("masks");
    assert_eq!(m.edge.stats().max, 0.0);
    for t in &tiles {
        let v = at(&m.cavity, [t[0] + 0.5 * t[2], t[1] + 0.02 * t[3]]);
        assert!(v > 0.8, "{v}");
        assert_eq!(at(&m.cavity, [t[0] + 0.5 * t[2], t[1] + 0.5 * t[3]]), 0.0);
    }
}

#[test]
fn curvature_masks_need_uvs_and_a_positive_width() {
    let (mut mesh, _) = atlas_cube();
    let bad = CurvatureOptions {
        width: 0.0,
        ..Default::default()
    };
    assert_eq!(
        curvature_masks(&mesh, 16, 16, &bad).err().map(|e| e.code()),
        Some("gen_invalid_input")
    );
    mesh.uvs.clear();
    assert_eq!(
        curvature_masks(&mesh, 16, 16, &CurvatureOptions::default())
            .err()
            .map(|e| e.code()),
        Some("gen_invalid_input")
    );
}

// ---------------------------------------------------------------------------------------------
// Wear
// ---------------------------------------------------------------------------------------------

/// Wear sits only on the edges (zero wherever the edge mask is zero), grows with `amount`, is the
/// same for the same seed and moves with the seed.
#[test]
fn edge_wear_stays_on_edges_grows_with_amount_and_is_seeded() {
    let (mesh, tiles) = atlas_cube();
    let opts = |amount: f32, seed: u64| WearOptions {
        seed,
        amount,
        curvature: CurvatureOptions {
            width: 0.15,
            ..Default::default()
        },
        ..WearOptions::default()
    };
    let low = wear_masks(&mesh, 96, 64, &opts(0.3, 7)).expect("low");
    let high = wear_masks(&mesh, 96, 64, &opts(0.8, 7)).expect("high");
    let again = wear_masks(&mesh, 96, 64, &opts(0.3, 7)).expect("again");
    let other = wear_masks(&mesh, 96, 64, &opts(0.3, 8)).expect("other");
    for (w, e) in low.wear.pixels.iter().zip(&low.edge.pixels) {
        if *e == 0.0 {
            assert_eq!(*w, 0.0);
        }
    }
    let worn = |m: &regia_gen::WearMasks| m.wear.pixels.iter().filter(|&&v| v > 0.5).count();
    assert!(worn(&high) > worn(&low), "{} vs {}", worn(&high), worn(&low));
    assert!(worn(&low) > 0);
    assert_eq!(
        rgba8_hash(&low.packed().expect("pack")),
        rgba8_hash(&again.packed().expect("pack"))
    );
    assert_ne!(image_hash(&low.wear), image_hash(&other.wear));
    // Face centres stay clean.
    for t in &tiles {
        assert_eq!(at(&high.wear, [t[0] + 0.5 * t[2], t[1] + 0.5 * t[3]]), 0.0);
    }
    let packed = low.packed().expect("pack");
    assert_eq!(
        (packed.width, packed.height, packed.rgba8.len()),
        (96, 64, 96 * 64 * 4)
    );
}

// ---------------------------------------------------------------------------------------------
// Recipe
// ---------------------------------------------------------------------------------------------

#[test]
fn texture_recipe_builds_layers_and_formats() {
    let recipe: TextureRecipe = serde_json::from_value(serde_json::json!({
        "width": 64, "height": 32,
        "source": { "noise": { "kind": "gradient", "seed": 3, "cells": 4, "octaves": 4, "tile": true } },
        "ops": [
            { "levels": { "in_low": 0.3, "in_high": 0.7 } },
            { "blend": { "mode": "multiply", "layer": { "source": { "constant": { "value": 0.5 } } } } },
            { "blur": { "radius": 1, "wrap": true } }
        ],
        "format": "r16"
    }))
    .expect("parse");
    let out = build_texture(&recipe, &TextureLimits::default()).expect("build");
    assert_eq!((out.image.width, out.image.height), (64, 32));
    assert_eq!(out.bytes().len(), 64 * 32 * 2);
    assert!(out.image.stats().max <= 0.5);
    let too_big: TextureRecipe = serde_json::from_value(serde_json::json!({
        "width": 8192, "height": 8192, "source": { "constant": { "value": 1 } }
    }))
    .expect("parse");
    assert_eq!(
        build_texture(&too_big, &TextureLimits::default())
            .err()
            .map(|e| e.code()),
        Some("gen_too_large")
    );
}

// ---------------------------------------------------------------------------------------------
// Byte pins
// ---------------------------------------------------------------------------------------------

#[test]
fn texture_byte_pins_hold() {
    let (cube, _) = atlas_cube();
    let masks = curvature_masks(
        &cube,
        96,
        64,
        &CurvatureOptions {
            width: 0.1,
            ..Default::default()
        },
    )
    .expect("masks");
    let wear = wear_masks(
        &cube,
        96,
        64,
        &WearOptions {
            seed: 11,
            ..WearOptions::default()
        },
    )
    .expect("wear");
    let blurred = blur(
        &noise_image(&noise(NoiseKind::Value, Fractal::Fbm, 5, true), 64, 64).expect("v"),
        2,
        2,
        true,
    )
    .expect("blur");
    let pins: [(&str, String, &str); 8] = [
        (
            "value_fbm",
            image_hash(&noise_image(&noise(NoiseKind::Value, Fractal::Fbm, 5, true), 64, 64).expect("v")),
            "88d3695bfc293363e107a88487670c158925fc9ebe48e683a0321f44ecfc45d8",
        ),
        (
            "gradient_ridged",
            image_hash(
                &noise_image(&noise(NoiseKind::Gradient, Fractal::Ridged, 5, false), 64, 48).expect("g"),
            ),
            "54a921039abad109a2b76a2fdbab5f50d8f21f5f2efd4654f48c7dadd26a213b",
        ),
        (
            "cellular_fbm",
            image_hash(&noise_image(&noise(NoiseKind::Cellular, Fractal::Fbm, 3, true), 64, 64).expect("c")),
            "979a54e21e914df4368a99b2c7bd3ee99851d7e8f767862b395254081ad5e2e2",
        ),
        (
            "cellular_edge",
            image_hash(
                &noise_image(&noise(NoiseKind::CellularEdge, Fractal::Fbm, 1, false), 48, 48).expect("e"),
            ),
            "9e106031cc53804f21cf94c8c9059f66fc7fbf8fd496c42cfcc91a4e850e3801",
        ),
        (
            "blur_value",
            image_hash(&blurred),
            "f8b61c8eb9f146ab1d0fc619835c081b692abd2e417cbde568b5d9c9a71a639b",
        ),
        (
            "cube_edge",
            image_hash(&masks.edge),
            "56dc1b938ed882b76d0b200c1907f5741c188162175f8bcb638573bf71fa5647",
        ),
        (
            "cube_wear_packed",
            rgba8_hash(&wear.packed().expect("pack")),
            "5581fe7b84a0f568af4349170b59e3854cc0a71aa5f7c478eb14c9d004f56493",
        ),
        (
            "gray_rgba8",
            rgba8_hash(&blurred.to_rgba8()),
            "196df84b20fec037b833ebd8945b89d641e6746c396b463cb606109416ad2f5f",
        ),
    ];
    let moved: Vec<String> = pins
        .iter()
        .filter(|(_, got, want)| got != want)
        .map(|(name, got, _)| format!("{name}: {got}"))
        .collect();
    assert!(moved.is_empty(), "moved pins:\n{}", moved.join("\n"));
}
