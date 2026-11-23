// SPDX-License-Identifier: MPL-2.0
//! Viewport rendering through the runtime path (`docs/specs/19-editor.md` §§ 1–3).
//!
//! The viewport draws the edited world the way the game does: [`resolve_mesh_sources`] turns
//! level mesh references into [`regia_render::MeshRenderer`]s, the runtime extraction system
//! ([`regia_render::extract_frame`]) collects meshes, materials (graph materials included),
//! lights, environment, atmosphere and fog into an [`ExtractedFrame`], and the frame renders
//! with the world's own [`RuntimeRenderer`] — so whatever the project installed on it (the
//! terrain's shared draw with its scatter, the VFX translucent layer with decals and
//! particles) draws here too. Without one, a viewport-owned renderer stands in.
//!
//! The editor only swaps the camera (its own, or a top/front/side telephoto from
//! [`crate::views`]) and the quality preset ([`ViewportQuality`]: the cheap stable editor
//! profile by default, or the game's / any named profile). Editor additions are kept apart and
//! switch off in game view: lit proxy cubes for entities with nothing to draw, a backdrop sky
//! when the preset has no atmosphere, a neutral preview sun when the level has none. In play,
//! a possessed viewport renders the game's own camera views untouched; with several window
//! cameras (split-screen players) it composites them into their viewport rects, as the game's
//! own window does (ADR-7395).
//!
//! [`render_pane`] returns the picture, the camera it used, the entity-id buffer for picking
//! and the selection outline ([`crate::picking`]), and the frame's statistics.

use std::sync::{Arc, Mutex, OnceLock};

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::{DetectChanges, Resource, World, bevy_ecs};
use regia_render::{
    CpuMesh, DeviceFrameRenderer, DirectionalLightUniform, DrawStats, ExtractedCamera, ExtractedFrame,
    ExtractedTarget, ExtractedView, FrameMesh, FramePathCounters, FrameQuality, FrameRenderer, FrameTarget,
    GpuTimings, LabMaterial, LabVertex, LevelColorLut, MeshRenderer, OffscreenTarget, RenderDevice,
    RenderError, RuntimeRenderer, pixel_viewport,
};
use regia_scene::prelude::Transform;

use crate::mesh_source::{pick_owner, resolve_mesh_sources, world_matrix};
use crate::picking::{IdBuffer, IdDraw};
use crate::viewport::EditorCamera;
use crate::views::{ViewCamera, ViewKind, ViewportLayout, ViewportQuality};

/// The headless GPU device of an editor preview, trace or agent render: a new one per call
/// ([`RenderDevice::headless`]). In this crate's unit tests it is the process's one shared device
/// ([`RenderDevice::shared_headless`]): the lib's tests run on parallel threads, and creating or
/// destroying a device while another thread names an object races the Vulkan loader's lists
/// (ADR-2113; ADR-9602: the lib died with SIGSEGV in `libvulkan` on the Intel Arc pass).
pub(crate) fn headless_device() -> Result<RenderDevice, regia_render::RenderError> {
    #[cfg(test)]
    {
        RenderDevice::shared_headless()
    }
    #[cfg(not(test))]
    {
        RenderDevice::headless()
    }
}

/// A viewport-owned renderer and the assets its caches are keyed by.
#[derive(Default)]
struct PaneRenderer {
    renderer: DeviceFrameRenderer,
    keep: KeepAlive,
}

/// Viewport-owned renderers, one per pane, for worlds without a [`RuntimeRenderer`]
/// (pipelines, shaders and IBL maps compile once; device-scoped so a new device rebuilds).
fn pane_renderer(pane: usize) -> &'static Mutex<PaneRenderer> {
    static RENDERERS: OnceLock<[Mutex<PaneRenderer>; 4]> = OnceLock::new();
    let all = RENDERERS.get_or_init(|| std::array::from_fn(|_| Mutex::new(PaneRenderer::default())));
    &all[pane.min(3)]
}

/// Viewport-owned renderers of [`crate::viewport_instance::ViewportInstance`] panes, keyed by
/// (instance id, pane), so each instance keeps its own temporal history and caches.
type InstanceRenderers = std::collections::HashMap<(u64, usize), Arc<Mutex<PaneRenderer>>>;

fn instance_renderers() -> &'static Mutex<InstanceRenderers> {
    static RENDERERS: OnceLock<Mutex<InstanceRenderers>> = OnceLock::new();
    RENDERERS.get_or_init(|| Mutex::new(InstanceRenderers::new()))
}

fn instance_renderer(id: u64, pane: usize) -> Arc<Mutex<PaneRenderer>> {
    let mut all = instance_renderers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(all.entry((id, pane.min(3))).or_default())
}

/// Frees the viewport-owned renderers of instance `id` (its GPU targets, pipelines and caches).
/// [`crate::viewport_instance::ViewportInstances::remove`] calls it; a later draw of the id
/// builds fresh ones.
pub fn release_instance_renderers(id: u64) {
    instance_renderers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|(owner, _), _| *owner != id);
    warmed_instances()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|(owner, _, _)| *owner != id);
}

/// Instance pane renderers a [`warm_instance_renderer_job`] built: (instance id, pane, device id).
fn warmed_instances() -> &'static Mutex<std::collections::HashSet<(u64, usize, u64)>> {
    static WARMED: OnceLock<Mutex<std::collections::HashSet<(u64, usize, u64)>>> = OnceLock::new();
    WARMED.get_or_init(Default::default)
}

/// Whether instance `id`'s pane `pane` renderer was warmed for `device` on the preview thread
/// ([`warm_instance_renderer_job`], ADR-8584). Never waits for a warm in progress.
pub(crate) fn instance_renderer_warm(id: u64, pane: usize, device: &RenderDevice) -> bool {
    warmed_instances()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&(id, pane.min(3), device.device_id()))
}

/// A job for the preview thread (ADR-8584) that builds instance `id`'s pane renderer for
/// `device` and compiles what `world`'s frame draws (`FrameRenderer::warm_extracted` on the frame
/// extracted now, or the engine's canonical frame when it has no window view), so the instance's
/// first pane render on the UI thread finds its pipelines built. The extraction runs here, on the
/// calling thread; the job only renders.
pub(crate) fn warm_instance_renderer_job(
    device: &RenderDevice,
    world: &mut World,
    id: u64,
    pane: usize,
) -> impl FnOnce() + Send + 'static {
    let _span = tracing::info_span!(target: "regia_editor", "viewport_instance_warm_extract").entered();
    let frame = extract(world);
    let device = device.clone();
    let cell = instance_renderer(id, pane);
    let pane = pane.min(3);
    move || {
        let _span = tracing::info_span!(target: "regia_editor", "viewport_instance_warm").entered();
        let started = std::time::Instant::now();
        let before = regia_render::pipeline_cache::pipelines_created_on_this_thread();
        {
            let mut scope = cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let renderer = scope.renderer.get(&device);
            let format = wgpu::TextureFormat::Rgba8Unorm;
            let warmed = if frame.window_views().is_empty() {
                renderer.warm_extracted(
                    &device,
                    &regia_render::pipeline_warm::canonical_frame(),
                    format,
                    true,
                )
            } else {
                renderer.warm_extracted(&device, &frame, format, true)
            };
            if let Err(err) = warmed {
                // The UI's first render builds what the warm could not, as before the warm.
                tracing::warn!(target: "regia_editor", "viewport instance warm failed: {err}");
            }
        }
        tracing::info!(
            target: "regia_editor",
            "viewport instance {id:#x} pane {pane} warmed: {} pipelines in {:.0} ms",
            regia_render::pipeline_cache::pipelines_created_on_this_thread() - before,
            started.elapsed().as_secs_f64() * 1000.0
        );
        warmed_instances()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((id, pane, device.device_id()));
    }
}

/// Which viewport-owned renderer a pane draws with when the world has no [`RuntimeRenderer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RendererSlot {
    /// The main viewport's pane (0–3).
    Pane(usize),
    /// An instance's pane: (instance id, pane).
    Instance(u64, usize),
}

/// Holds every mesh and material `Arc` a renderer has seen until well after the renderer's
/// own cache evicts it.
///
/// The frame renderer keys its GPU caches by `Arc` address. A pane renderer outlives the
/// worlds it draws (levels reopen, play worlds come and go, materials reload), so a freed
/// asset's address could be reused by a new one, which would then draw the stale GPU entry
/// (wrong geometry or material). Pinning the `Arc`s past the eviction window makes an
/// address unique for as long as any cache can remember it.
#[derive(Resource, Default)]
pub(crate) struct KeepAlive {
    frame: u64,
    swept: u64,
    held: std::collections::HashMap<u64, (Arc<dyn std::any::Any + Send + Sync>, u64)>,
}

impl KeepAlive {
    /// Renderer frames after which an unused entry is released: the renderer's eviction
    /// window (600) plus its sweep interval (256), with margin.
    const RELEASE_AFTER: u64 = 1_024;

    /// Pins the frame's assets; `frames` is how many renderer frames this call will run.
    pub(crate) fn hold(&mut self, extracted: &ExtractedFrame, frames: u64) {
        self.frame += frames.max(1);
        let frame = self.frame;
        for mesh in &extracted.meshes {
            let mesh_arc: Arc<dyn std::any::Any + Send + Sync> = mesh.mesh.clone();
            let material_arc: Arc<dyn std::any::Any + Send + Sync> = mesh.material.clone();
            self.held
                .insert(Arc::as_ptr(&mesh.mesh) as u64, (mesh_arc, frame));
            self.held
                .insert(Arc::as_ptr(&mesh.material) as u64, (material_arc, frame));
        }
        if frame - self.swept >= 64 {
            self.swept = frame;
            self.held
                .retain(|_, (_, last)| frame.saturating_sub(*last) <= Self::RELEASE_AFTER);
        }
    }
}

/// EV100 the editor profile pins: a sun-lit studio look, deterministic (the editor must not
/// adapt exposure between frames). Other presets pin the level's exposure band centre, or
/// this value when the level has none.
pub const VIEWPORT_EXPOSURE_EV: f32 = 14.5;

/// The preview sun's travel direction (the M10 viewport's key light).
const PREVIEW_SUN: [f32; 3] = [-0.577, -0.577, -0.577];

/// GPU cache keys for the proxy cube, its material and the backdrop sky (stable across frames).
const PROXY_CUBE_KEY: u64 = 0xed17_1001;
const PROXY_MATERIAL_KEY: u64 = 0xed17_1002;
const SKY_MESH_KEY: u64 = 0xed17_1004;
const SKY_MATERIAL_KEY: u64 = 0xed17_1005;
const SKY_OBJECT_KEY: u64 = 0xed17_1006;
/// Proxy object keys are the entity bits with this salt, so an entity that later gains a real
/// mesh gets a fresh object cache entry.
const PROXY_OBJECT_SALT: u64 = 0x9e37_79b9_7f4a_7c15;

