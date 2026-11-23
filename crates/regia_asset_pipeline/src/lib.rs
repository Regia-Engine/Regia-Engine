// SPDX-License-Identifier: MPL-2.0
//! Desktop-only importers, cookers, asset database and pak writer.
//!
//! Specification: `docs/specs/12-assets.md`.
//!
//! This crate provides functions (not a daemon) for importing source assets
//! into the cooked runtime types defined in `regia_asset`. It depends on
//! desktop-only crates (`gltf`, `image`) that are forbidden in runtime code
//! (`docs/06-testing-and-ci.md` § 7).

pub mod asset_graph;
mod astc_compress;
pub mod astc_encode;
pub mod bc7_encode;
/// ASTC encode and reference decode (the Android cook's codec, astcenc).
pub mod astc {
    pub use crate::astc_compress::{
        AstcCompressError, AstcContent, compress_astc_rgba8, compress_astc_rgba8_preset,
        decompress_astc_rgba8,
    };
}
pub mod atlas_cook;
pub mod bc6h_encode;
pub mod closed_mesh;
mod cook;
pub mod fracture;
mod fracture_clip;
pub mod fracture_cook;
mod fracture_render;
/// ETC2 / EAC encode and decode (the Android cook's fallback codec for GPUs without ASTC, ADR-1379).
pub mod etc2 {
    pub use crate::etc2_compress::{
        Etc2CompressError, Etc2Kind, compress_eac_r11, compress_eac_rg11, compress_etc2_rgb8,
        compress_etc2_rgba8, decode_etc2_rgba8,
    };
}
pub mod collision_cook;
pub mod data_cook;
mod db;
pub mod dcc_feed_bake;
mod etc2_compress;
pub mod etc2_encode;
// Extension content in a cook (specs/57 § 4.4) and importers extensions register (§ 3.3.2).
pub mod extension_cook;
pub mod extension_import;
mod glasscrack_cook;
pub mod gltf_anim;
mod gltf_conventions;
pub mod gltf_extensions;
mod gltf_import;
pub mod gltf_meshopt;
pub mod gltf_normalize;
mod gltf_settings;
mod ktx2_write;
pub mod live;
pub mod lod_classes;
mod matgraph_cook;
mod mesh_cook;
pub mod meshlet;
pub mod meshlet_cook;
mod normal_variance;
pub mod pak_writer;
pub use terrain_material_cook::{TerrainMaterialCookError, cook_terrain_material};
pub mod asset_audit;
pub mod atlas;
pub mod chunks;
mod cook_inputs;
mod cook_ledger;
pub mod fbx_import;
#[cfg(feature = "fbx_import")]
mod fbx_scene;
pub mod generated_mesh_export;
mod gltf_write;
pub use gltf_write::GlbWriteError;
pub mod hlod_cook;
pub mod lightmap_uv;
pub mod mip_stream;
pub mod obj_import;
// Biovision BVH motion capture to skeleton and clip (ADR-2532).
pub mod bvh_import;
// USD (.usda, .usdc, .usdz) through the pure-Rust `openusd` reader to an in-memory glTF (ADR-3235).
pub mod plant_export;
mod platform;
pub mod registry_bridge;
pub mod registry_build;
pub mod scene_export;
pub mod stream_cook;
pub mod terrain_cook;
pub mod terrain_data;
mod terrain_material_cook;
mod texture_compress;
mod texture_settings;
mod texture_share;
pub mod texture_stats;
pub mod tiled_import;
pub mod tiled_wang;
// Aseprite (.aseprite/.ase) and LDtk (.ldtk) importers (ADR-3358): both write the existing 2D
// runtime assets (SpriteAtlas / .tilemap.ron) and report through import_reports.json.
pub mod aseprite_import;
pub mod import_report;
pub mod ldtk_import;
pub mod ui_anim_validate;
pub mod usd_import;
pub mod validation_rules;
pub mod validation_triggers;
pub mod validators;
// Cloth and soft-body cooker and tet generator (spec 55 § 4, ADR-2083).
pub mod cloth_cook;
pub mod tet_gen;
// Fracture set -> runtime destruction chunk asset (ADR-1597).
pub mod destructible_cook;
// The cook's shader-permutation enumeration per render profile (ADR-2026).
pub mod permutation_cook;
// Loudness at import and the DDS importer (ADR-1855, `docs/specs/95-loudness-importers.md`).
pub mod audio_cook;
pub mod audio_transcode;
pub mod dds_import;
// The HDR image importer: .exr / .hdr to BC6H or RGBA16F KTX2, optional cube map (ADR-2546).
pub mod hdr_import;
pub mod svg_import;
// PLY and STL meshes and the PSD image (pure Rust readers, ADR-3244).
pub mod mesh_convert;
pub mod ply_import;
pub mod psd_import;
pub mod stl_import;
// Android texture cook policy and the cook size report (ADR-2063).
pub mod fuzz_entry;
mod texture_policy;
// Texture-set detection and material generation (spec 22 T2.14, ADR-3320).
pub mod texture_set;
// How the cook stores a lightmap's texels (ADR-2314).
pub mod lightmap_cook;
// Lightmaps for every package tier from one bake (ADR-3120).
pub mod lightmap_tiers;
pub mod texture_size_report;
// Authored `collision_*` proxies fitted to collider shapes at import (ADR-2341).
pub mod collision_proxies;
// Baked HLOD proxy atlases (ADR-2483).
pub mod hlod_atlas;
// IES LM-63 photometric profiles cooked for the renderer and the path tracer (ADR-2592).
pub mod ies_cook;
// The video importer: `.webm`/`.mkv` copied and validated, never transcoded (ADR-2410).
pub mod webm_cook;
// The provenance policy `config/provenance.ron` and sub-asset rename re-mapping (ADR-2428).
pub mod provenance_policy;
// The `dialogue_assets` validator (spec 36 § 11, ADR-2755).
pub mod dialogue_validator;
pub mod subasset_remap;
pub use db::{MetaRepair, MetaRepairReport, OrphanMeta, RepairRule};
// Generators: `generators/<name>.toml`, `regia gen run | verify`, provenance generator + seed (ADR-2412).
pub mod generators;
// Motion-matching databases (`.motiondb.ron`, spec 52 § 4) and the cooked-clip NaN scan (ADR-2409).
pub mod motiondb_cook;
// Sparse-brick voxel far proxies cooked beside the cluster hierarchy (ADR-2445).
pub mod fluid_cook;
pub mod voxel_far_cook;
// The shared cook cache: content-addressed cooked outputs a team shares (ADR-2612).
pub mod cook_cache;
pub use cook::cook_project_with_shared_cache;
pub mod font_subset;
pub mod font_subset_fixture;
// References of `.ui.ron` layouts (component `use`, mask assets) for the dependency graph (spec 111, ADR-3147).
pub mod ui_refs;
// Project-wide import settings rules (`config/import_rules.ron`, spec 12 § 10, ADR-3321): the
// cook-time resolution, the editor-facing effective settings and the check command's report.
pub mod import_rules;
// MaterialX (`.mtlx`) import onto material graphs (ADR-3341).
pub mod materialx_import;
// How `regia server --clients N` and play-in-editor's separate processes start a dev server and
// its clients: one code path for both (spec 76 § 10, spec 77 § 10.4, ADR-2682).
pub mod dev_server;
// Alpha-coverage mips and channel-pack recipes (ADR-3090).
pub mod alpha_coverage;
pub mod texture_pack;
// The project's `[cook] texture_quality` (spec 21 § 2, ADR-7217).
pub mod project_texture_quality;
// The cook's montage slot check (spec 34 M1, ADR-7216).
pub mod montage_cook;
// Translator exchange (PO / XLIFF) of a project's localization, shared by `regia loc export|import` and the
// editor's Localization panel (spec 36 § 3, ADR-7677).
pub mod loc_exchange;
// The manifest of an exported asset package (`.regia_assetpkg.ron` v1, spec addendum § 8.9, ADR-7788).
pub mod asset_package;
// Validation before commit: the staged files' check and the pre-commit hook (spec 32 § 4, ADR-7789).
pub mod precommit;
// A glTF file measured for `regia assets contract-check` (ADR-8450).
pub mod contract_observe;
pub mod table_json;
/// Cooked mip-chain tier packages (ADR-8059).
pub mod texture_tiers;

