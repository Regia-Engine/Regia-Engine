// SPDX-License-Identifier: MPL-2.0
//! The GPU frustum and Hi-Z cull of `FrameRenderer::render_into` (ADR-1552 split by pass): the
//! opaque draws' cull meshes, the depth pyramid, the cull dispatch, and the indirect slots handed
//! back to the draws. Moved out of `frame.rs` unchanged. It runs while the frame still borrows its
//! pipeline sets, so it takes the renderer fields it touches ([`CullFields`]) instead of `self`.

use regia_core::glam::Mat4;

use super::{Draw, FrameScene, Frustum, HIZ_MIN_RIGID_CASTERS, MeshEntry, scratch};
use crate::{RenderError, TrackedDevice};

/// The renderer fields the cull reads and writes.
pub(super) struct CullFields<'a> {
    pub(super) culler: &'a mut crate::gpu_driven::GpuCuller,
    pub(super) belt: &'a mut crate::staging::StagingBelt,
    pub(super) scratch: &'a mut scratch::FrameScratch,
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, MeshEntry>,
    pub(super) hiz_occlusion_off: bool,
}

/// What the cull reads from the frame.
pub(super) struct CullInputs<'a> {
    /// The frame's scene (eye, target, depth prepass switch).
    pub(super) scene: &'a FrameScene<'a>,
    /// Indices of the opaque draws.
    pub(super) opaque_ix: &'a [usize],
    /// The single-sample scene depth (the pyramid's source).
    pub(super) depth_view: &'a wgpu::TextureView,
    /// Attachment size.
    pub(super) attach_w: u32,
    /// Attachment size.
    pub(super) attach_h: u32,
    /// Unjittered view-projection.
    pub(super) unjittered_vp: Mat4,
    /// The unjittered view frustum.
    pub(super) frustum: &'a Frustum,
    /// A render-to-texture view is drawing (no Hi-Z: the pyramid is the main view's).
    pub(super) view_pass: bool,
}

/// A draw's world-space cull sphere. A skinned draw uses its joint-palette sphere (ADR-248,
/// already inflated) and ignores the rest pose. A static draw (rigid, or a folded rigid skin)
/// uses the mesh's rest-pose bound under the model, grown by the draw's `inflate`. `None` when
/// a skinned draw has no palette sphere, or a static draw's mesh has no bound.
pub(super) fn draw_sphere(draw: &Draw, entry: &MeshEntry) -> Option<([f32; 3], f32)> {
    if let Some(sphere) = draw.skin_sphere {
        return Some(sphere);
    }
    if !draw.rigid {
        return None;
    }
    let bounds = entry.bounds?;
    let model = regia_core::glam::Mat4::from_cols_array_2d(&draw.model);
    let center = model.transform_point3(regia_core::glam::Vec3::from_array(bounds.center));
    let scale = (model
        .x_axis
        .length_squared()
        .max(model.y_axis.length_squared())
        .max(model.z_axis.length_squared()))
    .sqrt();
    Some((center.to_array(), bounds.radius * scale + draw.inflate))
}

