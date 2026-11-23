// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests of the heightfield tools of `regia_gen` (ADR-7280): noise fill, terrace,
//! flatten-to-pad, clamp and normalize, deterministic thermal erosion, slope and normal maps, and
//! export to the engine's terrain heightmap form and to a `CpuMesh` grid. Byte pins as in
//! `textures.rs`.

use regia_gen::{
    Erosion, Fractal, Heightfield, HeightfieldLimits, HeightfieldRecipe, NoiseKind, NoiseParams, Pad,
    build_heightfield, mesh_hash, mesh_stats,
};

fn noise(seed: u64) -> NoiseParams {
    NoiseParams {
        kind: NoiseKind::Gradient,
        seed,
        cells: 4,
        octaves: 5,
        fractal: Fractal::Fbm,
        lacunarity: 2,
        gain: 0.5,
        tile: false,
    }
}

fn hilly(side: u32) -> Heightfield {
    let mut hf = Heightfield::flat(side, side, 1.0).expect("flat");
    hf.add_noise(&noise(42), 40.0, 10.0).expect("noise");
    hf
}

fn sum(hf: &Heightfield) -> f64 {
    hf.heights.iter().map(|&h| f64::from(h)).sum()
}

#[test]
fn noise_fill_spans_base_to_base_plus_amplitude() {
    let hf = hilly(65);
    let s = hf.stats();
    assert!(s.min >= 10.0 && s.max <= 50.0, "{s:?}");
    assert!(s.max - s.min > 10.0, "{s:?}");
}

/// A terrace of a linear ramp has exactly `steps` distinct heights, evenly spaced from the old
/// minimum to the old maximum, and never reorders two samples.
#[test]
fn terrace_has_the_requested_step_count() {
    for steps in [2u32, 3, 5, 8] {
        let mut hf = Heightfield::from_fn(101, 3, 1.0, |x, _| x as f32 * 0.5).expect("ramp");
        hf.terrace(steps, 1.0).expect("terrace");
        let mut distinct: Vec<u32> = hf.heights.iter().map(|h| h.to_bits()).collect();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), steps as usize, "steps {steps}");
        let row: Vec<f32> = hf.heights[..101].to_vec();
        assert!(row.windows(2).all(|w| w[0] <= w[1]), "monotone");
        assert_eq!((row[0], row[100]), (0.0, 50.0));
    }
    let mut hf = hilly(9);
    assert_eq!(
        hf.terrace(1, 1.0).err().map(|e| e.code()),
        Some("gen_invalid_input")
    );
}

/// Every sample inside the pad's radius ends within 1e-6 of the pad height; outside radius plus
/// falloff nothing changes; the falloff band blends between the two.
#[test]
fn a_flattened_pad_is_within_1e6_of_its_height() {
    let before = hilly(129);
    let mut hf = before.clone();
    let pad = Pad {
        center: [10.0, -6.0],
        radius: 12.0,
        falloff: 8.0,
        height: 23.5,
    };
    hf.flatten_pad(&pad).expect("pad");
    let mut inside = 0;
    let mut outside_same = 0;
    for row in 0..hf.height {
        for col in 0..hf.width {
            let [x, z] = hf.sample_position(col, row);
            let d = ((x - pad.center[0]).powi(2) + (z - pad.center[1]).powi(2)).sqrt();
            let i = (row * hf.width + col) as usize;
            if d <= pad.radius {
                assert!(
                    (hf.heights[i] - pad.height).abs() <= 1e-6,
                    "{d}: {}",
                    hf.heights[i]
                );
                inside += 1;
            } else if d >= pad.radius + pad.falloff {
                assert_eq!(hf.heights[i].to_bits(), before.heights[i].to_bits());
                outside_same += 1;
            } else {
                let (lo, hi) = (
                    before.heights[i].min(pad.height),
                    before.heights[i].max(pad.height),
                );
                assert!(hf.heights[i] >= lo - 1e-5 && hf.heights[i] <= hi + 1e-5);
            }
        }
    }
    assert!(inside > 400, "{inside}");
    assert!(outside_same > 10_000, "{outside_same}");
}

