// SPDX-License-Identifier: MPL-2.0
//! Scale acceptance for the photoreal terrain lane (spec 26 § 1 + § 6).
//!
//! The 4096² visual heightfield covers the DO27 1280 m extent. Shading in this test is the
//! in-memory RGBA8 `set_material` path. The 8K mask pair is cooked by
//! `cook_tiled_texture` (the terrain-data KTX2 path) and pulled through `TileCache`.
//! A full 8192² BC7 cook is the same function; its wall time (~114 s, ~837 MiB RSS) is
//! recorded in `docs/evidence/terrain-data/budgets.md`, so this test cooks a 512² pair
//! through that function and pins the canonical 8192² package with `terrain_map_budget`.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regia_asset_pipeline::{
    TerrainMapParams, TextureCookQuality, TileFormat, cook_terrain_material, cook_tiled_texture,
    terrain_map_budget,
};
use regia_terrain::gpu::{
    TerrainCamera, TerrainFrame, TerrainGpu, TimeOfDay, headless_bcn_device, render_offscreen_opts,
};
use regia_terrain::{
    CdlodConfig, LayerDesc, MacroBake, MaterialParams, MemoryTileSource, MinMaxPyramid, TerrainLayers,
    TerrainMap, TerrainMaterial, TileCache, TileFormat as StreamFormat, TileKey, TiledTexture, select_nodes,
    tail_key,
};

const W: u32 = 320;
const H: u32 = 180;
const MAGENTA: [f64; 4] = [1.0, 0.0, 1.0, 1.0];
const KTX2_MAGIC: [u8; 12] = [
    0xAB, 0x4B, 0x54, 0x58, 0x20, 0x32, 0x30, 0xBB, 0x0D, 0x0A, 0x1A, 0x0A,
];

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match headless_bcn_device() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// Analytic 4096² heightfield over the DO27 1280 m extent (~0.31 m/texel). No erosion:
/// the crack test only needs a surface that selects more than one CDLOD level.
fn do27_visual() -> TerrainMap {
    let n = 4096u32;
    let world = regia_terrain::DO27_WORLD_WIDTH_M;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let hill = 28.0 * (-(x * x + z * z) / 90_000.0).exp();
            let ripple = 3.0 * (x * 0.02).sin() * (z * 0.017).cos();
            heights.push(hill + ripple);
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 80.0, -8.0).expect("4096² map")
}

fn two_layers() -> TerrainLayers {
    let layer = |name: &str, albedo: [f32; 3]| LayerDesc {
        name: name.into(),
        albedo,
        roughness: 0.9,
        tiling: 0.4,
        slope_min: 0.0,
        slope_max: 1.6,
        altitude_min: -50.0,
        altitude_max: 80.0,
        blend_sharpness: 0.12,
        triplanar: false,
        height_blend: 0.0,
        ..LayerDesc::ground()
    };
    TerrainLayers {
        layers: vec![
            layer("low", [0.25, 0.32, 0.12]),
            layer("high", [0.45, 0.40, 0.32]),
        ],
        ..Default::default()
    }
}

fn is_magenta(p: &[u8]) -> bool {
    p[0] > 180 && p[1] < 70 && p[2] > 180
}

