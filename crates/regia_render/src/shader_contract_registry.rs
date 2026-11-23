// SPDX-License-Identifier: MPL-2.0
//! The registry of Rust and WGSL struct rows for `regia_render` (ADR-3002), and the allowlist of
//! bound structs that have no row yet.
//!
//! A module that owns a GPU type exposes `contract_rows()` (the type and its fields may be
//! private, so the row is built beside them with `offset_of!`); this file concatenates them.
//! Structs private to the frame (`frame/`) have their rows in the frame patch named in the lane
//! report and sit on the allowlist until it lands.

use crate::{AllowEntry, ContractRow};

/// Every `regia_render` row.
#[must_use]
pub fn render_contract_rows() -> Vec<ContractRow> {
    let mut rows = Vec::new();
    rows.extend(crate::ddgi::contract_rows());
    rows.extend(crate::ddgi_gpu::contract_rows());
    rows.extend(crate::mesh_occluders::contract_rows());
    rows.extend(crate::taau::contract_rows());
    rows.extend(crate::boot::contract_rows());
    rows.extend(crate::fade_overlay::contract_rows());
    rows.extend(crate::dof::contract_rows());
    rows.extend(crate::motion_blur::contract_rows());
    rows.extend(crate::local_exposure::contract_rows());
    rows.extend(crate::highend::contract_rows());
    rows.extend(crate::radiance_cache::contract_rows());
    rows.extend(crate::lens_effects::gpu::contract_rows());
    rows.extend(crate::display_stage::contract_rows());
    rows.extend(crate::hwrt::sun_trace::contract_rows());
    rows.extend(crate::hwrt::shadow::contract_rows());
    rows.extend(crate::shadow_occlusion::contract_rows());
    rows.extend(crate::contact_shadows::contract_rows());
    rows.extend(crate::gpu_driven::contract_rows());
    rows.extend(crate::prepass_occlusion::contract_rows());
    rows.extend(crate::thin_film::contract_rows());
    rows.extend(crate::lightmap::contract_rows());
    rows.extend(crate::gi::contract_rows());
    rows.extend(crate::gizmos::render::contract_rows());
    rows.extend(crate::scalar_overlay_pass::contract_rows());
    rows.extend(crate::skinning::contract_rows());
    rows.extend(crate::baked_gi::contract_rows());
    rows.extend(crate::baked_gi::points::contract_rows());
    rows.extend(crate::visbuffer::gpu::contract_rows());
    rows.extend(crate::visbuffer::gpu::software::contract_rows());
    rows.extend(crate::visbuffer::meshlet::contract_rows());
    // The frame-generation passes only exist with the `frame-gen` feature (not on Android).
    #[cfg(feature = "frame-gen")]
    rows.extend(crate::frame_gen::passes::contract_rows());
    rows.extend(crate::fsr3::contract_rows());
    rows.extend(crate::tsr::contract_rows());
    rows.extend(crate::restir_di::gpu::contract_rows());
    rows.extend(crate::restir_di::depth_gbuffer::contract_rows());
    rows.extend(crate::sssr::contract_rows());
    rows.extend(crate::gi_denoise::gpu::contract_rows());
    rows.extend(crate::decal_receiver::contract_rows());
    rows.extend(crate::ssao::contract_rows());
    rows.extend(crate::video_convert::contract_rows());
    rows.extend(crate::outdoor::contract_rows());
    rows.extend(crate::mask_decals::contract_rows());
    rows.extend(crate::visbuffer::gpu::shadow::contract_rows());
    rows
}

/// Bound structs of `regia_render`'s shaders with no row, each with the reason.
#[must_use]
pub fn render_contract_allowlist() -> Vec<AllowEntry> {
    let allow = ALLOWLIST.iter();
    #[cfg(not(feature = "frame-gen"))]
    let allow = allow.chain(FRAME_GEN_WITHOUT_THE_FEATURE);
    allow.copied().collect()
}

/// Why a frame-generation struct is listed in a build without `frame-gen`.
#[cfg(not(feature = "frame-gen"))]
const FRAME_GEN_ROWS: &str = "its row is `frame_gen::passes::contract_rows`, built only with the `frame-gen` feature, \
     while the shader gate scans the module in every build (`frame_gen::wgsl::checked_sources`); a build with the \
     feature (regia_render's own tests) checks the row (ADR-5960 (11))";

