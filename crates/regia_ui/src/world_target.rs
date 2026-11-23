// SPDX-License-Identifier: MPL-2.0
//! Camera-target and world-space UI (`specs/18` § 3, gap rows Y13/Y14, ADR-2406).
//!
//! A [`UiRoot`] whose [`UiTarget`] is not a window no longer draws over the window:
//!
//! - [`UiTarget::Camera`] with a `RenderTarget::Texture` camera: the root is laid out at the
//!   camera's texture size and drawn into that texture at the frame's `overlay` slot, after the
//!   view's own passes (an in-world monitor showing a menu). A window camera's root is laid out
//!   at the camera's viewport rect and drawn in the window's list, offset and clipped to that
//!   rect (split-screen HUDs); pointer hit tests follow the offset.
//! - [`UiTarget::WorldSpace`]: the root is laid out at a fixed `size` in UI px into an offscreen
//!   texture of that many pixels, and the frame composites that texture as a depth-tested quad
//!   `size / ppm` metres large at the root entity's `GlobalTransform` (the panel spans its
//!   local X right and Y up, facing +Z), at the `after_post` slot, so the panel's colours are
//!   the authored ones (no exposure, tonemap or bloom). [`WorldPanel`] adds billboarding, a
//!   distance fade, a pivot and the depth-test switch.
//!
//! Both composite through the frame's pass extension point (`regia_render::FramePass`): the two
//! passes are registered only while such a root exists, so a frame without camera or world UI
//! does exactly the work it did before (pinned by `tests/world_ui.rs`).
//!
//! Pointer input: the window pointer never hits nodes of texture or world roots. Agents and
//! games click them with [`world_click`] (`ui.world_click`): a ray against every panel, closest
//! hit, then that root's own hit test at the panel pixel. Focus and gamepad navigation are
//! unchanged.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};

use regia_app::prelude::*;
use regia_core::glam::{Affine3A, Mat4, Quat, Vec3, Vec3A};
use regia_render::{Camera, RenderDevice, RenderTarget};
use serde_json::{Value, json};

use crate::agent_bridge::UiBridgeError;
use crate::draw::{NO_CLIP, UiQuad};
use crate::interaction::{HitCandidate, UiEvent, UiEventKind, pick_topmost};
use crate::node::{ComputedNode, Disabled, Interactable, UiRoot, UiTarget};
use crate::render::{QuadBatch, SharedUiRenderer, UiRenderer, UiRendererHandle};

const SHADER: &str = include_str!("../shaders/ui-world.wgsl");

/// World-panel composite shader source (`ui-world.wgsl`), for `cargo xtask check-shaders`.
#[must_use]
pub fn world_shader_source() -> &'static str {
    SHADER
}

/// Name of the world-panel composite pass in `render.graph`.
pub const WORLD_PASS_NAME: &str = "ui_world_panels";
/// Name of the camera-target UI pass in `render.graph`.
pub const CAMERA_PASS_NAME: &str = "ui_camera_targets";

/// Format of a world panel's offscreen texture (what the overlay draws into on an sRGB window).
pub const PANEL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Largest world panel edge in pixels.
pub const MAX_PANEL_EDGE: f32 = 4096.0;

/// How a world panel turns toward the camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Billboard {
    /// The panel keeps its transform's rotation.
    #[default]
    None,
    /// The panel's +Z faces the eye (world +Y stays up).
    Full,
    /// The panel turns about world +Y only, toward the eye (signposts, name plates).
    AxisY,
}

impl Billboard {
    /// `"none"`, `"full"` or `"axis_y"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Full => "full",
            Self::AxisY => "axis_y",
        }
    }

    /// The mode named `name` ([`Self::as_str`]'s inverse).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [Self::None, Self::Full, Self::AxisY]
            .into_iter()
            .find(|b| b.as_str() == name)
    }
}

/// Optional settings of a [`UiTarget::WorldSpace`] root (put it on the root entity).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct WorldPanel {
    /// How the panel turns toward the camera.
    pub billboard: Billboard,
    /// The point of the panel at the transform's origin, in panel units (`[0.5, 0.5]` = centre,
    /// `[0, 0]` = top-left).
    pub pivot: [f32; 2],
    /// Distance fade `[start, end]` in metres: opaque nearer than `start`, invisible beyond
    /// `end`. `None` = no fade.
    pub fade: Option<[f32; 2]>,
    /// Hide the parts behind scene geometry (default). Off draws the panel over everything.
    pub depth_test: bool,
    /// Whole-panel opacity.
    pub opacity: f32,
    /// Redraw the panel's texture every frame even when its quads did not change (a panel
    /// showing a live image such as a render-to-texture view).
    pub live: bool,
}

impl Default for WorldPanel {
    fn default() -> Self {
        Self {
            billboard: Billboard::None,
            pivot: [0.5, 0.5],
            fade: None,
            depth_test: true,
            opacity: 1.0,
            live: false,
        }
    }
}

impl UiRoot {
    /// A world-space root of `size` UI px at `ppm` pixels per metre, laid out one UI px per
    /// texel ([`crate::scale::ScaleMode::ConstantPixelSize`]).
    #[must_use]
    pub fn world_space(size: [f32; 2], ppm: f32) -> Self {
        Self {
            target: UiTarget::WorldSpace { size, ppm },
            scale_mode: crate::scale::ScaleMode::ConstantPixelSize,
            sort_order: 0,
        }
    }

    /// A root drawn into `camera`'s target.
    #[must_use]
    pub fn camera(camera: Entity) -> Self {
        Self {
            target: UiTarget::Camera(camera),
            ..Self::default()
        }
    }
}

/// Which offscreen list a non-window root draws into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UiTargetKey {
    /// The texture of the `RenderTarget::Texture` camera with this asset id.
    CameraTexture(u64),
    /// The world panel of the root with these entity bits.
    World(u64),
}

/// Where extract puts a root's quads this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Placement {
    /// The window's list at the window origin (every window root).
    Window,
    /// The window's list, offset to a window camera's viewport and clipped to it.
    WindowRect {
        /// Viewport top-left in window px.
        origin: [f32; 2],
        /// Viewport rect in window px.
        clip: [f32; 4],
    },
    /// An offscreen target's list.
    Target(UiTargetKey),
    /// Draws nothing (the camera is missing or has no `Camera`).
    Hidden,
}

/// Marker written on every node of a camera-target or world-space root: the window pointer's
/// hit test skips it (`window_origin` `None`) or offsets it to a window camera's viewport
/// (`Some`).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct UiTargetNode {
    /// The root the node draws under.
    pub root: Entity,
    /// Window px of the root's top-left for a window camera's root; `None` for texture and world
    /// roots (not under the window pointer).
    pub window_origin: Option<[f32; 2]>,
}

/// A world panel's placement for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelParams {
    /// The root entity's world transform.
    pub transform: Affine3A,
    /// Panel size in metres (`size / ppm`).
    pub size_m: [f32; 2],
    /// Settings ([`WorldPanel`], default when absent).
    pub settings: WorldPanel,
}

impl PanelParams {
    /// The affine map from panel units (`u` right, `v` down, both `0..=1`) to world space, with
    /// the billboard turned toward `eye`.
    #[must_use]
    pub fn unit_to_world(&self, eye: Vec3) -> Affine3A {
        let (scale, rotation, translation) = self.transform.to_scale_rotation_translation();
        let rotation = billboard_rotation(self.settings.billboard, rotation, translation, eye);
        let [w, h] = self.size_m;
        let [px, py] = self.settings.pivot;
        // (u, v) -> ((u - px) w, (py - v) h, 0) in the panel's local metres.
        let local =
            Affine3A::from_cols_array(&[w, 0.0, 0.0, 0.0, -h, 0.0, 0.0, 0.0, 1.0, -px * w, py * h, 0.0]);
        Affine3A::from_scale_rotation_translation(scale, rotation, translation) * local
    }

    /// The opacity the panel draws with seen from `eye` (opacity times the distance fade).
    #[must_use]
    pub fn alpha_at(&self, eye: Vec3) -> f32 {
        let opacity = self.settings.opacity.clamp(0.0, 1.0);
        let Some([start, end]) = self.settings.fade else {
            return opacity;
        };
        let distance = self.transform.translation.distance(Vec3A::from(eye));
        opacity * fade_factor(distance, start, end)
    }
}

/// `1` up to `start`, `0` from `end`, linear between (a reversed or empty range is a hard cut at
/// `end`).
#[must_use]
pub fn fade_factor(distance: f32, start: f32, end: f32) -> f32 {
    if !distance.is_finite() {
        return 0.0;
    }
    if end <= start {
        return if distance < end { 1.0 } else { 0.0 };
    }
    ((end - distance) / (end - start)).clamp(0.0, 1.0)
}

fn billboard_rotation(mode: Billboard, own: Quat, position: Vec3, eye: Vec3) -> Quat {
    let mut to_eye = eye - position;
    match mode {
        Billboard::None => return own,
        Billboard::Full => {}
        Billboard::AxisY => to_eye.y = 0.0,
    }
    let Some(z) = to_eye.try_normalize() else {
        return own;
    };
    let x = Vec3::Y.cross(z).try_normalize().unwrap_or(Vec3::X);
    let y = z.cross(x);
    Quat::from_mat3(&regia_core::glam::Mat3::from_cols(x, y, z))
}

/// Where a ray meets a panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelHit {
    /// Distance along the normalized ray, in metres.
    pub distance: f32,
    /// Panel units (`u` right, `v` down).
    pub uv: [f32; 2],
}

