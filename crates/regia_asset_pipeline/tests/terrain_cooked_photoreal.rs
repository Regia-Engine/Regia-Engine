// SPDX-License-Identifier: MPL-2.0
//! DO27 CP_TERRAIN (ADR-8024): a host-built photoreal terrain cooked by
//! `write_photoreal_terrain`, packed into an uncompressed pak and drawn through
//! `TerrainSharedDraw::from_cooked` + `set_material_cooked` with the pak-backed blobs, against
//! the same terrain built at play (exact heights, holes, RGBA8 material).
//!
//! GPU: `headless_bcn_device` (lavapipe on CI: `VK_ICD_FILENAMES=.../lvp_icd.json`).

use std::sync::OnceLock;

use regia_asset::{AssetId, AssetManifest};
use regia_asset_pipeline::{
    PhotorealMaterialInput, PhotorealTerrainInput, TextureCookQuality, cook_terrain_material, write_pak,
    write_photoreal_terrain,
};
use regia_core::id::TypePathHash;
use regia_terrain::gpu::{
    TerrainCamera, TerrainFrame, TerrainGpu, TerrainSharedDraw, TimeOfDay, headless_bcn_device,
    render_offscreen_opts,
};
use regia_terrain::stream::{PakTileSource, TerrainHeightsEncoding};
use regia_terrain::{
    CdlodConfig, HoleEllipse, HolesMask, LayerDesc, MacroBake, MaterialParams, MinMaxPyramid, TerrainLayers,
    TerrainMap, TerrainMaterial, select_nodes,
};

const W: u32 = 320;
const H: u32 = 180;
const MAGENTA: [f64; 4] = [1.0, 0.0, 1.0, 1.0];
const N: u32 = 129;
const WORLD: f32 = 256.0;

fn device() -> &'static regia_render::RenderDevice {
    static DEVICE: OnceLock<regia_render::RenderDevice> = OnceLock::new();
    DEVICE.get_or_init(|| match headless_bcn_device() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU device (lavapipe?): {err}"),
    })
}

