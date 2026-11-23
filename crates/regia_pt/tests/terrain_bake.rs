// SPDX-License-Identifier: MPL-2.0
//! The terrain albedo bake renders the real-time terrain shader's own blended albedo
//! (`debug_view` 3) over the whole map in the heightfield's UV layout (row 0 at −Z).

#![allow(clippy::needless_range_loop)]

use regia_asset::Heightfield;
use regia_pt::terrain_bake::{AlbedoBake, bake_terrain_albedo};
use regia_render::RenderDevice;
use regia_terrain::cdlod::CdlodConfig;
use regia_terrain::{LayerDesc, TerrainLayers, TerrainMap, TerrainSharedDraw};

fn layer(name: &str, albedo: [f32; 3], altitude: (f32, f32)) -> LayerDesc {
    LayerDesc {
        name: name.into(),
        albedo,
        roughness: 0.9,
        tiling: 4.0,
        slope_min: 0.0,
        slope_max: 2.0,
        altitude_min: altitude.0,
        altitude_max: altitude.1,
        // Sharp bands: the legacy slope band starts at slope 0 with zero weight, so the
        // map is tilted 3° and the ramp has to be short.
        blend_sharpness: 0.01,
        triplanar: false,
        ..LayerDesc::ground()
    }
}

#[test]
fn the_bake_is_the_shaders_albedo_in_heightfield_uv() {
    let device = RenderDevice::headless().expect("headless device");
    // 128 m square: the −Z half at 0 m, the +Z half on a 20 m plateau.
    let n = 65u32;
    let world = 128.0f32;
    let (scale, offset) = (40.0f32, -8.0f32);
    let samples = (0..n * n)
        .map(|i| {
            let (row, col) = (i / n, i % n);
            let tilt = col as f32 / (n - 1) as f32 * world * 0.05;
            let h = tilt
                + if row < n / 2 - 4 {
                    0.0
                } else if row > n / 2 + 4 {
                    20.0
                } else {
                    (row as f32 - (n / 2 - 4) as f32) / 8.0 * 20.0
                };
            ((h - offset) / scale * 65535.0).round() as u16
        })
        .collect();
    let map = TerrainMap::new(
        &Heightfield {
            width: n,
            height: n,
            samples,
        },
        world,
        scale,
        offset,
    )
    .expect("map");
    let layers = TerrainLayers {
        layers: vec![
            layer("low", [0.6, 0.2, 0.1], (-50.0, 8.0)),
            layer("high", [0.1, 0.25, 0.6], (12.0, 80.0)),
        ],
        ..Default::default()
    };
    let mut draw =
        TerrainSharedDraw::new(&device, map, &layers, CdlodConfig::with_grid_size(8), 3).expect("draw");
    let bake = AlbedoBake {
        texel_m: 1.0,
        tile_px: 48,
        view_height_m: 150.0,
    };
    let tex = bake_terrain_albedo(&mut draw, &device, &bake).expect("bake");
    assert_eq!((tex.width, tex.height), (128, 128));
    assert!(tex.srgb);
    let mean = |rows: std::ops::Range<u32>| {
        let mut s = [0.0f64; 3];
        let mut k = 0.0;
        for y in rows {
            for x in 8..120 {
                let o = ((y * tex.width + x) * 4) as usize;
                for (acc, byte) in s.iter_mut().zip(&tex.rgba[o..o + 3]) {
                    *acc += f64::from(*byte);
                }
                k += 1.0;
            }
        }
        s.map(|v| v / k)
    };
    let low = mean(4..40);
    let high = mean(90..124);
    eprintln!("baked albedo: −Z half {low:?}, +Z plateau {high:?}");
    assert!(
        low[0] > low[2] + 20.0,
        "the low (−Z) rows are the red layer: {low:?}"
    );
    assert!(
        high[2] > high[0] + 20.0,
        "the plateau (+Z) rows are the blue layer: {high:?}"
    );
}
