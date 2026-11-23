// SPDX-License-Identifier: MPL-2.0
//! ADR-538 robustness: hostile bytes through a loader *and* the runtime code that consumes what
//! the loader accepts.
//!
//! The per-crate fuzz tests (`regia_asset/tests/fuzz.rs`, `regia_scene/tests/fuzz.rs`) stop
//! at the parse. A file can parse and validate and still carry values that crash the first
//! consumer: a terrain layer without columns, a brick with a zero count. These targets run the
//! renderer's CPU mirror of the baked GI lookup over every decoded `.fgiv` mutation, and the
//! renderer's material conversion over every `.material.ron` mutation. No GPU.

use regia_asset::MaterialAssetRon;
use regia_asset::baked_gi::{
    BakeInfo, BakeLights, BakedGiError, BakedGiVolume, DetailBrick, ProbeData, ProbeFlags, ProbeMeta,
    TerrainLayer, VolumeLayout,
};
use regia_render::{BakedGiFrameLights, BakedGiSettings, LabMaterial, component_scales, sample_static_gi};
use regia_test_utils::fuzz::{assert_no_panic, fuzz_loader, fuzz_text_loader};

/// Iterations per target.
const N: usize = 2_000;

/// Where the sampler is queried: inside the fixture volume, on its faces, far outside it, and at
/// non-finite points (a gameplay query can pass any of these).
const POINTS: [[f32; 3]; 8] = [
    [0.0, 1.0, 0.0],
    [-4.0, 0.5, -4.0],
    [4.0, 3.0, 4.0],
    [0.5, 0.75, 0.5],
    [1.0e6, -1.0e6, 1.0e6],
    [f32::NAN, 1.0, 0.0],
    [f32::INFINITY, f32::NEG_INFINITY, 0.0],
    [f32::NAN, f32::NAN, f32::NAN],
];

/// Surface normals of the queries, including a zero and a non-finite one.
const NORMALS: [[f32; 3]; 3] = [[0.0, 1.0, 0.0], [0.0, 0.0, 0.0], [f32::NAN, 0.0, 1.0]];

/// A small valid volume: a 2×2-column terrain layer of two levels and one 2×1×2 detail brick.
fn fixture_volume() -> BakedGiVolume {
    let mut layout = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [-4.0, -4.0],
            spacing: [8.0, 8.0],
            counts: [2, 2],
            levels: vec![1.0, 3.0],
            ground: vec![0.0, 0.5, 1.0, 1.5],
        }),
        bricks: vec![DetailBrick {
            origin: [-1.0, 0.5, -1.0],
            spacing: [2.0, 1.0, 2.0],
            counts: [2, 1, 2],
            first_probe: 0,
            flags: 0,
        }],
        moment_resolution: 2,
        max_distance: 8.0,
    };
    layout.assign_brick_offsets();
    probes_for(&layout)
}

/// `layout` with every probe valid, slightly relocated, and lit.
fn probes_for(layout: &VolumeLayout) -> BakedGiVolume {
    let texels = (layout.moment_resolution * layout.moment_resolution) as usize;
    let probes: Vec<ProbeData> = (0..layout.probe_count())
        .map(|i| ProbeData {
            sh: [[[0.5 + i as f32 * 0.1, 0.1, -0.1, 0.05]; 3]; 4],
            moments: vec![[1.0, 2.0]; texels],
            meta: ProbeMeta {
                offset: [0.1, 0.0, -0.1],
                flags: ProbeFlags::VALID,
            },
        })
        .collect();
    let lights = BakeLights {
        sun_illuminance: 1000.0,
        fill_lux: 1.0,
        ..BakeLights::default()
    };
    BakedGiVolume::from_probes(layout, lights, BakeInfo::default(), &probes).expect("valid layout")
}