/// Records the GPU cull; sets `Draw::indirect` on every draw the culler took. Returns whether it
/// built the Hi-Z pyramid from this frame's depth.
pub(super) fn record_gpu_cull(
    fields: CullFields<'_>,
    d: &TrackedDevice,
    encoder: &mut wgpu::CommandEncoder,
    draws: &mut [Draw],
    inputs: &CullInputs<'_>,
) -> Result<bool, RenderError> {
    let CullInputs {
        scene,
        opaque_ix,
        depth_view,
        attach_w,
        attach_h,
        unjittered_vp,
        frustum,
        view_pass,
    } = *inputs;
    let _span = tracing::info_span!("gpu_cull").entered();
    let _crumb = crate::breadcrumbs::frame_pass("gpu_cull");
    let mut cull_meshes = fields.scratch.cull_meshes.take();
    for index in opaque_ix {
        let draw = &draws[*index];
        // A masked instance (`mask_decals_gpu`), a thin film (`thin_film_gpu`), a scattering draw
        // (`subsurface_gpu`) or a lightmap receiver that keeps a uniform (`lightmap_paths`) draws
        // from the CPU colour loop.
        if !(draw.rigid || draw.skin_sphere.is_some())
            || draw.mask
            || (draw.lightmap.is_some() && draw.cpu_uniform)
            || draw.film
            || draw.subsurface
        {
            continue;
        }
        let Some(entry) = fields.meshes.get(&draw.mesh) else {
            continue;
        };
        let Some((center, radius)) = draw_sphere(draw, entry) else {
            continue;
        };
        cull_meshes.push(crate::gpu_driven::CullMesh {
            mesh: draw.mesh,
            material: draw.material,
            // The record id, not the object key: two draws that share a key (a cross-fade,
            // or two meshes) keep their own command.
            object: scratch::gpu_record_id(*index),
            center,
            radius,
            index_count: entry.index_count,
            first_index: entry.first_index,
            base_vertex: entry.base_vertex,
            double_sided: draw.cull_off,
            skinned: draw.skin_sphere.is_some(),
            is_static: draw.is_static,
        });
    }
    let eye = regia_core::glam::Vec3::from_array(scene.eye);
    let forward = (regia_core::glam::Vec3::from_array(scene.target) - eye).normalize_or_zero();
    // Below [`HIZ_MIN_RIGID_CASTERS`] the pyramid costs more than the
    // draws it rejects. Sponza stays frustum-only on purpose. A texture view culls
    // against its frustum only: the pyramid is the main view's (ADR-184 addendum).
    let hiz = !fields.hiz_occlusion_off
        && !view_pass
        && scene.depth_prepass
        && cull_meshes.len() >= HIZ_MIN_RIGID_CASTERS;
    // A level that reserved a dense view ([`Self::prewarm`]) gets the pyramid and its
    // bind groups from warm-up on, so the first dense view (a camera cut) creates
    // nothing. The cull binds the pyramid either way; `hiz` decides if it tests it.
    if !view_pass
        && scene.depth_prepass
        && (hiz || fields.culler.reserved() as usize >= HIZ_MIN_RIGID_CASTERS)
    {
        fields.culler.prepare_hiz(d, depth_view, attach_w, attach_h)?;
    }
    {
        let culler = &mut *fields.culler;
        let belt = &mut *fields.belt;
        let mut sink = crate::staging::LiveBelt {
            belt,
            device: d,
            encoder,
        };
        culler.upload(
            d,
            &mut sink,
            &cull_meshes,
            unjittered_vp.to_cols_array_2d(),
            frustum.planes(),
            forward.to_array(),
            attach_w,
            attach_h,
            hiz,
        )?;
    }
    if hiz {
        fields
            .culler
            .build_hiz(d, encoder, depth_view, attach_w, attach_h)?;
    }
    fields.culler.dispatch(d, encoder);
    fields.scratch.cull_meshes.give(cull_meshes);
    let mut slots = fields.scratch.cull_slots.take();
    index_cull_commands(&mut slots, fields.culler.draws());
    for (index, draw) in draws.iter_mut().enumerate() {
        if let Some(command) = cull_command(&slots, index) {
            draw.indirect = Some(command);
        }
    }
    fields.scratch.cull_slots.give(slots);
    Ok(hiz)
}

/// Fills `slots` with each cull record's indirect command.
///
/// `gpu` is [`crate::gpu_driven::GpuCuller::draws`]. Each `object` is the frame draw's
/// `scratch::gpu_record_id`, so two draws that share an object key keep two commands.
/// `slots` starts empty (`Slot::take`).
pub(super) fn index_cull_commands(
    slots: &mut crate::key_hash::KeyMap<u64, u32>,
    gpu: &[crate::gpu_driven::GpuDraw],
) {
    slots.extend(gpu.iter().map(|draw| (draw.object, draw.index)));
}

/// Indirect command of frame draw `index`, when the cull uploaded that draw.
pub(super) fn cull_command(slots: &crate::key_hash::KeyMap<u64, u32>, index: usize) -> Option<u32> {
    slots.get(&scratch::gpu_record_id(index)).copied()
}

#[cfg(test)]
mod tests {
    use super::super::scratch;
    use crate::gpu_driven::GpuDraw;

