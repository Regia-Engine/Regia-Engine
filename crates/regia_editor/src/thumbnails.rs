// SPDX-License-Identifier: MPL-2.0
//! Batch thumbnail renderer (Tincal engine request F8): many small, deterministic renders of
//! models or world entities through one device and one warm frame renderer.
//!
//! [`ThumbnailRenderer::render`] takes a slice of [`ThumbJob`]s and returns one
//! [`egui::ColorImage`] per job, in order. Every job is drawn on the same stage the asset viewer
//! uses (centered, framed by its bounds, a key light, the backdrop sky, the editor quality
//! profile), so two identical jobs give bit-identical pixels. The renderer owns a
//! [`regia_render::DeviceFrameRenderer`]: shaders and pipelines compile for the first job only,
//! and jobs are drawn grouped by size so each render target is allocated once per size.
//!
//! A job names its subject as a [`ThumbSubject::Model`] (an `Arc<LoadedModel>`, no world needed)
//! or a [`ThumbSubject::Entity`] (the entity and its descendants in a world, drawn with
//! [`ThumbnailRenderer::render_in`]). Its view is a [`ViewKind`] (`Top`, `Front` and `Side` look
//! along the axes, `Perspective` is a three-quarter view) or explicit orbit angles. Views are
//! perspective with the subject framed to fill the picture; orthographic panes belong to the
//! viewport.
//!
//! [`ThumbStats`] reports the last call, including the frame-path counters: after a warm-up job
//! of any size, `counters.pipelines` stays zero.

use std::sync::Arc;

use regia_core::math::{Mat4, Vec3};
use regia_ecs::prelude::*;
use regia_render::{
    CpuMaterial, CpuMesh, DebugView, DebugViewMode, DeviceFrameRenderer, DirectionalLightUniform,
    ExtractedCamera, ExtractedFrame, ExtractedMesh, ExtractedTarget, ExtractedView, FramePathCounters,
    FrameQuality, JointMatrix, RenderDevice,
};

use crate::mesh_source::{LoadedModel, pick_owner, resolve_mesh_sources};
use crate::render::{KeepAlive, VIEWPORT_EXPOSURE_EV};
use crate::views::ViewKind;

/// Thumbnails are at least this many pixels on a side.
pub const MIN_SIZE: u32 = 16;
/// Thumbnails are at most this many pixels on a side.
pub const MAX_SIZE: u32 = 2048;

/// Yaw of the perspective view, radians (three-quarter, from the front-right).
pub const PERSPECTIVE_YAW: f32 = 0.6;
/// Pitch of the perspective view, radians.
pub const PERSPECTIVE_PITCH: f32 = 0.35;

/// What a thumbnail shows.
#[derive(Clone)]
pub enum ThumbSubject {
    /// A loaded model, centered at its own bounds.
    Model(Arc<LoadedModel>),
    /// An entity and its descendants (and the model parts of either) in a world. Needs
    /// [`ThumbnailRenderer::render_in`].
    Entity(Entity),
}

impl From<Arc<LoadedModel>> for ThumbSubject {
    fn from(model: Arc<LoadedModel>) -> Self {
        Self::Model(model)
    }
}

impl From<Entity> for ThumbSubject {
    fn from(entity: Entity) -> Self {
        Self::Entity(entity)
    }
}

/// From where a thumbnail looks at its subject.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThumbView {
    /// A viewport pane's direction: `Top`, `Front` and `Side` look along an axis, `Perspective`
    /// is a three-quarter view ([`PERSPECTIVE_YAW`], [`PERSPECTIVE_PITCH`]).
    Kind(ViewKind),
    /// An orbit around the subject, radians. Yaw 0, pitch 0 looks down −Z from +Z (the front).
    Angles {
        /// Rotation about +Y.
        yaw: f32,
        /// Elevation above the horizon.
        pitch: f32,
    },
}

impl ThumbView {
    /// The orbit angles this view stands for.
    #[must_use]
    pub fn angles(self) -> (f32, f32) {
        match self {
            Self::Kind(kind) => kind
                .ortho_angles()
                .unwrap_or((PERSPECTIVE_YAW, PERSPECTIVE_PITCH)),
            Self::Angles { yaw, pitch } => (yaw, pitch),
        }
    }
}

