// SPDX-License-Identifier: MPL-2.0
//! The live preview: evaluates the open graph (or one clip) on the preview rig with the
//! panel's parameter values, and draws the posed skeleton.
//!
//! The pose runs through the same `regia_animation` evaluator the game uses
//! ([`evaluate_graph`]), with the rig's authoring edits applied, so what the preview shows is
//! what PIE will play. The skeleton is drawn as lit bone boxes through the engine's frame
//! renderer (like the material preview) or, without a GPU, as 2D lines in the panel.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use regia_animation::debug::{GraphDebug, NodePath, graph_debug};
use regia_animation::eval::{ClipLibrary, EvalContext, GraphInstance, evaluate_graph};
use regia_animation::graph::AnimGraph;
use regia_animation::params::{ParamValue, ParameterSet};
use regia_animation::pose::Pose;
use regia_animation::skeleton::Skeleton;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_render::{
    DeviceFrameRenderer, DirectionalLightUniform, FrameMesh, FrameScene, LabMaterial, RenderDevice,
};

use super::model::NodeId;
use super::{AnimDocs, PreviewRig};

/// Preview playback state (not undoable: parameter sliders are a test harness, not edits).
#[derive(Clone, Debug)]
pub struct PreviewState {
    /// Parameter values driving the preview.
    pub params: ParameterSet,
    /// Play (advance time) or hold.
    pub playing: bool,
    /// Playback speed multiplier.
    pub speed: f32,
    /// Accumulate root motion into the drawn rig.
    pub apply_root_motion: bool,
    /// Orbit camera: yaw, pitch (radians), distance factor.
    pub orbit: [f32; 3],
    /// The last evaluated local pose.
    pub pose: Pose,
    /// The last debug snapshot of the preview instance.
    pub debug: Option<GraphDebug>,
    /// Model node → runtime path of the compiled preview graph.
    pub paths: BTreeMap<NodeId, NodePath>,
    /// Why the graph cannot preview, when it cannot.
    pub error: Option<String>,
    /// Notifies fired recently (`(seconds ago, name)`), newest last.
    pub fired: Vec<(f32, String)>,
    /// Accumulated root-motion offset (object space).
    pub root_offset: Vec3,
    /// Accumulated root yaw.
    pub root_yaw: f32,
    instance: Option<GraphInstance>,
    built_for: Option<u64>,
    graph: Option<Arc<AnimGraph>>,
    library: Option<(u64, Arc<ClipLibrary>, Arc<Skeleton>)>,
    scratch: Vec<Pose>,
}

impl Default for PreviewState {
    fn default() -> Self {
        Self {
            params: ParameterSet::new(),
            playing: true,
            speed: 1.0,
            apply_root_motion: false,
            orbit: [0.7, 0.35, 1.0],
            pose: Pose::default(),
            debug: None,
            paths: BTreeMap::new(),
            error: None,
            fired: Vec::new(),
            root_offset: Vec3::ZERO,
            root_yaw: 0.0,
            instance: None,
            built_for: None,
            graph: None,
            library: None,
            scratch: Vec::new(),
        }
    }
}

impl PreviewState {
    /// Forgets the compiled graph and playback (rig change).
    pub fn reset(&mut self) {
        self.instance = None;
        self.built_for = None;
        self.graph = None;
        self.library = None;
        self.debug = None;
        self.root_offset = Vec3::ZERO;
        self.root_yaw = 0.0;
        self.fired.clear();
    }

    /// Sets a preview parameter from a value of the declared kind.
    pub fn set_param(&mut self, name: &str, value: ParamValue) {
        match value {
            ParamValue::Float(v) => self.params.set_float(name, v),
            ParamValue::Int(v) => self.params.set_int(name, v),
            ParamValue::Bool(v) => self.params.set_bool(name, v),
            ParamValue::Vec2(v) => self.params.set_vec2(name, v),
            ParamValue::Vec3(v) => self.params.set_vec3(name, v),
            ParamValue::Trigger => self.params.set_trigger(name),
        }
    }

    fn rig_data(
        &mut self,
        rig: &PreviewRig,
        docs: &AnimDocs,
        revision: u64,
    ) -> (Arc<ClipLibrary>, Arc<Skeleton>) {
        if let Some((rev, library, skeleton)) = &self.library
            && *rev == revision
        {
            return (library.clone(), skeleton.clone());
        }
        let library = Arc::new(rig.library(&docs.edits));
        let skeleton = Arc::new(rig.edited_skeleton(&docs.edits));
        self.library = Some((revision, library.clone(), skeleton.clone()));
        (library, skeleton)
    }