pub use cook::{
    CookError, CookOptions, CookReport, cook_project, gltf_importer_version, gltf_sidecars,
    terrain_input_files_for,
};
pub use cook::{REVIEW_NOTE_TYPE, strip_review_notes};
pub use db::cook_lock_holder;
pub use db::{AssetDatabase, DbEntry, DbError};
pub use glasscrack_cook::{GlassCrackCookError, cook_glasscrack, cook_glasscrack_bytes};
pub use gltf_conventions::{
    build_lod_groups, build_lod_groups_with, build_material_variant_sets, default_lod_screen_size,
    pack_orm_rgba8, parse_lod_name, prefab_socket_set, socket_from_node,
};
pub use gltf_import::{
    GltfImportResult, GltfOpened, GltfSubCook, gltf_extension_report, gltf_materials, import_gltf_glb_bytes,
    import_gltf_glb_bytes_with_handlers, import_gltf_path, import_gltf_path_with_handlers,
    import_gltf_path_with_lod, morph_target_names, open_gltf, open_gltf_checked, parse_gltf_document,
};
pub use gltf_settings::{CollisionImport, GenerateLodsSettings, GltfForward, GltfImporterSettings};
// Per-animation clip options in the glTF `.meta` settings (ADR-7904).
pub use gltf_settings::{ClipImportSettings, ClipRangeSettings};
// Per-mesh and per-material options in the glTF `.meta` settings (ADR-8376).
pub use gltf_import::import_gltf_glb_bytes_with_settings;
pub use gltf_settings::{
    GltfPartKeys, MaterialImportMode, MaterialImportSettings, MeshImportSettings, PartOptions,
    gltf_part_keys, mesh_key_of,
};
pub use ktx2_write::{
    AstcBlock, BcnFormat, Ktx2Layout, write_ktx2_astc, write_ktx2_bcn, write_ktx2_etc2, write_ktx2_layered,
};
pub use live::{LiveCooker, LiveCookerConfig, SourceCook, affected_sources, cook_sources};
pub use meshlet::{
    MeshletAttributes, MeshletBuildError, MeshletSettings, build_meshlet_mesh,
    build_meshlet_mesh_with_attributes,
};
pub use pak_writer::{CookedPack, UNMATCHED_CHUNK, pack_cooked_directory_report};
pub use pak_writer::{
    PAK_GPU_MESHES_ENV, PakWriteError, PakWriteOptions, gpu_meshes_for, migrate_pak_meshes,
    pack_cooked_directory, pack_cooked_directory_with, write_pak,
};
pub use platform::host_platform;
use regia_asset::{AssetId, CookedImage, CpuMesh, Heightfield};
pub use terrain_cook::{
    TerrainCookError, TerrainCookOutcome, TerrainSource, cook_terrain, discover_terrains,
    format_terrain_stats, terrain_input_files, terrain_is_cookable, terrain_settings_hash,
};
pub use terrain_data::{
    DEFAULT_TILE_SIZE, HeightfieldTile, ProfileBudget, RENDER_PROFILES, RenderProfile,
    TERRAIN_DATA_IMPORTER_VERSION, TILED_HEIGHTFIELD_TYPE_PATH, TerrainDataError, TerrainMapBudget,
    TerrainMapParams, TileCook, TileFormat, TiledHeightfield, TiledTextureCook, bcn_chain_bytes,
    cook_tiled_heightfield, cook_tiled_texture, format_budget_report, format_budget_report_for_platform,
    heightfield_asset_id, heightfield_asset_path, tail_asset_id, tail_asset_path, terrain_map_budget,
    terrain_map_budget_for_platform, tile_asset_id, tile_asset_path, tiled_texture_bytes,
    write_tiled_heightfield_cooked, write_tiled_texture_cooked,
};
pub use texture_compress::cook_texture_rgba8_coverage;
pub use texture_compress::{
    AndroidFormatRow, AndroidTextureFormat, android_format_report, cook_texture_rgba8_android,
    format_android_report,
};
pub use texture_compress::{
    TextureCookError, TextureCookQuality, cook_texture_rgba8, transcode_bcn_ktx2_to_astc,
};
pub use texture_policy::{AstcProfile, TexturePolicy, active_texture_policy, with_texture_policy};
pub use texture_settings::{
    HdrEncodingSetting, NormalGreenSetting, OrmChannelOrderSetting, TEXTURE_SUFFIX_TABLE,
    TextureImporterSettings, TextureQualitySetting, TextureSetSlot, TextureSuffixRow, TextureUsageSetting,
    apply_channel_transforms, gltf_slot_color_space, orm_channel_order_from_file_name, suffix_word,
    texture_set_slot_from_file_name, usage_from_file_name,
};
pub use texture_share::{CookedImageError, cook_graph_texture, cook_shared_image, cook_surface_maps};