impl From<ViewKind> for ThumbView {
    fn from(kind: ViewKind) -> Self {
        Self::Kind(kind)
    }
}

impl From<(f32, f32)> for ThumbView {
    fn from((yaw, pitch): (f32, f32)) -> Self {
        Self::Angles { yaw, pitch }
    }
}

/// One thumbnail to render.
#[derive(Clone)]
pub struct ThumbJob {
    /// The subject (the request's `model`).
    pub model: ThumbSubject,
    /// The direction to look from.
    pub view: ThumbView,
    /// Width and height of the square picture, pixels (clamped to [`MIN_SIZE`]..=[`MAX_SIZE`]).
    pub size: u32,
    /// [`DebugViewMode::Wireframe`] overlays the triangle edges; every other mode draws lit.
    pub debug: DebugViewMode,
}

impl ThumbJob {
    /// A lit three-quarter view of `model`.
    #[must_use]
    pub fn model(model: Arc<LoadedModel>, size: u32) -> Self {
        Self {
            model: ThumbSubject::Model(model),
            view: ThumbView::Kind(ViewKind::Perspective),
            size,
            debug: DebugViewMode::Lit,
        }
    }

    /// A lit three-quarter view of `entity` (render it with [`ThumbnailRenderer::render_in`]).
    #[must_use]
    pub fn entity(entity: Entity, size: u32) -> Self {
        Self {
            model: ThumbSubject::Entity(entity),
            view: ThumbView::Kind(ViewKind::Perspective),
            size,
            debug: DebugViewMode::Lit,
        }
    }

    /// The same job seen from `view`.
    #[must_use]
    pub fn viewed(mut self, view: impl Into<ThumbView>) -> Self {
        self.view = view.into();
        self
    }

    /// The same job with a debug mode.
    #[must_use]
    pub fn debug(mut self, debug: DebugViewMode) -> Self {
        self.debug = debug;
        self
    }
}

/// What the last [`ThumbnailRenderer::render`] call did.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThumbStats {
    /// Jobs asked for.
    pub jobs: usize,
    /// Jobs that produced a picture.
    pub rendered: usize,
    /// Jobs that produced a blank picture: no geometry, an entity job without a world, or a GPU
    /// failure.
    pub failed: usize,
    /// Distinct sizes drawn (render targets are allocated once per size).
    pub sizes: usize,
    /// Wall time of the call, milliseconds.
    pub cpu_ms: f32,
    /// Frame-path device calls made by the call (pipelines, bind groups, buffers, textures).
    pub counters: FramePathCounters,
}

/// Renders many thumbnails on one device with one warm frame renderer (see the module docs).
pub struct ThumbnailRenderer {
    device: RenderDevice,
    renderer: DeviceFrameRenderer,
    keep: KeepAlive,
    stats: ThumbStats,
}

impl ThumbnailRenderer {
    /// A renderer on `device` (a clone shares the device and queue).
    #[must_use]
    pub fn new(device: &RenderDevice) -> Self {
        Self {
            device: device.clone(),
            renderer: DeviceFrameRenderer::new(),
            keep: KeepAlive::default(),
            stats: ThumbStats::default(),
        }
    }

    /// The device this renderer draws on.
    #[must_use]
    pub fn device(&self) -> &RenderDevice {
        &self.device
    }

    /// Statistics of the last render call.
    #[must_use]
    pub fn stats(&self) -> ThumbStats {
        self.stats
    }

    /// Renders model jobs: one picture per job, in order. An entity job has no world here and
    /// yields a blank picture (use [`Self::render_in`]).
    #[tracing::instrument(target = "regia_editor", skip_all, name = "thumbnails", fields(jobs = jobs.len()))]
    pub fn render(&mut self, jobs: &[ThumbJob]) -> Vec<egui::ColorImage> {
        self.render_inner(None, jobs)
    }

    /// Renders model and entity jobs; entity subjects are looked up in `world`.
    #[tracing::instrument(target = "regia_editor", skip_all, name = "thumbnails", fields(jobs = jobs.len()))]
    pub fn render_in(&mut self, world: &mut World, jobs: &[ThumbJob]) -> Vec<egui::ColorImage> {
        self.render_inner(Some(world), jobs)
    }

