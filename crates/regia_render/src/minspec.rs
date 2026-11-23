// SPDX-License-Identifier: MPL-2.0
//! Minimum-spec device profiles: what the renderer may require of the GPU
//! (`specs/13` § 2, lane `minspec-features`).
//!
//! Regia Engine is developed on Blackwell (RTX 5080/5090) and lavapipe, but DO27's Steam players
//! have older cards. A [`MinspecProfile`] captures what a tier of real GPUs exposes, so a
//! device can be created — on any adapter, in tests on lavapipe — with the limits and
//! features of that tier clamped in: every wgpu limit is min()'d with the profile's number,
//! and the optional features the engine requests are intersected with the profile's mask.
//! Whatever the frame path still needs beyond the profile then fails loudly (a validation
//! error naming the limit), instead of quietly working only on the newest hardware.
//!
//! The profiles are derived from public Vulkan device reports (vulkan.gpuinfo.org, CC BY 4.0;
//! report ids cited per number below), not from vendor marketing material:
//!
//! | GPU | Report | Driver | API |
//! |---|---|---|---|
//! | NVIDIA GeForce GTX 1080 Ti (Pascal) | Linux `52316` (also 470-era `13896`, Windows `51723`) | 580.178.4.0 | Vulkan 1.4.312 |
//! | NVIDIA GeForce RTX 3080 (Ampere) | Linux `51786`, Windows `52370` | 615.71.9.0 | Vulkan 1.4.351 |
//! | ARM Mali-G715/G710 (Android) | `51090` | 1.508.955 | Vulkan 1.3.216 |
//!
//! Pascal got Vulkan 1.3 with driver 470+ (report `13896`: 470.62.22, API 1.3.203) and the
//! current 580 line keeps it; every feature Regia Engine optionally requests is present on both
//! generations except ASTC/ETC2 decompression (desktop NVIDIA never had it). The numbers
//! below use the lower of the two generations where they differ (invocations per workgroup:
//! Pascal 1536, Ampere 1024).
//!
//! Limit values are quoted **as wgpu exposes them**, not the raw Vulkan numbers: the profile
//! clamps wgpu's [`Limits`], which already carries wgpu's backend caps (e.g. wgpu caps
//! `max_bind_groups` at 8 and clamps `max_storage_buffer_binding_size` to `i32::MAX`). The
//! values here were measured on a real GTX 1080 through wgpu 30 / Vulkan (the `minspec-hw`
//! lane report, § 2); where the raw Vulkan value differs it is noted on the constant, so the
//! two are not confused.

use wgpu::{Features, Limits};

/// A minimum-spec tier the renderer must run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinspecProfile {
    /// NVIDIA Pascal desktop (GTX 10xx) via the current driver, and Ampere (RTX 30xx),
    /// which exposes the same or more. The floor of DO27's desktop Steam hardware.
    Pascal,
    /// A mid-range Android Vulkan 1.1 tier (Adreno 7xx / Mali-G7xx class): the floor of the
    /// mobile quality presets. Every limit is min()'d with a conservative mobile floor — below
    /// what the current mid-range hardware (Mali-G710, gpuinfo report `51090`) exposes, but
    /// always at or above what the engine itself requires, so a mobile frame still runs (the
    /// shared terrain draw's 5 bind groups, the forward shader's 27 sampled textures, and the
    /// 4/8 storage texture/buffer caps that exercise the per-mip Hi-Z and forward-only
    /// vis-buffer fallbacks).
    Mobile,
}