/// Samples the volume at every point and normal of the fixture grid; returns the summed
/// coverage (so the work is not optimized away).
fn sample_everywhere(volume: &BakedGiVolume) -> f32 {
    let settings = BakedGiSettings::default();
    let lights = BakedGiFrameLights {
        sun: Some([1000.0; 3]),
        to_sun: Some([0.0, 1.0, 0.0]),
        fill: Some([1.0; 3]),
    };
    let scales = component_scales(volume, &settings, &lights);
    let mut coverage = 0.0;
    for p in POINTS {
        for n in NORMALS {
            coverage += sample_static_gi(volume, &settings, &scales, p, n, [0.0, 0.0, 1.0]).coverage;
        }
    }
    coverage
}

/// The `.fgiv` reader followed by the lookup the runtime runs on what it accepted.
fn decode_and_sample(bytes: &[u8]) -> Result<f32, BakedGiError> {
    BakedGiVolume::from_bytes(bytes).map(|volume| sample_everywhere(&volume))
}

#[test]
fn fgiv_decode_then_sample_never_panics() {
    let fixture = fixture_volume().to_bytes().expect("v2 bytes");
    assert!(
        decode_and_sample(&fixture).expect("fixture decodes") > 0.0,
        "the fixture is lit"
    );
    fuzz_loader(&fixture, 0x0005_3811, N, decode_and_sample);
    let v1 = fixture_volume().to_bytes_v1().expect("v1 bytes");
    fuzz_loader(&v1, 0x0005_3812, N, decode_and_sample);
}

/// Layouts the reader accepts but no bake writes: a terrain layer without columns, and a
/// zero-count brick ahead of a real one. A non-finite query point used to reach `f32::clamp`
/// with crossed bounds (or `counts - 1` on 0) in both.
#[test]
fn sampler_survives_degenerate_valid_volumes() {
    let no_columns = VolumeLayout {
        terrain: Some(TerrainLayer {
            origin: [0.0, 0.0],
            spacing: [1.0, 1.0],
            counts: [0, 0],
            levels: vec![1.0],
            ground: Vec::new(),
        }),
        bricks: Vec::new(),
        moment_resolution: 2,
        max_distance: 4.0,
    };
    let brick = |counts: [u32; 3]| DetailBrick {
        origin: [0.0; 3],
        spacing: [1.0; 3],
        counts,
        first_probe: 0,
        flags: 0,
    };
    let mut zero_brick = VolumeLayout {
        terrain: None,
        bricks: vec![brick([0, 2, 2]), brick([2, 2, 2])],
        moment_resolution: 2,
        max_distance: 4.0,
    };
    zero_brick.assign_brick_offsets();
    let mut flat_brick = VolumeLayout {
        terrain: None,
        bricks: vec![brick([1, 1, 1])],
        moment_resolution: 1,
        max_distance: 4.0,
    };
    flat_brick.assign_brick_offsets();
    for (what, layout) in [
        ("terrain without columns", no_columns),
        ("zero-count brick", zero_brick),
        ("single-probe brick", flat_brick),
    ] {
        let volume = probes_for(&layout);
        // The volume also survives its own file round trip unchanged.
        let back = BakedGiVolume::from_bytes(&volume.to_bytes().expect("bytes")).expect("reads back");
        assert_eq!(back, volume, "{what}");
        assert_no_panic(what, || sample_everywhere(&volume));
    }
}

/// The material loader followed by the renderer's conversion of what it accepted.
fn load_and_convert(source: &str) -> Result<f32, regia_asset::MaterialError> {
    let material = MaterialAssetRon::from_ron(source.as_bytes())?;
    Ok(LabMaterial::from_authored(&material).roughness)
}

#[test]
fn material_ron_load_then_convert_never_panics() {
    let full = include_str!("../../regia_asset/tests/fixtures/material-full.example.ron");
    assert!(load_and_convert(full).is_ok(), "the full fixture loads");
    fuzz_text_loader(full, 0x0005_3813, N, load_and_convert);
    let v1 = include_str!("../../regia_asset/tests/fixtures/material.example.ron");
    fuzz_text_loader(v1, 0x0005_3814, N, load_and_convert);
}
