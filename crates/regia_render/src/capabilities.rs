// SPDX-License-Identifier: MPL-2.0
//! Adapter capabilities obtained at device creation (`specs/13` § 2).

use crate::host_checklist::{
    FrameProblem, PROBLEM_FALLBACK_CPU_INDIRECT_COUNT, PROBLEM_FALLBACK_INDIRECT_FIRST_INSTANCE,
    PROBLEM_FALLBACK_NO_BIND_GROUPS, PROBLEM_FALLBACK_NO_BLOCK_COMPRESSION,
    PROBLEM_FALLBACK_NO_EARLY_DEPTH_TEST, PROBLEM_FALLBACK_NO_FLOAT32_FILTER, PROBLEM_FALLBACK_NO_TIMESTAMPS,
    PROBLEM_FALLBACK_PER_MIP_DEPTH_PYRAMID, PROBLEM_FALLBACK_RGBA16F_HDR,
    PROBLEM_FALLBACK_SUN_SHADOW_PCF_ONLY, PROBLEM_FALLBACK_VISBUFFER_STORAGE,
};

/// Bind group slots the shared terrain draw uses (`terrain.wgsl` / `regia_terrain`): group 0
/// is the frame, 1 the terrain, 2 the node grid, 3 the engine CSM/sun map, and 4 the virtual
/// texture (colour) or the cascade view-projection (shadow). A device must expose at least 5
/// bind groups or the terrain virtual texture and self-shadow cascade are turned off.
pub const TERRAIN_SHARED_BIND_GROUPS: u32 = 5;

/// D3D12 Resource Binding Tier of a DX12 adapter, as far as wgpu exposes it.
///
/// wgpu does not pass the tier through, but its DX12 backend (wgpu-hal 30, `dx12/adapter.rs`)
/// derives the per-stage limits from it, following Microsoft's "Hardware support" table for
/// resource binding tiers (<https://learn.microsoft.com/en-us/windows/win32/direct3d12/hardware-support>):
///
/// | Per stage (wgpu limit)              | Tier 1      | Tier 2    | Tier 3    |
/// |-------------------------------------|-------------|-----------|-----------|
/// | CBVs (`max_uniform_buffers_…`)      | 14          | 14        | full heap |
/// | SRVs (`max_sampled_textures_…`)     | 128         | full heap | full heap |
/// | UAVs, all stages (storage tex + buf)| 8 or 64     | 64        | full heap |
/// | Samplers (`max_samplers_…`)         | 16          | 2048      | 2048      |
///
/// wgpu-hal splits the UAV count over two stages and then evenly between storage textures and
/// storage buffers, so a Tier 2 adapter reports 16 + 16 per stage. [`ResourceBindingTier::from_dx12_limits`]
/// reads the tier back from those limits. wgpu never exposes a Tier 1 adapter (it needs Tier 2 for
/// bindless samplers), so `Tier1` only appears for limits clamped below Tier 2, and wgpu turns on
/// the binding-array features (`TEXTURE_BINDING_ARRAY`) exactly for Tier 3 with Shader Model 5.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceBindingTier {
    /// Tier 1: 128 SRVs and 16 samplers per stage. wgpu does not expose such an adapter; this is
    /// what limits clamped to the WebGPU defaults (or a min-spec profile) read as.
    Tier1,
    /// Tier 2: no binding arrays (per-material bind groups only).
    Tier2,
    /// Tier 3 with Shader Model 5.1: bindless resource arrays are available.
    Tier3,
}

/// Constant buffer views per stage at Tier 1 and Tier 2 (Microsoft's table); Tier 3 is the full heap.
const DX12_TIER2_MAX_CBVS_PER_STAGE: u32 = 14;
/// UAVs across all stages at Tier 2; wgpu-hal reports half of it per stage as storage textures
/// plus storage buffers.
const DX12_TIER2_MAX_UAVS: u32 = 64;
/// SRVs per stage at Tier 1 (wgpu-hal subtracts one per bind group for its sampler table).
const DX12_TIER1_MAX_SRVS_PER_STAGE: u32 = 128;
/// Samplers per stage at Tier 1; Tier 2 and 3 allow 2048.
const DX12_TIER1_MAX_SAMPLERS_PER_STAGE: u32 = 16;

