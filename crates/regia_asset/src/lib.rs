// SPDX-License-Identifier: MPL-2.0
//! Runtime asset server: handles, loaders, cooked formats, pak files, hot reload.
//!
//! Specification: `docs/specs/12-assets.md`.
//!
//! This module provides the cooked-data types that the runtime loads without
//! parsing source formats (glTF, PNG, etc.). The pipeline crate
//! (`regia_asset_pipeline`) produces these types.
//!
//! # Getting started
//!
//! Every asset has a stable [`AssetId`]; sub-assets (a mesh inside a glTF file) get ids derived
//! from their parent, so cooking twice gives the same ids. Loaded values live in [`Assets`] slabs
//! and are reached through generational [`Handle`]s:
//!
//! ```
//! use regia_asset::{AssetId, Assets};
//!
//! let model = AssetId::new_random();
//! let mesh = AssetId::sub_asset(model, "meshes/0");
//! assert_eq!(mesh, AssetId::sub_asset(model, "meshes/0"));
//! assert_ne!(mesh, AssetId::sub_asset(model, "meshes/1"));
//!
//! let mut names: Assets<String> = Assets::new();
//! let handle = names.insert(mesh, "hull".to_string());
//! assert_eq!(names.get(&handle).map(String::as_str), Some("hull"));
//! assert_eq!(names.handle_for_id(mesh), Some(handle.clone()));
//!
//! // Removing the value makes every handle to it stop resolving.
//! assert!(names.remove(mesh));
//! assert!(names.get(&handle).is_none());
//! ```
//!
//! A running game does not build these by hand: [`AssetServer::open`] reads the cooked cache (or
//! mounts `*.fpak` files) and `load_mesh`, `load_texture` and `load_material` hand back handles.

pub mod baked_gi;
pub mod baked_gi_grid;
pub mod baked_gi_split;
pub mod bcn_decode;
pub mod content;
pub mod content_hook;
mod cooked;
mod cooked_header;
pub mod cooked_pack;
pub mod csv;
pub mod data_table;
pub mod glasscrack;
mod gltf_conventions;
pub mod gpu_mesh;
// Project-wide import settings rules (`config/import_rules.ron`, spec 12 § 10, ADR-3321); shared
// with the agent protocol, which must not depend on the desktop pipeline.
mod id;
pub mod import_rules;
pub mod lightmap;
mod live;
mod load_set;
mod loader;
pub mod lut_cube;
mod manifest;
mod material;
mod mesh_lod;
mod mesh_skin;
mod meta;
mod migrator;
mod mip_stream;
pub mod pak;
pub mod pak_stream;
pub mod pak_verify;
pub mod project_ops;
pub mod registry;
mod server;
mod storage;
// Localized asset variants (spec 36 § 6, ADR-1882).
pub mod localized;
// Voices, dialogue lines and subtitle cue sets (spec 36 §§ 7-8, ADR-1882).
pub mod dialogue;
// CPU decoders for the Android block formats, for adapters without them (ADR-2063).
pub mod astc_decode;
pub mod etc2_decode;
// BC6H and ASTC HDR blocks for baked light, encoder and CPU decoder (ADR-2314).
pub mod hdr_blocks;
// Cooked HDR textures (BC6H / RGBA16F / ASTC HDR KTX2) read on the CPU, cube and equirect directions (ADR-2546).
pub mod hdr_texture;
// Authored collision proxies cooked from `collision_<kind>_*` glTF nodes (ADR-2341).
pub mod collision_set;
// GPU-instanced copies of an imported model's nodes (`EXT_mesh_gpu_instancing`, ADR-3016).
pub mod prefab_instances;
// IES LM-63 photometric profiles and their cooked form (spec 13 § 6, ADR-2592).
pub mod ies;
// The read-only, confined `mods://` file system: load order, overrides, content-set hash (ADR-2418).
pub mod mods_vfs;
// Local reflection capture bakes (`.fcap`, ADR-2860).
pub mod local_capture_bake;

// Load counts and cooked bytes read, for `app.startup_work` (ADR-2987).
pub mod load_stats;

// Bake checkpoints (`.bakeck`): a long lightmap or probe bake continues after a crash (ADR-3095).
pub mod bake_checkpoint;

// Pak update diffs: what shipping a new build's paks over an old one's costs a player
// (spec 22 T2.14, ADR-3322): per-asset and per-chunk comparison plus a SteamPipe-style
// patch-size estimate.
pub mod pak_diff;

// Texture-set detection and material generation (spec 22 T2.14, ADR-3320): shared by the cook
// tools and the agent protocol, so it lives in the asset crate, not the desktop-only pipeline.
pub mod texture_sets;

