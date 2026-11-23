// SPDX-License-Identifier: MPL-2.0
//! Two-eye frames (spec 58 § 2.6, ADR-1918).
//!
//! An XR frame draws the scene once per eye through [`FrameRenderer::render_eye`]. Each eye is a
//! texture view (ADR-184): it swaps its own [`view_state::ViewState`] in, so the targets, the
//! temporal history, the shadow maps and the post chain are per eye, and meshes, materials and
//! pipelines stay shared. Both eyes take the same path, so an effect can never run in one eye
//! only: a difference between the eyes reads as a fault in a headset, not as a lower quality.
//!
//! [`EyeCamera`] replaces the look-at camera of [`FrameScene`]: an XR eye has an asymmetric
//! frustum and any roll, which an eye, a target and one field of view cannot state. Culling uses
//! [`EyeCamera::cull`], one frustum around both eyes, so both eyes draw the same set.
//!
//! The frame clock counts host frames, and a texture view does not advance it. An XR frame has no
//! main view, so [`FrameRenderer::begin_stereo_frame`] and [`FrameRenderer::end_stereo_frame`] do
//! the clock tick and the eviction sweep the main view would.

use regia_core::glam::{Mat4, Vec3, Vec4};

use super::pipelines::shader_module;
use super::{FrameRenderer, FrameScene, FrameTarget, TextureViewFrame, view_state};
use crate::{RenderDevice, RenderError};

/// The texture-view ids of the two eyes ([`crate::FramePassContext::texture_view`] names the eye a
/// registered pass is recording for). A range no asset id reaches.
pub const STEREO_EYE_VIEWS: [u64; 2] = [0xFD00_0000_0000_0000, 0xFD00_0000_0000_0001];

/// The eye (0 left, 1 right) of a [`crate::FramePassContext::texture_view`] id, `None` for any
/// other view.
#[must_use]
pub fn stereo_eye_of_view(view: Option<u64>) -> Option<usize> {
    STEREO_EYE_VIEWS.iter().position(|id| Some(*id) == view)
}

/// One eye's matrices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EyeCamera {
    /// World to view.
    pub view: Mat4,
    /// View to clip: the frame's standard depth (near 0, far 1), any frustum.
    pub projection: Mat4,
    /// World to clip of the frustum both eyes cull with (the eye's own `projection * view`, when
    /// the caller has no union).
    pub cull: Mat4,
}

impl EyeCamera {
    /// The eye's position in the world.
    #[must_use]
    pub fn position(&self) -> Vec3 {
        self.view.inverse().w_axis.truncate()
    }

    /// The eye's forward direction in the world.
    #[must_use]
    pub fn forward(&self) -> Vec3 {
        (-self.view.inverse().z_axis.truncate()).normalize_or_zero()
    }

    /// Tangents of the frustum's left, right, down and up edges (signed, as a field of view).
    #[must_use]
    pub fn tangents(&self) -> [f32; 4] {
        let p = self.projection;
        // At view depth -1: clip.x = m00 x - m20, clip.w = 1; likewise for y.
        let (mx, my) = (p.x_axis.x, p.y_axis.y);
        let (ox, oy) = (p.z_axis.x, p.z_axis.y);
        [
            (-1.0 + ox) / mx,
            (1.0 + ox) / mx,
            (-1.0 + oy) / my,
            (1.0 + oy) / my,
        ]
    }

    /// The vertical field of view of a symmetric frustum of aspect `aspect` that holds this one.
    /// The frame's shadow fit and level-of-detail code read a symmetric camera.
    #[must_use]
    pub fn bounding_fov_y(&self, aspect: f32) -> f32 {
        let [left, right, down, up] = self.tangents();
        let vertical = down.abs().max(up.abs());
        let horizontal = left.abs().max(right.abs()) / aspect.max(1.0e-4);
        2.0 * vertical.max(horizontal).atan()
    }

    /// `scene` with the eye's position, a target along its forward, its near and far planes and
    /// a field of view that holds the frustum: what the frame's camera-derived work reads.
    #[must_use]
    pub fn aim<'a>(&self, scene: FrameScene<'a>) -> FrameScene<'a> {
        let eye = self.position();
        let aspect = scene.width.max(1) as f32 / scene.height.max(1) as f32;
        let mut scene = FrameScene {
            eye: eye.to_array(),
            target: (eye + self.forward()).to_array(),
            fov_y: self.bounding_fov_y(aspect),
            ..scene
        };
        scene.quality.horizontal_plus = false;
        scene
    }
}

/// The hidden triangle mesh of one eye, already in the eye's clip space
/// ([`hidden_area_clip_vertices`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EyeHiddenArea<'a> {
    /// Identifies the mesh: the eye rebuilds its vertex buffer when this changes.
    pub generation: u64,
    /// Clip-space triangle list, three vertices per triangle.
    pub clip_vertices: &'a [[f32; 4]],
}

