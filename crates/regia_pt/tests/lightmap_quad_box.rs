// SPDX-License-Identifier: MPL-2.0
//! The CPU lightmap baker on its reference scene (lightmaps phase 2, ADR-1101): the floor quad of
//! `quad_box_scene` baked on its own (`bake_mesh_lightmap`) under a white sky and a low sun, with a
//! closed red box standing on it.
//!
//! - Direct light: the open floor gets the sky's `π L` (less what the box hides) plus the sun's
//!   `E cos θ`; in the box's sun shadow the sun is gone.
//! - Indirect light: next to the box's sunlit red face the floor turns red; with one path vertex
//!   (direct only) every texel is exactly grey, because the sky and the sun are white.
//! - Inside geometry: the texels under the box see only its back faces; they are dropped, and
//!   dilation refills the ones near the footprint's edge.
//! - Dilation: the gutters are filled.
//! - Determinism: byte-identical files across thread counts and runs.
//! - The golden (`lightmap_quad_box`): the tone-mapped atlas. Bless with
//!   `REGIA_BLESS=1 cargo test -p regia_pt --test lightmap_quad_box`.
//!
//! CPU only: no GPU is needed.

use regia_asset::lightmap::{Coverage, Lightmap};
use regia_pt::lightmap::{
    LightmapBakeReport, LightmapBakeSettings, LightmapScene, MeshLightmapRequest, QuadBoxScene,
    bake_mesh_lightmap, quad_box_scene,
};
use regia_test_utils::golden::{
    DEFAULT_SSIM_THRESHOLD, GoldenGuard, compare_rgba8_to_png, golden_path, guard_golden,
};

/// Atlas side of the analytic tests: 32 interior texels over 4 m (8 texels/m) and 2-texel gutters.
const SIZE: u32 = 36;
const PAD: u32 = 2;

fn bake(size: u32, settings: &LightmapBakeSettings) -> (Lightmap, LightmapBakeReport) {
    let qb = quad_box_scene();
    let scene = LightmapScene::from_pt_scene(&qb.scene).expect("flatten the quad+box scene");
    let request = MeshLightmapRequest {
        instance: qb.floor,
        name: "floor",
        uv: &qb.floor_uv,
        size: [size, size],
        padding: PAD,
    };
    bake_mesh_lightmap(&scene, &request, settings, |done, total| assert!(done <= total))
        .expect("bake the floor")
}

fn settings(samples: u32, bounces: u32) -> LightmapBakeSettings {
    LightmapBakeSettings {
        samples,
        bounces,
        seed: 7,
        direct_sun: true,
        filter: false,
        threads: 3,
        ..LightmapBakeSettings::default()
    }
}

/// The texel (column or row) of world x (or z) in a `SIZE` atlas.
fn texel(w: f32) -> u32 {
    let interior = (SIZE - 2 * PAD) as f32;
    (PAD as f32 + (w + QuadBoxScene::FLOOR_HALF) / (2.0 * QuadBoxScene::FLOOR_HALF) * interior).floor() as u32
}

/// The open floor's irradiance with nothing in the way: the sky's `π L` and the sun at its
/// elevation.
fn open_floor() -> f32 {
    std::f32::consts::PI * QuadBoxScene::SKY + QuadBoxScene::SUN_LUX * QuadBoxScene::SUN_TOWARD[1]
}

/// Direct and indirect light against their closed forms, the inside-geometry drop and the
/// dilation, with every coverage count derived from the geometry.
#[test]
fn the_floor_matches_its_closed_forms() {
    let (map, report) = bake(SIZE, &settings(64, 3));
    assert_eq!(report.entries, 1);
    assert_eq!(report.contents, ["sky", "sun_bounce", "sun_direct"]);
    assert!(report.rays > 0);

    // 32 × 32 interior texels; the 8 × 8 whose centres lie under the box see only its back faces.
    assert_eq!(report.texels_inside, 64, "{report:?}");
    assert_eq!(report.texels_baked, 32 * 32 - 64);
    // Three dilation rounds fill the 2-texel gutters (272 texels) and all but the 2 × 2 centre of
    // the box's footprint (60 texels).
    assert_eq!(map.coverage_counts(), [4, 32 * 32 - 64, 272 + 60]);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let gutter = x < PAD || y < PAD || x >= SIZE - PAD || y >= SIZE - PAD;
            if gutter {
                assert_eq!(map.coverage_at(x, y), Coverage::DILATED, "gutter ({x}, {y})");
            }
        }
    }
    let under = (texel(0.5), texel(0.0));
    assert_eq!(
        map.coverage_at(under.0, under.1),
        Coverage::EMPTY,
        "the footprint's centre"
    );

    // Far from the box: everything but the sliver of sky the box hides. Green is bounded above by
    // the open floor's value (the box reflects less green than the sky emits).
    let far = map.irradiance_at(texel(-1.5), texel(1.5));
    assert_eq!(map.coverage_at(texel(-1.5), texel(1.5)), Coverage::BAKED);
    assert!(
        far[1] > 0.9 * open_floor() && far[1] <= 1.005 * open_floor(),
        "far {far:?} open {}",
        open_floor()
    );

    // In the box's sun shadow (x from 1 to 1.75): no sun, some sky.
    let shadow = map.irradiance_at(texel(1.35), texel(0.0));
    assert!(shadow[1] < 0.5 * far[1], "shadow {shadow:?} far {far:?}");
    assert!(shadow[1] > 0.5, "the sky still reaches the shadow: {shadow:?}");

    // Next to the box's sunlit red face: red light bounces onto the floor.
    let near = map.irradiance_at(texel(-0.19), texel(0.0));
    assert!(
        near[0] > 1.05 * near[1],
        "no red bounce next to the box: {near:?}"
    );
    assert!(
        far[0] / far[1] < near[0] / near[1],
        "the far floor is less red than the floor next to the box: {far:?} vs {near:?}"
    );
}