    /// Advances the graph preview by `dt` seconds (0 = re-evaluate in place). Rebuilds the
    /// instance when the documents changed (`revision`).
    #[tracing::instrument(target = "regia_editor", skip_all, name = "anim_preview_tick")]
    pub fn tick_graph(&mut self, rig: &PreviewRig, docs: &AnimDocs, revision: u64, dt: f32) {
        if self.built_for != Some(revision) {
            self.built_for = Some(revision);
            match docs.graph.compile() {
                Ok(compiled) => {
                    // Declared parameters get their default unless the slider already set them.
                    let present: Vec<String> = self.params.entries().into_iter().map(|e| e.0).collect();
                    for decl in &compiled.graph.parameters {
                        if !present.contains(&decl.0) && decl.1 != ParamValue::Trigger {
                            self.set_param(&decl.0, decl.1);
                        }
                    }
                    let graph = compiled.graph.triangulated();
                    let same_shape = self.graph.as_ref().is_some_and(|g| g.root == graph.root);
                    if !same_shape {
                        self.instance = Some(GraphInstance::from_node(&graph.root));
                    }
                    self.paths = compiled.paths;
                    self.graph = Some(Arc::new(graph));
                    self.error = None;
                }
                Err(err) => {
                    self.error = Some(err);
                    self.graph = None;
                    self.instance = None;
                    self.debug = None;
                }
            }
        }
        let (Some(graph), Some(mut instance)) = (self.graph.clone(), self.instance.take()) else {
            return;
        };
        let (library, skeleton) = self.rig_data(rig, docs, revision);
        let dt = if self.playing { dt * self.speed } else { 0.0 };
        let sockets = HashMap::new();
        let scratch = std::mem::take(&mut self.scratch);
        let mut ctx = EvalContext::new(
            &skeleton,
            &library,
            &self.params,
            dt,
            Entity::PLACEHOLDER,
            &sockets,
            scratch,
        );
        let mut pose = skeleton.bind_pose_pose();
        let result = evaluate_graph(&graph, &mut instance, &mut ctx, &mut pose);
        let consumed = std::mem::take(&mut ctx.consumed_triggers);
        let (delta_t, delta_r) = ctx.root_motion;
        let fired: Vec<String> = ctx.notifies.iter().map(|n| n.name.clone()).collect();
        let finished = ctx.finish();
        self.scratch = finished.scratch;
        for trigger in consumed {
            self.params.consume_trigger(&trigger);
        }
        // Evaluation errors (a clip or joint the rig lacks) still leave the pose of everything
        // that evaluated before the failing node.
        self.error = result.err().map(|err| err.to_string());
        for entry in &mut self.fired {
            entry.0 += dt;
        }
        self.fired.retain(|e| e.0 < 2.0);
        self.fired.extend(fired.into_iter().map(|n| (0.0, n)));
        if self.apply_root_motion {
            let rotation = Quat::from_rotation_y(self.root_yaw);
            self.root_offset += rotation * delta_t;
            let (yaw, _, _) = delta_r.to_euler(regia_core::glam::EulerRot::YXZ);
            self.root_yaw += yaw;
            if self.root_offset.length() > 6.0 {
                self.root_offset = Vec3::ZERO;
            }
        }
        self.debug = Some(graph_debug(&graph, &instance, &self.params));
        self.instance = Some(instance);
        self.pose = pose;
    }

    /// Samples one clip (with edits) at `time` into the preview pose.
    pub fn sample_clip(&mut self, rig: &PreviewRig, docs: &AnimDocs, revision: u64, clip: &str, time: f32) {
        // The clip panel shows the clip in place: drop leftover graph root motion.
        self.root_offset = Vec3::ZERO;
        self.root_yaw = 0.0;
        let (library, skeleton) = self.rig_data(rig, docs, revision);
        let mut pose = skeleton.bind_pose_pose();
        if let Some(clip) = library.get(clip) {
            clip.sample_pose(time, &mut pose);
            self.error = None;
        } else {
            self.error = Some(format!("the rig has no clip {clip:?}"));
        }
        self.pose = pose;
    }