/// Intersects the ray `origin + t * direction` (`direction` need not be normalized; `distance` is
/// measured along the normalized one) with the panel spanned by `unit_to_world`. Both faces are
/// hit; a hit behind the origin, outside the panel or on a ray parallel to it is `None`.
#[must_use]
pub fn ray_panel(origin: Vec3, direction: Vec3, unit_to_world: &Affine3A) -> Option<PanelHit> {
    let dir = direction.try_normalize()?;
    let p0 = Vec3::from(unit_to_world.translation);
    let ex = Vec3::from(unit_to_world.matrix3.x_axis);
    let ey = Vec3::from(unit_to_world.matrix3.y_axis);
    let normal = ex.cross(ey);
    let denom = dir.dot(normal);
    if denom.abs() <= f32::EPSILON * normal.length().max(1.0) {
        return None;
    }
    let t = (p0 - origin).dot(normal) / denom;
    if !t.is_finite() || t < 0.0 {
        return None;
    }
    let q = origin + dir * t - p0;
    // Solve q = u ex + v ey (the axes need not be orthogonal under a skewed parent).
    let (a, b, c) = (ex.dot(ex), ex.dot(ey), ey.dot(ey));
    let (d, e) = (q.dot(ex), q.dot(ey));
    let det = a * c - b * b;
    if det.abs() <= f32::MIN_POSITIVE {
        return None;
    }
    let u = (d * c - e * b) / det;
    let v = (e * a - d * b) / det;
    const EDGE: f32 = 1e-5;
    if !(-EDGE..=1.0 + EDGE).contains(&u) || !(-EDGE..=1.0 + EDGE).contains(&v) {
        return None;
    }
    Some(PanelHit {
        distance: t,
        uv: [u.clamp(0.0, 1.0), v.clamp(0.0, 1.0)],
    })
}

// ---------------------------------------------------------------------------------------------
// Targets: sizes, placements and the renderer's per-target lists.
// ---------------------------------------------------------------------------------------------

/// One non-window root as [`refresh_targets`] last resolved it. A matching list skips the lock.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ResolvedTarget {
    root: Entity,
    target: UiTarget,
    placement: Placement,
    size_px: [u32; 2],
    panel: Option<PanelParams>,
}

/// What [`refresh_targets`] resolved for every non-window root last frame.
#[derive(Resource, Debug, Default)]
pub(crate) struct TargetPlacements {
    pub(crate) roots: HashMap<Entity, Placement>,
    /// Full resolution, including panel transforms. Compared before the renderer lock.
    resolved: Vec<ResolvedTarget>,
}

/// Roots [`tag_target_nodes`] last walked. A matching set with a stable hierarchy skips the walk.
#[derive(Resource, Default, PartialEq)]
struct TaggedRootSet {
    roots: Vec<(Entity, Placement)>,
}

/// One offscreen target's quads and GPU state, held by the [`UiRenderer`].
pub(crate) struct TargetSurface {
    /// The target's quads and instance buffer.
    pub(crate) batch: QuadBatch,
    /// Layout size in px (the panel texture's size).
    pub(crate) size: [u32; 2],
    /// World panel placement (`None` for camera textures).
    pub(crate) panel: Option<PanelParams>,
    /// The root entity (world panels) or `None`.
    pub(crate) root: Option<Entity>,
    /// Offscreen texture of a world panel.
    texture: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// Quad generation last drawn into `texture`.
    drawn_generation: Option<u64>,
    /// Image registrations as of that draw (a re-registered image redraws the panel).
    drawn_images: u64,
    /// Per-view composite uniforms of a world panel (one per view recorded in a frame).
    uniforms: Vec<(wgpu::Buffer, wgpu::BindGroup)>,
    /// Frames the target drew into (texture redraws for panels, overlay draws for cameras).
    pub(crate) draws: u64,
    /// Views the panel was composited into.
    pub(crate) composites: u64,
}

impl TargetSurface {
    fn new(size: [u32; 2], panel: Option<PanelParams>, root: Option<Entity>) -> Self {
        Self {
            batch: QuadBatch::default(),
            size,
            panel,
            root,
            texture: None,
            drawn_generation: None,
            drawn_images: 0,
            uniforms: Vec::new(),
            draws: 0,
            composites: 0,
        }
    }
}

/// Pixel size of a texture camera's image (its `TextureViewSettings`, else the default size).
fn camera_texture_size(world: &World, camera: Entity) -> [f32; 2] {
    let (w, h) = world.get::<regia_render::TextureViewSettings>(camera).map_or(
        (
            regia_render::TEXTURE_TARGET_SIZE[0],
            regia_render::TEXTURE_TARGET_SIZE[1],
        ),
        |s| s.extent(),
    );
    [w as f32, h as f32]
}

/// A window camera's viewport rect in window px.
fn camera_viewport_px(camera: &Camera, window: [f32; 2]) -> [f32; 4] {
    let v = camera.viewport;
    let finite = |x: f32| if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 };
    let (x, y) = (finite(v[0]), finite(v[1]));
    let (w, h) = (finite(v[2]).min(1.0 - x), finite(v[3]).min(1.0 - y));
    [
        (x * window[0]).round(),
        (y * window[1]).round(),
        (w * window[0]).round(),
        (h * window[1]).round(),
    ]
}

pub(crate) fn sanitize_panel_size(size: [f32; 2]) -> [f32; 2] {
    size.map(|s| {
        if s.is_finite() {
            s.clamp(1.0, MAX_PANEL_EDGE)
        } else {
            1.0
        }
    })
}

/// The layout size of `root` when it is not a window root: a world panel's `size`, a texture
/// camera's image size, a window camera's viewport size. `None` for window roots (they use the
/// window size).
#[must_use]
pub fn root_target_size(world: &World, root: Entity) -> Option<[f32; 2]> {
    let target = world.get::<UiRoot>(root)?.target;
    match target {
        UiTarget::Window(_) => None,
        UiTarget::WorldSpace { size, .. } => Some(sanitize_panel_size(size)),
        UiTarget::Camera(camera) => {
            let Some(cam) = world.get::<Camera>(camera) else {
                return Some([1.0, 1.0]);
            };
            match cam.target {
                RenderTarget::Texture { .. } => Some(camera_texture_size(world, camera)),
                RenderTarget::Window => {
                    let window = world
                        .get_resource::<crate::scale::UiScales>()
                        .map_or([1.0, 1.0], |s| s.window_size);
                    let r = camera_viewport_px(cam, window);
                    Some([r[2].max(1.0), r[3].max(1.0)])
                }
            }
        }
    }
}

fn placement_of(world: &World, root: Entity, target: UiTarget) -> Placement {
    match target {
        UiTarget::Window(_) => Placement::Window,
        UiTarget::WorldSpace { .. } => Placement::Target(UiTargetKey::World(root.to_bits())),
        UiTarget::Camera(camera) => match world.get::<Camera>(camera).map(|c| (c.target.clone(), c)) {
            None => Placement::Hidden,
            Some((RenderTarget::Texture { asset_id }, _)) => {
                Placement::Target(UiTargetKey::CameraTexture(asset_id))
            }
            Some((RenderTarget::Window, cam)) => {
                let window = world
                    .get_resource::<crate::scale::UiScales>()
                    .map_or([1.0, 1.0], |s| s.window_size);
                let r = camera_viewport_px(cam, window);
                Placement::WindowRect {
                    origin: [r[0], r[1]],
                    clip: r,
                }
            }
        },
    }
}

/// The placement extract uses for `root` (window roots are not stored).
pub(crate) fn root_placement(world: &World, root: Entity) -> Placement {
    world
        .get_resource::<TargetPlacements>()
        .and_then(|p| p.roots.get(&root).copied())
        .unwrap_or(Placement::Window)
}

fn panel_params(world: &World, root: Entity, size: [f32; 2], ppm: f32) -> PanelParams {
    let transform = world
        .get::<regia_scene::GlobalTransform>(root)
        .map_or(Affine3A::IDENTITY, |g| g.0);
    let ppm = if ppm.is_finite() && ppm > 0.0 { ppm } else { 100.0 };
    let size = sanitize_panel_size(size);
    PanelParams {
        transform,
        size_m: [size[0] / ppm, size[1] / ppm],
        settings: world.get::<WorldPanel>(root).copied().unwrap_or_default(),
    }
}

thread_local! {
    /// Times [`refresh_targets`] locked the renderer. A quiet frame must not increase this.
    ///
    /// Per test thread, so a parallel `cargo test` cannot bump another test's count.
    static REFRESH_LOCKS: Cell<u64> = const { Cell::new(0) };
    /// Times [`tag_target_nodes`] walked a target tree. A stable tree must not increase this.
    static TAG_WALKS: Cell<u64> = const { Cell::new(0) };
}

/// How many times this thread locked the renderer inside `refresh_targets`.
#[cfg(test)]
fn refresh_lock_count() -> u64 {
    REFRESH_LOCKS.with(Cell::get)
}

/// How many times this thread walked a target tree inside `tag_target_nodes`.
#[cfg(test)]
fn tag_walk_count() -> u64 {
    TAG_WALKS.with(Cell::get)
}

