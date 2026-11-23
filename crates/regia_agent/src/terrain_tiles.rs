// SPDX-License-Identifier: MPL-2.0
//! `assets.terrain_tiles` (ADR-5294, `book/agents/terrain-tiles.md`): tile counts and the
//! block encoding of a cooked terrain. Read-only. The encoding is the KTX2 `vkFormat` of a
//! slot's tail (Khronos KTX 2.0 § 3), reported as `bcn` or `astc`. The terrain index stays
//! version 1; the encoding is not a field on it.

use regia_app::prelude::*;
use regia_asset::cooked_pack::{GpuFamily, texture_form};
use regia_asset::{AssetServer, PakPayload};
use regia_terrain::stream::{TerrainIndex, TerrainIndexError, load_terrain_index_bytes};
use regia_terrain::tail_asset_id;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

const METHOD: &str = "assets.terrain_tiles";

/// `assets.terrain_tiles { asset }`: tile counts and `bcn` or `astc` for a cooked terrain.
///
/// `asset` is the terrain layer path (`assets/.../*.terrain.layers.ron`) or the cooked index id.
pub fn terrain_tiles(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !matches!(params.get("asset"), Some(Value::String(asset)) if !asset.is_empty()) {
        return Err(RpcError::new(
            protocol::INVALID_PARAMS,
            "terrain_tiles_bad_param",
            format!("{METHOD}: `asset` (a terrain layer path or its cooked id) is required"),
            "pass the .terrain.layers.ron path or the id from assets.list",
        ));
    }
    let Some(mut server) = world.get_resource_mut::<AssetServer>() else {
        return Err(crate::assets::no_asset_server());
    };
    let (path, id) = crate::assets::resolve_asset(&server, params)?;
    let (bytes, _) = server.read_cooked(id).map_err(|err| {
        missing(
            &path,
            &format!("no cooked terrain index for {path}: {err}"),
            "run regia cook for this platform",
        )
    })?;
    let index = load_terrain_index_bytes(&bytes).map_err(|err| index_error(&path, &err))?;
    report(&mut server, &path, &id.to_string(), &index)
}

fn report(server: &mut AssetServer, path: &str, id: &str, index: &TerrainIndex) -> Result<Value, RpcError> {
    let mut slots = Vec::with_capacity(index.slots.len());
    let mut labels: Vec<&'static str> = Vec::with_capacity(index.slots.len());
    for slot in &index.slots {
        let tail = tail_asset_id(index.map_id, &slot.name);
        let (file, _) = server.read_cooked(tail).map_err(|err| {
            missing(
                path,
                &format!("slot `{}` has no cooked tail: {err}", slot.name),
                "re-cook the terrain (regia cook); the tail is terrain/<slot>/tail",
            )
        })?;
        let encoding = encoding_of(&file).ok_or_else(|| {
            RpcError::new(
                protocol::INVALID_PARAMS,
                "terrain_tiles_unreadable",
                format!("{METHOD}: slot `{}` tail is not a KTX2 texture", slot.name),
                "re-cook the terrain; the tail file is damaged or not a cooked texture",
            )
        })?;
        labels.push(encoding);
        slots.push(json!({
            "name": slot.name,
            "tile_count": slot.tile_count,
            "width": slot.width,
            "height": slot.height,
            "tile_size": slot.tile_size,
            "format": format_name(slot.format),
            "encoding": encoding,
        }));
    }
    let encoding = match labels.first() {
        None => Value::Null,
        Some(first) if labels.iter().all(|label| label == first) => json!(first),
        Some(_) => json!("mixed"),
    };
    let heightfield = index.heightfield.as_ref().map(|height| {
        json!({
            "tiles_x": height.tiles_x,
            "tiles_y": height.tiles_y,
            "tile_count": u64::from(height.tiles_x) * u64::from(height.tiles_y),
            "encoding": Value::Null,
        })
    });
    Ok(json!({
        "asset": path,
        "id": id,
        "version": index.version,
        "encoding": encoding,
        "slots": slots,
        "heightfield": heightfield,
        "final_heights": final_heights_json(index),
        "holes": index.holes.as_ref().map(|holes| json!({ "bytes": holes.bytes })),
        "material": material_json(index),
        "source_grid": source_grid_json(index),
    }))
}