/// A host's final ground: a hill, mirrored across X (DO27 mirrors its visual map), with a
/// sunk shell pad and fractional heights R16 cannot hold.
fn host_heights() -> Vec<f32> {
    let mut heights = Vec::with_capacity((N * N) as usize);
    for row in 0..N {
        for col in 0..N {
            let mirrored = N - 1 - col;
            let x = mirrored as f32 / (N - 1) as f32 * WORLD - WORLD * 0.5;
            let z = row as f32 / (N - 1) as f32 * WORLD - WORLD * 0.5;
            let mut h = 8.0 * (-(x * x + z * z) / 4000.0).exp() + 0.37 * (x * 0.11).sin();
            if (40..48).contains(&row) && (70..80).contains(&col) {
                h -= 0.123_456_7; // a shell sink
            }
            heights.push(h + (row * N + col) as f32 * 1.0e-6);
        }
    }
    heights
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

fn magenta_count(px: &[u8]) -> usize {
    px.chunks(4).filter(|p| is_magenta(p)).count()
}

/// Cooks the host terrain with the adapter, packs it uncompressed and returns the content dir,
/// the map id and the host's inputs.
struct Cooked {
    content: std::path::PathBuf,
    cache: std::path::PathBuf,
    map_id: AssetId,
    heights: Vec<f32>,
    holes: HolesMask,
    material: TerrainMaterial,
}

fn cook() -> Cooked {
    let root = regia_test_utils::unique_temp_dir("terrain_cooked_photoreal");
    let cache = root.join("cache");
    let content = root.join("content");
    let empty = root.join("empty-cache");
    for dir in [&cache, &content, &empty] {
        std::fs::create_dir_all(dir).expect("dirs");
    }
    let heights = host_heights();
    let map = TerrainMap::from_heights(N, N, heights.clone(), WORLD, 20.0, -2.0).expect("map");
    let mut material = TerrainMaterial::procedural_cc0(64, 3, MaterialParams::default());
    material.macro_set = Some(MacroBake::from_heightfield(&map, 2, 64, 11).expect("bake"));
    material.params.macro_world_size = WORLD;
    let blobs = match cook_terrain_material(&material, "linux", TextureCookQuality::High) {
        Ok(blobs) => blobs,
        Err(err) => panic!("cook material: {err}"),
    };
    let holes = HolesMask::new(vec![HoleEllipse {
        center: [10.0, 20.0],
        radius: [14.0, 9.0],
        rotation_radians: 0.3,
    }])
    .expect("holes");
    let map_id = AssetId::sub_asset(AssetId::NIL, "maps/ridge");
    let input = PhotorealTerrainInput {
        map_id,
        width: N,
        height: N,
        heights: &heights,
        world_size_m: WORLD,
        world_origin: None,
        height_mapping: None,
        layers: vec!["low".into(), "high".into()],
        holes: Some(&holes),
        material: Some(PhotorealMaterialInput {
            cooked: blobs.view(),
            params: material.params,
            macro_tiles: 2,
        }),
    };
    let cooked = match write_photoreal_terrain(&input, "maps/ridge.level.ron", &cache) {
        Ok(cooked) => cooked,
        Err(err) => panic!("adapter: {err}"),
    };
    let heights_entry = cooked.index.heights.as_ref().expect("heights entry");
    assert_eq!(heights_entry.encoding, TerrainHeightsEncoding::Residual);
    assert!(
        cooked
            .manifest
            .iter()
            .all(|e| e.path.starts_with("maps/ridge.level.ron#terrain/"))
    );
    let entries: Vec<(AssetId, TypePathHash)> = cooked
        .manifest
        .iter()
        .map(|e| (e.id, TypePathHash::of(&e.type_path)))
        .collect();
    let mut sorted = cooked.manifest.clone();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest = AssetManifest { entries: sorted };
    let options = regia_asset_pipeline::PakWriteOptions {
        compress: false,
        chunk_name: "ridge".into(),
        ..Default::default()
    };
    if let Err(err) = write_pak(
        &content.join("chunk_ridge.fpak"),
        &cache,
        &entries,
        &manifest,
        &options,
    ) {
        panic!("pak: {err}");
    }
    Cooked {
        content,
        cache: empty,
        map_id,
        heights,
        holes,
        material,
    }
}

/// The cooked map comes back as the host built it: every height bit, the holes cut, and the
/// pak-backed material drawing the frame the play-time RGBA8 material draws, within the terrain
/// image bar of `cooked_bcn_material_renders_like_the_rgba8_material` (mean abs < 4/255, no new
/// tolerance).
#[test]
fn cooked_photoreal_terrain_draws_like_the_play_time_build() {
    let cooked = cook();
    let source = PakTileSource::with_verify(
        cooked.map_id,
        Some(cooked.content.as_path()),
        cooked.cache.clone(),
        None,
    );
    let layers = two_layers();
    let config = CdlodConfig::with_grid_size(16);
    // The arguments' world width and mapping are ignored: the index has exact heights.
    let (mut draw, index) = match TerrainSharedDraw::from_cooked(
        device(),
        &source,
        &layers,
        999.0,
        1.0,
        0.0,
        config.clone(),
        4,
    ) {
        Ok(pair) => pair,
        Err(err) => panic!("from_cooked: {err}"),
    };
    assert_eq!(index.version, regia_terrain::stream::TERRAIN_INDEX_FORMAT_VERSION);

    // Heights: bit for bit the host's (fractions, sinks and mirroring included).
    let bits = |v: &[f32]| v.iter().map(|h| h.to_bits()).collect::<Vec<_>>();
    assert_eq!(bits(draw.map().heights()), bits(&cooked.heights));
    assert_eq!(draw.map().world_width(), WORLD);
    // Holes: cut before the first frame.
    assert_eq!(draw.holes(), Some(&cooked.holes));

    // Material: views of the mapped pak, handed over as they are.
    let material = match source.load_cooked_material(&index) {
        Ok(Some(material)) => material,
        other => panic!("cooked material: {other:?}"),
    };
    assert_eq!(material.macro_tiles, 2);
    let blobs = &material.blobs;
    assert!(
        blobs
            .layer_albedo
            .iter()
            .chain(&blobs.layer_normal)
            .chain(&blobs.layer_height)
            .chain(&blobs.macro_albedo_tiles)
            .chain(&blobs.macro_normal_tiles)
            .all(regia_asset::PakPayload::is_mapped),
        "an uncompressed pak entry must be served as a view of the mapping"
    );
    if let Err(err) = draw.set_material_cooked(blobs, material.params, material.macro_tiles, &[]) {
        panic!("set_material_cooked: {err}");
    }

    // The play-time build: the same heights, holes and the RGBA8 material.
    let map = TerrainMap::from_heights(
        N,
        N,
        cooked.heights.clone(),
        WORLD,
        draw.map().height_scale(),
        draw.map().height_offset(),
    )
    .expect("map");
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
    let render = |gpu: &mut TerrainGpu| match render_offscreen_opts(
        gpu,
        device(),
        &frame,
        &selection,
        W,
        H,
        MAGENTA,
        false,
    ) {
        Ok(px) => px,
        Err(err) => panic!("render: {err}"),
    };
    let mut play = TerrainGpu::new(device(), &map, &layers, &config, 4, None, None).expect("gpu");
    play.set_material(&cooked.material, &[]).expect("rgba8 material");
    let whole = render(&mut play);
    play.set_holes(Some(&cooked.holes));
    let reference = render(&mut play);
    let drawn = render(draw.gpu_mut());

    let cut = magenta_count(&reference) - magenta_count(&whole).min(magenta_count(&reference));
    assert!(
        cut > 50,
        "the holes cut too little of the play-time frame ({cut} px)"
    );
    assert!(
        magenta_count(&drawn) >= magenta_count(&whole) + cut / 2,
        "the cooked holes are not cut: {} background px vs {} whole, {} with holes",
        magenta_count(&drawn),
        magenta_count(&whole),
        magenta_count(&reference)
    );
    let mean_abs = reference
        .iter()
        .zip(&drawn)
        .map(|(a, b)| f64::from(a.abs_diff(*b)))
        .sum::<f64>()
        / reference.len() as f64;
    eprintln!("play-time rgba8 vs cooked pak material: mean abs diff {mean_abs:.2}");
    assert!(
        mean_abs < 4.0,
        "the cooked photoreal terrain drifted from the play-time build: {mean_abs:.2}/255"
    );
    let _ = std::fs::remove_dir_all(cooked.content.parent().expect("root"));
}