#[test]
fn clamp_and_normalize_hit_their_bounds() {
    let mut hf = hilly(33);
    hf.normalize(-1.0, 3.0).expect("normalize");
    let s = hf.stats();
    assert_eq!((s.min, s.max), (-1.0, 3.0));
    hf.clamp(0.0, 1.0).expect("clamp");
    let s = hf.stats();
    assert_eq!((s.min, s.max), (0.0, 1.0));
    assert!(hf.clamp(2.0, 1.0).is_err());
}

/// Thermal erosion conserves the total height within 1e-4 relative, moves material downhill
/// (potential energy Σh² falls, the steepest slope eases, the peak drops and its foot rises), and
/// leaves a field already under the talus slope untouched.
#[test]
fn erosion_conserves_material_and_moves_it_downhill() {
    let mut hf = hilly(97);
    // A sharp spike to erode.
    let peak = (48 * 97 + 48) as usize;
    hf.heights[peak] += 60.0;
    let before = hf.clone();
    let energy = |h: &Heightfield| {
        h.heights
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum::<f64>()
    };
    let report = hf
        .thermal_erode(&Erosion {
            iterations: 50,
            talus: 0.8,
            rate: 0.25,
        })
        .expect("erode");
    let (s0, s1) = (sum(&before), sum(&hf));
    assert!(((s1 - s0) / s0).abs() <= 1e-4, "{s0} -> {s1}");
    assert!(report.moved > 0.0);
    assert!(energy(&hf) < energy(&before));
    assert!(hf.heights[peak] < before.heights[peak] - 20.0);
    assert!(hf.heights[peak + 1] > before.heights[peak + 1]);
    assert!(hf.slope_map().stats().max < before.slope_map().stats().max);

    let mut gentle = Heightfield::from_fn(17, 17, 1.0, |x, y| 0.1 * (x + y) as f32).expect("gentle");
    let still = gentle.clone();
    let report = gentle
        .thermal_erode(&Erosion {
            iterations: 10,
            talus: 0.5,
            rate: 0.25,
        })
        .expect("erode");
    assert_eq!(report.moved, 0.0);
    assert_eq!(gentle, still);
    assert!(
        gentle
            .thermal_erode(&Erosion {
                iterations: 1,
                talus: 0.5,
                rate: 0.3
            })
            .is_err()
    );
}

/// Slope of a tilted plane is its gradient everywhere (edges included); the normal map of a flat
/// field is (128, 128, 255).
#[test]
fn slope_and_normal_maps_of_planes() {
    let hf = Heightfield::from_fn(16, 12, 2.0, |x, _| x as f32 * 1.5).expect("tilt");
    let slope = hf.slope_map();
    for v in &slope.pixels {
        assert!((v - 0.75).abs() < 1e-6, "{v}");
    }
    let n = hf.normals();
    let expect = 1.0 / (1.0f32 + 0.75 * 0.75).sqrt();
    assert!(
        (n[0][1] - expect).abs() < 1e-6 && n[0][0] < 0.0 && n[0][2] == 0.0,
        "{:?}",
        n[0]
    );
    let flat = Heightfield::flat(8, 8, 1.0).expect("flat");
    let map = flat.normal_map();
    assert_eq!(&map.rgba8[..4], &[128, 128, 255, 255]);
}

/// The terrain export quantizes to the cooked 16-bit heightfield with a height scale and offset
/// that rebuild every sample within one quantum; the mesh grid matches `mesh_from_heightfield`'s
/// layout and is (w-1)(h-1)·2 triangles.
#[test]
fn terrain_and_mesh_export_round_trip() {
    let hf = hilly(65);
    let t = hf.to_terrain().expect("terrain");
    assert_eq!((t.heightfield.width, t.heightfield.height), (65, 65));
    assert_eq!(t.world_width, 64.0);
    let quantum = t.height_scale / 65535.0;
    for (q, h) in t.heightfield.samples.iter().zip(&hf.heights) {
        let back = f32::from(*q) / 65535.0 * t.height_scale + t.height_offset;
        assert!((back - h).abs() <= quantum, "{back} vs {h}");
    }
    let engine =
        regia_asset::mesh_from_heightfield(&t.heightfield, t.world_width, t.height_scale, t.height_offset)
            .expect("engine mesh");
    let ours = hf.to_mesh().expect("mesh");
    assert_eq!(ours.positions.len(), engine.positions.len());
    assert_eq!(ours.indices, engine.indices);
    for (a, b) in ours.positions.iter().zip(&engine.positions) {
        assert!(
            (a[0] - b[0]).abs() < 1e-4 && (a[2] - b[2]).abs() < 1e-4 && (a[1] - b[1]).abs() <= quantum + 1e-5
        );
    }
    let stats = mesh_stats(&ours);
    assert_eq!(stats.triangles, 64 * 64 * 2);
    assert_eq!(ours.uvs[64], [1.0, 0.0]);
    // Non-square grids mesh, but the terrain form is square.
    let wide = Heightfield::flat(9, 5, 1.0).expect("wide");
    assert_eq!(wide.to_mesh().expect("mesh").positions.len(), 45);
    assert_eq!(
        wide.to_terrain().err().map(|e| e.code()),
        Some("gen_invalid_input")
    );
}