impl ResourceBindingTier {
    /// The tier of an adapter with `backend` and `features`; `None` off DX12.
    ///
    /// From the binding-array feature alone, so it cannot tell Tier 1 from Tier 2;
    /// [`Self::for_adapter`] also reads the limits.
    #[must_use]
    pub fn from_features(backend: wgpu::Backend, features: wgpu::Features) -> Option<Self> {
        if !crate::backend_policy::has_resource_binding_tier(backend) {
            return None;
        }
        Some(if features.contains(wgpu::Features::TEXTURE_BINDING_ARRAY) {
            Self::Tier3
        } else {
            Self::Tier2
        })
    }

    /// The tier a DX12 adapter's wgpu `limits` describe (the table on [`ResourceBindingTier`]).
    ///
    /// Pure and total. Tier 1 when samplers or sampled textures per stage are at Tier 1's caps;
    /// Tier 3 only when both the constant buffers per stage and the storage textures plus storage
    /// buffers per stage are above Tier 2's caps; Tier 2 otherwise. Limits lowered by a host or a
    /// min-spec profile read as the lower tier, never a higher one. The device type plays no
    /// part: a software adapter (WARP) maps by its limits like any other.
    #[must_use]
    pub fn from_dx12_limits(limits: &wgpu::Limits) -> Self {
        if limits.max_samplers_per_shader_stage <= DX12_TIER1_MAX_SAMPLERS_PER_STAGE
            || limits.max_sampled_textures_per_shader_stage <= DX12_TIER1_MAX_SRVS_PER_STAGE
        {
            return Self::Tier1;
        }
        let uavs_per_stage = limits
            .max_storage_textures_per_shader_stage
            .saturating_add(limits.max_storage_buffers_per_shader_stage);
        if limits.max_uniform_buffers_per_shader_stage > DX12_TIER2_MAX_CBVS_PER_STAGE
            && uavs_per_stage > DX12_TIER2_MAX_UAVS / 2
        {
            Self::Tier3
        } else {
            Self::Tier2
        }
    }

    /// The tier of an adapter with `backend`, `limits` and `features`; `None` off DX12
    /// ([`crate::backend_policy::has_resource_binding_tier`]). The higher of
    /// [`Self::from_dx12_limits`] and the binding-array feature: wgpu enables
    /// `TEXTURE_BINDING_ARRAY` only on Tier 3, so the feature wins over limits a host clamped.
    #[must_use]
    pub fn for_adapter(
        backend: wgpu::Backend,
        limits: &wgpu::Limits,
        features: wgpu::Features,
    ) -> Option<Self> {
        if !crate::backend_policy::has_resource_binding_tier(backend) {
            return None;
        }
        Some(if features.contains(wgpu::Features::TEXTURE_BINDING_ARRAY) {
            Self::Tier3
        } else {
            Self::from_dx12_limits(limits)
        })
    }

    /// Whether this tier allows bindless resource arrays (Tier 3). On DX12 a bindless path needs
    /// this and wgpu's binding-array features.
    #[must_use]
    pub fn supports_binding_arrays(self) -> bool {
        self >= Self::Tier3
    }
}

impl std::fmt::Display for ResourceBindingTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Tier1 => "Tier 1 (limits below Tier 2; wgpu does not expose it)",
            Self::Tier2 => "Tier 2 (no binding arrays)",
            Self::Tier3 => "Tier 3 (binding arrays)",
        })
    }
}