/// The structs `frame_gen::passes::contract_rows` checks, for a build without `frame-gen`
/// (xtask's): one entry per row and shader, sorted by file, then struct.
#[cfg(not(feature = "frame-gen"))]
const FRAME_GEN_WITHOUT_THE_FEATURE: &[AllowEntry] = &[
    AllowEntry {
        file: "fg_debug.wgsl",
        wgsl_struct: "FgFrame",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_disocclusion.wgsl",
        wgsl_struct: "FgFrame",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_flow.wgsl",
        wgsl_struct: "FlowLevel",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_flow_field.wgsl",
        wgsl_struct: "FgFrame",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_flow_luma.wgsl",
        wgsl_struct: "LumaParams",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_game_field.wgsl",
        wgsl_struct: "FgFrame",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_inpaint.wgsl",
        wgsl_struct: "FgFrame",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_interpolate.wgsl",
        wgsl_struct: "FgFrame",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_prepare.wgsl",
        wgsl_struct: "FgFrame",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_pyramid.wgsl",
        wgsl_struct: "FgFrame",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_pyramid.wgsl",
        wgsl_struct: "PyramidLevel",
        reason: FRAME_GEN_ROWS,
    },
    AllowEntry {
        file: "fg_reconstruct_depth.wgsl",
        wgsl_struct: "FgFrame",
        reason: FRAME_GEN_ROWS,
    },
];

const PENDING: &str = "no Rust twin row yet: the CPU packer is hand-written or shared, convert when a row for it is written (ADR-3002 ratchet)";

const ADDED_AFTER_TABLE: &str = "no row yet: the struct was added to the shader after the ratchet's first table (v0.2.20 base, ADR-4901 merge); convert when a row is written";

/// Why the young-probe budget's buffer has no row (ADR-7870).
const YOUNG_HEAD: &str = "the CPU writes only its head (`view_proj`, `eye`, `params`: `ddgi::YoungParamsGpu`, \
     `YOUNG_PARAM_WORDS`) at offset 0; `state` and the 256-bin atomic histogram are written by the passes, from zero \
     each frame, so a row's host type would be smaller than the WGSL struct, which the ratchet refuses (ADR-7870)";

