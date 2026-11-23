// SPDX-License-Identifier: MPL-2.0
//! The instanced shadow and depth lists of `FrameRenderer::render_into` for the storage-object
//! path (ADR-1552 split, line m): each shadow job's casters and the depth prepass's opaque draws
//! as instance batches, the instance bind group and the instanced pipelines they draw with.
//! Moved out of `frame.rs` unchanged; built before the frame borrows its pipeline sets.

use super::shadow_passes::InstancedPasses;
use super::*;

/// What the instance lists are built from.
pub(super) struct InstanceListInputs<'a> {
    /// The frame's scene (depth prepass and double-sided switches).
    pub(super) scene: &'a FrameScene<'a>,
    /// The frame's draws.
    pub(super) draws: &'a [Draw],
    /// Each shadow job's caster list.
    pub(super) job_casters: &'a [std::sync::Arc<[usize]>],
    /// Visible opaque draws.
    pub(super) opaque_ix: &'a [usize],
    /// Instanced (storage-object) draws are on.
    pub(super) gpu_objects: bool,
    /// MSAA sample count before the per-frame fallbacks.
    pub(super) samples_early: u32,
}

impl FrameRenderer {
    /// Uploads the instanced shadow and depth lists and resolves their pipelines; `None` when
    /// the frame has no storage objects or nothing to instance.
    pub(super) fn build_instance_lists(
        &mut self,
        d: &crate::TrackedDevice,
        inputs: &InstanceListInputs<'_>,
    ) -> Result<Option<InstancedPasses>, RenderError> {
        let InstanceListInputs {
            scene,
            draws,
            job_casters,
            opaque_ix,
            gpu_objects,
            samples_early,
        } = *inputs;
        // Instanced shadow + depth for the storage-object path. Built before the
        // pipeline borrow so the culler can allocate without aliasing `self.pipes`.
        // One object buffer holds shadow instances, then depth instances.
        let instanced = if gpu_objects {
            let (shadow_bgl, view_bgl_inst, mat_bgl_inst) = {
                let pipes = self
                    .pipes
                    .as_ref()
                    .ok_or_else(|| RenderError::Shader("pipelines missing before instanced passes".into()))?;
                (
                    pipes.shadow_bgl.clone(),
                    pipes.view_bgl.clone(),
                    pipes.mat_bgl.clone(),
                )
            };
            let skin = self
                .skin_buf
                .clone()
                .ok_or_else(|| RenderError::Shader("skin buffer missing before instanced passes".into()))?;
            let to_src = |index: usize| {
                let draw = &draws[index];
                crate::gpu_driven::InstanceSrc {
                    mesh: draw.mesh,
                    material: draw.material,
                    model: draw.model,
                    joint_base: draw.joint_base,
                    lod_dither: draw.lod_dither,
                    double_sided: draw.cull_off,
                }
            };
            // Shadow casters batch by mesh. A vertex-stage graph also batches by material: it
            // draws with its own shadow pipeline and material group. So does a masked standard
            // caster under alpha-tested shadows (`set_alpha_tested_shadows`): its batch binds its
            // material for the alpha test.
            let alpha_tested_shadows = self.alpha_tested_shadows;
            let to_shadow_src = |index: usize| crate::gpu_driven::InstanceSrc {
                model: shadow_model(draws[index].model, draws[index].shadow),
                material: if draws[index].graph_vertex
                    || shadow_batches_by_material(
                        alpha_tested_shadows,
                        draws[index].alpha_mode,
                        draws[index].hair,
                    ) {
                    draws[index].material
                } else {
                    0
                },
                ..to_src(index)
            };
            // Double-sided casters join the lists too: their batches carry the cull mode
            // (`InstanceBatch::double_sided`) and draw with the cull-none pipeline.
            let (mut shadow_src, mut depth_src) = self.culler.take_instance_lists();
            for list in job_casters {
                let dst = shadow_src.push_list();
                dst.extend(list.iter().copied().map(to_shadow_src));
            }
            if scene.depth_prepass {
                depth_src.extend(opaque_ix.iter().copied().map(to_src));
            }
            let uploaded = {
                let culler = &mut self.culler;
                let belt = &mut self.belt;
                culler.upload_instance_lists(d, belt, shadow_src.lists(), &depth_src)
            };
            // Give the lists back before `?`: an upload error must not drop their capacity.
            self.culler.give_instance_lists(shadow_src, depth_src);
            let (shadow_batches, depth_batches) = uploaded?;
            let no_shadow = shadow_batches.iter().all(Vec::is_empty);
            if no_shadow && depth_batches.is_empty() {
                None
            } else {
                let bg = self.culler.instance_bind_group(d, &skin)?;
                // `[culled, cull-none]`. The cull-none pipeline is only fetched when a batch
                // needs it (built by [`Self::prewarm`] or the first such frame, then cached).
                let shadow_ds = shadow_batches.iter().flatten().any(|batch| batch.double_sided);
                let shadow_pipe = if no_shadow {
                    None
                } else {
                    let culled = self
                        .culler
                        .instanced_shadow_pipeline(d, &shadow_bgl, scene.double_sided)?;
                    let cull_none = if shadow_ds {
                        self.culler.instanced_shadow_pipeline(d, &shadow_bgl, true)?
                    } else {
                        culled.clone()
                    };
                    Some([culled, cull_none])
                };
                // Alpha-tested casters (`set_alpha_tested_shadows`): `[culled, cull-none]`, fetched
                // only when a batch carries a material (graph-vertex batches never use them).
                let mask_shadow_pipe = if self.alpha_tested_shadows
                    && shadow_batches.iter().flatten().any(|batch| batch.material != 0)
                {
                    let culled = self.culler.instanced_mask_shadow_pipeline(
                        d,
                        &shadow_bgl,
                        &mat_bgl_inst,
                        scene.double_sided,
                    )?;
                    let cull_none = if shadow_ds {
                        self.culler
                            .instanced_mask_shadow_pipeline(d, &shadow_bgl, &mat_bgl_inst, true)?
                    } else {
                        culled.clone()
                    };
                    Some([culled, cull_none])
                } else {
                    None
                };
                let depth_ds = depth_batches.iter().any(|batch| batch.double_sided);
                let depth_pipe = if depth_batches.is_empty() {
                    None
                } else {
                    // `[cull mode][mask]`: `[culled, cull-none]` × `[opaque, mask]`.
                    let mut pair = |double_sided: bool| -> Result<[wgpu::RenderPipeline; 2], RenderError> {
                        let mut pipe = |mask: bool| {
                            self.culler.instanced_depth_pipeline(
                                d,
                                &view_bgl_inst,
                                &mat_bgl_inst,
                                double_sided,
                                samples_early,
                                mask,
                            )
                        };
                        Ok([pipe(false)?, pipe(true)?])
                    };
                    let culled = pair(scene.double_sided)?;
                    let cull_none = if depth_ds { pair(true)? } else { culled.clone() };
                    Some([culled, cull_none])
                };
                Some((
                    bg,
                    shadow_batches,
                    depth_batches,
                    shadow_pipe,
                    depth_pipe,
                    mask_shadow_pipe,
                ))
            }
        } else {
            None
        };
        Ok(instanced)
    }
}
