// SPDX-License-Identifier: MPL-2.0
//! Package-time adapter for a host-built photoreal terrain (ADR-8024, spec 26 § 7,
//! `docs/formats/terrain.md` § 14, `book/agents/terrain-cooked-photoreal.md`).
//!
//! A host whose package step already built a terrain's final ground (mirrored heights with
//! fractional parts and sinks), its holes and its photoreal material (its own macro bake through
//! [`crate::cook_terrain_material`]) hands them to [`write_photoreal_terrain`]. It writes the
//! cooked map under the map id's stable `terrain/*` ids: the terrain index v2, the R16 tiled
//! heightfield (the heights' quantization), the exact heights, the holes and one KTX2
//! `CookedTexture` per material blob, and returns their manifest entries
//! (`<map rel path>#terrain/...`) for the pak and chunk step. At play,
//! `TerrainSharedDraw::from_cooked` builds the map from the exact heights and cuts the holes,
//! and `PakTileSource::load_cooked_material` hands the material to `set_material_cooked` as views
//! of the mapped pak.

use std::path::Path;

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, Heightfield, ManifestEntry, TextureCookUsage,
    cooked_file_name, encode_cooked_file,
};
use regia_terrain::cooked_terrain::{
    MaterialBlobKind, TERRAIN_COOKED_HOLES_TYPE_PATH, TERRAIN_FINAL_HEIGHTS_TYPE_PATH, cooked_holes_asset_id,
    cooked_holes_asset_path, encode_final_heights, final_heights_asset_id, final_heights_asset_path,
    height_mapping_of, material_blob_asset_id, material_blob_asset_path, quantize_heights,
};
use regia_terrain::stream::{
    TERRAIN_INDEX_FORMAT_VERSION, TERRAIN_INDEX_TYPE_PATH, TerrainHeightfieldIndex, TerrainHeightsIndex,
    TerrainHolesIndex, TerrainIndex, TerrainMaterialIndex, terrain_index_asset_id, terrain_index_asset_path,
};
use regia_terrain::{CookedMaterialKtx2, HolesMask, MaterialParams};
use thiserror::Error;

use crate::terrain_data::{
    DEFAULT_TILE_SIZE, TERRAIN_DATA_IMPORTER_VERSION, TILED_HEIGHTFIELD_TYPE_PATH, TerrainDataError,
    cook_tiled_heightfield, encode_cooked_texture, heightfield_asset_path, write_tiled_heightfield_cooked,
};

/// The host's cooked photoreal material: blobs from [`crate::cook_terrain_material`] (pass
/// `cooked.view()`), its knobs and its macro grid side.
#[derive(Clone, Debug)]
pub struct PhotorealMaterialInput<'a> {
    /// The KTX2 blobs, borrowed.
    pub cooked: CookedMaterialKtx2<&'a [u8]>,
    /// The material knobs.
    pub params: MaterialParams,
    /// Macro tile grid side (`MacroBake::tiles`); 0 = no macro set.
    pub macro_tiles: u32,
}

/// Everything the adapter cooks for one terrain map.
#[derive(Clone, Debug)]
pub struct PhotorealTerrainInput<'a> {
    /// The map id every `terrain/*` id derives from (the adapter owns them: the standard
    /// `.terrain.layers.ron` cook must not cook the same map id).
    pub map_id: AssetId,
    /// Width in samples.
    pub width: u32,
    /// Height in samples.
    pub height: u32,
    /// The final heights in metres, row-major, row 0 = −Z edge, already mirrored and sunk.
    pub heights: &'a [f32],
    /// World width in metres.
    pub world_size_m: f32,
    /// World origin in metres, when the host has one.
    pub world_origin: Option<[f32; 3]>,
    /// The R16 mapping `(height_scale, height_offset)`; `None` spans the heights' range.
    pub height_mapping: Option<(f32, f32)>,
    /// Material layer names, bottom-first.
    pub layers: Vec<String>,
    /// The holes to cut, when any.
    pub holes: Option<&'a HolesMask>,
    /// The photoreal material, when the host cooked one.
    pub material: Option<PhotorealMaterialInput<'a>>,
}