/// `Extract`, before the draw list rebuild: resolves every non-window root's placement and size,
/// refreshes world-panel transforms in the renderer (every frame: a moving panel redraws nothing
/// but its composite), creates and drops target lists, and forces a full rebuild when a
/// placement changed. Does nothing (and takes no lock) while no camera or world root exists.
pub(crate) fn refresh_targets(world: &mut World, renderer: &SharedUiRenderer) {
    let roots: Vec<(Entity, UiTarget)> = world
        .query::<(Entity, &UiRoot)>()
        .iter(world)
        .filter(|(_, r)| !matches!(r.target, UiTarget::Window(_)))
        .map(|(e, r)| (e, r.target))
        .collect();
    let had = world
        .get_resource::<TargetPlacements>()
        .is_some_and(|p| !p.roots.is_empty());
    if roots.is_empty() && !had {
        return;
    }
    let mut resolved: Vec<ResolvedTarget> = Vec::with_capacity(roots.len());
    for (root, target) in &roots {
        let placement = placement_of(world, *root, *target);
        let (size_px, panel) = if let Placement::Target(_) = placement {
            let size = root_target_size(world, *root).unwrap_or([1.0, 1.0]);
            let px = [size[0].round().max(1.0) as u32, size[1].round().max(1.0) as u32];
            let panel = match *target {
                UiTarget::WorldSpace { size, ppm } => Some(panel_params(world, *root, size, ppm)),
                _ => None,
            };
            (px, panel)
        } else {
            ([0, 0], None)
        };
        resolved.push(ResolvedTarget {
            root: *root,
            target: *target,
            placement,
            size_px,
            panel,
        });
    }
    resolved.sort_by_key(|item| item.root.to_bits());
    if world
        .get_resource::<TargetPlacements>()
        .is_some_and(|prev| prev.resolved == resolved)
    {
        return;
    }
    let _span = tracing::trace_span!("ui_targets_refresh").entered();
    let mut placements: HashMap<Entity, Placement> = HashMap::with_capacity(resolved.len());
    let mut wanted: BTreeMap<UiTargetKey, ([u32; 2], Option<PanelParams>, Option<Entity>)> = BTreeMap::new();
    for item in &resolved {
        placements.insert(item.root, item.placement);
        if let Placement::Target(key) = item.placement {
            let root = matches!(key, UiTargetKey::World(_)).then_some(item.root);
            wanted.insert(key, (item.size_px, item.panel, root));
        }
    }
    let mut changed = {
        let previous = world.get_resource::<TargetPlacements>();
        previous.is_none_or(|p| p.roots != placements)
    };
    {
        REFRESH_LOCKS.with(|n| n.set(n.get().wrapping_add(1)));
        let mut guard = renderer.lock();
        let before = guard.targets.len();
        guard.targets.retain(|key, _| wanted.contains_key(key));
        changed |= guard.targets.len() != before;
        for (key, (size, panel, root)) in wanted {
            match guard.targets.get_mut(&key) {
                Some(surface) => {
                    if surface.size != size {
                        surface.size = size;
                        surface.texture = None;
                        surface.drawn_generation = None;
                        changed = true;
                    }
                    surface.panel = panel;
                    surface.root = root;
                }
                None => {
                    guard.targets.insert(key, TargetSurface::new(size, panel, root));
                    changed = true;
                }
            }
        }
    }
    world.insert_resource(TargetPlacements {
        roots: placements,
        resolved,
    });
    if changed {
        if let Some(mut damage) = world.get_resource_mut::<crate::extract::UiDamage>() {
            damage.mark();
        }
        if let Some(mut set) = world.get_resource_mut::<crate::extract::UiDamageSet>() {
            set.full = true;
        }
        // A new size reaches layout on the next frame.
        if let Some(mut layout) = world.get_resource_mut::<crate::layout::LayoutDirty>() {
            layout.mark();
        }
    }
}

impl UiRenderer {
    /// Installs the quad lists of the offscreen targets (extract); targets with no list this
    /// rebuild get an empty one.
    pub(crate) fn set_target_quads(&mut self, mut lists: BTreeMap<UiTargetKey, Vec<UiQuad>>) {
        for (key, surface) in &mut self.targets {
            let quads = lists.remove(key).unwrap_or_default();
            // An unchanged list keeps its generation: the panel texture is not redrawn because
            // some other root changed.
            if surface.batch.quads_generation == 0 || surface.batch.quads != quads {
                surface.batch.set_quads(quads);
            }
        }
    }

    /// The quads of offscreen target `key` (tests and diagnostics).
    #[must_use]
    pub fn target_quads(&self, key: UiTargetKey) -> Option<&[UiQuad]> {
        self.targets.get(&key).map(|s| s.batch.quads.as_slice())
    }

    /// The offscreen targets that exist, in key order.
    #[must_use]
    pub fn target_keys(&self) -> Vec<UiTargetKey> {
        self.targets.keys().copied().collect()
    }

    /// Whether any world panel or camera texture target exists: `(world, camera)`.
    #[must_use]
    pub fn has_targets(&self) -> (bool, bool) {
        let world = self.targets.keys().any(|k| matches!(k, UiTargetKey::World(_)));
        let camera = self
            .targets
            .keys()
            .any(|k| matches!(k, UiTargetKey::CameraTexture(_)));
        (world, camera)
    }
}

/// Lists for [`crate::extract`]: a root's quads go to the window list or a target list.
pub(crate) type TargetLists = BTreeMap<UiTargetKey, Vec<UiQuad>>;

/// Context a root walk starts from: `(origin, clip)` of its placement.
pub(crate) fn walk_frame(placement: Placement) -> ([f32; 2], [f32; 4]) {
    match placement {
        Placement::WindowRect { origin, clip } => (origin, clip),
        _ => ([0.0; 2], NO_CLIP),
    }
}

// ---------------------------------------------------------------------------------------------
// Node tagging (pointer routing).
// ---------------------------------------------------------------------------------------------

/// `PostUpdate`, after layout: tags every node under a camera or world root with
/// [`UiTargetNode`] and untags nodes that left one. Cheap while no such root exists.
pub fn tag_target_nodes(world: &mut World) {
    // Resolved here (not read from the last extract), so a new root is routed on its first frame.
    let roots: Vec<(Entity, UiTarget)> = world
        .query::<(Entity, &UiRoot)>()
        .iter(world)
        .filter(|(_, r)| !matches!(r.target, UiTarget::Window(_)))
        .map(|(e, r)| (e, r.target))
        .collect();
    let placements: Vec<(Entity, Placement)> = roots
        .into_iter()
        .map(|(root, target)| (root, placement_of(world, root, target)))
        .collect();
    let tagged: Vec<Entity> = world
        .query_filtered::<Entity, With<UiTargetNode>>()
        .iter(world)
        .collect();
    if placements.is_empty() && tagged.is_empty() {
        return;
    }
    let mut signature: Vec<(Entity, Placement)> = placements
        .iter()
        .map(|(root, placement)| (*root, *placement))
        .collect();
    signature.sort_by_key(|(root, _)| root.to_bits());
    let hierarchy_dirty = world
        .query_filtered::<Entity, Or<(
            Changed<Children>,
            Changed<UiRoot>,
            Changed<Camera>,
            Changed<regia_render::TextureViewSettings>,
        )>>()
        .iter(world)
        .next()
        .is_some();
    let scales_dirty = world
        .get_resource_ref::<crate::scale::UiScales>()
        .is_some_and(|scales| scales.is_changed());
    let same_roots = world
        .get_resource::<TaggedRootSet>()
        .is_some_and(|prev| prev.roots == signature);
    if same_roots && !hierarchy_dirty && !scales_dirty {
        return;
    }
    let _span = tracing::trace_span!("ui_targets_tag").entered();
    TAG_WALKS.with(|n| n.set(n.get().wrapping_add(1)));
    let mut want: HashMap<Entity, UiTargetNode> = HashMap::new();
    for (root, placement) in placements {
        let window_origin = match placement {
            Placement::WindowRect { origin, .. } => Some(origin),
            _ => None,
        };
        let mut stack = vec![root];
        while let Some(e) = stack.pop() {
            want.insert(e, UiTargetNode { root, window_origin });
            if let Some(children) = world.get::<Children>(e) {
                stack.extend(children.iter());
            }
        }
    }
    for e in tagged {
        if !want.contains_key(&e)
            && let Ok(mut entity) = world.get_entity_mut(e)
        {
            entity.remove::<UiTargetNode>();
        }
    }
    for (e, tag) in want {
        if world.get::<UiTargetNode>(e) != Some(&tag)
            && let Ok(mut entity) = world.get_entity_mut(e)
        {
            entity.insert(tag);
        }
    }
    world.insert_resource(TaggedRootSet { roots: signature });
}

// ---------------------------------------------------------------------------------------------
// Frame passes.
// ---------------------------------------------------------------------------------------------

/// The pass state the registration system keeps.
#[derive(Default)]
pub(crate) struct PassState {
    world: bool,
    camera: bool,
}