const TERRAIN_MESH_KEY: u64 = 0xed17_2000_0000_0000;
const TERRAIN_MATERIAL_KEY: u64 = 0xed17_1007;
/// Vertices per side of a terrain's viewport preview mesh.
const TERRAIN_PREVIEW_SIDE: u32 = 256;

/// A terrain's preview mesh (world-space vertices with layer colours, indices).
type TerrainMesh = Arc<(Vec<LabVertex>, Vec<u32>)>;

/// The sculpt previews of one world's terrains (a world resource, so worlds never share them).
#[derive(Resource, Default)]
struct TerrainPreviewCache {
    /// Source of fresh GPU mesh keys: every rebuilt mesh gets a new one, so the renderer
    /// uploads it instead of drawing the cached geometry of the previous version.
    next_key: u64,
    entries: std::collections::HashMap<Entity, TerrainPreview>,
}

struct TerrainPreview {
    /// (edit generation, terrain change tick, paint change tick) the mesh was built from.
    stamp: (u64, u32, u32),
    key: u64,
    mesh: TerrainMesh,
}

/// Preview meshes of every visible terrain as `(mesh key, object key, mesh)`. Cached per world
/// and rebuilt when the terrain or its paint changes (a stroke rebuilds once per frame batch).
fn terrain_meshes(world: &mut World) -> Vec<(u64, u64, TerrainMesh)> {
    let mut cache = world.remove_resource::<TerrainPreviewCache>().unwrap_or_default();
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for entity_ref in world.iter_entities() {
        let Some(terrain) = entity_ref.get_ref::<regia_terrain::Terrain>() else {
            continue;
        };
        let entity = entity_ref.id();
        // Edit in place context stays drawn (dimmed, P-160).
        if crate::visibility::is_hidden(world, entity) && !crate::prefab_edit::is_drawn_context(world, entity)
        {
            continue;
        }
        let paint = entity_ref.get_ref::<regia_terrain::TerrainPaint>();
        let stamp = (
            entity_ref
                .get::<regia_terrain::TerrainEdits>()
                .map_or(0, regia_terrain::TerrainEdits::generation),
            terrain.last_changed().get(),
            paint.as_ref().map_or(0, |p| p.last_changed().get()),
        );
        let fresh = cache.entries.get(&entity).is_some_and(|e| e.stamp == stamp);
        if !fresh {
            cache.next_key += 1;
            let mesh = Arc::new(crate::terrain_tool::preview_mesh(
                &terrain,
                paint.as_deref(),
                TERRAIN_PREVIEW_SIDE,
            ));
            let key = TERRAIN_MESH_KEY ^ cache.next_key;
            cache.entries.insert(entity, TerrainPreview { stamp, key, mesh });
        }
        if let Some(entry) = cache.entries.get(&entity) {
            out.push((
                entry.key,
                TERRAIN_MESH_KEY ^ entity.to_bits(),
                Arc::clone(&entry.mesh),
            ));
        }
        seen.push(entity);
    }
    cache.entries.retain(|entity, _| seen.contains(entity));
    world.insert_resource(cache);
    out
}

/// The terrain preview material: white, rough; the layer colours ride on the vertices.
fn terrain_material() -> LabMaterial {
    LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        metallic: 0.0,
        roughness: 0.9,
        ..LabMaterial::default()
    }
}

/// The proxy material: neutral grey.
fn proxy_material() -> LabMaterial {
    LabMaterial {
        base_color: [0.75, 0.75, 0.75, 1.0],
        metallic: 0.0,
        roughness: 0.6,
        ..LabMaterial::default()
    }
}

/// The shared proxy cube (uploaded once, posed per entity).
fn proxy_cube() -> &'static Arc<CpuMesh> {
    static CUBE: OnceLock<Arc<CpuMesh>> = OnceLock::new();
    CUBE.get_or_init(|| {
        let (vertices, indices) = cube_mesh();
        Arc::new(CpuMesh { vertices, indices })
    })
}

/// Unlit white: the sky dome's vertex colours are the gradient.
pub(crate) fn sky_material() -> LabMaterial {
    LabMaterial {
        base_color: [1.0, 1.0, 1.0, 1.0],
        metallic: 0.0,
        roughness: 1.0,
        unlit: true,
        ..LabMaterial::default()
    }
}

/// Display-referred sky gradient. Matches `sky_color` in `utils.wgsl` / `ibl.rs`
/// (horizon, zenith, ground; `1 − exp(−k|y|)`).
pub(crate) fn sky_color(dir: Vec3) -> [f32; 3] {
    let t = dir.y;
    let horizon = [0.78, 0.85, 0.95];
    let zenith = [0.15, 0.25, 0.55];
    let ground = [0.12, 0.10, 0.08];
    let (a, b, f) = if t > 0.0 {
        (horizon, zenith, 1.0 - (-t * 3.0).exp())
    } else {
        (horizon, ground, 1.0 - (t * 2.0).exp())
    };
    [
        a[0] + (b[0] - a[0]) * f,
        a[1] + (b[1] - a[1]) * f,
        a[2] + (b[2] - a[2]) * f,
    ]
}

/// Inward unit sphere (the camera sits at its centre). Vertex colour is the sky
/// in that outward direction; winding faces the centre so back-face culling
/// keeps the shell visible from inside.
pub(crate) fn sky_sphere() -> &'static (Vec<LabVertex>, Vec<u32>) {
    static MESH: OnceLock<(Vec<LabVertex>, Vec<u32>)> = OnceLock::new();
    MESH.get_or_init(|| build_sky_sphere(16, 12))
}

