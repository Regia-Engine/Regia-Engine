// SPDX-License-Identifier: MPL-2.0
//! Per-frame pipeline resolution of `FrameRenderer::render_into` (ADR-1552 split): the forward
//! pipeline sets for the frame's target key (built side by side with the GPU-driven colour
//! variants when both are missing), the deferred blended and transmissive variants (built now
//! where the thread may wait, otherwise the draws whose variant is not ready are skipped this
//! frame), and the mask-decal variants. Moved out of `frame.rs` unchanged; free functions over
//! the pipeline fields they touch, since the frame keeps borrowing its pipeline sets.

use super::{
    ALPHA_ADDITIVE, ALPHA_BLEND, ALPHA_PREMULTIPLIED, Draw, FrameRenderer, MeshEntry, PipelineKey,
    PipelineSet, Pipes, build_pipeline_set, mask_decals_gpu,
};
use crate::{RenderError, TrackedDevice};

/// The renderer fields the forward pipeline sets read and write.
pub(super) struct ForwardSetFields<'a> {
    pub(super) pipes: &'a mut Pipes,
    pub(super) culler: &'a mut crate::gpu_driven::GpuCuller,
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, MeshEntry>,
    pub(super) serial_forward_families: bool,
}

/// What the forward pipeline sets read from the frame.
pub(super) struct ForwardSetInputs<'a> {
    /// The frame's draws.
    pub(super) draws: &'a [Draw],
    /// Indices of the opaque draws.
    pub(super) opaque_ix: &'a [usize],
    /// The frame's pipeline key.
    pub(super) key: PipelineKey,
    /// Its cull-none variant (double-sided materials in a culled frame).
    pub(super) key_ds: PipelineKey,
    /// The colour format of the pass.
    pub(super) color_format: wgpu::TextureFormat,
    /// MSAA sample count.
    pub(super) samples: u32,
    /// The frame draws through the GPU-driven object path.
    pub(super) gpu_objects: bool,
    /// The scene draws both faces.
    pub(super) double_sided: bool,
}

/// Builds the forward pipeline sets the frame's keys miss (and, in the same join, the GPU-driven
/// colour variants the forward pass will bind) and inserts them. Returns whether any draw is
/// double-sided in a culled frame (`any_cull_off`: the frame then binds the `key_ds` set too).
pub(super) fn build_forward_sets(
    fields: ForwardSetFields<'_>,
    d: &TrackedDevice,
    inputs: &ForwardSetInputs<'_>,
) -> Result<bool, RenderError> {
    let ForwardSetFields {
        pipes,
        culler,
        meshes,
        serial_forward_families,
    } = fields;
    let ForwardSetInputs {
        draws,
        opaque_ix,
        key,
        key_ds,
        color_format,
        samples,
        gpu_objects,
        double_sided,
    } = *inputs;
    let any_cull_off = draws.iter().any(|draw| draw.cull_off);
    // A many-light key's set shares its plain twin's depth, shadow, sky and deferred pipelines
    // and differs in the opaque colour pipeline only (ADR-2652): the twin is built first.
    for k in [key, key_ds] {
        if k.restir_di && (k == key || any_cull_off) && !pipes.sets.contains_key(&k.plain()) {
            let plain = build_pipeline_set(d, pipes, k.plain());
            pipes.sets.insert(k.plain(), plain);
        }
    }
    let need_key = !pipes.sets.contains_key(&key);
    let need_ds = any_cull_off && key_ds != key && !pipes.sets.contains_key(&key_ds);
    // The GPU-driven colour variants the forward pass binds further down
    // (`GpuCuller::color_pipelines`, same keys): the culler takes every opaque rigid draw
    // whose mesh has bounds and every skinned draw with a palette sphere (ADR-1368). When a
    // set must be built here, they compile in the same join instead of after it: those two
    // forward uber-shader families back to back were the longest serial stretch of a cold
    // game-frame warm (lane v018-stalls).
    let gpu_variants: Vec<(bool, bool)> = if (need_key || need_ds)
        && gpu_objects
        && !serial_forward_families
        && !FrameRenderer::forward_overlap_env_off()
    {
        let mut culled = None;
        for index in opaque_ix {
            let draw = &draws[*index];
            if (draw.rigid && meshes.get(&draw.mesh).is_some_and(|e| e.bounds.is_some()))
                || draw.skin_sphere.is_some()
            {
                culled = Some(culled.unwrap_or(false) || draw.cull_off);
            }
        }
        match culled {
            None => Vec::new(),
            Some(any_ds) => {
                let mut v = vec![(double_sided, false), (double_sided, true)];
                if any_ds {
                    v.extend([(true, false), (true, true)]);
                }
                v
            }
        }
    } else {
        Vec::new()
    };
    // A frame needing both sets compiles them side by side (`pipeline_warm::join`).
    let ((built, built_ds), gpu_built) = {
        let shared: &Pipes = pipes;
        let build = |k: PipelineKey| {
            build_pipeline_set(
                d,
                shared,
                PipelineKey {
                    format: color_format,
                    samples,
                    ..k
                },
            )
        };
        let sets = || match (need_key, need_ds) {
            (true, true) => {
                let (a, b) = crate::pipeline_warm::join(|| build(key), || build(key_ds));
                (Some(a), Some(b))
            }
            (true, false) => (Some(build(key)), None),
            (false, true) => (None, Some(build(key_ds))),
            (false, false) => (None, None),
        };
        if gpu_variants.is_empty() {
            (sets(), Ok(()))
        } else {
            let (view_bgl, mat_bgl) = (&shared.view_bgl, &shared.mat_bgl);
            let encode = key.encode;
            crate::pipeline_warm::join(sets, || {
                culler
                    .color_pipelines(
                        d,
                        color_format,
                        encode,
                        samples,
                        true,
                        view_bgl,
                        mat_bgl,
                        &gpu_variants,
                    )
                    .map(drop)
            })
        }
    };
    gpu_built?;
    if let Some(set) = built {
        pipes.sets.insert(key, set);
    }
    if let Some(set) = built_ds {
        pipes.sets.insert(key_ds, set);
    }
    Ok(any_cull_off)
}