/// `Extract`, after the draw list: registers the world-panel pass while a world panel exists and
/// the camera-target pass while a camera texture target exists, and removes each when its last
/// target goes (so frames without either are untouched). Waits while the game-frame warm holds
/// the renderer.
pub(crate) fn sync_target_passes(
    renderer: Option<ResMut<regia_render::RuntimeRenderer>>,
    device: Option<Res<RenderDevice>>,
    ui: Option<Res<SharedUiRenderer>>,
    warm: Option<Res<regia_render::GameFrameWarm>>,
    mut state: Local<PassState>,
) {
    let (Some(mut renderer), Some(ui)) = (renderer, ui) else {
        return;
    };
    if warm.is_some_and(|w| w.is_warming()) {
        return;
    }
    let (want_world, want_camera) = ui.lock().has_targets();
    if want_world == state.world && want_camera == state.camera {
        return;
    }
    let Some(device) = device else { return };
    if want_world != state.world {
        if want_world {
            let pass = WorldPanelPass::new(ui.0.clone(), (*device).clone());
            match renderer.0.register_pass(Box::new(pass)) {
                Ok(()) => state.world = true,
                Err(err) => tracing::warn!(target: "regia_ui", "world UI pass not registered: {err}"),
            }
        } else {
            renderer.0.remove_pass(WORLD_PASS_NAME);
            state.world = false;
        }
    }
    if want_camera != state.camera {
        if want_camera {
            let pass = CameraTargetPass {
                ui: ui.0.clone(),
                device: (*device).clone(),
            };
            match renderer.0.register_pass(Box::new(pass)) {
                Ok(()) => state.camera = true,
                Err(err) => tracing::warn!(target: "regia_ui", "camera UI pass not registered: {err}"),
            }
        } else {
            renderer.0.remove_pass(CAMERA_PASS_NAME);
            state.camera = false;
        }
    }
}

fn lock(handle: &UiRendererHandle) -> std::sync::MutexGuard<'_, UiRenderer> {
    match handle.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Draws the UI of camera texture targets into the texture views at the `overlay` slot.
pub struct CameraTargetPass {
    ui: UiRendererHandle,
    device: RenderDevice,
}

impl CameraTargetPass {
    /// A pass drawing `ui`'s camera texture targets.
    #[must_use]
    pub fn new(ui: UiRendererHandle, device: RenderDevice) -> Self {
        Self { ui, device }
    }
}

impl regia_render::FramePass for CameraTargetPass {
    fn name(&self) -> &str {
        CAMERA_PASS_NAME
    }

    fn slot(&self) -> regia_render::GraphSlot {
        regia_render::GraphSlot::Overlay
    }

    fn reads(&self) -> &[&'static str] {
        &["output"]
    }

    fn writes(&self) -> &[&'static str] {
        &["output"]
    }

    fn record(&mut self, ctx: &mut regia_render::FramePassContext<'_>) -> Result<(), String> {
        let Some(asset_id) = ctx.texture_view else {
            return Ok(());
        };
        let _span = tracing::trace_span!("ui_camera_target", asset_id).entered();
        let mut ui = lock(&self.ui);
        let key = UiTargetKey::CameraTexture(asset_id);
        let Some(mut surface) = ui.targets.remove(&key) else {
            return Ok(());
        };
        let texture = ctx.color.texture();
        let readable = texture.usage().contains(wgpu::TextureUsages::COPY_SRC) && texture.sample_count() == 1;
        ui.draw_batch(
            &mut surface.batch,
            &self.device,
            ctx.encoder,
            ctx.color,
            readable.then_some(texture),
            ctx.color_format,
            ctx.color_size.0,
            ctx.color_size.1,
            false,
        );
        surface.draws += 1;
        ui.targets.insert(key, surface);
        Ok(())
    }
}

/// Composite-pipeline GPU state of the world-panel pass.
struct CompositeGpu {
    panel_bgl: wgpu::BindGroupLayout,
    depth_bgl: [wgpu::BindGroupLayout; 2],
    layouts: [wgpu::PipelineLayout; 2],
    shaders: [wgpu::ShaderModule; 2],
    sampler: wgpu::Sampler,
    pipelines: HashMap<(wgpu::TextureFormat, bool), wgpu::RenderPipeline>,
}

/// Bytes of one panel's composite uniforms.
const PANEL_UNIFORM_BYTES: u64 = 112;

/// Redraws world-panel textures that changed and composites every panel as a depth-tested quad
/// at the `after_post` slot of every view.
pub struct WorldPanelPass {
    ui: UiRendererHandle,
    device: RenderDevice,
    gpu: Option<CompositeGpu>,
    /// Frame index of the last record, and how many views recorded in it.
    frame: (u64, usize),
    /// Depth view, multisampled flag, and the bind group built for that pair.
    depth_group: Option<(wgpu::TextureView, bool, wgpu::BindGroup)>,
}

impl WorldPanelPass {
    /// A pass compositing `ui`'s world panels.
    #[must_use]
    pub fn new(ui: UiRendererHandle, device: RenderDevice) -> Self {
        Self {
            ui,
            device,
            gpu: None,
            frame: (u64::MAX, 0),
            depth_group: None,
        }
    }

    fn ensure_gpu(&mut self, dev: &regia_render::TrackedDevice) -> &mut CompositeGpu {
        self.gpu.get_or_insert_with(|| build_composite(dev))
    }

    /// The depth bind group for this view. The same depth view and sample mode reuse one group.
    fn cached_depth_bind_group(
        &mut self,
        dev: &regia_render::TrackedDevice,
        layout: &wgpu::BindGroupLayout,
        depth: &wgpu::TextureView,
        multisampled: bool,
    ) -> wgpu::BindGroup {
        if let Some((view, sampled, group)) = &self.depth_group
            && view == depth
            && *sampled == multisampled
        {
            return group.clone();
        }
        let group = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui world depth bg"),
            layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(depth),
            }],
        });
        self.depth_group = Some((depth.clone(), multisampled, group.clone()));
        group
    }
}

fn build_composite(dev: &regia_render::TrackedDevice) -> CompositeGpu {
    let panel_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("ui world panel bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ],
    });
    let depth_bgl = [false, true].map(|multisampled| {
        dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui world depth bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled,
                },
                count: None,
            }],
        })
    });
    let layouts = [0, 1].map(|i| {
        dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui world pl"),
            bind_group_layouts: &[Some(&panel_bgl), Some(&depth_bgl[i])],
            immediate_size: 0,
        })
    });
    let shaders = [false, true].map(|multisampled| {
        let source = world_shader_variant(multisampled);
        dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui world"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        })
    });
    let sampler = dev.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("ui world sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });
    CompositeGpu {
        panel_bgl,
        depth_bgl,
        layouts,
        shaders,
        sampler,
        pipelines: HashMap::new(),
    }
}

/// The composite shader for a single-sampled or multisampled scene depth.
#[must_use]
pub fn world_shader_variant(multisampled: bool) -> String {
    if multisampled {
        SHADER.replace("texture_depth_2d;", "texture_depth_multisampled_2d;")
    } else {
        SHADER.to_owned()
    }
}