/// Pascal: desktop-NVIDIA limit floor (gpuinfo report 52316; identical in `13896`/`51723`,
/// so unchanged from the 470-era driver through 580, Linux and Windows).
const PASCAL_MAX_TEXTURE_DIMENSION_2D: u32 = 32768;
const PASCAL_MAX_TEXTURE_DIMENSION_3D: u32 = 16384;
const PASCAL_MAX_IMAGE_ARRAY_LAYERS: u32 = 2048;
/// wgpu caps `max_bind_groups` at 8 on every backend, so a Pascal device reports 8 through
/// wgpu even though its raw Vulkan `maxBoundDescriptorSets` is 32. The engine's shared
/// terrain draw needs 5, so 8 is comfortably above it.
const PASCAL_MAX_BIND_GROUPS: u32 = 8;
const PASCAL_MAX_UNIFORM_BUFFERS_PER_SHADER_STAGE: u32 = 15;
const PASCAL_MAX_UNIFORM_BUFFER_BINDING_SIZE: u64 = 64 << 10;
/// wgpu clamps `max_storage_buffer_binding_size` to `i32::MAX` (rounded down to 4-byte
/// alignment), so the real GTX 1080 reports 2_147_483_644, not the raw Vulkan
/// `maxStorageBufferRange` of 4 GiB − 1.
const PASCAL_MAX_STORAGE_BUFFER_BINDING_SIZE: u64 = 2_147_483_644;
/// wgpu exposes `max_buffer_size` as Vulkan `maxMemoryAllocationSize` (NVIDIA `0xFFE0_0000` =
/// 4 GiB − 2 MiB); the previous "1 TiB" was higher than any Pascal device reports.
const PASCAL_MAX_BUFFER_SIZE: u64 = 4_292_870_144;
const PASCAL_MAX_COMPUTE_WORKGROUP_STORAGE_SIZE: u32 = 48 << 10;
/// Pascal reports 1536 and Ampere 1024 invocations per workgroup; the engine's largest
/// workgroup is 256.
const PASCAL_MAX_COMPUTE_INVOCATIONS_PER_WORKGROUP: u32 = 1024;
const PASCAL_MAX_COLOR_ATTACHMENTS: u32 = 8;
/// The real GTX 1080 reports 31 via wgpu (the `minspec-hw` lane report, § 2), not the WebGPU
/// downlevel default of 16 the profile previously used: 16 is stricter than the weakest
/// supported GPU and could reject a shader that runs on Pascal. (Raw Vulkan
/// `maxInterStageShaderVariables` is 32; wgpu reports 31.)
const PASCAL_MAX_INTER_STAGE_SHADER_VARIABLES: u32 = 31;

impl MinspecProfile {
    /// Explain which optional features are outside this profile and which limits it clamps.
    ///
    /// This is a projection of [`Self::feature_mask`] and [`Self::clamp_limits`], so it stays
    /// aligned with the profile policy instead of maintaining a second tier table.
    #[must_use]
    pub fn explain(self) -> MinspecExplain {
        let mask = self.feature_mask();
        let features = Features::all()
            .iter_names()
            .filter(|(_, feature)| !mask.contains(*feature))
            .map(|(name, _)| format!("feature {name} is off"))
            .collect();

        let input = wide_limits();
        let defaults = self.clamp_limits(&Limits::default());
        let wide = self.clamp_limits(&input);
        // A field is capped when the clamp lowers it below the wide input of its own type
        // (`u32::MAX` or `u64::MAX`); an unconstrained `u32` field stays at `u32::MAX`.
        let limits = limit_fields(&defaults, &wide)
            .into_iter()
            .zip(limit_fields(&Limits::default(), &input))
            .filter(|((_, _, cap), (_, _, ceiling))| cap < ceiling)
            .map(|((field, _, cap), _)| format!("limit {field} is capped at {cap}"))
            .collect();
        MinspecExplain { features, limits }
    }