    /// Object-space joint matrices of the last pose (bind pose when nothing evaluated yet),
    /// with the root-motion offset applied.
    #[must_use]
    pub fn globals(&self, skeleton: &Skeleton) -> Vec<Mat4> {
        let base = Mat4::from_rotation_translation(Quat::from_rotation_y(self.root_yaw), self.root_offset);
        if self.pose.len() == skeleton.len() {
            skeleton.globals(&self.pose, Some(base))
        } else {
            skeleton.globals(&skeleton.bind_pose_pose(), Some(base))
        }
    }
}

/// Bounds of the joint positions: `(centre, radius)`.
#[must_use]
pub fn rig_bounds(globals: &[Mat4]) -> (Vec3, f32) {
    if globals.is_empty() {
        return (Vec3::ZERO, 1.0);
    }
    let mut lo = Vec3::splat(f32::MAX);
    let mut hi = Vec3::splat(f32::MIN);
    for m in globals {
        let p = m.w_axis.truncate();
        lo = lo.min(p);
        hi = hi.max(p);
    }
    let centre = (lo + hi) * 0.5;
    (centre, ((hi - lo).length() * 0.5).max(0.05))
}

/// One bone box (parent joint → joint) as a model matrix, plus the joint index.
#[must_use]
pub fn bone_boxes(skeleton: &Skeleton, globals: &[Mat4], radius: f32) -> Vec<(u32, Mat4)> {
    let mut out = Vec::new();
    for (i, parent) in skeleton.parents.iter().enumerate() {
        let Some(p) = parent.map(|p| p as usize) else {
            continue;
        };
        let (Some(a), Some(b)) = (globals.get(p), globals.get(i)) else {
            continue;
        };
        let a = a.w_axis.truncate();
        let b = b.w_axis.truncate();
        let len = (b - a).length();
        if len < radius * 1e-3 {
            continue;
        }
        let thickness = (len * 0.2).clamp(radius * 0.012, radius * 0.06);
        let rotation = Quat::from_rotation_arc(Vec3::Y, (b - a) / len);
        out.push((
            i as u32,
            Mat4::from_scale_rotation_translation(
                Vec3::new(thickness, len, thickness),
                rotation,
                (a + b) * 0.5,
            ),
        ));
    }
    out
}

/// The orbit camera's eye and target for the rig.
#[must_use]
pub fn orbit_camera(globals: &[Mat4], orbit: [f32; 3]) -> (Vec3, Vec3, f32) {
    let (centre, radius) = rig_bounds(globals);
    let distance = radius * 3.2 * orbit[2].clamp(0.2, 5.0);
    let (yaw, pitch) = (orbit[0], orbit[1].clamp(-1.4, 1.4));
    let eye = centre + Vec3::new(yaw.sin() * pitch.cos(), pitch.sin(), yaw.cos() * pitch.cos()) * distance;
    (eye, centre, radius)
}