/// The cooked count source's grid (index version 3, ADR-8871): its X/Z spacing and how many
/// samples hold no data, or null when the index has no count mapping.
fn source_grid_json(index: &TerrainIndex) -> Value {
    let Some(spacing) = index.source_spacing_m() else {
        return Value::Null;
    };
    json!({
        "spacing_m": spacing,
        "missing_samples": index.source_valid_mask.as_ref().map_or(0, |mask| mask.missing),
    })
}

/// The host-cooked exact heights (ADR-8024), or null for a standard cook.
fn final_heights_json(index: &TerrainIndex) -> Value {
    let Some(heights) = &index.heights else {
        return Value::Null;
    };
    json!({
        "width": heights.width,
        "height": heights.height,
        "encoding": match heights.encoding {
            regia_terrain::TerrainHeightsEncoding::Residual => "residual",
            regia_terrain::TerrainHeightsEncoding::Raw => "raw",
        },
        "height_scale": heights.height_scale,
        "height_offset": heights.height_offset,
        "bytes": heights.bytes,
    })
}

/// The host-cooked photoreal material (ADR-8024): blob counts per kind, or null.
fn material_json(index: &TerrainIndex) -> Value {
    let Some(material) = &index.material else {
        return Value::Null;
    };
    json!({
        "macro_tiles": material.macro_tiles,
        "layer_albedo": material.layer_albedo,
        "layer_normal": material.layer_normal,
        "layer_height": material.layer_height,
        "layer_orm": material.layer_orm,
        "macro_albedo_tiles": material.macro_albedo_tiles,
        "macro_normal_tiles": material.macro_normal_tiles,
        "macro_coarse_albedo": material.macro_coarse_albedo,
        "macro_coarse_normal": material.macro_coarse_normal,
        "bytes": material.bytes,
    })
}

fn format_name(format: regia_terrain::TileFormat) -> &'static str {
    match format {
        regia_terrain::TileFormat::Bc7Srgb => "bc7_srgb",
        regia_terrain::TileFormat::Bc5 => "bc5",
        regia_terrain::TileFormat::Bc4 => "bc4",
        regia_terrain::TileFormat::Bc7Linear => "bc7_linear",
    }
}

/// `bc` from the KTX2 family table is the desktop cook; the method reports it as `bcn`.
fn encoding_of(file: &[u8]) -> Option<&'static str> {
    let form = texture_form(&PakPayload::from(file))?;
    Some(match form.family {
        GpuFamily::Bc => "bcn",
        GpuFamily::Astc => "astc",
        _ => return None,
    })
}

fn missing(path: &str, message: &str, hint: &str) -> RpcError {
    RpcError::new(
        protocol::NOT_FOUND,
        "terrain_tiles_missing",
        format!("{METHOD}: {path}: {message}"),
        hint,
    )
}