    /// Short profile name for logs and test output.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pascal => "pascal",
            Self::Mobile => "mobile",
        }
    }

    /// The optional wgpu features this tier's GPUs expose: the engine's request is
    /// intersected with this mask, so a clamped device never carries more than the tier has.
    ///
    /// Pascal/Ampere (reports 52316/51786): `drawIndirectFirstInstance`, `drawIndirectCount`,
    /// `textureCompressionBC`, `hostQueryReset` + 64 timestamp-valid bits (wgpu's
    /// `TIMESTAMP_QUERY` family), `pipelineCreationCacheControl`, R11G10B10 colour
    /// attachment + blend, and linear filtering of `R32_SFLOAT`. Desktop NVIDIA exposes no
    /// ASTC/ETC2. Mobile keeps only what that tier reliably has (ASTC/ETC2 decompression
    /// and Vulkan-core early depth tests): the Vulkan-1.1-era mid-range lacks timestamps,
    /// BC, indirect count and first-instance, Rg11b10 render targets, float32 filtering
    /// and pipeline caches — a current Mali-G710 (`51090`) has several of those, but the
    /// tier floor keeps them off so the fallbacks are exercised.
    #[must_use]
    pub fn feature_mask(self) -> Features {
        match self {
            Self::Pascal => {
                Features::TIMESTAMP_QUERY
                    | Features::TIMESTAMP_QUERY_INSIDE_ENCODERS
                    | Features::TIMESTAMP_QUERY_INSIDE_PASSES
                    | Features::MULTI_DRAW_INDIRECT_COUNT
                    | Features::INDIRECT_FIRST_INSTANCE
                    | Features::RG11B10UFLOAT_RENDERABLE
                    | Features::TEXTURE_COMPRESSION_BC
                    | Features::SHADER_EARLY_DEPTH_TEST
                    | Features::FLOAT32_FILTERABLE
                    | Features::PIPELINE_CACHE
                    // Every Vulkan device has it (wgpu's Vulkan backend clears with a copy or a
                    // render pass); the stochastic SSR wake zeroes its kept targets with it.
                    | Features::CLEAR_TEXTURE
            }
            Self::Mobile => {
                Features::TEXTURE_COMPRESSION_ASTC
                    | Features::TEXTURE_COMPRESSION_ETC2
                    | Features::SHADER_EARLY_DEPTH_TEST
            }
        }
    }

    /// Clamps `adapter` limits down to what the tier's GPUs expose: every field is the
    /// minimum of the adapter's and the profile's number, so the device never carries a
    /// limit the real hardware lacks (fields the tier does not constrain keep the
    /// adapter's value).
    #[must_use]
    pub fn clamp_limits(self, adapter: &Limits) -> Limits {
        match self {
            Self::Pascal => {
                let mut l = adapter.clone();
                l.max_texture_dimension_1d = l.max_texture_dimension_1d.min(PASCAL_MAX_TEXTURE_DIMENSION_2D);
                l.max_texture_dimension_2d = l.max_texture_dimension_2d.min(PASCAL_MAX_TEXTURE_DIMENSION_2D);
                l.max_texture_dimension_3d = l.max_texture_dimension_3d.min(PASCAL_MAX_TEXTURE_DIMENSION_3D);
                l.max_texture_array_layers = l.max_texture_array_layers.min(PASCAL_MAX_IMAGE_ARRAY_LAYERS);
                l.max_bind_groups = l.max_bind_groups.min(PASCAL_MAX_BIND_GROUPS);
                l.max_uniform_buffers_per_shader_stage = l
                    .max_uniform_buffers_per_shader_stage
                    .min(PASCAL_MAX_UNIFORM_BUFFERS_PER_SHADER_STAGE);
                l.max_uniform_buffer_binding_size = l
                    .max_uniform_buffer_binding_size
                    .min(PASCAL_MAX_UNIFORM_BUFFER_BINDING_SIZE);
                l.max_storage_buffer_binding_size = l
                    .max_storage_buffer_binding_size
                    .min(PASCAL_MAX_STORAGE_BUFFER_BINDING_SIZE);
                l.max_buffer_size = l.max_buffer_size.min(PASCAL_MAX_BUFFER_SIZE);
                l.max_compute_workgroup_storage_size = l
                    .max_compute_workgroup_storage_size
                    .min(PASCAL_MAX_COMPUTE_WORKGROUP_STORAGE_SIZE);
                l.max_compute_invocations_per_workgroup = l
                    .max_compute_invocations_per_workgroup
                    .min(PASCAL_MAX_COMPUTE_INVOCATIONS_PER_WORKGROUP);
                l.max_compute_workgroup_size_x = l
                    .max_compute_workgroup_size_x
                    .min(PASCAL_MAX_COMPUTE_INVOCATIONS_PER_WORKGROUP);
                l.max_compute_workgroup_size_y = l
                    .max_compute_workgroup_size_y
                    .min(PASCAL_MAX_COMPUTE_INVOCATIONS_PER_WORKGROUP);
                l.max_compute_workgroup_size_z = l.max_compute_workgroup_size_z.min(64);
                l.max_color_attachments = l.max_color_attachments.min(PASCAL_MAX_COLOR_ATTACHMENTS);
                l.max_inter_stage_shader_variables = l
                    .max_inter_stage_shader_variables
                    .min(PASCAL_MAX_INTER_STAGE_SHADER_VARIABLES);
                l
            }
            Self::Mobile => {
                // The conservative mobile floor: wgpu's downlevel (GLES-3.1-compatible) tier
                // where that is lower than the real hardware (Mali-G710, report `51090`,
                // exposes 16 KiB workgroup storage, 512 invocations, 8 bind groups and
                // half a million descriptors per stage; Vulkan-1.1-era mid-range exposes
                // less). Storage textures stay at 4 on purpose: that is below the 7 the
                // single-pass depth downsample wants, so the per-mip fallback is exercised.
                // 8192-pixel textures sit 2× below the G710's 16384 to cover that older
                // tier. 256 invocations is exactly the engine's largest workgroup (16×16).
                let mut l = adapter.clone();
                l.max_texture_dimension_1d = l.max_texture_dimension_1d.min(8192);
                l.max_texture_dimension_2d = l.max_texture_dimension_2d.min(8192);
                l.max_texture_dimension_3d = l.max_texture_dimension_3d.min(512);
                l.max_texture_array_layers = l.max_texture_array_layers.min(2048);
                // The shared terrain draw needs 5 bind groups (group 4 is the VT colour atlas
                // / cascade shadow view-projection), so the tier floor is the engine's own
                // minimum — below real mid-range (Mali-G710, report 51090, has 8) but the
                // lowest a terrain frame can run.
                l.max_bind_groups = l.max_bind_groups.min(crate::TERRAIN_SHARED_BIND_GROUPS);
                // The forward shader samples 30 textures in its fragment stage (the 19 IBL /
                // shadow / contact / translucent / LTC / emitter / cloud-map / studio bindings, plus 7
                // material and 4 glass textures), so the sampled-texture floor must be above that — the
                // WebGPU/GLES-3.1 minimum of 16 is below the engine's own forward shader. Real
                // mid-range (Mali-G710, report 51090) exposes 1048576; 32 is a runnable floor
                // that still sits far below it.
                l.max_sampled_textures_per_shader_stage = l.max_sampled_textures_per_shader_stage.min(32);
                l.max_samplers_per_shader_stage = l.max_samplers_per_shader_stage.min(32);
                l.max_storage_buffers_per_shader_stage = l.max_storage_buffers_per_shader_stage.min(8);
                l.max_storage_textures_per_shader_stage = l.max_storage_textures_per_shader_stage.min(4);
                l.max_uniform_buffers_per_shader_stage = l.max_uniform_buffers_per_shader_stage.min(12);
                l.max_uniform_buffer_binding_size = l.max_uniform_buffer_binding_size.min(16 << 10);
                l.max_storage_buffer_binding_size = l.max_storage_buffer_binding_size.min(128 << 20);
                l.max_buffer_size = l.max_buffer_size.min(256 << 20);
                l.max_vertex_buffers = l.max_vertex_buffers.min(8);
                l.max_vertex_attributes = l.max_vertex_attributes.min(16);
                l.max_inter_stage_shader_variables = l.max_inter_stage_shader_variables.min(15);
                l.max_color_attachments = l.max_color_attachments.min(4);
                l.max_compute_workgroup_storage_size = l.max_compute_workgroup_storage_size.min(16 << 10);
                l.max_compute_invocations_per_workgroup = l.max_compute_invocations_per_workgroup.min(256);
                l.max_compute_workgroup_size_x = l.max_compute_workgroup_size_x.min(256);
                l.max_compute_workgroup_size_y = l.max_compute_workgroup_size_y.min(256);
                l.max_compute_workgroup_size_z = l.max_compute_workgroup_size_z.min(64);
                l.max_compute_workgroups_per_dimension = l.max_compute_workgroups_per_dimension.min(65535);
                l
            }
        }
    }
}