    fn render_inner(&mut self, mut world: Option<&mut World>, jobs: &[ThumbJob]) -> Vec<egui::ColorImage> {
        let started = std::time::Instant::now();
        let counters_before = regia_render::frame_path_counters();
        let mut images: Vec<Option<egui::ColorImage>> = vec![None; jobs.len()];
        // Same-size jobs run back to back: the renderer reallocates its targets on a size change.
        let mut order: Vec<usize> = (0..jobs.len()).collect();
        order.sort_by_key(|&i| clamp_size(jobs[i].size));
        // One extraction serves every entity job of the call.
        let mut extracted: Option<ExtractedFrame> = None;
        let mut stats = ThumbStats {
            jobs: jobs.len(),
            ..ThumbStats::default()
        };
        let mut last_size = 0;
        for index in order {
            let job = &jobs[index];
            let size = clamp_size(job.size);
            if size != last_size {
                stats.sizes += 1;
                last_size = size;
            }
            let subject = match &job.model {
                ThumbSubject::Model(model) => Some(Subject::of_model(model)),
                ThumbSubject::Entity(entity) => world
                    .as_deref_mut()
                    .and_then(|world| Subject::of_entity(world, *entity, &mut extracted)),
            };
            let image = subject.and_then(|subject| self.draw(&subject, job, size));
            if image.is_some() {
                stats.rendered += 1;
            } else {
                stats.failed += 1;
            }
            images[index] =
                Some(image.unwrap_or_else(|| {
                    egui::ColorImage::filled([size as usize; 2], egui::Color32::TRANSPARENT)
                }));
        }
        stats.cpu_ms = started.elapsed().as_secs_f32() * 1000.0;
        stats.counters = regia_render::frame_path_counters().since(counters_before);
        self.stats = stats;
        images.into_iter().flatten().collect()
    }

    fn draw(&mut self, subject: &Subject, job: &ThumbJob, size: u32) -> Option<egui::ColorImage> {
        let (yaw, pitch) = job.view.angles();
        let (min, max) = subject.bounds;
        let center = (min + max) * 0.5;
        let radius = ((max - min).length() * 0.5).max(0.05);
        let fov = 40f32.to_radians();
        let distance = radius / (fov * 0.5).sin() * 1.15;
        let dir = Vec3::new(pitch.cos() * yaw.sin(), pitch.sin(), pitch.cos() * yaw.cos());
        let eye = center + dir * distance;

        let mut frame = ExtractedFrame {
            meshes: subject.meshes.clone(),
            directional: Some(DirectionalLightUniform::new(
                [-0.45, -0.75, -0.5],
                [1.0, 0.97, 0.92],
                100_000.0,
            )),
            quality: FrameQuality::EDITOR,
            ..ExtractedFrame::default()
        };
        frame.views = vec![ExtractedView {
            camera: ExtractedCamera {
                eye: eye.to_array(),
                target: center.to_array(),
                fov_y: fov,
                near: (distance - radius * 2.0).max(distance * 0.01),
                far: distance * 40.0,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post: regia_render::PostSettings::default(),
        }];
        // Pin every Arc the renderer's address-keyed caches will see (see `KeepAlive`).
        self.keep.hold(&frame, 1);

        let mut scene = frame.frame_scene(0, size, size);
        scene.post = None;
        scene.exposure_ev = VIEWPORT_EXPOSURE_EV;
        scene.depth_prepass = false;
        scene.directional_shadows = false;
        let sky_material = crate::render::sky_material();
        let sky = crate::render::sky_sphere();
        crate::render::push_sky(
            &mut scene.meshes,
            &sky.0,
            &sky.1,
            &sky_material,
            eye,
            distance * 20.0,
        );

        let wireframe = job.debug == DebugViewMode::Wireframe;
        let renderer = self.renderer.get(&self.device);
        if wireframe {
            let view = DebugView {
                mode: DebugViewMode::Wireframe,
                ..DebugView::default()
            };
            renderer.set_gizmos(Some(regia_render::wireframe_frame(
                &frame.gizmos,
                &frame.meshes,
                &view,
            )));
        }
        let result = renderer.render_offscreen(&self.device, &scene);
        if wireframe {
            renderer.set_gizmos(None);
        }
        match result {
            Ok(result) => Some(egui::ColorImage::from_rgba_unmultiplied(
                [result.width as usize, result.height as usize],
                &result.pixels,
            )),
            Err(err) => {
                tracing::warn!(target: "regia_editor", "thumbnail render failed: {err}");
                None
            }
        }
    }
}

fn clamp_size(size: u32) -> u32 {
    size.clamp(MIN_SIZE, MAX_SIZE)
}

/// What one job draws: extracted meshes and their world bounds.
struct Subject {
    meshes: Vec<ExtractedMesh>,
    bounds: (Vec3, Vec3),
}

impl Subject {
    fn of_model(model: &LoadedModel) -> Self {
        let meshes = model
            .parts
            .iter()
            .enumerate()
            .map(|(index, part)| extracted_part(&part.mesh, &part.material, index as u64))
            .collect();
        Self {
            meshes,
            bounds: (model.aabb_min, model.aabb_max),
        }
    }