/// Turns a hidden triangle mesh on the plane `z = -1` into clip-space triangles of `projection`
/// at the near plane (`z = 0`), for [`EyeFrame::hidden_area`].
#[must_use]
pub fn hidden_area_clip_vertices(vertices: &[[f32; 2]], indices: &[u32], projection: &Mat4) -> Vec<[f32; 4]> {
    indices
        .iter()
        .filter_map(|&index| vertices.get(index as usize))
        .map(|v| {
            let clip = *projection * Vec4::new(v[0], v[1], -1.0, 1.0);
            [clip.x, clip.y, 0.0, clip.w]
        })
        .collect()
}

/// One eye of a stereo frame.
pub struct EyeFrame<'a> {
    /// 0 left, 1 right.
    pub eye: usize,
    /// The eye's matrices.
    pub camera: EyeCamera,
    /// The scene, sized to the eye target. [`EyeCamera::aim`] is applied here.
    pub scene: FrameScene<'a>,
    /// The eye image.
    pub target: FrameTarget<'a>,
    /// The hidden triangle mesh: its pixels are masked in depth before the prepass, when the
    /// frame draws a single-sample prepass.
    pub hidden_area: Option<EyeHiddenArea<'a>>,
    /// Receives the eye's depth (`Depth32Float`, the target's size, `COPY_DST`): the depth-info
    /// layer image.
    pub depth_target: Option<&'a wgpu::Texture>,
}

/// What [`FrameRenderer::render_eye`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EyeFrameReport {
    /// CPU time of the eye, milliseconds.
    pub cpu_ms: f32,
    /// GPU time of the eye's last timed frame, milliseconds.
    pub gpu_ms: f32,
    /// Draws after culling.
    pub drawn: usize,
    /// Sun shadow passes drawn.
    pub shadow_passes: u32,
    /// Triangles of the hidden-area mesh drawn into depth.
    pub hidden_triangles: u32,
    /// The eye's depth reached [`EyeFrame::depth_target`].
    pub depth_copied: bool,
}

/// The hidden-area draw the prepass records for the eye being rendered.
#[derive(Clone)]
pub(super) struct EyeMaskDraw {
    pipeline: wgpu::RenderPipeline,
    vertices: wgpu::Buffer,
    count: u32,
}

impl EyeMaskDraw {
    /// Records the mask into the prepass, after its depth clear and before any draw.
    pub(super) fn record(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, self.vertices.slice(..));
        pass.draw(0..self.count, 0..1);
    }
}

/// The renderer's XR eye state.
#[derive(Default)]
pub(super) struct StereoState {
    /// Full image dimensions during a still sub-rectangle capture.
    pub(super) capture_size: Option<[u32; 2]>,
    /// The eye being rendered.
    pub(super) camera: Option<EyeCamera>,
    /// Its hidden-area draw.
    pub(super) mask: Option<EyeMaskDraw>,
    pipeline: Option<wgpu::RenderPipeline>,
    /// Each eye's uploaded mesh: its generation, vertices and vertex count.
    masks: [Option<(u64, wgpu::Buffer, u32)>; 2],
}

impl StereoState {
    /// The eye's mask draw, uploading a new mesh or building the pipeline on first use.
    fn mask_draw(
        &mut self,
        device: &RenderDevice,
        eye: usize,
        area: EyeHiddenArea<'_>,
    ) -> Result<Option<EyeMaskDraw>, RenderError> {
        let Ok(count) = u32::try_from(area.clip_vertices.len()) else {
            return Ok(None);
        };
        if count == 0 {
            return Ok(None);
        }
        let d = device.tracked();
        if self.pipeline.is_none() {
            self.pipeline = Some(mask_pipeline(&d)?);
        }
        let slot = &mut self.masks[eye];
        if slot
            .as_ref()
            .is_none_or(|(generation, ..)| *generation != area.generation)
        {
            let bytes = bytemuck::cast_slice::<[f32; 4], u8>(area.clip_vertices);
            let buffer = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("xr hidden area"),
                size: bytes.len() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            device.tracked_queue().write_buffer(&buffer, 0, bytes);
            *slot = Some((area.generation, buffer, count));
        }
        let (Some(pipeline), Some((_, buffer, count))) = (self.pipeline.as_ref(), slot.as_ref()) else {
            return Ok(None);
        };
        Ok(Some(EyeMaskDraw {
            pipeline: pipeline.clone(),
            vertices: buffer.clone(),
            count: *count,
        }))
    }
}

