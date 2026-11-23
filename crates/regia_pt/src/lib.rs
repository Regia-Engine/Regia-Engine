// SPDX-License-Identifier: MPL-2.0
//! Regia Engine's reference path tracer (`docs/specs/27-renderer-excellence.md` § 1).
//!
//! An offline ground-truth renderer: wgpu compute kernels (one code path for lavapipe, RADV
//! and NVIDIA), fed the same extracted world as the real-time frame ([`extract`]) and shading
//! with the forward pass's BRDF (`regia::pbr::brdf`, imported through naga_oil). Progressive
//! and deterministic per seed; outputs the AOVs of § 1.2 as a layered linear EXR plus an AgX
//! preview through the runtime's display transform ([`output`]).
//!
//! - [`PtScene`]: meshes, instances, materials, lights, sun, sky, terrain heightfield, camera.
//! - [`PathTracer`] / [`PtSession`]: compile once, upload a scene, accumulate samples.
//! - [`PtImage`]: the AOVs and metadata; EXR/PNG writers.
//! - [`scenes`]: the validation scenes of § 1.5 (furnace, analytic lights, Cornell box).
//! - [`extract`]: `FrameScene` (+ atmosphere, terrain, scatter) → [`PtScene`].
//! - [`probe`]: probe baking (L1 SH per light component, distance moments, relocation,
//!   filtering) into a `regia_asset::baked_gi::BakedGiVolume` (`regia bake-gi`).
//! - [`bake_desc`], [`bake_meshes`], [`auto_bricks`]: `.bake.ron` descriptions with static
//!   meshes, interiors and automatic detail bricks (`regia bake-gi`, ADR-279).
//! - [`cpu_probe`]: the probe trace on the CPU, for bakes without a GPU (`regia bake-gi
//!   --cpu`, ADR-365).
//! - [`leak`]: probe leak control (walls between neighboring probes, ADR-366).
//! - [`env_map`], [`hdr`], [`bake_inputs`], [`analytic`]: HDRI skies (equirect, RGBE/EXR,
//!   sun extraction), area emitters as bake inputs, and the analytic references the bake's
//!   energy checks use (ADR-1095).
//! - [`bake_quality`]: bounce budget, sample groups, firefly suppression and the noise estimate
//!   of probe bakes (ADR-1096).
//! - [`bake_sizing`]: a synthetic large-map stand-in and the sizing model of a probe layout
//!   (probes, volume bytes, bake rays and memory; ADR-1099).
//! - [`presets`], [`denoise_atrous`], `render_preset`: named quality presets (draft, preview, final,
//!   reference), the built-in edge-avoiding a-trous denoiser, adaptive sampling and caustics modes
//!   (ADR-2433).
//! - `denoise` (feature `denoise`): OIDN previews of low-spp traces, never a reference.
//! - [`bake_incremental`]: incremental bakes that trace only the cells near an edit (`regia
//!   bake-gi --incremental`, ADR-1094).
//! - [`cloud`]: a path-traced cloud layer (a scattering slab) that validates the real-time cloud
//!   shadow map and the overcast sky light (ADR-320/321, ADR-1834).
//! - [`cloud_compare`]: the raster sky's cloud density as a traced medium (`PtScene::clouds`,
//!   delta and ratio tracking) against the raster march and shadow map, in numbers (ADR-2127).
//! - [`placement`]: probe placement before the bake: adaptive detail bricks denser near
//!   geometry, solid culling and leak-safe offsets (ADR-418).
//!
//! The tracer never ships in a game build and is not a dependency of runtime crates (§ 6).