    fn of_entity(world: &mut World, entity: Entity, cache: &mut Option<ExtractedFrame>) -> Option<Self> {
        world.get_entity(entity).ok()?;
        let frame = cache.get_or_insert_with(|| {
            // Poses and model parts must be current, as in the viewport.
            regia_scene::prelude::propagate_transforms(world);
            resolve_mesh_sources(world);
            crate::render::extract(world)
        });
        let meshes: Vec<ExtractedMesh> = frame
            .meshes
            .iter()
            .filter(|mesh| belongs_to(world, pick_owner(world, mesh.entity), entity))
            .cloned()
            .collect();
        let bounds = world_bounds(&meshes)?;
        Some(Self { meshes, bounds })
    }
}

/// A model part as an extracted mesh at the origin.
fn extracted_part(mesh: &Arc<CpuMesh>, material: &Arc<CpuMaterial>, index: u64) -> ExtractedMesh {
    ExtractedMesh {
        is_static: false,
        entity: Entity::PLACEHOLDER,
        mesh: Arc::clone(mesh),
        material: Arc::clone(material),
        layer: 0,
        model: Mat4::IDENTITY.to_cols_array_2d(),
        joints: Vec::new(),
        decal_layer: 0,
        uv1: None,
        shadow: regia_render::ShadowCast::All,
        draw_indices: None,
        lod_key: 0,
        object_key_xor: 0x00ed_17b0 + index,
        gi_proxy: true,
        lod_dither: 0.0,
    }
}

/// Whether `entity` is `root` or one of its descendants.
fn belongs_to(world: &World, entity: Entity, root: Entity) -> bool {
    let mut current = entity;
    // The depth bound guards a malformed parent cycle.
    for _ in 0..4096 {
        if current == root {
            return true;
        }
        match world.get::<ChildOf>(current) {
            Some(parent) => current = parent.parent(),
            None => return false,
        }
    }
    false
}

/// World-space bounds of the meshes' vertices (skinned vertices where the palette puts them).
fn world_bounds(meshes: &[ExtractedMesh]) -> Option<(Vec3, Vec3)> {
    let mut min = Vec3::splat(f32::MAX);
    let mut max = Vec3::splat(f32::MIN);
    for extracted in meshes {
        let model = Mat4::from_cols_array_2d(&extracted.model);
        let skinned = !extracted.joints.is_empty();
        for vertex in &extracted.mesh.vertices {
            let point = if skinned {
                let pick = |i: usize| {
                    extracted
                        .joints
                        .get(vertex.joints[i] as usize)
                        .copied()
                        .unwrap_or(JointMatrix::identity())
                };
                Vec3::from_array(regia_render::skin_position(
                    vertex.position,
                    &[pick(0), pick(1), pick(2), pick(3)],
                    vertex.weights,
                ))
            } else {
                model.transform_point3(Vec3::from_array(vertex.position))
            };
            min = min.min(point);
            max = max.max(point);
        }
    }
    (min.x <= max.x).then_some((min, max))
}