/// Features and limits the renderer relies on.
#[derive(Debug, Clone)]
pub struct RenderCapabilities {
    /// Adapter name.
    pub adapter_name: String,
    /// Backend in use.
    pub backend: wgpu::Backend,
    /// Timestamp queries available between passes (`TIMESTAMP_QUERY` and `TIMESTAMP_QUERY_INSIDE_ENCODERS`).
    pub timestamp_query: bool,
    /// `MULTI_DRAW_INDIRECT` (one call, many indirect commands).
    pub multi_draw_indirect: bool,
    /// `MULTI_DRAW_INDIRECT_COUNT` (the GPU writes how many commands to draw).
    pub multi_draw_indirect_count: bool,
    /// Non-zero `first_instance` in an indirect command is honoured.
    pub indirect_first_instance: bool,
    /// `RG11B10UFLOAT` can be a colour attachment (mobile HDR target).
    pub rg11b10_renderable: bool,
    /// BC texture compression.
    pub texture_compression_bc: bool,
    /// ASTC texture compression.
    pub texture_compression_astc: bool,
    /// ETC2 texture compression.
    pub texture_compression_etc2: bool,
    /// `@early_depth_test(force)` keeps early-Z in a fragment stage with storage writes
    /// (the terrain virtual texture's feedback pass).
    pub early_depth_test: bool,
    /// 32-bit float textures can be filtered (the terrain height pyramid).
    pub float32_filterable: bool,
    /// Maximum storage buffer binding size.
    pub max_storage_buffer_binding_size: u64,
    /// Maximum storage buffers bound in one shader stage.
    pub max_storage_buffers_per_shader_stage: u32,
    /// Maximum storage textures bound in one shader stage.
    pub max_storage_textures_per_shader_stage: u32,
    /// Maximum uniform buffers bound in one shader stage.
    pub max_uniform_buffers_per_shader_stage: u32,
    /// Maximum workgroup shared memory (`var<workgroup>`) bytes.
    pub max_compute_workgroup_storage_size: u32,
    /// Maximum bind group slots a pipeline layout may use.
    pub max_bind_groups: u32,
    /// Maximum 2D texture side.
    pub max_texture_dimension_2d: u32,
}

impl RenderCapabilities {
    /// Reads capabilities from an adapter after the device is created.
    #[must_use]
    pub fn from_adapter(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Self {
        let info = adapter.get_info();
        let features = device.features();
        let limits = device.limits();
        Self {
            adapter_name: info.name,
            backend: info.backend,
            // The frame timer writes between passes (`encoder.write_timestamp`), which needs the
            // encoder feature too. Metal has pass-boundary timestamps only, so no frame timer there.
            timestamp_query: features.contains(wgpu::Features::TIMESTAMP_QUERY)
                && features.contains(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS),
            // Indexed indirect draws need `INDIRECT_EXECUTION`. The count buffer is a
            // separate feature; without it the GPU path uses a CPU command count
            // (`gpu_driven::IndirectStrategy`).
            multi_draw_indirect: adapter
                .get_downlevel_capabilities()
                .flags
                .contains(wgpu::DownlevelFlags::INDIRECT_EXECUTION),
            multi_draw_indirect_count: features.contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT),
            indirect_first_instance: features.contains(wgpu::Features::INDIRECT_FIRST_INSTANCE),
            rg11b10_renderable: features.contains(wgpu::Features::RG11B10UFLOAT_RENDERABLE),
            texture_compression_bc: features.contains(wgpu::Features::TEXTURE_COMPRESSION_BC),
            texture_compression_astc: features.contains(wgpu::Features::TEXTURE_COMPRESSION_ASTC),
            texture_compression_etc2: features.contains(wgpu::Features::TEXTURE_COMPRESSION_ETC2),
            early_depth_test: features.contains(wgpu::Features::SHADER_EARLY_DEPTH_TEST),
            float32_filterable: features.contains(wgpu::Features::FLOAT32_FILTERABLE),
            max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
            max_storage_buffers_per_shader_stage: limits.max_storage_buffers_per_shader_stage,
            max_storage_textures_per_shader_stage: limits.max_storage_textures_per_shader_stage,
            max_uniform_buffers_per_shader_stage: limits.max_uniform_buffers_per_shader_stage,
            max_compute_workgroup_storage_size: limits.max_compute_workgroup_storage_size,
            max_bind_groups: limits.max_bind_groups,
            max_texture_dimension_2d: limits.max_texture_dimension_2d,
        }
    }