pub mod auto_bricks;
pub mod bake_desc;
pub mod bake_incremental;
pub mod bake_meshes;
pub mod bake_quality;
pub mod bake_sizing;
pub mod bvh;
pub mod cloud;
pub mod cloud_compare;
pub mod compare;
pub mod cpu_brdf;
pub mod cpu_probe;
#[cfg(feature = "denoise")]
pub mod denoise;
pub mod denoise_atrous;
pub mod exr;
pub mod extract;
pub mod leak;
pub mod lightmap;
pub mod output;
mod pack;
pub mod placement;
pub mod presets;
pub mod probe;
pub mod probe_rays;
pub mod radiosity;
pub mod scene;
pub mod scenes;
mod sha256;
/// Lowercase hex SHA-256 of a file's contents, streamed (FIPS 180-4).
pub use sha256::file_hex as sha256_file_hex;
mod render_preset;
pub mod terrain_bake;
mod tracer;

// Bake light inputs: HDRI skies, area emitters, analytic energy references (ADR-1095).
pub mod analytic;
pub mod bake_inputs;
pub mod env_map;
pub mod hdr;
// CPU mirror of the shader's ray/triangle arithmetic, for precision tests (ADR-1411).
#[cfg(test)]
mod gpu_ray_mirror;
// Checkpointed, resumable lightmap and probe bakes (ADR-3095).
pub mod checkpoint;

pub use output::{Aov, PtImage, PtMetadata};
pub use pack::PtCache;
pub use presets::{
    AdaptiveSettings, BIASED_MARK, CausticsMode, DenoiserKind, MAX_SPP, PresetSettings, PtPreset, PtQuality,
};
pub use render_preset::{PresetReport, PresetResult, TileReport, render_preset, run_preset};
pub use scene::{
    PtCamera, PtCloudDensity, PtCloudField, PtCloudPhase, PtDetail, PtHeightfield, PtInstance, PtLight,
    PtMaterial, PtMedia, PtMediumLayer, PtMesh, PtPhase, PtScene, PtSky, PtSkyBody, PtSun, PtTexture,
    PtViewAlbedo,
};
// IES profiles on analytic lights (ADR-2592).
pub use scene::PtLightProfile;
// Material graphs and subsurface scattering in the tracer (ADR-2725).
pub mod graph;
pub use graph::{PtGraphRefusal, PtGraphStats};
pub use scene::{PtSubsurface, PtSurfaceGraph};
pub use tracer::{MAX_PATH_VERTICES, PathTracer, PtSession, PtSettings};
// Projected decals in the reference trace (ADR-2951).
pub mod decal;
pub use decal::PtDecal;

/// Path tracer errors.
#[derive(Debug, thiserror::Error)]
pub enum PtError {
    /// The scene cannot be traced as given.
    #[error("path tracer scene: {0}")]
    Scene(String),
    /// Shader, pipeline or GPU execution failure.
    #[error("path tracer gpu: {0}")]
    Gpu(String),
    /// Writing or reading an output file.
    #[error("path tracer io: {0}")]
    Io(String),
    /// A denoised image was given as a comparison reference (`specs/27` § 2): denoising biases
    /// the image, so only a raw trace may be the ground truth. Carries the denoiser name.
    #[error(
        "the reference is denoised ({0}); a denoised trace is a preview, never a reference (specs/27 § 2): trace the reference without denoise"
    )]
    DenoisedReference(String),
}

/// `true` when this build has the GPU lightmap baker's host types (`lightmap-gpu`), so
/// [`shader_contract_rows`] has rows for `lightmap_bake.wgsl`.
pub const LIGHTMAP_GPU_CONTRACT_ROWS: bool = cfg!(feature = "lightmap-gpu");

/// Rust and WGSL struct rows of the path tracer and the GPU lightmap baker: the host type of each
/// uniform and storage struct, built beside the private types (`regia_render::shader_contract`,
/// ADR-3002).
#[must_use]
pub fn shader_contract_rows() -> Vec<regia_render::ContractRow> {
    let mut rows = pack::contract_rows();
    rows.extend(tracer::contract_rows());
    #[cfg(feature = "lightmap-gpu")]
    rows.extend(lightmap::gpu_contract_rows());
    rows
}