/// Magenta background must not leak through a 4096² surface at a CDLOD level boundary.
#[test]
fn forty_ninety_six_level_boundary_leaks_no_magenta() {
    let map = do27_visual();
    assert_eq!(map.width(), 4096);
    assert_eq!(map.world_width(), regia_terrain::DO27_WORLD_WIDTH_M);
    let layers = two_layers();
    let config = CdlodConfig::with_grid_size(16);
    let root_level = 8u32;
    let camera = TerrainCamera {
        position: [0.0, 70.0, 260.0],
        target: [0.0, 12.0, -40.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.5,
        far: 4000.0,
    };
    let pyramid = MinMaxPyramid::build(&map, root_level).expect("pyramid");
    let selection = select_nodes(
        &map,
        &pyramid,
        &config,
        root_level,
        [camera.position[0], camera.position[2]],
        None,
    );
    let distinct: std::collections::BTreeSet<u32> = selection.nodes.iter().map(|n| n.node.level).collect();
    assert!(
        distinct.len() >= 2,
        "camera must straddle a level boundary, saw {distinct:?}"
    );

    let mut gpu = TerrainGpu::new(device(), &map, &layers, &config, root_level, None, None).expect("gpu");
    let frame = TerrainFrame::from_camera(&camera, &TimeOfDay::default(), 0.0, 1.0);
    let pixels =
        render_offscreen_opts(&mut gpu, device(), &frame, &selection, W, H, MAGENTA, false).expect("render");

    let mut min_x = W;
    let mut max_x = 0u32;
    for y in 0..H {
        for x in 0..W {
            let p = &pixels[(y * W + x) as usize * 4..][..4];
            if !is_magenta(p) {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
            }
        }
    }
    assert!(max_x > min_x + 80, "terrain not visible");
    let mut leaks = 0usize;
    let mut columns = 0usize;
    for x in min_x..=max_x {
        let bottom = &pixels[((H - 1) * W + x) as usize * 4..][..4];
        if is_magenta(bottom) {
            continue;
        }
        columns += 1;
        let mut top = H - 1;
        for y in (0..H).rev() {
            let p = &pixels[(y * W + x) as usize * 4..][..4];
            if !is_magenta(p) {
                top = y;
            }
        }
        for y in top..H {
            let p = &pixels[(y * W + x) as usize * 4..][..4];
            if is_magenta(p) {
                leaks += 1;
            }
        }
    }
    assert!(columns > 80, "too few covered columns: {columns}");
    assert_eq!(
        leaks, 0,
        "{leaks} magenta pixels leaked through the 4096² surface"
    );
}

/// The streaming subset is what gets drawn. Residency of one macro tile must be
/// smaller than the full set and must still cover the frame.
#[test]
fn streaming_subset_is_what_renders() {
    let n = 129u32;
    let world = 256.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
            heights.push(8.0 * (-(x * x + z * z) / 4000.0).exp());
        }
    }
    let map = TerrainMap::from_heights(n, n, heights, world, 20.0, -2.0).expect("map");
    let layers = two_layers();
    let mut material = TerrainMaterial::procedural_cc0(64, 3, MaterialParams::default());
    material.macro_set = Some(MacroBake::from_heightfield(&map, 2, 64, 11).expect("bake"));
    material.params.macro_world_size = world;
    material.params.macro_strength = 0.8;

    let config = CdlodConfig::with_grid_size(16);
    let camera = TerrainCamera {
        position: [0.0, 40.0, 90.0],
        target: [0.0, 4.0, 0.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.4,
        far: 800.0,
    };
    let build = |resident: &[u32]| -> (TerrainGpu, u64) {
        let mut gpu = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("gpu");
        // RGBA8 upload. The KTX2 tiles live in the cache test below; this path is the
        // shader's residency bitmask, and it is the frame that must actually draw.
        gpu.set_material(&material, resident).expect("material");
        let bytes = gpu.texture_bytes();
        (gpu, bytes)
    };
    let (full, full_bytes) = build(&[]);
    let (mut subset, subset_bytes) = build(&[0]);
    assert!(
        subset_bytes < full_bytes,
        "a one-tile residency must be smaller: {subset_bytes} vs {full_bytes}"
    );
    drop(full);

    let pyramid = MinMaxPyramid::build(&map, 4).expect("pyramid");
    let selection = select_nodes(
        &map,
        &pyramid,
        &config,
        4,
        [camera.position[0], camera.position[2]],
        None,
    );
    let frame = TerrainFrame::from_camera(&camera, &TimeOfDay::default(), 0.0, 1.0);
    let pixels = render_offscreen_opts(&mut subset, device(), &frame, &selection, W, H, MAGENTA, false)
        .expect("render");
    let covered = pixels.chunks(4).filter(|p| !is_magenta(p)).count();
    assert!(
        covered > 8_000,
        "streaming subset did not render the surface: {covered} px"
    );
}