    /// Every capability fallback active on this device: an optional feature the adapter
    /// lacks (or a limit below what the fast path wants) and the working fallback the
    /// renderer runs instead, as [`FrameProblem`]s so `render.problems` and the host
    /// checklist report them (`specs/13` § 2, min-spec auditing).
    ///
    /// These are differences, not errors: every fallback draws a correct frame, at a cost
    /// the message names. The list is a function of the device only, so it is stable for a
    /// given device (recorded once per [`crate::FrameRenderer`], not re-derived per frame).
    #[must_use]
    pub fn fallback_notes(&self) -> Vec<FrameProblem> {
        let mut out = Vec::new();
        if !self.timestamp_query {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_NO_TIMESTAMPS,
                message: "the adapter has no GPU timestamp queries: per-pass GPU timings are \
                          off (CPU frame timings and the hitch counters are unaffected)"
                    .to_string(),
            });
        }
        if !self.texture_compression_bc {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_NO_BLOCK_COMPRESSION,
                message: "the adapter has no BC decompression: every BC-compressed (cooked) \
                          map is CPU-decoded to RGBA8 on upload — 4× the VRAM and upload \
                          bandwidth of the compressed form (ASTC maps, if any, are unaffected)"
                    .to_string(),
            });
        }
        if !self.multi_draw_indirect_count {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_CPU_INDIRECT_COUNT,
                message: "the adapter has no indirect-draw count buffers \
                          (MULTI_DRAW_INDIRECT_COUNT): the GPU-driven cull cannot compact the \
                          visible object slots into a count, so each (mesh, material) batch \
                          draws as a fixed-size instanced draw instead of only its visible \
                          instances (the count is never read back to the CPU)"
                    .to_string(),
            });
        }
        if !self.indirect_first_instance {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_INDIRECT_FIRST_INSTANCE,
                message: "the adapter ignores a non-zero first_instance in indirect draws: \
                          every batch starts at instance 0 (one indirect draw per object \
                          batch, no instanced replay)"
                    .to_string(),
            });
        }
        if !self.rg11b10_renderable {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_RGBA16F_HDR,
                message: "the adapter cannot render to RG11B10UFLOAT: the mobile HDR colour \
                          target falls back to RGBA16F (2× the colour-target bandwidth)"
                    .to_string(),
            });
        }
        if crate::shader_workarounds::sun_shadow_pcf_only() {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_SUN_SHADOW_PCF_ONLY,
                message: "the driver cannot compile the full forward shader (Adreno, ADR-1749): \
                          the sun shadow is PCF on the cascades only, with no near tile and no \
                          PCSS soft shadows; the image light's ground plane, local reflection \
                          captures and image-light debug views are off (ADR-2042); fitted emitters and \
                          LTC sphere lights send no light, surfaces stay dry under weather and a \
                          pane behind the nearest one keeps its environment reflection (ADR-3145)"
                    .to_string(),
            });
        }
        if !self.float32_filterable {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_NO_FLOAT32_FILTER,
                message: "the adapter cannot filter 32-bit float textures: the terrain \
                          budget's hardware-filtered height normals fall back to unfiltered \
                          (sampled) taps"
                    .to_string(),
            });
        }
        if !self.early_depth_test {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_NO_EARLY_DEPTH_TEST,
                message: "the adapter has no forced early depth tests: the terrain virtual \
                          texture's feedback pass shades every covered pixel in full (its \
                          early-Z cull is off)"
                    .to_string(),
            });
        }
        if self.max_storage_textures_per_shader_stage < crate::depth_pyramid::SPD_STORAGE_TEXTURES {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_PER_MIP_DEPTH_PYRAMID,
                message: format!(
                    "the adapter binds only {} storage textures per stage (the single-pass \
                     downsample wants {}): the Hi-Z/SSSR pyramids build one dispatch per mip",
                    self.max_storage_textures_per_shader_stage,
                    crate::depth_pyramid::SPD_STORAGE_TEXTURES
                ),
            });
        }
        if self.max_storage_buffers_per_shader_stage < crate::visbuffer::RESOLVE_STORAGE_BUFFERS {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_VISBUFFER_STORAGE,
                message: format!(
                    "the adapter binds only {} storage buffers per stage (the vis-buffer \
                     resolve wants {}): virtual-geometry meshes stay on the forward path",
                    self.max_storage_buffers_per_shader_stage,
                    crate::visbuffer::RESOLVE_STORAGE_BUFFERS
                ),
            });
        }
        if self.max_bind_groups < TERRAIN_SHARED_BIND_GROUPS {
            out.push(FrameProblem {
                code: PROBLEM_FALLBACK_NO_BIND_GROUPS,
                message: format!(
                    "the adapter exposes only {} bind groups (the shared terrain draw wants \
                     {}: group 4 is the virtual-texture colour atlas / cascade shadow \
                     view-projection): the terrain virtual texture is off and the terrain \
                     does not cast its self-shadow cascade",
                    self.max_bind_groups, TERRAIN_SHARED_BIND_GROUPS
                ),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(bc: bool, astc: bool, stamps: bool, storage_tex: u32) -> RenderCapabilities {
        RenderCapabilities {
            adapter_name: "test".to_string(),
            backend: wgpu::Backend::Vulkan,
            timestamp_query: stamps,
            multi_draw_indirect: true,
            multi_draw_indirect_count: true,
            indirect_first_instance: true,
            rg11b10_renderable: true,
            texture_compression_bc: bc,
            texture_compression_astc: astc,
            texture_compression_etc2: !bc,
            early_depth_test: true,
            float32_filterable: true,
            max_storage_buffer_binding_size: 128 << 20,
            max_storage_buffers_per_shader_stage: 16,
            max_storage_textures_per_shader_stage: storage_tex,
            max_uniform_buffers_per_shader_stage: 12,
            max_compute_workgroup_storage_size: 32 << 10,
            max_bind_groups: 8,
            max_texture_dimension_2d: 8192,
        }
    }

    #[test]
    fn a_full_feature_set_has_no_fallback_notes() {
        assert!(caps(true, false, true, 8).fallback_notes().is_empty());
    }

    #[test]
    fn every_missing_capability_names_its_fallback() {
        let notes = caps(false, false, false, 2).fallback_notes();
        let codes: Vec<_> = notes.iter().map(|n| n.code).collect();
        assert_eq!(
            codes,
            [
                PROBLEM_FALLBACK_NO_TIMESTAMPS,
                PROBLEM_FALLBACK_NO_BLOCK_COMPRESSION,
                PROBLEM_FALLBACK_PER_MIP_DEPTH_PYRAMID,
            ]
        );
        for note in &notes {
            assert!(!note.message.is_empty());
        }
    }

    #[test]
    fn astc_without_bc_still_reports_the_block_compression_fallback() {
        // BC absent but ASTC present: a mobile device samples ASTC maps natively, but the
        // cooked desktop maps are still BC-compressed, so they CPU-decode to RGBA8.
        let notes = caps(false, true, true, 8).fallback_notes();
        let codes: Vec<_> = notes.iter().map(|n| n.code).collect();
        assert_eq!(codes, [PROBLEM_FALLBACK_NO_BLOCK_COMPRESSION]);
    }

    #[test]
    fn fewer_than_five_bind_groups_names_the_terrain_fallback() {
        let mut c = caps(true, false, true, 8);
        c.max_bind_groups = 4;
        let notes = c.fallback_notes();
        let codes: Vec<_> = notes.iter().map(|n| n.code).collect();
        assert_eq!(codes, [PROBLEM_FALLBACK_NO_BIND_GROUPS]);
    }

    #[test]
    fn binding_tier_comes_from_the_dx12_binding_array_feature() {
        use wgpu::{Backend, Features};
        assert_eq!(
            ResourceBindingTier::from_features(Backend::Vulkan, Features::all()),
            None
        );
        assert_eq!(
            ResourceBindingTier::from_features(Backend::Dx12, Features::empty()),
            Some(ResourceBindingTier::Tier2)
        );
        assert_eq!(
            ResourceBindingTier::from_features(Backend::Dx12, Features::TEXTURE_BINDING_ARRAY),
            Some(ResourceBindingTier::Tier3)
        );
    }

    /// wgpu-hal 30's `cap_limits_to_be_under_the_sum_limit`: the smallest limits keep their
    /// value, the rest share what is left of `sum` evenly.
    fn cap_under_sum(values: &mut [u32], sum: u32) {
        let mut order: Vec<usize> = (0..values.len()).collect();
        order.sort_by_key(|&i| values[i]);
        let mut rest = sum;
        let mut left = u32::try_from(values.len()).unwrap_or(u32::MAX);
        for i in order {
            values[i] = values[i].min(rest / left);
            rest -= values[i];
            left -= 1;
        }
    }

    /// The per-stage limits wgpu-hal 30's DX12 adapter reports for a Resource Binding `tier`
    /// (`dx12/adapter.rs`, from Microsoft's hardware-support table): `fl_11_0` is a feature level
    /// 11.0 device (Tier 1 has 8 UAVs there), `ray_tracing` shares the SRVs with acceleration
    /// structures. Everything else is the WebGPU default.
    fn hal_dx12_limits(tier: u8, fl_11_0: bool, ray_tracing: bool) -> wgpu::Limits {
        const BIND_GROUPS: u32 = 8;
        let full_heap: u32 = if tier >= 3 { 1 << 20 } else { 1_000_000 };
        let cbvs = if tier >= 3 { full_heap } else { 14 };
        let srvs = if tier == 1 {
            128 - BIND_GROUPS
        } else {
            full_heap - BIND_GROUPS
        };
        let uavs = match tier {
            1 if fl_11_0 => 8,
            1 | 2 => 64,
            _ => full_heap,
        };
        let uavs_per_stage = uavs / 2;
        let storage_textures = uavs_per_stage / 2;
        // [sampled textures, acceleration structures, storage buffers] share the SRVs.
        let mut shared = if ray_tracing {
            [srvs / 2, srvs / 2, uavs_per_stage / 2]
        } else {
            [srvs, 0, uavs_per_stage / 2]
        };
        cap_under_sum(&mut shared, srvs);
        wgpu::Limits {
            max_sampled_textures_per_shader_stage: shared[0],
            max_storage_buffers_per_shader_stage: shared[2],
            max_storage_textures_per_shader_stage: storage_textures,
            max_uniform_buffers_per_shader_stage: cbvs,
            max_samplers_per_shader_stage: if tier == 1 { 16 } else { 2048 },
            ..wgpu::Limits::default()
        }
    }

    #[test]
    fn the_hal_model_gives_the_documented_tier_two_and_three_numbers() {
        // Tier 2: 14 CBVs, 64 UAVs over two stages (16 + 16), 2048 samplers.
        let t2 = hal_dx12_limits(2, false, false);
        assert_eq!(t2.max_uniform_buffers_per_shader_stage, 14);
        assert_eq!(t2.max_storage_textures_per_shader_stage, 16);
        assert_eq!(t2.max_storage_buffers_per_shader_stage, 16);
        assert_eq!(t2.max_sampled_textures_per_shader_stage, 999_976);
        assert_eq!(t2.max_samplers_per_shader_stage, 2048);
        // Tier 3: the 2^20 heap everywhere, split the same way.
        let t3 = hal_dx12_limits(3, false, false);
        assert_eq!(t3.max_uniform_buffers_per_shader_stage, 1 << 20);
        assert_eq!(t3.max_storage_textures_per_shader_stage, 1 << 18);
        assert_eq!(t3.max_storage_buffers_per_shader_stage, 1 << 18);
        assert_eq!(t3.max_sampled_textures_per_shader_stage, 786_424);
    }

    #[test]
    fn dx12_limits_map_to_their_binding_tier() {
        use ResourceBindingTier::{Tier1, Tier2, Tier3};
        for fl_11_0 in [false, true] {
            assert_eq!(
                ResourceBindingTier::from_dx12_limits(&hal_dx12_limits(1, fl_11_0, false)),
                Tier1,
                "tier 1 (feature level 11.0: {fl_11_0})"
            );
        }
        for ray_tracing in [false, true] {
            assert_eq!(
                ResourceBindingTier::from_dx12_limits(&hal_dx12_limits(2, false, ray_tracing)),
                Tier2,
                "tier 2 (ray tracing: {ray_tracing})"
            );
            assert_eq!(
                ResourceBindingTier::from_dx12_limits(&hal_dx12_limits(3, false, ray_tracing)),
                Tier3,
                "tier 3 (ray tracing: {ray_tracing})"
            );
        }
    }

    #[test]
    fn a_warp_like_software_adapter_maps_by_its_limits() {
        use wgpu::{Backend, Features};
        // WARP (the D3D12 software rasteriser, a `Cpu` device type in wgpu) goes through the same
        // wgpu-hal code: its tier comes from its limits, whichever tier it reports.
        let warp_t3 = hal_dx12_limits(3, false, false);
        assert_eq!(
            ResourceBindingTier::for_adapter(Backend::Dx12, &warp_t3, Features::empty()),
            Some(ResourceBindingTier::Tier3)
        );
        let warp_t2 = hal_dx12_limits(2, false, false);
        assert_eq!(
            ResourceBindingTier::for_adapter(Backend::Dx12, &warp_t2, Features::empty()),
            Some(ResourceBindingTier::Tier2)
        );
        // The binding-array feature means Tier 3 even when a host clamped the limits.
        assert_eq!(
            ResourceBindingTier::for_adapter(
                Backend::Dx12,
                &wgpu::Limits::default(),
                Features::TEXTURE_BINDING_ARRAY
            ),
            Some(ResourceBindingTier::Tier3)
        );
    }

    #[test]
    fn clamped_limits_never_read_as_a_higher_tier() {
        use ResourceBindingTier::{Tier1, Tier2};
        // The WebGPU defaults (16 samplers, 16 sampled textures) promise nothing above Tier 1.
        assert_eq!(
            ResourceBindingTier::from_dx12_limits(&wgpu::Limits::default()),
            Tier1
        );
        // A Tier 3 adapter with its storage limits clamped to Tier 2's is read as Tier 2.
        let mut clamped = hal_dx12_limits(3, false, false);
        clamped.max_storage_textures_per_shader_stage = 16;
        clamped.max_storage_buffers_per_shader_stage = 16;
        assert_eq!(ResourceBindingTier::from_dx12_limits(&clamped), Tier2);
        // So is one with its constant buffers clamped to 14.
        let mut cbvs = hal_dx12_limits(3, false, false);
        cbvs.max_uniform_buffers_per_shader_stage = 14;
        assert_eq!(ResourceBindingTier::from_dx12_limits(&cbvs), Tier2);
        // u32::MAX storage limits do not overflow.
        let mut huge = hal_dx12_limits(3, false, false);
        huge.max_storage_textures_per_shader_stage = u32::MAX;
        huge.max_storage_buffers_per_shader_stage = u32::MAX;
        assert_eq!(
            ResourceBindingTier::from_dx12_limits(&huge),
            ResourceBindingTier::Tier3
        );
    }

    #[test]
    fn the_tier_is_dx12_only_and_tier_three_alone_allows_arrays() {
        use wgpu::{Backend, Features, Limits};
        let t3 = hal_dx12_limits(3, false, false);
        for backend in [Backend::Vulkan, Backend::Metal, Backend::Gl] {
            assert_eq!(
                ResourceBindingTier::for_adapter(backend, &t3, Features::all()),
                None,
                "{backend:?}"
            );
        }
        assert!(ResourceBindingTier::Tier3.supports_binding_arrays());
        assert!(!ResourceBindingTier::Tier2.supports_binding_arrays());
        assert!(!ResourceBindingTier::Tier1.supports_binding_arrays());
        assert!(ResourceBindingTier::Tier1 < ResourceBindingTier::Tier2);
        // The listing strings `windows-pass` records for Tier 2 and 3 are unchanged.
        assert_eq!(
            ResourceBindingTier::Tier2.to_string(),
            "Tier 2 (no binding arrays)"
        );
        assert_eq!(ResourceBindingTier::Tier3.to_string(), "Tier 3 (binding arrays)");
        assert_eq!(
            ResourceBindingTier::for_adapter(Backend::Dx12, &Limits::default(), Features::empty()),
            Some(ResourceBindingTier::Tier1)
        );
    }
}
