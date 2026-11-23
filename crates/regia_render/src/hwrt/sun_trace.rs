// SPDX-License-Identifier: MPL-2.0
//! The frame's sun-visibility trace (ADR-2035): one compute pass over the single-sample scene
//! depth that reconstructs each pixel's world position and asks a tracer whether the sun is
//! visible from it. Its output is the texture contact shadows write (R = sun visibility, the
//! forward pass's binding 22).
//!
//! Three tracers share the pass ([`SunShadowVariant`]):
//!
//! - [`SunShadowVariant::RayQuery`]: hardware ray queries against a top-level acceleration
//!   structure built from the mesh arena ([`super::GpuAccel`]).
//! - [`SunShadowVariant::ComputeFallback`]: the stackless BVH in storage buffers, on any adapter.
//!   The reference path; it is rebuilt on the CPU whenever a caster moves, so it is not for play.
//! - [`SunShadowVariant::ScreenTrace`]: a march through the depth buffer, where hardware ray
//!   queries are unavailable. A ray that leaves the screen is lit, so this variant keeps the shadow
//!   map (the forward shader multiplies the two) and needs no acceleration structure.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};

use super::variant::SunShadowVariant;
use crate::RenderError;

/// The pass with the ray-query tracer. Needs `EXPERIMENTAL_RAY_QUERY`.
pub const SUN_TRACE_RAY_QUERY_WGSL: &str = concat!(
    "enable wgpu_ray_query;\n",
    include_str!("sun_trace_main.wgsl"),
    include_str!("sun_trace_ray_query.wgsl")
);

/// The pass with the compute BVH tracer.
pub const SUN_TRACE_BVH_WGSL: &str = concat!(
    include_str!("sun_trace_main.wgsl"),
    include_str!("sun_trace_bvh.wgsl"),
    include_str!("bvh_walk.wgsl")
);

/// The pass with the screen-space tracer.
pub const SUN_TRACE_SCREEN_WGSL: &str = concat!(
    include_str!("sun_trace_main.wgsl"),
    include_str!("sun_trace_screen.wgsl")
);

/// The pass's output format: what contact shadows write and the forward pass reads.
pub const SUN_TRACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Threads per workgroup side (`sun_trace_main.wgsl`).
pub const SUN_TRACE_GROUP: u32 = 8;

/// Ray start offset along the surface normal, metres at the eye (`SunTraceParams::tune.x`).
pub const RAY_OFFSET_M: f32 = 0.004;
/// The offset's growth per metre of distance to the eye: depth precision falls with distance
/// (`SunTraceParams::tune.y`).
pub const RAY_OFFSET_PER_M: f32 = 0.0008;
/// Shortest ray the traced tracers use, world units: a small far plane still reaches casters.
pub const MIN_RAY_M: f32 = 1000.0;

/// Depth-buffer samples along one screen-space ray.
pub const SCREEN_STEPS: u32 = 48;
/// First step of the screen-space march, metres (`sun_trace_screen.wgsl`).
pub const SCREEN_FIRST_STEP_M: f32 = 0.03;
/// Longest screen-space ray, metres (`sun_trace_screen.wgsl`).
pub const SCREEN_RAY_M: f32 = 120.0;
/// A sample shadows the ray when it is in front of it by more than this fraction of view depth
/// (`sun_trace_screen.wgsl`).
pub const SCREEN_BIAS: f32 = 0.0015;
/// Assumed occluder thickness as a fraction of view depth (`sun_trace_screen.wgsl`).
pub const SCREEN_THICKNESS: f32 = 0.04;
/// Least assumed occluder thickness, metres (`sun_trace_screen.wgsl`).
pub const SCREEN_THICKNESS_MIN_M: f32 = 0.25;
/// Most assumed occluder thickness, metres: anything thicker is the shadow map's
/// (`sun_trace_screen.wgsl`).
pub const SCREEN_THICKNESS_MAX_M: f32 = 4.0;

/// `SunTraceParams` in `sun_trace_main.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub(crate) struct SunTraceUniform {
    pub(crate) inv_view_proj: [[f32; 4]; 4],
    pub(crate) view_proj: [[f32; 4]; 4],
    pub(crate) to_sun: [f32; 4],
    pub(crate) eye: [f32; 4],
    pub(crate) viewport: [f32; 4],
    pub(crate) tune: [f32; 4],
    pub(crate) counts: [u32; 4],
}

/// The longest ray of a frame whose far plane is `far`.
#[must_use]
pub fn sun_ray_length(far: f32) -> f32 {
    far.max(MIN_RAY_M)
}

/// Compute workgroups for a `width × height` target.
#[must_use]
pub fn sun_trace_groups(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(SUN_TRACE_GROUP), height.div_ceil(SUN_TRACE_GROUP))
}

/// One tracer's compiled pass: a device-level object, shared by the renderers on a device.
#[derive(Clone)]
pub(crate) struct SunTracePipeline {
    pub(crate) layout: wgpu::BindGroupLayout,
    pub(crate) pipeline: wgpu::ComputePipeline,
}

/// The module source, and the label, of `variant`'s pass (`None` for the shadow map).
#[must_use]
fn pass_source(variant: SunShadowVariant) -> Option<(&'static str, &'static str)> {
    match variant {
        SunShadowVariant::ShadowMap => None,
        SunShadowVariant::RayQuery => Some(("sun trace ray query", SUN_TRACE_RAY_QUERY_WGSL)),
        SunShadowVariant::ComputeFallback => Some(("sun trace bvh", SUN_TRACE_BVH_WGSL)),
        SunShadowVariant::ScreenTrace => Some(("sun trace screen", SUN_TRACE_SCREEN_WGSL)),
    }
}