fn index_error(path: &str, err: &TerrainIndexError) -> RpcError {
    match err {
        TerrainIndexError::WrongType { .. } => RpcError::new(
            protocol::NOT_FOUND,
            "terrain_tiles_not_terrain",
            format!("{METHOD}: {path} is not a cooked terrain index: {err}"),
            "pass the terrain layer path (*.terrain.layers.ron) or the cooked index id",
        ),
        TerrainIndexError::Read(_) => missing(
            path,
            &format!("the cooked index does not read: {err}"),
            "run regia cook for this platform",
        ),
        _ => RpcError::new(
            protocol::INVALID_PARAMS,
            "terrain_tiles_bad_index",
            format!("{METHOD}: {path} is not a readable terrain index: {err}"),
            "re-cook the terrain with this build (regia cook)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::{
        AssetId, AssetManifest, CookedHeader, ManifestEntry, cooked_cache_dir, cooked_file_name,
        encode_cooked_file,
    };
    use regia_terrain::stream::{
        TERRAIN_INDEX_FORMAT_VERSION, TERRAIN_INDEX_TYPE_PATH, TerrainHeightfieldIndex, TerrainSlotIndex,
    };

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|data| data["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn constant(size: u32, texel: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity((size * size * 4) as usize);
        for _ in 0..size * size {
            out.extend_from_slice(&texel);
        }
        out
    }

    fn stream_format(format: regia_asset_pipeline::TileFormat) -> regia_terrain::TileFormat {
        match format {
            regia_asset_pipeline::TileFormat::Bc7Srgb => regia_terrain::TileFormat::Bc7Srgb,
            regia_asset_pipeline::TileFormat::Bc5 => regia_terrain::TileFormat::Bc5,
            regia_asset_pipeline::TileFormat::Bc4 => regia_terrain::TileFormat::Bc4,
            regia_asset_pipeline::TileFormat::Bc7Linear => regia_terrain::TileFormat::Bc7Linear,
        }
    }

    /// Cooks one constant slot and writes a terrain index that names it.
    fn cook_index(
        platform: &str,
        format: regia_asset_pipeline::TileFormat,
        slot: &str,
    ) -> (std::path::PathBuf, String) {
        let root = regia_test_utils::unique_temp_dir("terrain_tiles_agent");
        let cache = cooked_cache_dir(&root, platform);
        std::fs::create_dir_all(&cache).expect("cache dir");
        let map_id = AssetId::sub_asset(AssetId::NIL, "terrain-tiles-test");
        let size = 16u32;
        let cooked = regia_asset_pipeline::cook_tiled_texture(
            slot,
            size,
            size,
            &constant(size, [10, 20, 30, 255]),
            size,
            format,
            platform,
            regia_asset_pipeline::TextureCookQuality::High,
        )
        .expect("cook slot");
        let written =
            regia_asset_pipeline::write_tiled_texture_cooked(map_id, &cooked, &cache).expect("write tiles");
        let bytes: u64 = written.iter().map(|(_, n)| *n).sum();
        let index = TerrainIndex {
            version: TERRAIN_INDEX_FORMAT_VERSION,
            map_id,
            slots: vec![TerrainSlotIndex {
                name: slot.to_string(),
                width: size,
                height: size,
                tile_size: size,
                format: stream_format(format),
                mip_count: cooked.mip_count,
                tail_mips: cooked.tail_mips,
                tile_count: cooked.tiles.len() as u32,
                bytes,
            }],
            heightfield: Some(TerrainHeightfieldIndex {
                width: 32,
                height: 32,
                tile_size: 16,
                tiles_x: 2,
                tiles_y: 2,
                bytes: 0,
            }),
            layers: vec!["ground".to_string()],
            world_size_m: None,
            world_origin: None,
            source_mapping: None,
            source_spacing_z_m: None,
            source_valid_mask: None,
            heights: None,
            holes: None,
            material: None,
        };
        let payload = index.to_postcard().expect("postcard");
        let index_id = regia_terrain::stream::terrain_index_asset_id(map_id);
        let header = CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_core::id::TypePathHash::of(TERRAIN_INDEX_TYPE_PATH),
            asset_id: index_id,
            importer_version: 1,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("index file");
        std::fs::write(cache.join(cooked_file_name(index_id)), &file).expect("write index");
        let path = "assets/textures/terrain.layers.ron".to_string();
        AssetManifest {
            entries: vec![ManifestEntry {
                path: path.clone(),
                id: index_id,
                type_path: TERRAIN_INDEX_TYPE_PATH.to_string(),
            }],
        }
        .write_to_cache(&cache)
        .expect("manifest");
        (root, path)
    }

    fn open(root: &std::path::Path, platform: &str) -> World {
        let server = AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.to_path_buf(),
            platform: platform.to_string(),
            content_dir: None,
        })
        .expect("asset server");
        let mut world = World::new();
        world.insert_resource(server);
        world
    }

    #[test]
    fn method_name_is_the_protocol_name() {
        assert_eq!(METHOD, "assets.terrain_tiles");
    }

    /// ADR-8871: a version-3 index reports its source grid's X/Z spacing and missing samples; an
    /// index without a count mapping reports null.
    #[test]
    fn source_grid_reports_spacing_and_missing_samples() {
        let mut index = TerrainIndex {
            version: TERRAIN_INDEX_FORMAT_VERSION,
            map_id: AssetId::sub_asset(AssetId::NIL, "maps/grid"),
            slots: Vec::new(),
            heightfield: None,
            layers: Vec::new(),
            world_size_m: None,
            world_origin: None,
            heights: None,
            holes: None,
            material: None,
            source_mapping: None,
            source_spacing_z_m: None,
            source_valid_mask: None,
        };
        assert!(source_grid_json(&index).is_null());
        index.source_mapping = Some(regia_terrain::source::CountMapping {
            cols: 3,
            rows: 3,
            meters_per_count: 0.01,
            zero_counts: 0,
            cell_m: 2.0,
            sample0_m: [0.0, 0.0],
        });
        assert_eq!(
            source_grid_json(&index),
            json!({ "spacing_m": [2.0, 2.0], "missing_samples": 0 })
        );
        index.source_spacing_z_m = Some(0.5);
        index.source_valid_mask = Some(regia_terrain::TerrainValidMaskIndex {
            cols: 3,
            rows: 3,
            missing: 1,
            bits: vec![0b1110_1111, 0b0000_0001],
        });
        assert_eq!(
            source_grid_json(&index),
            json!({ "spacing_m": [2.0, 0.5], "missing_samples": 1 })
        );
    }

    #[test]
    fn missing_asset_is_bad_param() {
        let mut world = World::new();
        let err = terrain_tiles(&mut world, &json!({})).expect_err("missing asset");
        assert_eq!(code(&err), "terrain_tiles_bad_param");
    }

    #[test]
    fn no_server_is_f0016() {
        let mut world = World::new();
        let err = terrain_tiles(
            &mut world,
            &json!({ "asset": "assets/textures/terrain.layers.ron" }),
        )
        .expect_err("no server");
        assert_eq!(code(&err), "F0016");
    }

    #[test]
    fn android_report_is_astc_and_counts_tiles() {
        let (root, path) = cook_index(
            "android",
            regia_asset_pipeline::TileFormat::Bc7Linear,
            "macro_pack0",
        );
        let mut world = open(&root, "android");
        world.insert_resource(crate::safety::AgentPermissions {
            config: crate::safety::AgentConfig::fail_closed(),
            source: None,
        });
        let report = crate::dispatch::call_in_process(&mut world, METHOD, json!({ "asset": path }))
            .expect("synchronous method")
            .expect("report");
        assert_eq!(report["encoding"], "astc");
        assert_eq!(report["version"], TERRAIN_INDEX_FORMAT_VERSION);
        // A standard cook carries no host-cooked heights, holes or material (ADR-8024).
        assert!(
            report["final_heights"].is_null() && report["holes"].is_null() && report["material"].is_null()
        );
        // No count mapping in this cook, so no source grid (ADR-8871).
        assert!(report["source_grid"].is_null(), "{report}");
        assert_eq!(report["slots"][0]["name"], "macro_pack0");
        assert_eq!(report["slots"][0]["encoding"], "astc");
        assert_eq!(report["slots"][0]["format"], "bc7_linear");
        assert_eq!(report["slots"][0]["tile_count"], 1);
        assert_eq!(report["heightfield"]["tile_count"], 4);
        assert!(report["heightfield"]["encoding"].is_null());
        let by_id = crate::dispatch::call_in_process(&mut world, METHOD, json!({ "asset": report["id"] }))
            .expect("synchronous method")
            .expect("resolve cooked id");
        assert_eq!(by_id["slots"], report["slots"]);
        drop(world);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn desktop_report_is_bcn() {
        let (root, path) = cook_index("linux", regia_asset_pipeline::TileFormat::Bc4, "layer0_height");
        let mut world = open(&root, "linux");
        let report = crate::dispatch::call_in_process(&mut world, METHOD, json!({ "asset": path }))
            .expect("synchronous method")
            .expect("report");
        assert_eq!(report["encoding"], "bcn");
        assert_eq!(report["slots"][0]["encoding"], "bcn");
        assert_eq!(report["slots"][0]["format"], "bc4");
        assert!(report["slots"][0]["tile_count"].as_u64().unwrap_or(0) >= 1);
        drop(world);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    /// A map cooked by the package-time adapter (ADR-8024) reports its exact heights, holes
    /// and material blob counts.
    #[test]
    fn a_photoreal_cook_reports_heights_holes_and_material() {
        let root = regia_test_utils::unique_temp_dir("terrain_tiles_photoreal");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let blob = regia_asset_pipeline::cook_tiled_texture(
            "albedo",
            16,
            16,
            &constant(16, [90, 120, 60, 255]),
            16,
            regia_asset_pipeline::TileFormat::Bc7Srgb,
            "linux",
            regia_asset_pipeline::TextureCookQuality::High,
        )
        .expect("blob")
        .tail_ktx2;
        let heights: Vec<f32> = (0..9).map(|i| i as f32 * 0.3 + 0.01).collect();
        let holes = regia_terrain::HolesMask::new(vec![regia_terrain::HoleEllipse {
            center: [0.0, 0.0],
            radius: [1.0, 1.0],
            rotation_radians: 0.0,
        }])
        .expect("holes");
        let input = regia_asset_pipeline::PhotorealTerrainInput {
            map_id: AssetId::sub_asset(AssetId::NIL, "photoreal"),
            width: 3,
            height: 3,
            heights: &heights,
            world_size_m: 10.0,
            world_origin: None,
            height_mapping: None,
            layers: vec!["ground".into()],
            holes: Some(&holes),
            material: Some(regia_asset_pipeline::PhotorealMaterialInput {
                cooked: regia_terrain::CookedMaterialKtx2 {
                    layer_albedo: vec![blob.as_slice()],
                    ..Default::default()
                },
                params: regia_terrain::MaterialParams::default(),
                macro_tiles: 0,
            }),
        };
        let path = "assets/maps/photoreal.level.ron";
        let cook = regia_asset_pipeline::write_photoreal_terrain(&input, path, &cache).expect("adapter");
        let mut entries = cook.manifest.clone();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        AssetManifest { entries }
            .write_to_cache(&cache)
            .expect("manifest");
        let mut world = open(&root, "linux");
        let report = crate::dispatch::call_in_process(
            &mut world,
            METHOD,
            json!({ "asset": format!("{path}#terrain/index") }),
        )
        .expect("synchronous method")
        .expect("report");
        assert_eq!(report["version"], TERRAIN_INDEX_FORMAT_VERSION);
        assert_eq!(report["final_heights"]["width"], 3);
        assert_eq!(report["final_heights"]["encoding"], "residual");
        assert!(report["holes"]["bytes"].as_u64().unwrap_or(0) > 0);
        assert_eq!(report["material"]["layer_albedo"], 1);
        assert_eq!(report["material"]["layer_normal"], 0);
        assert_eq!(report["material"]["macro_tiles"], 0);
        assert!(report["slots"].as_array().is_some_and(Vec::is_empty));
        drop(world);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_missing_tail_reports_terrain_tiles_missing_through_dispatch() {
        let (root, path) = cook_index("android", regia_asset_pipeline::TileFormat::Bc4, "height");
        let cache = cooked_cache_dir(&root, "android");
        let map_id = AssetId::sub_asset(AssetId::NIL, "terrain-tiles-test");
        std::fs::remove_file(cache.join(cooked_file_name(tail_asset_id(map_id, "height"))))
            .expect("remove tail");
        let mut world = open(&root, "android");
        let err = crate::dispatch::call_in_process(&mut world, METHOD, json!({ "asset": path }))
            .expect("synchronous method")
            .expect_err("missing tail");
        assert_eq!(code(&err), "terrain_tiles_missing");
        drop(world);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_non_terrain_asset_is_not_terrain() {
        let root = regia_test_utils::unique_temp_dir("terrain_tiles_wrong");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let id = AssetId::sub_asset(AssetId::NIL, "not-terrain");
        let header = CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_core::id::TypePathHash::of("regia_asset::CookedTexture"),
            asset_id: id,
            importer_version: 1,
            dependencies: Vec::new(),
            payload_len: 1,
        };
        // A one-byte payload is not a terrain index. The type tag is already wrong, so the
        // loader refuses it as another asset before it reads the payload.
        let file = encode_cooked_file(&header, &[0]).expect("file");
        std::fs::write(cache.join(cooked_file_name(id)), file).expect("write");
        let path = "assets/meshes/box.gltf";
        AssetManifest {
            entries: vec![ManifestEntry {
                path: path.to_string(),
                id,
                type_path: "regia_asset::CookedTexture".to_string(),
            }],
        }
        .write_to_cache(&cache)
        .expect("manifest");
        let mut world = open(&root, "linux");
        let err = terrain_tiles(&mut world, &json!({ "asset": path })).expect_err("not terrain");
        assert_eq!(code(&err), "terrain_tiles_not_terrain");
        drop(world);
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