#[test]
fn heightfield_recipe_builds_and_reports_limits() {
    let recipe: HeightfieldRecipe = serde_json::from_value(serde_json::json!({
        "width": 65, "height": 65, "spacing": 2.0,
        "ops": [
            { "noise": { "noise": { "kind": "value", "seed": 9, "cells": 4, "octaves": 4 }, "amplitude": 30, "base": 5 } },
            { "erode": { "iterations": 10, "talus": 0.7 } },
            { "terrace": { "steps": 6, "strength": 0.5 } },
            { "flatten": { "center": [0, 0], "radius": 10, "falloff": 6, "height": 12 } },
            { "normalize": { "min": 0, "max": 40 } },
            { "clamp": { "min": 2, "max": 38 } }
        ]
    }))
    .expect("parse");
    let hf = build_heightfield(&recipe, &HeightfieldLimits::default()).expect("build");
    let s = hf.stats();
    assert_eq!((s.min, s.max), (2.0, 38.0));
    let big: HeightfieldRecipe = serde_json::from_value(serde_json::json!({
        "width": 1025, "height": 1025, "ops": [ { "erode": { "iterations": 256, "talus": 0.5 } } ]
    }))
    .expect("parse");
    assert_eq!(
        build_heightfield(&big, &HeightfieldLimits::default())
            .err()
            .map(|e| e.code()),
        Some("gen_too_large")
    );
}

#[test]
fn heightfield_byte_pins_hold() {
    let mut eroded = hilly(65);
    eroded
        .thermal_erode(&Erosion {
            iterations: 20,
            talus: 0.6,
            rate: 0.25,
        })
        .expect("erode");
    let mut terraced = hilly(33);
    terraced.terrace(5, 0.75).expect("terrace");
    let pins: [(&str, String, &str); 6] = [
        (
            "hilly",
            hilly(65).hash(),
            "d2b8b908c8c4195df0e634465ff7126e68975588b6242af8ac91ac4b63518a1a",
        ),
        (
            "eroded",
            eroded.hash(),
            "14ddc4dcb4d51ee977927b6930548d4e3d5ed8bcf2137cc908e34eacaf79329d",
        ),
        (
            "terraced",
            terraced.hash(),
            "4f391efc290257cb2bd7c54d4c17b4054cc03e486fbea300fb803a7800f156c2",
        ),
        (
            "terrain_r16",
            regia_gen::r16_hash(&eroded.to_terrain().expect("terrain").heightfield),
            "be2837846cd3ae971b35f5c808b4a33263392b7eb2e80ae3fd07c293d7dc6aad",
        ),
        (
            "mesh",
            mesh_hash(&eroded.to_mesh().expect("mesh")),
            "ca70b11f3b71c3a45478a462cd0918eee345164cb316b1169888c82bc184a515",
        ),
        (
            "normal_map",
            regia_gen::rgba8_hash(&eroded.normal_map()),
            "bab4aeecf0d076791c621cc97323665776a102b304d278a32465d0ff7b1ed1e9",
        ),
    ];
    let moved: Vec<String> = pins
        .iter()
        .filter(|(_, got, want)| got != want)
        .map(|(name, got, _)| format!("{name}: {got}"))
        .collect();
    assert!(moved.is_empty(), "moved pins:\n{}", moved.join("\n"));
}