fn build_sky_sphere(rings: usize, stacks: usize) -> (Vec<LabVertex>, Vec<u32>) {
    let mut vertices: Vec<LabVertex> = Vec::with_capacity((rings + 1) * (stacks + 1));
    let mut indices: Vec<u32> = Vec::with_capacity(rings * stacks * 6);
    for stack in 0..=stacks {
        let phi = std::f32::consts::PI * stack as f32 / stacks as f32;
        let (sin_phi, cos_phi) = phi.sin_cos();
        for ring in 0..=rings {
            let theta = std::f32::consts::TAU * ring as f32 / rings as f32;
            let (sin_theta, cos_theta) = theta.sin_cos();
            let dir = Vec3::new(sin_phi * cos_theta, cos_phi, sin_phi * sin_theta);
            let color = sky_color(dir);
            vertices.push(LabVertex {
                position: [dir.x, dir.y, dir.z],
                normal: [-dir.x, -dir.y, -dir.z],
                uv: [ring as f32 / rings as f32, stack as f32 / stacks as f32],
                tangent: [cos_theta, 0.0, -sin_theta, 1.0],
                color: [color[0], color[1], color[2], 1.0],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    for stack in 0..stacks {
        for ring in 0..rings {
            let a = (stack * (rings + 1) + ring) as u32;
            let b = a + 1;
            let c = a + (rings + 1) as u32;
            let d = c + 1;
            // Reversed from the outward preview sphere: front faces point inward.
            indices.extend_from_slice(&[a, d, b, a, c, d]);
        }
    }
    (vertices, indices)
}

/// A sky dome of `radius` metres centred on `eye`.
pub(crate) fn push_sky<'a>(
    meshes: &mut Vec<FrameMesh<'a>>,
    vertices: &'a [LabVertex],
    indices: &'a [u32],
    material: &'a LabMaterial,
    eye: Vec3,
    radius: f32,
) {
    let model = Mat4::from_scale_rotation_translation(Vec3::splat(radius), Quat::IDENTITY, eye);
    meshes.push(FrameMesh {
        is_static: false,
        mesh_key: SKY_MESH_KEY,
        material_key: SKY_MATERIAL_KEY,
        object_key: SKY_OBJECT_KEY,
        vertices,
        indices,
        material,
        model: model.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: regia_render::ShadowCast::All,
        uv1: &[],
        lod_dither: 0.0,
    });
}

/// One pane to render.
#[derive(Debug, Clone)]
pub struct PaneRequest {
    /// Pane index (0–3): picks the viewport-owned renderer when the world has none.
    pub pane: usize,
    /// Which way the pane looks.
    pub kind: ViewKind,
    /// The pane's editor camera.
    pub camera: EditorCamera,
    /// Output width, pixels.
    pub width: u32,
    /// Output height, pixels.
    pub height: u32,
    /// Also render the entity-id buffer (picking, outline, box select).
    pub ids: bool,
    /// Frames to render before the kept one (temporal presets converge; 0 = one frame).
    pub warm_frames: u32,
    /// Debug view mode. [`regia_render::DebugViewMode::Wireframe`] overlays the meshes' edges
    /// on the lit frame; `UvChecker`, `TexelDensity` and `MaterialId` replace the surface
    /// shading (the material ID legend comes back in [`PaneInfo::debug_legend`]); the other
    /// modes render as lit (see [`regia_render::DebugView`]).
    pub debug_view: regia_render::DebugViewMode,
    /// A reference image drawn in the pane (`None` costs nothing; see [`crate::reference`]).
    pub reference: Option<crate::reference::ReferenceImage>,
}

/// What one pane render produced besides the picture.
#[derive(Debug, Clone, Default)]
pub struct PaneInfo {
    /// The material ID legend of this frame (`MaterialId` mode; empty otherwise), sorted by name.
    pub debug_legend: Vec<regia_render::LegendEntry>,
    /// The quality preset's stable name.
    pub quality: &'static str,
    /// The frame-quality profile name the renderer reports (`editor`, `custom`, …).
    pub profile: &'static str,
    /// Temporal upscaling was on.
    pub taau: bool,
    /// The physical (LUT) atmosphere drew the sky (else the editor backdrop).
    pub atmosphere: bool,
    /// The pane rendered through the play world's game camera (possessed).
    pub possessed: bool,
    /// The level has no directional light and the preview sun stood in.
    pub preview_sun: bool,
    /// Mesh instances extracted from the world.
    pub meshes: usize,
    /// Proxy cubes drawn for entities with nothing renderable.
    pub proxies: usize,
    /// Local (point/spot) lights.
    pub local_lights: usize,
    /// The renderer is the world's [`RuntimeRenderer`] (project hooks: terrain, VFX).
    pub runtime_renderer: bool,
    /// Draw statistics of the kept frame.
    pub draws: DrawStats,
    /// GPU pass timings (zero without timestamp queries or stats off).
    pub gpu: GpuTimings,
    /// CPU wall time of the render call, ms.
    pub cpu_ms: f32,
    /// Frame-path device calls made by this render (pipelines, bind groups, polls, submits).
    pub counters: FramePathCounters,
    /// The internal render size, pixels (the pane size scaled by the quality preset and the
    /// pane's screen percentage; ADR-7770).
    pub render_size: [u32; 2],
    /// The pane's screen percentage (100 = the pane's pixels).
    pub screen_percentage: u16,
    /// The EV100 the frame is pinned to (`None` when the frame's own auto exposure runs: game
    /// view at editor quality, or a possessed pane).
    pub exposure_ev: Option<f32>,
    /// The pane's view mode name ([`crate::views::ViewMode::name`] of the request's debug view
    /// and the pane's mode).
    pub view_mode: &'static str,
}

/// A rendered pane.
#[derive(Clone)]
pub struct PaneFrame {
    /// The picture (no overlays).
    pub image: egui::ColorImage,
    /// The camera the picture was rendered with (overlays and picking use it).
    pub view: ViewCamera,
    /// The entity-id buffer, when requested.
    pub ids: Option<IdBuffer>,
    /// Statistics and the settings in effect.
    pub info: PaneInfo,
}

/// Marks a play-in-editor world (possession applies only there).
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct PlayWorld;

/// How many pictures of a play world were rendered (viewport panes and the play window together;
/// ADR-7283: with the play window open the viewport draws none, so play renders once per frame).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PlayWorldRenders(u64);

/// The pictures rendered of `world` so far when it is a play world (0 for any other world).
#[must_use]
pub fn play_world_renders(world: &World) -> u64 {
    world.get_resource::<PlayWorldRenders>().map_or(0, |r| r.0)
}

/// Entities drawn as proxy cubes: user content with a transform and nothing renderable.
pub(crate) fn proxy_entities(world: &World) -> Vec<(Entity, Mat4)> {
    let mut out = Vec::new();
    for entity_ref in world.iter_entities() {
        let entity = entity_ref.id();
        // Edit in place context keeps its cube, dimmed (P-160).
        if (crate::visibility::is_hidden(world, entity)
            && !crate::prefab_edit::is_drawn_context(world, entity))
            || entity_ref.get::<Transform>().is_none()
            || entity_ref.get::<MeshRenderer>().is_some()
            || crate::mesh_source::has_resolved_parts(world, entity)
            || entity_ref.get::<regia_terrain::Terrain>().is_some()
            || entity_ref.get::<crate::blockout::BlockoutBrush>().is_some()
            || crate::overlays::icon_of(world, entity).is_some()
            || is_user_hidden(world, entity)
        {
            continue;
        }
        if let Some(model) = world_matrix(world, entity) {
            out.push((entity, model));
        }
    }
    out
}

/// The object key of `entity`'s proxy cube draw.
pub(crate) fn proxy_object_key(entity: Entity) -> u64 {
    entity.to_bits() ^ PROXY_OBJECT_SALT
}

/// The render overrides of the proxy cubes, keyed like their draws ([`proxy_object_key`]).
pub(crate) fn proxy_overrides(
    world: &World,
    proxies: &[(Entity, Mat4)],
) -> Vec<(u64, regia_render::RenderOverride)> {
    proxies
        .iter()
        .filter_map(|(entity, _)| {
            regia_render::resolve_render_override(world, *entity)
                .map(|over| (proxy_object_key(*entity), over))
        })
        .collect()
}

/// The scene's `Visibility::Hidden` (outliner eye toggle), or the editor-only hide (H, isolate;
/// [`crate::view_state::is_view_hidden`], ADR-7770).
fn is_user_hidden(world: &World, entity: Entity) -> bool {
    world.get::<regia_scene::layers::LayerParked>(entity).is_some()
        || matches!(
            world.get::<regia_scene::prelude::Visibility>(entity),
            Some(regia_scene::prelude::Visibility::Hidden)
        )
        || crate::view_state::is_view_hidden(world, entity)
}

/// The plain materials of the lighting-only and wireframe view modes (ADR-7770): `[surface][double
/// sided]`, shared so the renderer's material cache keeps one entry each.
fn plain_materials() -> &'static [[Arc<LabMaterial>; 2]; 2] {
    static MATERIALS: OnceLock<[[Arc<LabMaterial>; 2]; 2]> = OnceLock::new();
    MATERIALS.get_or_init(|| {
        let pair = |material: LabMaterial| {
            let double = LabMaterial {
                double_sided: true,
                ..material.clone()
            };
            [Arc::new(material), Arc::new(double)]
        };
        [
            // Clay: a lit mid-gray rough dielectric, so only the light varies.
            pair(LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                metallic: 0.0,
                roughness: 0.8,
                ..LabMaterial::default()
            }),
            // Flat: an unlit dark surface under the wires.
            pair(LabMaterial {
                base_color: [0.06, 0.06, 0.07, 1.0],
                unlit: true,
                ..LabMaterial::default()
            }),
        ]
    })
}

/// `meshes` with every material swapped for the plain `surface` (keeps each mesh's double-sided
/// flag; the swapped material reads no second UV set).
fn plain_surface_meshes(
    meshes: &[regia_render::ExtractedMesh],
    surface: crate::views::PlainSurface,
) -> Vec<regia_render::ExtractedMesh> {
    let set = &plain_materials()[match surface {
        crate::views::PlainSurface::Clay => 0,
        crate::views::PlainSurface::Flat => 1,
    }];
    meshes
        .iter()
        .map(|mesh| {
            let mut twin = mesh.clone();
            twin.material = Arc::clone(&set[usize::from(mesh.material.double_sided)]);
            twin.uv1 = None;
            twin
        })
        .collect()
}

/// The EV100 the pane's Exposure menu pins (`None` = the automatic editor exposure; ADR-7770).
fn pane_exposure_ev(world: &World, exposure: crate::views::ViewportExposure) -> Option<f32> {
    match exposure {
        crate::views::ViewportExposure::Auto => None,
        crate::views::ViewportExposure::Fixed(_) => exposure.fixed_ev(),
        crate::views::ViewportExposure::LevelBand => {
            let look = &world.get_resource::<regia_scene::level::LevelSettings>()?.look;
            Some(0.5 * (look.min_ev100? + look.max_ev100?))
        }
    }
}

/// Terrains the editor installed, per renderer (address) → terrain key. A renderer whose shared
/// draw was installed by the project (not in this map) is left alone.
fn installed_terrains() -> &'static Mutex<std::collections::HashMap<usize, u64>> {
    static MAP: OnceLock<Mutex<std::collections::HashMap<usize, u64>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// The terrain bridge: the first [`regia_terrain::Terrain`] not parked by a data layer becomes the
/// renderer's shared draw (`TerrainSharedDraw`, the game's terrain path), rebuilt when the
/// component changes and removed when it goes. Projects that install their own terrain draw
/// keep it. While the terrain tool sculpts (`sculpting`) the draw is removed; it rebuilds from
/// the edited map when the tool is turned off.
fn sync_terrain(world: &World, device: &RenderDevice, renderer: &mut FrameRenderer, sculpting: bool) {
    let address = std::ptr::from_ref::<FrameRenderer>(renderer) as usize;
    let mut installed = installed_terrains()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ours = installed.get(&address).copied();
    if renderer.has_shared_draw() && ours.is_none() {
        return;
    }
    if sculpting {
        // The terrain tool's preview mesh stands in (see `render_pane`).
        if ours.is_some() {
            renderer.set_shared_draw(None);
            installed.remove(&address);
        }
        return;
    }
    match drawn_terrain(world) {
        // Installed only while the renderer still has the draw: `DeviceFrameRenderer` rebuilds
        // its renderer in place on a device change, so a fresh renderer can sit at an address
        // this map remembers (audit 11-03 editor M1).
        Some((key, _)) if ours == Some(key) && renderer.has_shared_draw() => {}
        Some((key, entity)) => {
            let Some(terrain) = world.get::<regia_terrain::Terrain>(entity) else {
                return;
            };
            match regia_terrain::TerrainSharedDraw::new(
                device,
                terrain.map.clone(),
                &terrain.layers,
                terrain.cdlod.clone(),
                terrain.root_level,
            ) {
                Ok(mut draw) => {
                    // The hole mask lives on `Terrain` when no `TerrainHoles` resource is linked;
                    // the rebuilt draw cuts it too (audit wave 5, terrain-g2b H1).
                    draw.set_holes(terrain.holes.as_ref());
                    renderer.set_shared_draw(Some(Box::new(draw)));
                    installed.insert(address, key);
                }
                Err(err) => tracing::warn!(target: "regia_editor", "terrain draw: {err}"),
            }
        }
        None => {
            if ours.is_some() {
                renderer.set_shared_draw(None);
                installed.remove(&address);
            }
        }
    }
}

/// The terrain the bridge draws, with its rebuild key: the first [`regia_terrain::Terrain`] that
/// is neither parked by a data layer (spec 37 § 13) nor hidden. A parked or hidden terrain
/// leaves the draw and a drawn one rebuilds it, as a game's renderer withholds it
/// (`regia_terrain::parked_draw`, ADR-7200) and as a hidden mesh leaves extraction. Hidden means
/// a resolved `InheritedVisibility(false)` (a hidden ancestor) or its own `Visibility::Hidden`,
/// which the viewport honours before visibility propagation has run.
pub(crate) fn drawn_terrain(world: &World) -> Option<(u64, Entity)> {
    world.iter_entities().find_map(|entity| {
        let parked = entity.contains::<regia_scene::layers::LayerParked>();
        let inherited = entity.get::<regia_scene::components::InheritedVisibility>();
        let own_hidden = entity.get::<regia_scene::components::Visibility>()
            == Some(&regia_scene::components::Visibility::Hidden);
        if own_hidden || !regia_terrain::parked_draw::terrain_drawn(parked, inherited) {
            return None;
        }
        let terrain = entity.get_ref::<regia_terrain::Terrain>()?;
        // Hidden in the editor only (H, isolate; ADR-7770): picking and mesh retain skip it, so the
        // terrain draw does too (review sprint-w12-rev-g2 #5).
        if crate::view_state::is_view_hidden(world, entity.id()) {
            return None;
        }
        Some((
            entity.id().to_bits().wrapping_mul(0x9e37_79b9) ^ u64::from(terrain.last_changed().get()),
            entity.id(),
        ))
    })
}

/// Runs the runtime extraction on `world` and returns the frame (the world keeps its copy).
pub(crate) fn extract(world: &mut World) -> ExtractedFrame {
    if !world.contains_resource::<ExtractedFrame>() {
        world.init_resource::<ExtractedFrame>();
    }
    if !world.contains_resource::<LevelColorLut>() {
        world.init_resource::<LevelColorLut>();
    }
    if let Err(err) = world.run_system_once(regia_render::load_level_lut) {
        tracing::debug!(target: "regia_editor", "level LUT: {err}");
    }
    // Post-volume LUTs (ADR-2323): once the blend is on (`render.set_post_volume_luts`; off by
    // default, so nothing runs), load the tables the volumes name, as the runtime's `PreUpdate`
    // loader does, so `extract_frame` blends them into the game cameras' views here too.
    if world
        .get_resource::<regia_render::PostVolumeLuts>()
        .is_some_and(|luts| luts.enabled)
        && let Err(err) = world.run_system_once(regia_render::post_volume_luts::load_post_volume_luts)
    {
        tracing::debug!(target: "regia_editor", "post-volume LUTs: {err}");
    }
    // The open level's lightmap receivers (ADR-2635), as the runtime's `Render` schedule maps them.
    crate::lightmap_panel::apply_level_lightmap_receivers(world);
    if let Err(err) = world.run_system_once(regia_render::extract_frame) {
        tracing::warn!(target: "regia_editor", "viewport extraction failed: {err}");
    }
    // Morphed meshes (`MeshMorphTargets` + `MorphWeights`) draw deformed in the viewport too.
    if let Err(err) = world.run_system_once(regia_render::extract_morphs) {
        tracing::debug!(target: "regia_editor", "morph extraction: {err}");
    }
    world.resource::<ExtractedFrame>().clone()
}

/// The post settings an editor-camera view of a non-editor preset uses: the preset's post
/// stack, the level's look and LUT, exposure pinned (the band centre, else
/// [`VIEWPORT_EXPOSURE_EV`]).
fn preset_post(world: &World, settings: &regia_render::RenderSettings) -> regia_render::PostSettings {
    let mut post = settings.resolve_post();
    let mut banded = false;
    if let Some(level) = world.get_resource::<regia_scene::level::LevelSettings>() {
        regia_render::apply_level_look(&mut post, &level.look);
        banded = level.look.min_ev100.is_some() && level.look.max_ev100.is_some();
    }
    if let Some(lut) = world.get_resource::<LevelColorLut>().and_then(|l| l.lut.clone()) {
        post.lut = Some(lut);
    }
    post.auto_exposure = false;
    post.use_physical_camera = false;
    if !banded {
        post.exposure_ev = VIEWPORT_EXPOSURE_EV;
    }
    post
}

/// Renders one pane of `world` (see the module docs). `None` when the GPU render failed.
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_pane")]
pub fn render_pane(device: &RenderDevice, world: &mut World, request: &PaneRequest) -> Option<PaneFrame> {
    let layout = world
        .get_resource::<ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    render_pane_in(device, world, request, &layout, RendererSlot::Pane(request.pane))
}

/// Renders pane `request.pane` of `instance` (its layout: quality preset, show flags, game view,
/// possession). `request.camera` is the camera drawn; [`ViewportInstance::pane_camera`] gives
/// the instance's. `None` when the GPU render failed.
///
/// [`ViewportInstance::pane_camera`]: crate::viewport_instance::ViewportInstance::pane_camera
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_instance_pane")]
pub fn render_instance_pane(
    device: &RenderDevice,
    world: &mut World,
    instance: &crate::viewport_instance::ViewportInstance,
    request: &PaneRequest,
) -> Option<PaneFrame> {
    render_pane_in(
        device,
        world,
        request,
        &instance.layout,
        RendererSlot::Instance(instance.id, request.pane),
    )
}

/// [`render_instance_pane`] with every editor overlay painted in (see [`capture_pane`]), under
/// the instance's show flags.
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_capture_instance_pane")]
pub fn capture_instance_pane(
    device: &RenderDevice,
    world: &mut World,
    instance: &crate::viewport_instance::ViewportInstance,
    request: &PaneRequest,
) -> Option<PaneFrame> {
    let mut request = request.clone();
    request.ids = true;
    let mut frame = render_instance_pane(device, world, instance, &request)?;
    crate::overlays::paint_capture_overlays_for(
        &mut frame,
        world,
        request.kind,
        &instance.layout,
        instance.settings.audio,
    );
    // Show ▸ Post volumes (ADR-7593).
    crate::volume_overlay::paint_capture_post_volumes(&mut frame, world, Some(&instance.layout));
    Some(frame)
}

/// A headless capture of `instance`'s pane of `kind` (its own camera for that pane, overlays
/// painted in) at `size` pixels. The pane of `kind` must be in the instance's layout; the
/// perspective pane otherwise.
#[must_use]
pub fn capture_instance(
    device: &RenderDevice,
    world: &mut World,
    instance: &crate::viewport_instance::ViewportInstance,
    kind: ViewKind,
    size: [u32; 2],
) -> Option<PaneFrame> {
    let pane = instance
        .layout
        .panes
        .iter()
        .position(|p| p.kind == kind)
        .unwrap_or(0);
    let (kind, camera) = instance.pane_camera(pane);
    let request = PaneRequest {
        pane,
        kind,
        camera,
        width: size[0].max(16),
        height: size[1].max(16),
        ids: true,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    capture_instance_pane(device, world, instance, &request)
}

/// A pane rendered straight into a GPU texture (see [`render_instance_pane_gpu`]).
///
/// The picture stays on the GPU: `target` is the renderer's `Rgba8Unorm` LDR target (the same
/// sRGB-encoded bytes [`PaneFrame::image`] holds after a readback), ready to register with
/// egui's native-texture path. Only the id buffer, when requested, is read back.
#[derive(Clone)]
pub struct PaneGpuFrame {
    /// The finished picture (no overlays, no reference image).
    pub target: regia_render::OffscreenTarget,
    /// The camera the picture was rendered with (overlays and picking use it).
    pub view: ViewCamera,
    /// The entity-id buffer, when requested.
    pub ids: Option<IdBuffer>,
    /// Statistics and the settings in effect.
    pub info: PaneInfo,
    /// The normalized viewport rects of the game's window views the frame composited, in draw
    /// order (ADR-7395): split-screen players get one entry each, a possessed single view one
    /// full-rect entry, an editor-camera pane none.
    pub views: Vec<[f32; 4]>,
}

impl PaneGpuFrame {
    /// Width and height of the picture in pixels.
    #[must_use]
    pub fn size(&self) -> [u32; 2] {
        [self.target.width, self.target.height]
    }
}

/// What the renderer produced for a pane: pixels read back, or the target left on the GPU.
#[derive(Clone)]
enum PaneImage {
    Pixels(Vec<u8>),
    Target(regia_render::OffscreenTarget),
}

/// [`render_pane`] under `layout`, drawing with the world's [`RuntimeRenderer`] or the
/// viewport-owned renderer of `slot`.
fn render_pane_in(
    device: &RenderDevice,
    world: &mut World,
    request: &PaneRequest,
    layout: &ViewportLayout,
    slot: RendererSlot,
) -> Option<PaneFrame> {
    let (image, view, ids, info, _) = render_pane_core(device, world, request, layout, slot, false)?;
    let PaneImage::Pixels(pixels) = image else {
        return None;
    };
    let (width, height) = (request.width.max(16), request.height.max(16));
    let mut image = egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &pixels);
    // The reference image goes on the finished scene, before the editor overlays.
    if let Some(reference) = request.reference.as_ref().filter(|_| !info.possessed) {
        crate::reference::apply_reference(&mut image, ids.as_ref(), reference);
    }
    Some(PaneFrame {
        image,
        view,
        ids,
        info,
    })
}

/// [`render_instance_pane`] without the readback: the picture stays in a GPU texture
/// ([`PaneGpuFrame`]). A pane with a reference image needs the pixels on the CPU (the
/// reference is composited there), so it returns `None` like a failed render; callers use
/// [`render_instance_pane`] for those.
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_instance_pane_gpu")]
pub fn render_instance_pane_gpu(
    device: &RenderDevice,
    world: &mut World,
    instance: &crate::viewport_instance::ViewportInstance,
    request: &PaneRequest,
) -> Option<PaneGpuFrame> {
    if request.reference.is_some() {
        return None;
    }
    let (image, view, ids, info, views) = render_pane_core(
        device,
        world,
        request,
        &instance.layout,
        RendererSlot::Instance(instance.id, request.pane),
        true,
    )?;
    let PaneImage::Target(target) = image else {
        return None;
    };
    Some(PaneGpuFrame {
        target,
        view,
        ids,
        info,
        views,
    })
}

/// [`render_pane`] without the readback (see [`render_instance_pane_gpu`]), for the main
/// viewport's panes.
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_pane_gpu")]
pub fn render_pane_gpu(
    device: &RenderDevice,
    world: &mut World,
    request: &PaneRequest,
) -> Option<PaneGpuFrame> {
    if request.reference.is_some() {
        return None;
    }
    let layout = world
        .get_resource::<ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    let (image, view, ids, info, views) = render_pane_core(
        device,
        world,
        request,
        &layout,
        RendererSlot::Pane(request.pane),
        true,
    )?;
    let PaneImage::Target(target) = image else {
        return None;
    };
    Some(PaneGpuFrame {
        target,
        view,
        ids,
        info,
        views,
    })
}

/// The separate play window's picture (`crate::play_window`, ADR-7272): the play world through its
/// game camera (`possess`, Play) or the editor camera (Simulate), in game view (no editor
/// additions), at `size` pixels, left on the GPU for the window's blit. It draws with the play
/// world's [`RuntimeRenderer`] when it has one, else with its own viewport-owned renderer
/// (`crate::play_window::PLAY_WINDOW_VIEW_ID`), so the viewport's panes keep theirs.
#[tracing::instrument(target = "regia_editor", skip_all, name = "play_window_frame")]
pub fn render_play_window_gpu(
    device: &RenderDevice,
    world: &mut World,
    size: [u32; 2],
    possess: bool,
) -> Option<PaneGpuFrame> {
    let mut layout = world
        .get_resource::<ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    layout.quad = false;
    layout.active = 0;
    layout.game_view = true;
    layout.possessed = possess;
    // The game's picture: no pane view mode, exposure or screen percentage (ADR-7770).
    layout.panes[0].display = crate::views::PaneDisplay::default();
    let request = PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: world.get_resource::<EditorCamera>().cloned().unwrap_or_default(),
        width: size[0],
        height: size[1],
        ids: false,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    let (image, view, ids, info, views) = render_pane_core(
        device,
        world,
        &request,
        &layout,
        RendererSlot::Instance(crate::play_window::PLAY_WINDOW_VIEW_ID, 0),
        true,
    )?;
    let PaneImage::Target(target) = image else {
        return None;
    };
    Some(PaneGpuFrame {
        target,
        view,
        ids,
        info,
        views,
    })
}

/// Editor additions stay off in game view and while a possessed play view uses the game camera.
///
/// [`render_pane_core`] uses this for the editor fog clear, the preview sun, the backdrop sky
/// and the studio exposure override.
#[must_use]
fn editor_look_suppressed(layout: &ViewportLayout, possessed: bool) -> bool {
    layout.game_view || possessed
}

/// What [`render_pane_core`] returns: the picture (pixels or the GPU target), the camera it
/// used, the id buffer, the stats, and the normalized viewport rects of the window views a
/// possessed pane composited (one entry per split-screen player, a full rect for one view,
/// empty for an editor camera; ADR-7395).
type PaneRendered = (PaneImage, ViewCamera, Option<IdBuffer>, PaneInfo, Vec<[f32; 4]>);

/// A dedicated immutable copy, never a renderer cache target that another pane can overwrite.
#[derive(Resource)]
struct FrozenDebuggerPane {
    slot: RendererSlot,
    device: u64,
    size: [u32; 2],
    pixels: Vec<u8>,
    target: regia_render::OffscreenTarget,
    rendered: PaneRendered,
}

pub(crate) fn release_frame_capture(world: &mut World) {
    world.remove_resource::<FrozenDebuggerPane>();
}

fn frozen_frame(
    world: &World,
    device: &RenderDevice,
    slot: RendererSlot,
    request: &PaneRequest,
    gpu: bool,
) -> Option<PaneRendered> {
    let frozen = world.get_resource::<FrozenDebuggerPane>()?;
    if frozen.slot != slot || frozen.device != device.device_id() {
        return None;
    }
    let mut frame = frozen.rendered.clone();
    frame.0 = if gpu {
        PaneImage::Target(frozen.target.clone())
    } else {
        let size = [request.width.max(16), request.height.max(16)];
        let pixels = if size == frozen.size {
            frozen.pixels.clone()
        } else {
            if let Some(ids) = &mut frame.2 {
                let sample = |x: u32, y: u32| {
                    let sx =
                        ((u64::from(x) * 2 + 1) * u64::from(ids.width) / (u64::from(size[0]) * 2)) as usize;
                    let sy =
                        ((u64::from(y) * 2 + 1) * u64::from(ids.height) / (u64::from(size[1]) * 2)) as usize;
                    sy * ids.width as usize + sx
                };
                let indices: Vec<_> = (0..size[1])
                    .flat_map(|y| (0..size[0]).map(move |x| sample(x, y)))
                    .collect();
                ids.ids = indices.iter().map(|&i| ids.ids[i]).collect();
                ids.distance = indices.iter().map(|&i| ids.distance[i]).collect();
                ids.width = size[0];
                ids.height = size[1];
            }
            let image = image::RgbaImage::from_raw(frozen.size[0], frozen.size[1], frozen.pixels.clone())?;
            image::imageops::resize(&image, size[0], size[1], image::imageops::FilterType::Nearest).into_raw()
        };
        PaneImage::Pixels(pixels)
    };
    Some(frame)
}

fn freeze_target(device: &RenderDevice, size: [u32; 2], pixels: &[u8]) -> regia_render::OffscreenTarget {
    let texture =
        regia_render::TrackedDevice::observe(&device.device).create_texture(&wgpu::TextureDescriptor {
            label: Some("frame debugger frozen viewport"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
    device.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size[0] * 4),
            rows_per_image: Some(size[1]),
        },
        texture.size(),
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    regia_render::OffscreenTarget {
        texture,
        view,
        width: size[0],
        height: size[1],
    }
}

/// The shared body of the pane renders: `gpu` keeps the picture on the GPU instead of reading
/// it back.
///
/// A possessed pane composites its window views whenever there are more than one
/// (`window_views.len() > 1`): split-screen players, or a game with a second window camera
/// (ADR-7395, ADR-7466).
fn render_pane_core(
    device: &RenderDevice,
    world: &mut World,
    request: &PaneRequest,
    layout: &ViewportLayout,
    slot: RendererSlot,
    gpu: bool,
) -> Option<PaneRendered> {
    if world
        .get_resource::<FrozenDebuggerPane>()
        .is_some_and(|frozen| frozen.slot == slot && frozen.device != device.device_id())
    {
        crate::frame_debugger::resume(world);
        crate::frame_debugger::failed(world, "Render device changed; capture another frame".into());
    }
    if let Some(frozen) = frozen_frame(world, device, slot, request, gpu) {
        return Some(frozen);
    }
    // The main viewport's pane, or the in-process play window (ADR-8224: a play-window capture
    // request reaches the play world through `frame_debugger::sync_play_window`).
    let capturing = match slot {
        RendererSlot::Pane(_) => crate::frame_debugger::pending(world, request.pane),
        RendererSlot::Instance(id, _) => {
            id == crate::play_window::PLAY_WINDOW_VIEW_ID
                && crate::frame_debugger::play_capture_requested(world)
        }
    };
    let started = std::time::Instant::now();
    let counters_before = regia_render::frame_path_counters();
    // A play world's picture is play time (ADR-8594): its extraction and draw run inside the
    // `pie_render` play span (`crate::profiler_model::PIE_SPANS`), with the play flame context
    // for the systems a parallel extract hands to the compute pool, so the Profiler's play source
    // holds them and the edit source no longer does.
    let play = world.contains_resource::<PlayWorld>();
    let _play_span = play.then(|| tracing::info_span!(target: "regia_editor", "pie_render").entered());
    let _play_context = play.then(|| regia_core::profiling::flame_context("pie_render"));
    if play {
        world.get_resource_or_insert_with(PlayWorldRenders::default).0 += 1;
    }
    let width = request.width.max(16);
    let height = request.height.max(16);
    // World poses must be current: an edit made since the last schedule run (or a world with
    // no scene schedule at all, like a tool's) would otherwise render at stale or identity
    // poses. Dirty-tracked, so a settled world costs a scan. Mesh parts copy their owner's
    // pose in the resolve, so it runs after.
    regia_scene::prelude::propagate_transforms(world);
    resolve_mesh_sources(world);
    let show = layout.effective_show();
    // Headless captures do not run the shell's project-settings load. A malformed palette is
    // reported by its settings/agent surface; rendering still uses the existing fallback colors.
    if !world.contains_resource::<PlayWorld>() {
        let _ = crate::debug_colors::ensure(world);
    }
    let mut extracted = extract(world);
    let play_debug =
        matches!(slot, RendererSlot::Instance(id, _) if id == crate::play_window::PLAY_WINDOW_VIEW_ID);
    if play_debug {
        let debug = crate::overlays::play_window_debug_frame(world);
        if !debug.is_empty() {
            Arc::make_mut(&mut extracted.gizmos).batches.extend(debug.batches);
        }
    }
    // Masked instances (mask recolor, decal slots, `regia_render::mask_decals`): a model's part
    // entity takes its owner's component.
    let mask_decals = regia_render::mask_decals::collect_mask_decals(&extracted.meshes, |entity| {
        world
            .get::<regia_render::MaskDecals>(entity)
            .or_else(|| world.get::<regia_render::MaskDecals>(crate::mesh_source::pick_owner(world, entity)))
            .cloned()
    });

    // Possession: in play, look through the game's own (window) cameras with their own post.
    // Split-screen (specs/19 § 4, ADR-7395): every local player's window view is kept, in
    // camera order, and the frame composites them into their viewport rects of one target —
    // the play window and a possessed viewport pane show all players, exactly as the game's
    // own window draws them.
    let window_views = extracted.window_views();
    let game_view_index = window_views.first().map(|(index, _)| *index);
    let possessed = world.contains_resource::<PlayWorld>() && layout.possessed && game_view_index.is_some();
    let quality_preset = layout.quality;
    let settings = quality_preset.settings(world);
    let mut quality: FrameQuality = quality_preset.frame_quality(world);
    // The pane's display settings (ADR-7770): exposure, screen percentage, view mode surface.
    let display = *layout.display(request.pane);
    let pane_ev = pane_exposure_ev(world, display.exposure);
    let screen_percentage = display.screen_percentage();
    if screen_percentage != 100 {
        let permille = u32::from(quality.render_scale_permille) * u32::from(screen_percentage) / 100;
        quality.render_scale_permille = permille.clamp(100, 4000) as u16;
    }
    let split: Vec<[f32; 4]> = match (possessed, window_views.len() > 1) {
        (true, true) => window_views.iter().map(|(_, view)| view.viewport).collect(),
        _ => Vec::new(),
    };
    let view = if possessed {
        let index = game_view_index.unwrap_or(0);
        let game = extracted.views[index].camera;
        extracted.views = window_views.into_iter().map(|(_, view)| view).collect();
        ViewCamera {
            eye: Vec3::from_array(game.eye),
            target: Vec3::from_array(game.target),
            fov_y: game.fov_y,
            near: game.near,
            far: game.far,
            ortho: false,
        }
    } else {
        let view = ViewCamera::of(request.kind, &request.camera);
        let mut post = settings
            .as_ref()
            .map_or_else(regia_render::PostSettings::default, |s| preset_post(world, s));
        // The pane's Exposure menu (ADR-7770): a fixed EV or the level band's centre.
        if let Some(ev) = pane_ev {
            post.auto_exposure = false;
            post.use_physical_camera = false;
            post.exposure_ev = ev;
        }
        extracted.views = vec![ExtractedView {
            camera: ExtractedCamera {
                eye: view.eye.to_array(),
                target: view.target.to_array(),
                fov_y: view.fov_y,
                near: view.near,
                far: view.far,
            },
            viewport: [0.0, 0.0, 1.0, 1.0],
            order: 0,
            clear: true,
            target: ExtractedTarget::Window,
            layer_mask: u32::MAX,
            post,
        }];
        view
    };
    let runtime = world.contains_resource::<RuntimeRenderer>();
    let shared_history = runtime && crate::viewport_instance::ViewportInstances::shared_history(world);
    if view.ortho || layout.quad || shared_history {
        // Telephoto panes: no aerial haze over 50 km of air, no temporal history shared
        // between panes of one renderer (quad panes, or several viewports drawing through
        // the world's renderer).
        quality.taau = false;
        quality.physical_atmosphere &= !view.ortho;
        quality.volumetric_fog &= !view.ortho;
        quality.volumetric_clouds &= !view.ortho;
    }
    extracted.quality = quality;
    // View ▸ Color Vision: the viewport's views carry the filter's LUT (ADR-7838).
    crate::chrome_color_vision::apply_viewport_color_vision(world, &mut extracted);
    let suppress_editor_look = editor_look_suppressed(layout, possessed);
    if settings.is_none() && !suppress_editor_look {
        // The editor profile: the M10 look (no aerial fog, studio exposure).
        extracted.fog = regia_render::FogSettings::default();
    }
    if view.ortho {
        extracted.fog = regia_render::FogSettings::default();
        extracted.outdoor.fog = regia_render::VolumetricFogSettings::default();
    }
    // Hidden (outliner eye) entities and their parts do not draw.
    extracted
        .meshes
        .retain(|mesh| !is_user_hidden(world, pick_owner(world, mesh.entity)));
    // Surface debug views (UV checker, texel density, material ID) swap in debug twins.
    let mut debug_legend = Vec::new();
    {
        let label = |entity| {
            world
                .get::<regia_render::MaterialLabel>(entity)
                .map(|l| l.0.clone())
        };
        if let Some(frame) = regia_render::debug_surface_meshes(&extracted.meshes, request.debug_view, &label)
        {
            extracted.meshes = frame.meshes;
            debug_legend = frame.legend;
            if let Some(sink) = world.get_resource::<regia_render::DebugLegendSink>() {
                (sink.0)(&debug_legend);
            }
        }
    }
    // The lightmap views (ADR-2656) swap in their twins and, for lightmap only, take the sun out.
    if regia_render::lightmap_debug::handles(request.debug_view) {
        let map = world
            .get_resource::<regia_render::lightmap::InstalledLightmap>()
            .and_then(|i| i.lightmap.clone());
        let settings = world
            .get_resource::<regia_render::lightmap::LightmapSettings>()
            .copied()
            .unwrap_or_default();
        let entry_of = |entity| {
            world
                .get::<regia_render::lightmap::AppliedLightmapReceiver>(entity)
                .map(regia_render::lightmap::AppliedLightmapReceiver::entry)
        };
        regia_render::lightmap_debug::apply_lightmap_debug_view(
            &mut extracted,
            request.debug_view,
            map.as_ref(),
            settings,
            &entry_of,
        );
    }
    // The cloud transmittance map debug view (ADR-320) draws over the outdoor composite.
    if request.debug_view == regia_render::DebugViewMode::CloudShadowMap {
        extracted.outdoor.clouds.shadow_map_debug = true;
    }
    // The forward shader's debug views and the overdraw / LOD material swaps (ADR-7211).
    extracted.lights_views.shading_view = request.debug_view;
    if let Some(meshes) =
        regia_render::debug_shading::debug_shading_meshes(&extracted.meshes, request.debug_view)
    {
        extracted.meshes = meshes;
    }
    // The lighting-only and wireframe view modes draw every mesh with a plain surface (ADR-7770).
    let view_mode = if display.view_mode.debug_view() == request.debug_view {
        display.view_mode
    } else {
        crate::views::ViewMode::of_debug_view(request.debug_view)
    };
    if let Some(surface) = view_mode.surface() {
        extracted.meshes = plain_surface_meshes(&extracted.meshes, surface);
    }
    let preview_sun = !suppress_editor_look && show.preview_light && extracted.directional.is_none();
    if preview_sun {
        extracted.directional = Some(DirectionalLightUniform::new(
            PREVIEW_SUN,
            [1.0, 0.97, 0.92],
            100_000.0,
        ));
    }

    // Editor additions: proxy cubes and the backdrop sky.
    let proxies = if show.proxies && !possessed {
        proxy_entities(world)
    } else {
        Vec::new()
    };
    let grey = proxy_material();
    let sky_mat = sky_material();
    let sky = sky_sphere();
    let backdrop = !quality.physical_atmosphere && !suppress_editor_look;

    // Terrain tool: while sculpting, the terrain draws as the tool's preview mesh (live heights,
    // painted-layer colours) instead of the game's terrain draw, which rebuilds from the edited
    // map once the tool is off.
    let sculpting = !possessed
        && world
            .get_resource::<crate::terrain_tool::TerrainTool>()
            .is_some_and(|tool| tool.active);
    let terrains = if sculpting {
        terrain_meshes(world)
    } else {
        Vec::new()
    };
    let terrain_mat = terrain_material();

    // The renderer scales only a frame it upscales (post stack or TAAU): a scaled pane at editor
    // quality draws through a plain post stack pinned to the same EV (ADR-7770).
    let scaled_post = regia_render::PostSettings {
        auto_exposure: false,
        use_physical_camera: false,
        exposure_ev: pane_ev.unwrap_or(VIEWPORT_EXPOSURE_EV),
        ..regia_render::PostSettings::default()
    };
    let mut scene = extracted.frame_scene(0, width, height);
    if settings.is_none() && !suppress_editor_look {
        scene.post = (screen_percentage != 100).then_some(&scaled_post);
        scene.exposure_ev = pane_ev.unwrap_or(VIEWPORT_EXPOSURE_EV);
    }
    let exposure_ev = match scene.post {
        None => Some(scene.exposure_ev),
        Some(post) if !post.auto_exposure => Some(post.exposure_ev),
        Some(_) => None,
    };
    let cube = proxy_cube();
    for (entity, model) in &proxies {
        scene.meshes.push(FrameMesh {
            is_static: false,
            mesh_key: PROXY_CUBE_KEY,
            material_key: PROXY_MATERIAL_KEY,
            object_key: proxy_object_key(*entity),
            vertices: &cube.vertices,
            indices: &cube.indices,
            material: &grey,
            model: model.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
    }
    if backdrop {
        // Just inside the far plane (behind the ortho slab's target too).
        let radius = view.far - (view.far - view.near) * 0.02;
        push_sky(&mut scene.meshes, &sky.0, &sky.1, &sky_mat, view.eye, radius);
    }
    for (mesh_key, object_key, mesh) in &terrains {
        scene.meshes.push(FrameMesh {
            is_static: false,
            mesh_key: *mesh_key,
            material_key: TERRAIN_MATERIAL_KEY,
            object_key: *object_key,
            vertices: &mesh.0,
            indices: &mesh.1,
            material: &terrain_mat,
            model: Mat4::IDENTITY.to_cols_array_2d(),
            joints: &[],
            receiver_layer: 0,
            shadow: regia_render::ShadowCast::All,
            uv1: &[],
            lod_dither: 0.0,
        });
    }
    let stats_on = show.stats;
    let mut info = PaneInfo {
        quality: quality_preset.name(),
        profile: quality.profile_name(),
        taau: quality.taau,
        atmosphere: quality.physical_atmosphere,
        possessed,
        preview_sun,
        debug_legend,
        meshes: extracted.meshes.len(),
        proxies: proxies.len(),
        local_lights: extracted.local_lights.len(),
        render_size: {
            let (w, h) = quality.scaled_extent(width, height);
            [w, h]
        },
        screen_percentage,
        exposure_ev,
        view_mode: view_mode.name(),
        ..PaneInfo::default()
    };
    // Render overrides (tint, ghost, x-ray, unlit): a part entity takes its owner's.
    let mut overrides =
        regia_render::ExtractedRenderOverrides::from_world_with(world, &extracted.meshes, |entity| {
            let owner = pick_owner(world, entity);
            (owner != entity).then_some(owner)
        });
    // Proxy cubes take their entity's override too (the Edit in place context's dim, P-160).
    overrides.keys.extend(proxy_overrides(world, &proxies));
    // Per-vertex scalar overlays (skin-weight and deformation heatmaps, damage brushes).
    let scalar_overlays = regia_render::ExtractedScalarOverlays::from_world(world, &extracted.meshes);
    // Character shading models (skin, eye, sheen) by material.
    let shading = regia_render::ExtractedMaterialShading::from_world(world, &extracted.meshes);
    let mut captured = None;
    let mut capture_error = None;
    // The frame debugger's pass-boundary request (ADR-7860), read once for this pane.
    let pass_request = capturing.then(|| crate::frame_debugger::pass_request(world));
    let mut render =
        |world_ref: &World, renderer: &mut FrameRenderer| -> Option<(PaneImage, DrawStats, GpuTimings)> {
            renderer.set_render_overrides(overrides.keys.iter().copied());
            renderer.set_scalar_overlays(scalar_overlays.entries.iter().cloned());
            renderer.set_material_shading(shading.entries());
            renderer.set_outdoor(extracted.outdoor.clone());
            renderer.set_interior_scene(extracted.enclosure.clone());
            renderer.set_morph_instances(&extracted.morphs);
            renderer.set_gpu_timing(device, stats_on || capturing);
            renderer.set_image_light_view(request.debug_view);
            // Extra directional lights, ambient volumes and the shader debug view (ADR-7211).
            renderer.set_lights_views(&extracted.lights_views);
            // Wireframe: the extracted meshes' edges ride the gizmo pass (exact colour, over the
            // lit frame); every other mode leaves the renderer's gizmos alone.
            let wireframe = request.debug_view == regia_render::DebugViewMode::Wireframe;
            if play_debug {
                renderer.set_gizmos(Some(Arc::clone(&extracted.gizmos)));
            }
            if wireframe {
                let view = regia_render::DebugView {
                    mode: request.debug_view,
                    ..world_ref
                        .get_resource::<regia_render::DebugView>()
                        .cloned()
                        .unwrap_or_default()
                };
                renderer.set_gizmos(Some(regia_render::wireframe_frame(
                    &extracted.gizmos,
                    &extracted.meshes,
                    &view,
                )));
            }
            sync_terrain(world_ref, device, renderer, sculpting);
            renderer.set_mask_decals(&mask_decals);
            for _ in 0..request.warm_frames {
                if let Err(err) = renderer.render_offscreen(device, &scene) {
                    tracing::warn!(target: "regia_editor", "viewport warm frame failed: {err}");
                }
            }
            let session = if let Some(pass_request) = pass_request.as_ref().filter(|_| split.is_empty()) {
                match regia_render::frame_capture::CaptureSession::arm_with_passes(renderer, pass_request) {
                    Ok(session) => Some(session),
                    Err(error) => {
                        capture_error = Some(format!("{}: {error}", error.code()));
                        None
                    }
                }
            } else {
                if capturing {
                    capture_error = Some("Split-screen frame capture is not supported".into());
                }
                None
            };
            let drawn = if !split.is_empty() {
                // Split-screen possession (ADR-7395): every window view into its viewport rect
                // of the one target; a CPU pane then reads the composite back.
                match render_split_views(device, renderer, &extracted, &split, width, height) {
                    Ok(target) if gpu => Ok(PaneImage::Target(target)),
                    Ok(target) => renderer
                        .read_offscreen(device, target)
                        .map(|result| PaneImage::Pixels(result.pixels)),
                    Err(err) => Err(err),
                }
            } else if gpu {
                // The pane image only feeds the screen: draw like the window path, without
                // the capture overrides (ordered cull stays default-on; emissive fits keep
                // their wall-clock budget; DDGI bakes stay async).
                renderer
                    .begin_offscreen_live(device, &scene, wgpu::TextureFormat::Rgba8Unorm)
                    .map(PaneImage::Target)
            } else {
                renderer
                    .render_offscreen(device, &scene)
                    .map(|result| PaneImage::Pixels(result.pixels))
            };
            let drawn = if let Some(session) = session {
                match drawn {
                    Ok(image) => {
                        let pixels = match &image {
                            PaneImage::Pixels(pixels) => Ok(pixels.clone()),
                            PaneImage::Target(target) => {
                                renderer.read_offscreen(device, target.clone()).map(|r| r.pixels)
                            }
                        };
                        match pixels {
                            Ok(pixels) => match session.finish(renderer, device) {
                                Ok(snapshot) => captured = Some((snapshot, pixels)),
                                Err(error) => capture_error = Some(error.to_string()),
                            },
                            Err(error) => {
                                session.cancel(renderer);
                                capture_error = Some(error.to_string());
                            }
                        }
                        Ok(image)
                    }
                    Err(error) => {
                        session.cancel(renderer);
                        capture_error = Some(error.to_string());
                        Err(error)
                    }
                }
            } else {
                drawn
            };
            let rendered = match drawn {
                Ok(image) => Some((image, renderer.draw_stats(), renderer.gpu_timings())),
                Err(err) => {
                    tracing::warn!(target: "regia_editor", "viewport render failed: {err}");
                    None
                }
            };
            if wireframe || play_debug {
                renderer.set_gizmos(None);
            }
            if capturing {
                renderer.set_gpu_timing(device, stats_on);
            }
            rendered
        };
    let frames = u64::from(request.warm_frames) + 1;
    let rendered = if runtime {
        info.runtime_renderer = true;
        world
            .get_resource_or_insert_with(KeepAlive::default)
            .hold(&extracted, frames);
        world.resource_scope(|world, mut runtime: regia_ecs::prelude::Mut<RuntimeRenderer>| {
            render(world, &mut runtime.0)
        })
    } else {
        let owned;
        let cell = match slot {
            RendererSlot::Pane(pane) => pane_renderer(pane),
            RendererSlot::Instance(id, pane) => {
                owned = instance_renderer(id, pane);
                &*owned
            }
        };
        let mut scope = cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let PaneRenderer { renderer, keep } = &mut *scope;
        keep.hold(&extracted, frames);
        render(world, renderer.get(device))
    };
    if let Some(error) = capture_error {
        crate::frame_debugger::capture_failed(world, error);
    }
    let (image, draws, gpu_timings) = rendered?;
    info.draws = draws;
    info.gpu = gpu_timings;

    // The id buffer: the same meshes (parts map to owners) and proxies, same camera.
    let behind_reference = request
        .reference
        .as_ref()
        .is_some_and(|r| r.mode == crate::reference::ReferenceMode::Behind);
    let ids = if (request.ids || behind_reference) && !possessed {
        let draws = id_draws(world, &extracted.meshes, &proxies);
        let aspect = width as f32 / height as f32;
        match crate::picking::render_id_buffer(
            device,
            &draws,
            view.view_projection(aspect),
            view.eye.to_array(),
            width,
            height,
        ) {
            Ok(buffer) => Some(buffer),
            Err(err) => {
                tracing::warn!(target: "regia_editor", "id buffer failed: {err}");
                None
            }
        }
    } else {
        None
    };
    info.cpu_ms = started.elapsed().as_secs_f32() * 1000.0;
    info.counters = regia_render::frame_path_counters().since(counters_before);
    let views = if possessed {
        match split.as_slice() {
            [] => vec![[0.0, 0.0, 1.0, 1.0]],
            rects => rects.to_vec(),
        }
    } else {
        Vec::new()
    };
    let mut result = (image, view, ids, info, views);
    if let Some((snapshot, pixels)) = captured {
        let target = freeze_target(device, [width, height], &pixels);
        if gpu {
            result.0 = PaneImage::Target(target.clone());
        }
        let mut kept = result.clone();
        kept.0 = PaneImage::Target(target.clone());
        world.insert_resource(FrozenDebuggerPane {
            slot,
            device: device.device_id(),
            size: [width, height],
            pixels,
            target,
            rendered: kept,
        });
        crate::frame_debugger::capture_complete(world, snapshot);
    }
    Some(result)
}

/// Composites the split-screen window views of a possessed pane into one offscreen target
/// (ADR-7395): each view renders into its viewport rect of the target, mirroring the runtime's
/// window path (`regia_render`'s `render_windows`): a first view that covers the whole target
/// clears what it draws over, otherwise the target is cleared once up front and every view
/// loads, so no region is left stale.
fn render_split_views(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    extracted: &ExtractedFrame,
    viewports: &[[f32; 4]],
    width: u32,
    height: u32,
) -> Result<OffscreenTarget, RenderError> {
    let target = renderer.offscreen_target(device, width, height, false);
    let first_full = viewports
        .first()
        .is_some_and(|rect| *rect == [0.0, 0.0, 1.0, 1.0]);
    let mut pre_cleared = false;
    if !first_full {
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("split-screen clear"),
            });
        regia_render::encode_clear(&mut encoder, &target.view, [0.0, 0.0, 0.0, 1.0]);
        device.queue.submit([encoder.finish()]);
        pre_cleared = true;
    }
    renderer.set_light_extras(extracted.light_extras.clone());
    for (index, viewport) in viewports.iter().enumerate() {
        let rect = pixel_viewport(*viewport, width, height);
        let scene = extracted.frame_scene(index, rect[2], rect[3]);
        let frame_target = FrameTarget::rect(
            &target.view,
            wgpu::TextureFormat::Rgba8Unorm,
            width,
            height,
            rect,
            pre_cleared || index > 0,
        )
        .with_color(Some(&target.texture));
        renderer.render_into(device, &scene, frame_target)?;
    }
    Ok(target)
}

/// The id pass's draws for a pane: the extracted meshes (a part maps to its owner) and the
/// proxy cubes, in the order the id pass resolves ties.
fn id_draws(
    world: &World,
    meshes: &[regia_render::ExtractedMesh],
    proxies: &[(Entity, Mat4)],
) -> Vec<IdDraw> {
    let mut draws: Vec<IdDraw> = meshes
        .iter()
        .map(|mesh| IdDraw {
            mesh: Arc::clone(&mesh.mesh),
            model: Mat4::from_cols_array_2d(&mesh.model),
            entity: pick_owner(world, mesh.entity),
        })
        .collect();
    draws.extend(proxies.iter().map(|(entity, model)| IdDraw {
        mesh: Arc::clone(proxy_cube()),
        model: *model,
        entity: *entity,
    }));
    crate::picking::sort_id_draws(&mut draws);
    draws
}

/// Only the id pass of a pane, reading back `rect` of it (cut to the pane): the cheap path for
/// a pointer pick on a pane whose picture is not re-rendered this frame. It draws no shaded
/// frame, and the readback is the window, not the whole buffer. Returns the pane's view (what
/// the ids were rendered with) and the window; pass them with `request.width` x
/// `request.height` as the size to [`crate::viewport::pick_in_view`] or
/// [`crate::viewport::pick_hit_in_view`].
///
/// The ids are those [`render_pane`] gives with `request.ids` for the same world and request
/// (same meshes, hidden entities, proxies and camera). `None` when the pane is possessed (a
/// play-world game camera has no id buffer), the id pass failed, or `rect` is outside the pane.
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_pane_id_window")]
pub fn render_pane_id_window(
    device: &RenderDevice,
    world: &mut World,
    request: &PaneRequest,
    rect: crate::picking::IdRect,
) -> Option<(ViewCamera, crate::picking::IdWindow)> {
    let width = request.width.max(16);
    let height = request.height.max(16);
    let layout = world
        .get_resource::<ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    regia_scene::prelude::propagate_transforms(world);
    resolve_mesh_sources(world);
    let show = layout.effective_show();
    let mut extracted = extract(world);
    let possessed =
        world.contains_resource::<PlayWorld>() && layout.possessed && !extracted.window_views().is_empty();
    if possessed {
        return None;
    }
    let view = ViewCamera::of(request.kind, &request.camera);
    extracted
        .meshes
        .retain(|mesh| !is_user_hidden(world, pick_owner(world, mesh.entity)));
    let proxies = if show.proxies {
        proxy_entities(world)
    } else {
        Vec::new()
    };
    let draws = id_draws(world, &extracted.meshes, &proxies);
    let aspect = width as f32 / height as f32;
    match crate::picking::render_id_window(
        device,
        &draws,
        view.view_projection(aspect),
        view.eye.to_array(),
        width,
        height,
        rect,
    ) {
        Ok(window) => Some((view, window)),
        Err(err) => {
            tracing::warn!(target: "regia_editor", "id window failed: {err}");
            None
        }
    }
}

/// The perspective editor view of `world` (no overlays), for the panel background and tests.
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_render")]
pub fn render_viewport(
    device: &RenderDevice,
    world: &mut World,
    camera: &EditorCamera,
    size: [f32; 2],
) -> Option<egui::ColorImage> {
    let request = PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: camera.clone(),
        width: size[0].max(64.0) as u32,
        height: size[1].max(64.0) as u32,
        ids: false,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    render_pane(device, world, &request).map(|frame| frame.image)
}

/// A pane with every editor overlay painted into the pixels: selection outline, grid, icons,
/// bounds and the selection gizmo — what the agent protocol returns. The live panel paints
/// the same overlays with egui instead.
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_capture_pane")]
pub fn capture_pane(device: &RenderDevice, world: &mut World, request: &PaneRequest) -> Option<PaneFrame> {
    let mut request = request.clone();
    request.ids = true;
    let mut frame = render_pane(device, world, &request)?;
    crate::overlays::paint_capture_overlays(&mut frame, world, request.kind);
    // Show ▸ Post volumes (ADR-7593).
    crate::volume_overlay::paint_capture_post_volumes(&mut frame, world, None);
    Some(frame)
}

/// The perspective viewport with overlays (see [`capture_pane`]).
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_capture")]
pub fn capture_viewport(
    device: &RenderDevice,
    world: &mut World,
    camera: &EditorCamera,
    size: [f32; 2],
) -> Option<egui::ColorImage> {
    let request = PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: camera.clone(),
        width: size[0].max(64.0) as u32,
        height: size[1].max(64.0) as u32,
        ids: true,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    capture_pane(device, world, &request).map(|frame| frame.image)
}

/// A quality preset by name for the agent protocol and menus (see [`ViewportQuality::from_name`]).
#[must_use]
pub fn quality_by_name(name: &str) -> Option<ViewportQuality> {
    ViewportQuality::from_name(name)
}

/// PNG bytes of a viewport image, then standard base64 (RFC 4648 § 4).
///
/// # Errors
///
/// The PNG encoder rejected the buffer.
pub fn viewport_png_base64(image: &egui::ColorImage) -> Result<String, String> {
    let mut raw = Vec::with_capacity(image.pixels.len() * 4);
    for pixel in &image.pixels {
        raw.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
    }
    let mut png_bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut png_bytes, image.width() as u32, image.height() as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|err| err.to_string())?;
    writer.write_image_data(&raw).map_err(|err| err.to_string())?;
    drop(writer);
    Ok(base64_encode(&png_bytes))
}

/// Standard base64 with padding (RFC 4648 § 4).
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A shaded unit cube (object space, ±0.5) as vertex/index lists.
pub(crate) fn cube_mesh() -> (Vec<LabVertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    append_cube(&mut vertices, &mut indices);
    (vertices, indices)
}

/// Appends a shaded unit cube (object space, ±0.5) to the mesh lists.
fn append_cube(vertices: &mut Vec<LabVertex>, indices: &mut Vec<u32>) {
    // (corner offsets, normal, uv corners) per face, for a unit cube at ±0.5.
    const HALF: f32 = 0.5;
    #[allow(clippy::type_complexity)] // a static cube table; a type alias would obscure it
    const FACES: [([[f32; 3]; 4], [f32; 3], [[f32; 2]; 4]); 6] = [
        // +X
        (
            [
                [HALF, -HALF, -HALF],
                [HALF, HALF, -HALF],
                [HALF, HALF, HALF],
                [HALF, -HALF, HALF],
            ],
            [1.0, 0.0, 0.0],
            [[1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]],
        ),
        // -X
        (
            [
                [-HALF, -HALF, HALF],
                [-HALF, HALF, HALF],
                [-HALF, HALF, -HALF],
                [-HALF, -HALF, -HALF],
            ],
            [-1.0, 0.0, 0.0],
            [[1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]],
        ),
        // +Y
        (
            [
                [-HALF, HALF, -HALF],
                [-HALF, HALF, HALF],
                [HALF, HALF, HALF],
                [HALF, HALF, -HALF],
            ],
            [0.0, 1.0, 0.0],
            [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        ),
        // -Y
        (
            [
                [-HALF, -HALF, -HALF],
                [HALF, -HALF, -HALF],
                [HALF, -HALF, HALF],
                [-HALF, -HALF, HALF],
            ],
            [0.0, -1.0, 0.0],
            [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        ),
        // +Z
        (
            [
                [-HALF, -HALF, HALF],
                [HALF, -HALF, HALF],
                [HALF, HALF, HALF],
                [-HALF, HALF, HALF],
            ],
            [0.0, 0.0, 1.0],
            [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        ),
        // -Z
        (
            [
                [HALF, -HALF, -HALF],
                [-HALF, -HALF, -HALF],
                [-HALF, HALF, -HALF],
                [HALF, HALF, -HALF],
            ],
            [0.0, 0.0, -1.0],
            [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        ),
    ];

    for (corners, normal, uvs) in FACES {
        let base = vertices.len() as u32;
        for (i, corner) in corners.iter().enumerate() {
            vertices.push(LabVertex {
                position: *corner,
                normal,
                uv: uvs[i],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::prelude::Visibility;

    #[test]
    fn editor_additions_stay_off_in_game_view_and_while_possessed() {
        let layout = ViewportLayout::default();
        assert!(!editor_look_suppressed(&layout, false));
        let game = ViewportLayout {
            game_view: true,
            ..layout.clone()
        };
        assert!(editor_look_suppressed(&game, false));
        assert!(editor_look_suppressed(&layout, true));
    }

    fn small_terrain() -> regia_terrain::Terrain {
        // The terrain tool's test terrain (129 samples, root level 4).
        let n = 129u32;
        let heights = (0..n * n).map(|i| (i % n) as f32 * 0.1).collect();
        let map = regia_terrain::TerrainMap::from_heights(n, n, heights, 128.0, 100.0, 0.0).expect("map");
        regia_terrain::Terrain::new(
            &map.to_heightfield(),
            128.0,
            100.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            4,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain")
    }

    fn park(world: &mut World, entity: Entity, parked: bool) {
        let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
            panic!("live entity");
        };
        if parked {
            entity_mut.insert(regia_scene::layers::LayerParked {
                restore: Visibility::Inherited,
                inherited: false,
            });
        } else {
            entity_mut.remove::<regia_scene::layers::LayerParked>();
        }
    }

    /// Review sprint-w12-rev-g2 #5: a terrain hidden in the editor only (H or isolate) is not drawn;
    /// unhidden, it draws again.
    #[test]
    fn the_terrain_bridge_skips_an_editor_hidden_terrain() {
        let mut world = World::new();
        let terrain = world.spawn(small_terrain()).id();
        assert_eq!(drawn_terrain(&world).map(|(_, e)| e), Some(terrain));
        assert_eq!(
            crate::view_state::set_view_hidden(&mut world, &[terrain], true),
            1
        );
        assert_eq!(drawn_terrain(&world), None, "hidden in the editor: no draw");
        crate::view_state::set_view_hidden(&mut world, &[terrain], false);
        assert_eq!(drawn_terrain(&world).map(|(_, e)| e), Some(terrain));
    }

    #[test]
    fn the_terrain_bridge_skips_a_parked_terrain() {
        let mut world = World::new();
        let first = world.spawn(small_terrain()).id();
        let second = world.spawn(small_terrain()).id();
        assert_eq!(drawn_terrain(&world).map(|(_, e)| e), Some(first));
        let (key, _) = drawn_terrain(&world).expect("drawn");

        park(&mut world, first, true);
        assert_eq!(
            drawn_terrain(&world).map(|(_, e)| e),
            Some(second),
            "the next live terrain"
        );
        park(&mut world, second, true);
        assert_eq!(drawn_terrain(&world), None, "every terrain parked: no draw");

        // Back live: the same key as before, so the bridge rebuilds only because it removed the draw.
        park(&mut world, first, false);
        assert_eq!(drawn_terrain(&world), Some((key, first)));
    }

    /// A hidden (not parked) terrain leaves the viewport as it leaves a game
    /// (`regia_terrain::parked_draw`, ADR-7200): its own `Visibility::Hidden`, or a resolved
    /// `InheritedVisibility(false)` (a hidden ancestor). Shown again, it is the drawn terrain.
    #[test]
    fn the_terrain_bridge_skips_a_hidden_terrain_as_a_game_does() {
        let mut world = World::new();
        let first = world.spawn(small_terrain()).id();
        let second = world.spawn(small_terrain()).id();
        assert_eq!(drawn_terrain(&world).map(|(_, e)| e), Some(first));
        if let Ok(mut entity_mut) = world.get_entity_mut(first) {
            entity_mut.insert(Visibility::Hidden);
        }
        assert_eq!(
            drawn_terrain(&world).map(|(_, e)| e),
            Some(second),
            "a hidden terrain is not drawn: the next visible one is"
        );
        if let Ok(mut entity_mut) = world.get_entity_mut(second) {
            entity_mut.insert(regia_scene::components::InheritedVisibility(false));
        }
        assert_eq!(
            drawn_terrain(&world),
            None,
            "a terrain under a hidden ancestor is not drawn either"
        );
        if let Ok(mut entity_mut) = world.get_entity_mut(first) {
            entity_mut.insert(Visibility::Visible);
        }
        assert_eq!(drawn_terrain(&world).map(|(_, e)| e), Some(first), "shown again");
    }

    /// Audit 11-03 editor M1: `DeviceFrameRenderer` rebuilds its renderer in place on a device
    /// change. The new renderer has the old one's address, so the bridge's per-address record
    /// said "installed" and the terrain vanished from the viewport.
    #[test]
    fn the_terrain_bridge_reinstalls_into_a_renderer_rebuilt_in_place() {
        let Ok(device) = headless_device() else {
            eprintln!("skipped: no headless device");
            return;
        };
        let mut world = World::new();
        world.spawn(small_terrain());
        let mut renderer = FrameRenderer::new();
        sync_terrain(&world, &device, &mut renderer, false);
        assert!(
            renderer.has_shared_draw(),
            "precondition: the bridge installs the terrain"
        );
        // What `DeviceFrameRenderer::get` does on a device change: a fresh renderer, same place.
        renderer = FrameRenderer::new();
        sync_terrain(&world, &device, &mut renderer, false);
        assert!(
            renderer.has_shared_draw(),
            "the rebuilt renderer at the same address never got the terrain back"
        );
        installed_terrains()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(std::ptr::from_ref::<FrameRenderer>(&renderer) as usize));
    }

    #[test]
    fn cubes_build_well_formed_meshes() {
        let (vertices, indices) = cube_mesh();
        assert_eq!(vertices.len(), 24, "six faces × four corners");
        assert_eq!(indices.len(), 36, "six faces × two triangles");
        // Every index points at a real vertex.
        assert!(indices.iter().all(|i| (*i as usize) < vertices.len()));
        // Object-space geometry: the first +X corner sits at the cube's edge.
        assert_eq!(vertices[0].position, [0.5, -0.5, -0.5]);
    }

    #[test]
    fn model_matrices_pose_the_shared_cube() {
        // A translated entity must move the shared cube's corners into the
        // world position the panel expects (this is what the viewport draws).
        let transform = Transform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            ..Default::default()
        };
        let model = Mat4::from(transform.compute_affine());
        let corner = model.transform_point3(Vec3::new(0.5, -0.5, -0.5));
        assert_eq!(corner.to_array(), [1.5, 1.5, 2.5]);
    }

    #[test]
    fn sky_gradient_is_blue_overhead_and_dark_below() {
        let up = sky_color(Vec3::Y);
        let down = sky_color(-Vec3::Y);
        assert!(up[2] > up[0], "zenith is blue: {up:?}");
        assert!(
            down[0] + down[1] + down[2] < up[0] + up[1] + up[2],
            "ground is darker"
        );
        let (vertices, indices) = sky_sphere();
        assert!(indices.len() >= 6 && indices.iter().all(|i| (*i as usize) < vertices.len()));
    }

    #[test]
    fn viewport_png_base64_is_padded() {
        // RFC 4648 § 10 vectors. "f" / "fo" / "foo" are the 1-, 2- and 3-byte leftovers.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}

#[cfg(test)]
mod post_volume_lut_tests {
    use super::*;
    use regia_render::{PostVolume, PostVolumeLuts};
    use regia_scene::GlobalTransform;

    /// Writes a 2-edge identity `.cube` LUT atomically and returns its absolute path.
    fn write_cube(dir: &std::path::Path, name: &str) -> String {
        let mut text = String::from("LUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    text.push_str(&format!("{r} {g} {b}\n"));
                }
            }
        }
        let path = dir.join(name);
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, text).expect("write the LUT");
        std::fs::rename(&tmp, &path).expect("publish the LUT");
        path.display().to_string()
    }

    fn world_with_volume(lut: &str) -> World {
        let mut world = World::new();
        world.spawn((
            PostVolume {
                lut: Some(lut.to_string()),
                ..PostVolume::default()
            },
            GlobalTransform::IDENTITY,
        ));
        world
    }

    #[test]
    fn extract_loads_volume_luts_only_when_the_blend_is_on() {
        let dir = regia_test_utils::unique_temp_dir("editor_post_volume_lut");
        let good = write_cube(&dir, "room.cube");
        let bad = dir.join("missing.cube").display().to_string();

        // No resource: extraction runs and nothing is inserted (default off, nothing moves).
        let mut world = world_with_volume(&good);
        let _ = extract(&mut world);
        assert!(!world.contains_resource::<PostVolumeLuts>());

        // Off: the tables are not read.
        world.init_resource::<PostVolumeLuts>();
        let _ = extract(&mut world);
        let luts = world.resource::<PostVolumeLuts>();
        assert!(luts.table(&good).is_none() && !luts.failed(&good));
        assert_eq!(luts.stats().loads, 0);

        // On: the volume's table loads on the next extraction, once.
        world.resource_mut::<PostVolumeLuts>().enabled = true;
        let _ = extract(&mut world);
        let _ = extract(&mut world);
        let luts = world.resource::<PostVolumeLuts>();
        assert_eq!(luts.table(&good).map(|l| l.size()), Some(2));
        assert_eq!(luts.stats().loads, 1, "a loaded path is not read again");

        // A missing file is recorded as failed and not retried.
        let mut world = world_with_volume(&bad);
        let mut on = PostVolumeLuts::default();
        on.enabled = true;
        world.insert_resource(on);
        let _ = extract(&mut world);
        let _ = extract(&mut world);
        let luts = world.resource::<PostVolumeLuts>();
        assert!(luts.failed(&bad));
        assert_eq!(luts.stats().failures, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
