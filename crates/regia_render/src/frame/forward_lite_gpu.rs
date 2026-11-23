// SPDX-License-Identifier: MPL-2.0
//! Pipelines of the lite forward path (ADR-1874, `crate::forward_lite`): the lab vertex stage with
//! `forward_mobile.wgsl`'s fragment stage on the frame's forward layout, built the first time a
//! frame of that key and tier draws. Up to 2 pipelines (opaque, alpha-tested) per key and tier,
//! and 2 more on wet frames (the surface-weather variant, spec 99, ADR-2032).

use std::sync::Arc;

use super::{HashMap, PipelineKey, Pipes, RenderError, shader_module};
use crate::TrackedDevice;
use crate::forward_lite::{FORWARD_LITE_SHADER_FILE, ForwardLite, lite_defs};
use crate::lab_frame::LabVertex;

/// One key's lite colour pipelines.
pub(super) struct LiteColor {
    /// Opaque draws: no `discard`, so a tile GPU keeps its hidden-surface removal.
    pub(super) opaque: wgpu::RenderPipeline,
    /// Masked and LOD cross-fading draws.
    pub(super) tested: wgpu::RenderPipeline,
}

/// The renderer's lite modules and pipelines (rebuilt with `Pipes` after a shader edit).
#[derive(Default)]
pub(super) struct LiteCache {
    modules: HashMap<(ForwardLite, bool, bool, bool), wgpu::ShaderModule>,
    colors: HashMap<(PipelineKey, ForwardLite, bool), Arc<LiteColor>>,
}

fn lite_module(
    cache: &mut LiteCache,
    d: &TrackedDevice,
    tier: ForwardLite,
    alpha_test: bool,
    encode: bool,
    weather: bool,
) -> Result<wgpu::ShaderModule, RenderError> {
    if let Some(module) = cache.modules.get(&(tier, alpha_test, encode, weather)) {
        return Ok(module.clone());
    }
    let module = shader_module(
        d,
        "lite forward",
        include_str!("../../shaders/forward_mobile.wgsl"),
        FORWARD_LITE_SHADER_FILE,
        lite_defs(tier, alpha_test, encode, weather),
    )?;
    cache
        .modules
        .insert((tier, alpha_test, encode, weather), module.clone());
    Ok(module)
}

fn lite_pipeline(
    pipes: &Pipes,
    d: &TrackedDevice,
    fragment: &wgpu::ShaderModule,
    label: &'static str,
    key: PipelineKey,
) -> wgpu::RenderPipeline {
    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipes.forward_layout),
        vertex: wgpu::VertexState {
            module: &pipes.forward_mod,
            entry_point: Some("vs_main"),
            buffers: &[Some(LabVertex::layout())],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: fragment,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: key.format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: if key.double_sided {
                None
            } else {
                Some(wgpu::Face::Back)
            },
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState {
            count: key.samples,
            mask: u64::MAX,
            alpha_to_coverage_enabled: false,
        },
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(d).as_deref(),
    })
}

/// The lite pipelines of `key` at `tier` (`weather`: the surface-weather variant), built when
/// missing.
fn lite_color(
    pipes: &mut Pipes,
    d: &TrackedDevice,
    key: PipelineKey,
    tier: ForwardLite,
    weather: bool,
) -> Result<Arc<LiteColor>, RenderError> {
    // This family keeps the full forward module under either studio key (ADR-2308).
    let key = key.full_studio();
    if let Some(color) = pipes.lite.colors.get(&(key, tier, weather)) {
        return Ok(color.clone());
    }
    let opaque = lite_module(&mut pipes.lite, d, tier, false, key.encode, weather)?;
    let tested = lite_module(&mut pipes.lite, d, tier, true, key.encode, weather)?;
    let color = Arc::new(LiteColor {
        opaque: lite_pipeline(pipes, d, &opaque, "lite forward", key),
        tested: lite_pipeline(pipes, d, &tested, "lite forward tested", key),
    });
    pipes.lite.colors.insert((key, tier, weather), color.clone());
    Ok(color)
}

/// The frame's lite colour pipelines, `[key, key_ds]`, or `None` at [`ForwardLite::Off`].
/// `weather` picks the surface-weather variant (a wet, snowy or extinguishing frame).
pub(super) fn frame_lite_colors(
    pipes: &mut Pipes,
    d: &TrackedDevice,
    (key, key_ds): (PipelineKey, PipelineKey),
    tier: ForwardLite,
    weather: bool,
) -> Result<Option<[Arc<LiteColor>; 2]>, RenderError> {
    if tier == ForwardLite::Off {
        return Ok(None);
    }
    Ok(Some([
        lite_color(pipes, d, key, tier, weather)?,
        lite_color(pipes, d, key_ds, tier, weather)?,
    ]))
}