fn composite_pipeline(
    gpu: &mut CompositeGpu,
    dev: &regia_render::TrackedDevice,
    format: wgpu::TextureFormat,
    multisampled: bool,
) -> wgpu::RenderPipeline {
    let index = usize::from(multisampled);
    if let Some(p) = gpu.pipelines.get(&(format, multisampled)) {
        return p.clone();
    }
    let pipeline = dev.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("ui world composite"),
        layout: Some(&gpu.layouts[index]),
        vertex: wgpu::VertexState {
            module: &gpu.shaders[index],
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &gpu.shaders[index],
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    gpu.pipelines.insert((format, multisampled), pipeline.clone());
    pipeline
}

/// The composite uniforms of one panel in one view (`ui-world.wgsl::Panel`).
#[must_use]
pub fn panel_uniforms(
    clip_from_unit: Mat4,
    viewport: [f32; 4],
    depth_size: [f32; 2],
    depth_test: bool,
    alpha: f32,
    encoding: crate::render::OutputEncoding,
    paper_white_nits: f32,
) -> [f32; 28] {
    let mut out = [0.0f32; 28];
    out[..16].copy_from_slice(&clip_from_unit.to_cols_array());
    out[16..20].copy_from_slice(&viewport);
    out[20] = depth_size[0];
    out[21] = depth_size[1];
    out[22] = if depth_test { 1.0 } else { 0.0 };
    out[23] = alpha;
    out[24] = encoding.mode();
    out[25] = paper_white_nits;
    out
}

impl regia_render::FramePass for WorldPanelPass {
    fn name(&self) -> &str {
        WORLD_PASS_NAME
    }

    fn slot(&self) -> regia_render::GraphSlot {
        regia_render::GraphSlot::AfterPost
    }

    fn reads(&self) -> &[&'static str] {
        &["output", "depth"]
    }

    fn writes(&self) -> &[&'static str] {
        &["output"]
    }

    fn prewarm(&mut self, device: &regia_render::TrackedDevice) -> Result<usize, String> {
        self.ensure_gpu(device);
        // The overlay pipeline the panel textures are drawn with.
        let created = lock(&self.ui).warm(&self.device, PANEL_FORMAT);
        Ok(usize::from(created))
    }

    fn record(&mut self, ctx: &mut regia_render::FramePassContext<'_>) -> Result<(), String> {
        let _span = tracing::trace_span!("ui_world_panels").entered();
        if self.frame.0 == ctx.frame_index {
            self.frame.1 += 1;
        } else {
            self.frame = (ctx.frame_index, 0);
        }
        let view_slot = self.frame.1;
        let ui_handle = self.ui.clone();
        let mut ui = lock(&ui_handle);
        let keys: Vec<UiTargetKey> = ui
            .targets
            .keys()
            .copied()
            .filter(|k| matches!(k, UiTargetKey::World(_)))
            .collect();
        if keys.is_empty() {
            return Ok(());
        }
        let device = self.device.clone();
        let dev = regia_render::TrackedDevice::observe(&device.device);
        let multisampled = ctx.depth_multisampled;
        let format = ctx.color_format;
        let pipeline = {
            let gpu = self.ensure_gpu(&dev);
            composite_pipeline(gpu, &dev, format, multisampled)
        };
        let Some(depth_layout) = self
            .gpu
            .as_ref()
            .map(|gpu| gpu.depth_bgl[usize::from(multisampled)].clone())
        else {
            return Ok(());
        };
        let depth_bg = self.cached_depth_bind_group(&dev, &depth_layout, ctx.depth, multisampled);
        let depth_size = {
            let size = ctx.depth.texture().size();
            [size.width as f32, size.height as f32]
        };
        let eye = Vec3::from(ctx.eye);
        let encoding = crate::render::OutputEncoding::for_format(format);
        let paper_white = ui.settings.paper_white_nits;
        // Back to front by distance to the eye, so overlapping translucent panels blend in order.
        let mut order: Vec<(f32, UiTargetKey)> = keys
            .iter()
            .filter_map(|k| {
                let panel = ui.targets.get(k)?.panel?;
                Some((Vec3::from(panel.transform.translation).distance_squared(eye), *k))
            })
            .collect();
        order.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        // `gpu` lived only for the pipeline lookup above. Clone the two handles the
        // per-panel groups need so this loop does not borrow `self.gpu`.
        let (panel_bgl, sampler) = {
            let Some(gpu) = self.gpu.as_ref() else {
                return Ok(());
            };
            (gpu.panel_bgl.clone(), gpu.sampler.clone())
        };
        let mut draws: Vec<(UiTargetKey, wgpu::BindGroup)> = Vec::with_capacity(order.len());
        for (_, key) in order {
            let Some(mut surface) = ui.targets.remove(&key) else {
                continue;
            };
            let Some(panel) = surface.panel else {
                ui.targets.insert(key, surface);
                continue;
            };
            // Redraw the panel's texture when its quads changed (or every frame when live).
            let [w, h] = surface.size;
            if surface.texture.is_none() {
                let texture = dev.create_texture(&wgpu::TextureDescriptor {
                    label: Some("ui world panel"),
                    size: wgpu::Extent3d {
                        width: w.max(1),
                        height: h.max(1),
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: PANEL_FORMAT,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                surface.texture = Some((texture, view));
                surface.drawn_generation = None;
                surface.uniforms.clear();
            }
            let images = ui.images_generation();
            let stale = surface.drawn_generation != Some(surface.batch.quads_generation)
                || surface.drawn_images != images;
            let redraw =
                ((stale || panel.settings.live) && view_slot == 0) || surface.drawn_generation.is_none();
            if redraw && let Some((texture, view)) = surface.texture.clone() {
                clear_transparent(ctx.encoder, &view);
                ui.draw_batch(
                    &mut surface.batch,
                    &device,
                    ctx.encoder,
                    &view,
                    Some(&texture),
                    PANEL_FORMAT,
                    w,
                    h,
                    false,
                );
                surface.drawn_generation = Some(surface.batch.quads_generation);
                surface.drawn_images = images;
                surface.draws += 1;
            }
            let alpha = panel.alpha_at(eye);
            if alpha <= 0.0 {
                ui.targets.insert(key, surface);
                continue;
            }
            let clip_from_unit = ctx.clip_from_world * Mat4::from(panel.unit_to_world(eye));
            let uniforms = panel_uniforms(
                clip_from_unit,
                ctx.viewport,
                depth_size,
                panel.settings.depth_test,
                alpha,
                encoding,
                paper_white,
            );
            while surface.uniforms.len() <= view_slot {
                let Some((_, view)) = surface.texture.as_ref() else {
                    break;
                };
                let buffer = dev.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("ui world panel uniforms"),
                    size: PANEL_UNIFORM_BYTES,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bind_group = dev.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("ui world panel bg"),
                    layout: &panel_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(view),
                        },
                    ],
                });
                surface.uniforms.push((buffer, bind_group));
            }
            if let Some((buffer, bind_group)) = surface.uniforms.get(view_slot) {
                ctx.write_buffer(buffer, 0, bytemuck::cast_slice(&uniforms));
                draws.push((key, bind_group.clone()));
            }
            surface.composites += 1;
            ui.targets.insert(key, surface);
        }
        drop(ui);
        if draws.is_empty() {
            return Ok(());
        }
        let [vx, vy, vw, vh] = ctx.viewport;
        let mut pass = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ui world panels"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: ctx.color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        if vw > 0.0 && vh > 0.0 {
            pass.set_viewport(vx, vy, vw, vh, 0.0, 1.0);
        }
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(1, &depth_bg, &[]);
        for (_, bind_group) in &draws {
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
        Ok(())
    }
}

fn clear_transparent(encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
    let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("ui world panel clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    });
}

// ---------------------------------------------------------------------------------------------
// Pointer injection and the agent methods.
// ---------------------------------------------------------------------------------------------

/// A click on a world panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldClick {
    /// The panel's root.
    pub root: Entity,
    /// Where on the panel (`u` right, `v` down).
    pub uv: [f32; 2],
    /// Panel pixel (root-local UI px).
    pub px: [f32; 2],
    /// Distance along the ray (`None` for a direct `entity` + `uv` click).
    pub distance: Option<f32>,
    /// The interactable node hit at `px` (`None` when the click lands on no widget).
    pub node: Option<Entity>,
}

/// Every world panel's root and placement as of the last extract (panels not yet extracted are
/// read from the ECS directly).
fn world_panels(world: &mut World) -> Vec<(Entity, PanelParams, [f32; 2])> {
    let roots: Vec<(Entity, [f32; 2], f32)> = world
        .query::<(Entity, &UiRoot)>()
        .iter(world)
        .filter_map(|(e, r)| match r.target {
            UiTarget::WorldSpace { size, ppm } => Some((e, size, ppm)),
            _ => None,
        })
        .collect();
    let active = crate::output::active_variant(world);
    roots
        .into_iter()
        .filter(|(e, ..)| crate::output::root_active(world, *e, active))
        .map(|(e, size, ppm)| (e, panel_params(world, e, size, ppm), sanitize_panel_size(size)))
        .collect()
}

/// Casts a ray against every world panel and returns the closest hit as `(root, hit, size)`.
#[must_use]
pub fn raycast_panels(
    world: &mut World,
    origin: Vec3,
    direction: Vec3,
) -> Option<(Entity, PanelHit, [f32; 2])> {
    let mut best: Option<(Entity, PanelHit, [f32; 2])> = None;
    for (root, params, size) in world_panels(world) {
        // The billboard turns toward the ray's origin (the eye of a camera ray).
        let matrix = params.unit_to_world(origin);
        if let Some(hit) = ray_panel(origin, direction, &matrix)
            && best.is_none_or(|(b, h, _)| {
                hit.distance < h.distance || (hit.distance == h.distance && root.to_bits() < b.to_bits())
            })
        {
            best = Some((root, hit, size));
        }
    }
    best
}

/// The topmost interactable node of `root` at root-local `px`.
#[must_use]
pub fn hit_test_root(world: &mut World, root: Entity, px: [f32; 2]) -> Option<Entity> {
    let renderer = world.get_resource::<SharedUiRenderer>().cloned();
    let candidates: Vec<HitCandidate> = world
        .query::<(
            Entity,
            &ComputedNode,
            &Interactable,
            Option<&Disabled>,
            Option<&UiTargetNode>,
            Option<&crate::group_layer::UiClipMasks>,
        )>()
        .iter(world)
        .filter(|(_, _, _, _, tag, _)| tag.is_some_and(|t| t.root == root))
        // Rounded clips and image masks (ADR-2866): `px` is already in the root's space.
        .filter(|(.., masks)| {
            masks.is_none_or(|m| {
                m.admits(px, |id, uv| {
                    renderer.as_ref().and_then(|r| r.lock().image_alpha(id, uv))
                })
            })
        })
        .map(|(entity, computed, interactable, disabled, _, _)| HitCandidate {
            entity,
            rect: computed.rect,
            clip: computed.clip_rect,
            pick_through: interactable.pick_through,
            disabled: disabled.is_some() || crate::node::is_disabled(world, entity),
            z: computed.z,
            paint_order: computed.paint_order,
        })
        .collect();
    pick_topmost(candidates.into_iter(), px)
}

/// Clicks world panel `root` at `uv`: hit-tests the root at that pixel and writes a `Click`
/// (left button, one click) on the node hit. Returns what was hit.
pub fn click_panel(
    world: &mut World,
    root: Entity,
    uv: [f32; 2],
    distance: Option<f32>,
) -> Option<WorldClick> {
    let size = match world.get::<UiRoot>(root)?.target {
        UiTarget::WorldSpace { size, .. } => sanitize_panel_size(size),
        _ => return None,
    };
    let px = [uv[0] * size[0], uv[1] * size[1]];
    let node = hit_test_root(world, root, px);
    if let Some(node) = node {
        let _ = world.write_message(UiEvent::new(node, UiEventKind::Click).with_click_count(1));
    }
    Some(WorldClick {
        root,
        uv,
        px,
        distance,
        node,
    })
}

fn err(
    class: &'static str,
    reason: &'static str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> UiBridgeError {
    UiBridgeError {
        class,
        reason,
        message: message.into(),
        hint: hint.into(),
    }
}