/// Renders the posed skeleton (bone boxes, joint cubes, a ground slab) through the engine's
/// frame renderer; `highlight` draws one joint's bone orange. `None` when rendering fails.
#[must_use]
#[tracing::instrument(target = "regia_editor", skip_all, name = "anim_preview_render")]
pub fn render_rig(
    device: &RenderDevice,
    skeleton: &Skeleton,
    globals: &[Mat4],
    highlight: Option<u32>,
    orbit: [f32; 3],
    size: [f32; 2],
) -> Option<egui::ColorImage> {
    static RENDERER: OnceLock<Mutex<DeviceFrameRenderer>> = OnceLock::new();
    static NO_LIGHTS: [regia_render::LabLocalLight; 0] = [];
    const CUBE_KEY: u64 = 0xa417_0001;
    const BONE_MAT: u64 = 0xa417_0002;
    const HIGHLIGHT_MAT: u64 = 0xa417_0003;
    const FLOOR_MAT: u64 = 0xa417_0004;
    const JOINT_MAT: u64 = 0xa417_0005;

    let width = size[0].max(64.0) as u32;
    let height = size[1].max(64.0) as u32;
    let (cube_v, cube_i) = crate::render::cube_mesh();
    let (eye, target, radius) = orbit_camera(globals, orbit);
    let bone = LabMaterial {
        base_color: [0.78, 0.8, 0.86, 1.0],
        roughness: 0.5,
        ..LabMaterial::default()
    };
    let lit = LabMaterial {
        base_color: [1.0, 0.55, 0.15, 1.0],
        roughness: 0.4,
        ..LabMaterial::default()
    };
    let joint = LabMaterial {
        base_color: [0.25, 0.55, 0.95, 1.0],
        roughness: 0.4,
        ..LabMaterial::default()
    };
    let floor = LabMaterial {
        base_color: [0.32, 0.33, 0.35, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    };
    let sky_mat = crate::render::sky_material();
    let sky = crate::render::sky_sphere();
    let mut meshes: Vec<FrameMesh<'_>> = Vec::new();
    crate::render::push_sky(&mut meshes, &sky.0, &sky.1, &sky_mat, eye, radius * 30.0);
    let ground_y = globals
        .iter()
        .map(|m| m.w_axis.y)
        .fold(f32::MAX, f32::min)
        .min(0.0);
    // (model, material slot): 0 bone, 1 highlighted bone, 2 joint, 3 floor.
    let mut parts: Vec<(Mat4, u8)> = vec![(
        Mat4::from_scale_rotation_translation(
            Vec3::new(radius * 4.0, radius * 0.02, radius * 4.0),
            Quat::IDENTITY,
            Vec3::new(target.x, ground_y - radius * 0.01, target.z),
        ),
        3,
    )];
    for (joint_index, model) in bone_boxes(skeleton, globals, radius) {
        parts.push((model, if Some(joint_index) == highlight { 1 } else { 0 }));
    }
    for m in globals {
        let p = m.w_axis.truncate();
        parts.push((
            Mat4::from_scale_rotation_translation(Vec3::splat(radius * 0.035), Quat::IDENTITY, p),
            2,
        ));
    }
    for (object, (model, slot)) in parts.iter().enumerate() {
        let (key, material) = match slot {
            1 => (HIGHLIGHT_MAT, &lit),
            2 => (JOINT_MAT, &joint),
            3 => (FLOOR_MAT, &floor),
            _ => (BONE_MAT, &bone),
        };
        meshes.push(FrameMesh {
            is_static: false,
            mesh_key: CUBE_KEY,
            material_key: key,
            object_key: object as u64 + 2,
            vertices: &cube_v,
            indices: &cube_i,
            material,
            model: model.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
    }
    let scene = FrameScene {
        width,
        height,
        eye: eye.to_array(),
        target: target.to_array(),
        fov_y: std::f32::consts::FRAC_PI_4,
        near: radius * 0.02,
        far: radius * 80.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: true,
        exposure_ev: crate::render::VIEWPORT_EXPOSURE_EV,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: None,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([-0.4, -0.85, -0.35], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: &NO_LIGHTS,
        quality: regia_render::FrameQuality::EDITOR,
        meshes,
    };
    let mut renderer = RENDERER
        .get_or_init(|| Mutex::new(DeviceFrameRenderer::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let result = renderer.get(device).render_offscreen(device, &scene).ok()?;
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [result.width as usize, result.height as usize],
        &result.pixels,
    ))
}

/// Projects object-space points with the orbit camera into `rect` (orthographic-ish
/// perspective), for the painter fallback and joint picking.
#[must_use]
pub fn project(globals: &[Mat4], orbit: [f32; 3], rect: egui::Rect) -> Vec<egui::Pos2> {
    let (eye, target, _) = orbit_camera(globals, orbit);
    let view = regia_core::glam::camera::rh::view::look_at_mat4(eye, target, Vec3::Y);
    let aspect = rect.width() / rect.height().max(1.0);
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(
        std::f32::consts::FRAC_PI_4,
        aspect.max(0.01),
        0.01,
        1000.0,
    );
    let vp = proj * view;
    globals
        .iter()
        .map(|m| {
            let p = vp.project_point3(m.w_axis.truncate());
            egui::pos2(
                rect.center().x + p.x * rect.width() * 0.5,
                rect.center().y - p.y * rect.height() * 0.5,
            )
        })
        .collect()
}

/// Paints the skeleton as lines (the no-GPU fallback, and the joint labels overlay).
pub fn paint_rig(
    painter: &egui::Painter,
    rect: egui::Rect,
    skeleton: &Skeleton,
    globals: &[Mat4],
    orbit: [f32; 3],
    highlight: Option<u32>,
    lines: bool,
) {
    let points = project(globals, orbit, rect);
    if lines {
        painter.rect_filled(rect, 0.0, egui::Color32::from_gray(28));
        for (i, parent) in skeleton.parents.iter().enumerate() {
            let Some(p) = parent.map(|p| p as usize) else {
                continue;
            };
            let colour = if Some(i as u32) == highlight {
                egui::Color32::from_rgb(255, 140, 40)
            } else {
                egui::Color32::from_gray(210)
            };
            if let (Some(a), Some(b)) = (points.get(p), points.get(i)) {
                painter.line_segment([*a, *b], egui::Stroke::new(3.0, colour));
            }
        }
        for p in &points {
            painter.circle_filled(*p, 3.0, egui::Color32::from_rgb(70, 140, 240));
        }
    }
    if let Some(h) = highlight
        && let (Some(p), Some(name)) = (points.get(h as usize), skeleton.joint_names.get(h as usize))
    {
        painter.text(
            *p + egui::vec2(6.0, -6.0),
            egui::Align2::LEFT_BOTTOM,
            name,
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(255, 190, 120),
        );
    }
}

/// The GPU picture the rig preview widget `id` shows, once one has been rendered (ADR-8584).
#[must_use]
pub fn rig_preview_texture(ctx: &egui::Context, id: &str) -> Option<egui::TextureHandle> {
    ctx.data(|d| d.get_temp::<egui::TextureHandle>(egui::Id::new(("regia-rig-preview-texture", id))))
}

/// The rig preview's latest finished render for widget `id` (ADR-8584): the render of this pose
/// is queued on the preview thread (`crate::preview_worker`) and the frame shows the last image
/// that finished, so the UI frame never builds the renderer or compiles its pipelines. `None`
/// until the first render ends (the caller draws the line rig).
fn rig_texture(
    ctx: &egui::Context,
    id: &str,
    device: &RenderDevice,
    (skeleton, globals): (&Skeleton, &[Mat4]),
    (highlight, orbit, size): (Option<u32>, [f32; 3], [f32; 2]),
) -> Option<egui::TextureHandle> {
    use std::hash::{Hash, Hasher};
    let job_id = egui::Id::new(("regia-rig-preview-job", id));
    let texture_id = egui::Id::new(("regia-rig-preview-texture", id));
    let job = ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<crate::preview_worker::AsyncPreview<Option<egui::ColorImage>>>(job_id)
            .clone()
    });
    if let Some((_, Some(image))) = job.take() {
        let texture = ctx.load_texture(id, image, egui::TextureOptions::LINEAR);
        ctx.data_mut(|d| d.insert_temp(texture_id, texture));
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    device.device_id().hash(&mut h);
    skeleton.len().hash(&mut h);
    for m in globals {
        for v in m.to_cols_array() {
            v.to_bits().hash(&mut h);
        }
    }
    highlight.hash(&mut h);
    for v in orbit.iter().chain(&size) {
        v.to_bits().hash(&mut h);
    }
    let (device, skeleton, globals) = (device.clone(), skeleton.clone(), globals.to_vec());
    job.request(h.finish(), Some(ctx), move || {
        render_rig(&device, &skeleton, &globals, highlight, orbit, size)
    });
    ctx.data(|d| d.get_temp::<egui::TextureHandle>(texture_id))
}

/// Draws the preview into `ui` (GPU render when `device` is available, lines otherwise);
/// dragging orbits, scrolling zooms. Returns the joint clicked, if any.
pub fn preview_widget(
    ui: &mut egui::Ui,
    id: &str,
    skeleton: &Skeleton,
    globals: &[Mat4],
    orbit: &mut [f32; 3],
    highlight: Option<u32>,
    device: Option<&RenderDevice>,
) -> Option<u32> {
    let size = ui.available_size().max(egui::vec2(64.0, 64.0));
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    if response.dragged() {
        let d = response.drag_delta();
        orbit[0] -= d.x * 0.01;
        orbit[1] = (orbit[1] + d.y * 0.01).clamp(-1.4, 1.4);
    }
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            orbit[2] = (orbit[2] * (1.0 - scroll * 0.002)).clamp(0.2, 5.0);
        }
    }
    let painter = ui.painter_at(rect);
    let texture = device.and_then(|d| {
        rig_texture(
            ui.ctx(),
            id,
            d,
            (skeleton, globals),
            (highlight, *orbit, [rect.width(), rect.height()]),
        )
    });
    match texture {
        Some(texture) => {
            painter.image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            paint_rig(&painter, rect, skeleton, globals, *orbit, highlight, false);
        }
        None => paint_rig(&painter, rect, skeleton, globals, *orbit, highlight, true),
    }
    if response.clicked()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let points = project(globals, *orbit, rect);
        return points
            .iter()
            .enumerate()
            .map(|(i, p)| (i, p.distance(pointer)))
            .filter(|(_, d)| *d < 12.0)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i as u32);
    }
    None
}