/// The BCn material from `cook_terrain_material` (the tiled cook, ORM included) renders
/// the same frame as the in-memory RGBA8 material, up to block-compression error, and its
/// ORM occlusion is live: an AO-0 ORM on every layer turns the sky-lit frame dark. ORM
/// occlusion acts on indirect light only (glTF 2.0; ADR-060), so the probe is lit by the
/// sky alone (sun off).
#[test]
fn cooked_bcn_material_renders_like_the_rgba8_material() {
    // `device()` is `headless_bcn_device`: lavapipe and desktop GPUs expose BCn, and the
    // cooked upload below fails loudly without it.
    let n = 129u32;
    let world = 256.0f32;
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
            heights.push(8.0 * (-(x * x + z * z) / 4000.0).exp());
        }
    }
    let map = TerrainMap::from_heights(n, n, heights, world, 20.0, -2.0).expect("map");
    let layers = two_layers();
    let mut material = TerrainMaterial::procedural_cc0(64, 3, MaterialParams::default());
    material.macro_set = Some(MacroBake::from_heightfield(&map, 2, 64, 11).expect("bake"));
    material.params.macro_world_size = world;
    for set in material.layers.iter_mut().flatten() {
        set.use_orm = true;
        set.orm = regia_terrain::Rgba8Image::solid(64, 64, [255, 230, 0, 255]);
    }
    let config = CdlodConfig::with_grid_size(16);
    let camera = TerrainCamera {
        position: [0.0, 40.0, 90.0],
        target: [0.0, 4.0, 0.0],
        fov_y_radians: 1.0,
        aspect: W as f32 / H as f32,
        near: 0.4,
        far: 800.0,
    };
    let pyramid = MinMaxPyramid::build(&map, 4).expect("pyramid");
    let selection = select_nodes(
        &map,
        &pyramid,
        &config,
        4,
        [camera.position[0], camera.position[2]],
        None,
    );
    let mut frame = TerrainFrame::from_camera(&camera, &TimeOfDay::default(), 0.0, 1.0);
    frame.debug_view = 3; // albedo: compares the textures, not the lighting
    let render = |gpu: &mut TerrainGpu| {
        render_offscreen_opts(gpu, device(), &frame, &selection, W, H, MAGENTA, false).expect("render")
    };
    let mut rgba8 = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("gpu");
    rgba8.set_material(&material, &[]).expect("rgba8 material");
    let reference = render(&mut rgba8);

    let cook = |m: &TerrainMaterial| match cook_terrain_material(m, "linux", TextureCookQuality::High) {
        Ok(cooked) => cooked,
        Err(err) => panic!("cook: {err}"),
    };
    let mut bcn = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("gpu");
    bcn.set_material_cooked(&cook(&material), material.params, 2, &[])
        .expect("cooked material");
    let cooked = render(&mut bcn);
    let mean_abs = reference
        .iter()
        .zip(&cooked)
        .map(|(a, b)| f64::from(a.abs_diff(*b)))
        .sum::<f64>()
        / reference.len() as f64;
    let channel_means = |px: &[u8]| {
        let mut m = [0.0f64; 3];
        let mut n = 0.0;
        for p in px.chunks(4).filter(|p| !is_magenta(p)) {
            for c in 0..3 {
                m[c] += f64::from(p[c]);
            }
            n += 1.0;
        }
        m.map(|v| v / f64::max(n, 1.0))
    };
    eprintln!(
        "rgba8 vs bcn mean abs diff {mean_abs:.2}; means {:?} vs {:?}",
        channel_means(&reference),
        channel_means(&cooked)
    );
    assert!(
        mean_abs < 4.0,
        "cooked BCn frame drifted from the RGBA8 frame: {mean_abs:.2}/255"
    );

    // Sky light only: the shaded frame with the sun off.
    let sky_only = TimeOfDay {
        illuminance_lux: 0.0,
        ambient: [0.5, 0.5, 0.5],
        ..TimeOfDay::default()
    };
    let sky_frame = TerrainFrame::from_camera(&camera, &sky_only, 0.0, 1.0);
    let render_sky = |gpu: &mut TerrainGpu| {
        render_offscreen_opts(gpu, device(), &sky_frame, &selection, W, H, MAGENTA, false).expect("render")
    };
    let lit_px = render_sky(&mut bcn);
    let mut dark = material.clone();
    for set in dark.layers.iter_mut().flatten() {
        set.orm = regia_terrain::Rgba8Image::solid(64, 64, [0, 230, 0, 255]);
    }
    bcn.set_material_cooked(&cook(&dark), dark.params, 2, &[])
        .expect("dark cooked material");
    let dark_px = render_sky(&mut bcn);
    let mean = |px: &[u8]| {
        let terrain: Vec<&[u8]> = px.chunks(4).filter(|p| !is_magenta(p)).collect();
        terrain.iter().map(|p| f64::from(p[1])).sum::<f64>() / terrain.len().max(1) as f64
    };
    eprintln!(
        "sky-lit green mean: AO 1 {:.1}, AO 0 {:.1}",
        mean(&lit_px),
        mean(&dark_px)
    );
    assert!(
        mean(&dark_px) < mean(&lit_px) * 0.25,
        "cooked ORM occlusion is not sampled: {:.1} vs {:.1}",
        mean(&dark_px),
        mean(&lit_px)
    );
}

