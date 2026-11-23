// SPDX-License-Identifier: MPL-2.0
//! Per-camera GPU state of the shared terrain path (ADR-184 "Texture views keep their own
//! view state").
//!
//! The renderer draws the terrain for the host's main camera and for every render-to-texture
//! camera (cockpit screens, a target feed, a map). Everything the shared path writes per camera
//! (the frame and cascade uniforms, the node slots and draw lists, the engine-CSM group and
//! the CPU mirrors that skip unchanged uploads) lives in a [`ViewGpu`]. [`TerrainGpu`] keeps the
//! active camera's set in its own fields and swaps another camera's set in for that camera's
//! draw, so a texture view neither overwrites the main camera's node slots nor makes it
//! re-upload them the next frame. Heights, layers, materials, pipelines and the virtual
//! texture stay shared: no texture is duplicated.

use std::mem::size_of;

use super::{
    CascadeUniform, DrawList, EngineCsmUniform, FrameUniform, TerrainGpu, engine_csm_bind_group,
    frame_bind_group, make_node_storage, node_bind_group,
};

/// Node slots a new camera's buffer starts with (grows like the main camera's).
const VIEW_NODE_CAPACITY: u32 = 64;
/// Draw-list words a new camera's slot buffer starts with.
const VIEW_SLOT_CAPACITY: u32 = 256;

/// One camera's shared-path GPU state, swapped in and out of [`TerrainGpu`].
pub(crate) struct ViewGpu {
    frame_buffer: wgpu::Buffer,
    occluder_buffer: wgpu::Buffer,
    occluder_bytes: Vec<u8>,
    frame_bind: wgpu::BindGroup,
    sky_irradiance: wgpu::TextureView,
    sky_brdf: wgpu::TextureView,
    sky_irradiance_on: bool,
    motion_set: bool,
    prev_view_proj: [[f32; 4]; 4],
    unjittered_view_proj: [[f32; 4]; 4],
    cascade_bufs: Vec<wgpu::Buffer>,
    cascade_binds: Vec<wgpu::BindGroup>,
    cascade_bytes: Vec<Vec<u8>>,
    node_buffer: wgpu::Buffer,
    node_capacity: u32,
    node_bind: wgpu::BindGroup,
    node_bind_holes: u64,
    slot_buffer: wgpu::Buffer,
    slot_capacity: u32,
    slot_cpu: Vec<u32>,
    draw_lists: Vec<DrawList>,
    dense_cpu: Vec<bool>,
    detail_eye: Option<[f32; 2]>,
    engine_csm_buffer: wgpu::Buffer,
    engine_csm_bind: wgpu::BindGroup,
    engine_csm_views: Vec<wgpu::TextureView>,
    engine_ao: wgpu::TextureView,
    engine_ao_on: bool,
    engine_gi: (wgpu::Buffer, wgpu::Buffer),
    engine_lights: regia_render::EngineLights,
    engine_light_profiles: wgpu::TextureView,
    engine_contact: wgpu::TextureView,
    engine_contact_on: bool,
    engine_cloud: wgpu::TextureView,
    engine_cloud_params: [f32; 4],
    node_cpu: Vec<u8>,
    node_upload_last: (u32, u64),
    frame_bytes: Vec<u8>,
    engine_csm_bytes: Vec<u8>,
    /// The mouth bands' camera slot: a texture view draws band camera 1 (ADR-8824).
    band_camera: u32,
}