/// The preview's collapsed "Locomotion test bed" section (ADR-8806): walk the gait being
/// authored over a ground preset (`super::locomotion_panel`).
pub fn locomotion_section(ui: &mut egui::Ui, bed: &mut super::locomotion_panel::LocomotionTestBed) {
    let frame = ui.ctx().cumulative_frame_nr();
    egui::CollapsingHeader::new("Locomotion test bed")
        .id_salt("anim-locomotion-test-bed")
        .show(ui, |ui| super::locomotion_panel::test_bed_ui(ui, bed, frame));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anim::model::{NodeKind, Scope};
    use regia_animation::graph::ClipNode;

    #[test]
    fn sampling_a_clip_drops_leftover_root_motion() {
        let rig = PreviewRig::demo();
        let docs = AnimDocs::default();
        let mut state = PreviewState {
            root_offset: Vec3::new(2.0, 0.0, 3.0),
            root_yaw: 1.0,
            ..Default::default()
        };
        state.sample_clip(&rig, &docs, 1, "no_such_clip", 0.0);
        assert_eq!(state.root_offset, Vec3::ZERO);
        assert_eq!(state.root_yaw, 0.0);
    }

    #[test]
    fn graph_preview_evaluates_and_reports_debug() {
        let rig = PreviewRig::demo();
        let mut docs = AnimDocs::default();
        let id = docs
            .graph
            .add_node(
                NodeKind::Clip(ClipNode {
                    clip: "Walk".into(),
                    looping: true,
                    speed: 1.0,
                    sync_group: None,
                }),
                Scope::Root,
                [0.0, 0.0],
            )
            .expect("add");
        let mut state = PreviewState::default();
        state.tick_graph(&rig, &docs, 1, 0.1);
        assert!(state.error.is_some(), "no output yet");
        docs.graph.set_output(&Scope::Root, Some(id)).expect("output");
        state.tick_graph(&rig, &docs, 2, 0.0);
        state.tick_graph(&rig, &docs, 2, 0.25);
        assert!(state.error.is_none(), "{:?}", state.error);
        assert_eq!(state.pose.len(), rig.skeleton.len());
        let debug = state.debug.clone().expect("debug");
        assert!(debug.nodes.iter().any(|n| n.path == "root"));
        assert_eq!(state.paths.get(&id).map(String::as_str), Some("root"));
        // The thigh swings: the pose differs from bind.
        let thigh = rig.skeleton.joint_index("thigh_l").expect("joint") as usize;
        assert!(state.pose.rotations[thigh].angle_between(Quat::IDENTITY) > 0.05);
        let globals = state.globals(&rig.skeleton);
        assert_eq!(globals.len(), rig.skeleton.len());
        let (_, radius) = rig_bounds(&globals);
        assert!(radius > 0.5 && radius < 2.0, "{radius}");
        let boxes = bone_boxes(&rig.skeleton, &globals, radius);
        assert_eq!(boxes.len(), rig.skeleton.len() - 1, "one bone per non-root joint");

        // Clip sampling.
        state.sample_clip(&rig, &docs, 2, "Run", 0.15);
        assert!(state.error.is_none());
        state.sample_clip(&rig, &docs, 2, "Nope", 0.15);
        assert!(state.error.is_some());
    }

    #[test]
    fn rig_renders_offscreen_when_a_gpu_exists() {
        let device = match crate::render::headless_device() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("skip rig_renders_offscreen_when_a_gpu_exists: {err}");
                return;
            }
        };
        let rig = PreviewRig::demo();
        let globals = rig.skeleton.globals(&rig.skeleton.bind_pose_pose(), None);
        let image = render_rig(
            &device,
            &rig.skeleton,
            &globals,
            rig.skeleton.joint_index("shin_l"),
            [0.7, 0.35, 1.0],
            [160.0, 160.0],
        )
        .expect("renders");
        assert_eq!(image.size, [160, 160]);
        // Some orange (the highlighted bone) is visible.
        let orange = image
            .pixels
            .iter()
            .filter(|c| c.r() > 120 && c.r() > c.b().saturating_add(60) && c.r() > c.g().saturating_add(20))
            .count();
        assert!(orange > 4, "highlighted bone visible: {orange}");
    }
}