fn mask_pipeline(d: &wgpu::Device) -> Result<wgpu::RenderPipeline, RenderError> {
    let module = shader_module(
        d,
        "xr hidden area",
        include_str!("../../shaders/xr_eye_mask.wgsl"),
        "xr_eye_mask.wgsl",
        std::collections::HashMap::new(),
    )?;
    let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("xr hidden area"),
        bind_group_layouts: &[],
        immediate_size: 0,
    });
    Ok(d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("xr hidden area"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: 16,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x4],
            })],
            compilation_options: Default::default(),
        },
        fragment: None,
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Always),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(d).as_deref(),
    }))
}

impl FrameRenderer {
    /// Starts one XR frame: the frame clock ticks once for both eyes.
    pub fn begin_stereo_frame(&mut self) {
        self.frame_index += 1;
    }

    /// Ends one XR frame: the eviction sweep the main view runs at its frame end.
    pub fn end_stereo_frame(&mut self) {
        if self.cache_trim_pending || self.frame_index.is_multiple_of(super::EVICT_SWEEP_EVERY) {
            let pressure = std::mem::take(&mut self.cache_trim_pending);
            self.evict(self.frame_index, pressure);
        }
    }

    /// Renders one eye of an XR frame into [`EyeFrame::target`] (see the module doc).
    ///
    /// # Errors
    ///
    /// As [`Self::render_into`], and [`RenderError::Shader`] when the hidden-area pipeline does
    /// not build. An `eye` other than 0 or 1 is [`RenderError::Readback`]. The main view's state
    /// is restored either way.
    #[tracing::instrument(skip_all, name = "xr_render_eye", fields(eye = frame.eye))]
    pub fn render_eye(
        &mut self,
        device: &RenderDevice,
        frame: EyeFrame<'_>,
    ) -> Result<EyeFrameReport, RenderError> {
        let EyeFrame {
            eye,
            camera,
            scene,
            target,
            hidden_area,
            depth_target,
        } = frame;
        let Some(asset_id) = STEREO_EYE_VIEWS.get(eye).copied() else {
            return Err(RenderError::Readback(format!("no eye {eye}")));
        };
        let started = std::time::Instant::now();
        self.sync_shader_epoch();
        let scene = camera.aim(scene);
        let (width, height) = (scene.width.max(1), scene.height.max(1));
        let mask = match hidden_area {
            Some(area) => self.stereo.mask_draw(device, eye, area)?,
            None => None,
        };
        let hidden_triangles = mask.as_ref().map_or(0, |draw| draw.count / 3);
        let mut state = self
            .texture_views
            .remove(&asset_id)
            .unwrap_or_else(|| Box::new(view_state::ViewState::new()));
        let timed = self.gpu_timer.is_some();
        self.swap_view_state(&mut state);
        if timed && self.gpu_timer.is_none() {
            self.set_gpu_timing(device, true);
        }
        self.view_pass = Some(asset_id);
        self.stereo.camera = Some(camera);
        self.stereo.mask = mask;
        if let Some(draw) = self.shared_draw.as_mut() {
            draw.set_view(Some(asset_id));
        }
        // Debug lines are the desktop camera's; the probes follow the main view (ADR-184).
        let gizmos = self.gizmos.take();
        let probes_active = std::mem::replace(&mut self.ddgi.active, false);
        let result = self.render_into(device, &scene, target);
        let drawn = self.last_draw_stats.drawn;
        let depth_copied = result.is_ok()
            && depth_target.is_some_and(|depth| self.copy_eye_depth(device, depth, (width, height)));
        self.ddgi.active = probes_active;
        self.gizmos = gizmos;
        if let Some(draw) = self.shared_draw.as_mut() {
            draw.set_view(None);
        }
        self.stereo.camera = None;
        self.stereo.mask = None;
        self.view_pass = None;
        self.swap_view_state(&mut state);
        let timing = TextureViewFrame {
            cpu_ms: started.elapsed().as_secs_f32() * 1000.0,
            gpu_ms: state.gpu_timings().frame_ms,
            shadow_passes: state.shadow_report().shadow_passes,
        };
        self.texture_views.insert(asset_id, state);
        result.map(|()| EyeFrameReport {
            cpu_ms: timing.cpu_ms,
            gpu_ms: timing.gpu_ms,
            drawn,
            shadow_passes: timing.shadow_passes,
            hidden_triangles,
            depth_copied,
        })
    }