fn vec3_param(value: Option<&Value>, name: &str) -> Result<Option<Vec3>, UiBridgeError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let parts: Option<Vec<f32>> = value.as_array().filter(|a| a.len() == 3).and_then(|a| {
        a.iter()
            .map(|v| v.as_f64().filter(|f| f.is_finite()).map(|f| f as f32))
            .collect()
    });
    match parts {
        Some(p) => Ok(Some(Vec3::new(p[0], p[1], p[2]))),
        None => Err(err(
            "invalid_params",
            "ui_bad_param",
            format!("`{name}` must be an array of three finite numbers"),
            "for example [0, 1.6, 4]",
        )),
    }
}

fn uv_param(params: &Value) -> Result<Option<[f32; 2]>, UiBridgeError> {
    let Some(value) = params.get("uv").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let parts: Option<Vec<f32>> = value.as_array().filter(|a| a.len() == 2).and_then(|a| {
        a.iter()
            .map(|v| v.as_f64().filter(|f| f.is_finite()).map(|f| f as f32))
            .collect()
    });
    match parts {
        Some(p) if (0.0..=1.0).contains(&p[0]) && (0.0..=1.0).contains(&p[1]) => Ok(Some([p[0], p[1]])),
        _ => Err(err(
            "invalid_params",
            "ui_bad_param",
            "`uv` must be two numbers in 0..=1 (u right, v down)",
            "[0.5, 0.5] is the panel's centre",
        )),
    }
}

fn entity_param(world: &World, value: &Value, name: &str) -> Result<Entity, UiBridgeError> {
    let bits = value.as_u64().ok_or_else(|| {
        err(
            "invalid_params",
            "ui_bad_param",
            format!("`{name}` must be an entity id (an unsigned integer)"),
            "take the id from ui.query or ui.targets",
        )
    })?;
    let entity = Entity::try_from_bits(bits).ok_or_else(|| {
        err(
            "not_found",
            "ui_node_not_found",
            format!("`{name}` {bits} is not an entity id"),
            "take the id from ui.query or ui.targets",
        )
    })?;
    if world.get_entity(entity).is_err() {
        return Err(err(
            "not_found",
            "ui_node_not_found",
            format!("entity {bits} does not exist"),
            "take the id from ui.query or ui.targets",
        ));
    }
    Ok(entity)
}

fn click_json(click: &WorldClick, occluded: bool, occluder: Option<f32>) -> Value {
    json!({
        "root": click.root.to_bits(),
        "uv": click.uv,
        "px": click.px,
        "distance": click.distance,
        "node": click.node.map(Entity::to_bits),
        "clicked": click.node.is_some() && !occluded,
        "occluded": occluded,
        "occluder_distance": occluder,
    })
}

/// `ui.world_click`: `{entity, uv?}` clicks panel `entity` at `uv` (default the centre);
/// `{origin, direction}` (or `{ray: {origin, direction}}`) casts a ray against every panel and
/// clicks the closest hit. `occluder_distance` (filled in by the agent from the scene pick)
/// rejects a hit behind scene geometry. The `Click` is written for the next `Update`.
///
/// # Errors
/// `ui_bad_param`, `ui_node_not_found`, `ui_not_world_panel`, `ui_world_miss`.
pub fn world_click(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let uv = uv_param(params)?;
    if let Some(entity) = params.get("entity").filter(|v| !v.is_null()) {
        let root = entity_param(world, entity, "entity")?;
        let is_panel = world
            .get::<UiRoot>(root)
            .is_some_and(|r| matches!(r.target, UiTarget::WorldSpace { .. }));
        if !is_panel {
            return Err(err(
                "rejected",
                "ui_not_world_panel",
                format!("entity {} is not a world-space UI root", root.to_bits()),
                "pass a root whose target is world (ui.targets lists them), or use ui.click for window UI",
            ));
        }
        let click = click_panel(world, root, uv.unwrap_or([0.5, 0.5]), None)
            .ok_or_else(|| err("not_found", "ui_node_not_found", "the panel vanished", "retry"))?;
        return Ok(click_json(&click, false, None));
    }
    let ray = params.get("ray").filter(|v| v.is_object()).unwrap_or(params);
    let origin = vec3_param(ray.get("origin"), "origin")?;
    let direction = vec3_param(ray.get("direction"), "direction")?;
    let (Some(origin), Some(direction)) = (origin, direction) else {
        return Err(err(
            "invalid_params",
            "ui_bad_param",
            "give `entity` (with optional `uv`), or a ray: `origin` and `direction` (or a screen point `x`/`y` or `u`/`v`)",
            "for example {\"origin\": [0, 1.6, 4], \"direction\": [0, 0, -1]}",
        ));
    };
    if direction.length_squared() <= f32::MIN_POSITIVE {
        return Err(err(
            "invalid_params",
            "ui_bad_param",
            "`direction` must not be zero",
            "point the ray where the panel is",
        ));
    }
    let occluder = match params.get("occluder_distance").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).ok_or_else(|| {
            err(
                "invalid_params",
                "ui_bad_param",
                "`occluder_distance` must be a non-negative number",
                "leave it out: the agent fills it from the scene pick",
            )
        })? as f32),
    };
    let Some((root, hit, _)) = raycast_panels(world, origin, direction) else {
        return Err(err(
            "not_found",
            "ui_world_miss",
            "the ray hits no world-space UI panel",
            "aim at a panel (ui.targets lists their transforms and sizes)",
        ));
    };
    // A scene surface nearer than the panel (beyond a small bias for panels mounted on a mesh)
    // takes the click.
    const BIAS: f32 = 0.01;
    let occluded = occluder.is_some_and(|d| d + BIAS < hit.distance);
    if occluded {
        let size = root_target_size(world, root).unwrap_or([1.0, 1.0]);
        let click = WorldClick {
            root,
            uv: hit.uv,
            px: [hit.uv[0] * size[0], hit.uv[1] * size[1]],
            distance: Some(hit.distance),
            node: hit_test_root(world, root, [hit.uv[0] * size[0], hit.uv[1] * size[1]]),
        };
        return Ok(click_json(&click, true, occluder));
    }
    let click = click_panel(world, root, hit.uv, Some(hit.distance))
        .ok_or_else(|| err("not_found", "ui_node_not_found", "the panel vanished", "retry"))?;
    Ok(click_json(&click, false, occluder))
}

fn target_json(world: &World, root: Entity) -> Value {
    let Some(r) = world.get::<UiRoot>(root) else {
        return Value::Null;
    };
    let size = root_target_size(world, root);
    match r.target {
        UiTarget::Window(window) => {
            json!({ "root": root.to_bits(), "target": "window", "window": window.map(Entity::to_bits) })
        }
        UiTarget::Camera(camera) => {
            let cam = world.get::<Camera>(camera);
            let texture = cam.and_then(|c| match c.target {
                RenderTarget::Texture { asset_id } => Some(asset_id),
                RenderTarget::Window => None,
            });
            json!({
                "root": root.to_bits(),
                "target": "camera",
                "camera": camera.to_bits(),
                "camera_found": cam.is_some(),
                "texture": texture,
                "size": size,
            })
        }
        UiTarget::WorldSpace { size: px, ppm } => {
            let params = panel_params(world, root, px, ppm);
            let s = params.settings;
            json!({
                "root": root.to_bits(),
                "target": "world",
                "size": size,
                "ppm": ppm,
                "size_m": params.size_m,
                "position": Vec3::from(params.transform.translation).to_array(),
                "billboard": s.billboard.as_str(),
                "pivot": s.pivot,
                "fade": s.fade,
                "depth_test": s.depth_test,
                "opacity": s.opacity,
                "live": s.live,
            })
        }
    }
}