fn wait_until(cache: &mut TileCache, pred: impl Fn(&TileCache) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        cache.tick();
        if pred(cache) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cache.tick();
    pred(cache)
}

/// Two mask textures through the terrain-data KTX2 cook, then the streaming cache.
/// The canonical 8K × 2 maps package is pinned by `terrain_map_budget` (under 6 GiB).
#[test]
fn mask_pair_cooks_to_ktx2_and_the_package_fits() {
    let budget = terrain_map_budget(&TerrainMapParams::default());
    let params = TerrainMapParams::default();
    assert_eq!(params.macro_size, 8192);
    assert_eq!(params.mask_count, 2);
    assert_eq!(params.heightfield_size, 4096);
    assert_eq!(params.map_count, 2);
    assert!(
        budget.total_cooked_bytes < 6 * 1024 * 1024 * 1024,
        "two-map package {} bytes exceeds 6 GiB",
        budget.total_cooked_bytes
    );
    assert_eq!(budget.profiles.len(), 3);
    assert_eq!(budget.profiles[0].name, "RTX 5090");
    assert_eq!(budget.profiles[1].name, "RX 7900 XTX");
    assert_eq!(budget.profiles[2].name, "Steam Deck (Medium)");
    assert!(
        budget.profiles[0].fits,
        "5090 tile budget should hold the cooked set"
    );
    assert!(
        budget.profiles[1].fits,
        "7900 XTX tile budget should hold the cooked set"
    );
    // Deck Medium keeps a 1 GiB cache. The full two-map footprint does not fit it;
    // streaming (the cache below) is what keeps the resident set inside that budget.
    assert!(
        !budget.profiles[2].fits,
        "Deck must stream; the full footprint exceeds 1 GiB"
    );
    assert_eq!(budget.profiles[2].budget_bytes, 1024 * 1024 * 1024);

    let frames = regia_terrain::do27_gpu_budgets();
    assert_eq!(frames[0].gpu, "RTX 5090");
    assert!(frames[0].terrain_pass_ms <= 2.5);
    assert!(frames[1].terrain_pass_ms <= 2.5);
    assert!(frames[2].terrain_pass_ms <= 4.0);

    // Same cook the 8K masks use. 512² keeps this test off the 114 s / 837 MiB 8K bench.
    let size = 512u32;
    let tile = 256u32;
    let mut src = MemoryTileSource::new();
    for (slot, name) in [(0u8, "mask0"), (1u8, "mask1")] {
        let mut rgba = vec![0u8; (size * size * 4) as usize];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = (i as u32) % size;
            px[0] = (x & 255) as u8;
            px[1] = ((slot as u32) * 80) as u8;
            px[2] = 40;
            px[3] = 255;
        }
        let cooked = cook_tiled_texture(
            name,
            size,
            size,
            &rgba,
            tile,
            TileFormat::Bc7Linear,
            "desktop",
            TextureCookQuality::Low,
        )
        .expect("cook mask");
        assert!(
            cooked.tail_ktx2.starts_with(&KTX2_MAGIC),
            "{name} tail is not KTX2"
        );
        assert!(!cooked.tiles.is_empty(), "{name} produced no tiles");
        assert!(cooked.tiles[0].ktx2.starts_with(&KTX2_MAGIC));
        src.insert(tail_key(slot), cooked.tail_ktx2);
        for tile_cook in cooked.tiles {
            src.insert(
                TileKey {
                    texture: slot,
                    mip: tile_cook.mip as u8,
                    x: tile_cook.x,
                    y: tile_cook.y,
                },
                tile_cook.ktx2,
            );
        }
    }

    let mut cache = TileCache::new(8 * 1024 * 1024, src);
    cache.register_texture(0, TiledTexture::new("mask0", size, tile, StreamFormat::Bc7Linear));
    cache.register_texture(1, TiledTexture::new("mask1", size, tile, StreamFormat::Bc7Linear));
    let n0 = cache.request(0, [0.0, 0.0], 1280.0, 400.0, 4);
    let n1 = cache.request(1, [0.0, 0.0], 1280.0, 400.0, 4);
    assert!(n0 + n1 > 0, "request enqueued nothing");
    assert!(
        wait_until(&mut cache, |c| {
            c.is_resident(tail_key(0)) && c.is_resident(tail_key(1)) && c.resident_count() >= 4
        }),
        "mask tiles did not become resident (count {})",
        cache.resident_count()
    );
    let tail = cache.residency(tail_key(0)).expect("mask0 tail");
    assert!(tail.starts_with(&KTX2_MAGIC));
}