const ALLOWLIST: &[AllowEntry] = &[
    AllowEntry {
        file: "baked_gi_points.wgsl",
        wgsl_struct: "BgiPointRange",
        reason: PENDING,
    },
    AllowEntry {
        file: "baked_gi_screen.wgsl",
        wgsl_struct: "BakedGiOccluders",
        reason: PENDING,
    },
    AllowEntry {
        file: "cluster_build.wgsl",
        wgsl_struct: "GpuLight",
        reason: PENDING,
    },
    AllowEntry {
        file: "cluster_build.wgsl",
        wgsl_struct: "LabFrame",
        reason: PENDING,
    },
    AllowEntry {
        file: "ddgi_maxmip.wgsl",
        wgsl_struct: "Heightfield",
        reason: PENDING,
    },
    AllowEntry {
        file: "ddgi_trace.wgsl",
        wgsl_struct: "DdgiYoung",
        reason: YOUNG_HEAD,
    },
    AllowEntry {
        file: "decal_receiver.rs",
        wgsl_struct: "Frame",
        reason: PENDING,
    },
    AllowEntry {
        file: "decal_receiver.rs",
        wgsl_struct: "Mat3x4",
        reason: PENDING,
    },
    AllowEntry {
        file: "fg_flow_luma.wgsl",
        wgsl_struct: "FgState",
        reason: PENDING,
    },
    AllowEntry {
        file: "fg_flow.wgsl",
        wgsl_struct: "FgState",
        reason: PENDING,
    },
    AllowEntry {
        file: "fg_inpaint.wgsl",
        wgsl_struct: "FgState",
        reason: PENDING,
    },
    AllowEntry {
        file: "fg_interpolate.wgsl",
        wgsl_struct: "FgState",
        reason: PENDING,
    },
    AllowEntry {
        file: "forward_mobile.wgsl",
        wgsl_struct: "DirectionalLight",
        reason: PENDING,
    },
    AllowEntry {
        file: "forward_mobile.wgsl",
        wgsl_struct: "GpuLight",
        reason: PENDING,
    },
    AllowEntry {
        file: "forward_mobile.wgsl",
        wgsl_struct: "LabFrame",
        reason: PENDING,
    },
    AllowEntry {
        file: "forward_mobile.wgsl",
        wgsl_struct: "LabMaterial",
        reason: PENDING,
    },
    AllowEntry {
        file: "forward_mobile.wgsl",
        wgsl_struct: "ShadowTile",
        reason: PENDING,
    },
    AllowEntry {
        file: "forward_mobile.wgsl",
        wgsl_struct: "ShadowUniforms",
        reason: PENDING,
    },
    AllowEntry {
        file: "hwrt_shadow_fallback.wgsl",
        wgsl_struct: "BvhNode",
        reason: PENDING,
    },
    AllowEntry {
        file: "immediate.wgsl",
        wgsl_struct: "ImmediateView",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward_graph.wgsl",
        wgsl_struct: "DirectionalLight",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward_graph.wgsl",
        wgsl_struct: "GpuLight",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward_graph.wgsl",
        wgsl_struct: "LabFrame",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward_graph.wgsl",
        wgsl_struct: "LabMaterial",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward_graph.wgsl",
        wgsl_struct: "Mat3x4",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward_graph.wgsl",
        wgsl_struct: "ObjectData",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward_graph.wgsl",
        wgsl_struct: "ShadowTile",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward_graph.wgsl",
        wgsl_struct: "ShadowUniforms",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "DirectionalLight",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "GlassParams",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "GpuLight",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "LabFrame",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "LabMaterial",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "LightmapDraw",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "ManyLightReservoir",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "Mat3x4",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "ObjectData",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "ShadowTile",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "ShadowUniforms",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_shadow_graph.wgsl",
        wgsl_struct: "GraphMaterial",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_shadow_graph.wgsl",
        wgsl_struct: "Mat3x4",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_shadow_graph.wgsl",
        wgsl_struct: "ObjectData",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_shadow_graph.wgsl",
        wgsl_struct: "ShadowVp",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_shadow.wgsl",
        wgsl_struct: "MaskMaterial",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_shadow.wgsl",
        wgsl_struct: "Mat3x4",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_shadow.wgsl",
        wgsl_struct: "ObjectData",
        reason: PENDING,
    },
    AllowEntry {
        file: "lab_shadow.wgsl",
        wgsl_struct: "ShadowVp",
        reason: PENDING,
    },
    AllowEntry {
        file: "post.wgsl",
        wgsl_struct: "PostParams",
        reason: PENDING,
    },
    AllowEntry {
        file: "restir_di.wgsl",
        wgsl_struct: "GpuLight",
        reason: PENDING,
    },
    AllowEntry {
        file: "restir_di.wgsl",
        wgsl_struct: "Reservoir",
        reason: PENDING,
    },
    AllowEntry {
        file: "scalar_overlay.wgsl",
        wgsl_struct: "Mat3x4",
        reason: PENDING,
    },
    AllowEntry {
        file: "sgsr.wgsl",
        wgsl_struct: "PostParams",
        reason: PENDING,
    },
    AllowEntry {
        file: "skin_compute.wgsl",
        wgsl_struct: "Params",
        reason: PENDING,
    },
    AllowEntry {
        file: "smaa.wgsl",
        wgsl_struct: "SmaaInfo",
        reason: PENDING,
    },
    AllowEntry {
        file: "sssr_classify.wgsl",
        wgsl_struct: "Counters",
        reason: PENDING,
    },
    AllowEntry {
        file: "sssr_intersect.wgsl",
        wgsl_struct: "Counters",
        reason: PENDING,
    },
    AllowEntry {
        file: "sssr_prefilter.wgsl",
        wgsl_struct: "Counters",
        reason: PENDING,
    },
    AllowEntry {
        file: "sssr_probe.wgsl",
        wgsl_struct: "Counters",
        reason: PENDING,
    },
    AllowEntry {
        file: "sssr_reproject.wgsl",
        wgsl_struct: "Counters",
        reason: PENDING,
    },
    AllowEntry {
        file: "sssr_resolve.wgsl",
        wgsl_struct: "Counters",
        reason: PENDING,
    },
    AllowEntry {
        file: "sssr_spd.wgsl",
        wgsl_struct: "SpdParams",
        reason: PENDING,
    },
    AllowEntry {
        file: "subsurface_scatter.wgsl",
        wgsl_struct: "ScatterParams",
        reason: PENDING,
    },
    AllowEntry {
        file: "sun_trace_main.wgsl",
        wgsl_struct: "BvhNode",
        reason: PENDING,
    },
    AllowEntry {
        file: "taau_sharpen.wgsl",
        wgsl_struct: "SharpenParams",
        reason: PENDING,
    },
    AllowEntry {
        file: "translucent_shadow.wgsl",
        wgsl_struct: "CasterMaterial",
        reason: PENDING,
    },
    AllowEntry {
        file: "translucent_shadow.wgsl",
        wgsl_struct: "GlassParams",
        reason: PENDING,
    },
    AllowEntry {
        file: "translucent_shadow.wgsl",
        wgsl_struct: "Mat3x4",
        reason: PENDING,
    },
    AllowEntry {
        file: "translucent_shadow.wgsl",
        wgsl_struct: "ObjectData",
        reason: PENDING,
    },
    AllowEntry {
        file: "translucent_shadow.wgsl",
        wgsl_struct: "ShadowVp",
        reason: PENDING,
    },
    AllowEntry {
        file: "velocity_graph.wgsl",
        wgsl_struct: "GraphMaterial",
        reason: PENDING,
    },
    AllowEntry {
        file: "velocity.wgsl",
        wgsl_struct: "Mat3x4",
        reason: PENDING,
    },
    AllowEntry {
        file: "video_fullscreen.wgsl",
        wgsl_struct: "Draw",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "DirectionalLight",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "GpuLight",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "LabFrame",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "LabMaterial",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "LightmapDraw",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "Mat3x4",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "ObjectData",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "ShadowTile",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "ShadowUniforms",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "VisInstance",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "VisLightmap",
        reason: PENDING,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "VisMeshlet",
        reason: PENDING,
    },
    AllowEntry {
        file: "reference_overlay.wgsl",
        wgsl_struct: "Uniforms",
        reason: ADDED_AFTER_TABLE,
    },
    AllowEntry {
        file: "fog_of_war_overlay.wgsl",
        wgsl_struct: "Uniforms",
        reason: "packed by hand as floats in `fog_overlay.rs` (matrix + five vec4); convert when a row is written",
    },
    AllowEntry {
        file: "scripted_vendor.wgsl",
        wgsl_struct: "Sizes",
        reason: ADDED_AFTER_TABLE,
    },
    AllowEntry {
        file: "scripted_vendor.wgsl",
        wgsl_struct: "Texel",
        reason: ADDED_AFTER_TABLE,
    },
    AllowEntry {
        file: "vis_cull.wgsl",
        wgsl_struct: "LevelRun",
        reason: ADDED_AFTER_TABLE,
    },
    AllowEntry {
        file: "vis_raster.wgsl",
        wgsl_struct: "SwayTable",
        reason: ADDED_AFTER_TABLE,
    },
    AllowEntry {
        file: "vis_raster_masked.wgsl",
        wgsl_struct: "SwayTable",
        reason: ADDED_AFTER_TABLE,
    },
    AllowEntry {
        file: "vis_resolve.wgsl",
        wgsl_struct: "SwayTable",
        reason: ADDED_AFTER_TABLE,
    },
    AllowEntry {
        file: "vis_shadow_raster.wgsl",
        wgsl_struct: "SwayTable",
        reason: ADDED_AFTER_TABLE,
    },
    AllowEntry {
        file: "vis_software.wgsl",
        wgsl_struct: "SwayTable",
        reason: ADDED_AFTER_TABLE,
    },
    AllowEntry {
        file: "vis_velocity.wgsl",
        wgsl_struct: "SwayTable",
        reason: ADDED_AFTER_TABLE,
    },
    AllowEntry {
        file: "lab_forward.wgsl",
        wgsl_struct: "FdData",
        reason: "the forward-decal buffer (ADR-3255) ends in a runtime-sized atlas array, so it has no fixed-size Rust twin: `forward_decals::pack_forward_decals` writes the header, decals and masks as words and its tests hold the layout",
    },
];