/// Owned explanations of a minimum-spec profile's feature mask and limit caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinspecExplain {
    /// Optional wgpu features excluded by this profile.
    pub features: Vec<String>,
    /// Limit fields this profile caps, with the ceiling [`MinspecProfile::clamp_limits`] applies to a
    /// wide adapter (one reporting the largest value of every field). A ceiling may be above
    /// `Limits::default()` (Pascal's 2D texture size is 32768); a field the profile leaves
    /// unconstrained is not listed.
    pub limits: Vec<String>,
}

fn wide_limits() -> Limits {
    Limits {
        max_texture_dimension_1d: u32::MAX,
        max_texture_dimension_2d: u32::MAX,
        max_texture_dimension_3d: u32::MAX,
        max_texture_array_layers: u32::MAX,
        max_bind_groups: u32::MAX,
        max_bind_groups_plus_vertex_buffers: u32::MAX,
        max_bindings_per_bind_group: u32::MAX,
        max_dynamic_uniform_buffers_per_pipeline_layout: u32::MAX,
        max_dynamic_storage_buffers_per_pipeline_layout: u32::MAX,
        max_sampled_textures_per_shader_stage: u32::MAX,
        max_samplers_per_shader_stage: u32::MAX,
        max_storage_buffers_per_shader_stage: u32::MAX,
        max_storage_textures_per_shader_stage: u32::MAX,
        max_uniform_buffers_per_shader_stage: u32::MAX,
        max_binding_array_elements_per_shader_stage: u32::MAX,
        max_binding_array_acceleration_structure_elements_per_shader_stage: u32::MAX,
        max_binding_array_sampler_elements_per_shader_stage: u32::MAX,
        max_uniform_buffer_binding_size: u64::MAX,
        max_storage_buffer_binding_size: u64::MAX,
        max_vertex_buffers: u32::MAX,
        max_buffer_size: u64::MAX,
        max_vertex_attributes: u32::MAX,
        max_vertex_buffer_array_stride: u32::MAX,
        max_inter_stage_shader_variables: u32::MAX,
        max_color_attachments: u32::MAX,
        max_color_attachment_bytes_per_sample: u32::MAX,
        max_compute_workgroup_storage_size: u32::MAX,
        max_compute_invocations_per_workgroup: u32::MAX,
        max_compute_workgroup_size_x: u32::MAX,
        max_compute_workgroup_size_y: u32::MAX,
        max_compute_workgroup_size_z: u32::MAX,
        max_compute_workgroups_per_dimension: u32::MAX,
        max_immediate_size: u32::MAX,
        max_non_sampler_bindings: u32::MAX,
        max_task_workgroup_total_count: u32::MAX,
        max_task_workgroups_per_dimension: u32::MAX,
        max_mesh_workgroup_total_count: u32::MAX,
        max_mesh_workgroups_per_dimension: u32::MAX,
        max_task_invocations_per_workgroup: u32::MAX,
        max_task_invocations_per_dimension: u32::MAX,
        max_mesh_invocations_per_workgroup: u32::MAX,
        max_mesh_invocations_per_dimension: u32::MAX,
        max_task_payload_size: u32::MAX,
        max_mesh_output_vertices: u32::MAX,
        max_mesh_output_primitives: u32::MAX,
        max_mesh_output_layers: u32::MAX,
        max_mesh_multiview_view_count: u32::MAX,
        max_blas_primitive_count: u32::MAX,
        max_blas_geometry_count: u32::MAX,
        max_tlas_instance_count: u32::MAX,
        max_acceleration_structures_per_shader_stage: u32::MAX,
        max_buffers_and_acceleration_structures_per_shader_stage: u32::MAX,
        max_multiview_view_count: u32::MAX,
        max_ray_dispatch_count: u32::MAX,
        max_ray_recursion_depth: u32::MAX,
        ..Limits::default()
    }
}