fn entry(binding: u32, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    }
}

fn storage_read(binding: u32) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
    )
}

/// `variant`'s pass from the device's shared pipelines, built on first use (through the tracked
/// device, so the hitch counters see a build on the frame path).
pub(crate) fn sun_trace_pipeline(
    d: &crate::TrackedDevice,
    shared: &Arc<crate::device_pipelines::DevicePipelines>,
    variant: SunShadowVariant,
) -> Result<SunTracePipeline, RenderError> {
    let Some((label, source)) = pass_source(variant) else {
        return Err(RenderError::Shader("the shadow map has no trace pass".into()));
    };
    if !variant.runs_on(d.features()) {
        return Err(RenderError::Shader(format!(
            "{label}: the device has no ray queries"
        )));
    }
    let key = crate::device_pipelines::pipeline_key(&(label, crate::shader_reload::shader_epoch()));
    Ok(shared.get_or_create(key, || {
        let _span = tracing::info_span!("sun_trace_pipeline", variant = variant.as_str()).entered();
        let mut entries = vec![
            entry(
                0,
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
            ),
            entry(
                1,
                wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
            ),
            entry(
                2,
                wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: SUN_TRACE_FORMAT,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
            ),
        ];
        match variant {
            SunShadowVariant::RayQuery => entries.push(entry(
                3,
                wgpu::BindingType::AccelerationStructure { vertex_return: false },
            )),
            SunShadowVariant::ComputeFallback => {
                entries.push(storage_read(3));
                entries.push(storage_read(4));
            }
            SunShadowVariant::ShadowMap | SunShadowVariant::ScreenTrace => {}
        }
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(label),
            entries: &entries,
        });
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(label),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(label),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(label),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("trace_sun"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(d).as_deref(),
        });
        SunTracePipeline { layout, pipeline }
    }))
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "sun_trace_main.wgsl,sun_trace_ray_query.wgsl,sun_trace_screen.wgsl",
        "SunTraceParams",
        SunTraceUniform {
            inv_view_proj,
            view_proj,
            to_sun,
            eye,
            viewport,
            tune,
            counts
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(source: &str, caps: naga::valid::Capabilities) -> Result<naga::Module, String> {
        let module = naga::front::wgsl::parse_str(source).map_err(|e| e.emit_to_string(source))?;
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), caps)
            .validate(&module)
            .map_err(|e| format!("{e:?}"))?;
        Ok(module)
    }

    #[test]
    fn the_plain_tracers_validate_without_any_capability() {
        for source in [SUN_TRACE_BVH_WGSL, SUN_TRACE_SCREEN_WGSL] {
            let module = validate(source, naga::valid::Capabilities::empty()).expect("tracer");
            assert!(module.entry_points.iter().any(|e| e.name == "trace_sun"));
            assert!(!source.contains("wgpu_ray_query"));
        }
    }

    #[test]
    fn the_ray_query_tracer_needs_the_capability() {
        let module = validate(SUN_TRACE_RAY_QUERY_WGSL, naga::valid::Capabilities::RAY_QUERY).expect("rq");
        assert!(module.entry_points.iter().any(|e| e.name == "trace_sun"));
        assert!(validate(SUN_TRACE_RAY_QUERY_WGSL, naga::valid::Capabilities::default()).is_err());
    }

    /// The uniform has WGSL's layout: two matrices, four `vec4<f32>` and one `vec4<u32>`.
    #[test]
    fn the_uniform_matches_the_shader() {
        assert_eq!(std::mem::size_of::<SunTraceUniform>(), 64 * 2 + 16 * 5);
        let main = include_str!("sun_trace_main.wgsl").replace("\r\n", "\n");
        assert!(main.contains("counts: vec4<u32>,"));
        assert!(main.contains(&format!("@workgroup_size({SUN_TRACE_GROUP}, {SUN_TRACE_GROUP})")));
    }

    /// The screen march's constants are the shader's.
    #[test]
    fn the_screen_constants_match_the_shader() {
        let src = include_str!("sun_trace_screen.wgsl");
        for (name, value) in [
            ("SCREEN_FIRST_STEP_M", SCREEN_FIRST_STEP_M),
            ("SCREEN_RAY_M", SCREEN_RAY_M),
            ("SCREEN_BIAS", SCREEN_BIAS),
            ("SCREEN_THICKNESS", SCREEN_THICKNESS),
            ("SCREEN_THICKNESS_MIN_M", SCREEN_THICKNESS_MIN_M),
            ("SCREEN_THICKNESS_MAX_M", SCREEN_THICKNESS_MAX_M),
        ] {
            let line = format!("const {name}: f32 = {value:?};");
            assert!(src.contains(&line), "{line}");
        }
    }

    #[test]
    fn groups_cover_every_pixel() {
        assert_eq!(sun_trace_groups(1, 1), (1, 1));
        assert_eq!(sun_trace_groups(8, 16), (1, 2));
        assert_eq!(sun_trace_groups(9, 17), (2, 3));
    }

    #[test]
    fn rays_reach_past_a_small_far_plane() {
        assert_eq!(sun_ray_length(10.0), MIN_RAY_M);
        assert_eq!(sun_ray_length(50_000.0), 50_000.0);
    }

    /// Every traced variant names a pass; the shadow map names none.
    #[test]
    fn each_variant_names_its_pass() {
        assert!(pass_source(SunShadowVariant::ShadowMap).is_none());
        for v in [
            SunShadowVariant::RayQuery,
            SunShadowVariant::ComputeFallback,
            SunShadowVariant::ScreenTrace,
        ] {
            let (_, source) = pass_source(v).expect("pass");
            assert_eq!(source, v.trace_shader().expect("trace").source);
        }
    }
}
