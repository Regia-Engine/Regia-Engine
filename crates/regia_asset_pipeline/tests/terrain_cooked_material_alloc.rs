// SPDX-License-Identifier: MPL-2.0
//! DO27 CP_TERRAIN (ADR-8024): reading a cooked photoreal material from an uncompressed pak does
//! not copy its KTX2 chains. Its own binary: a counting global allocator; one test, so no other
//! test allocates while it counts.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use regia_asset::{AssetId, AssetManifest, PakPayload};
use regia_asset_pipeline::{
    PakWriteOptions, PhotorealMaterialInput, PhotorealTerrainInput, TextureCookQuality,
    cook_terrain_material, write_pak, write_photoreal_terrain,
};
use regia_core::id::TypePathHash;
use regia_terrain::stream::PakTileSource;
use regia_terrain::{CookedMaterialKtx2, MacroBake, MaterialParams, TerrainMap, TerrainMaterial};

struct Counting;

static BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator; only counts bytes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        BYTES.fetch_add(new_size, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn bytes() -> usize {
    BYTES.load(Ordering::Relaxed)
}

/// The material read back from the pak allocates per blob (slot vectors, ids, headers), never
/// per byte: well under a tenth of its blob bytes, every blob a view of the mapping. The owned
/// form the v0.5.0 API required (`CookedMaterialKtx2` of `Vec<u8>`) costs at least every byte.
#[test]
fn a_pak_backed_material_is_read_without_copying_its_blobs() {
    let root = regia_test_utils::unique_temp_dir("terrain_cooked_material_alloc");
    let cache = root.join("cache");
    let content = root.join("content");
    let empty = root.join("empty-cache");
    for dir in [&cache, &content, &empty] {
        std::fs::create_dir_all(dir).expect("dirs");
    }
    let n = 65u32;
    let heights: Vec<f32> = (0..n * n)
        .map(|i| (i % n) as f32 * 0.25 + (i / n) as f32 * 0.1)
        .collect();
    let map = TerrainMap::from_heights(n, n, heights.clone(), 128.0, 20.0, 0.0).expect("map");
    let mut material = TerrainMaterial::procedural_cc0(128, 5, MaterialParams::default());
    material.macro_set = Some(MacroBake::from_heightfield(&map, 2, 64, 3).expect("bake"));
    let blobs = match cook_terrain_material(&material, "linux", TextureCookQuality::Low) {
        Ok(blobs) => blobs,
        Err(err) => panic!("cook material: {err}"),
    };
    let map_id = AssetId::sub_asset(AssetId::NIL, "maps/alloc");
    let input = PhotorealTerrainInput {
        map_id,
        width: n,
        height: n,
        heights: &heights,
        world_size_m: 128.0,
        world_origin: None,
        height_mapping: None,
        layers: Vec::new(),
        holes: None,
        material: Some(PhotorealMaterialInput {
            cooked: blobs.view(),
            params: material.params,
            macro_tiles: 2,
        }),
    };
    let cooked = match write_photoreal_terrain(&input, "maps/alloc.level.ron", &cache) {
        Ok(cooked) => cooked,
        Err(err) => panic!("adapter: {err}"),
    };
    let entries: Vec<(AssetId, TypePathHash)> = cooked
        .manifest
        .iter()
        .map(|e| (e.id, TypePathHash::of(&e.type_path)))
        .collect();
    let mut sorted = cooked.manifest.clone();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let options = PakWriteOptions {
        compress: false,
        chunk_name: "alloc".into(),
        ..Default::default()
    };
    if let Err(err) = write_pak(
        &content.join("chunk_alloc.fpak"),
        &cache,
        &entries,
        &AssetManifest { entries: sorted },
        &options,
    ) {
        panic!("pak: {err}");
    }
    let source = PakTileSource::with_verify(map_id, Some(content.as_path()), empty, None);
    let index = source.load_terrain_index().expect("index");
    let blob_bytes = blobs.total_bytes() as usize;
    let blob_count = blobs.layer_albedo.len()
        + blobs.layer_normal.len()
        + blobs.layer_height.len()
        + blobs.layer_orm.len()
        + blobs.macro_albedo_tiles.len()
        + blobs.macro_normal_tiles.len()
        + usize::from(blobs.macro_coarse_albedo.is_some())
        + usize::from(blobs.macro_coarse_normal.is_some());
    assert!(blob_count >= 16, "{blob_count} blobs");

    // Warm-up read (lazy statics, the pak's first touch), then the counted one.
    drop(source.load_cooked_material(&index));
    let before = bytes();
    let loaded = source.load_cooked_material(&index);
    let read_bytes = bytes() - before;
    let material_back = match loaded {
        Ok(Some(material)) => material,
        other => panic!("cooked material: {other:?}"),
    };
    let back = &material_back.blobs;
    assert_eq!(back.total_bytes() as usize, blob_bytes);
    assert_eq!(back.view().layer_albedo, blobs.view().layer_albedo);
    assert_eq!(back.view().macro_normal_tiles, blobs.view().macro_normal_tiles);
    assert!(
        back.layer_albedo
            .iter()
            .chain(&back.layer_normal)
            .chain(&back.layer_height)
            .chain(&back.macro_albedo_tiles)
            .chain(&back.macro_normal_tiles)
            .all(PakPayload::is_mapped),
        "every blob is a view of the mapped pak"
    );
    eprintln!("{blob_count} blobs, {blob_bytes} blob bytes, {read_bytes} bytes allocated by the read");
    assert!(
        read_bytes * 10 < blob_bytes,
        "reading the material allocated {read_bytes} bytes for {blob_bytes} blob bytes: a blob was copied"
    );

    // The owned form the v0.5.0 `set_material_cooked` took costs every byte again.
    let before = bytes();
    let owned: CookedMaterialKtx2 = CookedMaterialKtx2 {
        layer_albedo: back.layer_albedo.iter().map(|b| b.to_vec()).collect(),
        layer_normal: back.layer_normal.iter().map(|b| b.to_vec()).collect(),
        layer_height: back.layer_height.iter().map(|b| b.to_vec()).collect(),
        layer_orm: back.layer_orm.iter().map(|b| b.to_vec()).collect(),
        macro_albedo_tiles: back.macro_albedo_tiles.iter().map(|b| b.to_vec()).collect(),
        macro_normal_tiles: back.macro_normal_tiles.iter().map(|b| b.to_vec()).collect(),
        macro_coarse_albedo: back.macro_coarse_albedo.as_ref().map(|b| b.to_vec()),
        macro_coarse_normal: back.macro_coarse_normal.as_ref().map(|b| b.to_vec()),
    };
    let copy_bytes = bytes() - before;
    assert!(copy_bytes >= blob_bytes, "{copy_bytes} < {blob_bytes}");
    drop(owned);
    let _ = std::fs::remove_dir_all(&root);
}