// Encoded URI helpers shared by desktop import and DCC tools.
pub use gltf_import::{MAX_GLTF_IMAGE_BYTES, MAX_GLTF_IMAGE_EDGE, import_gltf_checked, parse_gltf_checked};
pub use gltf_import::{data_uri_bytes, decode_gltf_uri};
pub use project_texture_quality::{
    ProjectTextureQuality, default_texture_quality, with_default_texture_quality,
};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Forward axis conversion
// ---------------------------------------------------------------------------

/// Whether to convert glTF's +Z forward axis to Regia Engine's −Z forward.
///
/// `Convert` bakes a 180° rotation about +Y into mesh data:
/// `(x, y, z) → (−x, y, −z)`. This is the default and matches the convention
/// in `docs/formats/gltf-conventions.md` and ADR-009.
///
/// `Keep` imports the file unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forward {
    /// Bake a 180° Y-rotation so glTF +Z becomes Regia Engine −Z.
    Convert,
    /// Import the file as-is with no axis conversion.
    Keep,
}

// ---------------------------------------------------------------------------
// Pipeline errors
// ---------------------------------------------------------------------------

/// Errors that can occur during asset import.
#[derive(Debug, Error)]
pub enum ImportError {
    /// glTF parsing failed.
    #[error("glTF parse error: {0}")]
    Gltf(String),

    /// No mesh found in the glTF file.
    #[error("no mesh found in glTF file")]
    NoMesh,

    /// Reserved: no import path constructs it today. A glTF with no usable mesh is `NoMesh`.
    #[error("no primitive in first mesh")]
    NoPrimitive,

    /// A glTF primitive uses an unsupported mode (only triangles, mode 4, are
    /// supported).
    #[error("unsupported primitive mode {0}: only triangles (mode 4) are supported")]
    UnsupportedMode(u32),

    /// The primitive has no position attribute.
    #[error("primitive is missing POSITION attribute")]
    MissingPositions,

    /// A glTF extension stopped the import: required and unreadable, or its data is broken (ADR-3016).
    /// `code` is the agent error code (`gltf_ext_unsupported`, `gltf_meshopt_invalid`,
    /// `gltf_quantization_invalid`, `gltf_instancing_invalid`).
    #[error("{code}: {message}")]
    GltfExtension {
        /// The agent error code.
        code: &'static str,
        /// The extension's name.
        extension: String,
        /// What went wrong.
        message: String,
    },

    /// Image decoding failed.
    #[error("image decode error: {0}")]
    Image(String),

    /// The PNG is not 16-bit grayscale (heightfield import).
    #[error("heightfield PNG must be 16-bit grayscale, got {0}-bit with color type {1:?}")]
    HeightfieldNot16Bit(u8, image::ColorType),