    /// Copies the active eye's scene depth into `depth`. False when no target of that size
    /// exists (a scaled frame) or `depth` does not match it.
    fn copy_eye_depth(&self, device: &RenderDevice, depth: &wgpu::Texture, size: (u32, u32)) -> bool {
        let Some(pair) = self.targets.get(&size) else {
            return false;
        };
        let source = &pair.depth_texture;
        if source.size() != depth.size() || source.format() != depth.format() {
            return false;
        }
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xr eye depth"),
            });
        encoder.copy_texture_to_texture(source.as_image_copy(), depth.as_image_copy(), source.size());
        device.tracked_queue().submit([encoder.finish()]);
        true
    }

    /// Releases both eyes' state and hidden-area meshes (a session ended, or the eye size
    /// changed for good).
    pub fn drop_stereo_views(&mut self) {
        for id in STEREO_EYE_VIEWS {
            self.drop_texture_view(id);
        }
        self.stereo.masks = [None, None];
    }

    /// Reads the scene depth an eye drew last, as `read_scene_depth` reads the main view's.
    ///
    /// # Errors
    ///
    /// When the eye has not rendered at this size.
    pub fn read_eye_depth(
        &mut self,
        device: &RenderDevice,
        eye: usize,
        width: u32,
        height: u32,
    ) -> Result<Vec<f32>, RenderError> {
        let Some(asset_id) = STEREO_EYE_VIEWS.get(eye).copied() else {
            return Err(RenderError::Readback(format!("no eye {eye}")));
        };
        let Some(mut state) = self.texture_views.remove(&asset_id) else {
            return Err(RenderError::Readback(format!("eye {eye} has not rendered")));
        };
        self.swap_view_state(&mut state);
        let depth = self.read_scene_depth(device, width, height);
        self.swap_view_state(&mut state);
        self.texture_views.insert(asset_id, state);
        depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::{Quat, camera::rh::proj::directx::perspective};

    /// An off-axis frustum in the frame's standard depth, built by hand so the test does not
    /// share the code it checks.
    fn off_axis(left: f32, right: f32, down: f32, up: f32) -> Mat4 {
        let (near, far) = (0.1_f32, 100.0_f32);
        let (w, h) = (right - left, up - down);
        Mat4::from_cols(
            Vec4::new(2.0 / w, 0.0, 0.0, 0.0),
            Vec4::new(0.0, 2.0 / h, 0.0, 0.0),
            Vec4::new((right + left) / w, (up + down) / h, far / (near - far), -1.0),
            Vec4::new(0.0, 0.0, near * far / (near - far), 0.0),
        )
    }

    fn camera(view: Mat4, projection: Mat4) -> EyeCamera {
        EyeCamera {
            view,
            projection,
            cull: projection * view,
        }
    }

    #[test]
    fn an_eye_reads_its_position_and_forward_back_from_the_view() {
        let pose = Mat4::from_rotation_translation(Quat::from_rotation_y(0.6), Vec3::new(1.0, 2.0, 3.0));
        let cam = camera(pose.inverse(), off_axis(-1.0, 1.0, -1.0, 1.0));
        assert!((cam.position() - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-4);
        let expected = Quat::from_rotation_y(0.6) * Vec3::NEG_Z;
        assert!((cam.forward() - expected).length() < 1e-5);
    }

    #[test]
    fn tangents_round_trip_an_off_axis_frustum() {
        let cam = camera(Mat4::IDENTITY, off_axis(-1.2, 0.8, -0.9, 1.1));
        let t = cam.tangents();
        for (got, want) in t.iter().zip([-1.2, 0.8, -0.9, 1.1]) {
            assert!((got - want).abs() < 1e-5, "{t:?}");
        }
    }

    #[test]
    fn the_bounding_field_of_view_holds_every_edge() {
        let fov = 1.1_f32;
        let cam = camera(Mat4::IDENTITY, perspective(fov, 1.0, 0.1, 100.0));
        assert!(
            (cam.bounding_fov_y(1.0) - fov).abs() < 1e-5,
            "a symmetric frustum is itself"
        );
        let wide = camera(Mat4::IDENTITY, off_axis(-2.0, 0.5, -0.5, 0.5));
        let half = wide.bounding_fov_y(1.0) * 0.5;
        assert!(half.tan() >= 2.0 - 1e-5, "the left edge decides");
    }

    #[test]
    fn hidden_area_vertices_land_at_the_near_plane_on_the_projected_points() {
        let projection = off_axis(-1.0, 1.0, -1.0, 1.0);
        let clip = hidden_area_clip_vertices(&[[0.5, -0.5], [1.0, 1.0]], &[0, 1, 9], &projection);
        assert_eq!(clip.len(), 2, "an index out of range is dropped");
        let v = clip[0];
        assert_eq!(v[2], 0.0);
        assert!((v[0] / v[3] - 0.5).abs() < 1e-6 && (v[1] / v[3] + 0.5).abs() < 1e-6);
    }

    #[test]
    fn eye_view_ids_name_their_eye_and_no_other_view() {
        assert_eq!(stereo_eye_of_view(Some(STEREO_EYE_VIEWS[1])), Some(1));
        assert_eq!(stereo_eye_of_view(Some(42)), None);
        assert_eq!(stereo_eye_of_view(None), None);
    }
}