/// `ui.targets`: every UI root with its target, size and (world panels) placement, plus the
/// offscreen lists the renderer holds and the frame passes registered for them.
#[must_use]
pub fn targets(world: &mut World) -> Value {
    let mut roots: Vec<Entity> = world
        .query_filtered::<Entity, With<UiRoot>>()
        .iter(world)
        .collect();
    roots.sort_by_key(|e| e.to_bits());
    let list: Vec<Value> = roots.iter().map(|r| target_json(world, *r)).collect();
    let surfaces: Vec<Value> = world
        .get_resource::<SharedUiRenderer>()
        .map(|ui| {
            let guard = ui.lock();
            guard
                .targets
                .iter()
                .map(|(key, s)| {
                    let (kind, id) = match key {
                        UiTargetKey::CameraTexture(id) => ("camera_texture", *id),
                        UiTargetKey::World(bits) => ("world", *bits),
                    };
                    json!({
                        "kind": kind,
                        "id": id,
                        "root": s.root.map(Entity::to_bits),
                        "size": s.size,
                        "quads": s.batch.quads.len(),
                        "draws": s.draws,
                        "composites": s.composites,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let passes: Vec<String> = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map(|r| {
            r.0.registered_passes()
                .into_iter()
                .filter(|n| n == WORLD_PASS_NAME || n == CAMERA_PASS_NAME)
                .collect()
        })
        .unwrap_or_default();
    json!({ "roots": list, "surfaces": surfaces, "passes": passes })
}

fn parse_target(world: &World, value: &Value) -> Result<(UiTarget, Option<WorldPanel>), UiBridgeError> {
    let bad = |message: &str| {
        err(
            "invalid_params",
            "ui_bad_target",
            message.to_owned(),
            "`target` is \"window\", {\"camera\": <entity>} or {\"world\": {\"size\": [w, h], \"ppm\": n}}",
        )
    };
    if value.as_str() == Some("window") {
        return Ok((UiTarget::Window(None), None));
    }
    let Some(obj) = value.as_object() else {
        return Err(bad("`target` must be \"window\" or an object"));
    };
    if let Some(camera) = obj.get("camera") {
        let camera = entity_param(world, camera, "camera")?;
        if world.get::<Camera>(camera).is_none() {
            return Err(err(
                "rejected",
                "ui_not_a_camera",
                format!("entity {} has no Camera", camera.to_bits()),
                "pass a camera entity (render.cameras or scene queries list them)",
            ));
        }
        return Ok((UiTarget::Camera(camera), None));
    }
    let Some(spec) = obj.get("world").and_then(Value::as_object) else {
        return Err(bad("`target` object needs `camera` or `world`"));
    };
    let pair = |v: Option<&Value>| -> Option<[f32; 2]> {
        let a = v?.as_array().filter(|a| a.len() == 2)?;
        let x = a[0].as_f64().filter(|f| f.is_finite())? as f32;
        let y = a[1].as_f64().filter(|f| f.is_finite())? as f32;
        // The f64 finitude check above does not catch a value that overflows (or underflows
        // to zero) the f32 the panel stores; a non-finite pivot or fade breaks the transform
        // and the fade math downstream.
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        Some([x, y])
    };
    let size = pair(spec.get("size"))
        .filter(|s| s.iter().all(|v| (1.0..=MAX_PANEL_EDGE).contains(v)))
        .ok_or_else(|| bad("`world.size` must be two numbers from 1 to 4096 (UI px)"))?;
    let ppm = spec
        .get("ppm")
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite() && *f > 0.0)
        .map(|f| f as f32)
        .filter(|f| f.is_finite() && *f > 0.0)
        .ok_or_else(|| bad("`world.ppm` must be a positive number (pixels per metre)"))?;
    let mut panel = WorldPanel::default();
    if let Some(b) = spec.get("billboard").filter(|v| !v.is_null()) {
        panel.billboard = b
            .as_str()
            .and_then(Billboard::parse)
            .ok_or_else(|| bad("`world.billboard` must be none, full or axis_y"))?;
    }
    if let Some(p) = spec.get("pivot").filter(|v| !v.is_null()) {
        panel.pivot = pair(Some(p)).ok_or_else(|| bad("`world.pivot` must be two numbers"))?;
    }
    if let Some(f) = spec.get("fade") {
        panel.fade = if f.is_null() {
            None
        } else {
            Some(
                pair(Some(f))
                    .filter(|r| r[0] >= 0.0 && r[1] >= 0.0)
                    .ok_or_else(|| bad("`world.fade` must be [start, end] in metres, or null"))?,
            )
        };
    }
    if let Some(d) = spec.get("depth_test").filter(|v| !v.is_null()) {
        panel.depth_test = d
            .as_bool()
            .ok_or_else(|| bad("`world.depth_test` must be a boolean"))?;
    }
    if let Some(o) = spec.get("opacity").filter(|v| !v.is_null()) {
        panel.opacity = o
            .as_f64()
            .filter(|f| (0.0..=1.0).contains(f))
            .ok_or_else(|| bad("`world.opacity` must be a number in 0..=1"))? as f32;
    }
    if let Some(l) = spec.get("live").filter(|v| !v.is_null()) {
        panel.live = l.as_bool().ok_or_else(|| bad("`world.live` must be a boolean"))?;
    }
    Ok((UiTarget::WorldSpace { size, ppm }, Some(panel)))
}

/// `ui.set_target`: `{root, target}` moves root `root` (an id, or the node filters of
/// `ui.click`) to `target`: `"window"`, `{"camera": <entity>}` or `{"world": {size, ppm,
/// billboard?, pivot?, fade?, depth_test?, opacity?, live?}}`. A world target also writes the
/// root's [`WorldPanel`] and switches its scale mode to one UI px per texel. Takes effect on the
/// next frame.
///
/// # Errors
/// `ui_node_required`, `ui_node_not_found`, `ui_not_a_root`, `ui_bad_target`, `ui_not_a_camera`.
pub fn set_target(world: &mut World, params: &Value) -> Result<Value, UiBridgeError> {
    let root = match params.get("root").filter(|v| !v.is_null()) {
        Some(v) if v.is_u64() => entity_param(world, v, "root")?,
        Some(v) if v.is_string() => {
            let name = v.as_str().unwrap_or_default().to_owned();
            crate::agent_bridge::single_node(world, &json!({ "name": name }))?
        }
        Some(_) => {
            return Err(err(
                "invalid_params",
                "ui_bad_param",
                "`root` must be an entity id or a node name",
                "ui.targets lists the roots",
            ));
        }
        None => crate::agent_bridge::single_node(world, params)?,
    };
    if world.get::<UiRoot>(root).is_none() {
        return Err(err(
            "rejected",
            "ui_not_a_root",
            format!("entity {} is not a UI root", root.to_bits()),
            "pass the UiRoot of the tree (ui.targets lists the roots)",
        ));
    }
    let target = params.get("target").ok_or_else(|| {
        err(
            "invalid_params",
            "ui_bad_target",
            "`target` is required",
            "`target` is \"window\", {\"camera\": <entity>} or {\"world\": {\"size\": [w, h], \"ppm\": n}}",
        )
    })?;
    let (target, panel) = parse_target(world, target)?;
    if let Some(mut r) = world.get_mut::<UiRoot>(root) {
        r.target = target;
        if panel.is_some() {
            r.scale_mode = crate::scale::ScaleMode::ConstantPixelSize;
        }
    }
    if let Some(panel) = panel {
        world.entity_mut(root).insert(panel);
    }
    world.resource_mut::<crate::layout::LayoutDirty>().mark();
    Ok(target_json(world, root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(transform: Affine3A, settings: WorldPanel) -> PanelParams {
        PanelParams {
            transform,
            size_m: [2.0, 1.0],
            settings,
        }
    }

    #[test]
    fn unit_square_maps_to_the_panel_rect_around_the_pivot() {
        let p = panel(
            Affine3A::from_translation(Vec3::new(1.0, 2.0, 3.0)),
            WorldPanel::default(),
        );
        let m = p.unit_to_world(Vec3::ZERO);
        let tl = m.transform_point3(Vec3::new(0.0, 0.0, 0.0));
        let br = m.transform_point3(Vec3::new(1.0, 1.0, 0.0));
        assert!((tl - Vec3::new(0.0, 2.5, 3.0)).length() < 1e-5, "{tl}");
        assert!((br - Vec3::new(2.0, 1.5, 3.0)).length() < 1e-5, "{br}");
    }

    #[test]
    fn a_ray_hits_the_expected_uv_and_misses_outside() {
        let p = panel(Affine3A::IDENTITY, WorldPanel::default());
        let m = p.unit_to_world(Vec3::new(0.0, 0.0, 5.0));
        let hit = ray_panel(Vec3::new(0.5, 0.25, 5.0), Vec3::new(0.0, 0.0, -2.0), &m).expect("hit");
        assert!((hit.distance - 5.0).abs() < 1e-5);
        assert!(
            (hit.uv[0] - 0.75).abs() < 1e-5 && (hit.uv[1] - 0.25).abs() < 1e-5,
            "{hit:?}"
        );
        // Back face counts; behind the origin, outside and parallel do not.
        assert!(ray_panel(Vec3::new(0.0, 0.0, -5.0), Vec3::Z, &m).is_some());
        assert!(ray_panel(Vec3::new(0.0, 0.0, 5.0), Vec3::Z, &m).is_none());
        assert!(ray_panel(Vec3::new(1.5, 0.0, 5.0), Vec3::NEG_Z, &m).is_none());
        assert!(ray_panel(Vec3::new(0.0, 0.0, 5.0), Vec3::X, &m).is_none());
    }

    #[test]
    fn billboards_face_the_eye() {
        let settings = WorldPanel {
            billboard: Billboard::Full,
            ..WorldPanel::default()
        };
        let p = panel(Affine3A::IDENTITY, settings);
        let eye = Vec3::new(4.0, 3.0, 0.0);
        let m = p.unit_to_world(eye);
        // `v` runs down the panel, so the front normal is `u × -v`.
        let normal = Vec3::from(m.matrix3.x_axis)
            .cross(-Vec3::from(m.matrix3.y_axis))
            .normalize();
        assert!((normal - eye.normalize()).length() < 1e-4, "{normal}");
        let axis = panel(
            Affine3A::IDENTITY,
            WorldPanel {
                billboard: Billboard::AxisY,
                ..WorldPanel::default()
            },
        )
        .unit_to_world(eye);
        let up = -Vec3::from(axis.matrix3.y_axis).normalize();
        assert!((up - Vec3::Y).length() < 1e-5, "axis_y keeps world up: {up}");
    }

    #[test]
    fn fade_is_linear_between_start_and_end() {
        assert_eq!(fade_factor(1.0, 2.0, 4.0), 1.0);
        assert!((fade_factor(3.0, 2.0, 4.0) - 0.5).abs() < 1e-6);
        assert_eq!(fade_factor(5.0, 2.0, 4.0), 0.0);
        assert_eq!(fade_factor(3.0, 4.0, 4.0), 1.0);
        assert_eq!(fade_factor(f32::NAN, 2.0, 4.0), 0.0);
    }

    #[test]
    fn both_composite_shader_variants_validate() {
        let single = world_shader_variant(false);
        let multi = world_shader_variant(true);
        assert!(multi.contains("texture_depth_multisampled_2d;"));
        regia_render::validate_wgsl_modules(&[
            ("ui-world.wgsl", single.as_str()),
            ("ui-world.wgsl (msaa depth)", multi.as_str()),
        ])
        .expect("ui-world.wgsl validates");
    }

    #[test]
    fn billboard_names_round_trip() {
        for b in [Billboard::None, Billboard::Full, Billboard::AxisY] {
            assert_eq!(Billboard::parse(b.as_str()), Some(b));
        }
        assert_eq!(Billboard::parse("spin"), None);
    }

    #[test]
    fn set_target_rejects_world_fields_that_do_not_survive_the_f32_cast() {
        // Audit ui1 correctness 7: the f64 finitude checks do not catch a value that becomes
        // non-finite (or zero) as the f32 the panel stores; a non-finite ppm, pivot or fade
        // breaks panel_params, ray_panel or fade_factor downstream.
        let world = World::new();
        assert!(parse_target(&world, &json!({ "world": { "size": [100, 100], "ppm": 50 } })).is_ok());
        for (name, value) in [
            (
                "ppm overflow",
                json!({ "world": { "size": [100, 100], "ppm": 1e40 } }),
            ),
            (
                "ppm underflow to zero",
                json!({ "world": { "size": [100, 100], "ppm": 1e-50 } }),
            ),
            (
                "pivot overflow",
                json!({ "world": { "size": [100, 100], "ppm": 50, "pivot": [1e40, 0.5] } }),
            ),
            (
                "fade overflow",
                json!({ "world": { "size": [100, 100], "ppm": 50, "fade": [0, 1e40] } }),
            ),
        ] {
            assert!(
                parse_target(&world, &value).is_err(),
                "{name} must be refused: {value}"
            );
        }
    }

    #[test]
    fn uniforms_pack_in_shader_order() {
        let u = panel_uniforms(
            Mat4::IDENTITY,
            [1.0, 2.0, 3.0, 4.0],
            [64.0, 32.0],
            true,
            0.5,
            crate::render::OutputEncoding::Pq,
            203.0,
        );
        assert_eq!(u[0], 1.0);
        assert_eq!(&u[16..20], &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(&u[20..26], &[64.0, 32.0, 1.0, 0.5, 3.0, 203.0]);
        assert_eq!(std::mem::size_of_val(&u) as u64, PANEL_UNIFORM_BYTES);
    }

    #[test]
    fn refresh_targets_is_quiet_when_roots_stay() {
        use regia_scene::GlobalTransform;

        let mut world = World::new();
        let renderer = SharedUiRenderer(UiRenderer::new_handle());
        let root = world
            .spawn((
                UiRoot::world_space([200.0, 100.0], 100.0),
                GlobalTransform(Affine3A::IDENTITY),
            ))
            .id();
        world.insert_resource(crate::extract::UiDamage(false));
        refresh_targets(&mut world, &renderer);
        world.clear_trackers();
        let locks = refresh_lock_count();
        refresh_targets(&mut world, &renderer);
        assert_eq!(
            refresh_lock_count(),
            locks,
            "the second refresh locked the renderer"
        );
        assert!(
            !world
                .get_resource_ref::<TargetPlacements>()
                .expect("placements")
                .is_changed(),
            "the second refresh replaced TargetPlacements"
        );
        world
            .entity_mut(root)
            .insert(GlobalTransform(Affine3A::from_translation(Vec3::new(
                1.0, 0.0, 0.0,
            ))));
        world.insert_resource(crate::extract::UiDamage(false));
        refresh_targets(&mut world, &renderer);
        let panel = renderer
            .lock()
            .targets
            .values()
            .next()
            .expect("surface")
            .panel
            .expect("panel");
        assert!((panel.transform.translation.x - 1.0).abs() < 1e-5, "{panel:?}");
        assert!(
            !world.resource::<crate::extract::UiDamage>().0,
            "a moved panel set UiDamage"
        );
    }

    /// Corr-ui-b: a world panel redraws its texture only on a new quads generation (or when
    /// `live`), and the clean-frame patches (material time, video seek thumbs) changed its quads
    /// without one, so an animated material or a seek thumb on a world panel stood still.
    #[test]
    fn patched_quads_restale_a_world_panel() {
        use regia_scene::GlobalTransform;

        let mut world = World::new();
        let renderer = SharedUiRenderer(UiRenderer::new_handle());
        world.spawn((
            UiRoot::world_space([200.0, 100.0], 100.0),
            GlobalTransform(Affine3A::IDENTITY),
        ));
        world.insert_resource(crate::extract::UiDamage(false));
        refresh_targets(&mut world, &renderer);
        let mut ui = renderer.lock();
        let key = *ui.targets.keys().next().expect("a world panel surface");
        let shine = crate::ui_material::UiMaterial::new(crate::ui_material::UiMaterialKind::Shine)
            .quad([0.0, 0.0, 20.0, 20.0], [0.0; 4]);
        let thumb = UiQuad::fill([40.0, 0.0], [8.0, 8.0], crate::color::UiColor::WHITE);
        ui.set_target_quads(BTreeMap::from([(key, vec![shine, thumb])]));
        let drawn = ui.targets[&key].batch.quads_generation;
        assert!(ui.patch_material_angles(0.016));
        let patched = ui.targets[&key].batch.quads_generation;
        assert_ne!(
            patched, drawn,
            "patched material time must restale the panel texture"
        );
        let moved = UiQuad::fill([44.0, 0.0], [8.0, 8.0], crate::color::UiColor::WHITE);
        assert!(ui.replace_quads_matching(&[([40.0, 0.0], [8.0, 8.0])], &[moved]));
        assert_ne!(
            ui.targets[&key].batch.quads_generation, patched,
            "a moved seek thumb must restale the panel texture"
        );
    }

    #[test]
    fn tag_target_nodes_skips_a_stable_tree() {
        use crate::node::UiNode;

        let mut world = World::new();
        let root_a = world.spawn(UiRoot::world_space([10.0, 10.0], 100.0)).id();
        let root_b = world.spawn(UiRoot::world_space([10.0, 10.0], 100.0)).id();
        let kept = world.spawn(UiNode).id();
        let moved = world.spawn(UiNode).id();
        let child = world.spawn(UiNode).id();
        world.entity_mut(root_a).add_child(kept);
        world.entity_mut(root_a).add_child(moved);
        world.entity_mut(moved).add_child(child);
        tag_target_nodes(&mut world);
        world.clear_trackers();
        let walks = tag_walk_count();
        tag_target_nodes(&mut world);
        assert_eq!(tag_walk_count(), walks, "a stable tree was walked");
        let changed = |world: &World, e: Entity| {
            world
                .entity(e)
                .get_ref::<UiTargetNode>()
                .expect("tag")
                .is_changed()
        };
        assert!(!changed(&world, kept));
        assert!(!changed(&world, moved));
        assert!(!changed(&world, child));

        world.entity_mut(root_b).add_child(moved);
        tag_target_nodes(&mut world);
        assert!(tag_walk_count() > walks, "a reparent did not retag");
        assert!(!changed(&world, kept), "a sibling was retagged");
        assert!(changed(&world, moved), "the moved node was not retagged");
        assert!(changed(&world, child), "the moved subtree was not retagged");
        assert_eq!(world.get::<UiTargetNode>(moved).expect("moved").root, root_b);
        assert_eq!(world.get::<UiTargetNode>(child).expect("child").root, root_b);
        assert_eq!(world.get::<UiTargetNode>(kept).expect("kept").root, root_a);
    }

    fn depth_texture(device: &wgpu::Device, samples: u32) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("ui world depth test"),
            size: wgpu::Extent3d {
                width: 4,
                height: 4,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: samples,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    }

    /// The world-panel pass creates one depth bind group per distinct depth view, not one per record.
    #[test]
    fn world_panel_reuses_depth_bind_group() {
        let device = RenderDevice::shared_headless()
            .unwrap_or_else(|err| panic!("no headless GPU (install Mesa lavapipe): {err}"));
        let dev = regia_render::TrackedDevice::observe(&device.device);
        let mut pass = WorldPanelPass::new(UiRenderer::new_handle(), device.clone());
        let layouts = {
            let gpu = pass.ensure_gpu(&dev);
            [gpu.depth_bgl[0].clone(), gpu.depth_bgl[1].clone()]
        };
        let before = regia_render::frame_path_counters().bind_groups;
        let depth = depth_texture(&device.device, 1);
        let view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let _first = pass.cached_depth_bind_group(&dev, &layouts[0], &view, false);
        let after_first = regia_render::frame_path_counters().bind_groups;
        assert_eq!(
            after_first,
            before + 1,
            "the first depth view creates one bind group"
        );
        let _same = pass.cached_depth_bind_group(&dev, &layouts[0], &view, false);
        assert_eq!(
            regia_render::frame_path_counters().bind_groups,
            after_first,
            "the same depth view created another bind group"
        );
        let other = depth_texture(&device.device, 1);
        let other_view = other.create_view(&wgpu::TextureViewDescriptor::default());
        let _other = pass.cached_depth_bind_group(&dev, &layouts[0], &other_view, false);
        assert_eq!(
            regia_render::frame_path_counters().bind_groups,
            after_first + 1,
            "a new depth view must create one bind group"
        );
        let multi = depth_texture(&device.device, 4);
        let multi_view = multi.create_view(&wgpu::TextureViewDescriptor::default());
        let _multi = pass.cached_depth_bind_group(&dev, &layouts[1], &multi_view, true);
        assert_eq!(
            regia_render::frame_path_counters().bind_groups,
            after_first + 2,
            "a multisampled depth view must create one bind group"
        );
    }
}