// `.fpak` archives as read-only `Vfs` mounts (`Vfs::mount_pak`, spec 11 § 4, ADR-7232).
pub mod vfs_pak;
// Custom data assets: `ReflectAsset`, `#[derive(Asset)]` and `.data.ron` (spec 10 § 2.4, ADR-7232).
pub mod data_asset;
// The `engine://` asset source: text assets the engine ships in code (ADR-7232).
pub mod engine_source;
// Asset collections, favorite folders and browser settings (`config/asset_collections.ron`, ADR-7759).
pub mod asset_collections;
// Named import presets per importer (`config/import_presets.ron`, ADR-7904).
pub mod import_presets;

#[cfg(all(feature = "dev-assets", not(target_os = "android")))]
pub mod hot_reload;

pub use collision_set::{
    COLLISION_SET_FORMAT_VERSION, COLLISION_SET_SUB_PATH, COLLISION_SET_TYPE_PATH, CollisionProxyKind,
    CollisionSetError, CookedCollisionGeometry, CookedCollisionSet, CookedCollisionShape,
    collision_set_file_version, collision_set_migration_sources, collision_set_schema_fingerprint,
    load_collision_set_bytes,
};
pub use content_hook::ContentLoaderHook;
pub use cooked::*;
pub use cooked_header::*;
pub use gltf_conventions::*;
pub use gpu_mesh::{
    GPU_MESH_EXTRAS_MAGIC, GPU_MESH_MIN_VERSION, GPU_MESH_TYPE_PATH, GPU_MESH_VERSION, GPU_VERTEX_STRIDE,
    GpuMeshError, GpuMeshExtras, GpuMeshInfo, GpuMeshLod, GpuMeshParts, GpuMeshSubmesh, GpuMeshView,
    encode_gpu_mesh_file, encode_gpu_mesh_file_parts, encode_gpu_mesh_file_with_submeshes,
    encode_gpu_mesh_parts, encode_gpu_mesh_with_submeshes, gpu_mesh_migration_sources,
    gpu_mesh_schema_fingerprint,
};
pub use id::*;
pub use live::*;
pub use load_set::{
    KindProgress, LoadItem, LoadKind, LoadProgress, LoadSet, LoadSets, ProgressCallbackId, refresh_load_sets,
};
pub use loader::*;
pub use manifest::*;
pub use material::*;
pub use mesh_lod::*;
pub use mesh_skin::*;
pub use meta::*;
pub use migrator::*;
pub use mip_stream::{MIP_STREAM_KEY, MIP_STREAM_VERSION, MipStreamMeta};
pub use pak::*;
pub use pak_stream::{PakStreamer, StreamProgress, StreamRequest, StreamedAsset};
pub use pak_verify::{
    PakIntegrity, PakVerifyOptions, PakVerifyOutcome, is_recorded, spawn_pak_verify, spawn_pak_verify_gated,
    verify_pak_file,
};
pub use prefab_instances::{
    PREFAB_INSTANCES_FORMAT_VERSION, PREFAB_INSTANCES_SUB_PATH, PREFAB_INSTANCES_TYPE_PATH,
    PrefabInstanceSet, PrefabInstancedNode, PrefabInstancesError, PrefabVariantSelection,
    load_prefab_instances_bytes, prefab_instances_file_version, prefab_instances_migration_sources,
    prefab_instances_schema_fingerprint,
};
// Assets pushed into a running game by the editor (`assets.push`, ADR-7836).
pub mod push;
// Per-class delivery contract (`.contract.ron`, ADR-8401).
pub mod delivery_contract;
pub use server::*;
pub use storage::*;
pub use vfs_pak::{PakVfsMount, install_vfs_pak_mounts};

// Loaded count and bytes per asset type, on demand for `memory.report` (ADR-9740).
mod type_memory;
pub use type_memory::{AssetTypeMemory, mesh_bytes};

// Per-asset load times (read, decode, upload) for the editor Profiler's Loads tab (P-314).
pub mod load_times;

// Environment-gated asset I/O diagnostics (ADR-8160).
pub mod access_trace;

// The asset id manifest `.assetids.ron` v1: ids of sources not in version control (ADR-8460).
pub mod asset_ids;
pub mod package_parity;

// Pack provenance records (`provenance/*.toml`) into `.meta` provenance (ADR-8834).
pub mod provenance_import;

// Overlay asset roots: the shared configuration and the runtime source lookup (ADR-8155, ADR-8845).
pub mod overlay_roots;
pub use overlay_roots::{ResolvedSource, SourceRoots};