/// One path vertex is direct light only: under a white sky and a white sun every texel is grey,
/// and the indirect bake only adds light (the first hemisphere ray of each sample is the same
/// in both bakes, and the indirect part is never negative).
#[test]
fn indirect_light_adds_the_box_colour_and_direct_only_is_grey() {
    let (direct, report) = bake(SIZE, &settings(64, 1));
    assert_eq!(
        report.contents,
        ["sky", "sun_direct"],
        "one vertex holds no sun bounce"
    );
    let (full, _) = bake(SIZE, &settings(64, 3));
    for y in 0..SIZE {
        for x in 0..SIZE {
            if direct.coverage_at(x, y) != Coverage::BAKED {
                continue;
            }
            let d = direct.irradiance_at(x, y);
            assert!(
                d[0] == d[1] && d[1] == d[2],
                "direct-only texel ({x}, {y}) is not grey: {d:?}"
            );
            let f = full.irradiance_at(x, y);
            assert!(
                f.iter().zip(d).all(|(f, d)| *f >= d * 0.995),
                "indirect light removed light at ({x}, {y}): {f:?} vs {d:?}"
            );
        }
    }
    let at = (texel(-0.19), texel(0.0));
    let (d, f) = (direct.irradiance_at(at.0, at.1), full.irradiance_at(at.0, at.1));
    assert!(f[0] > d[0] + 1.0, "the red face bounces light: {f:?} vs {d:?}");
    assert!(
        f[0] - d[0] > 4.0 * (f[1] - d[1]),
        "the bounce is red: {f:?} vs {d:?}"
    );
}

/// The same settings give the same bytes whatever the thread count and however often it runs;
/// another seed gives other bytes.
#[test]
fn the_bake_is_byte_identical_across_threads_and_runs() {
    let one = LightmapBakeSettings {
        threads: 1,
        ..settings(16, 3)
    };
    let four = LightmapBakeSettings {
        threads: 4,
        ..settings(16, 3)
    };
    let (a, ra) = bake(SIZE, &one);
    let (b, rb) = bake(SIZE, &four);
    let (c, _) = bake(SIZE, &four);
    let bytes = |m: &Lightmap| m.to_bytes().expect("encode");
    assert_eq!(bytes(&a), bytes(&b), "thread count changed the file");
    assert_eq!(bytes(&b), bytes(&c), "a second run changed the file");
    assert_eq!(ra, rb);
    let reseeded = LightmapBakeSettings { seed: 8, ..four };
    let (d, _) = bake(SIZE, &reseeded);
    assert_ne!(bytes(&a), bytes(&d), "the seed is part of the bake");
}

/// Linear to sRGB, 8 bits.
fn srgb8(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
}

/// The atlas as an image: irradiance × 1/16 (the open floor, about 11 lux, lands near 0.7), sRGB;
/// empty texels black.
fn tone_map(map: &Lightmap) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((map.width * map.height * 4) as usize);
    for y in 0..map.height {
        for x in 0..map.width {
            let e = if map.coverage_at(x, y) == Coverage::EMPTY {
                [0.0; 3]
            } else {
                map.irradiance_at(x, y)
            };
            rgba.extend(e.map(|c| srgb8(c / 16.0)));
            rgba.push(255);
        }
    }
    rgba
}

/// The golden: the floor's 68 × 68 atlas (16 texels/m) at 32 samples, three vertices, the
/// filter on, tone-mapped. Bless with `REGIA_BLESS=1 cargo test -p regia_pt --test
/// lightmap_quad_box` (the reason goes in the commit message).
#[test]
fn quad_box_golden() {
    let size = 68;
    let s = LightmapBakeSettings {
        filter: true,
        ..settings(32, 3)
    };
    let (map, _) = bake(size, &s);
    let rgba = tone_map(&map);
    guard_golden(
        &rgba,
        size,
        size,
        "lightmap_quad_box",
        GoldenGuard {
            min_lum: 60.0,
            max_lum: 235.0,
            max_white_pct: 5.0,
            max_black_pct: 5.0,
            min_object_pct: 95.0,
        },
    );
    compare_rgba8_to_png(
        &rgba,
        size,
        size,
        &golden_path("lightmap_quad_box"),
        DEFAULT_SSIM_THRESHOLD,
    )
    .expect("lightmap_quad_box golden (bless with REGIA_BLESS=1 if the change is intended)");
}