/// The deferred blended, transmissive and x-ray variants of the frame's draws: built now, side
/// by side, where this thread may wait; otherwise a blended draw whose variant is not built yet
/// leaves `blend_ix` this frame (its build jumps the background queue).
pub(super) fn resolve_deferred_variants(
    set: &PipelineSet,
    set_ds: &PipelineSet,
    draws: &[Draw],
    blend_ix: &mut Vec<usize>,
) {
    type Deferred = crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>;
    // A fixed array, not a `Vec`: this runs per blended draw on every frame.
    let variants = |draw: &Draw| -> [Option<&Deferred>; 6] {
        if draw.graph.is_some() || (draw.alpha_mode < ALPHA_BLEND && !draw.transmissive) {
            return [None; 6];
        }
        let s = if draw.cull_off { set_ds } else { set };
        let first = match draw.alpha_mode {
            _ if draw.xray && !draw.transmissive => &s.xray,
            ALPHA_PREMULTIPLIED => &s.premul,
            ALPHA_ADDITIVE => &s.add,
            _ => &s.blend,
        };
        if draw.transmissive {
            [
                Some(first),
                Some(&s.transmit_mul),
                Some(&s.transmit_add),
                Some(&s.transmit_add_depth),
                Some(&s.transmit_front),
                Some(&s.transmit_refract),
            ]
        } else {
            [Some(first), None, None, None, None, None]
        }
    };
    if crate::pipeline_warm::may_block() {
        let mut need: Vec<&Deferred> = Vec::new();
        for draw in draws {
            for v in variants(draw).into_iter().flatten() {
                if !v.is_built() && !need.iter().any(|n| n.same(v)) {
                    need.push(v);
                }
            }
        }
        let jobs: Vec<_> = need
            .into_iter()
            .map(|p| {
                move || {
                    p.get();
                }
            })
            .collect();
        crate::pipeline_warm::join_all(jobs);
    } else {
        blend_ix.retain(|&i| {
            variants(&draws[i])
                .into_iter()
                .flatten()
                .all(|v| v.try_get().is_some())
        });
    }
}

/// The mask-decal variants of the frame's masked draws, resolved like the deferred variants:
/// built here where this thread may wait; mid-play a masked draw whose variant is not built yet
/// keeps the standard surface this frame (its `mask` is cleared). Counts the draws in
/// `mask_decals`.
pub(super) fn resolve_mask_sets(
    mask: &mut mask_decals_gpu::MaskPipes,
    groups: [&wgpu::BindGroupLayout; 3],
    mask_decals: &mut mask_decals_gpu::MaskDecalState,
    d: &TrackedDevice,
    (key, key_ds): (PipelineKey, PipelineKey),
    draws: &mut [Draw],
    mask_unsupported: usize,
) -> [Option<mask_decals_gpu::MaskSet>; 2] {
    // `MASK_DECALS` variants (`mask_decals_gpu`), resolved like the variants above: built here
    // where this thread may wait; mid-play a masked draw whose variant is not built yet keeps
    // the standard surface this frame (never skipped: it may be opaque).
    let mut mask_sets: [Option<mask_decals_gpu::MaskSet>; 2] = [None, None];
    if draws.iter().any(|draw| draw.mask) {
        mask_sets[0] = Some(mask.set(d, groups, key));
        if draws.iter().any(|draw| draw.mask && draw.cull_off) {
            mask_sets[1] = Some(mask.set(d, groups, key_ds));
        }
        let variant = |draw: &Draw| {
            let set = mask_sets[usize::from(draw.cull_off)].as_ref()?;
            Some(if draw.alpha_mode >= ALPHA_BLEND {
                set.blend.clone()
            } else {
                set.opaque.clone()
            })
        };
        if crate::pipeline_warm::may_block() {
            let mut need: Vec<crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>> = Vec::new();
            for v in draws.iter().filter(|draw| draw.mask).filter_map(variant) {
                if !v.is_built() && !need.iter().any(|n| n.same(&v)) {
                    need.push(v);
                }
            }
            let jobs: Vec<_> = need
                .into_iter()
                .map(|p| {
                    move || {
                        p.get();
                    }
                })
                .collect();
            crate::pipeline_warm::join_all(jobs);
        }
        let mut fallback = 0;
        for draw in draws.iter_mut().filter(|draw| draw.mask) {
            if variant(draw).is_none_or(|v| v.try_get().is_none()) {
                draw.mask = false;
                if draw.visible {
                    fallback += 1;
                }
            }
        }
        let masked = draws.iter().filter(|draw| draw.mask && draw.visible).count();
        mask_decals.note_draws(masked, fallback, mask_unsupported);
    } else {
        mask_decals.note_draws(0, 0, mask_unsupported);
    }
    mask_sets
}