/// What [`write_photoreal_terrain`] wrote.
#[derive(Clone, Debug)]
pub struct PhotorealTerrainCook {
    /// The terrain index id (`terrain/index` of the map id).
    pub index_id: AssetId,
    /// The index written.
    pub index: TerrainIndex,
    /// One entry per file written, paths `<rel path>#terrain/...`, index first.
    pub manifest: Vec<ManifestEntry>,
}

/// Errors of [`write_photoreal_terrain`].
#[derive(Debug, Error)]
pub enum PhotorealTerrainError {
    /// The input is inconsistent (sizes, non-finite values, mapping, layer count).
    #[error("photoreal terrain input: {0}")]
    Input(String),
    /// Encoding the exact heights failed.
    #[error("photoreal terrain heights: {0}")]
    Heights(#[from] regia_terrain::stream::TerrainIndexError),
    /// The holes could not be written as `.holes.ron`.
    #[error("photoreal terrain holes: {0}")]
    Holes(String),
    /// The tiled heightfield or a material blob could not be cooked.
    #[error("photoreal terrain data: {0}")]
    Data(#[from] TerrainDataError),
    /// A cooked file could not be encoded.
    #[error("photoreal terrain cooked file: {0}")]
    Cooked(#[from] regia_asset::CookedIoError),
    /// The index could not be serialized.
    #[error("photoreal terrain index: {0}")]
    Postcard(#[from] postcard::Error),
    /// Writing a file failed.
    #[error("photoreal terrain io: {0}")]
    Io(#[from] std::io::Error),
}

/// Writes one `.fckd` with a raw payload under `id` and returns its byte length.
fn write_raw(
    out_dir: &Path,
    id: AssetId,
    type_path: &str,
    payload: &[u8],
) -> Result<u64, PhotorealTerrainError> {
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(type_path),
        asset_id: id,
        importer_version: TERRAIN_DATA_IMPORTER_VERSION,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, payload)?;
    crate::cook_ledger::write_atomic(&out_dir.join(cooked_file_name(id)), &file)?;
    Ok(file.len() as u64)
}

/// The usage stamped on a material blob's `CookedTexture` (metadata: the KTX2 `vkFormat` is
/// what the GPU reads).
fn blob_usage(kind: MaterialBlobKind) -> TextureCookUsage {
    match kind {
        MaterialBlobKind::LayerAlbedo
        | MaterialBlobKind::MacroAlbedo
        | MaterialBlobKind::MacroCoarseAlbedo => TextureCookUsage::ColorSrgb,
        MaterialBlobKind::LayerNormal
        | MaterialBlobKind::MacroNormal
        | MaterialBlobKind::MacroCoarseNormal => TextureCookUsage::NormalMap,
        MaterialBlobKind::LayerHeight => TextureCookUsage::Mask,
        MaterialBlobKind::LayerOrm => TextureCookUsage::ColorLinear,
    }
}

/// The blobs of one kind, as a list (a coarse blob is a list of zero or one).
fn blobs_of<'b>(cooked: &CookedMaterialKtx2<&'b [u8]>, kind: MaterialBlobKind) -> Vec<&'b [u8]> {
    match kind {
        MaterialBlobKind::LayerAlbedo => cooked.layer_albedo.clone(),
        MaterialBlobKind::LayerNormal => cooked.layer_normal.clone(),
        MaterialBlobKind::LayerHeight => cooked.layer_height.clone(),
        MaterialBlobKind::LayerOrm => cooked.layer_orm.clone(),
        MaterialBlobKind::MacroAlbedo => cooked.macro_albedo_tiles.clone(),
        MaterialBlobKind::MacroNormal => cooked.macro_normal_tiles.clone(),
        MaterialBlobKind::MacroCoarseAlbedo => cooked.macro_coarse_albedo.into_iter().collect(),
        MaterialBlobKind::MacroCoarseNormal => cooked.macro_coarse_normal.into_iter().collect(),
    }
}

fn count(n: usize, what: &str) -> Result<u32, PhotorealTerrainError> {
    u32::try_from(n).map_err(|_| PhotorealTerrainError::Input(format!("{n} {what} blobs")))
}

/// Cooks a host-built photoreal terrain into `out_dir` (a cooked cache directory) under the
/// stable ids of `input.map_id`; `rel_path` (the map's manifest path, `assets/...` as `regia cook`
/// writes it) prefixes the manifest paths so the pak's chunk rules place the terrain with its map.
///
/// The R16 heightfield is the heights' quantization with `input.height_mapping` (or their range),
/// and `terrain/heights` restores every f32 bit (residuals when they can, raw otherwise).
///
/// # Errors
///
/// [`PhotorealTerrainError`] for an inconsistent input or a failed write.
pub fn write_photoreal_terrain(
    input: &PhotorealTerrainInput<'_>,
    rel_path: &str,
    out_dir: &Path,
) -> Result<PhotorealTerrainCook, PhotorealTerrainError> {
    let _span = tracing::info_span!("write_photoreal_terrain", path = rel_path).entered();
    let bad = PhotorealTerrainError::Input;
    let (w, h) = (input.width, input.height);
    let expected = (w as usize)
        .checked_mul(h as usize)
        .ok_or_else(|| bad("size overflows".into()))?;
    if w < 2 || h < 2 || input.heights.len() != expected {
        return Err(bad(format!(
            "{} heights for a {w}x{h} grid (at least 2x2)",
            input.heights.len()
        )));
    }
    if !input.world_size_m.is_finite() || input.world_size_m <= 0.0 {
        return Err(bad(format!("world size {} m", input.world_size_m)));
    }
    if input.layers.len() > regia_terrain::layers::MAX_LAYERS {
        return Err(bad(format!(
            "{} layers (at most {})",
            input.layers.len(),
            regia_terrain::layers::MAX_LAYERS
        )));
    }
    let (scale, offset) = match input.height_mapping {
        Some(mapping) => mapping,
        None => height_mapping_of(input.heights).ok_or_else(|| bad("a height is not finite".into()))?,
    };
    if !scale.is_finite() || !offset.is_finite() || scale <= 0.0 {
        return Err(bad(format!("height mapping scale {scale}, offset {offset}")));
    }
    std::fs::create_dir_all(out_dir)?;
    let map_id = input.map_id;
    let mut manifest = Vec::new();
    let entry = |sub: String, id: AssetId, type_path: &str| ManifestEntry {
        path: format!("{rel_path}#{sub}"),
        id,
        type_path: type_path.to_string(),
    };

    // R16 heightfield: the heights quantized with the mapping.
    let heightfield = Heightfield {
        width: w,
        height: h,
        samples: quantize_heights(input.heights, scale, offset),
    };
    let tiled = cook_tiled_heightfield(&heightfield, DEFAULT_TILE_SIZE)?;
    let (hf_id, hf_bytes) = write_tiled_heightfield_cooked(map_id, &tiled, out_dir)?;
    manifest.push(entry(
        heightfield_asset_path(),
        hf_id,
        TILED_HEIGHTFIELD_TYPE_PATH,
    ));

    // Exact heights over that grid.
    let (encoding, payload) = encode_final_heights(w, h, input.heights, &heightfield.samples, scale, offset)?;
    let heights_id = final_heights_asset_id(map_id);
    let heights_bytes = write_raw(out_dir, heights_id, TERRAIN_FINAL_HEIGHTS_TYPE_PATH, &payload)?;
    manifest.push(entry(
        final_heights_asset_path(),
        heights_id,
        TERRAIN_FINAL_HEIGHTS_TYPE_PATH,
    ));

    // Holes as their frozen `.holes.ron` text.
    let holes = match input.holes {
        Some(mask) => {
            let text = mask
                .to_holes_ron()
                .map_err(|e| PhotorealTerrainError::Holes(e.to_string()))?;
            let id = cooked_holes_asset_id(map_id);
            let bytes = write_raw(out_dir, id, TERRAIN_COOKED_HOLES_TYPE_PATH, text.as_bytes())?;
            manifest.push(entry(
                cooked_holes_asset_path(),
                id,
                TERRAIN_COOKED_HOLES_TYPE_PATH,
            ));
            Some(TerrainHolesIndex { bytes })
        }
        None => None,
    };

    // Material: one KTX2 `CookedTexture` per blob.
    let material = match &input.material {
        Some(material) => {
            let cooked = &material.cooked;
            let mut bytes = 0u64;
            for kind in MaterialBlobKind::ALL {
                for (i, blob) in blobs_of(cooked, kind).into_iter().enumerate() {
                    let i = count(i, kind.name())?;
                    let id = material_blob_asset_id(map_id, kind, i);
                    let file = encode_cooked_texture(id, blob_usage(kind), blob)?;
                    crate::cook_ledger::write_atomic(&out_dir.join(cooked_file_name(id)), &file)?;
                    bytes += file.len() as u64;
                    manifest.push(entry(
                        material_blob_asset_path(kind, i),
                        id,
                        "regia_asset::CookedTexture",
                    ));
                }
            }
            Some(TerrainMaterialIndex {
                params: material.params.into(),
                macro_tiles: material.macro_tiles,
                layer_albedo: count(cooked.layer_albedo.len(), "layer albedo")?,
                layer_normal: count(cooked.layer_normal.len(), "layer normal")?,
                layer_height: count(cooked.layer_height.len(), "layer height")?,
                layer_orm: count(cooked.layer_orm.len(), "layer orm")?,
                macro_albedo_tiles: count(cooked.macro_albedo_tiles.len(), "macro albedo")?,
                macro_normal_tiles: count(cooked.macro_normal_tiles.len(), "macro normal")?,
                macro_coarse_albedo: cooked.macro_coarse_albedo.is_some(),
                macro_coarse_normal: cooked.macro_coarse_normal.is_some(),
                bytes,
            })
        }
        None => None,
    };

    let index = TerrainIndex {
        version: TERRAIN_INDEX_FORMAT_VERSION,
        map_id,
        slots: Vec::new(),
        heightfield: Some(TerrainHeightfieldIndex {
            width: w,
            height: h,
            tile_size: DEFAULT_TILE_SIZE,
            tiles_x: w.div_ceil(DEFAULT_TILE_SIZE),
            tiles_y: h.div_ceil(DEFAULT_TILE_SIZE),
            bytes: hf_bytes,
        }),
        layers: input.layers.clone(),
        world_size_m: Some(input.world_size_m),
        world_origin: input.world_origin,
        heights: Some(TerrainHeightsIndex {
            width: w,
            height: h,
            encoding,
            height_scale: scale,
            height_offset: offset,
            bytes: heights_bytes,
        }),
        holes,
        material,
        source_mapping: None,
        source_spacing_z_m: None,
        source_valid_mask: None,
    };
    let index_id = terrain_index_asset_id(map_id);
    let payload = index.to_postcard()?;
    write_raw(out_dir, index_id, TERRAIN_INDEX_TYPE_PATH, &payload)?;
    // The loader's own checks (sizes, counts), before anything ships.
    let written = std::fs::read(out_dir.join(cooked_file_name(index_id)))?;
    regia_terrain::stream::load_terrain_index_bytes(&written)?;
    manifest.insert(
        0,
        entry(terrain_index_asset_path(), index_id, TERRAIN_INDEX_TYPE_PATH),
    );
    Ok(PhotorealTerrainCook {
        index_id,
        index,
        manifest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_terrain::stream::{PakTileSource, TerrainHeightsEncoding};

    fn input<'a>(heights: &'a [f32], holes: Option<&'a HolesMask>) -> PhotorealTerrainInput<'a> {
        PhotorealTerrainInput {
            map_id: AssetId::sub_asset(AssetId::NIL, "adapter-test"),
            width: 5,
            height: 4,
            heights,
            world_size_m: 40.0,
            world_origin: Some([1.0, 2.0, 3.0]),
            height_mapping: None,
            layers: vec!["ground".into()],
            holes,
            material: None,
        }
    }

    /// Cooked from the cache directory (no pak, no GPU): the exact heights come back bit for
    /// bit, the holes equal, and the R16 grid is their quantization.
    #[test]
    fn the_adapter_round_trips_exact_heights_and_holes() {
        let dir = regia_test_utils::unique_temp_dir("photoreal_adapter");
        let heights: Vec<f32> = (0..20)
            .map(|i| (i as f32 * 0.77).cos() * 3.3 - 0.123_456)
            .collect();
        let holes = HolesMask::new(vec![regia_terrain::HoleEllipse {
            center: [2.0, 3.0],
            radius: [4.0, 2.0],
            rotation_radians: 0.0,
        }])
        .expect("holes");
        let cook = write_photoreal_terrain(&input(&heights, Some(&holes)), "assets/maps/a.level.ron", &dir)
            .expect("adapter");
        assert_eq!(cook.manifest[0].path, "assets/maps/a.level.ron#terrain/index");
        assert_eq!(cook.manifest.len(), 4, "index, heightfield, heights, holes");
        let source = PakTileSource::with_verify(cook.index.map_id, None, dir.clone(), None);
        let index = source.load_terrain_index().expect("index");
        assert_eq!(index, cook.index);
        assert_eq!(index.world_origin, Some([1.0, 2.0, 3.0]));
        let r16 = source.load_heightfield().expect("r16");
        let back = source
            .load_final_heights(&index, Some(&r16))
            .expect("heights")
            .expect("some heights");
        let bits = |v: &[f32]| v.iter().map(|h| h.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&back), bits(&heights));
        let entry = index.heights.as_ref().expect("entry");
        assert_eq!(
            r16.samples,
            regia_terrain::cooked_terrain::quantize_heights(
                &heights,
                entry.height_scale,
                entry.height_offset
            )
        );
        assert_eq!(source.load_holes(&index).expect("holes"), Some(holes));
        assert!(source.load_cooked_material(&index).expect("material").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Heights the R16 grid holds exactly store all-zero residuals (they compress away in a pak).
    #[test]
    fn r16_exact_heights_store_zero_residuals() {
        let dir = regia_test_utils::unique_temp_dir("photoreal_adapter_zero");
        let heights: Vec<f32> = (0..20u16)
            .map(|i| regia_terrain::cooked_terrain::r16_height(i * 3000, 10.0, -1.0))
            .collect();
        let mut with_mapping = input(&heights, None);
        with_mapping.height_mapping = Some((10.0, -1.0));
        let cook = write_photoreal_terrain(&with_mapping, "assets/maps/b.level.ron", &dir).expect("adapter");
        let entry = cook.index.heights.as_ref().expect("entry");
        assert_eq!(entry.encoding, TerrainHeightsEncoding::Residual);
        let file = std::fs::read(dir.join(cooked_file_name(final_heights_asset_id(cook.index.map_id))))
            .expect("file");
        let (_, payload) = regia_asset::decode_cooked_file(&file).expect("wrapper");
        let stored = regia_terrain::cooked_terrain::FinalHeightsPayload::parse(payload).expect("payload");
        assert!(
            stored.values.iter().all(|r| r.to_bits() == 0),
            "{:?}",
            stored.values
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Inconsistent input is refused before anything is written.
    #[test]
    fn bad_input_is_refused() {
        let dir = regia_test_utils::unique_temp_dir("photoreal_adapter_bad");
        let heights = vec![0.0f32; 20];
        let mut short = input(&heights[..19], None);
        assert!(matches!(
            write_photoreal_terrain(&short, "assets/m.level.ron", &dir),
            Err(PhotorealTerrainError::Input(_))
        ));
        short.heights = &heights;
        short.world_size_m = 0.0;
        assert!(write_photoreal_terrain(&short, "assets/m.level.ron", &dir).is_err());
        short.world_size_m = 10.0;
        short.height_mapping = Some((0.0, 0.0));
        assert!(write_photoreal_terrain(&short, "assets/m.level.ron", &dir).is_err());
        let nan = vec![f32::NAN; 20];
        assert!(write_photoreal_terrain(&input(&nan, None), "assets/m.level.ron", &dir).is_err());
        assert!(
            !dir.join(cooked_file_name(terrain_index_asset_id(short.map_id)))
                .exists()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