    /// Two cull records that share an object key keep their own command indexes.
    ///
    /// The record id is the frame draw index. Keying the map by the object key would
    /// leave both draws with the last command.
    #[test]
    fn two_gpu_draws_of_one_object_key_keep_their_own_commands() {
        let object_key = 7u64;
        let gpu = [
            GpuDraw {
                object: scratch::gpu_record_id(0),
                index: 4,
                object_slot: 0,
            },
            GpuDraw {
                object: scratch::gpu_record_id(1),
                index: 9,
                object_slot: 1,
            },
        ];
        assert_ne!(gpu[0].object, gpu[1].object);
        assert_ne!(gpu[0].object, object_key);
        let mut slots = crate::key_hash::KeyMap::default();
        super::index_cull_commands(&mut slots, &gpu);
        assert_eq!(super::cull_command(&slots, 0), Some(4));
        assert_eq!(super::cull_command(&slots, 1), Some(9));
    }
}

#[cfg(test)]
mod bound_tests {
    use super::*;
    use crate::{FrameMesh, FrameRenderer, JointMatrix, LabMaterial, LabVertex, RenderDevice, ShadowCast};

    #[test]
    fn mixed_visitors_keep_the_rigid_draw_sphere_equal_to_the_rest_bound() {
        let device = RenderDevice::headless().expect("lavapipe device");
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let vertices = [
            LabVertex::from_pos_normal_uv([-1.0, -1.0, 0.0], [0.0, 0.0, 1.0], [0.0; 2]),
            LabVertex::from_pos_normal_uv([1.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0; 2]),
        ];
        let indices = [0, 1, 0];
        let material = LabMaterial::default();
        let palette = [JointMatrix::identity(); 2];
        let model = Mat4::from_translation(regia_core::glam::Vec3::new(2.0, 0.0, 0.0));
        let mesh = FrameMesh {
            is_static: false,
            mesh_key: 1,
            material_key: 1,
            object_key: 1,
            vertices: &vertices,
            indices: &indices,
            material: &material,
            model: model.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        };
        let rest = super::super::mesh_bounds(&vertices).expect("rest sphere");
        let mut draw = Draw {
            is_static: false,
            morphed: false,
            mesh: 1,
            material: 1,
            object: 1,
            sort_depth: 0.0,
            alpha_mode: 0,
            transmissive: false,
            refracts: false,
            model: mesh.model,
            joint_base: 0,
            joint_count: 0,
            visible: true,
            rigid: true,
            skin_sphere: None,
            skin_bound: None,
            receiver_layer: 0,
            shadow: ShadowCast::All,
            indirect: None,
            cpu_uniform: false,
            vis: false,
            vis_masked: false,
            vis_instance: None,
            graph: None,
            graph_vertex: false,
            animated: false,
            inflate: 0.0,
            vertex_offset_bounded: false,
            lod_dither: 0.0,
            glass: None,
            cull_off: false,
            over: [0; 2],
            xray: false,
            mask: false,
            hair: false,
            lightmap: None,
            film: false,
            subsurface: false,
            canopy_outside: false,
            src: 0,
        };
        for skinned_first in [false, true] {
            let mut renderer = FrameRenderer::new();
            for skinned in [skinned_first, !skinned_first] {
                let visitor = FrameMesh {
                    joints: if skinned { &palette } else { &[] },
                    ..mesh
                };
                renderer
                    .ensure_mesh(&device.device, &queue, &visitor, 1, false)
                    .expect("mesh upload or reuse");
            }
            let entry = &renderer.meshes[&mesh.mesh_key];
            draw.rigid = true;
            draw.skin_sphere = None;
            assert_eq!(draw_sphere(&draw, entry), Some(([2.0, 0.0, 0.0], rest.radius)));
            draw.rigid = false;
            assert_eq!(
                draw_sphere(&draw, entry),
                None,
                "unbounded skin ignores rest pose"
            );
            let palette_sphere = ([400.0, 0.0, 0.0], 2.0);
            draw.skin_sphere = Some(palette_sphere);
            assert_eq!(draw_sphere(&draw, entry), Some(palette_sphere));
        }
    }
}
