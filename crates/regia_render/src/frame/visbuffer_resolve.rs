// SPDX-License-Identifier: MPL-2.0
//! The visibility-buffer material resolve of `FrameRenderer::render_into` (ADR-1552 split by
//! pass): clears (or keeps) the colour like the forward pass would, writes the material depth,
//! then shades the visibility pixels with the forward fragment body per material. Moved out of
//! `frame.rs` unchanged. The pass takes the renderer fields it touches ([`ResolveFields`])
//! instead of `self`. On the texture table path a profile can ask for one draw that shades every
//! material ([`crate::VisibilityPath::single_pass_resolve`], ADR-2071).

use crate::parallel_finish::pass_cuts::SegmentedEncoder as _;

use super::{FrameTarget, MaterialEntry};
use crate::{RenderError, TrackedDevice};

/// The renderer fields the resolve reads and writes.
pub(super) struct ResolveFields<'a> {
    pub(super) visbuffer: &'a mut crate::visbuffer::VisBufferPass,
    pub(super) materials: &'a crate::key_hash::KeyMap<u64, MaterialEntry>,
    /// The material arena (the single-pass resolve binds it whole).
    pub(super) arena: Option<&'a crate::bindless::MaterialArena>,
}

/// What the resolve reads from the frame.
pub(super) struct ResolveInputs<'a> {
    /// The ray-traced sun's forward module ([`super::PipelineKey::sun_rt`]).
    pub(super) sun_rt: bool,
    /// The frame's target (whether the colour is kept or cleared).
    pub(super) target: &'a FrameTarget<'a>,
    /// The linear HDR colour (visibility frames are HDR).
    pub(super) hdr_view: &'a Option<wgpu::TextureView>,
    /// The frame's view bind group (group 0).
    pub(super) view_bg: &'a wgpu::BindGroup,
    /// The view bind group layout (resolve pipelines).
    pub(super) view_bgl: &'a wgpu::BindGroupLayout,
    /// The material bind group layout (resolve pipelines).
    pub(super) mat_bgl: &'a wgpu::BindGroupLayout,
    /// The bindless texture table, when the frame binds one (draw tally).
    pub(super) texture_table_group: &'a Option<wgpu::BindGroup>,
    /// The colour format of the pass.
    pub(super) color_format: wgpu::TextureFormat,
    /// The pipelines encode sRGB in the shader.
    pub(super) encode: bool,
    /// Pre-exposure of the clear colour.
    pub(super) pre_exposure: f32,
    /// The profile asks for one draw over every material (ADR-2071).
    pub(super) single_pass: bool,
    /// The `LIGHTMAP` shade pipeline and its group 3, when an instance is a lightmap receiver
    /// (ADR-2107).
    pub(super) lightmap: Option<(&'a wgpu::RenderPipeline, &'a wgpu::BindGroup)>,
}

/// The colour attachment's load: what the forward colour pass would do.
fn color_load(target: &FrameTarget<'_>, pre_exposure: f32) -> wgpu::LoadOp<wgpu::Color> {
    if target.load_color {
        wgpu::LoadOp::Load
    } else {
        // Same pre-exposed clear as the forward colour pass.
        let k = f64::from(pre_exposure);
        wgpu::LoadOp::Clear(wgpu::Color {
            r: 0.02 * k,
            g: 0.02 * k,
            b: 0.025 * k,
            a: 1.0,
        })
    }
}

/// Draws `list`'s materials with the bound shade pipeline: one full-screen triangle each, the
/// material depth's equal test keeping its pixels.
fn shade_materials(
    tally: &mut crate::frame_stats::DrawTally,
    pass: &mut wgpu::RenderPass<'_>,
    materials: &crate::key_hash::KeyMap<u64, MaterialEntry>,
    list: &[(u32, u64)],
) {
    for &(slot, key) in list {
        let Some(material) = materials.get(&key) else {
            continue;
        };
        tally.set_bind_group(pass, 2, &material.bg, material.offsets());
        tally.draw(pass, 0..3, slot..slot + 1);
    }
}

/// The single-pass resolve (ADR-2071). `Ok(None)` when it cannot run this frame: the device says
/// why in `fallback`, or the frame's materials have no arena record.
fn record_single_pass(
    fields: &mut ResolveFields<'_>,
    d: &TrackedDevice,
    q: &crate::device::TrackedQueue,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &ResolveInputs<'_>,
    draw_counts: &mut crate::frame_stats::FrameDrawCounts,
) -> Result<Option<u32>, RenderError> {
    let Some(arena) = fields.arena else {
        return Ok(None);
    };
    let stride = arena.stats().stride.max(1);
    let materials = fields.materials;
    let pipeline = fields.visbuffer.single_pass_pipeline(
        d,
        inputs.view_bgl,
        crate::visbuffer::single_pass::SinglePassTarget {
            stride,
            format: inputs.color_format,
            encode: inputs.encode,
            sun_rt: inputs.sun_rt,
        },
    )?;
    let Some((group, named)) = fields.visbuffer.single_pass_group(d, q, arena, |key| {
        let offset = materials.get(&key)?.dyn_offset?;
        u32::try_from(u64::from(offset) / stride).ok()
    }) else {
        return Ok(None);
    };
    let (Some(vis_bg), Some(color_view)) = (fields.visbuffer.resolve_bind_group(), inputs.hdr_view.as_ref())
    else {
        return Ok(None);
    };
    let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("vis resolve single pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: color_view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: color_load(inputs.target, inputs.pre_exposure),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        ..Default::default()
    });
    let mut tally = crate::frame_stats::DrawTally::with_texture_table(inputs.texture_table_group.as_ref());
    tally.set_bind_group(&mut pass, 0, inputs.view_bg, &[]);
    tally.set_bind_group(&mut pass, 1, vis_bg, &[]);
    tally.set_bind_group(&mut pass, 2, &group, &[]);
    tally.set_pipeline(&mut pass, &pipeline);
    tally.draw(&mut pass, 0..3, 0..1);
    draw_counts.visbuffer_resolve = tally.finish();
    Ok(Some(named))
}