impl ViewGpu {
    /// A fresh camera: its own uniform, node and slot buffers and their groups, bound to the
    /// placeholders until the first frame binds the engine's views. Created once per camera.
    pub(crate) fn new(gpu: &TerrainGpu) -> Self {
        let d = &gpu.device;
        let uniform = |label: &str, size: usize| {
            d.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let frame_buffer = uniform("terrain view frame", size_of::<FrameUniform>());
        let occluder_buffer = uniform(
            "terrain view mesh occluders",
            size_of::<regia_render::MeshOccluderBlock>(),
        );
        let frame_bind = frame_bind_group(
            d,
            &gpu.frame_layout,
            &frame_buffer,
            &gpu.sky_irradiance_dummy,
            &gpu.sky_sampler,
            &gpu.sky_irradiance_dummy,
            &occluder_buffer,
        );
        let mut cascade_bufs = Vec::with_capacity(4);
        let mut cascade_binds = Vec::with_capacity(4);
        for _ in 0..4 {
            let buf = uniform("terrain view cascade", size_of::<CascadeUniform>());
            cascade_binds.push(d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("terrain view cascade bind"),
                layout: &gpu.cascade_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buf.as_entire_binding(),
                }],
            }));
            cascade_bufs.push(buf);
        }
        let node_buffer = make_node_storage(
            d,
            "terrain view nodes",
            u64::from(gpu.node_stride) * u64::from(VIEW_NODE_CAPACITY),
        );
        let slot_buffer = make_node_storage(d, "terrain view node slots", u64::from(VIEW_SLOT_CAPACITY) * 4);
        let node_bind = node_bind_group(d, &gpu.node_layout, &node_buffer, &slot_buffer, &gpu.holes_buffer);
        let engine_csm_buffer = uniform("terrain view engine csm", size_of::<EngineCsmUniform>());
        let dummy = &gpu.engine_csm_dummy;
        let engine_csm_bind = engine_csm_bind_group(
            d,
            &gpu.engine_csm_layout,
            [dummy, dummy, dummy, dummy],
            &gpu.shadow_sampler,
            &engine_csm_buffer,
            &gpu.engine_ao_dummy,
            &gpu.engine_ao_dummy,
            (&gpu.engine_gi_dummy.0, &gpu.engine_gi_dummy.1),
            gpu.local_lights
                .then_some((&gpu.engine_lights_dummy, &gpu.engine_ao_dummy)),
            &gpu.engine_ao_dummy,
            &gpu.engine_csm_dummy,
        );
        let identity = regia_core::glam::Mat4::IDENTITY.to_cols_array_2d();
        Self {
            frame_buffer,
            occluder_buffer,
            occluder_bytes: Vec::new(),
            frame_bind,
            sky_irradiance: gpu.sky_irradiance_dummy.clone(),
            sky_brdf: gpu.sky_irradiance_dummy.clone(),
            sky_irradiance_on: false,
            motion_set: false,
            prev_view_proj: identity,
            unjittered_view_proj: identity,
            cascade_bufs,
            cascade_binds,
            cascade_bytes: Vec::new(),
            node_buffer,
            node_capacity: VIEW_NODE_CAPACITY,
            node_bind,
            node_bind_holes: gpu.holes_generation,
            slot_buffer,
            slot_capacity: VIEW_SLOT_CAPACITY,
            slot_cpu: Vec::new(),
            draw_lists: Vec::new(),
            dense_cpu: Vec::new(),
            detail_eye: None,
            engine_csm_buffer,
            engine_csm_bind,
            engine_csm_views: Vec::new(),
            engine_ao: gpu.engine_ao_dummy.clone(),
            engine_ao_on: false,
            engine_gi: gpu.engine_gi_dummy.clone(),
            engine_lights: gpu.engine_lights_dummy.clone(),
            engine_light_profiles: gpu.engine_ao_dummy.clone(),
            engine_contact: gpu.engine_ao_dummy.clone(),
            engine_contact_on: false,
            engine_cloud: gpu.engine_ao_dummy.clone(),
            engine_cloud_params: [0.0; 4],
            node_cpu: vec![0; VIEW_NODE_CAPACITY as usize * gpu.node_stride as usize],
            node_upload_last: (0, 0),
            frame_bytes: Vec::new(),
            engine_csm_bytes: Vec::new(),
            band_camera: 1,
        }
    }
}

impl TerrainGpu {
    /// Exchanges the active camera's shared-path state with `view`. Swapping twice restores
    /// both. A node group built before the last hole change is rebuilt for the incoming camera.
    pub(crate) fn swap_view(&mut self, view: &mut ViewGpu) {
        use std::mem::swap;
        swap(&mut self.frame_buffer, &mut view.frame_buffer);
        swap(&mut self.occluder_buffer, &mut view.occluder_buffer);
        swap(&mut self.occluder_bytes, &mut view.occluder_bytes);
        swap(&mut self.frame_bind, &mut view.frame_bind);
        swap(&mut self.sky_irradiance, &mut view.sky_irradiance);
        swap(&mut self.sky_brdf, &mut view.sky_brdf);
        swap(&mut self.sky_irradiance_on, &mut view.sky_irradiance_on);
        swap(&mut self.motion_set, &mut view.motion_set);
        swap(&mut self.prev_view_proj, &mut view.prev_view_proj);
        swap(&mut self.unjittered_view_proj, &mut view.unjittered_view_proj);
        swap(&mut self.cascade_bufs, &mut view.cascade_bufs);
        swap(&mut self.cascade_binds, &mut view.cascade_binds);
        swap(&mut self.cascade_bytes, &mut view.cascade_bytes);
        swap(&mut self.node_buffer, &mut view.node_buffer);
        swap(&mut self.node_capacity, &mut view.node_capacity);
        swap(&mut self.node_bind, &mut view.node_bind);
        swap(&mut self.node_bind_holes, &mut view.node_bind_holes);
        swap(&mut self.slot_buffer, &mut view.slot_buffer);
        swap(&mut self.slot_capacity, &mut view.slot_capacity);
        swap(&mut self.slot_cpu, &mut view.slot_cpu);
        swap(&mut self.draw_lists, &mut view.draw_lists);
        swap(&mut self.dense_cpu, &mut view.dense_cpu);
        swap(&mut self.detail_eye, &mut view.detail_eye);
        swap(&mut self.engine_csm_buffer, &mut view.engine_csm_buffer);
        swap(&mut self.engine_csm_bind, &mut view.engine_csm_bind);
        swap(&mut self.engine_csm_views, &mut view.engine_csm_views);
        swap(&mut self.engine_ao, &mut view.engine_ao);
        swap(&mut self.engine_ao_on, &mut view.engine_ao_on);
        swap(&mut self.engine_gi, &mut view.engine_gi);
        swap(&mut self.engine_lights, &mut view.engine_lights);
        swap(&mut self.engine_light_profiles, &mut view.engine_light_profiles);
        swap(&mut self.engine_contact, &mut view.engine_contact);
        swap(&mut self.engine_contact_on, &mut view.engine_contact_on);
        swap(&mut self.engine_cloud, &mut view.engine_cloud);
        swap(&mut self.engine_cloud_params, &mut view.engine_cloud_params);
        swap(&mut self.node_cpu, &mut view.node_cpu);
        swap(&mut self.node_upload_last, &mut view.node_upload_last);
        swap(&mut self.frame_bytes, &mut view.frame_bytes);
        swap(&mut self.engine_csm_bytes, &mut view.engine_csm_bytes);
        swap(&mut self.band_camera, &mut view.band_camera);
        if self.node_bind_holes != self.holes_generation {
            self.rebuild_node_bind();
        }
    }
}