    /// Mesh validation failed after import.
    #[error("mesh validation failed: {0}")]
    MeshValidation(#[from] regia_asset::MeshValidationError),

    /// FBX reading, checking or conversion failed (ADR-1602); `FbxError::code` is the agent
    /// error code.
    #[error("FBX import error: {0}")]
    Fbx(#[from] crate::fbx_import::FbxError),

    /// OBJ reading or conversion failed; `ObjError::code` is the agent error code.
    #[error("OBJ import error: {0}")]
    Obj(#[from] crate::obj_import::ObjError),

    /// BVH reading or conversion failed (ADR-2532); `BvhError::code` is the agent error code.
    #[error("BVH import error: {0}")]
    Bvh(#[from] crate::bvh_import::BvhError),

    /// Aseprite reading or compositing failed (ADR-3358); `AsepriteError::code` is the error code.
    #[error("Aseprite import error: {0}")]
    Aseprite(#[from] crate::aseprite_import::AsepriteError),

    /// LDtk reading or conversion failed (ADR-3358); `LdtkError::code` is the error code.
    #[error("LDtk import error: {0}")]
    Ldtk(#[from] crate::ldtk_import::LdtkError),

    /// PLY reading or conversion failed (ADR-3244); `PlyError::code` is the agent error code.
    #[error("PLY import error: {0}")]
    Ply(#[from] crate::ply_import::PlyError),

    /// STL reading or conversion failed (ADR-3244); `StlError::code` is the agent error code.
    #[error("STL import error: {0}")]
    Stl(#[from] crate::stl_import::StlError),

    /// PSD reading failed (ADR-3244); `PsdError::code` is the agent error code.
    #[error("PSD import error: {0}")]
    Psd(#[from] crate::psd_import::PsdError),

    /// USD reading, composition or conversion failed (ADR-3235); `UsdError::code` is the agent
    /// error code.
    #[error("USD import error: {0}")]
    Usd(#[from] crate::usd_import::UsdError),

    /// The importer of this format was compiled out: the host built the crate without the feature
    /// that carries it (ADR-7972, `default-features = false`). Agent code `importer_disabled`.
    #[error(
        "importer_disabled: the {importer} importer is not built in (regia_asset_pipeline feature `{feature}` is off)"
    )]
    ImporterDisabled {
        /// The importer's name (`fbx`, `usd`, `svg`).
        importer: &'static str,
        /// The Cargo feature that brings it back (`fbx_import`, `usd_import`, `svg_import`).
        feature: &'static str,
    },
}

/// The agent error code of [`ImportError::ImporterDisabled`].
pub const IMPORTER_DISABLED_CODE: &str = "importer_disabled";

/// The optional features this build of the crate carries (ADR-7972), in `Cargo.toml` order. The
/// default build lists all five; a host built with `default-features = false` lists the ones it
/// turned back on.
#[must_use]
pub fn enabled_features() -> Vec<&'static str> {
    let mut on = Vec::new();
    if cfg!(feature = "http_cache") {
        on.push("http_cache");
    }
    if cfg!(feature = "fbx_import") {
        on.push("fbx_import");
    }
    if cfg!(feature = "usd_import") {
        on.push("usd_import");
    }
    if cfg!(feature = "svg_import") {
        on.push("svg_import");
    }
    if cfg!(feature = "audio_transcode") {
        on.push("audio_transcode");
    }
    on
}

/// The [`ImportError::ImporterDisabled`] a cook or an import of a file with extension `ext`
/// (lowercase, no dot) answers in this build, or `None` when its importer is built in (or the
/// extension is not one of a feature's). Never a panic and never a silent skip: the cook reports it
/// as the source's error.
#[must_use]
pub fn disabled_importer(ext: &str) -> Option<ImportError> {
    let (importer, feature, on) = match ext {
        "fbx" => ("fbx", "fbx_import", cfg!(feature = "fbx_import")),
        "usd" | "usda" | "usdc" | "usdz" => ("usd", "usd_import", cfg!(feature = "usd_import")),
        "svg" => ("svg", "svg_import", cfg!(feature = "svg_import")),
        _ => return None,
    };
    (!on).then_some(ImportError::ImporterDisabled { importer, feature })
}

impl ImportError {
    /// The agent error code of an error that has one.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::GltfExtension { code, .. } => Some(code),
            Self::Fbx(error) => Some(error.code()),
            Self::Obj(error) => Some(error.code()),
            Self::Bvh(error) => Some(error.code()),
            Self::Ply(error) => Some(error.code()),
            Self::Stl(error) => Some(error.code()),
            Self::Psd(error) => Some(error.code()),
            Self::Usd(error) => Some(error.code()),
            Self::ImporterDisabled { .. } => Some(IMPORTER_DISABLED_CODE),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// glTF import
// ---------------------------------------------------------------------------

/// Import the first primitive of the first mesh from a glTF 2.0 file
/// (`.glb` or `.gltf`).
///
/// # Node filtering
///
/// Nodes whose name starts with `socket_` or `collision_` are skipped as meshes
/// (`socket_*` become prefab sockets; `collision_*` are collision-only: their fitted shapes are the
/// `collision` sub-asset, [`collision_proxies`], ADR-2341). This function returns the primary mesh
/// only: the first `_LOD0` or non-LOD primitive. LOD groups, sockets and other primitives are
/// dropped here; the cook path keeps them (see `GltfImportResult`), and there authored
/// `_LOD0`…`_LODN` meshes are all imported and grouped into a [`regia_asset::LodGroup`].
///
/// # Forward conversion
///
/// When `forward` is [`Forward::Convert`], positions and normals are rotated
/// 180° about +Y: `(x, y, z) → (−x, y, −z)`. This turns glTF's +Z front
/// into Regia Engine's −Z forward (ADR-009).
///
/// # Normals
///
/// If the file has no normal attribute, flat normals are generated: one
/// area-weighted normal per face, then averaged and normalised per vertex.
///
/// # UVs
///
/// Missing UVs become `(0.0, 0.0)` for every vertex.
///
/// # Errors
///
/// Returns [`ImportError`] if the file cannot be parsed, contains no meshes,
/// or uses an unsupported primitive mode, or the cooked mesh does not unpack.
///
/// The mesh is the cooked form unpacked: vertices are reordered for vertex fetch, UVs pass
/// through f16 and normals through an octahedral encoding, so values differ slightly from
/// the accessors in the file.
pub fn import_gltf(bytes: &[u8], forward: Forward) -> Result<CpuMesh, ImportError> {
    let parent = AssetId::new_random();
    import_gltf_glb_bytes(
        bytes,
        parent,
        forward,
        "linux",
        texture_compress::TextureCookQuality::default(),
    )
    .and_then(|r| {
        r.primary_mesh
            .to_cpu_mesh()
            .map_err(|e| ImportError::Gltf(format!("cooked mesh does not unpack: {e}")))
    })
}

// ---------------------------------------------------------------------------
// PNG import
// ---------------------------------------------------------------------------

/// Import an 8-bit RGBA PNG into a [`CookedImage`].
///
/// The output is sRGB, straight alpha. The `image` crate handles decoding.
///
/// # Errors
///
/// Returns `ImportError::Image` if the bytes are not a decodable image. The decoder sniffs the
/// format, so a JPEG also imports (`cook_png` relies on that for `.jpg` sources).
pub fn import_png_rgba8(bytes: &[u8]) -> Result<CookedImage, ImportError> {
    let img = image::load_from_memory(bytes).map_err(|e| ImportError::Image(e.to_string()))?;

    let rgba8 = img.into_rgba8();
    let (width, height) = rgba8.dimensions();
    let pixels = rgba8.into_raw();

    let cooked = CookedImage {
        width,
        height,
        rgba8: pixels,
    };

    cooked.validate().map_err(|e| ImportError::Image(e.to_string()))?;

    Ok(cooked)
}

/// Import a 16-bit grayscale PNG into a [`Heightfield`].
///
/// 0 = lowest point, 65535 = highest point. Samples are row-major,
/// top-to-bottom.
///
/// # Errors
///
/// Returns `ImportError::HeightfieldNot16Bit` if the PNG is not 16-bit
/// grayscale. Returns `ImportError::Image` for other decoding failures.
pub fn import_png_height(bytes: &[u8]) -> Result<Heightfield, ImportError> {
    let dyn_img = image::load_from_memory(bytes).map_err(|e| ImportError::Image(e.to_string()))?;

    let (width, height, samples_u16) = match dyn_img {
        image::DynamicImage::ImageLuma16(buf) => {
            let (w, h) = buf.dimensions();
            let raw = buf.into_raw();
            // raw is Vec<u16>, each element is one sample
            (w, h, raw)
        }
        other => {
            let color = other.color();
            let bits = u8::try_from(color.bits_per_pixel() / u16::from(color.channel_count().max(1)))
                .unwrap_or(u8::MAX);
            return Err(ImportError::HeightfieldNot16Bit(bits, color));
        }
    };

    let hf = Heightfield {
        width,
        height,
        samples: samples_u16,
    };

    hf.validate().map_err(|e| ImportError::Image(e.to_string()))?;

    Ok(hf)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(unused_imports)]
    use image::ImageEncoder;

    /// ADR-7972: each optional importer answers `importer_disabled` exactly when its feature is
    /// off, naming the feature; formats no feature carries are never disabled.
    #[test]
    fn disabled_importers_follow_the_build_features() {
        let on = enabled_features();
        for (ext, feature) in [
            ("fbx", "fbx_import"),
            ("usd", "usd_import"),
            ("usda", "usd_import"),
            ("usdc", "usd_import"),
            ("usdz", "usd_import"),
            ("svg", "svg_import"),
        ] {
            match disabled_importer(ext) {
                None => assert!(on.contains(&feature), "{ext}: {feature} is off but not disabled"),
                Some(err) => {
                    assert!(!on.contains(&feature), "{ext}: {feature} is on but disabled");
                    assert_eq!(err.code(), Some(IMPORTER_DISABLED_CODE));
                    assert!(err.to_string().contains(feature), "{err}");
                }
            }
        }
        for ext in [
            "glb", "gltf", "png", "jpg", "obj", "ply", "stl", "wav", "ogg", "ron",
        ] {
            assert!(disabled_importer(ext).is_none(), "{ext}");
        }
        assert_eq!(
            on.len(),
            [
                cfg!(feature = "http_cache"),
                cfg!(feature = "fbx_import"),
                cfg!(feature = "usd_import"),
                cfg!(feature = "svg_import"),
                cfg!(feature = "audio_transcode"),
            ]
            .iter()
            .filter(|f| **f)
            .count()
        );
    }

    /// ADR-7972: the glTF cook runs in every build (a host with `default-features = false`
    /// cooks glTF at run time); this is the smallest import of the lane's player use case.
    #[test]
    fn gltf_imports_in_every_feature_set() {
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let glb = make_triangle_glb(&positions, &[0, 1, 2]);
        let mesh = import_gltf(&glb, Forward::Keep).expect("the glTF importer is always built");
        assert_eq!(mesh.indices.len(), 3);
    }

    // -- minimal glTF 2.0 GLB builder (hand-written JSON, no gltf::json) ---

    /// Build a minimal glTF 2.0 GLB binary with one triangle mesh.
    ///
    /// Constructs the GLB directly per the glTF 2.0 specification (Khronos):
    /// 12-byte header + JSON chunk + BIN chunk.
    fn make_triangle_glb(positions: &[[f32; 3]], indices: &[u32]) -> Vec<u8> {
        make_triangle_glb_full(positions, indices, None, None)
    }

    fn make_triangle_glb_full(
        positions: &[[f32; 3]],
        indices: &[u32],
        normals: Option<&[[f32; 3]]>,
        uvs: Option<&[[f32; 2]]>,
    ) -> Vec<u8> {
        let vc = positions.len();

        let mut bin = Vec::new();

        let pos_offset = bin.len();
        for p in positions {
            bin.extend_from_slice(&p[0].to_le_bytes());
            bin.extend_from_slice(&p[1].to_le_bytes());
            bin.extend_from_slice(&p[2].to_le_bytes());
        }

        let nrm_offset = if let Some(nrms) = normals {
            let o = bin.len();
            for n in nrms {
                bin.extend_from_slice(&n[0].to_le_bytes());
                bin.extend_from_slice(&n[1].to_le_bytes());
                bin.extend_from_slice(&n[2].to_le_bytes());
            }
            Some(o)
        } else {
            None
        };

        let uv_offset = if let Some(uv_data) = uvs {
            let o = bin.len();
            for uv in uv_data {
                bin.extend_from_slice(&uv[0].to_le_bytes());
                bin.extend_from_slice(&uv[1].to_le_bytes());
            }
            Some(o)
        } else {
            None
        };

        let idx_offset = bin.len();
        for &i in indices {
            bin.extend_from_slice(&i.to_le_bytes());
        }

        let x0 = positions.iter().map(|p| p[0]).fold(f32::MAX, f32::min);
        let y0 = positions.iter().map(|p| p[1]).fold(f32::MAX, f32::min);
        let z0 = positions.iter().map(|p| p[2]).fold(f32::MAX, f32::min);
        let x1 = positions.iter().map(|p| p[0]).fold(f32::MIN, f32::max);
        let y1 = positions.iter().map(|p| p[1]).fold(f32::MIN, f32::max);
        let z1 = positions.iter().map(|p| p[2]).fold(f32::MIN, f32::max);

        // Build JSON parts
        let pos_accessor = format!(
            r#"{{"bufferView":0,"byteOffset":0,"componentType":5126,"count":{vc},"type":"VEC3","min":[{x0},{y0},{z0}],"max":[{x1},{y1},{z1}]}}"#
        );
        let idx_accessor = format!(
            r#"{{"bufferView":1,"byteOffset":0,"componentType":5125,"count":{ic},"type":"SCALAR"}}"#,
            ic = indices.len(),
        );

        let pos_view = format!(
            r#"{{"buffer":0,"byteOffset":{po},"byteLength":{pl},"byteStride":12,"target":34962}}"#,
            po = pos_offset,
            pl = vc * 12,
        );
        let idx_view = format!(
            r#"{{"buffer":0,"byteOffset":{io},"byteLength":{il},"target":34963}}"#,
            io = idx_offset,
            il = indices.len() * 4,
        );

        let mut accessors = pos_accessor + "," + &idx_accessor;
        let mut buffer_views = pos_view + "," + &idx_view;
        let mut attribs = r#""POSITION":0"#.to_string();
        let has_normals = nrm_offset.is_some();
        let _has_uvs = uv_offset.is_some();

        if let Some(no) = nrm_offset {
            let nv = 2u32;
            let na = 2u32;
            buffer_views.push_str(&format!(
                r#",{{"buffer":0,"byteOffset":{no},"byteLength":{nl},"byteStride":12,"target":34962}}"#,
                no = no,
                nl = vc * 12,
            ));
            accessors.push_str(&format!(
                r#",{{"bufferView":{nv},"byteOffset":0,"componentType":5126,"count":{vc},"type":"VEC3"}}"#
            ));
            attribs.push_str(&format!(r#","NORMAL":{na}"#));
        }

        if let Some(uo) = uv_offset {
            let nv = if has_normals { 3u32 } else { 2u32 };
            let na = if has_normals { 3u32 } else { 2u32 };
            buffer_views.push_str(&format!(
                r#",{{"buffer":0,"byteOffset":{uo},"byteLength":{ul},"byteStride":8,"target":34962}}"#,
                uo = uo,
                ul = vc * 8,
            ));
            accessors.push_str(&format!(
                r#",{{"bufferView":{nv},"byteOffset":0,"componentType":5126,"count":{vc},"type":"VEC2"}}"#
            ));
            attribs.push_str(&format!(r#","TEXCOORD_0":{na}"#));
        }

        let json = format!(
            r#"{{"accessors":[{accessors}],"bufferViews":[{buffer_views}],"buffers":[{{"byteLength":{bl}}}],"meshes":[{{"primitives":[{{"attributes":{{{attribs}}},"indices":1,"mode":4}}]}}],"nodes":[{{"mesh":0}}],"scenes":[{{"nodes":[0]}}],"scene":0,"asset":{{"version":"2.0"}}}}"#,
            bl = bin.len(),
        );

        // GLB packaging
        let mut json_bytes = json.into_bytes();
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }

        let total_len = 12 + 8 + json_bytes.len() + 8 + bin.len();
        let mut glb = Vec::with_capacity(total_len);

        glb.extend_from_slice(&0x46546C67u32.to_le_bytes());
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total_len as u32).to_le_bytes());

        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json_bytes);

        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);

        glb
    }

    // -- import_gltf tests --------------------------------------------------

    #[test]
    fn import_gltf_simple_triangle_keep() {
        let positions = [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
        let normals = [[0.0f32, 1.0, 0.0], [0.0, 1.0, 0.0], [0.0, 1.0, 0.0]];
        let glb = make_triangle_glb_full(&positions, &[0, 1, 2], Some(&normals), None);

        let mesh = import_gltf(&glb, Forward::Keep).expect("import");
        assert_eq!(mesh.positions.len(), 3);
        assert_eq!(mesh.positions[0], [0.0, 0.0, 0.0]);
        assert_eq!(mesh.positions[1], [1.0, 0.0, 0.0]);
        assert_eq!(mesh.positions[2], [0.0, 0.0, 1.0]);
        assert_eq!(mesh.indices, vec![0, 1, 2]);
        assert_eq!(mesh.uvs, vec![[0.0, 0.0]; 3]);
    }

    #[test]
    fn import_gltf_convert_vertex_at_0_1_2() {
        let positions = [[0.0f32, 1.0, 2.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let normals = [[0.0f32, 0.0, 1.0], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0]];
        let glb = make_triangle_glb_full(&positions, &[0, 1, 2], Some(&normals), None);

        let mesh = import_gltf(&glb, Forward::Convert).expect("import");
        assert_eq!(mesh.positions[0], [0.0, 1.0, -2.0]);
        assert_eq!(mesh.positions[1], [-1.0, 0.0, 0.0]);
        assert_eq!(mesh.positions[2], [0.0, 1.0, 0.0]);
        assert_eq!(mesh.normals[0], [0.0, 0.0, -1.0]);
    }

    #[test]
    fn import_gltf_keep_leaves_unchanged() {
        let positions = [[0.0f32, 1.0, 2.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let normals = [[0.0f32, 0.0, 1.0], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0]];
        let glb = make_triangle_glb_full(&positions, &[0, 1, 2], Some(&normals), None);

        let mesh = import_gltf(&glb, Forward::Keep).expect("import");
        assert_eq!(mesh.positions[0], [0.0, 1.0, 2.0]);
        assert_eq!(mesh.positions[1], [1.0, 0.0, 0.0]);
        assert_eq!(mesh.positions[2], [0.0, 1.0, 0.0]);
        assert_eq!(mesh.normals[0], [0.0, 0.0, 1.0]);
    }

    #[test]
    fn import_gltf_generates_normals_when_missing() {
        // CCW winding when viewed from +Y: (0,0,0)→(0,0,1)→(1,0,0) gives +Y normal
        let positions = [[0.0f32, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]];
        let glb = make_triangle_glb(&positions, &[0, 1, 2]);

        let mesh = import_gltf(&glb, Forward::Keep).expect("import");
        assert_eq!(mesh.normals.len(), 3);
        for n in &mesh.normals {
            assert!((n[0]).abs() < 0.001, "normal X={} should be ~0", n[0]);
            assert!((n[1] - 1.0).abs() < 0.001, "normal Y={} should be ~1", n[1]);
            assert!((n[2]).abs() < 0.001, "normal Z={} should be ~0", n[2]);
        }
    }

    #[test]
    fn import_gltf_convert_rotates_generated_normals() {
        // CCW winding when viewed from +Y: (0,0,0)→(0,0,1)→(1,0,0) gives +Y normal
        let positions = [[0.0f32, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]];
        let glb = make_triangle_glb(&positions, &[0, 1, 2]);

        let mesh_keep = import_gltf(&glb, Forward::Keep).expect("import");
        assert!((mesh_keep.normals[0][1] - 1.0).abs() < 0.001);

        let mesh_convert = import_gltf(&glb, Forward::Convert).expect("import");
        assert!((mesh_convert.normals[0][1] - 1.0).abs() < 0.001);
    }

    #[test]
    fn import_gltf_uvs() {
        let positions = [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
        let normals = [[0.0f32, 1.0, 0.0], [0.0, 1.0, 0.0], [0.0, 1.0, 0.0]];
        let uvs = [[0.0f32, 0.0], [1.0, 0.0], [0.5, 1.0]];
        let glb = make_triangle_glb_full(&positions, &[0, 1, 2], Some(&normals), Some(&uvs));

        let mesh = import_gltf(&glb, Forward::Keep).expect("import");
        assert_eq!(mesh.uvs, uvs);
    }

    // -- import_png_rgba8 tests ---------------------------------------------

    #[test]
    fn import_png_rgba8_round_trip() {
        let pixels: Vec<u8> = vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 128, 128, 128, 255];

        let mut png_bytes = Vec::new();
        {
            let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
            encoder
                .write_image(&pixels, 2, 2, image::ExtendedColorType::Rgba8)
                .expect("PNG encode");
        }

        let cooked = import_png_rgba8(&png_bytes).expect("import");
        assert_eq!(cooked.width, 2);
        assert_eq!(cooked.height, 2);
        assert_eq!(cooked.rgba8.len(), 16);
        assert_eq!(cooked.rgba8[0], 255);
        assert_eq!(cooked.rgba8[3], 255);
    }

    #[test]
    fn import_png_rgba8_rejects_bad_bytes() {
        let result = import_png_rgba8(b"not a png file");
        assert!(result.is_err());
    }

    // -- import_png_height tests --------------------------------------------

    #[test]
    fn import_png_height_1x1_value_1000() {
        let mut png_bytes = Vec::new();
        {
            let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
            let raw: [u8; 2] = 1000u16.to_le_bytes();
            encoder
                .write_image(&raw, 1, 1, image::ExtendedColorType::L16)
                .expect("PNG encode");
        }

        let hf = import_png_height(&png_bytes).expect("import");
        assert_eq!(hf.width, 1);
        assert_eq!(hf.height, 1);
        assert_eq!(hf.samples.len(), 1);
        assert_eq!(hf.samples[0], 1000);

        let postcard_bytes = hf.to_postcard().expect("postcard encode");
        let hf2 = regia_asset::Heightfield::from_postcard(&postcard_bytes).expect("postcard decode");
        assert_eq!(hf, hf2);
    }

    #[test]
    fn import_png_height_rejects_8bit() {
        let mut png_bytes = Vec::new();
        {
            let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
            encoder
                .write_image(&[128u8], 1, 1, image::ExtendedColorType::L8)
                .expect("PNG encode");
        }

        let result = import_png_height(&png_bytes);
        assert!(matches!(result, Err(ImportError::HeightfieldNot16Bit(8, _))));
    }

    /// A 16-bit colour PNG reports its real depth, so an author is told the colour type is the
    /// problem and not "8-bit".
    #[test]
    fn import_png_height_reports_the_real_bit_depth() {
        let mut png_bytes = Vec::new();
        {
            let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
            encoder
                .write_image(&[0u8; 6], 1, 1, image::ExtendedColorType::Rgb16)
                .expect("PNG encode");
        }
        match import_png_height(&png_bytes) {
            Err(ImportError::HeightfieldNot16Bit(bits, color)) => {
                assert_eq!(bits, 16);
                assert_eq!(color, image::ColorType::Rgb16);
            }
            other => panic!("expected HeightfieldNot16Bit, got {other:?}"),
        }
    }

    #[test]
    fn import_png_height_rejects_bad_bytes() {
        let result = import_png_height(b"not a png");
        assert!(result.is_err());
    }

    // -- node filtering tests -----------------------------------------------

    fn make_glb_with_node_name(name: &str) -> Vec<u8> {
        let positions = [[0.0f32; 3]; 3];
        let indices = [0u32, 1, 2];
        let vc = positions.len();

        let mut bin = Vec::new();
        for p in &positions {
            bin.extend_from_slice(&p[0].to_le_bytes());
            bin.extend_from_slice(&p[1].to_le_bytes());
            bin.extend_from_slice(&p[2].to_le_bytes());
        }
        for &i in &indices {
            bin.extend_from_slice(&i.to_le_bytes());
        }

        // Hand-built glTF 2.0 JSON with a named node
        let json = format!(
            r#"{{"accessors":[{{"bufferView":0,"byteOffset":0,"componentType":5126,"count":{vc},"type":"VEC3","min":[0.0,0.0,0.0],"max":[0.0,0.0,0.0]}},{{"bufferView":1,"byteOffset":0,"componentType":5125,"count":3,"type":"SCALAR"}}],"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{pl},"byteStride":12,"target":34962}},{{"buffer":0,"byteOffset":{io},"byteLength":12,"target":34963}}],"buffers":[{{"byteLength":{bl}}}],"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0}},"indices":1,"mode":4}}]}}],"nodes":[{{"mesh":0,"name":"{nm}"}}],"scenes":[{{"nodes":[0]}}],"scene":0,"asset":{{"version":"2.0"}}}}"#,
            vc = vc,
            pl = vc * 12,
            io = vc * 12,
            bl = bin.len(),
            nm = name,
        );

        let mut json_bytes = json.into_bytes();
        while !json_bytes.len().is_multiple_of(4) {
            json_bytes.push(b' ');
        }

        let total_len = 12 + 8 + json_bytes.len() + 8 + bin.len();
        let mut glb = Vec::new();
        glb.extend_from_slice(&0x46546C67u32.to_le_bytes());
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total_len as u32).to_le_bytes());
        glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"JSON");
        glb.extend_from_slice(&json_bytes);
        glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        glb.extend_from_slice(b"BIN\0");
        glb.extend_from_slice(&bin);
        glb
    }

    #[test]
    fn import_gltf_skips_socket_nodes() {
        let glb = make_glb_with_node_name("socket_turret");
        let result = import_gltf(&glb, Forward::Keep);
        assert!(matches!(result, Err(ImportError::NoMesh)));
    }

    #[test]
    fn import_gltf_skips_collision_nodes() {
        let glb = make_glb_with_node_name("collision_hull");
        let result = import_gltf(&glb, Forward::Keep);
        assert!(matches!(result, Err(ImportError::NoMesh)));
    }

    #[test]
    fn import_gltf_lod1_alone_is_the_primary() {
        // A file whose only mesh is `_LOD1` still imports, and that mesh is the
        // primary (ADR-7959). A `LodGroup` still needs `_LOD0` plus another level.
        let glb = make_glb_with_node_name("Body_LOD1");
        let mesh = import_gltf(&glb, Forward::Keep).expect("lone LOD1 imports");
        assert_eq!(mesh.positions.len(), 3);
    }

    #[test]
    fn import_gltf_imports_lod0_nodes() {
        let glb = make_glb_with_node_name("Body_LOD0");
        let mesh = import_gltf(&glb, Forward::Keep).expect("import");
        assert_eq!(mesh.positions.len(), 3);
    }
}

/// Named editor launch recipes, stored as project tool data (v1).
pub mod launch_profiles;

/// Read-only geometry and rig lint for glTF sources.
pub mod gltf_lint;

/// glTF material authoring lint and metadata-only fixes (ADR-8061).
pub mod gltf_material_lint;

/// Cook diet rules: per-glob texture edge caps, a triangle ceiling that drops LODs, deterministic
/// variant picks (DO27-absorb row C-04, ADR-8461).
pub mod cook_diet;

// One bounded root-evidence resolver for chunk cooks and named build profiles (CT-25, ADR-8477).
pub mod closure_roots;
/// Terrain mesh-layer cook: W-06's canonical half plus the chunk LOD meshes (spec 26 § 2a.5, ADR-8398).
pub mod terrain_mesh_layers;

/// The `regia cook` pass that cooks every level's mesh layers from its terrain block (ADR-8451).
pub mod level_mesh_layers;

/// Package-time adapter for a host-built photoreal terrain: exact heights, holes and a cooked
/// material under the map's stable `terrain/*` ids (ADR-8024).
pub mod terrain_photoreal_cook;
pub use terrain_photoreal_cook::{
    PhotorealMaterialInput, PhotorealTerrainCook, PhotorealTerrainError, PhotorealTerrainInput,
    write_photoreal_terrain,
};
/// Shared bounded input policy for authoring diagnostics.
pub mod content_lint;
pub use gltf_import::open_gltf_bounded;

/// Project source-lint policy and evidence reports (ADR-8248).
pub mod source_lint;

mod gltf_texture_share;
/// CLI and cook/package adapters over source lint (ADR-8390).
pub mod source_lint_cache;
// Overlay asset roots: fill missing glTF payloads before a cook (ADR-8155).
pub mod overlay_roots;
/// Texture header evidence for the shared source-lint stage (ADR-8536).
pub mod texture_lint;