fn limit_fields(defaults: &Limits, wide: &Limits) -> Vec<(&'static str, u64, u64)> {
    vec![
        (
            "max_texture_dimension_1d",
            defaults.max_texture_dimension_1d.into(),
            wide.max_texture_dimension_1d.into(),
        ),
        (
            "max_texture_dimension_2d",
            defaults.max_texture_dimension_2d.into(),
            wide.max_texture_dimension_2d.into(),
        ),
        (
            "max_texture_dimension_3d",
            defaults.max_texture_dimension_3d.into(),
            wide.max_texture_dimension_3d.into(),
        ),
        (
            "max_texture_array_layers",
            defaults.max_texture_array_layers.into(),
            wide.max_texture_array_layers.into(),
        ),
        (
            "max_bind_groups",
            defaults.max_bind_groups.into(),
            wide.max_bind_groups.into(),
        ),
        (
            "max_sampled_textures_per_shader_stage",
            defaults.max_sampled_textures_per_shader_stage.into(),
            wide.max_sampled_textures_per_shader_stage.into(),
        ),
        (
            "max_samplers_per_shader_stage",
            defaults.max_samplers_per_shader_stage.into(),
            wide.max_samplers_per_shader_stage.into(),
        ),
        (
            "max_storage_buffers_per_shader_stage",
            defaults.max_storage_buffers_per_shader_stage.into(),
            wide.max_storage_buffers_per_shader_stage.into(),
        ),
        (
            "max_storage_textures_per_shader_stage",
            defaults.max_storage_textures_per_shader_stage.into(),
            wide.max_storage_textures_per_shader_stage.into(),
        ),
        (
            "max_uniform_buffers_per_shader_stage",
            defaults.max_uniform_buffers_per_shader_stage.into(),
            wide.max_uniform_buffers_per_shader_stage.into(),
        ),
        (
            "max_uniform_buffer_binding_size",
            defaults.max_uniform_buffer_binding_size,
            wide.max_uniform_buffer_binding_size,
        ),
        (
            "max_storage_buffer_binding_size",
            defaults.max_storage_buffer_binding_size,
            wide.max_storage_buffer_binding_size,
        ),
        ("max_buffer_size", defaults.max_buffer_size, wide.max_buffer_size),
        (
            "max_compute_workgroup_storage_size",
            defaults.max_compute_workgroup_storage_size.into(),
            wide.max_compute_workgroup_storage_size.into(),
        ),
        (
            "max_compute_invocations_per_workgroup",
            defaults.max_compute_invocations_per_workgroup.into(),
            wide.max_compute_invocations_per_workgroup.into(),
        ),
        (
            "max_compute_workgroup_size_x",
            defaults.max_compute_workgroup_size_x.into(),
            wide.max_compute_workgroup_size_x.into(),
        ),
        (
            "max_compute_workgroup_size_y",
            defaults.max_compute_workgroup_size_y.into(),
            wide.max_compute_workgroup_size_y.into(),
        ),
        (
            "max_compute_workgroup_size_z",
            defaults.max_compute_workgroup_size_z.into(),
            wide.max_compute_workgroup_size_z.into(),
        ),
        (
            "max_compute_workgroups_per_dimension",
            defaults.max_compute_workgroups_per_dimension.into(),
            wide.max_compute_workgroups_per_dimension.into(),
        ),
        (
            "max_vertex_buffers",
            defaults.max_vertex_buffers.into(),
            wide.max_vertex_buffers.into(),
        ),
        (
            "max_vertex_attributes",
            defaults.max_vertex_attributes.into(),
            wide.max_vertex_attributes.into(),
        ),
        (
            "max_color_attachments",
            defaults.max_color_attachments.into(),
            wide.max_color_attachments.into(),
        ),
        (
            "max_inter_stage_shader_variables",
            defaults.max_inter_stage_shader_variables.into(),
            wide.max_inter_stage_shader_variables.into(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamping_never_raises_a_limit() {
        let base = Limits::default();
        for profile in [MinspecProfile::Pascal, MinspecProfile::Mobile] {
            let clamped = profile.clamp_limits(&base);
            assert!(
                clamped.max_bind_groups <= base.max_bind_groups,
                "{}",
                profile.name()
            );
            assert!(
                clamped.max_storage_buffers_per_shader_stage <= base.max_storage_buffers_per_shader_stage,
                "{}",
                profile.name()
            );
            assert!(
                clamped.max_compute_workgroup_storage_size <= base.max_compute_workgroup_storage_size,
                "{}",
                profile.name()
            );
            assert!(
                clamped.max_buffer_size <= base.max_buffer_size,
                "{}",
                profile.name()
            );
            assert!(
                clamped.max_texture_dimension_2d <= base.max_texture_dimension_2d,
                "{}",
                profile.name()
            );
        }
    }

    #[test]
    fn pascal_keeps_the_engine_largest_workgroup() {
        let clamped = MinspecProfile::Pascal.clamp_limits(&Limits::default());
        assert!(clamped.max_compute_invocations_per_workgroup >= 256);
        assert!(clamped.max_compute_workgroup_storage_size >= 16 << 10);
    }

    #[test]
    fn mobile_masks_off_what_the_tier_lacks() {
        let mask = MinspecProfile::Mobile.feature_mask();
        assert!(!mask.contains(Features::TIMESTAMP_QUERY));
        assert!(!mask.contains(Features::TEXTURE_COMPRESSION_BC));
        assert!(!mask.contains(Features::MULTI_DRAW_INDIRECT_COUNT));
        assert!(!mask.contains(Features::RG11B10UFLOAT_RENDERABLE));
        let pascal = MinspecProfile::Pascal.feature_mask();
        assert!(pascal.contains(Features::TEXTURE_COMPRESSION_BC));
        assert!(pascal.contains(Features::TIMESTAMP_QUERY_INSIDE_PASSES));
        assert!(!pascal.contains(Features::TEXTURE_COMPRESSION_ASTC));
    }

    #[test]
    fn explanations_project_the_masks_and_clamped_caps() {
        for profile in [MinspecProfile::Pascal, MinspecProfile::Mobile] {
            let explanation = profile.explain();
            let mask = profile.feature_mask();
            for (name, feature) in Features::all().iter_names() {
                let text = format!("feature {name} is off");
                assert_eq!(
                    explanation.features.contains(&text),
                    !mask.contains(feature),
                    "{}: {text}",
                    profile.name()
                );
            }

            let input = wide_limits();
            let defaults = profile.clamp_limits(&Limits::default());
            let wide = profile.clamp_limits(&input);
            let expected: Vec<_> = limit_fields(&defaults, &wide)
                .into_iter()
                .zip(limit_fields(&Limits::default(), &input))
                .filter(|((_, _, cap), (_, _, ceiling))| cap < ceiling)
                .map(|((field, _, cap), _)| format!("limit {field} is capped at {cap}"))
                .collect();
            assert_eq!(explanation.limits, expected, "{}", profile.name());
        }

        // Pascal quotes its real ceilings, and nothing for the fields it leaves unconstrained.
        let pascal = MinspecProfile::Pascal.explain();
        assert!(
            pascal
                .limits
                .contains(&"limit max_texture_dimension_2d is capped at 32768".to_string()),
            "{:?}",
            pascal.limits
        );
        assert!(
            !pascal.limits.iter().any(|l| l.contains("max_vertex_buffers")),
            "{:?}",
            pascal.limits
        );
        assert!(
            !pascal.limits.iter().any(|l| l.ends_with(&u32::MAX.to_string())),
            "{:?}",
            pascal.limits
        );
        assert_ne!(
            MinspecProfile::Pascal.explain().features,
            MinspecProfile::Mobile.explain().features
        );
        assert_ne!(
            MinspecProfile::Pascal.explain().limits,
            MinspecProfile::Mobile.explain().limits
        );
    }
}