/// Records the resolve when the frame drew visibility geometry (`vis_drawn`). Returns whether the
/// resolve ran (the forward pass then keeps its colour) and stores its draw tally in
/// `draw_counts.visbuffer_resolve`.
pub(super) fn record_visbuffer_resolve(
    mut fields: ResolveFields<'_>,
    d: &TrackedDevice,
    q: &crate::device::TrackedQueue,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &ResolveInputs<'_>,
    vis_drawn: bool,
    draw_counts: &mut crate::frame_stats::FrameDrawCounts,
) -> Result<bool, RenderError> {
    // One draw over every material where the device and this frame allow (ADR-2071).
    let fallback = if inputs.single_pass && vis_drawn {
        if inputs.lightmap.is_some() {
            // The single-pass module has no `LIGHTMAP` variant (ADR-2106).
            Some(crate::visbuffer::SinglePassFallback::LightmapReceivers)
        } else {
            crate::visbuffer::single_pass::fallback_on(d)
        }
    } else {
        None
    };
    if inputs.single_pass
        && vis_drawn
        && fallback.is_none()
        && let Some(named) = {
            let _span = tracing::info_span!("visbuffer_resolve", single_pass = true).entered();
            let _crumb = crate::breadcrumbs::frame_pass("visbuffer_resolve");
            record_single_pass(&mut fields, d, q, encoder, inputs, draw_counts)?
        }
    {
        fields.visbuffer.set_resolve_report(crate::VisResolveReport {
            requested: true,
            single_pass: true,
            fallback: None,
            draws: draw_counts.visbuffer_resolve.draws,
            materials: named,
            lightmap_materials: 0,
        });
        return Ok(true);
    }
    let ResolveFields {
        visbuffer, materials, ..
    } = fields;
    let ResolveInputs {
        sun_rt,
        target,
        hdr_view,
        view_bg,
        view_bgl,
        mat_bgl,
        texture_table_group,
        color_format,
        encode,
        pre_exposure,
        lightmap,
        ..
    } = *inputs;
    let mut vis_resolved = false;
    let mut lightmap_materials = 0u32;
    if vis_drawn {
        let _span = tracing::info_span!("visbuffer_resolve").entered();
        let _crumb = crate::breadcrumbs::frame_pass("visbuffer_resolve");
        let (depth_pipe, shade_pipe) =
            visbuffer.resolve_pipelines(d, color_format, encode, sun_rt, view_bgl, mat_bgl)?;
        let color_view = hdr_view.as_ref().expect("visibility frames are HDR");
        let slots: Vec<(u32, u64)> = visbuffer.frame_materials().collect();
        let first_material = slots.iter().find_map(|(_, key)| materials.get(key));
        if let (Some(material_depth), Some(vis_bg), Some(first)) = (
            visbuffer.material_depth_view(),
            visbuffer.resolve_bind_group(),
            first_material,
        ) {
            vis_resolved = true;
            let mut pass = encoder.segmented_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vis resolve"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: color_load(target, pre_exposure),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: material_depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                ..Default::default()
            });
            let mut tally = crate::frame_stats::DrawTally::with_texture_table(texture_table_group.as_ref());
            tally.set_bind_group(&mut pass, 0, view_bg, &[]);
            tally.set_bind_group(&mut pass, 1, vis_bg, &[]);
            tally.set_bind_group(&mut pass, 2, &first.bg, first.offsets());
            tally.set_pipeline(&mut pass, &depth_pipe);
            tally.draw(&mut pass, 0..3, 0..1);
            // Only the materials with a receiver take the `LIGHTMAP` variant (ADR-2689): the plain
            // ones first, then the variant and its table once.
            let variants = visbuffer.resolve_variants(lightmap.is_some());
            lightmap_materials = variants.lightmap.len() as u32;
            if !variants.plain.is_empty() {
                tally.set_pipeline(&mut pass, &shade_pipe);
                shade_materials(&mut tally, &mut pass, materials, &variants.plain);
            }
            if let Some((pipe, bg)) = lightmap.filter(|_| !variants.lightmap.is_empty()) {
                tally.set_pipeline(&mut pass, pipe);
                tally.set_bind_group(&mut pass, 3, bg, &[]);
                shade_materials(&mut tally, &mut pass, materials, &variants.lightmap);
            }
            draw_counts.visbuffer_resolve = tally.finish();
        }
        if vis_resolved {
            visbuffer.set_resolve_report(crate::VisResolveReport {
                requested: inputs.single_pass,
                single_pass: false,
                fallback,
                draws: draw_counts.visbuffer_resolve.draws,
                materials: slots.len() as u32,
                lightmap_materials,
            });
        }
    }
    Ok(vis_resolved)
}
