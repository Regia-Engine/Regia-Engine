// SPDX-License-Identifier: MPL-2.0
//! The Viewport panel: one or four panes (perspective + top/front/side) rendering the real
//! game frame ([`crate::render`]), editor camera (orbit/fly/pan), GPU picking and box select
//! through the entity-id buffer ([`crate::picking`]), transform gizmos, overlays
//! ([`crate::overlays`]), camera bookmarks, possess/eject in play, and a stats overlay
//! (`docs/specs/19-editor.md` §§ 2–4).
//!
//! The camera math (orbit target/distance/yaw/pitch, fly movement, view/projection matrices)
//! is pure and unit-tested; the panel wires it to egui input, [`transform_gizmo_egui`] and the
//! [`UndoStack`]. Without a GPU the panel falls back to CPU picking
//! ([`crate::viewport_ops::cpu_pick`]: a ray against each drawn mesh's BVH and the terrain,
//! ADR-7204). [`pick_entities`] is the older sphere test, kept for callers.

use crate::spacemouse::{
    GesturePhase, NavContext, NavDelta, ObjectGesture, SpaceMouseAction, SpaceMouseNav, SpaceMouseState,
    actions as spacemouse_actions, nav,
};
use regia_core::math::{Mat4, Quat, Vec2, Vec3, Vec4};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::ecs::message::MessageCursor;
use regia_ecs::prelude::*;
use regia_render::gizmos::GizmoConfigGroup;
use regia_scene::prelude::{GlobalTransform, Transform};

use crate::command::{SetTransform, TransformGesture, UndoStack};
use crate::selection::Selection;
use crate::viewport_instance::{InstancePane, LinkSync, ViewportInstance, ViewportInstances};
use crate::views::ViewportLayout;
use transform_gizmo_egui::mint;

/// The editor camera. Orbit is Alt+LMB drag, and RMB drag without Alt (half scale).
/// Scroll with no button dollies ([`Self::zoom`]); RMB+scroll changes fly speed. Fly is
/// RMB plus the fly chords (default WASD and E up / Q down). The camera is also the
/// look-through view for rendering.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct EditorCamera {
    /// Orbit/fly target point in world space.
    pub target: Vec3,
    /// Distance from the target.
    pub distance: f32,
    /// Yaw around +Y, radians.
    pub yaw: f32,
    /// Pitch above the horizon, radians.
    pub pitch: f32,
    /// Fly speed, metres per second.
    pub speed: f32,
    /// Perspective vertical field of view, degrees.
    pub fov_deg: f32,
    /// The perspective view's near clip plane, meters ([`Self::NEAR`] by default; the
    /// `camera.near_clip` preference, then per pane; ADR-7755).
    pub near: f32,
    /// The perspective view's far clip plane, meters ([`crate::views::PERSPECTIVE_FAR`] by
    /// default; the `camera.far_clip` preference, then per pane; ADR-7755).
    pub far: f32,
}

impl Default for EditorCamera {
    fn default() -> Self {
        Self {
            target: Vec3::new(0.0, 1.0, 0.0),
            distance: 8.0,
            yaw: 0.6,
            pitch: 0.45,
            speed: 6.0,
            fov_deg: 60.0,
            near: Self::NEAR,
            far: crate::views::PERSPECTIVE_FAR,
        }
    }
}

/// The clip planes a perspective pane accepts: near in `0.001..=100` m, far in `1..=100000` m,
/// and far at least ten times near (depth precision).
pub const CLIP_RANGE: ((f32, f32), (f32, f32)) = ((0.001, 100.0), (1.0, 100_000.0));

/// Checks a near/far pair against [`CLIP_RANGE`].
///
/// # Errors
///
/// A message naming the accepted range.
pub fn check_clip(near: f32, far: f32) -> Result<(), String> {
    let ((near_min, near_max), (far_min, far_max)) = CLIP_RANGE;
    if !near.is_finite() || !(near_min..=near_max).contains(&near) {
        return Err(format!("near {near} is outside {near_min}..={near_max} m"));
    }
    if !far.is_finite() || !(far_min..=far_max).contains(&far) {
        return Err(format!("far {far} is outside {far_min}..={far_max} m"));
    }
    if far < near * 10.0 {
        return Err(format!("far {far} must be at least ten times near {near}"));
    }
    Ok(())
}

impl EditorCamera {
    /// The camera's eye position.
    #[must_use]
    pub fn eye(&self) -> Vec3 {
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        self.target + self.distance * Vec3::new(cos_pitch * sin_yaw, sin_pitch, cos_pitch * cos_yaw)
    }

    /// The world-space forward direction (eye → target).
    #[must_use]
    pub fn forward(&self) -> Vec3 {
        (self.target - self.eye()).normalize_or_zero()
    }

    /// The world-space right direction.
    #[must_use]
    pub fn right(&self) -> Vec3 {
        self.forward().cross(Vec3::Y).normalize_or_zero()
    }

    /// Right-handed view matrix (world → view).
    #[must_use]
    #[allow(deprecated)] // facade exposes only the deprecated glam spellings (docs/03 § 3)
    pub fn view(&self) -> Mat4 {
        Mat4::look_at_rh(self.eye(), self.target, Vec3::Y)
    }

    /// Near plane, metres. The viewport renderer uses the same value so the
    /// gizmo and the grid sit on the rendered picture.
    pub const NEAR: f32 = 0.1;
    /// Far plane of [`Self::projection`], metres. Picking and
    /// [`project_to_screen`] clip with this 100 m plane.
    ///
    /// The perspective viewport does not. [`crate::views::ViewCamera::of`] sets
    /// that camera's far plane to [`crate::views::PERSPECTIVE_FAR`] (4000 m), and
    /// the backdrop sky is drawn just inside that distance.
    pub const FAR: f32 = 100.0;

    /// Right-handed projection matrix (view → clip), [`Self::NEAR`]..[`Self::FAR`].
    #[must_use]
    #[allow(deprecated)] // facade exposes only the deprecated glam spellings (docs/03 § 3)
    pub fn projection(&self, aspect: f32) -> Mat4 {
        Mat4::perspective_rh(self.fov_deg.to_radians(), aspect.max(0.01), Self::NEAR, Self::FAR)
    }

    /// World → clip.
    #[must_use]
    pub fn view_projection(&self, aspect: f32) -> Mat4 {
        self.projection(aspect) * self.view()
    }

    /// Applies one orbit drag (`dx`, `dy` in egui points; positive = right/up).
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * 0.01;
        self.pitch = (self.pitch + dy * 0.01).clamp(-1.45, 1.45);
    }

    /// Applies a pan drag: moves the target in the view plane.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        let right = self.right();
        let up = right.cross(self.forward()).normalize_or_zero();
        let scale = self.distance * 0.002;
        self.target += right * (-dx * scale) + up * (dy * scale);
    }

    /// Zooms by a multiplicative factor (scroll).
    pub fn zoom(&mut self, factor: f32) {
        self.distance = (self.distance * factor).clamp(0.2, 200.0);
    }

    /// One fly step. `dt` is seconds. `forward`, `right` and `up` are unitless axes in
    /// [-1, 1]. `up` moves the target along world +Y. The viewport feeds the axes from the
    /// fly chords while RMB is held; the defaults are W/S, A/D and E/Q
    /// (`camera.fly_up` / `camera.fly_down`), not Space and Ctrl.
    /// Moves the target with the eye, keeping the orbit distance.
    pub fn fly(&mut self, dt: f32, forward: f32, right: f32, up: f32) {
        let step = self.speed * dt;
        let f = self.forward();
        let r = self.right();
        let motion = f * forward + r * right + Vec3::Y * up;
        if motion.length_squared() > 0.0 {
            self.target += motion.normalize() * step;
        }
    }

    /// Frames `positions` (selection focus, `F`): centres the target and fits the distance.
    pub fn frame(&mut self, positions: &[Vec3]) {
        if positions.is_empty() {
            return;
        }
        let center = positions.iter().sum::<Vec3>() / positions.len() as f32;
        let radius = positions
            .iter()
            .map(|p| (*p - center).length())
            .fold(0.5f32, f32::max);
        self.target = center;
        self.distance = (radius * 3.0).clamp(1.0, 200.0);
    }
}

/// The world translation of an entity (its `GlobalTransform` when propagated, else its local
/// `Transform`).
#[must_use]
pub fn world_position(world: &World, entity: Entity) -> Option<Vec3> {
    if let Some(global) = world.get::<GlobalTransform>(entity) {
        return Some(global.translation());
    }
    world.get::<Transform>(entity).map(|t| t.translation)
}

/// The picking ray through a viewport pixel: `pixel` is relative to the viewport's top-left,
/// `size` its logical size.
#[must_use]
pub fn pick_ray(camera: &EditorCamera, pixel: Vec2, size: Vec2) -> (Vec3, Vec3) {
    let aspect = size.x / size.y.max(1.0);
    let ndc_x = 2.0 * pixel.x / size.x - 1.0;
    let ndc_y = 1.0 - 2.0 * pixel.y / size.y;
    let inverse = camera.view_projection(aspect).inverse();
    let unproject = |z: f32| {
        let clip = Vec4::new(ndc_x, ndc_y, z, 1.0);
        let world = inverse * clip;
        world.truncate() / world.w
    };
    let near_point = unproject(0.0);
    let far_point = unproject(1.0);
    let direction = (far_point - near_point).normalize_or_zero();
    (camera.eye(), direction)
}

/// Sphere-picking result: the hit entity and the ray parameter. (Not the surface hit of
/// [`crate::picking::PickHit`].)
pub struct SpherePickHit {
    /// The picked entity.
    pub entity: Entity,
    /// Distance along the ray, metres.
    pub distance: f32,
}

/// Old name of [`SpherePickHit`], renamed so it no longer collides with [`crate::picking::PickHit`].
#[deprecated(note = "renamed to `SpherePickHit`; the surface hit is `regia_editor::picking::PickHit`")]
pub type PickHit = SpherePickHit;

/// A coarse sphere pick: the nearest entity whose translation sphere (proxy radius from its
/// scale, `min_radius` floor) the ray intersects. The viewport's CPU fallback is
/// [`crate::viewport_ops::cpu_pick`] (a ray against the mesh BVH, ADR-7204); this stays for callers
/// that pick entities without meshes.
#[must_use]
pub fn pick_entities(world: &World, origin: Vec3, direction: Vec3, min_radius: f32) -> Vec<SpherePickHit> {
    let mut hits: Vec<SpherePickHit> = Vec::new();
    if direction.length_squared() < 1e-8 {
        return hits;
    }
    for entity_ref in world.iter_entities() {
        let entity = entity_ref.id();
        if crate::visibility::is_hidden(world, entity) {
            continue; // editor-internal entities are never selectable
        }
        let transform = world.get::<Transform>(entity);
        let Some(position) = world_position(world, entity) else {
            continue;
        };
        let radius = transform
            .map(|t| t.scale.abs().max_element() * 0.5)
            .unwrap_or(0.0)
            .max(min_radius);
        if let Some(t) = ray_sphere(origin, direction, position, radius).filter(|t| t.is_finite()) {
            hits.push(SpherePickHit { entity, distance: t });
        }
    }
    // Total order: non-finite distances are dropped, equal distances break on the entity id so the
    // result does not depend on archetype iteration order.
    hits.sort_by(|a, b| {
        a.distance
            .total_cmp(&b.distance)
            .then_with(|| a.entity.to_bits().cmp(&b.entity.to_bits()))
    });
    hits
}

/// Ray–sphere intersection: the nearest positive ray parameter, or `None`.
/// Scratchapixel, "Ray-Sphere Intersection" (public reference); see `docs/provenance.md`.
#[must_use]
pub fn ray_sphere(origin: Vec3, direction: Vec3, center: Vec3, radius: f32) -> Option<f32> {
    let oc = origin - center;
    let b = oc.dot(direction);
    let c = oc.dot(oc) - radius * radius;
    let discriminant = b * b - c;
    if discriminant < 0.0 {
        return None;
    }
    let sqrt_d = discriminant.sqrt();
    let near = -b - sqrt_d;
    let far = -b + sqrt_d;
    if near >= 0.0 {
        Some(near)
    } else if far >= 0.0 {
        Some(far) // origin inside the sphere
    } else {
        None
    }
}

/// The `(ease key, egui frame)` pairs whose eased camera move already advanced this frame (the
/// UI pass can run twice per frame; the move advances once).
#[derive(Resource, Debug, Default)]
struct EaseFrame(Vec<((u64, usize), u64)>);

/// Viewport tools (`Q` select, `W` translate, `E` rotate, `R` scale).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GizmoMode {
    /// Translate.
    #[default]
    Translate,
    /// Rotate.
    Rotate,
    /// Scale.
    Scale,
    /// Select only: no gizmo; a click picks and a drag box-selects (ADR-7756).
    Select,
}

impl GizmoMode {
    /// Every tool, in key order (Q, W, E, R).
    pub const ALL: [Self; 4] = [Self::Select, Self::Translate, Self::Rotate, Self::Scale];

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Translate => "translate",
            Self::Rotate => "rotate",
            Self::Scale => "scale",
            Self::Select => "select",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }
}

/// The axes a gizmo moves, turns and scales along (ADR-7756; `gizmo.toggle_space`, Ctrl+`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GizmoSpace {
    /// The world axes.
    #[default]
    World,
    /// The active entity's own axes.
    Local,
    /// The active entity's parent's axes (the world axes for a root).
    Parent,
    /// The camera's axes: right, up and toward the eye.
    View,
}

impl GizmoSpace {
    /// Every space, in toggle order.
    pub const ALL: [Self; 4] = [Self::World, Self::Local, Self::Parent, Self::View];

    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::World => "world",
            Self::Local => "local",
            Self::Parent => "parent",
            Self::View => "view",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The toolbar label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::World => "World",
            Self::Local => "Local",
            Self::Parent => "Parent",
            Self::View => "View",
        }
    }

    /// The next space of the toggle (View wraps to World).
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::World => Self::Local,
            Self::Local => Self::Parent,
            Self::Parent => Self::View,
            Self::View => Self::World,
        }
    }
}

/// Gizmo interaction settings (snapping, reference space, tool).
#[derive(Resource, Debug, Clone)]
pub struct GizmoState {
    /// Active tool.
    pub mode: GizmoMode,
    /// Snap to grid steps while dragging (the Ctrl key toggles it live).
    pub snap: bool,
    /// Translation snap step, metres.
    pub snap_distance: f32,
    /// Rotation snap step, degrees.
    pub snap_angle_deg: f32,
    /// Scale snap step.
    pub snap_scale: f32,
    /// The reference space of the gizmo's axes (replaces the old `local` flag, ADR-7756).
    pub space: GizmoSpace,
}

impl Default for GizmoState {
    fn default() -> Self {
        Self {
            mode: GizmoMode::Translate,
            snap: false,
            snap_distance: 0.5,
            snap_angle_deg: 15.0,
            snap_scale: 0.25,
            space: GizmoSpace::World,
        }
    }
}

/// Viewport display settings (show flags, realtime toggle).
#[derive(Resource, Debug, Clone)]
pub struct ViewportSettings {
    /// Show the reference grid.
    pub grid: bool,
    /// Show entity name labels at their projected positions.
    pub labels: bool,
    /// Render every frame (off = render on change only, for idle CPU).
    pub realtime: bool,
    /// Show audio gizmos: emitter markers, and for the selection their range spheres, cones
    /// and reverb-zone volumes ([`crate::audio_gizmo`]).
    pub audio: bool,
    /// Frosted backing under the overlay chips (the stats block, the view label): the Gaussian
    /// sigma in points, 0 for off, the default ([`crate::viewport_blur`], ADR-2129).
    pub overlay_blur: f32,
}

impl Default for ViewportSettings {
    fn default() -> Self {
        Self {
            grid: true,
            labels: true,
            realtime: true,
            audio: true,
            overlay_blur: 0.0,
        }
    }
}

/// The view label in the top right corner of a pane, its anchor and font.
fn view_label_galley(ui: &egui::Ui, label: String) -> std::sync::Arc<egui::Galley> {
    ui.painter().layout_no_wrap(
        label,
        egui::FontId::proportional(11.0),
        egui::Color32::from_gray(200),
    )
}

/// The backing of the view label: its text rect with a small margin.
fn view_label_chip(rect: egui::Rect, galley: &egui::Galley) -> egui::Rect {
    let size = galley.size() + egui::vec2(8.0, 4.0);
    egui::Rect::from_min_size(rect.right_top() + egui::vec2(-2.0 - size.x, 2.0), size)
}

/// The stats block's text and its backing rect in the pane.
fn stats_chip(
    ui: &egui::Ui,
    rect: egui::Rect,
    stats: &ViewportStats,
) -> (std::sync::Arc<egui::Galley>, egui::Rect) {
    let galley = ui.painter().layout_no_wrap(
        stats.lines().join("\n"),
        egui::FontId::monospace(11.0),
        egui::Color32::from_rgb(220, 255, 220),
    );
    let bg = egui::Rect::from_min_size(
        rect.left_top() + egui::vec2(4.0, 4.0),
        galley.size() + egui::vec2(8.0, 6.0),
    );
    (galley, bg)
}

/// Projects a world position to the viewport's pixel space (`None` behind the camera).
#[must_use]
pub fn project_to_screen(camera: &EditorCamera, position: Vec3, size: Vec2) -> Option<Vec2> {
    let aspect = size.x / size.y.max(1.0);
    let clip = camera.view_projection(aspect) * position.extend(1.0);
    if clip.w <= 0.0 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    Some(Vec2::new(
        (ndc.x + 1.0) * 0.5 * size.x,
        (1.0 - ndc.y) * 0.5 * size.y,
    ))
}

/// Live viewport statistics (the stats overlay and `editor.viewport_stats`): frame pacing from
/// the panel's frame times, hitches, and the last pane render's [`crate::render::PaneInfo`].
#[derive(Resource, Debug, Clone, Default)]
pub struct ViewportStats {
    /// Recent frame times, seconds (newest last, up to 240).
    pub frame_times: std::collections::VecDeque<f32>,
    /// Frames counted as hitches since the stats were reset: a frame slower than twice the
    /// median of the recent ones (and slower than 1/30 s), or one whose render created GPU
    /// pipelines after warm-up (a shader compile in the frame path).
    pub hitches: u32,
    /// Frames observed.
    pub frames: u64,
    /// The last rendered pane's information.
    pub last: Option<crate::render::PaneInfo>,
    /// The path the last pane picture took to egui (`None` before the first).
    pub pane_path: Option<crate::pane_texture::PanePath>,
    /// Pane pictures shown since the stats were reset (all panes).
    pub pane_frames: u64,
    /// Egui times (seconds) of the most recent pane pictures (up to 240).
    pub pane_times: std::collections::VecDeque<f64>,
}

impl ViewportStats {
    /// Frames before pipeline creation counts as a hitch (first-use compiles are expected).
    pub const WARM_UP_FRAMES: u64 = 30;

    /// Records one frame: its duration and the pipelines the frame's renders created.
    pub fn record(&mut self, dt: f32, pipelines_created: u64) {
        if !(dt.is_finite() && dt > 0.0) {
            return;
        }
        let median = self.median();
        let slow = median.is_some_and(|m| dt > (2.0 * m).max(1.0 / 30.0));
        let compiled = self.frames >= Self::WARM_UP_FRAMES && pipelines_created > 0;
        if slow || compiled {
            self.hitches += 1;
        }
        self.frames += 1;
        self.frame_times.push_back(dt);
        while self.frame_times.len() > 240 {
            self.frame_times.pop_front();
        }
    }

    /// Records one pane picture shown at egui time `time` (seconds) through `path`.
    pub fn record_pane(&mut self, time: f64, path: crate::pane_texture::PanePath) {
        self.pane_path = Some(path);
        self.pane_frames += 1;
        self.pane_times.push_back(time);
        while self.pane_times.len() > 240 {
            self.pane_times.pop_front();
        }
    }

    /// Pane pictures per second over the recent window, all panes together (0 with fewer than
    /// two samples).
    #[must_use]
    pub fn pane_fps(&self) -> f32 {
        let (Some(first), Some(last)) = (self.pane_times.front(), self.pane_times.back()) else {
            return 0.0;
        };
        let span = last - first;
        if span > 0.0 {
            ((self.pane_times.len() - 1) as f64 / span) as f32
        } else {
            0.0
        }
    }

    /// Median recent frame time (needs ≥ 10 samples).
    #[must_use]
    pub fn median(&self) -> Option<f32> {
        if self.frame_times.len() < 10 {
            return None;
        }
        let mut sorted: Vec<f32> = self.frame_times.iter().copied().collect();
        sorted.sort_by(f32::total_cmp);
        Some(sorted[sorted.len() / 2])
    }

    /// Frames per second over the recent window.
    #[must_use]
    pub fn fps(&self) -> f32 {
        let total: f32 = self.frame_times.iter().sum();
        if total > 0.0 {
            self.frame_times.len() as f32 / total
        } else {
            0.0
        }
    }

    /// The overlay's text lines.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        // The median once there are enough samples, the mean before that.
        let fps = self.fps();
        let frame_ms = self.median().unwrap_or(if fps > 0.0 { 1.0 / fps } else { 0.0 }) * 1000.0;
        let mut lines = vec![format!(
            "{fps:.1} fps ({frame_ms:.2} ms)  hitches {}",
            self.hitches
        )];
        if let Some(path) = self.pane_path {
            lines.push(format!(
                "pane {} · {:.1} pictures/s",
                path.name(),
                self.pane_fps()
            ));
        }
        if let Some(info) = &self.last {
            let g = &info.gpu;
            lines.push(format!(
                "{} · draws {} (culled {}) · meshes {} proxies {} lights {}",
                info.quality,
                info.draws.drawn,
                info.draws.culled,
                info.meshes,
                info.proxies,
                info.local_lights
            ));
            lines.push(format!(
                "GPU {:.2} ms: shadows {:.2} prepass {:.2} fwd {:.2} ssao {:.2} atmo {:.2} fog {:.2} post {:.2}",
                g.frame_ms, g.shadows_ms, g.prepass_ms, g.forward_ms, g.ssao_ms, g.atmosphere_ms, g.fog_ms, g.post_ms
            ));
            // The internal render size and the pane's screen percentage (ADR-7770).
            lines.push(format!(
                "internal {}x{} ({} %) · {}",
                info.render_size[0], info.render_size[1], info.screen_percentage, info.view_mode
            ));
            lines.push(format!(
                "CPU {:.2} ms · pipelines +{} bind groups +{} submits {}{}{}",
                info.cpu_ms,
                info.counters.pipelines,
                info.counters.bind_groups,
                info.counters.submits,
                if info.preview_sun { " · preview sun" } else { "" },
                if info.possessed { " · possessed" } else { "" },
            ));
        }
        lines
    }
}

/// Per-panel state the shell keeps: one interactive gizmo per pane and the box-select drag.
/// Keep one per [`ViewportInstance`] drawn with [`viewport_panel_with`].
pub struct ViewportPanelState {
    /// Gizmos, one per pane (each keeps its own drag state).
    pub gizmos: [transform_gizmo_egui::Gizmo; 4],
    /// Draw the toolbar (quality, layout, game view, show flags) above the panes. Default on;
    /// a lightbox or compare pane may turn it off and drive its instance's layout itself.
    pub toolbar: bool,
    /// Box select in progress: pane index and the drag start (pane-local points).
    drag: Option<(usize, egui::Pos2)>,
    /// The lasso's outline while the box-select drag is a lasso (screen points; ADR-7769).
    lasso: Vec<egui::Pos2>,
    /// Each pane drew its gizmo last frame (a hidden gizmo's focus is stale; ADR-7674).
    gizmo_shown: [bool; 4],
    /// SpaceMouse button actions already handled.
    spacemouse_actions: MessageCursor<SpaceMouseAction>,
}

impl Default for ViewportPanelState {
    fn default() -> Self {
        Self {
            gizmos: std::array::from_fn(|_| transform_gizmo_egui::Gizmo::default()),
            toolbar: true,
            drag: None,
            lasso: Vec::new(),
            gizmo_shown: [false; 4],
            spacemouse_actions: MessageCursor::default(),
        }
    }
}

/// What a click in a pane hit: the GPU id buffer (meshes, proxies) or the terrain, whichever
/// is nearer; `None` on background.
///
/// A thin wrapper over [`pick_in_view`] with the frame's view, image size and id buffer.
#[must_use]
pub fn pick_in_frame(world: &World, frame: &crate::render::PaneFrame, pixel: Vec2) -> Option<(Entity, Vec3)> {
    let size = [frame.image.width() as u32, frame.image.height() as u32];
    pick_in_view(world, &frame.view, size, frame.ids.as_ref(), pixel)
}

/// [`pick_in_frame`] for a pane left on the GPU ([`crate::render::PaneGpuFrame`], which has no
/// CPU image): its view, its target size and its id buffer. Same result as [`pick_in_frame`]
/// on a [`crate::render::PaneFrame`] of the same size, view and ids.
#[must_use]
pub fn pick_in_gpu_frame(
    world: &World,
    frame: &crate::render::PaneGpuFrame,
    pixel: Vec2,
) -> Option<(Entity, Vec3)> {
    pick_in_view(world, &frame.view, frame.size(), frame.ids.as_ref(), pixel)
}

/// What a click hit, from the pieces a pick needs (no pane type): the `view` the ids were
/// rendered with, the pane `size` in pixels (`[width, height]`), and the ids (an
/// [`crate::picking::IdBuffer`], or an [`crate::picking::IdWindow`] around the pointer; `None`
/// casts terrain only). The GPU id buffer's mesh or the terrain, whichever is nearer; `None`
/// on background or when `pixel` is outside the window.
#[must_use]
pub fn pick_in_view<I: crate::picking::IdLookup + ?Sized>(
    world: &World,
    view: &crate::views::ViewCamera,
    size: [u32; 2],
    ids: Option<&I>,
    pixel: Vec2,
) -> Option<(Entity, Vec3)> {
    let size = Vec2::new(size[0] as f32, size[1] as f32);
    let (origin, direction) = view.ray(pixel, size);
    let mesh = ids.and_then(|ids| ids.at(pixel.x.max(0.0) as u32, pixel.y.max(0.0) as u32));
    // The id buffer's distance is from the eye; the ray starts on the near plane.
    let start = (origin - view.eye).length();
    let mesh_t = mesh.map(|(_, d)| d - start);
    let terrain = crate::picking::pick_terrain(world, origin, direction, mesh_t.unwrap_or(view.far));
    match (mesh, terrain) {
        (_, Some((entity, t))) if mesh_t.is_none_or(|m| t < m) => Some((entity, origin + direction * t)),
        (Some((entity, d)), _) => Some((entity, view.eye + direction * d)),
        _ => None,
    }
}

/// A rendered live pane: read back to the CPU, or left in a GPU texture (ADR-1555).
enum PaneDrawn {
    Cpu(crate::render::PaneFrame),
    Gpu(crate::render::PaneGpuFrame),
}

impl PaneDrawn {
    fn view(&self) -> crate::views::ViewCamera {
        match self {
            Self::Cpu(frame) => frame.view,
            Self::Gpu(frame) => frame.view,
        }
    }

    fn size(&self) -> [u32; 2] {
        match self {
            Self::Cpu(frame) => [frame.image.width() as u32, frame.image.height() as u32],
            Self::Gpu(frame) => frame.size(),
        }
    }

    fn info(&self) -> &crate::render::PaneInfo {
        match self {
            Self::Cpu(frame) => &frame.info,
            Self::Gpu(frame) => &frame.info,
        }
    }

    fn ids(&self) -> Option<&crate::picking::IdBuffer> {
        match self {
            Self::Cpu(frame) => frame.ids.as_ref(),
            Self::Gpu(frame) => frame.ids.as_ref(),
        }
    }

    fn path(&self) -> crate::pane_texture::PanePath {
        match self {
            Self::Cpu(_) => crate::pane_texture::PanePath::Cpu,
            Self::Gpu(_) => crate::pane_texture::PanePath::Gpu,
        }
    }
}

/// The surface under a pixel of a rendered pane (Tincal engine request F7): mesh entity,
/// triangle, barycentric weights, world point and normal. The id buffer narrows the cast to the
/// entity it names (plus skinned meshes); without one every mesh is cast. Terrain is not
/// included (see [`pick_in_frame`]).
///
/// A thin wrapper over [`pick_hit_in_view`] with the frame's view, image size and id buffer.
#[must_use]
pub fn pick_hit_in_frame(
    world: &World,
    frame: &crate::render::PaneFrame,
    pixel: Vec2,
    cull: crate::picking::FaceCull,
) -> Option<crate::picking::PickHit> {
    let size = [frame.image.width() as u32, frame.image.height() as u32];
    pick_hit_in_view(world, &frame.view, size, frame.ids.as_ref(), pixel, cull)
}

/// [`pick_hit_in_frame`] for a pane left on the GPU ([`crate::render::PaneGpuFrame`]).
#[must_use]
pub fn pick_hit_in_gpu_frame(
    world: &World,
    frame: &crate::render::PaneGpuFrame,
    pixel: Vec2,
    cull: crate::picking::FaceCull,
) -> Option<crate::picking::PickHit> {
    pick_hit_in_view(world, &frame.view, frame.size(), frame.ids.as_ref(), pixel, cull)
}

/// The surface under a pixel from the pieces a pick needs (see [`pick_in_view`]): the `view`
/// the ids were rendered with, the pane `size` in pixels (`[width, height]`) and the ids (an
/// [`crate::picking::IdBuffer`] or an [`crate::picking::IdWindow`]; `None` casts every mesh).
/// Same result as [`pick_hit_in_frame`] for the same view, size and ids. Terrain is not
/// included, but it hides a mesh behind it.
#[must_use]
pub fn pick_hit_in_view<I: crate::picking::IdLookup + ?Sized>(
    world: &World,
    view: &crate::views::ViewCamera,
    size: [u32; 2],
    ids: Option<&I>,
    pixel: Vec2,
    cull: crate::picking::FaceCull,
) -> Option<crate::picking::PickHit> {
    let size = Vec2::new(size[0] as f32, size[1] as f32);
    let px = (pixel.x.max(0.0) as u32, pixel.y.max(0.0) as u32);
    // Through the pixel's centre, where the id pass sampled it.
    let (origin, direction) = view.ray(Vec2::new(px.0 as f32 + 0.5, px.1 as f32 + 0.5), size);
    crate::picking::pick_hit_along(world, ids, px, view.eye, origin, direction, cull)
}

thread_local! {
    /// Id-buffer requests the viewport panes drawn on this thread made (one per pane frame that
    /// wanted ids; the hover pre-highlight never adds one, ADR-7769). A test probe.
    static PANE_ID_REQUESTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many pane frames drawn on this thread asked for an id buffer (see `PANE_ID_REQUESTS`).
#[must_use]
pub fn pane_id_requests() -> u64 {
    PANE_ID_REQUESTS.with(std::cell::Cell::get)
}

/// Frames the selection's bounds in `camera` (focus, `F`): centres the target and fits the
/// distance to the bounding sphere for the camera's field of view.
pub fn focus_camera(world: &World, camera: &mut EditorCamera, entities: &[Entity]) {
    let Some((min, max)) = crate::overlays::selection_aabb(world, entities) else {
        return;
    };
    let center = (min + max) * 0.5;
    let radius = ((max - min).length() * 0.5).max(0.25);
    camera.target = center;
    let half_fov = (camera.fov_deg.to_radians() * 0.5).max(0.05);
    camera.distance = (radius / half_fov.sin() * 1.1).clamp(0.5, 5000.0);
}

/// One frame of SpaceMouse navigation for a pane (`docs/specs/29-spacemouse.md` § 2.4; the
/// reference integration every Regia Engine app copies): the shared [`nav::step`] in orbit mode with
/// the camera's distance and pitch, the user's overrides and the toggles, applied with
/// [`apply_nav_delta`]. Returns the delta ([`NavDelta::IDENTITY`] without a plugin or input).
pub fn spacemouse_step(
    world: &World,
    camera: &mut EditorCamera,
    kind: crate::views::ViewKind,
    dt: f32,
) -> NavDelta {
    let Some(state) = world.get_resource::<SpaceMouseState>() else {
        return NavDelta::IDENTITY;
    };
    let toggles = world.get_resource::<SpaceMouseNav>().copied().unwrap_or_default();
    let config = world
        .get_resource::<regia_input::InputConfig>()
        .map(|c| c.spacemouse)
        .unwrap_or_default();
    let ctx = NavContext::orbit(camera.distance)
        .with_pitch(camera.pitch)
        .with_config(&config)
        .with_toggles(&toggles);
    let delta = nav::step(state, dt, ctx);
    apply_nav_delta(camera, kind, &delta);
    delta
}

/// Applies a [`NavDelta`] to the editor camera: pan moves the target in the view plane, zoom
/// scales the distance, orbit adds to yaw (eye azimuth, counter-clockwise from above) and pitch
/// (eye elevation). Orthographic panes pan in their plane and zoom; they do not orbit. The
/// editor camera has no roll, so `roll` (horizon lock off) is not applied.
pub fn apply_nav_delta(camera: &mut EditorCamera, kind: crate::views::ViewKind, delta: &NavDelta) {
    let (right, up) = view_axes(camera, kind);
    camera.target += right * delta.pan.x + up * delta.pan.y;
    if delta.zoom != 1.0 {
        camera.zoom(delta.zoom);
    }
    if !kind.is_ortho() {
        camera.yaw += delta.orbit.x;
        camera.pitch = (camera.pitch + delta.orbit.y).clamp(-1.45, 1.45);
    }
}

/// The pane's view-plane axes in world space: `(right, up)`.
fn view_axes(camera: &EditorCamera, kind: crate::views::ViewKind) -> (Vec3, Vec3) {
    // The six orthographic views (ADR-7755) share one table with the pointer pan.
    kind.screen_axes().unwrap_or_else(|| {
        let right = camera.right();
        (right, right.cross(camera.forward()).normalize_or_zero())
    })
}

/// The SpaceMouse Object tool's gesture state (`docs/specs/29-spacemouse.md` § 2.3b): switched by
/// [`SpaceMouseNav::object_tool`] (the `spacemouse.object_tool_toggle` action or the toolbar), it
/// moves the primary selection instead of the camera.
#[derive(Resource, Debug, Default)]
pub struct SpaceMouseObjectTool {
    gesture: ObjectGesture,
    /// The entity and its transform when the gesture began.
    start: Option<(Entity, Transform)>,
}

impl SpaceMouseObjectTool {
    /// True while a gesture is open (until the cap has rested for 0.3 s).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.gesture.is_active()
    }
}

/// One frame of the SpaceMouse Object tool (§ 2.3b): the shared [`nav::step`] delta (orbit
/// context at the camera's distance) moves the primary selection's `Transform` live: `pan` is a
/// translation in the pane's view plane, `orbit` a rotation about the entity's own origin (yaw
/// about world up, pitch about the view's right axis; the object follows the cap), `zoom` and
/// `roll` are unused. A gesture (first deflection until 0.3 s of rest, or the tool switched off)
/// is pushed as exactly one [`TransformGesture`] undo entry. The camera does not move. For a
/// child entity the axes are its parent's space. Returns the delta applied.
pub fn spacemouse_object_step(
    world: &mut World,
    camera: &EditorCamera,
    kind: crate::views::ViewKind,
    dt: f32,
) -> NavDelta {
    let toggles = world.get_resource::<SpaceMouseNav>().copied().unwrap_or_default();
    let state = world
        .get_resource::<SpaceMouseState>()
        .copied()
        .unwrap_or_default();
    let mut tool = world
        .remove_resource::<SpaceMouseObjectTool>()
        .unwrap_or_default();
    let phase = if toggles.object_tool {
        tool.gesture.update(&state, dt)
    } else if tool.gesture.is_active() {
        // Switched off mid-gesture: close the entry now.
        tool.gesture = ObjectGesture::default();
        GesturePhase::Ended
    } else {
        GesturePhase::Idle
    };
    if phase == GesturePhase::Began {
        tool.start = world
            .get_resource::<Selection>()
            .and_then(Selection::primary)
            .and_then(|entity| world.get::<Transform>(entity).map(|t| (entity, *t)));
    }
    let mut delta = NavDelta::IDENTITY;
    if matches!(phase, GesturePhase::Began | GesturePhase::Moving)
        && let Some((entity, _)) = tool.start
    {
        let config = world
            .get_resource::<regia_input::InputConfig>()
            .map(|c| c.spacemouse)
            .unwrap_or_default();
        let ctx = NavContext::orbit(camera.distance)
            .with_config(&config)
            .with_toggles(&toggles);
        delta = nav::step(&state, dt, ctx);
        let (right, up) = view_axes(camera, kind);
        if let Some(mut transform) = world.get_mut::<Transform>(entity) {
            transform.translation += right * delta.pan.x + up * delta.pan.y;
            let spin =
                Quat::from_axis_angle(Vec3::Y, delta.orbit.x) * Quat::from_axis_angle(right, -delta.orbit.y);
            transform.rotation = (spin * transform.rotation).normalize();
        }
    }
    if phase == GesturePhase::Ended
        && let Some((entity, old)) = tool.start.take()
        && let Some(new) = world.get::<Transform>(entity).copied()
        && new != old
    {
        let entry = TransformGesture(SetTransform {
            entity: entity.to_bits(),
            old,
            new,
        });
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(entry));
    }
    world.insert_resource(tool);
    delta
}

/// The editor's home view (`view.home`, SpaceMouse button 2): the start placement, keeping the
/// user's fly speed and field of view.
pub fn home_camera(camera: &mut EditorCamera) {
    let home = EditorCamera::default();
    camera.target = home.target;
    camera.distance = home.distance;
    camera.yaw = home.yaw;
    camera.pitch = home.pitch;
}

/// Handles this frame's SpaceMouse button actions for the active pane (§ 2.5):
/// `view.fit_selection` frames the selection (everything when nothing is selected) and
/// `view.home` resets to [`home_camera`]; `spacemouse.object_tool_toggle` (when an app binds a
/// button to it) flips the Object tool. With `apply` false (a text field has focus) the
/// actions are consumed without effect, so they never fire late. Returns how many were applied.
pub fn handle_spacemouse_actions(
    world: &mut World,
    cursor: &mut MessageCursor<SpaceMouseAction>,
    apply: bool,
) -> usize {
    let Some(messages) = world.get_resource::<Messages<SpaceMouseAction>>() else {
        return 0;
    };
    let actions: Vec<String> = cursor.read(messages).map(|a| a.action.clone()).collect();
    if !apply {
        return 0;
    }
    let mut applied = 0;
    for action in actions {
        if action == spacemouse_actions::OBJECT_TOOL_TOGGLE {
            let mut toggles = world.get_resource_or_insert_with::<SpaceMouseNav>(SpaceMouseNav::default);
            toggles.object_tool = !toggles.object_tool;
            applied += 1;
            continue;
        }
        let mut layout = world
            .get_resource_or_insert_with::<ViewportLayout>(ViewportLayout::default)
            .clone();
        let active = layout.active;
        let (_, mut camera) = layout.pane_camera(world, active);
        if !spacemouse_camera_action(world, &action, &mut camera) {
            continue;
        }
        layout.set_pane_camera(world, active, camera);
        world.insert_resource(layout);
        applied += 1;
    }
    applied
}

/// [`handle_spacemouse_actions`] for a [`ViewportInstance`] panel: `view.fit_selection` and
/// `view.home` move the instance's active pane, not the world's main viewport.
fn handle_instance_spacemouse_actions(
    world: &mut World,
    cursor: &mut MessageCursor<SpaceMouseAction>,
    apply: bool,
    view: &mut ViewportInstance,
) -> usize {
    let Some(messages) = world.get_resource::<Messages<SpaceMouseAction>>() else {
        return 0;
    };
    let actions: Vec<String> = cursor.read(messages).map(|a| a.action.clone()).collect();
    if !apply {
        return 0;
    }
    let mut applied = 0;
    for action in actions {
        if action == spacemouse_actions::OBJECT_TOOL_TOGGLE {
            let mut toggles = world.get_resource_or_insert_with::<SpaceMouseNav>(SpaceMouseNav::default);
            toggles.object_tool = !toggles.object_tool;
            applied += 1;
            continue;
        }
        let active = view.layout.active;
        let (_, mut camera) = view.pane_camera(active);
        if spacemouse_camera_action(world, &action, &mut camera) {
            view.set_pane_camera(active, camera);
            applied += 1;
        }
    }
    applied
}

/// The camera side of a SpaceMouse button action: `view.fit_selection` frames the selection
/// (everything visible when nothing is selected), `view.home` resets. False for other actions.
fn spacemouse_camera_action(world: &mut World, action: &str, camera: &mut EditorCamera) -> bool {
    match action {
        spacemouse_actions::FIT_SELECTION => {
            let mut entities = world
                .get_resource::<Selection>()
                .map(|s| s.entities().to_vec())
                .unwrap_or_default();
            if entities.is_empty() {
                let mut query = world.query_filtered::<Entity, With<Transform>>();
                entities = query.iter(world).collect();
                entities.retain(|e| !crate::visibility::is_hidden(world, *e));
            }
            focus_camera(world, camera, &entities);
            true
        }
        spacemouse_actions::HOME => {
            home_camera(camera);
            true
        }
        _ => false,
    }
}

/// The viewport panel: one pane or four (perspective + top/front/side), each rendered through
/// the runtime path ([`crate::render::render_pane`]) with its overlays, GPU picking, box
/// select and an interactive gizmo; a toolbar for the quality preset, layout and show flags.
/// Hotkeys act on the active pane: `W`/`E`/`R` gizmo mode, `F` focus (eased), `G` game view,
/// `F8` possess/eject (play), `1`…`4` the perspective/top/front/side view presets (eased),
/// `Ctrl+0…9` store and `Shift+0…9` recall a camera bookmark, `Ctrl+D` duplicate (ADR-7204).
/// A pane becomes active by a click or by keyboard focus (Tab into it; FU-9).
///
/// This is the main viewport: its camera, layout and settings are the world's
/// [`EditorCamera`], [`ViewportLayout`] and [`ViewportSettings`] resources. For more views of
/// one world, each with its own, see [`viewport_panel_with`].
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_panel")]
pub fn viewport_panel(
    ui: &mut egui::Ui,
    world: &mut World,
    device: Option<&regia_render::RenderDevice>,
    state: &mut ViewportPanelState,
) {
    let layout = world
        .get_resource_or_insert_with::<ViewportLayout>(ViewportLayout::default)
        .clone();
    let settings = world
        .get_resource_or_insert_with::<ViewportSettings>(ViewportSettings::default)
        .clone();
    let camera = world
        .get_resource_or_insert_with::<EditorCamera>(EditorCamera::default)
        .clone();
    let mut view = ViewportInstance {
        id: 0,
        camera: camera.clone(),
        layout,
        settings,
        link: None,
    };
    let mut stats = world.remove_resource::<ViewportStats>().unwrap_or_default();
    ViewportInstances::main_drawn(world, ui.ctx().cumulative_frame_nr() + 1);
    panel_body(ui, world, device, state, &mut view, &mut stats, PanelKind::Main);
    world.insert_resource(stats);
    world.insert_resource(view.layout);
    if view.camera != camera {
        world.insert_resource(view.camera);
    }
}

/// The viewport panel over one [`ViewportInstance`] (Tincal engine request F2): the same
/// panes, overlays, picking, gizmos, hotkeys and toolbar as [`viewport_panel`], with the
/// instance's own camera, layout (quality preset, show flags, game view) and settings instead
/// of the world singletons, so several views of one world stay independent. The selection,
/// gizmo mode, bookmarks and undo stack stay shared by the world.
///
/// Hotkeys act in the panel the pointer was last over (the main viewport keeps them until an
/// instance panel is hovered). Keep one [`ViewportPanelState`] per instance. A linked instance
/// ([`ViewportInstance::link`])
/// syncs its camera with its group before and after its input; a pane drawn earlier in the
/// frame follows a later one on the next frame (a repaint is requested). The instance's
/// snapshot, panes and statistics are published to [`ViewportInstances`] for the agent
/// protocol, and agent edits made since the last draw are applied first.
#[tracing::instrument(target = "regia_editor", skip_all, name = "viewport_panel_with")]
pub fn viewport_panel_with(
    ui: &mut egui::Ui,
    world: &mut World,
    device: Option<&regia_render::RenderDevice>,
    state: &mut ViewportPanelState,
    view: &mut ViewportInstance,
) {
    // Registry frame numbers are the egui frame number + 1 (0 = never drawn).
    let frame_nr = ui.ctx().cumulative_frame_nr() + 1;
    let mut stats = ViewportInstances::begin_draw(world, view, frame_nr);
    view.sync_link();
    let panes = panel_body(ui, world, device, state, view, &mut stats, PanelKind::Instance);
    if view.sync_link() == Some(LinkSync::Published) {
        ui.ctx().request_repaint(); // the other members follow next frame
    }
    ViewportInstances::end_draw(world, view, frame_nr, panes, stats);
}

/// The key of pane `index`'s eased camera moves in [`crate::viewport_ops::CameraEases`]: the main
/// viewport's perspective pane moves the world's camera ([`crate::viewport_ops::MAIN_CAMERA`],
/// which the agent's `editor.view_preset` eases too); every other pane has its own key.
fn pane_ease_key(
    panel: PanelKind,
    kind: crate::views::ViewKind,
    instance: u64,
    index: usize,
) -> (u64, usize) {
    if panel == PanelKind::Main && kind == crate::views::ViewKind::Perspective {
        crate::viewport_ops::MAIN_CAMERA
    } else {
        (instance, index)
    }
}

/// Which viewport a panel draws: the main one (world singletons) or an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelKind {
    Main,
    Instance,
}

/// The egui temp-data key naming the panel that has the hotkeys (ctx-wide).
fn key_focus_id() -> egui::Id {
    egui::Id::new("regia_editor::viewport-key-focus")
}

/// A panel's hotkey id (the value stored under [`key_focus_id`]).
fn keys_id(ui: &egui::Ui, panel: PanelKind, instance: u64) -> egui::Id {
    match panel {
        PanelKind::Main => ui.id().with("viewport-keys"),
        PanelKind::Instance => ui.id().with(("viewport-keys", instance)),
    }
}

/// Hotkeys (and the SpaceMouse) go to the panel the pointer was last over; the main viewport
/// keeps them until an instance panel is hovered.
fn panel_has_keys(ui: &egui::Ui, panel: PanelKind, instance: u64) -> bool {
    let focused = ui.ctx().data(|data| data.get_temp::<egui::Id>(key_focus_id()));
    let keys = keys_id(ui, panel, instance);
    match panel {
        PanelKind::Main => focused.is_none_or(|f| f == keys),
        PanelKind::Instance => focused == Some(keys),
    }
}

/// The panel over `view`; returns the panes drawn.
fn panel_body(
    ui: &mut egui::Ui,
    world: &mut World,
    device: Option<&regia_render::RenderDevice>,
    state: &mut ViewportPanelState,
    view: &mut ViewportInstance,
    stats: &mut ViewportStats,
    panel: PanelKind,
) -> Vec<InstancePane> {
    world.get_resource_or_insert_with::<Selection>(Selection::default);
    if view.layout.show.stats {
        let lines = stats.lines();
        let stats_id = ui.id().with("viewport-stats-lines");
        let previous = ui.ctx().data_mut(|data| {
            let previous = data.get_temp::<Vec<String>>(stats_id);
            data.insert_temp(stats_id, lines.clone());
            previous
        });
        if previous.is_some_and(|previous| previous != lines) {
            ui.ctx().request_repaint();
        }
    }
    let counters_before = regia_render::frame_path_counters();

    if state.toolbar {
        let instance = (panel == PanelKind::Instance).then_some(view.id);
        // The active pane's camera for the speed slider and the Camera menu (ADR-7755).
        let active = view.layout.active;
        let (active_kind, mut camera) = view.pane_camera(active);
        let before = camera.clone();
        let ease_key = pane_ease_key(panel, active_kind, view.id, active);
        crate::kit_paint::toolbar(ui, world);
        viewport_toolbar(
            ui,
            world,
            &mut view.layout,
            instance,
            &mut camera,
            active_kind,
            ease_key,
        );
        if camera != before {
            view.set_pane_camera(active, camera);
        }
    }
    // SpaceMouse buttons (specs/29 § 2.5) act in the panel that has the hotkeys; the others
    // consume them without effect.
    let apply = !ui.ctx().egui_wants_keyboard_input() && panel_has_keys(ui, panel, view.id);
    match panel {
        PanelKind::Main => {
            world.insert_resource(view.layout.clone());
            if handle_spacemouse_actions(world, &mut state.spacemouse_actions, apply) > 0 {
                view.layout = world.resource::<ViewportLayout>().clone();
                if let Some(camera) = world.get_resource::<EditorCamera>() {
                    view.camera = camera.clone();
                }
            }
        }
        PanelKind::Instance => {
            handle_instance_spacemouse_actions(world, &mut state.spacemouse_actions, apply, view);
        }
    }

    // The panes fill what is left of the panel, never more: a toolbar row that still overflows
    // (one widget wider than the panel) must not push the panes past the panel's right edge.
    let mut full = ui.available_rect_before_wrap();
    let clip = ui.clip_rect();
    if clip.is_positive() && full.max.x > clip.max.x && clip.max.x > full.min.x {
        full.max.x = clip.max.x;
    }
    // 1, 2, 3 or 4 panes (ADR-7755), each with its own camera and view kind.
    let panes: Vec<(usize, egui::Rect)> = view
        .layout
        .pane_rects(
            Vec2::new(full.min.x, full.min.y),
            Vec2::new(full.width(), full.height()),
        )
        .into_iter()
        .map(|(index, min, size)| {
            let rect = if view.layout.quad {
                egui::Rect::from_min_size(egui::pos2(min.x, min.y), egui::vec2(size.x, size.y))
            } else {
                full
            };
            (index, rect)
        })
        .collect();
    ui.allocate_rect(full, egui::Sense::hover());
    let mut drawn = Vec::with_capacity(panes.len());
    for (index, rect) in panes {
        drawn.push(viewport_pane(
            ui, world, device, state, view, stats, panel, index, rect,
        ));
    }

    let dt = ui.input(|i| i.unstable_dt);
    let created = regia_render::frame_path_counters()
        .since(counters_before)
        .pipelines;
    stats.record(dt, created);
    if view.settings.realtime {
        ui.ctx().request_repaint();
    }
    drawn
}

/// The toolbar's resolution preview: a toggle for the frame and a combo of the play resolution
/// presets (`editor.viewport_resolution_preview`, ADR-7744).
fn resolution_preview_toolbar(ui: &mut egui::Ui, world: &mut World, instance: Option<u64>) {
    use crate::resolution_preview::ResolutionPreview;
    let mut preview = world
        .get_resource::<ResolutionPreview>()
        .copied()
        .unwrap_or_default();
    let before = preview;
    crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_RESOLUTION_PREVIEW, &mut preview.on)
        .on_hover_text(
            "Frame the picture the play window would have at a chosen resolution, with safe-area guides",
        );
    if preview.on {
        let combo = match instance {
            None => egui::ComboBox::from_id_salt("viewport-resolution-preview"),
            Some(id) => egui::ComboBox::from_id_salt(("viewport-resolution-preview", id)),
        };
        combo
            .selected_text(crate::resolution_preview::combo_text(preview.resolution))
            .show_ui(ui, |ui| {
                for preset in crate::play_window::RESOLUTION_PRESETS {
                    ui.selectable_value(&mut preview.resolution, preset.resolution, preset.label);
                }
            });
    }
    if preview != before {
        world.insert_resource(preview);
    }
}

/// The last AI Show flags applied to [`regia_ai::AiDebugDraw`], and the egui frame that applied them.
///
/// A missing stamp means the flags have never been applied, which is the same as all off. The
/// toolbar's first frame therefore does not clobber layers an agent already set.
#[derive(Resource, Clone, Copy)]
struct AiDebugShowSync {
    frame: Option<u64>,
    bits: [bool; 7],
}

/// Copies the seven AI Show flags into [`regia_ai::AiDebugDraw`] and the `ai_debug` gizmo group.
///
/// `frame` is the egui `cumulative_frame_nr` when the toolbar calls this. A second call in that
/// same frame returns immediately. `None` (an agent `editor.set_viewport`) always applies the
/// flags. Layers are written only when they differ from the last applied stamp, or when `frame`
/// is `None`. The gizmo group is enabled when any of the seven is on, and disabled when a change
/// leaves all seven off. An absent [`Selection`] does not clear an agent-set entity; a present
/// selection copies its primary entity (or the first, or none).
pub(crate) fn sync_ai_debug_from_show(world: &mut World, show: &crate::views::ShowFlags, frame: Option<u64>) {
    if let Some(n) = frame
        && world
            .get_resource::<AiDebugShowSync>()
            .is_some_and(|stamp| stamp.frame == Some(n))
    {
        return;
    }
    let bits = [
        show.ai_bt,
        show.ai_state_tree,
        show.ai_goap,
        show.ai_squad,
        show.ai_formation,
        show.ai_move_to,
        show.ai_blackboard,
    ];
    let previous = world
        .get_resource::<AiDebugShowSync>()
        .map(|stamp| stamp.bits)
        .unwrap_or([false; 7]);
    let layers_changed = frame.is_none() || previous != bits;
    let selected = world.get_resource::<Selection>().map(|selection| {
        selection
            .primary()
            .or_else(|| selection.entities().first().copied())
    });
    if layers_changed || selected.is_some() {
        let mut draw = world.get_resource_or_insert_with(regia_ai::AiDebugDraw::default);
        if layers_changed {
            draw.layers = regia_ai::AiDebugLayers::from_flags(bits);
        }
        if let Some(entity) = selected {
            draw.selected = entity;
        }
    }
    if layers_changed
        && let Some(mut store) = world.get_resource_mut::<regia_render::gizmos::GizmoConfigStore>()
    {
        let _ = store.set_enabled(regia_render::gizmos::AiDebugGizmos::NAME, bits.contains(&true));
    }
    world.insert_resource(AiDebugShowSync { frame, bits });
}

/// The active pane's independent control groups (ADR-8199). Their areas allocate no parent
/// space, so placement, collapse and orientation cannot resize the rendered pane.
fn viewport_toolbar(
    ui: &mut egui::Ui,
    world: &mut World,
    layout: &mut ViewportLayout,
    instance: Option<u64>,
    camera: &mut EditorCamera,
    active_kind: crate::views::ViewKind,
    ease_key: (u64, usize),
) {
    let full = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    let pane = layout
        .pane_rects(
            Vec2::new(full.min.x, full.min.y),
            Vec2::new(full.width(), full.height()),
        )
        .into_iter()
        .find(|(index, _, _)| *index == layout.active)
        .map(|(_, min, size)| egui::Rect::from_min_size(egui::pos2(min.x, min.y), egui::vec2(size.x, size.y)))
        .unwrap_or(full);
    // Once per egui frame: a second pass in the same frame must not write the overlay again.
    sync_ai_debug_from_show(world, &layout.show, Some(ui.ctx().cumulative_frame_nr()));
    crate::viewport_overlays::show(ui, world, instance, active_kind, pane, |ui, world, group| {
        use crate::viewport_overlays::Group;
        match group {
            Group::View => {
                // A named toolbar for screen readers (ADR-7746).
                crate::a11y_bridge::region(
                    ui,
                    egui::accesskit::Role::Toolbar,
                    crate::a11y_bridge::VIEWPORT_TOOLBAR,
                );
                let combo = match instance {
                    None => egui::ComboBox::from_id_salt("viewport-quality"),
                    Some(id) => egui::ComboBox::from_id_salt(("viewport-quality", id)),
                };
                let quality = combo
                    .selected_text(format!("Quality: {}", layout.quality.name()))
                    .show_ui(ui, |ui| {
                        for quality in crate::views::ViewportQuality::ALL {
                            ui.selectable_value(&mut layout.quality, quality, quality.name());
                        }
                    });
                // A combo box shows only its value: give it a spoken name (ADR-7746).
                crate::a11y_bridge::name_node(&quality.response, "Viewport quality");
                crate::camera_nav::layout_combo(ui, layout, instance);
                ui.toggle_value(&mut layout.game_view, "Game view (G)");
                resolution_preview_toolbar(ui, world, instance);
                // The active pane's view mode, exposure and screen percentage (ADR-7770).
                pane_display_menus(ui, layout);
                ui.menu_button("Show", |ui| {
            ui.checkbox(&mut layout.show.grid, "Grid");
            ui.checkbox(&mut layout.show.icons, "Icons");
            ui.checkbox(&mut layout.show.bounds, "Bounds and ranges");
            ui.checkbox(&mut layout.show.outline, "Selection outline");
            ui.checkbox(&mut layout.show.proxies, "Proxies for empty entities");
            ui.checkbox(&mut layout.show.audio, "Audio ranges");
            ui.checkbox(
                &mut layout.show.preview_light,
                "Preview sun when the level has none",
            );
            ui.checkbox(&mut layout.show.stats, "Stats");
            // Per-kind icon filters, size and opacity of the active pane (ADR-7770):
            // `editor.set_viewport { icon_filters, icon_size, icon_opacity }`.
            icon_filter_menu(ui, layout);
            // The baked nav mesh (ADR-7468): `editor.set_viewport { navigation }`.
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SHOW_NAVIGATION, &mut layout.show.navigation);
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SHOW_PATHS, &mut layout.show.paths);
            // The det-sim indexes of play (ADR-8562): `editor.set_viewport { lockstep }`.
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SHOW_LOCKSTEP, &mut layout.show.lockstep);
            // Every collider, every light's reach, every post volume (ADR-7217, ADR-7593):
            // `editor.set_viewport { colliders, light_radii, post_volumes }`.
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SHOW_COLLIDERS, &mut layout.show.colliders);
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SHOW_LIGHT_RADII, &mut layout.show.light_radii);
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SHOW_POST_VOLUMES, &mut layout.show.post_volumes);
            // The fog of war debug view (audit gap FW-1): `editor.set_viewport { fog }`.
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SHOW_FOG, &mut layout.show.fog);
            if layout.show.fog {
                ui.indent("fog_debug", |ui| crate::fog_debug::fog_menu(ui, world));
            }
            // In-world AI layers (ADR-8463): `editor.set_viewport { ai_bt, ai_state_tree, ... }`.
            // Game view keeps these; they are not helper overlays.
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SHOW_AI_BT, &mut layout.show.ai_bt);
            crate::agent_parity::action_toggle(
                ui,
                &crate::agent_parity::VIEW_SHOW_AI_STATE_TREE,
                &mut layout.show.ai_state_tree,
            );
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SHOW_AI_GOAP, &mut layout.show.ai_goap);
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SHOW_AI_SQUAD, &mut layout.show.ai_squad);
            crate::agent_parity::action_toggle(
                ui,
                &crate::agent_parity::VIEW_SHOW_AI_FORMATION,
                &mut layout.show.ai_formation,
            );
            crate::agent_parity::action_toggle(
                ui,
                &crate::agent_parity::VIEW_SHOW_AI_MOVE_TO,
                &mut layout.show.ai_move_to,
            );
            crate::agent_parity::action_toggle(
                ui,
                &crate::agent_parity::VIEW_SHOW_AI_BLACKBOARD,
                &mut layout.show.ai_blackboard,
            );
            ui.separator();
            // The editor's own nav mesh from the level's static collision (ADR-7593):
            // `editor.nav_preview { action }`. Building also turns Show ▸ Navigation on.
            // The bake is editor job `nav_preview:<n>` (ADR-7664): its end turns Show ▸ Navigation on.
            if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::VIEW_NAV_PREVIEW_BUILD).clicked() {
                if let Err(err) = crate::nav_preview::start_build(world, true) {
                    tracing::warn!(target: "regia_editor", code = err.code(), "navigation preview: {err:?}");
                }
                ui.close();
            }
            if world.contains_resource::<crate::nav_preview::NavPreviewJob>()
                && crate::agent_parity::action_menu_item(ui, &crate::agent_parity::VIEW_NAV_PREVIEW_CANCEL).clicked()
            {
                crate::nav_preview::cancel(world);
                ui.close();
            }
            if crate::agent_parity::action_menu_item(ui, &crate::agent_parity::VIEW_NAV_PREVIEW_CLEAR).clicked() {
                if let Err(err) = crate::nav_preview::clear(world) {
                    tracing::warn!(target: "regia_editor", code = err.code(), "navigation preview: {err:?}");
                }
                ui.close();
            }
        });
            }
            Group::Tools => {
                ui.menu_button("Tools", |ui| {
            let mut options =
                world.get_resource_or_insert_with(crate::surface_tools::PlacementOptions::default);
            ui.checkbox(&mut options.align_to_normal, "Align to surface")
                .on_hover_text("Drop to floor (End) also turns the selection's up onto the surface normal");
            // Surface snap while dragging (ADR-7756): `editor.placement_options { surface_* }`.
            let mut surface = world
                .get_resource::<crate::surface_tools::SurfaceSnap>()
                .copied()
                .unwrap_or_default();
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SURFACE_SNAP, &mut surface.enabled)
                .on_hover_text("While translating, the selection slides on the surface under the pointer");
            crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEW_SURFACE_ALIGN, &mut surface.align)
                .on_hover_text("Surface snap also turns the selection's up onto the surface normal");
            ui.horizontal(|ui| {
                let label = ui.label("Offset along the normal");
                let response = ui
                    .add(egui::DragValue::new(&mut surface.offset).speed(0.01).suffix(" m"))
                    .labelled_by(label.id);
                crate::a11y_audit::named_drag_value(response, "Surface snap offset", f64::from(surface.offset));
            });
            if world.get_resource::<crate::surface_tools::SurfaceSnap>() != Some(&surface) {
                world.insert_resource(surface);
            }
            let mut tool =
                world.get_resource_or_insert_with(crate::collider_handles::ColliderHandleTool::default);
            ui.checkbox(&mut tool.enabled, "Collider handles (C)").on_hover_text(
                "Grips on the selection's collider (or reflection capture box and margin) replace its gizmo; each drag is one undo step",
            );
            // Extension viewport tools and overlays, then extension actions that joined the
            // viewport's menu, on the level selection (ADR-7791).
            crate::extension::viewport_tools::tool_buttons(ui, world);
            let selection = crate::extension::actions::MenuSelection::level(world);
            if let Some(message) = crate::extension::actions::context_menu(
                ui,
                world,
                crate::extension::actions::ActionPlace::Viewport,
                &selection,
            ) {
                tracing::info!(target: "regia_editor", "{message}");
            }
        });
            }
            Group::Transform => {
                // Gizmo pivot and snap toggle (ADR-7204); Ctrl inverts snapping while held.
                let mut pivot = world
                    .get_resource::<crate::viewport_ops::GizmoPivot>()
                    .copied()
                    .unwrap_or_default();
                let pivot_combo = match instance {
                    None => egui::ComboBox::from_id_salt("viewport-pivot"),
                    Some(id) => egui::ComboBox::from_id_salt(("viewport-pivot", id)),
                };
                let pivot_menu = pivot_combo
                    .selected_text(format!("Pivot: {}", pivot.0.label()))
                    .show_ui(ui, |ui| {
                        for mode in crate::viewport_ops::PivotMode::ALL {
                            ui.selectable_value(&mut pivot.0, mode, mode.label());
                        }
                    });
                crate::a11y_bridge::name_node(&pivot_menu.response, "Gizmo pivot");
                if world.get_resource::<crate::viewport_ops::GizmoPivot>() != Some(&pivot) {
                    world.insert_resource(pivot);
                }
                // The reference space (ADR-7756): Ctrl+` cycles it; `editor.set_gizmo { space }`.
                let gizmo_now = world.get_resource::<GizmoState>().cloned().unwrap_or_default();
                let space_combo = match instance {
                    None => egui::ComboBox::from_id_salt("viewport-space"),
                    Some(id) => egui::ComboBox::from_id_salt(("viewport-space", id)),
                };
                let mut space = gizmo_now.space;
                let space_response = space_combo
                    .selected_text(format!("Space: {}", space.label()))
                    .show_ui(ui, |ui| {
                        for option in GizmoSpace::ALL {
                            ui.selectable_value(&mut space, option, option.label());
                        }
                    })
                    .response;
                // Hover: its name; the axes are world, the entity's own, its parent's or the camera's (Ctrl+`).
                crate::a11y_audit::name(
                    space_response,
                    egui::WidgetType::ComboBox,
                    "Reference space (Ctrl+`)",
                );
                if space != gizmo_now.space
                    && let Some(mut gizmo) = world.get_resource_mut::<GizmoState>()
                {
                    gizmo.space = space;
                }
            }
            Group::Snap => {
                if let Some(mut gizmo) = world.get_resource_mut::<GizmoState>() {
                    let mut snap = gizmo.snap;
                    if ui
                        .toggle_value(&mut snap, "Snap")
                        .on_hover_text(
                            "Snap gizmo drags to the grid, angle and scale steps; hold Ctrl to invert",
                        )
                        .changed()
                    {
                        gizmo.snap = snap;
                    }
                }
                // The snap steps (ADR-7756), saved per project: `editor.set_snap`.
                crate::viewport_ops::ensure_snap_steps(world);
                snap_step_menus(ui, world, instance);
            }
            Group::Camera => {
                // The camera menu (ADR-7768): scene cameras, pilot, the preview size, composition guides.
                if instance.is_none() {
                    crate::viewport_pilot::camera_menu(ui, world);
                }
                if world.contains_resource::<crate::render::PlayWorld>() {
                    ui.toggle_value(&mut layout.possessed, "Possess (F8)");
                }
                // The SpaceMouse Object tool (specs/29 § 2.3b): offered while a device is connected.
                let connected = world
                    .get_resource::<crate::spacemouse::SpaceMouseDevices>()
                    .is_some_and(|d| d.connected() > 0);
                if let Some(mut toggles) = world.get_resource_mut::<SpaceMouseNav>()
                    && (connected || toggles.object_tool)
                {
                    crate::agent_parity::action_toggle(ui, &crate::agent_parity::VIEWPORT_SPACEMOUSE_OBJECT, &mut toggles.object_tool)
                .on_hover_text("SpaceMouse Object tool: the cap moves the selected object instead of the camera; each gesture is one undo step");
                }
                // Speed slider, orbit-selection toggle and the Camera menu (ADR-7755).
                crate::camera_nav::toolbar(ui, world, camera, !active_kind.is_ortho(), ease_key);
            }
            Group::Select => {} // The overlay module owns the shared Box/Lasso operation.
        }
    });
}

/// The grid, angle and scale step menus beside the snap toggle (ADR-7756): presets plus a typed
/// custom value, each a focusable widget; a change goes through `viewport_ops::set_snap_steps`,
/// which saves the steps to the project.
fn snap_step_menus(ui: &mut egui::Ui, world: &mut World, instance: Option<u64>) {
    let gizmo = world.get_resource::<GizmoState>().cloned().unwrap_or_default();
    let rows: [(&str, &str, &[f32], f32, &str); 3] = [
        (
            "viewport-snap-grid",
            "Grid",
            crate::viewport_ops::GRID_STEP_PRESETS,
            gizmo.snap_distance,
            " m",
        ),
        (
            "viewport-snap-angle",
            "Angle",
            crate::viewport_ops::ANGLE_STEP_PRESETS,
            gizmo.snap_angle_deg,
            "°",
        ),
        (
            "viewport-snap-scale",
            "Scale",
            crate::viewport_ops::SCALE_STEP_PRESETS,
            gizmo.snap_scale,
            "",
        ),
    ];
    let mut changed = [None; 3];
    // A custom value being dragged applies each frame and is saved when the drag ends.
    let (mut dragging, mut released) = (false, false);
    for (slot, (salt, name, presets, value, unit)) in rows.into_iter().enumerate() {
        let combo = match instance {
            None => egui::ComboBox::from_id_salt(salt),
            Some(id) => egui::ComboBox::from_id_salt((salt, id)),
        };
        let mut chosen = value;
        let response = combo
            .selected_text(format!("{name}: {}{unit}", step_text(value)))
            .show_ui(ui, |ui| {
                for preset in presets {
                    ui.selectable_value(&mut chosen, *preset, format!("{}{unit}", step_text(*preset)));
                }
                ui.separator();
                ui.horizontal(|ui| {
                    let label = ui.label("Custom");
                    let response = ui
                        .add(
                            egui::DragValue::new(&mut chosen)
                                .speed(0.01)
                                .range(0.001..=10_000.0),
                        )
                        .labelled_by(label.id);
                    dragging |= response.dragged();
                    released |= response.drag_stopped();
                    crate::a11y_audit::named_drag_value(
                        response,
                        &format!("Custom {} snap step", name.to_lowercase()),
                        f64::from(chosen),
                    );
                });
            })
            .response;
        let spoken = format!("{name} snap step");
        crate::a11y_audit::name(response, egui::WidgetType::ComboBox, &spoken);
        if chosen != value && chosen.is_finite() && chosen > 0.0 {
            changed[slot] = Some(chosen);
        }
    }
    if dragging && !released {
        let mut gizmo = world.get_resource_or_insert_with(GizmoState::default);
        let [distance, angle, scale] = changed;
        gizmo.snap_distance = distance.unwrap_or(gizmo.snap_distance);
        gizmo.snap_angle_deg = angle.unwrap_or(gizmo.snap_angle_deg);
        gizmo.snap_scale = scale.unwrap_or(gizmo.snap_scale);
    } else if (changed.iter().any(Option::is_some) || released)
        && let Err(err) = crate::viewport_ops::set_snap_steps(world, changed[0], changed[1], changed[2])
    {
        tracing::warn!(target: "regia_editor", code = err.code(), "snap steps: {err}");
    }
}

/// A snap step as the menus show it: up to four decimals, no trailing zeros.
fn step_text(value: f32) -> String {
    let text = format!("{value:.4}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The egui frame whose Shift+H toggled the isolate (a second UI pass of that frame skips it).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
struct IsolateKeyFrame(u64);

/// The View mode, Exposure and Scale menus of the active pane (ADR-7770); every choice is also
/// `editor.set_viewport { pane, view_mode, exposure, exposure_ev, screen_percentage }`.
fn pane_display_menus(ui: &mut egui::Ui, layout: &mut ViewportLayout) {
    let pane = layout.active.min(3);
    let display = layout.display_mut(pane);
    ui.menu_button(format!("View: {}", display.view_mode.label()), |ui| {
        for (i, mode) in crate::views::ViewMode::KEYED.into_iter().enumerate() {
            ui.selectable_value(
                &mut display.view_mode,
                mode,
                format!("{}  (Alt+{})", mode.label(), i + 1),
            );
        }
        ui.menu_button("Buffer views", |ui| {
            for (name, label) in crate::views::BUFFER_VIEWS {
                if let Some(mode) = crate::views::ViewMode::from_name(name) {
                    ui.selectable_value(&mut display.view_mode, mode, label);
                }
            }
        });
    })
    .response
    .on_hover_text("What the active pane shows (Alt+1 to Alt+8)");
    let exposure_text = match display.exposure.fixed_ev() {
        Some(ev) => format!("Exposure: EV {ev:.1}"),
        None => format!("Exposure: {}", exposure_label(display.exposure)),
    };
    ui.menu_button(exposure_text, |ui| {
        ui.selectable_value(
            &mut display.exposure,
            crate::views::ViewportExposure::Auto,
            "Auto",
        );
        let fixed = display
            .exposure
            .fixed_ev()
            .unwrap_or(crate::render::VIEWPORT_EXPOSURE_EV);
        ui.selectable_value(
            &mut display.exposure,
            crate::views::ViewportExposure::Fixed(fixed),
            "Fixed EV",
        );
        ui.selectable_value(
            &mut display.exposure,
            crate::views::ViewportExposure::LevelBand,
            "Level band",
        );
        if let Some(mut ev) = display.exposure.fixed_ev()
            && ui
                .add(egui::Slider::new(&mut ev, crate::views::ViewportExposure::EV_RANGE).text("EV100"))
                .changed()
        {
            display.exposure = crate::views::ViewportExposure::Fixed(ev);
        }
    })
    .response
    .on_hover_text("How the active pane sets its exposure");
    ui.menu_button(format!("Scale: {} %", display.screen_percentage()), |ui| {
        let mut percent = display.screen_percentage();
        if ui
            .add(
                egui::Slider::new(&mut percent, crate::views::PaneDisplay::SCREEN_PERCENTAGE_RANGE)
                    .text("Screen percentage")
                    .suffix(" %"),
            )
            .changed()
        {
            display.screen_percentage = percent;
        }
    })
    .response
    .on_hover_text("The active pane's internal render scale; the upscaler brings it to the pane size");
}

/// The menu label of an exposure mode.
fn exposure_label(exposure: crate::views::ViewportExposure) -> &'static str {
    match exposure {
        crate::views::ViewportExposure::Auto => "Auto",
        crate::views::ViewportExposure::Fixed(_) => "Fixed EV",
        crate::views::ViewportExposure::LevelBand => "Level band",
    }
}

/// Show ▸ Icons of the active pane: a switch per kind, icon size and opacity (ADR-7770).
fn icon_filter_menu(ui: &mut egui::Ui, layout: &mut ViewportLayout) {
    let pane = layout.active.min(3);
    let icons = &mut layout.display_mut(pane).icons;
    ui.menu_button("Icons by kind", |ui| {
        for (name, label) in crate::views::IconFilters::KINDS {
            if let Some(on) = icons.kind_mut(name) {
                ui.checkbox(on, label);
            }
        }
        ui.separator();
        ui.add(
            egui::Slider::new(&mut icons.size_percent, crate::views::IconFilters::SIZE_RANGE)
                .text("Icon size")
                .suffix(" %"),
        );
        ui.add(
            egui::Slider::new(
                &mut icons.opacity_percent,
                crate::views::IconFilters::OPACITY_RANGE,
            )
            .text("Icon opacity")
            .suffix(" %"),
        );
    });
}

/// The largest side, in pixels, a pane renders at (egui's texture limit is 2048).
const PANE_MAX_SIDE: f32 = 2040.0;

/// The pixel size a viewport pane of `points` (egui points) renders at, on a window of
/// `pixels_per_point`: the pixels the pane covers on screen (a Retina or HiDPI window has more
/// than one per point; X04), rounded to whole pixels, then sized by [`pane_buffer_size`] (neither
/// side past `PANE_MAX_SIDE`, at least 16 pixels a side, the pane's aspect kept). A scale that is
/// not a positive finite number counts as one pixel per point.
fn pane_render_size(points: egui::Vec2, pixels_per_point: f32) -> Vec2 {
    let scale = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    let size = points * scale;
    let [width, height] = pane_buffer_size(Vec2::new(size.x.round(), size.y.round()));
    Vec2::new(width as f32, height as f32)
}

/// Pixel size of the picture requested for a pane of `pane` pixels.
///
/// Clamps the long side to `PANE_MAX_SIDE`, then grows both axes by the same factor so the short
/// side can reach 16 without changing the pane's aspect. Each axis is then floored to an integer
/// of at least 16 and capped at `PANE_MAX_SIDE` again: a pane thinner than 16 : `PANE_MAX_SIDE`
/// (a squeezed panel, a zero-height pane under a wrapped toolbar) would otherwise grow its long
/// side past egui's texture limit (a 268 × 0 pane asked for 4288 × 16; ADR-8366), so such a pane
/// alone loses its aspect. The picture is still drawn stretched across the whole pane; any other
/// buffer has the pane's aspect, so the projection matches what the user sees.
#[must_use]
pub(crate) fn pane_buffer_size(pane: Vec2) -> [u32; 2] {
    const FLOOR: f32 = 16.0;
    let pane_x = pane.x.max(1.0);
    let pane_y = pane.y.max(1.0);
    let shrink = (pane_x / PANE_MAX_SIDE).max(pane_y / PANE_MAX_SIDE).max(1.0);
    let fit_x = pane_x / shrink;
    let fit_y = pane_y / shrink;
    let grow = (FLOOR / fit_x).max(FLOOR / fit_y).max(1.0);
    let width = (fit_x * grow).floor().clamp(FLOOR, PANE_MAX_SIDE) as u32;
    let height = (fit_y * grow).floor().clamp(FLOOR, PANE_MAX_SIDE) as u32;
    [width, height]
}

/// Maps a pointer in pane points into the picture's pixels.
///
/// X uses the integer buffer width over the pane width, and Y the integer buffer height over
/// the pane height. The result is clamped to the last pixel so an edge click is inside the
/// id buffer.
#[must_use]
pub(crate) fn pane_pointer_px(local: Vec2, pane: Vec2, buffer: [u32; 2]) -> Vec2 {
    let pane_x = pane.x.max(1.0);
    let pane_y = pane.y.max(1.0);
    let x = (local.x * buffer[0] as f32 / pane_x).clamp(0.0, buffer[0].saturating_sub(1) as f32);
    let y = (local.y * buffer[1] as f32 / pane_y).clamp(0.0, buffer[1].saturating_sub(1) as f32);
    Vec2::new(x, y)
}

/// Aspect ratio the transform gizmo's projection uses: the integer picture, which matches the
/// buffer the pane draws.
#[must_use]
pub(crate) fn pane_gizmo_aspect(buffer: [u32; 2]) -> f32 {
    buffer[0] as f32 / buffer[1].max(1) as f32
}

/// Runs and draws pane `rect`'s transform gizmo over `targets`: what transform-gizmo-egui's
/// `GizmoExt::interact` does, without its cursor widget (ADR-7674, amending ADR-8870 and
/// ADR-7593 (3)).
///
/// `GizmoExt::interact` registers a 1x1 click-and-drag widget under the pointer every frame
/// (id `ui.id().with("_interaction")`, the same in every pane) only to learn whether anything
/// covers the gizmo. egui gives that topmost widget the hover and every drag, so a pane tool that
/// reads the pane's response after the press (the spline grips) lost the pointer position for the
/// whole drag whenever the gizmo was shown. Here no widget is registered: the pane's own response answers the gizmo's question, so each
/// press has one owner, decided at press time. The gizmo takes it when a handle was under the
/// pointer (its focus; the pane reads it as `gizmo_active` and keeps its box select, picking,
/// Alt-orbit and spline grips off); otherwise the pane and its tools do.
///
/// The gizmo sees the pointer only while the pane is hovered and no button is held, or on the
/// press frame itself, so a drag that began elsewhere never highlights or grabs a handle it
/// passes over. The drag then runs on the raw primary button, as the crate's does.
///
/// The update-and-paint sequence follows transform-gizmo-egui 0.11.0 `GizmoExt::interact`
/// (MIT OR Apache-2.0, THIRD_PARTY_NOTICES.md; docs/provenance.md row of ADR-7674).
fn gizmo_update(
    ui: &egui::Ui,
    gizmo: &mut transform_gizmo_egui::Gizmo,
    response: &egui::Response,
    targets: &[transform_gizmo_egui::math::Transform],
) -> Option<(
    transform_gizmo_egui::GizmoResult,
    Vec<transform_gizmo_egui::math::Transform>,
)> {
    let (cursor, pressed, down, any_down, any_pressed) = ui.input(|i| {
        (
            i.pointer.hover_pos().unwrap_or_default(),
            i.pointer.button_pressed(egui::PointerButton::Primary),
            i.pointer.button_down(egui::PointerButton::Primary),
            i.pointer.any_down(),
            i.pointer.any_pressed(),
        )
    });
    let result = gizmo.update(
        transform_gizmo_egui::GizmoInteraction {
            cursor_pos: (cursor.x, cursor.y),
            hovered: response.hovered() && (!any_down || any_pressed),
            drag_started: pressed,
            dragging: down,
        },
        targets,
    );
    let viewport = gizmo.config().viewport;
    let clip = if viewport.is_finite() {
        viewport
    } else {
        ui.clip_rect()
    };
    let draw = gizmo.draw();
    let vertices = draw
        .vertices
        .into_iter()
        .zip(draw.colors)
        .map(|(pos, [r, g, b, a])| egui::epaint::Vertex {
            pos: pos.into(),
            uv: egui::Pos2::ZERO,
            color: egui::Rgba::from_rgba_premultiplied(r, g, b, a).into(),
        })
        .collect();
    egui::Painter::new(ui.ctx().clone(), ui.layer_id(), clip).add(egui::Mesh {
        indices: draw.indices,
        vertices,
        ..Default::default()
    });
    result
}

/// One pane of `instance`: render, overlays, input. Returns what it drew.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // one pane's draw + input in order
fn viewport_pane(
    ui: &mut egui::Ui,
    world: &mut World,
    device: Option<&regia_render::RenderDevice>,
    state: &mut ViewportPanelState,
    instance: &mut ViewportInstance,
    stats: &mut ViewportStats,
    panel: PanelKind,
    index: usize,
    rect: egui::Rect,
) -> InstancePane {
    let id = match panel {
        PanelKind::Main => ui.id().with(("viewport-pane", index)),
        PanelKind::Instance => ui.id().with(("viewport-pane", instance.id, index)),
    };
    let response = ui.interact(rect, id, egui::Sense::click_and_drag());
    // FU-9 (`docs/evidence/editor-a11y-2026-11.md`): the pane is a focusable widget named for
    // screen readers. Keyboard focus on it makes it the active pane below, so Tab reaches the
    // viewport and its hotkeys without a click. No hover text: a pane-sized tooltip would
    // follow the pointer across the whole viewport.
    let pane_name = format!("Viewport pane {}", index + 1);
    let response = crate::a11y_audit::spoken_only(response, egui::WidgetType::Panel, &pane_name);
    let layout = instance.layout.clone();
    let settings = instance.settings.clone();
    let (kind, mut camera) = instance.pane_camera(index);
    let selection = world.resource::<Selection>().clone();
    // The pane's own icon filters (ADR-7770).
    let show = layout.pane_show(index);
    let grid_on = show.grid && settings.grid;
    let size = rect.size();
    // Hotkeys go to the panel the pointer was last over (ctx-wide, so several instance panels
    // never all react); the main viewport keeps them until an instance panel is hovered.
    // Keyboard focus on the pane activates it the same way (FU-9).
    if response.hovered() || response.clicked() || response.has_focus() {
        instance.layout.active = index;
        let keys = keys_id(ui, panel, instance.id);
        ui.ctx().data_mut(|data| data.insert_temp(key_focus_id(), keys));
    }

    // Render at the pane's physical pixels (clamped to egui's texture limit, aspect kept);
    // pointers map from points into that buffer per axis.
    let render_size = pane_render_size(size, ui.ctx().pixels_per_point());
    let pane = Vec2::new(size.x, size.y);
    let buffer = [render_size.x as u32, render_size.y as u32];
    // The id buffer also depth-tests the grid. Only a real pointer click over the pane picks
    // (ADR-8870): `response.clicked()` is also true for Enter or Space on the focused pane
    // (egui's fake click). The gizmo's own handles keep the click from picking through
    // `gizmo_active` below (ADR-7674).
    let pointer_clicked = ui.input(|i| i.pointer.primary_clicked()) && response.contains_pointer();
    let wants_ids = pointer_clicked
        || response.drag_stopped()
        || grid_on
        || (show.outline && !selection.entities().is_empty())
        || (show.outline && crate::collab::needs_ids(world));
    // The hover pre-highlight reads this same buffer and never asks for one (ADR-7769).
    if wants_ids {
        PANE_ID_REQUESTS.with(|count| count.set(count.get() + 1));
    }
    // `ViewportReferences` belongs to the main viewport's panes; instances have none (F2).
    let reference = match panel {
        PanelKind::Main => world
            .get_resource::<crate::reference::ViewportReferences>()
            .and_then(|r| r.panes.get(&index).cloned()),
        PanelKind::Instance => None,
    };
    // The GPU path (ADR-1555) keeps the picture in a texture egui samples; a reference image is
    // composited on the CPU, so such a pane reads back.
    let textures = world
        .get_resource::<crate::pane_texture::PaneTextures>()
        .filter(|t| t.gpu_enabled() && reference.is_none())
        .cloned();
    let drawn = device.and_then(|device| {
        let request = crate::render::PaneRequest {
            pane: index,
            kind,
            camera: camera.clone(),
            width: render_size.x as u32,
            height: render_size.y as u32,
            ids: wants_ids,
            warm_frames: 0,
            // The pane's View mode (ADR-7770): the same field `editor.capture_viewport` sets.
            debug_view: layout.display(index).view_mode.debug_view(),
            reference,
        };
        match (panel, textures.is_some()) {
            (PanelKind::Main, false) => {
                crate::render::render_pane(device, world, &request).map(PaneDrawn::Cpu)
            }
            (PanelKind::Main, true) => {
                crate::render::render_pane_gpu(device, world, &request).map(PaneDrawn::Gpu)
            }
            (PanelKind::Instance, false) => {
                crate::render::render_instance_pane(device, world, instance, &request).map(PaneDrawn::Cpu)
            }
            (PanelKind::Instance, true) => {
                crate::render::render_instance_pane_gpu(device, world, instance, &request).map(PaneDrawn::Gpu)
            }
        }
    });
    let view = drawn
        .as_ref()
        .map_or_else(|| crate::views::ViewCamera::of(kind, &camera), PaneDrawn::view);
    // Content drops run after this UI, so keep the pointer ray while the cursor is over the pane.
    if response.hovered()
        && let Some(pointer) = response.hover_pos()
    {
        let delta = pointer - rect.left_top();
        let local = pane_pointer_px(Vec2::new(delta.x, delta.y), pane, buffer);
        let (origin, direction) = view.ray(local, render_size);
        let target_depth = (view.target - origin).dot(direction);
        world.insert_resource(crate::content::ViewportDropAim {
            origin,
            direction,
            target_depth,
        });
        // The ghost of an asset dragged from the Content Browser (P-067, ADR-7769).
        crate::content::update_drop_preview(ui.ctx(), world);
    }
    let drawn_size = drawn.as_ref().map_or([0, 0], PaneDrawn::size);
    let drawn_pane = InstancePane {
        index,
        kind,
        view,
        size: drawn_size,
    };
    let possessed = drawn.as_ref().is_some_and(|d| d.info().possessed);
    // Hover pre-highlight (P-071, ADR-7769): the entity under the pointer, from the id buffer the
    // pane already read, while no drag, button or content drag is active.
    let hover = (!possessed
        && show.outline
        && response.hovered()
        && state.drag.is_none()
        && !ui.input(|i| i.pointer.any_down())
        && !egui::DragAndDrop::has_any_payload(ui.ctx())
        && !crate::measure_tool::MeasureTool::is_active(world))
    .then(|| response.hover_pos())
    .flatten()
    .and_then(|pointer| {
        let ids = drawn.as_ref().and_then(PaneDrawn::ids)?;
        let delta = pointer - rect.left_top();
        let local = pane_pointer_px(Vec2::new(delta.x, delta.y), pane, buffer);
        crate::overlays::hover_target(world, ids, local, ui.input(|i| i.modifiers.ctrl))
    });
    // A click on the possessed play view captures the mouse (Shift+F1 releases it; specs/19 § 4).
    // A pointer click only: Enter or Space on the focused pane is not a click (ADR-8870).
    if possessed && pointer_clicked {
        world.insert_resource(crate::play_input::CaptureClick);
    }
    let mut drawn = drawn;
    let view_label = if possessed {
        "game camera (F8 ejects)".to_string()
    } else {
        format!("{} · {}", kind.name(), layout.quality.name())
    };
    let overlay_blur = settings.overlay_blur;
    if let Some(drawn) = drawn.as_mut() {
        stats.last = Some(drawn.info().clone());
        stats.record_pane(ui.input(|input| input.time), drawn.path());
        // The chips go behind the text below; the picture under them is blurred before egui reads it.
        if overlay_blur > 0.0 {
            let mut chips = vec![view_label_chip(rect, &view_label_galley(ui, view_label.clone()))];
            if layout.show.stats {
                chips.push(stats_chip(ui, rect, stats).1);
            }
            match &mut *drawn {
                PaneDrawn::Cpu(frame) => {
                    let blurred = crate::viewport_blur::blur_image_behind_chips(
                        &mut frame.image,
                        rect,
                        &chips,
                        overlay_blur,
                    );
                    world
                        .get_resource_or_insert_with(crate::viewport_blur::ViewportBlurState::default)
                        .chips = blurred;
                }
                PaneDrawn::Gpu(frame) => {
                    if let Some(device) = device {
                        crate::viewport_blur::blur_behind_chips(
                            world,
                            device,
                            &frame.target,
                            rect,
                            &chips,
                            overlay_blur,
                        );
                    }
                }
            }
        }
        let name = match panel {
            PanelKind::Main => format!("viewport-render-{index}"),
            PanelKind::Instance => format!("viewport-render-{}-{index}", instance.id),
        };
        let full_uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0));
        match &mut *drawn {
            PaneDrawn::Cpu(frame) => {
                if grid_on && !possessed {
                    crate::overlays::paint_grid_with(frame, kind, &crate::theme::tokens(world).overlay);
                }
                if show.outline && !possessed {
                    crate::overlays::paint_outline(frame, world);
                }
                if let Some(hover) = hover {
                    crate::overlays::paint_hover_outline(frame, world, hover);
                }
                let texture = ui
                    .ctx()
                    .load_texture(name, frame.image.clone(), egui::TextureOptions::LINEAR);
                ui.painter()
                    .image(texture.id(), rect, full_uv, egui::Color32::WHITE);
            }
            PaneDrawn::Gpu(frame) => {
                let key = crate::pane_texture::PaneKey {
                    instance: match panel {
                        PanelKind::Main => None,
                        PanelKind::Instance => Some(instance.id),
                    },
                    pane: index,
                };
                match textures.as_ref().and_then(|t| t.submit(key, &frame.target)) {
                    Some(id) => {
                        ui.painter().image(id, rect, full_uv, egui::Color32::WHITE);
                    }
                    // Registered at the end of this frame: the next one paints the picture.
                    None => {
                        ui.painter()
                            .rect_filled(rect, 2.0, egui::Color32::from_rgb(24, 24, 28));
                    }
                }
                // Grid and outline are opaque pixel stamps over the picture: stamp them into a
                // transparent image (they need the id buffer on the CPU) and lay it on top.
                let stamp_grid = grid_on && !possessed;
                let stamp_outline = show.outline && !possessed;
                if stamp_grid || stamp_outline || hover.is_some() {
                    let [width, height] = frame.size();
                    let mut overlay = crate::render::PaneFrame {
                        image: egui::ColorImage::filled(
                            [width as usize, height as usize],
                            egui::Color32::TRANSPARENT,
                        ),
                        view: frame.view,
                        ids: frame.ids.take(),
                        info: crate::render::PaneInfo::default(),
                    };
                    if stamp_grid {
                        crate::overlays::paint_grid_with(
                            &mut overlay,
                            kind,
                            &crate::theme::tokens(world).overlay,
                        );
                    }
                    if stamp_outline {
                        crate::overlays::paint_outline(&mut overlay, world);
                    }
                    if let Some(hover) = hover {
                        crate::overlays::paint_hover_outline(&mut overlay, world, hover);
                    }
                    frame.ids = overlay.ids.take();
                    let texture = ui.ctx().load_texture(
                        format!("{name}-overlay"),
                        overlay.image,
                        egui::TextureOptions::LINEAR,
                    );
                    ui.painter()
                        .image(texture.id(), rect, full_uv, egui::Color32::WHITE);
                }
            }
        }
    } else {
        ui.painter()
            .rect_filled(rect, 2.0, egui::Color32::from_rgb(24, 24, 28));
    }
    // A fullscreen video section of the sequence preview, over the perspective pane (ADR-8591).
    if kind == crate::views::ViewKind::Perspective {
        crate::seq_tracks::paint_preview_overlay(ui, rect, world);
    }
    let frame = drawn;
    let drawn = drawn_pane;

    // Overlays (in pane points: the shapes are built at the pane's logical size).
    let size_vec = Vec2::new(size.x, size.y);
    let mut review_pin_active = false;
    if !possessed {
        let mut flags = show;
        flags.grid = frame.is_none() && grid_on; // stamped into the picture when there is one
        let mut shapes =
            crate::overlays::build_overlays_for(world, &view, kind, size_vec, flags, settings.audio);
        // Show ▸ Post volumes (ADR-7593).
        if flags.post_volumes {
            shapes.extend(crate::volume_overlay::post_volume_shapes(world, &view, size_vec));
        }
        if settings.labels && !layout.game_view {
            for entity in selection.entities() {
                if let Some(position) = world_position(world, *entity)
                    && let Some(p) = view.project(position, size_vec)
                {
                    shapes.push(crate::overlays::OverlayShape::Text {
                        at: p + Vec2::new(6.0, -6.0),
                        text: crate::outliner::display_name(world, *entity),
                        color: egui::Color32::from_rgb(255, 220, 120),
                    });
                }
            }
        }
        crate::overlays::paint_shapes_egui(ui.painter(), rect, &shapes);
        // The resolution preview frame and safe-area guides (ADR-7744), in the perspective pane.
        if kind == crate::views::ViewKind::Perspective
            && let Some(preview) = world.get_resource::<crate::resolution_preview::ResolutionPreview>()
        {
            crate::resolution_preview::paint(ui.painter(), rect, preview);
        }
    }
    if possessed {
        let shapes = crate::overlays::play_debug_shapes(world, &view, size_vec, &layout);
        crate::overlays::paint_shapes_egui(ui.painter(), rect, &shapes);
    } else {
        // Explicit gameplay inspection remains available through the possessed game camera.
        let mut shapes = crate::debug_overlay_selected::overlays(world, &view, size_vec);
        shapes.extend(crate::visual_log_panel::overlays(world, &view, size_vec));
        crate::overlays::paint_shapes_egui(ui.painter(), rect, &shapes);
        if !layout.game_view {
            review_pin_active = crate::review_notes::pins(ui, world, &view, rect, (instance.id, index));
        }
    }
    if overlay_blur > 0.0 {
        let chip = view_label_chip(rect, &view_label_galley(ui, view_label.clone()));
        ui.painter().rect_filled(
            chip,
            3.0,
            egui::Color32::from_black_alpha(crate::viewport_blur::FROSTED_CHIP_ALPHA),
        );
    }
    ui.painter().text(
        rect.right_top() + egui::vec2(-6.0, 4.0),
        egui::Align2::RIGHT_TOP,
        view_label,
        egui::FontId::proportional(11.0),
        egui::Color32::from_gray(200),
    );
    // Pane badges (ADR-7770): Realtime off, and the settings that differ from their defaults.
    let warn = crate::theme::tokens(world).warn;
    let actions = crate::viewport_badges::draw_badges(
        ui,
        rect,
        &instance.layout,
        instance.settings.realtime,
        index,
        warn,
    );
    if crate::viewport_badges::apply_actions(&actions, &mut instance.layout, &mut instance.settings, index)
        && panel == PanelKind::Main
    {
        let realtime = instance.settings.realtime;
        world
            .get_resource_or_insert_with(ViewportSettings::default)
            .realtime = realtime;
    }
    // Split-screen play (specs/19 § 4): one outlined, labeled viewport per local player.
    if possessed {
        for (player, pane) in crate::pie::split_screen_panes(world, rect) {
            ui.painter().rect_stroke(
                pane,
                0.0,
                egui::Stroke::new(1.0, egui::Color32::from_white_alpha(140)),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                pane.left_top() + egui::vec2(6.0, 4.0),
                egui::Align2::LEFT_TOP,
                format!("P{}", u16::from(player) + 1),
                egui::FontId::proportional(11.0),
                egui::Color32::from_gray(220),
            );
        }
    }
    if layout.show.stats {
        let (galley, bg) = stats_chip(ui, rect, stats);
        let alpha = if overlay_blur > 0.0 {
            crate::viewport_blur::FROSTED_CHIP_ALPHA
        } else {
            160
        };
        ui.painter()
            .rect_filled(bg, 3.0, egui::Color32::from_black_alpha(alpha));
        ui.painter()
            .galley(bg.left_top() + egui::vec2(4.0, 3.0), galley, egui::Color32::WHITE);
    }

    let kit_painting = kind == crate::views::ViewKind::Perspective
        && !possessed
        && crate::kit_paint::viewport_input(ui, world, &response, &camera, size_vec);
    if kit_painting {
        crate::kit_paint::paint_overlay(ui, world, &camera, response.rect);
    }
    // Terrain tool (perspective panes): brush outline + status, then its input. While it is
    // on, the left button sculpts instead of box-selecting or picking.
    if kind == crate::views::ViewKind::Perspective && !possessed && !kit_painting {
        crate::terrain_tool::paint_overlay(ui, world, &camera, response.rect);
        crate::terrain_tool::viewport_input(ui, world, &response, &camera, size_vec);
        // The cloth panel's paint mode (spec 55 § 6): left-drag paints the sim mesh.
        crate::cloth_panel::viewport_input(ui, world, &response, &camera, size_vec);
    }
    // The Fracture panel's click modes (ADR-9750): a click picks a support chunk or hits the test.
    let fracture_clicking = kind == crate::views::ViewKind::Perspective
        && !kit_painting
        && !possessed
        && crate::fracture_tools::viewport_input(ui, world, &response, &camera, size_vec);
    // 2D mode (front panes, ADR-1889): bounds, grid and hover cell, then tile strokes. While it
    // is on, the left button paints instead of box-selecting or picking.
    let mode2d_pane = kind == crate::views::ViewKind::Front && !possessed && crate::mode2d::is_active(world);
    if mode2d_pane {
        crate::mode2d::paint_overlay(ui, world, &view, response.rect);
        crate::mode2d::viewport_input(ui, world, &response, &view, size_vec);
    }
    let sculpting = kit_painting
        || world
            .get_resource::<crate::terrain_tool::TerrainTool>()
            .is_some_and(|tool| tool.active);
    // Cloth paint mode (ADR-2884) owns the pointer while it paints a loaded document.
    let cloth_painting = world
        .get_resource::<crate::cloth_panel::ClothPanel>()
        .is_some_and(|panel| panel.painting && panel.doc().is_some());
    // Collider handles (ADR-2696): grips under the pointer take it from box select and picking. They
    // show where the transform gizmo would.
    // Procedural Path and Area (spec 67 § 5): click, drag, Backspace, C, Enter, Esc. While armed
    // the primary button draws instead of box-selecting. Spline handle drags take the pointer only
    // when that tool does not.
    let proc_size = egui::vec2(size_vec.x, size_vec.y);
    // One owner per press, decided at press time (ADR-7674): the gizmo owns a press that lands on
    // one of its handles (its focus, from last frame's hover, when this pane drew it), so the
    // spline grips, box select, picking and Alt-orbit leave it alone; any other press is the
    // pane's and its tools'.
    let gizmo_active = state.gizmo_shown[index] && state.gizmos[index].is_focused();
    let proc_tool = kind == crate::views::ViewKind::Perspective
        && !kit_painting
        && !possessed
        && crate::procgen::tools::viewport_input(ui, world, &response, &camera, proc_size);
    let spline_grip = !possessed
        && !instance.layout.game_view
        && !mode2d_pane
        && !sculpting
        && !proc_tool
        && !gizmo_active
        && crate::procgen::view::viewport_input(ui, world, &response, &camera, proc_size);
    let collider_grip = !possessed
        && !instance.layout.game_view
        && !mode2d_pane
        && !sculpting
        && !cloth_painting
        && !proc_tool
        && !spline_grip
        && crate::collider_handles::viewport_input(ui, world, &view, rect, index);
    // Reflection capture box and margin grips (ADR-7663), under the same tool toggle.
    let capture_grip = !possessed
        && !instance.layout.game_view
        && !mode2d_pane
        && !sculpting
        && !cloth_painting
        && !proc_tool
        && !spline_grip
        && !collider_grip
        && crate::capture_helper::viewport_input(ui, world, &view, rect, index);
    // Smart-object slot and entrance handles of the open definition (ADR-7736), which replace the
    // gizmo on that object as the collider grips do.
    let slot_grip = !possessed
        && !instance.layout.game_view
        && !mode2d_pane
        && !sculpting
        && !cloth_painting
        && !proc_tool
        && !spline_grip
        && !collider_grip
        && !capture_grip
        && crate::smart_object_handles::viewport_input(ui, world, &view, rect, index);
    // The active extension viewport tool's handles (ADR-7791), after every built-in owner.
    let extension_grip = !possessed
        && !instance.layout.game_view
        && !mode2d_pane
        && !sculpting
        && !cloth_painting
        && !proc_tool
        && !spline_grip
        && !collider_grip
        && !capture_grip
        && !slot_grip
        && !gizmo_active
        && crate::extension::viewport_tools::viewport_input(ui, world, &view, rect, index);
    if !instance.layout.game_view {
        crate::extension::viewport_tools::draw_overlays(ui, world, rect, index);
    }
    // The measure tool (P-069, ADR-7769) owns the primary button while it is on.
    let measuring = !possessed && !gizmo_active && {
        let to_pixel = |p: egui::Pos2| {
            let d = p - rect.left_top();
            pane_pointer_px(Vec2::new(d.x, d.y), pane, buffer)
        };
        let snap = crate::command_palette::hold_chord(world, "gizmo.vertex_snap")
            .is_some_and(|chord| ui.input(|i| crate::command_palette::chord_held(i, chord)));
        crate::measure_tool::viewport_input(
            ui,
            world,
            &response,
            &crate::measure_tool::MeasurePane {
                view: &view,
                pane: index,
                to_pixel: &to_pixel,
                buffer: render_size,
                snap,
            },
        )
    };
    let tool_active = measuring
        || review_pin_active
        || mode2d_pane
        || collider_grip
        || capture_grip
        || slot_grip
        || extension_grip
        || sculpting
        || cloth_painting
        || proc_tool
        || spline_grip
        || fracture_clicking;

    // Box select: LMB drag (no Alt) in the pane, released → the ids inside the rectangle. A lasso
    // (P-064, ADR-7769) when the selection shape is Lasso or the lasso key (L) is held at the
    // drag start: the ids inside the freehand outline. Shift adds, Ctrl+Shift subtracts, Ctrl
    // toggles (`RegionMode`).
    let alt = ui.input(|i| i.modifiers.alt);
    if response.drag_started_by(egui::PointerButton::Primary)
        && !alt
        && !gizmo_active
        && !tool_active
        && let Some(pos) = response.interact_pointer_pos()
    {
        let start = ui.input(|i| i.pointer.press_origin()).unwrap_or(pos);
        state.drag = Some((index, start));
        let lasso_key = crate::command_palette::hold_chord(world, "selection.lasso")
            .is_some_and(|chord| ui.input(|i| crate::command_palette::chord_held(i, chord)));
        state.lasso.clear();
        if lasso_key || crate::selection::SelectionShape::lasso_on(world) {
            state.lasso.push(start);
        }
    }
    if let Some((drag_pane, start)) = state.drag
        && drag_pane == index
    {
        let stroke = egui::Stroke::new(1.0, crate::theme::tokens(world).overlay.selection_outline);
        if let Some(now) = ui.input(|i| i.pointer.hover_pos()) {
            if state.lasso.is_empty() {
                let band = egui::Rect::from_two_pos(start, now);
                ui.painter()
                    .rect_stroke(band, 0.0, stroke, egui::StrokeKind::Inside);
            } else {
                if state
                    .lasso
                    .last()
                    .is_none_or(|last| (*last - now).length() >= 2.0)
                {
                    state.lasso.push(now);
                }
                ui.painter()
                    .add(egui::Shape::closed_line(state.lasso.clone(), stroke));
            }
        }
        if response.drag_stopped() {
            state.drag = None;
            let outline = std::mem::take(&mut state.lasso);
            let end = response
                .interact_pointer_pos()
                .or_else(|| ui.input(|i| i.pointer.interact_pos()))
                .unwrap_or(start);
            let to_px = |p: egui::Pos2| {
                let d = p - rect.left_top();
                pane_pointer_px(Vec2::new(d.x, d.y), pane, buffer)
            };
            let ids = frame.as_ref().and_then(PaneDrawn::ids);
            let chosen = if outline.len() >= 3 {
                let polygon: Vec<[f32; 2]> = outline.iter().map(|p| to_px(*p).to_array()).collect();
                Some(
                    ids.map(|ids| crate::selection::entities_in_lasso(ids, &polygon))
                        .unwrap_or_default(),
                )
            } else if (end - start).length() > 4.0 {
                let (a, b) = (to_px(start), to_px(end));
                Some(
                    ids.map(|ids| {
                        ids.entities_in_rect(
                            (a.x.max(0.0) as u32, a.y.max(0.0) as u32),
                            (b.x.max(0.0) as u32, b.y.max(0.0) as u32),
                        )
                    })
                    .unwrap_or_default(),
                )
            } else {
                None
            };
            if let Some(mut chosen) = chosen {
                chosen.retain(|e| !crate::viewport_ops::is_locked(world, *e));
                let mode = ui.input(|i| {
                    crate::selection::RegionMode::from_modifiers(i.modifiers.ctrl, i.modifiers.shift)
                });
                let mut next = selection.clone();
                next.apply_region(&chosen, mode);
                world.insert_resource(next);
            }
        }
    }
    // The drop ghost (P-067) and the measure overlay (P-069), in pane points.
    if !possessed {
        let mut shapes = Vec::new();
        if response.hovered()
            && world
                .get_resource::<crate::content::DropPreview>()
                .is_some_and(|p| p.frame == ui.ctx().cumulative_frame_nr())
        {
            shapes.extend(crate::overlays::drop_preview_shapes(world, &view, size_vec));
        }
        shapes.extend(crate::measure_tool::overlay_shapes(world, &view, size_vec));
        if !shapes.is_empty() {
            crate::overlays::paint_shapes_egui(ui.painter(), rect, &shapes);
        }
    }

    // Camera input. A primary drag that began on a gizmo handle stays the gizmo's (ADR-7674).
    // The navigation preset's chord table picks the gesture and `camera_nav` applies the invert
    // options, the orbit pivot, zoom toward the cursor and the fly speed (ADR-7755).
    let nav = world
        .get_resource::<crate::camera_nav::CameraNav>()
        .copied()
        .unwrap_or_default();
    // A secondary or middle drag that began on an overlay group belongs to the group's area in
    // egui; it still orbits and pans this pane (ADR-8532).
    let through = crate::viewport_overlays::camera_drag_through(ui.ctx(), rect);
    let held = |button| through && ui.input(|i| i.pointer.button_down(button));
    let hovered = response.hovered() || through;
    let primary_down = response.dragged_by(egui::PointerButton::Primary);
    let secondary_down = !kit_painting
        && (response.dragged_by(egui::PointerButton::Secondary) || held(egui::PointerButton::Secondary));
    let middle_down = response.dragged_by(egui::PointerButton::Middle) || held(egui::PointerButton::Middle);
    let drag_delta = if through {
        ui.input(|i| i.pointer.delta())
    } else {
        response.drag_delta()
    };
    let drag = Vec2::new(drag_delta.x, drag_delta.y);
    let (alt_held, shift_held) = ui.input(|i| (i.modifiers.alt, i.modifiers.shift));
    let held_button = if middle_down {
        Some(crate::camera_nav::NavButton::Middle)
    } else if secondary_down {
        Some(crate::camera_nav::NavButton::Secondary)
    } else if primary_down && !gizmo_active {
        // Alt+LMB on the gizmo is the duplicate drag (ADR-7204), not an orbit.
        Some(crate::camera_nav::NavButton::Primary)
    } else {
        None
    };
    let gesture = held_button.and_then(|button| nav.preset.gesture(button, alt_held, shift_held));
    if hovered && let Some(gesture) = gesture {
        if let Some((right, up)) = kind.screen_axes() {
            // Orthographic panes pan with every gesture but orbit.
            if gesture != crate::camera_nav::NavGesture::Orbit {
                let per_point = view.units_per_pixel(size.y);
                let d = nav.drag_delta(crate::camera_nav::NavGesture::Pan, drag);
                camera.target += (right * -d.x + up * d.y) * per_point;
            }
        } else {
            let pivot = if gesture == crate::camera_nav::NavGesture::Orbit {
                crate::camera_nav::orbit_pivot(world, &nav)
            } else {
                None
            };
            crate::camera_nav::apply_drag(&mut camera, &nav, gesture, drag, pivot);
        }
    }
    if hovered {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            if secondary_down && !kind.is_ortho() {
                camera.speed = nav.wheel_speed(camera.speed, scroll);
            } else {
                let factor = nav.wheel_factor(scroll);
                let anchor = response
                    .hover_pos()
                    .filter(|_| nav.zoom_to_cursor)
                    .and_then(|pointer| {
                        let at = pointer - rect.left_top();
                        crate::camera_nav::cursor_anchor(
                            &crate::views::ViewCamera::of(kind, &camera),
                            Vec2::new(at.x, at.y),
                            pane,
                        )
                    });
                match anchor {
                    Some(anchor) => crate::camera_nav::zoom_toward(&mut camera, factor, anchor),
                    None => camera.zoom(factor),
                }
            }
        }
        if secondary_down && !kind.is_ortho() {
            // "Speed scales with distance" flies the same keys farther (ADR-7755).
            let d = ui.input(|i| i.stable_dt) * nav.fly_speed(&camera) / camera.speed.max(1e-6);
            // The fly keys are commands: a user shortcut moves them (ADR-2696).
            let held = |id: &str| {
                crate::command_palette::hold_chord(world, id)
                    .is_some_and(|chord| ui.input(|i| crate::command_palette::chord_held(i, chord)))
            };
            let axis = |pos: &str, neg: &str| {
                if held(pos) {
                    1.0
                } else if held(neg) {
                    -1.0
                } else {
                    0.0
                }
            };
            camera.fly(
                d,
                axis("camera.fly_forward", "camera.fly_back"),
                axis("camera.fly_right", "camera.fly_left"),
                axis("camera.fly_up", "camera.fly_down"),
            );
        }
    }

    // SpaceMouse (specs/29): the active pane, while no text field has keyboard focus. Keep
    // repainting while the cap is deflected (samples only arrive when it moves).
    // With the Object tool on (§ 2.3b) the cap moves the selection and the camera stays put; it
    // runs every frame until its gesture closes after 0.3 s of rest.
    // The panel's active pane, and only in the panel that has the hotkeys. The pane itself
    // having focus does not count as egui wanting the keyboard (FU-9): egui reports it whenever
    // any widget has focus.
    let has_keys = panel_has_keys(ui, panel, instance.id);
    let keys_free = !ui.ctx().egui_wants_keyboard_input() || response.has_focus();
    if instance.layout.active == index && has_keys && keys_free {
        let dt = ui.input(|i| i.stable_dt);
        let deflected = world
            .get_resource::<SpaceMouseState>()
            .is_some_and(|s| s.axes().iter().any(|v| *v != 0.0));
        let object_tool = world
            .get_resource::<SpaceMouseNav>()
            .is_some_and(|n| n.object_tool)
            || world
                .get_resource::<SpaceMouseObjectTool>()
                .is_some_and(SpaceMouseObjectTool::is_active);
        if object_tool {
            spacemouse_object_step(world, &camera, kind, dt);
            if deflected
                || world
                    .get_resource::<SpaceMouseObjectTool>()
                    .is_some_and(SpaceMouseObjectTool::is_active)
            {
                ui.ctx().request_repaint();
            }
        } else if deflected {
            spacemouse_step(world, &mut camera, kind, dt);
            ui.ctx().request_repaint();
        }
    }

    // Click picking (GPU id buffer + terrain; the CPU mesh ray without a GPU). A pointer click
    // only: with the pane focused (FU-9) egui reports Enter and Space as a primary click, which
    // would replace the selection with whatever is under the last pointer position (ADR-8870).
    // A selection root (P-074) takes a click on its descendants unless Ctrl is held; Alt+click
    // (P-073) lists everything under the cursor instead of picking (ADR-7769).
    let (ctrl, shift) = ui.input(|i| (i.modifiers.ctrl, i.modifiers.shift));
    let stack_request = instance.layout.active == index
        && has_keys
        && world
            .remove_resource::<crate::pick_stack::PickStackRequest>()
            .is_some();
    if (pointer_clicked || stack_request) && !gizmo_active && !possessed && !tool_active {
        let pointer = if pointer_clicked {
            response.interact_pointer_pos().unwrap_or(rect.center())
        } else {
            response.hover_pos().unwrap_or(rect.center())
        };
        let local_px = pointer - rect.left_top();
        let local = pane_pointer_px(Vec2::new(local_px.x, local_px.y), pane, buffer);
        if stack_request || crate::pick_stack::click_opens(world, alt) {
            crate::mesh_source::resolve_mesh_sources(world);
            let (stack_view, stack_size) = frame
                .as_ref()
                .map_or((view, [render_size.x as u32, render_size.y as u32]), |f| {
                    (f.view(), f.size())
                });
            let entries = crate::pick_stack::pick_stack(
                world,
                &stack_view,
                stack_size,
                frame.as_ref().and_then(PaneDrawn::ids),
                local,
            );
            if !entries.is_empty() {
                world.insert_resource(crate::pick_stack::PickStackPopup::new(
                    response.id,
                    [pointer.x, pointer.y],
                    entries,
                    crate::selection::RegionMode::from_modifiers(ctrl, shift),
                ));
            }
        } else {
            let hit = match frame.as_ref() {
                Some(frame) => {
                    let size = frame.size();
                    pick_in_view(world, &frame.view(), size, frame.ids(), local).map(|(e, _)| e)
                }
                None => {
                    // No id buffer: the CPU ray against each mesh's BVH and the terrain (ADR-7204).
                    let (origin, direction) = view.ray(local, render_size);
                    crate::viewport_ops::cpu_pick(world, origin, direction, view.far).map(|(e, _)| e)
                }
            };
            // The Outliner lock: a locked entity is never picked.
            let hit = hit
                .filter(|e| !crate::viewport_ops::is_locked(world, *e))
                .map(|e| crate::selection::selection_root_of(world, e, ctrl));
            // Ctrl+click toggles: egui's second pass of the frame must not toggle it back.
            let mut stamps = world.get_resource::<StepKeyFrame>().copied().unwrap_or_default();
            if first_in_frame(&mut stamps.click, ui.ctx().cumulative_frame_nr()) {
                world.insert_resource(stamps);
                let mut next = selection.clone();
                match hit {
                    Some(entity) => next.apply_region(
                        &[entity],
                        crate::selection::RegionMode::from_modifiers(ctrl, shift),
                    ),
                    None => next.clear(),
                }
                world.insert_resource(next);
            }
        }
    }
    crate::pick_stack::popup_ui(ui, world, response.id);
    // Measure mode keys (P-069): M (the `viewport.measure` command's built-in key) toggles it in
    // the active pane, Escape leaves it.
    if instance.layout.active == index && has_keys && keys_free {
        let bare = ui.input(|i| i.modifiers.is_none());
        let toggle = !crate::command_palette::user_bound(world, "viewport.measure")
            && bare
            && !secondary_down
            && ui.input(|i| i.key_pressed(egui::Key::M));
        let measuring_now = crate::measure_tool::MeasureTool::is_active(world);
        let leave =
            measuring_now && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let mut stamps = world.get_resource::<StepKeyFrame>().copied().unwrap_or_default();
        if (toggle || leave) && first_in_frame(&mut stamps.measure, ui.ctx().cumulative_frame_nr()) {
            world.insert_resource(stamps);
            let mut tool = world
                .get_resource::<crate::measure_tool::MeasureTool>()
                .cloned()
                .unwrap_or_default();
            tool.set_active(toggle && !measuring_now);
            world.insert_resource(tool);
        }
    }

    // Hotkeys for the active pane of the panel that has them (the pane's own focus keeps them,
    // FU-9).
    let active = instance.layout.active == index && has_keys;
    // The key of this pane's eased camera moves: the main viewport's perspective pane moves the
    // world's camera, which the agent's `editor.view_preset` eases too.
    let ease_key = pane_ease_key(panel, kind, instance.id, index);
    if active && keys_free {
        let pressed = |k: egui::Key| ui.input(|i| i.key_pressed(k));
        // A user shortcut for a gizmo or framing command replaces its built-in key (ADR-2321).
        let own = |id: &str| !crate::command_palette::user_bound(world, id);
        let (own_w, own_e, own_r, own_f) = (
            own("gizmo.translate"),
            own("gizmo.rotate"),
            own("gizmo.scale"),
            own("view.frame_selection"),
        );
        let (own_drop, own_handles) = (own("selection.drop_to_floor"), own("collider.toggle_handles"));
        // ADR-7756: the select tool, the space toggle and the fixed-step rotate keys.
        let (own_q, own_space, own_step, own_step_back) = (
            own("gizmo.select"),
            own("gizmo.toggle_space"),
            own("gizmo.rotate_step"),
            own("gizmo.rotate_step_back"),
        );
        let own_note = own("review.add_note");
        let mut gizmo_state = world.get_resource::<GizmoState>().cloned().unwrap_or_default();
        if own_w && pressed(egui::Key::W) && !secondary_down {
            gizmo_state.mode = GizmoMode::Translate;
        }
        if own_e && pressed(egui::Key::E) && !secondary_down {
            gizmo_state.mode = GizmoMode::Rotate;
        }
        if own_r && pressed(egui::Key::R) {
            gizmo_state.mode = GizmoMode::Scale;
        }
        // Q: the select-only tool (ADR-7756); bare Q with the right button held flies down.
        if own_q && pressed(egui::Key::Q) && !secondary_down {
            gizmo_state.mode = GizmoMode::Select;
        }
        // Ctrl+`: the next reference space (ADR-7756), once per egui frame (a second UI pass of
        // the frame sees the same key).
        let key_frame = ui.ctx().cumulative_frame_nr();
        let mut stepped = world.get_resource::<StepKeyFrame>().copied().unwrap_or_default();
        if own_space
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Backtick))
            && stepped.space != Some(key_frame)
        {
            gizmo_state.space = gizmo_state.space.next();
            stepped.space = Some(key_frame);
        }
        // `snap` is the toggle; Ctrl inverts it while held (applied in the gizmo config).
        world.insert_resource(gizmo_state);
        // U / Shift+U: turn the selection by the angle step about the world up (ADR-7756).
        if !possessed && !world.contains_resource::<crate::render::PlayWorld>() && !secondary_down {
            let (back, ahead) = (
                own_step_back && ui.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::U)),
                own_step && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::U)),
            );
            if (back || ahead)
                && stepped.rotate != Some(key_frame)
                && !world.resource::<Selection>().is_empty()
            {
                stepped.rotate = Some(key_frame);
                if let Err(err) = crate::viewport_ops::rotate_step(world, if back { -1.0 } else { 1.0 }) {
                    tracing::warn!(target: "regia_editor", "rotate step: {err}");
                }
            }
        }
        world.insert_resource(stepped);
        // Plain F only: Ctrl+F (Cmd+F) belongs to the search box of the panel it is pressed over
        // (review sprint-v06-rev-late-k #18: the Inspector's Ctrl+F also framed the selection
        // when the viewport drew first).
        let plain_f = ui.input(|i| {
            i.key_pressed(egui::Key::F) && !i.modifiers.command && !i.modifiers.ctrl && !i.modifiers.mac_cmd
        });
        if own_f && plain_f {
            // An eased move of `EASE_SECONDS` (ADR-7204).
            let selected = world.resource::<Selection>().entities().to_vec();
            let to = crate::viewport_ops::focus_pose(world, &camera, &selected);
            world
                .get_resource_or_insert_with::<crate::viewport_ops::CameraEases>(Default::default)
                .start(ease_key, camera.clone(), to);
        }
        // Placement keys (ADR-2696); edits go to the edit world, so not while playing.
        if !possessed && !world.contains_resource::<crate::render::PlayWorld>() {
            if own_note
                && !secondary_down
                && ui.input(|input| input.modifiers.is_none())
                && pressed(egui::Key::N)
            {
                crate::review_notes::begin_note(world);
            }
            if own_drop && pressed(egui::Key::End) {
                crate::surface_tools::drop_selection(world);
            }
            // Ctrl+D duplicates the selection, one undo step (the Outliner's binding, ADR-7204).
            let ctrl_d = ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::D));
            if ctrl_d
                && !world.resource::<Selection>().is_empty()
                && let Err(err) = crate::viewport_ops::duplicate_selection(world)
            {
                tracing::warn!(target: "regia_editor", "duplicate: {err}");
            }
            // Bare C: Ctrl+C belongs to the text and the panels under it.
            let bare = ui.input(|i| i.modifiers.is_none());
            if own_handles && bare && pressed(egui::Key::C) && !secondary_down {
                crate::command_palette::run(world, "collider.toggle_handles");
            }
        }
        if pressed(egui::Key::G) {
            instance.layout.game_view = !instance.layout.game_view;
        }
        // ADR-7770: Alt+1..Alt+8 pick this pane's view mode (consumed, so the bare-digit view
        // presets below do not see them); H hides the selection in the editor only, Alt+H shows
        // everything hidden that way, Shift+H isolates the selection and restores. A user shortcut
        // for the command replaces the built-in key.
        let mode_keys = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
        ];
        for (slot, key) in mode_keys.into_iter().enumerate() {
            // The navigation preset's own Alt view keys come first (ADR-7755: the default preset's
            // Alt+2/3/4 turn to the bottom, back and opposite side): such a digit is not consumed
            // here and reaches the view keys below (ADR-8039).
            let preset_view_key = u8::try_from(slot + 1)
                .ok()
                .and_then(|digit| nav.preset.view_key(digit, true))
                .is_some();
            if !preset_view_key
                && !crate::command_palette::user_bound(world, &format!("view.mode_{}", slot + 1))
                && ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT, key))
            {
                instance.layout.display_mut(index).view_mode = crate::views::ViewMode::KEYED[slot];
            }
        }
        if !crate::command_palette::user_bound(world, "view.unhide_all")
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::H))
        {
            crate::view_state::unhide_all(world);
        }
        // A toggle: once per egui frame (the UI pass can run twice with the same input).
        let isolate_key = !crate::command_palette::user_bound(world, "view.isolate")
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::H));
        let frame_nr = ui.ctx().cumulative_frame_nr();
        if isolate_key
            && world
                .get_resource::<IsolateKeyFrame>()
                .is_none_or(|f| f.0 != frame_nr)
        {
            world.insert_resource(IsolateKeyFrame(frame_nr));
            if let Err(err) = crate::view_state::toggle_isolate(world) {
                tracing::info!(target: "regia_editor", code = err.code(), "isolate: nothing selected");
            }
        }
        if !crate::command_palette::user_bound(world, "view.hide_selection")
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::H))
        {
            let selected = world.resource::<Selection>().entities().to_vec();
            crate::view_state::set_view_hidden(world, &selected, true);
        }
        if pressed(egui::Key::F8) {
            instance.layout.possessed = !instance.layout.possessed;
        }
        // Pilot the selection, again to eject (ADR-7768); a user shortcut for `view.pilot` replaces it.
        if panel == PanelKind::Main
            && !crate::command_palette::user_bound(world, "view.pilot")
            && ui
                .input_mut(|i| i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::L))
        {
            crate::command_palette::run(world, "view.pilot");
        }
        let digits = [
            egui::Key::Num0,
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
            egui::Key::Num9,
        ];
        for (slot, key) in digits.iter().enumerate() {
            if pressed(*key) {
                let (ctrl, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
                if ctrl || shift {
                    // Bookmarks: Ctrl+digit stores, Shift+digit recalls (ADR-7204). Ctrl+0 is only
                    // bookmark 0: egui's keyboard zoom, whose reset was Ctrl+0, is off and the
                    // interface zoom is View ▸ Reset zoom (ADR-7733).
                    let mut marks = world
                        .get_resource_or_insert_with::<crate::views::CameraBookmarks>(Default::default)
                        .clone();
                    if ctrl {
                        marks.set(slot, camera.clone());
                    } else if let Some(stored) = marks.get(slot) {
                        let to = stored.clone();
                        world
                            .get_resource_or_insert_with::<crate::viewport_ops::CameraEases>(Default::default)
                            .start(ease_key, camera.clone(), to);
                    }
                    world.insert_resource(marks);
                } else if let Some(preset) = u8::try_from(slot)
                    .ok()
                    .and_then(|digit| nav.preset.view_key(digit, ui.input(|i| i.modifiers.alt)))
                {
                    // The navigation preset's view keys (bare 1-4, Alt for the opposite views;
                    // ADR-7755): eased in a perspective pane (specs/19 § 3); an orthographic pane
                    // turns to that view and keeps its camera.
                    if kind.is_ortho() {
                        if let Some(ortho) = preset.ortho_kind() {
                            instance.layout.panes[index.min(3)].kind = ortho;
                        }
                    } else {
                        let to = preset.pose(&camera);
                        world
                            .get_resource_or_insert_with::<crate::viewport_ops::CameraEases>(Default::default)
                            .start(ease_key, camera.clone(), to);
                    }
                }
            }
        }
    }
    // A running eased move (focus, preset, bookmark) advances by the frame time; once per egui
    // frame (the UI pass can run twice).
    let frame_nr = ui.ctx().cumulative_frame_nr();
    let step_ease = world
        .get_resource::<EaseFrame>()
        .is_none_or(|f| !f.0.contains(&(ease_key, frame_nr)));
    if step_ease && let Some(mut eases) = world.remove_resource::<crate::viewport_ops::CameraEases>() {
        let dt = ui.input(|i| i.stable_dt);
        if let Some(pose) = eases.advance(ease_key, dt) {
            camera = pose;
            ui.ctx().request_repaint();
        }
        world.insert_resource(eases);
        let mut seen = world.remove_resource::<EaseFrame>().unwrap_or_default();
        seen.0.retain(|(_, nr)| *nr == frame_nr);
        seen.0.push((ease_key, frame_nr));
        world.insert_resource(seen);
    } else if let Some(pose) = world
        .get_resource::<crate::viewport_ops::CameraEases>()
        .and_then(|eases| eases.moves.get(&ease_key))
        .map(crate::viewport_ops::CameraEase::sample)
    {
        camera = pose;
    }
    // 2D mode hotkeys (`B`, `,` `.` `[` `]`) and framing the map in the front pane.
    // The pane's own keyboard focus keeps them, as for the viewport hotkeys (ADR-8870).
    crate::mode2d::viewport_frame(
        ui,
        world,
        &mut instance.layout,
        index,
        &mut camera,
        active,
        response.has_focus(),
    );
    // Pilot and look-through follow, the camera preview, composition guides (ADR-7768).
    if panel == PanelKind::Main && kind == crate::views::ViewKind::Perspective && !possessed {
        crate::screenshot::note_pane_size(world, buffer);
        crate::viewport_pilot::pane_hook(ui, world, device, rect, &mut camera);
    }
    // A piloted entity sits at the eye: no gizmo on it while it is piloted (ADR-7768).
    let tool_active = tool_active || crate::viewport_pilot::hides_gizmo(world);
    instance.set_pane_camera(index, camera.clone());

    // The interactive gizmo over the primary selection (hidden while the terrain tool sculpts).
    state.gizmo_shown[index] = false;
    // A temporary pivot offset lapses once the selection changes (ADR-7756), also to an empty
    // selection or while no gizmo shows: before the early returns below (review finding 12).
    let selected: Vec<Entity> = world.resource::<Selection>().entities().to_vec();
    crate::viewport_ops::lapse_pivot_offset(world, &selected);
    if possessed || tool_active || instance.layout.game_view {
        return drawn;
    }
    let Some(primary) = world.resource::<Selection>().primary() else {
        return drawn;
    };
    let Some(current) = world.get::<Transform>(primary).copied() else {
        return drawn;
    };
    // Gizmos do not attach to live data of the DCC feed (specs/60 § 2.4).
    if world.get::<regia_scene::prelude::FeedOwned>(primary).is_some() {
        return drawn;
    }
    // The collider tool's grips replace the gizmo on an entity with a collider (or a reflection
    // capture, ADR-7663).
    if crate::blockout::handles_show(world, primary)
        || crate::collider_handles::tool_shows(world, primary)
        || crate::capture_helper::handles_show(world, primary)
        || crate::smart_object_handles::handles_show(world, primary)
    {
        return drawn;
    }
    let gizmo_state = world.get_resource::<GizmoState>().cloned().unwrap_or_default();
    // The select-only tool shows no gizmo: a press picks or box-selects (ADR-7756).
    if gizmo_state.mode == GizmoMode::Select {
        return drawn;
    }
    let view = crate::views::ViewCamera::of(kind, &camera);
    let aspect = pane_gizmo_aspect(buffer);
    // Holding the pivot key (P) turns the gizmo into a handle on the pivot itself: dragging it
    // moves the pivot, not the selection (rotate and scale then turn about the moved point).
    let pivot_held = gizmo_state.mode != GizmoMode::Translate
        && crate::command_palette::hold_chord(world, "gizmo.pivot_offset")
            .is_some_and(|chord| ui.input(|i| crate::command_palette::chord_held(i, chord)));
    let modes = match gizmo_state.mode {
        _ if pivot_held => transform_gizmo_egui::GizmoMode::all_translate(),
        GizmoMode::Translate | GizmoMode::Select => transform_gizmo_egui::GizmoMode::all_translate(),
        GizmoMode::Rotate => transform_gizmo_egui::GizmoMode::all_rotate(),
        GizmoMode::Scale => transform_gizmo_egui::GizmoMode::all_scale(),
    };
    // The gizmo sits on the pivot of the whole selection (median, the active entity's origin for
    // individual origins and active element, the meshes' box for bounds center, plus the temporary
    // offset) with the active entity's rotation and scale, drawn in world space.
    let pivot_mode = world
        .get_resource::<crate::viewport_ops::GizmoPivot>()
        .copied()
        .unwrap_or_default()
        .0;
    let items = crate::viewport_ops::selection_items(world, &selected);
    let frame_t = crate::viewport_ops::gizmo_frame_in(world, &items, Some(primary), pivot_mode, &selected)
        .unwrap_or(current);
    let parent = crate::viewport_ops::parent_affine(world, primary);
    let frame_world = crate::viewport_ops::to_world(parent, &frame_t);
    // Reference space (ADR-7756): the library knows world and the target's own axes, so the target
    // carries the space's basis; a scale runs along the entity's own axes in parent and view space.
    let space = match gizmo_state.space {
        GizmoSpace::Parent | GizmoSpace::View if gizmo_state.mode == GizmoMode::Scale && !pivot_held => {
            GizmoSpace::Local
        }
        space => space,
    };
    let (_, parent_rotation, _) = parent.to_scale_rotation_translation();
    let basis = crate::viewport_ops::space_basis(
        space,
        frame_world.rotation,
        parent_rotation,
        crate::viewport_ops::view_rotation(&view),
    );
    let shown = Transform {
        rotation: if space == GizmoSpace::World {
            frame_world.rotation
        } else {
            basis
        },
        ..frame_world
    };
    state.gizmo_shown[index] = true;
    let gizmo = &mut state.gizmos[index];
    gizmo.update_config(transform_gizmo_egui::GizmoConfig {
        view_matrix: to_row(view.view()),
        projection_matrix: to_row(view.projection(aspect)),
        viewport: rect,
        modes,
        orientation: if space == GizmoSpace::World {
            transform_gizmo_egui::GizmoOrientation::Global
        } else {
            transform_gizmo_egui::GizmoOrientation::Local
        },
        // The snap toggle, inverted while Ctrl is held (ADR-7204).
        snapping: crate::viewport_ops::effective_snap(gizmo_state.snap, ui.input(|i| i.modifiers.command)),
        snap_distance: gizmo_state.snap_distance,
        snap_angle: gizmo_state.snap_angle_deg.to_radians(),
        snap_scale: gizmo_state.snap_scale,
        pixels_per_point: ui.ctx().pixels_per_point(),
        ..*gizmo.config()
    });
    let target = gizmo_target(&shown);
    let translating = gizmo_state.mode == GizmoMode::Translate && !pivot_held;
    // Vertex snap (V held, or the palette's toggle): the pivot goes to the vertex under the pointer.
    let snap_held = translating
        && (world
            .get_resource::<crate::surface_tools::PlacementOptions>()
            .is_some_and(|options| options.vertex_snap)
            || crate::command_palette::hold_chord(world, "gizmo.vertex_snap")
                .is_some_and(|chord| ui.input(|i| crate::command_palette::chord_held(i, chord))));
    // Only the pane under the pointer: each pane has its own camera.
    let pane_pointer = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| rect.contains(*p))
        .map(|p| Vec2::new(p.x - rect.left(), p.y - rect.top()));
    let vertex = if snap_held {
        pane_pointer
            .and_then(|local| crate::surface_tools::vertex_snap_at(world, &view, size_vec, local, &[primary]))
    } else {
        None
    };
    if let Some(snap) = vertex
        && let Some(at) = view.project(snap.position, size_vec)
    {
        ui.painter().circle_stroke(
            rect.left_top() + egui::vec2(at.x, at.y),
            6.0,
            egui::Stroke::new(2.0, crate::theme::tokens(world).overlay.selection_outline),
        );
    }
    // Surface snap (ADR-7756): while translating, the pivot slides on the surface under the pointer.
    let surface_options = world
        .get_resource::<crate::surface_tools::SurfaceSnap>()
        .copied()
        .unwrap_or_default();
    let surface_on = translating && vertex.is_none() && surface_options.enabled;
    let mut drag = world.remove_resource::<GizmoDrag>().unwrap_or_default();
    if let Some((_result, updated)) = gizmo_update(ui, gizmo, &response, &[target])
        && let Some(new_shown) = transform_from_gizmo(&updated[0])
    {
        if pivot_held {
            // The pivot handle: only the offset moves; no edit, no undo entry.
            // The pivot without an offset (no selection has one), in world space.
            let bare = crate::viewport_ops::selection_pivot(world, &items, Some(primary), pivot_mode, &[])
                .unwrap_or(current.translation);
            let base = parent.transform_point3(bare);
            crate::viewport_ops::set_pivot_offset(world, &selected, new_shown.translation - base);
            world.insert_resource(drag);
            return drawn;
        }
        // Back from the shown basis to the entity's own world pose, then to its parent's space.
        let turn = (new_shown.rotation * shown.rotation.inverse()).normalize();
        let mut new_world = Transform {
            translation: new_shown.translation,
            rotation: (turn * frame_world.rotation).normalize(),
            scale: if gizmo_state.mode == GizmoMode::Scale {
                new_shown.scale
            } else {
                frame_world.scale
            },
        };
        if surface_on
            && let Some(local) = pane_pointer
            && let Some(hit) = crate::surface_tools::surface_at(world, &view, size_vec, local, &selected)
        {
            new_world = crate::surface_tools::surface_snap_pose(&frame_world, &hit, &surface_options);
        }
        let new = crate::viewport_ops::to_parent(parent, &new_world).unwrap_or(frame_t);
        let mut delta = crate::viewport_ops::GizmoDelta::between(&frame_t, &new);
        if let Some(translation) =
            vertex.and_then(|snap| crate::surface_tools::local_translation_for(world, primary, snap.position))
        {
            delta.translation = translation - current.translation;
        }
        // A new drag is its own undo entry; Alt at its start duplicates the selection and the
        // copies move (one undo step with the move, ADR-7204).
        let start = !drag.active;
        if start {
            drag.active = true;
            world
                .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
                .seal();
        }
        let duplicate = start && ui.input(|i| i.modifiers.alt);
        if let Err(err) =
            crate::viewport_ops::gizmo_drag(world, &selected, Some(primary), &delta, duplicate, false)
        {
            tracing::warn!(target: "regia_editor", "gizmo drag: {err}");
        }
    } else if drag.active && !ui.input(|i| i.pointer.primary_down()) {
        drag.active = false;
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .seal();
    }
    world.insert_resource(drag);
    drawn
}

/// The egui frame whose Ctrl+` and U presses already acted (a second UI pass of a frame sees the
/// same key events; ADR-7756).
#[derive(Resource, Debug, Clone, Copy, Default)]
struct StepKeyFrame {
    space: Option<u64>,
    rotate: Option<u64>,
    /// The M / Escape measure toggle (review sprint-w12-rev-g2 #1).
    measure: Option<u64>,
    /// The pick click's selection change (review sprint-w12-rev-g2 #3).
    click: Option<u64>,
}

/// Whether a once-per-frame action stamped in `slot` may act in egui frame `frame` (it then stamps
/// it): egui's second UI pass of a frame sees the same key and click events.
fn first_in_frame(slot: &mut Option<u64>, frame: u64) -> bool {
    if *slot == Some(frame) {
        return false;
    }
    *slot = Some(frame);
    true
}

/// Whether a gizmo drag is in progress (its first frame starts a new undo entry and, with Alt,
/// the duplicate).
#[derive(Resource, Debug, Default)]
struct GizmoDrag {
    active: bool,
}

/// glam (column-major) → the gizmo's `mint` row matrix.
fn to_row(m: Mat4) -> mint::RowMatrix4<f64> {
    mint::RowMatrix4 {
        x: [
            f64::from(m.x_axis.x),
            f64::from(m.y_axis.x),
            f64::from(m.z_axis.x),
            f64::from(m.w_axis.x),
        ]
        .into(),
        y: [
            f64::from(m.x_axis.y),
            f64::from(m.y_axis.y),
            f64::from(m.z_axis.y),
            f64::from(m.w_axis.y),
        ]
        .into(),
        z: [
            f64::from(m.x_axis.z),
            f64::from(m.y_axis.z),
            f64::from(m.z_axis.z),
            f64::from(m.w_axis.z),
        ]
        .into(),
        w: [
            f64::from(m.x_axis.w),
            f64::from(m.y_axis.w),
            f64::from(m.z_axis.w),
            f64::from(m.w_axis.w),
        ]
        .into(),
    }
}

/// `Transform` → gizmo transform (glam versions differ across crates; convert through scalars,
/// per the docs/03 note on transform-gizmo-egui).
fn gizmo_target(t: &Transform) -> transform_gizmo_egui::math::Transform {
    transform_gizmo_egui::math::Transform {
        scale: mint::Vector3 {
            x: f64::from(t.scale.x),
            y: f64::from(t.scale.y),
            z: f64::from(t.scale.z),
        },
        rotation: mint::Quaternion {
            s: f64::from(t.rotation.w),
            v: mint::Vector3 {
                x: f64::from(t.rotation.x),
                y: f64::from(t.rotation.y),
                z: f64::from(t.rotation.z),
            },
        },
        translation: mint::Vector3 {
            x: f64::from(t.translation.x),
            y: f64::from(t.translation.y),
            z: f64::from(t.translation.z),
        },
    }
}

/// Gizmo transform → `Transform`.
fn transform_from_gizmo(value: &transform_gizmo_egui::math::Transform) -> Option<Transform> {
    let quat = Quat::from_xyzw(
        value.rotation.v.x as f32,
        value.rotation.v.y as f32,
        value.rotation.v.z as f32,
        value.rotation.s as f32,
    );
    if !quat.is_finite() {
        return None;
    }
    let translation = Vec3::new(
        value.translation.x as f32,
        value.translation.y as f32,
        value.translation.z as f32,
    );
    let scale = Vec3::new(value.scale.x as f32, value.scale.y as f32, value.scale.z as f32);
    if !translation.is_finite() || !scale.is_finite() {
        return None;
    }
    let quat = if quat.length_squared() < 1e-8 {
        Quat::IDENTITY
    } else {
        quat.normalize()
    };
    Some(Transform {
        translation,
        rotation: quat,
        scale,
    })
}

/// Clips the world-space segment `a→b` to the plane a `near_offset` metres in front of the eye
/// along `forward`: `None` when the whole segment is behind, the clipped pair otherwise.
/// Grid lines crossing the camera horizon stay straight instead of skewing through the
/// projection's behind-camera singularity.
#[must_use]
pub fn clip_segment_to_near(
    eye: Vec3,
    forward: Vec3,
    near_offset: f32,
    a: Vec3,
    b: Vec3,
) -> Option<(Vec3, Vec3)> {
    let in_front = |p: Vec3| (p - eye).dot(forward) > near_offset;
    let va = in_front(a);
    let vb = in_front(b);
    if va && vb {
        return Some((a, b));
    }
    if !va && !vb {
        return None;
    }
    let along = (b - a).dot(forward);
    if along.abs() < 1e-6 {
        return None;
    }
    let t = (near_offset - (a - eye).dot(forward)) / along;
    let cut = a + (b - a) * t;
    Some(if va { (a, cut) } else { (cut, b) })
}

/// Major grid line (the axes through the target).
pub const GRID_MAJOR: egui::Color32 = egui::Color32::from_rgb(120, 120, 130);
/// Minor grid line.
pub const GRID_MINOR: egui::Color32 = egui::Color32::from_rgb(60, 60, 68);

/// Translate/scale gizmo axis colours. Saturated so a capture can count them apart
/// from the sky and the lit proxies.
pub const GIZMO_X: egui::Color32 = egui::Color32::from_rgb(220, 40, 40);
/// Y axis.
pub const GIZMO_Y: egui::Color32 = egui::Color32::from_rgb(40, 200, 60);
/// Z axis.
pub const GIZMO_Z: egui::Color32 = egui::Color32::from_rgb(50, 90, 230);
/// Rotate-mode ring.
pub const GIZMO_RING: egui::Color32 = egui::Color32::from_rgb(230, 190, 60);

/// Measurements of one viewport capture (`specs/19` § 3, the M11 recheck).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportReport {
    /// Mean luminance of the top band (the sky), 0..1.
    pub sky_luminance: f32,
    /// Mean blue−red of that band, 0..1. Positive is a blue sky.
    pub sky_blue_bias: f32,
    /// Brightest luminance in the central half, ignoring overlay pixels, 0..1.
    pub lit_peak: f32,
    /// The lit-object shading variation in that half: the widest luminance range
    /// within any single scan line. The sky dome's gradient runs top-to-bottom,
    /// so a sky-only frame has a near-zero value; a lit object (and its shadowed
    /// faces) breaks a scan line and raises it.
    pub shading_range: f32,
    /// The selection outline is on screen (at least 8 pixels of
    /// [`crate::overlays::SELECTION_OUTLINE`]).
    pub selection_highlight: bool,
    /// Pixels painted in the reference-grid colours.
    pub grid_pixels: u32,
    /// How many of the three axis colours are present (3 = translate/scale gizmo).
    pub gizmo_axes: u32,
    /// Pixels of the rotate ring.
    pub gizmo_ring_pixels: u32,
}

impl ViewportReport {
    /// The sky band is a lit horizon, not the renderer's near-black clear.
    #[must_use]
    pub fn sky_visible(self) -> bool {
        self.sky_luminance > 0.35 && self.sky_blue_bias > 0.02
    }

    /// Something in the middle of the frame is lit, and not the sky's flat
    /// vertical gradient: a lit object shows scan-line shading variation.
    #[must_use]
    pub fn shading_visible(self) -> bool {
        self.lit_peak > 0.40 && self.shading_range > 0.04
    }

    /// The reference grid was painted.
    #[must_use]
    pub fn grid_visible(self) -> bool {
        self.grid_pixels >= 40
    }

    /// The translate/scale gizmo's three axes are on screen.
    #[must_use]
    pub fn gizmo_visible(self) -> bool {
        self.gizmo_axes == 3
    }

    /// What this frame fails, for an assertion message. Empty when it passes
    /// `sky`, `shading`, `grid`, `gizmo` and `highlight`.
    #[must_use]
    pub fn failures(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.sky_visible() {
            out.push("sky/background");
        }
        if !self.shading_visible() {
            out.push("lit shading");
        }
        if !self.selection_highlight {
            out.push("selection highlight");
        }
        if !self.gizmo_visible() {
            out.push("gizmo");
        }
        if !self.grid_visible() {
            out.push("grid");
        }
        out
    }
}

/// Measures `image` the way the M11 viewport check does.
#[must_use]
pub fn measure_viewport(image: &egui::ColorImage) -> ViewportReport {
    measure_viewport_with(image, &crate::theme::OverlayTokens::regia_default())
}

/// [`measure_viewport`] for a themed editor: the overlay colors come from `colors`.
#[must_use]
pub fn measure_viewport_with(
    image: &egui::ColorImage,
    colors: &crate::theme::OverlayTokens,
) -> ViewportReport {
    let overlay_colors = colors.colors();
    let w = image.width().max(1);
    let h = image.height().max(1);
    let sky_rows = (h / 8).max(1);
    let mut sky_luma = 0.0_f64;
    let mut sky_bias = 0.0_f64;
    let mut sky_n = 0_u64;
    for y in 0..sky_rows {
        for x in 0..w {
            let p = image.pixels[y * w + x];
            if overlay_colors.contains(&p) {
                continue;
            }
            sky_luma += f64::from(luma(p));
            sky_bias += f64::from(p.b()) - f64::from(p.r());
            sky_n += 1;
        }
    }
    let sky_n = sky_n.max(1) as f64;
    let x0 = w / 4;
    let x1 = 3 * w / 4;
    let y0 = h / 4;
    let y1 = 3 * h / 4;
    let mut outline_pixels = 0_u32;
    let mut grid_pixels = 0_u32;
    let mut axis_x = false;
    let mut axis_y = false;
    let mut axis_z = false;
    let mut ring = 0_u32;
    for p in &image.pixels {
        if *p == colors.grid_major || *p == colors.grid_minor {
            grid_pixels += 1;
        }
        if *p == colors.gizmo_x {
            axis_x = true;
        }
        if *p == colors.gizmo_y {
            axis_y = true;
        }
        if *p == colors.gizmo_z {
            axis_z = true;
        }
        if *p == colors.gizmo_ring {
            ring += 1;
        }
        // The selection reads as its outline (a warm material is not a selection).
        if *p == colors.selection_outline {
            outline_pixels += 1;
        }
    }

    // Lit-object shading variation in the central half. The sky dome's gradient
    // runs top-to-bottom, so a single scan line is nearly uniform; a lit object
    // and its shadowed faces break that and widen the per-row luminance range.
    let mut lit_peak = 0.0_f32;
    let mut shading_range = 0.0_f32;
    for y in y0..y1 {
        let mut row_lo = 1.0_f32;
        let mut row_hi = 0.0_f32;
        let mut row_n = 0_u32;
        for x in x0..x1 {
            let p = image.pixels[y * w + x];
            if overlay_colors.contains(&p) {
                continue;
            }
            let value = luma(p);
            row_lo = row_lo.min(value);
            row_hi = row_hi.max(value);
            row_n += 1;
        }
        if row_n == 0 {
            continue;
        }
        lit_peak = lit_peak.max(row_hi);
        shading_range = shading_range.max(row_hi - row_lo);
    }
    let axes = u32::from(axis_x) + u32::from(axis_y) + u32::from(axis_z);
    ViewportReport {
        sky_luminance: (sky_luma / sky_n) as f32,
        sky_blue_bias: (sky_bias / sky_n / 255.0) as f32,
        lit_peak,
        shading_range,
        selection_highlight: outline_pixels >= 8,
        grid_pixels,
        gizmo_axes: axes,
        gizmo_ring_pixels: ring,
    }
}

fn luma(p: egui::Color32) -> f32 {
    (f32::from(p.r()) * 0.2126 + f32::from(p.g()) * 0.7152 + f32::from(p.b()) * 0.0722) / 255.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review sprint-w12-rev-g2 #1 and #3: the measure toggle and the pick click act once per egui
    /// frame, so the second pass neither turns measure mode back off nor toggles a Ctrl+click back.
    #[test]
    fn once_per_frame_stamps_skip_the_second_pass() {
        let mut slot = None;
        assert!(first_in_frame(&mut slot, 4));
        assert!(!first_in_frame(&mut slot, 4), "the second pass of frame 4");
        assert!(first_in_frame(&mut slot, 5));
        let mut stamps = StepKeyFrame::default();
        assert!(first_in_frame(&mut stamps.measure, 9));
        assert!(
            first_in_frame(&mut stamps.click, 9),
            "each action has its own stamp"
        );
    }
    use crate::selection::SelectMode;

    /// ADR-8366: a pane thinner than 16 : PANE_MAX_SIDE (here no height at all, as under a wrapped
    /// toolbar in a small window) still asks for a texture egui can hold: neither side past
    /// `PANE_MAX_SIDE`, neither under 16. It asked for 4288 × 16, and egui's 2048 limit panicked.
    #[test]
    fn a_pane_with_no_height_stays_inside_the_texture_limit() {
        for pane in [
            Vec2::new(268.0, 0.0),
            Vec2::new(5000.0, 1.0),
            Vec2::new(1.0, 3000.0),
            Vec2::new(0.0, 0.0),
        ] {
            let [width, height] = pane_buffer_size(pane);
            for side in [width, height] {
                assert!(
                    (16..=PANE_MAX_SIDE as u32).contains(&side),
                    "{pane:?} -> {width} x {height}"
                );
            }
        }
        assert_eq!(pane_buffer_size(Vec2::new(268.0, 0.0)), [2040, 16]);
    }

    #[test]
    fn pane_buffer_keeps_aspect_and_the_pointer_scales_each_axis() {
        let pane = Vec2::new(10.0, 298.0);
        let buffer = pane_buffer_size(pane);
        assert!(buffer[0] >= 16 && buffer[1] >= 16, "{buffer:?}");
        let pane_aspect = pane.x / pane.y;
        let buffer_aspect = buffer[0] as f32 / buffer[1] as f32;
        assert!(
            (pane_aspect - buffer_aspect).abs() < 0.002,
            "pane {pane_aspect} buffer {buffer_aspect} ({buffer:?})"
        );
        let center = pane_pointer_px(Vec2::new(5.0, 149.0), pane, buffer);
        let expected_x = 5.0 * buffer[0] as f32 / pane.x;
        let expected_y = 149.0 * buffer[1] as f32 / pane.y;
        assert!((center.x - expected_x).abs() < 1e-3, "{center:?}");
        assert!((center.y - expected_y).abs() < 1e-3, "{center:?}");
        assert!(center.x < buffer[0] as f32 && center.y < buffer[1] as f32);
        let low = pane_pointer_px(Vec2::new(1.0, 250.0), pane, buffer);
        assert!(
            low.y < buffer[1] as f32,
            "y=250 mapped to {} on a buffer {} rows tall",
            low.y,
            buffer[1]
        );
        let gizmo = pane_gizmo_aspect(buffer);
        assert!(
            (gizmo - buffer_aspect).abs() < 0.002,
            "gizmo {gizmo} buffer {buffer_aspect}"
        );

        let wide = Vec2::new(800.9, 600.0);
        let wide_buffer = pane_buffer_size(wide);
        assert_eq!(wide_buffer[0], 800, "{wide_buffer:?}");
        let edge = pane_pointer_px(Vec2::new(800.4, 10.0), wide, wide_buffer);
        assert!(
            edge.x < wide_buffer[0] as f32,
            "x {} landed on or past width {}",
            edge.x,
            wide_buffer[0]
        );
        let far = pane_pointer_px(wide, wide, wide_buffer);
        assert!(far.x <= (wide_buffer[0] - 1) as f32, "{}", far.x);
        assert!(far.y <= (wide_buffer[1] - 1) as f32, "{}", far.y);
    }

    #[test]
    fn transform_from_gizmo_rejects_non_finite_translation_and_scale() {
        fn xform(translation: [f64; 3], scale: [f64; 3]) -> transform_gizmo_egui::math::Transform {
            transform_gizmo_egui::math::Transform {
                translation: mint::Vector3 {
                    x: translation[0],
                    y: translation[1],
                    z: translation[2],
                },
                rotation: mint::Quaternion {
                    s: 1.0,
                    v: mint::Vector3 {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                    },
                },
                scale: mint::Vector3 {
                    x: scale[0],
                    y: scale[1],
                    z: scale[2],
                },
            }
        }
        assert!(transform_from_gizmo(&xform([1e40, 0.0, 0.0], [1.0, 1.0, 1.0])).is_none());
        assert!(transform_from_gizmo(&xform([f64::NAN, 0.0, 0.0], [1.0, 1.0, 1.0])).is_none());
        assert!(transform_from_gizmo(&xform([0.0, 0.0, 0.0], [f64::NAN, 1.0, 1.0])).is_none());
        let kept = transform_from_gizmo(&xform([1.0, 2.0, 3.0], [1.0, 1.0, 1.0])).expect("finite");
        assert_eq!(kept.translation, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(kept.scale, Vec3::new(1.0, 1.0, 1.0));
    }

    #[test]
    fn grid_segments_clip_at_the_near_plane() {
        let eye = Vec3::new(0.0, 4.0, 8.0);
        let forward = (-eye).normalize(); // looking at the origin
        let near = 0.2;

        // Fully behind the eye: dropped.
        let behind = Vec3::new(0.0, 0.0, 20.0);
        assert_eq!(
            clip_segment_to_near(eye, forward, near, behind, behind + Vec3::X),
            None
        );
        // Fully in front: unchanged.
        let front = Vec3::new(0.0, 0.0, -5.0);
        assert_eq!(
            clip_segment_to_near(eye, forward, near, front, front + Vec3::X),
            Some((front, front + Vec3::X))
        );
        // Crossing: the cut point sits on the near plane.
        let a = Vec3::new(0.0, 4.0, -8.0); // beside the eye, in front
        let b = Vec3::new(0.0, 4.0, 24.0); // behind
        let (kept, cut) = clip_segment_to_near(eye, forward, near, a, b).expect("clipped");
        assert_eq!(kept, a, "the in-front end is kept");
        assert!(
            ((cut - eye).dot(forward) - near).abs() < 1e-4,
            "the cut lies on the near plane: {cut:?}"
        );
        // A line parallel to the plane with one endpoint behind is dropped, not skewed.
        let side = Vec3::new(1.0, 0.0, 0.0);
        assert_eq!(
            clip_segment_to_near(eye, forward, near, behind, behind + side),
            None
        );
    }

    /// Fills `image` with a vertical sky gradient: a blue zenith at the top, a
    /// bright near-neutral horizon at the vertical centre, and dark ground below.
    /// Every scan line is uniform, so the sky alone has no horizontal variation.
    fn fill_sky_gradient(image: &mut egui::ColorImage) {
        let w = image.width();
        let h = image.height();
        let half = h as f32 / 2.0;
        for y in 0..h {
            // t = -1 at the top, +1 at the bottom, 0 at the horizon.
            let t = (y as f32 - half) / half;
            let (r, g, b) = if t < 0.0 {
                let f = -t;
                (120.0 - 80.0 * f, 160.0 - 50.0 * f, 230.0 - 15.0 * f)
            } else {
                let f = t;
                (200.0 - 170.0 * f, 215.0 - 185.0 * f, 235.0 - 210.0 * f)
            };
            let color = egui::Color32::from_rgb(r as u8, g as u8, b as u8);
            for x in 0..w {
                image.pixels[y * w + x] = color;
            }
        }
    }

    #[test]
    fn measure_accepts_a_lit_frame_and_rejects_a_black_one() {
        let w = 64;
        let h = 48;
        let mut image = egui::ColorImage::filled([w, h], egui::Color32::from_rgb(5, 5, 6));
        let black = measure_viewport(&image);
        assert!(!black.sky_visible(), "{black:?}");
        assert!(!black.shading_visible(), "{black:?}");
        assert!(black.failures().len() >= 4, "{black:?}");

        // A sky-only frame passes `sky_visible` but must fail `shading_visible`:
        // the gradient is not lit-object variation.
        fill_sky_gradient(&mut image);
        let sky_only = measure_viewport(&image);
        assert!(sky_only.sky_visible(), "{sky_only:?}");
        assert!(
            !sky_only.shading_visible(),
            "sky alone must not shade: {sky_only:?}"
        );

        // A gizmo-only frame must not read as a selection: the warm X axis is an
        // overlay, not a proxy.
        image.pixels[24 * w + 28] = GIZMO_X;
        image.pixels[24 * w + 30] = GIZMO_Y;
        image.pixels[24 * w + 32] = GIZMO_Z;
        let gizmo_only = measure_viewport(&image);
        assert!(!gizmo_only.selection_highlight, "gizmo alone: {gizmo_only:?}");

        // A warm material alone is not a selection: only the outline colour is.
        image.pixels[24 * w + 26] = egui::Color32::from_rgb(230, 130, 50);
        assert!(
            !measure_viewport(&image).selection_highlight,
            "warm material is not a selection"
        );

        // A lit object in the centre: a grey cube whose bright and shadowed faces
        // share scan lines (horizontal shading variation), the selection outline,
        // the grid and the three axes.
        for y in 18..30 {
            for x in 24..40 {
                image.pixels[y * w + x] = if x < 32 {
                    egui::Color32::from_rgb(220, 220, 220) // lit face
                } else {
                    egui::Color32::from_rgb(90, 90, 96) // shadowed face
                };
            }
        }
        for x in 23..41 {
            image.pixels[17 * w + x] = crate::overlays::SELECTION_OUTLINE; // outline above the cube
        }
        for x in 0..48 {
            image.pixels[40 * w + x] = GRID_MINOR;
        }
        image.pixels[20 * w + 20] = GIZMO_X;
        image.pixels[20 * w + 22] = GIZMO_Y;
        image.pixels[20 * w + 24] = GIZMO_Z;
        let report = measure_viewport(&image);
        assert!(
            report.failures().is_empty(),
            "{report:?} failed {:?}",
            report.failures()
        );
    }

    #[test]
    fn shading_visible_rejects_a_sky_only_frame() {
        let mut image = egui::ColorImage::filled([64, 48], egui::Color32::BLACK);
        fill_sky_gradient(&mut image);
        let report = measure_viewport(&image);
        assert!(report.sky_visible(), "the frame is a sky: {report:?}");
        assert!(
            !report.shading_visible(),
            "the sky gradient is not lit-object shading: {report:?}"
        );
        assert!(report.shading_range < 0.04, "range: {report:?}");
    }

    #[test]
    fn selection_highlight_rejects_a_gizmo_only_frame() {
        let w = 64;
        let mut image = egui::ColorImage::filled([w, 48], egui::Color32::from_rgb(120, 160, 230));
        // Only the gizmo's warm X axis (and the other axes) are painted: no proxy.
        for x in 24..34 {
            image.pixels[24 * w + x] = GIZMO_X;
        }
        image.pixels[24 * w + 30] = GIZMO_Y;
        image.pixels[24 * w + 32] = GIZMO_Z;
        let report = measure_viewport(&image);
        assert_eq!(report.gizmo_axes, 3, "the axes are on screen: {report:?}");
        assert!(
            !report.selection_highlight,
            "gizmo axes are overlays, not a selection: {report:?}"
        );
    }

    #[test]
    fn hidden_entities_are_not_pickable() {
        let mut world = World::new();
        let visible = world
            .spawn(regia_scene::prelude::Transform {
                translation: Vec3::new(0.0, 0.0, -5.0),
                ..Default::default()
            })
            .id();
        let hidden = world
            .spawn(regia_scene::prelude::Transform {
                translation: Vec3::new(0.0, 0.0, -6.0),
                ..Default::default()
            })
            .id();
        world
            .get_entity_mut(hidden)
            .expect("alive")
            .insert(crate::visibility::EditorHidden);

        let hits = pick_entities(&world, Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0), 0.4);
        assert_eq!(hits.len(), 1, "only the visible entity is pickable");
        assert_eq!(hits[0].entity, visible);
    }

    /// Spec 29 § 5: an injected SpaceMouse state moves the editor camera by exactly
    /// `nav::step`'s delta (the test every Regia Engine app has, pinning "same across the board").
    #[test]
    fn spacemouse_injection_moves_the_camera_by_nav_step() {
        use regia_app::prelude::App;
        use regia_input::spacemouse::{FakeSpaceMouse, SpaceMouseInjection, SpaceMousePlugin};

        let mut app = App::new();
        app.add_plugins(SpaceMousePlugin::with_backend(
            FakeSpaceMouse::default().backend(),
        ));
        let injected = SpaceMouseState {
            translation: [0.6, -0.3, -0.8],
            rotation: [0.5, 0.9, 0.0],
            buttons: 0,
        };
        app.world_mut()
            .resource_mut::<SpaceMouseInjection>()
            .inject(injected, 1);
        app.update();
        assert_eq!(*app.world().resource::<SpaceMouseState>(), injected);

        let before = EditorCamera::default();
        let mut camera = before.clone();
        let dt = 1.0 / 60.0;
        let delta = spacemouse_step(app.world(), &mut camera, crate::views::ViewKind::Perspective, dt);
        let expected = nav::step(
            &injected,
            dt,
            NavContext::orbit(before.distance).with_pitch(before.pitch),
        );
        assert_eq!(delta, expected, "the editor uses the shared function unchanged");
        assert!(!delta.is_identity());

        let right = before.right();
        let up = right.cross(before.forward()).normalize_or_zero();
        let target = before.target + right * expected.pan.x + up * expected.pan.y;
        assert!(
            (camera.target - target).length() < 1e-5,
            "{:?} vs {target:?}",
            camera.target
        );
        assert!((camera.distance - before.distance * expected.zoom).abs() < 1e-5);
        assert!(camera.distance < before.distance, "cap pushed away zooms in");
        assert!((camera.yaw - (before.yaw + expected.orbit.x)).abs() < 1e-6);
        assert!((camera.pitch - (before.pitch + expected.orbit.y)).abs() < 1e-6);
        assert!(
            camera.yaw > before.yaw && camera.pitch > before.pitch,
            "twist CCW, tilt away"
        );

        // Orthographic panes pan and zoom but never orbit.
        let mut top = before.clone();
        let flat = spacemouse_step(app.world(), &mut top, crate::views::ViewKind::Top, dt);
        assert_eq!(top.yaw, before.yaw);
        assert!((top.target.x - (before.target.x + flat.pan.x)).abs() < 1e-6);
    }

    #[test]
    fn spacemouse_home_button_resets_the_active_pane() {
        use regia_app::prelude::App;
        use regia_input::spacemouse::{FakeSpaceMouse, SpaceMouseInjection, SpaceMousePlugin};

        let mut app = App::new();
        app.add_plugins(SpaceMousePlugin::with_backend(
            FakeSpaceMouse::default().backend(),
        ));
        app.world_mut().insert_resource(ViewportLayout::default());
        app.world_mut().insert_resource(EditorCamera {
            yaw: 2.0,
            distance: 40.0,
            speed: 20.0,
            ..EditorCamera::default()
        });
        let home = SpaceMouseState {
            buttons: 0b10,
            ..SpaceMouseState::default()
        };
        app.world_mut()
            .resource_mut::<SpaceMouseInjection>()
            .inject(home, 1);
        app.update();
        // While typing, the press is consumed without moving the camera (it must not fire later).
        let mut typing = MessageCursor::default();
        assert_eq!(handle_spacemouse_actions(app.world_mut(), &mut typing, false), 0);
        assert_eq!(app.world().resource::<EditorCamera>().yaw, 2.0);
        assert_eq!(
            handle_spacemouse_actions(app.world_mut(), &mut typing, true),
            0,
            "not replayed"
        );

        let mut cursor = MessageCursor::default();
        assert_eq!(handle_spacemouse_actions(app.world_mut(), &mut cursor, true), 1);
        let camera = app.world().resource::<EditorCamera>().clone();
        let default = EditorCamera::default();
        assert_eq!((camera.yaw, camera.distance), (default.yaw, default.distance));
        assert_eq!(camera.speed, 20.0, "fly speed kept");
        assert_eq!(
            handle_spacemouse_actions(app.world_mut(), &mut cursor, true),
            0,
            "handled once"
        );
    }

    /// Spec 29 § 5: with the Object tool on, one gesture (pushes with short pauses, then 0.3 s of
    /// rest) moves the selection, leaves the camera alone and is exactly one undo entry.
    #[test]
    fn spacemouse_object_tool_gesture_is_one_undo_entry() {
        let mut world = World::new();
        let entity = world.spawn(Transform::IDENTITY).id();
        let mut selection = Selection::default();
        selection.select(entity, SelectMode::Replace);
        world.insert_resource(selection);
        world.insert_resource(UndoStack::new());
        world.insert_resource(SpaceMouseNav {
            object_tool: true,
            ..SpaceMouseNav::default()
        });
        let camera = EditorCamera::default();
        let kind = crate::views::ViewKind::Perspective;
        let dt = 1.0 / 60.0;
        let push = SpaceMouseState {
            translation: [0.8, 0.0, 0.0],
            rotation: [0.0, 0.7, 0.0],
            buttons: 0,
        };
        let run = |world: &mut World, state: SpaceMouseState, frames: usize| {
            world.insert_resource(state);
            for _ in 0..frames {
                spacemouse_object_step(world, &camera, kind, dt);
            }
        };
        // Push 0.5 s, rest 0.1 s (shorter than the rest time), push 0.2 s, rest 0.5 s.
        run(&mut world, push, 30);
        run(&mut world, SpaceMouseState::default(), 6);
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            0,
            "still one open gesture"
        );
        run(&mut world, push, 12);
        run(&mut world, SpaceMouseState::default(), 30);
        assert_eq!(
            world.resource::<UndoStack>().undo_depth(),
            1,
            "one gesture, one entry"
        );
        let moved = *world.get::<Transform>(entity).expect("transform");
        let right = camera.right();
        assert!(
            moved.translation.dot(right) > 0.1,
            "moved along view right: {moved:?}"
        );
        let (axis, angle) = moved.rotation.to_axis_angle();
        assert!(
            axis.y > 0.99 && angle > 0.0,
            "twist CCW spins the object CCW from above"
        );

        // A second gesture on the same entity is a second entry, not merged into the first.
        run(&mut world, push, 10);
        run(&mut world, SpaceMouseState::default(), 30);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 2);

        // Undo both: back at the start. The camera was never touched (it is borrowed immutably).
        world.resource_scope(|world, mut undo: Mut<UndoStack>| {
            assert!(undo.undo(world) && undo.undo(world));
        });
        assert_eq!(
            *world.get::<Transform>(entity).expect("transform"),
            Transform::IDENTITY
        );

        // Switching the tool off mid-gesture closes the entry at once.
        run(&mut world, push, 5);
        world.resource_mut::<SpaceMouseNav>().object_tool = false;
        spacemouse_object_step(&mut world, &camera, kind, dt);
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
    }

    #[test]
    fn orbit_camera_positions_eye_behind_target() {
        let camera = EditorCamera::default();
        let eye = camera.eye();
        // Eye above and away from the target.
        assert!(eye.y > camera.target.y, "pitch puts the eye above: {eye:?}");
        let forward = camera.forward();
        assert!(
            forward.dot(camera.target - eye) > 0.99,
            "forward points at the target"
        );
    }

    #[test]
    fn drag_moving_orbit_and_pan() {
        let mut camera = EditorCamera::default();
        let before = camera.eye();
        camera.orbit(10.0, 0.0);
        assert_ne!(camera.eye(), before, "orbit moves the eye");
        let target_before = camera.target;
        camera.pan(5.0, 0.0);
        assert_ne!(camera.target, target_before, "pan moves the target");
    }

    #[test]
    fn fly_moves_target_forward() {
        let mut camera = EditorCamera::default();
        let before = camera.target;
        camera.fly(1.0, 1.0, 0.0, 0.0);
        let expected = before + camera.forward() * camera.speed;
        assert!(
            camera.target.distance(expected) < 1e-4,
            "flying forward moves along the view direction: {:?} vs {expected:?}",
            camera.target
        );
        let distance = camera.target.distance(before);
        assert!(
            (distance - camera.speed).abs() < 0.01,
            "one second at speed 6 moves ~6 m: {distance}"
        );
    }

    /// `up` is a world-Y axis. Scroll dolly ([`EditorCamera::zoom`]) changes distance and
    /// leaves yaw alone. The default fly-up / fly-down chords are E and Q.
    #[test]
    fn fly_up_raises_the_target_and_zoom_dollies_without_orbiting() {
        let mut camera = EditorCamera::default();
        let y = camera.target.y;
        camera.fly(1.0, 0.0, 0.0, 1.0);
        assert!(camera.target.y > y, "a positive up axis raises the target");
        let mut camera = EditorCamera::default();
        let yaw = camera.yaw;
        let distance = camera.distance;
        camera.zoom(0.9);
        assert!((camera.yaw - yaw).abs() < 1e-6, "dolly does not orbit");
        assert!(camera.distance < distance, "dolly shortens the orbit distance");
        let up = crate::command_palette::COMMANDS
            .iter()
            .find(|command| command.id == "camera.fly_up")
            .expect("fly up");
        let down = crate::command_palette::COMMANDS
            .iter()
            .find(|command| command.id == "camera.fly_down")
            .expect("fly down");
        assert_eq!(up.shortcut, "E");
        assert_eq!(down.shortcut, "Q");
    }

    #[test]
    fn frame_fits_selection() {
        let mut camera = EditorCamera {
            distance: 50.0,
            ..Default::default()
        };
        camera.frame(&[Vec3::new(10.0, 0.0, 0.0), Vec3::new(-10.0, 0.0, 0.0)]);
        assert_eq!(camera.target, Vec3::ZERO);
        // Radius 10 around the centre, framed at three radii.
        assert!((camera.distance - 30.0).abs() < 1e-4, "{}", camera.distance);
    }

    #[test]
    fn picking_hits_nearest_sphere_on_ray() {
        let mut world = World::new();
        let propagate = |world: &mut World| regia_scene::prelude::propagate_transforms(world);
        let near = world
            .spawn(regia_scene::prelude::Transform {
                translation: Vec3::new(0.0, 0.0, -5.0),
                ..Default::default()
            })
            .id();
        let far = world
            .spawn(regia_scene::prelude::Transform {
                translation: Vec3::new(0.0, 0.0, -15.0),
                ..Default::default()
            })
            .id();

        // `Transform` requires a `GlobalTransform`; propagate so the world pose is current.
        propagate(&mut world);
        let hits = pick_entities(&world, Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0), 0.4);
        assert_eq!(hits.len(), 2, "both spheres on the ray");
        assert_eq!(hits[0].entity, near, "nearest first");
        assert_eq!(hits[1].entity, far);
        assert!(
            (hits[0].distance - 4.5).abs() < 0.01,
            "sphere of r=0.5 centered on the ray"
        );

        let empty = pick_entities(&world, Vec3::ZERO, Vec3::new(1.0, 0.0, 0.0), 0.4);
        assert!(empty.is_empty(), "ray misses both spheres");
    }

    #[test]
    fn ray_sphere_math() {
        // Straight through the centre.
        assert_eq!(
            ray_sphere(Vec3::ZERO, Vec3::Z, Vec3::new(0.0, 0.0, 5.0), 1.0),
            Some(4.0)
        );
        // Smaller sphere, still centred on the ray.
        assert_eq!(
            ray_sphere(Vec3::ZERO, Vec3::Z, Vec3::new(0.0, 0.0, 5.0), 0.5),
            Some(4.5)
        );
        // Off the axis: misses when the offset exceeds the radius.
        assert_eq!(
            ray_sphere(Vec3::ZERO, Vec3::Z, Vec3::new(1.0, 0.0, 5.0), 0.5),
            None,
            "offset larger than the radius"
        );
        // Grazes the surface (offset == radius): a single tangential hit.
        let tangent = ray_sphere(Vec3::ZERO, Vec3::Z, Vec3::new(0.5, 0.0, 5.0), 0.5);
        assert!(tangent.is_some(), "tangent rays hit the surface");
        // From z=5 toward -Z: straight through the origin sphere.
        assert_eq!(
            ray_sphere(
                Vec3::new(0.0, 0.0, 5.0),
                Vec3::new(0.0, 0.0, -1.0),
                Vec3::ZERO,
                1.0
            ),
            Some(4.0),
            "through the center"
        );
        // Origin inside the sphere returns the far intersection.
        assert_eq!(ray_sphere(Vec3::ZERO, Vec3::Z, Vec3::ZERO, 2.0), Some(2.0));
    }

    #[test]
    fn a_hidpi_pane_renders_at_physical_pixels() {
        // X04 (Mac Retina, 2 pixels per point): a 421x282-point pane covers 842x564 pixels, and
        // rendering it at 421x282 showed the viewport upscaled 2x.
        let points = egui::vec2(421.0, 282.0);
        assert_eq!(pane_render_size(points, 2.0), Vec2::new(842.0, 564.0));
        assert_eq!(pane_render_size(points, 1.0), Vec2::new(421.0, 282.0));
        // Whole pixels: the request is in u32 and `to_pixels` must map onto it exactly.
        assert_eq!(pane_render_size(points, 1.5), Vec2::new(632.0, 423.0));
        // The texture limit still holds in pixels, aspect kept: 1600x800 points at 2x would be
        // 3200x1600 pixels.
        assert_eq!(
            pane_render_size(egui::vec2(1600.0, 800.0), 2.0),
            Vec2::new(PANE_MAX_SIDE, PANE_MAX_SIDE / 2.0)
        );
        // A scale that is not a usable number renders at one pixel per point.
        for bad in [0.0, -2.0, f32::NAN, f32::INFINITY] {
            assert_eq!(pane_render_size(points, bad), Vec2::new(421.0, 282.0), "{bad}");
        }
        // Tiny panes keep the 16-pixel floor.
        assert_eq!(pane_render_size(egui::vec2(4.0, 4.0), 2.0), Vec2::new(16.0, 16.0));
    }

    #[test]
    fn screen_projection_round_trips() {
        let camera = EditorCamera::default();
        let size = Vec2::new(800.0, 600.0);
        let world_point = camera.target;
        let screen = project_to_screen(&camera, world_point, size).expect("in front");
        let (origin, direction) = pick_ray(&camera, Vec2::new(screen.x, screen.y), size);
        let hit = ray_sphere(origin, direction, world_point, 0.1);
        assert!(hit.is_some(), "the pixel through the target hits the target");
        // The near intersection of a 0.1 m sphere sits just in front of the target.
        assert!((hit.unwrap_or(0.0) - (camera.distance - 0.1)).abs() < 0.05);
    }
    #[test]
    fn cpu_pick_order_is_total_for_nan_and_ties() {
        let mut world = World::new();
        let a = world
            .spawn(Transform::from_translation(Vec3::new(0.0, 0.0, -5.0)))
            .id();
        let b = world
            .spawn(Transform::from_translation(Vec3::new(0.0, 0.0, -5.0)))
            .id();
        let far = world
            .spawn(Transform::from_translation(Vec3::new(0.0, 0.0, -9.0)))
            .id();
        world
            .entity_mut(a)
            .insert(regia_scene::prelude::EntityGuid::new_random());
        // `Transform` requires `GlobalTransform`, which stays at the origin until propagated.
        regia_scene::prelude::propagate_transforms(&mut world);
        let hits = pick_entities(&world, Vec3::ZERO, Vec3::NEG_Z, 0.5);
        let ids: Vec<Entity> = hits.iter().map(|h| h.entity).collect();
        // The tie at 5 m breaks on `Entity`'s own order, not spawn order in bevy_ecs 0.20 (ADR-1389).
        assert_eq!(
            ids,
            [a.min(b), a.max(b), far],
            "ties break on entity id, then distance"
        );
        // A NaN position never panics and never outranks a finite hit.
        world.spawn(Transform::from_translation(Vec3::new(f32::NAN, 0.0, 0.0)));
        regia_scene::prelude::propagate_transforms(&mut world);
        let hits = pick_entities(&world, Vec3::ZERO, Vec3::NEG_Z, 0.5);
        assert_eq!(hits.first().map(|h| h.entity), Some(a.min(b)));
    }

    /// FU-4 (ADR-7733): Ctrl+0 is camera bookmark 0 only; it leaves the interface zoom alone.
    #[test]
    fn ctrl_0_stores_bookmark_0_and_leaves_the_interface_zoom() {
        // With the editor's preferences applied (UI scale 1.5), Ctrl+0 on a focused pane stores
        // camera bookmark 0 and the interface zoom stays at 1.5.
        let mut world = World::new();
        world.insert_resource(Selection::default());
        let mut prefs = crate::editor_prefs::EditorPreferences::default();
        prefs
            .set("appearance.ui_scale", &crate::editor_prefs::PrefValue::Float(1.5))
            .expect("ui_scale");
        world.insert_resource(prefs);
        let mut state = ViewportPanelState::default();
        let mut instance = ViewportInstance::new(0);
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 400.0));
        let mut frame = |events: Vec<egui::Event>, world: &mut World| {
            crate::editor_prefs::apply_prefs(Some(&ctx), world);
            let raw = egui::RawInput {
                events,
                screen_rect: Some(screen),
                ..Default::default()
            };
            let mut stats = ViewportStats::default();
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let rect = ui.available_rect_before_wrap();
                    viewport_pane(
                        ui,
                        world,
                        None,
                        &mut state,
                        &mut instance,
                        &mut stats,
                        PanelKind::Main,
                        0,
                        rect,
                    );
                });
            });
            output.textures_delta.clear();
        };
        let key = |key: egui::Key, modifiers: egui::Modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        frame(Vec::new(), &mut world);
        frame(vec![key(egui::Key::Tab, egui::Modifiers::NONE)], &mut world);
        frame(Vec::new(), &mut world);
        assert!(ctx.memory(|m| m.focused()).is_some(), "Tab focused the pane");
        assert!((ctx.zoom_factor() - 1.5).abs() < 1e-6, "the UI scale is applied");
        // egui 0.36 takes the held modifiers from ModifiersChanged, not from the key event.
        frame(
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                key(egui::Key::Num0, egui::Modifiers::COMMAND),
            ],
            &mut world,
        );
        frame(
            vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)],
            &mut world,
        );
        assert!(
            world
                .get_resource::<crate::views::CameraBookmarks>()
                .is_some_and(|m| m.get(0).is_some()),
            "Ctrl+0 stored bookmark 0"
        );
        assert!(
            (ctx.zoom_factor() - 1.5).abs() < 1e-6,
            "Ctrl+0 left the interface zoom at 1.5, got {}",
            ctx.zoom_factor()
        );
    }

    /// FU-9 (`docs/evidence/editor-a11y-2026-11.md`): each viewport pane is a focusable widget
    /// named "Viewport pane \<n\>"; keyboard focus alone (Tab, no pointer event) makes it the
    /// active pane, so a viewport hotkey (`F`, frame selection) acts on the focused pane's
    /// camera instead of the last-clicked one.
    #[test]
    fn keyboard_focus_activates_a_pane_and_its_hotkey() {
        let key = |key: egui::Key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        // One selected entity 30 m along +X: framing (F) centers a pane's camera on it.
        let mut world = World::new();
        let mut selection = Selection::default();
        let transform = Transform::from_translation(Vec3::new(30.0, 0.0, 0.0));
        selection.select(
            world
                .spawn((transform, GlobalTransform(transform.compute_affine())))
                .id(),
            SelectMode::Replace,
        );
        world.insert_resource(selection);
        let mut state = ViewportPanelState::default();
        let mut instance = ViewportInstance::new(0);
        let perspective_camera = instance.camera.clone();
        let top_distance_before = instance.layout.panes[1].camera.distance;

        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 400.0));
        let mut pane2_slot: Option<egui::Id> = None;
        let frame = |events: Vec<egui::Event>,
                     world: &mut World,
                     state: &mut ViewportPanelState,
                     instance: &mut ViewportInstance,
                     pane2: &mut Option<egui::Id>|
         -> Option<egui::accesskit::TreeUpdate> {
            let raw = egui::RawInput {
                events,
                screen_rect: Some(screen),
                ..Default::default()
            };
            let mut stats = ViewportStats::default();
            let mut taken = None;
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let full = ui.available_rect_before_wrap();
                    let half = full.size() * 0.5;
                    let left = egui::Rect::from_min_size(full.left_top(), half - egui::vec2(2.0, 2.0));
                    let right = egui::Rect::from_min_size(
                        full.left_top() + egui::vec2(half.x, 0.0),
                        half - egui::vec2(2.0, 2.0),
                    );
                    viewport_pane(
                        ui,
                        world,
                        None,
                        state,
                        instance,
                        &mut stats,
                        PanelKind::Main,
                        0,
                        left,
                    );
                    viewport_pane(
                        ui,
                        world,
                        None,
                        state,
                        instance,
                        &mut stats,
                        PanelKind::Main,
                        1,
                        right,
                    );
                    *pane2 = Some(ui.id().with(("viewport-pane", 1)));
                });
            });
            output.textures_delta.clear();
            std::mem::swap(&mut taken, &mut output.platform_output.accesskit_update);
            taken
        };

        // Settle one frame so both panes are registered, then Tab (keyboard only) until pane 2
        // has focus. No pointer event is ever sent.
        let update = frame(Vec::new(), &mut world, &mut state, &mut instance, &mut pane2_slot);
        let pane2_id = pane2_slot.expect("the pane id");
        let mut named = false;
        if let Some(update) = update.as_ref() {
            named = update.nodes.iter().any(|(_, node)| {
                node.role() == egui::accesskit::Role::Pane && node.label() == Some("Viewport pane 2")
            });
        }
        assert!(named, "the pane carries its spoken name in the AccessKit tree");
        let mut reached = false;
        for _ in 0..8 {
            frame(
                vec![key(egui::Key::Tab)],
                &mut world,
                &mut state,
                &mut instance,
                &mut pane2_slot,
            );
            if ctx.memory(|m| m.focused()) == Some(pane2_id) {
                reached = true;
                break;
            }
        }
        assert!(
            reached,
            "Tab reaches pane 2; focus is {:?}",
            ctx.memory(|m| m.focused())
        );
        // Focus alone makes pane 2 the active pane (the click path stays: hovered() or clicked()).
        assert_eq!(instance.layout.active, 1, "pane 2 is active after focus");

        // F frames the selection on the active pane: pane 2's own camera moves, the perspective
        // pane's camera does not.
        frame(
            vec![key(egui::Key::F)],
            &mut world,
            &mut state,
            &mut instance,
            &mut pane2_slot,
        );
        // Focus eases over `EASE_SECONDS` since ADR-7204 (integ-v06b): let the move finish.
        for _ in 0..40 {
            frame(Vec::new(), &mut world, &mut state, &mut instance, &mut pane2_slot);
        }
        let framed = instance.layout.panes[1].camera.clone();
        assert!(
            (framed.target - Vec3::new(30.0, 0.0, 0.0)).length() < 1e-3,
            "pane 2 looks at the entity: {:?}",
            framed.target
        );
        assert_ne!(
            framed.distance, top_distance_before,
            "pane 2's camera distance fit the entity"
        );
        assert_eq!(
            instance.camera, perspective_camera,
            "the perspective pane's camera is untouched"
        );
    }

    /// Two main-viewport panes side by side (pane 1 left, perspective; pane 2 right, top)
    /// drawn through `viewport_pane` with no render device, and optionally a text field above
    /// them (first in the Tab order). For the keyboard-focus tests (ADR-8870).
    struct FocusRig {
        ctx: egui::Context,
        world: World,
        state: ViewportPanelState,
        instance: ViewportInstance,
        /// The text field's contents, when the rig draws one.
        text: Option<String>,
        /// The ids of pane 1 and pane 2 and the text field, as of the last frame.
        pane_ids: [Option<egui::Id>; 2],
        text_id: egui::Id,
        /// The panes' rectangles, as of the last frame.
        rects: [egui::Rect; 2],
    }

    impl FocusRig {
        fn new(mut world: World, text_field: bool) -> Self {
            // `viewport_pane` reads the selection every frame (the editor always has one).
            if !world.contains_resource::<Selection>() {
                world.insert_resource(Selection::default());
            }
            let ctx = egui::Context::default();
            ctx.enable_accesskit();
            let mut rig = Self {
                ctx,
                world,
                state: ViewportPanelState::default(),
                instance: ViewportInstance::new(0),
                text: text_field.then(String::new),
                pane_ids: [None; 2],
                text_id: egui::Id::new("focus-rig-text"),
                rects: [egui::Rect::NOTHING; 2],
            };
            // Settle one frame so every widget is registered for Tab.
            rig.frame(Vec::new());
            rig
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let Self {
                ctx,
                world,
                state,
                instance,
                text,
                pane_ids,
                text_id,
                rects,
            } = self;
            let raw = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 440.0),
                )),
                ..Default::default()
            };
            let mut stats = ViewportStats::default();
            let mut output = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    if let Some(text) = text.as_mut() {
                        ui.add(egui::TextEdit::singleline(text).id(*text_id));
                    }
                    let full = ui.available_rect_before_wrap();
                    let half = egui::vec2(full.width() * 0.5, full.height()) - egui::vec2(2.0, 2.0);
                    rects[0] = egui::Rect::from_min_size(full.left_top(), half);
                    rects[1] = egui::Rect::from_min_size(
                        full.left_top() + egui::vec2(full.width() * 0.5, 0.0),
                        half,
                    );
                    for (index, rect) in rects.iter().copied().enumerate() {
                        viewport_pane(
                            ui,
                            world,
                            None,
                            state,
                            instance,
                            &mut stats,
                            PanelKind::Main,
                            index,
                            rect,
                        );
                        pane_ids[index] = Some(ui.id().with(("viewport-pane", index)));
                    }
                });
            });
            output.textures_delta.clear();
        }

        /// Presses Tab (keyboard only, no pointer event) until `id` has focus.
        fn tab_to(&mut self, id: egui::Id) {
            for _ in 0..8 {
                if self.ctx.memory(|m| m.focused()) == Some(id) {
                    return;
                }
                self.frame(vec![focus_key(egui::Key::Tab)]);
            }
            assert_eq!(self.ctx.memory(|m| m.focused()), Some(id), "Tab reaches {id:?}");
        }

        fn pane_id(&self, index: usize) -> egui::Id {
            self.pane_ids[index].unwrap_or_else(|| panic!("pane {index} was drawn"))
        }

        /// A key typed with keyboard focus wherever it is: the key press and, for a printable
        /// key, its text (what a text field reads).
        fn type_key(&mut self, key: egui::Key, text: Option<&str>) {
            let mut events = vec![focus_key(key)];
            if let Some(text) = text {
                events.push(egui::Event::Text(text.to_owned()));
            }
            self.frame(events);
        }

        /// A real primary click at `pos`: move there, press, release.
        fn click(&mut self, pos: egui::Pos2) {
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(pos)]);
            self.frame(vec![button(true)]);
            self.frame(vec![button(false)]);
        }

        fn selected(&self) -> Vec<Entity> {
            self.world.resource::<Selection>().entities().to_vec()
        }

        /// A real pointer drag with `button` from `from` to `to`: hover still for a frame (so
        /// the gizmo reads the hover, as in a real session), press, move
        /// in `steps` frames, release. `mid` runs after the last move, before the release.
        fn drag(
            &mut self,
            button: egui::PointerButton,
            from: egui::Pos2,
            to: egui::Pos2,
            mid: impl FnOnce(&Self),
        ) {
            let press = |pos, pressed| egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(from)]);
            self.frame(Vec::new());
            self.frame(vec![press(from, true)]);
            let steps = 6;
            for step in 1..=steps {
                let t = step as f32 / steps as f32;
                self.frame(vec![egui::Event::PointerMoved(from.lerp(to, t))]);
            }
            mid(self);
            self.frame(vec![press(to, false)]);
            self.frame(Vec::new());
        }
    }

    /// `true` while the transform gizmo is drawn for the selection (pane 0's gizmo has a target).
    fn gizmo_shown(rig: &FocusRig) -> bool {
        rig.world
            .resource::<Selection>()
            .primary()
            .is_some_and(|primary| rig.world.get::<Transform>(primary).is_some())
    }

    /// ADR-8870 follow-up (lane w9-viewport-flags-nav, ADR-7593): with the transform gizmo
    /// shown, transform-gizmo-egui's `interact` put a 1x1 click-and-drag widget under the pointer
    /// every frame and egui gave the press to it (the viewport no longer calls it, ADR-7674). A
    /// box select that starts away from the gizmo's
    /// handles must still run: it ends with the box over empty ground, which (Replace mode)
    /// clears the selection. The control is the same drag with nothing selected (no gizmo).
    #[test]
    fn a_box_select_that_starts_off_the_gizmo_handles_still_selects() {
        let (world, selected, _) = picking_world();
        let mut rig = FocusRig::new(world, false);
        assert!(gizmo_shown(&rig), "the selection shows the gizmo");
        let from = rig.rects[0].left_top() + egui::vec2(12.0, 12.0);
        let to = from + egui::vec2(60.0, 40.0);
        let mut banding = false;
        rig.drag(egui::PointerButton::Primary, from, to, |rig| {
            assert!(
                !rig.state.gizmos[0].is_focused(),
                "no gizmo handle under the drag"
            );
            banding = rig.state.drag.is_some_and(|(pane, _)| pane == 0);
        });
        assert!(banding, "the box select started with the gizmo shown");
        assert!(rig.state.drag.is_none(), "the release ended it");
        assert!(
            !rig.selected().contains(&selected),
            "a box over empty ground replaced the selection: {:?}",
            rig.selected()
        );
    }

    /// ADR-8870 follow-up (ADR-7593, ADR-7674): a right-button orbit and a middle-button pan
    /// that start under the pointer with the gizmo shown move the camera, as they do with nothing
    /// selected.
    #[test]
    fn a_camera_drag_over_the_gizmo_cursor_still_moves_the_camera() {
        for button in [egui::PointerButton::Secondary, egui::PointerButton::Middle] {
            for with_gizmo in [false, true] {
                let (mut world, _, _) = picking_world();
                if !with_gizmo {
                    world.resource_mut::<Selection>().clear();
                }
                let mut rig = FocusRig::new(world, false);
                assert_eq!(gizmo_shown(&rig), with_gizmo);
                let before = rig.instance.camera.clone();
                let from = rig.rects[0].center() + egui::vec2(-40.0, 30.0);
                rig.drag(button, from, from + egui::vec2(50.0, 20.0), |_| {});
                assert_ne!(
                    rig.instance.camera, before,
                    "{button:?} drag moved the camera (gizmo shown: {with_gizmo})"
                );
            }
        }
    }

    /// ADR-8870 follow-up (ADR-7593): the wheel zooms the pane the pointer rests on while the
    /// gizmo is shown (no gizmo widget covers the pane, ADR-7674).
    #[test]
    fn the_wheel_zooms_with_the_gizmo_shown() {
        let (world, _, _) = picking_world();
        let mut rig = FocusRig::new(world, false);
        assert!(gizmo_shown(&rig));
        let at = rig.rects[0].center() + egui::vec2(-40.0, 30.0);
        rig.frame(vec![egui::Event::PointerMoved(at)]);
        rig.frame(Vec::new());
        let before = rig.instance.camera.distance;
        rig.frame(vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, 3.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        }]);
        for _ in 0..20 {
            rig.frame(Vec::new());
        }
        assert!(
            rig.instance.camera.distance < before,
            "the wheel zoomed in: {before} -> {}",
            rig.instance.camera.distance
        );
    }

    fn focus_key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// A world with `selected` (30 m along +X, outside every pane's center) selected and
    /// `target` sitting on the perspective camera's orbit target, so the perspective pane's
    /// center ray hits it.
    fn picking_world() -> (World, Entity, Entity) {
        let mut world = World::new();
        let place = |world: &mut World, at: Vec3| {
            let transform = Transform::from_translation(at);
            world
                .spawn((transform, GlobalTransform(transform.compute_affine())))
                .id()
        };
        let selected = place(&mut world, Vec3::new(30.0, 0.0, 0.0));
        let target = place(&mut world, EditorCamera::default().target);
        // The CPU pick casts meshes (ADR-7204, integ-v06b): the target is a 1 m cube.
        let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
        world.entity_mut(target).insert(regia_render::MeshRenderer::new(
            std::sync::Arc::new(regia_render::CpuMesh { vertices, indices }),
            std::sync::Arc::new(regia_render::CpuMaterial::default()),
        ));
        let mut selection = Selection::default();
        selection.select(selected, SelectMode::Replace);
        world.insert_resource(selection);
        (world, selected, target)
    }

    /// Review finding 1 of lane w6-a11y-labels-z (ADR-8870): with a pane focused, egui turns
    /// Enter and Space into a fake primary click; they must not run click picking (which would
    /// replace the selection with whatever is under the pane center). A real pointer click on an
    /// entity still picks it.
    #[test]
    fn enter_or_space_on_a_focused_pane_keeps_the_selection() {
        let (world, selected, target) = picking_world();
        let mut rig = FocusRig::new(world, false);
        let pane = rig.pane_id(0);
        rig.tab_to(pane);
        assert_eq!(rig.instance.layout.active, 0, "the focused pane is active");
        for key in [egui::Key::Enter, egui::Key::Space] {
            rig.type_key(key, None);
            assert_eq!(
                rig.ctx.memory(|m| m.focused()),
                Some(pane),
                "{key:?} keeps the focus"
            );
            assert_eq!(
                rig.selected(),
                [selected],
                "{key:?} on the focused pane is not a pick"
            );
        }
        // A real click on the entity at the pane center picks it.
        rig.click(rig.rects[0].center());
        assert_eq!(rig.selected(), [target], "a pointer click still picks");
    }

    /// Review finding 4 of lane w6-a11y-labels-z (ADR-8870): the 2D-mode keys reach a pane that
    /// holds keyboard focus (Tab, no pointer); with a text field focused the key types into it.
    #[test]
    fn a_focused_pane_takes_the_2d_mode_key_and_a_text_field_keeps_it() {
        let active = |rig: &FocusRig| {
            rig.world
                .get_resource::<crate::mode2d::Mode2d>()
                .is_some_and(|m| m.active)
        };
        let mut rig = FocusRig::new(World::new(), true);
        let pane = rig.pane_id(0);
        rig.tab_to(pane);
        rig.type_key(egui::Key::B, Some("b"));
        assert!(active(&rig), "B turned the 2D mode on from the focused pane");
        assert_eq!(rig.text.as_deref(), Some(""), "the text field had no focus");

        let mut rig = FocusRig::new(World::new(), true);
        let text = rig.text_id;
        rig.tab_to(text);
        rig.type_key(egui::Key::B, Some("b"));
        assert!(
            !active(&rig),
            "B typed into the text field does not toggle the 2D mode"
        );
        assert_eq!(rig.text.as_deref(), Some("b"), "the text field got the key");
    }

    /// Review finding 4 of lane w6-a11y-labels-z (ADR-8870): the terrain brush keys reach a pane
    /// that holds keyboard focus; with a text field focused the key types into it.
    #[test]
    fn a_focused_pane_takes_the_terrain_brush_key_and_a_text_field_keeps_it() {
        let tool_world = || {
            // `TerrainTool` has private fields, so no struct-update literal here.
            let mut tool = crate::terrain_tool::TerrainTool::default();
            tool.active = true;
            let mut world = World::new();
            world.insert_resource(tool);
            world
        };
        let reach = |rig: &FocusRig| {
            rig.world
                .resource::<crate::terrain_tool::TerrainTool>()
                .brush
                .shape
                .reach()
        };
        let mut rig = FocusRig::new(tool_world(), true);
        let before = reach(&rig);
        let pane = rig.pane_id(0);
        rig.tab_to(pane);
        rig.type_key(egui::Key::CloseBracket, Some("]"));
        assert!(
            (reach(&rig) - before * 1.15).abs() < 1e-4,
            "] grew the brush from the focused pane: {before} -> {}",
            reach(&rig)
        );
        assert_eq!(rig.text.as_deref(), Some(""), "the text field had no focus");

        let mut rig = FocusRig::new(tool_world(), true);
        let text = rig.text_id;
        rig.tab_to(text);
        rig.type_key(egui::Key::CloseBracket, Some("]"));
        assert_eq!(
            reach(&rig),
            before,
            "] typed into the text field leaves the brush alone"
        );
        assert_eq!(rig.text.as_deref(), Some("]"), "the text field got the key");
    }

    /// Review finding 3 of lane w6-a11y-labels-z (documented, no behavior change; ADR-8870):
    /// while a pane holds keyboard focus its keys stay with it even with the pointer over
    /// another pane, and `layout.active` ends the frame on the later-drawn of the two.
    #[test]
    fn a_focused_pane_keeps_its_keys_while_another_pane_is_hovered() {
        let (world, _, _) = picking_world();
        let mut rig = FocusRig::new(world, false);
        let pane = rig.pane_id(0);
        rig.tab_to(pane);
        let top_before = rig.instance.layout.panes[1].camera.clone();
        rig.frame(vec![egui::Event::PointerMoved(rig.rects[1].center())]);
        assert_eq!(
            rig.ctx.memory(|m| m.focused()),
            Some(pane),
            "hover keeps the focus"
        );
        assert_eq!(
            rig.instance.layout.active, 1,
            "the later-drawn (hovered) pane is marked active"
        );
        rig.type_key(egui::Key::F, None);
        // Focus eases over `EASE_SECONDS` since ADR-7204 (integ-v06b): let the move finish.
        for _ in 0..40 {
            rig.frame(Vec::new());
        }
        assert!(
            (rig.instance.camera.target - Vec3::new(30.0, 0.0, 0.0)).length() < 1e-3,
            "F framed the focused pane 1: {:?}",
            rig.instance.camera.target
        );
        assert_eq!(
            rig.instance.layout.panes[1].camera, top_before,
            "the hovered pane 2 did not take the key"
        );
    }

    /// A primary click at `pos` with `modifiers` held (egui 0.36 reads held modifiers from
    /// `ModifiersChanged`).
    fn click_with(rig: &mut FocusRig, pos: egui::Pos2, modifiers: egui::Modifiers) {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers,
        };
        rig.frame(vec![
            egui::Event::ModifiersChanged(modifiers),
            egui::Event::PointerMoved(pos),
        ]);
        rig.frame(vec![button(true)]);
        rig.frame(vec![button(false)]);
        rig.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
    }

    /// A 1 m cube with a transform at `at` (the CPU pick casts meshes).
    fn place_cube(world: &mut World, at: Vec3) -> Entity {
        let transform = Transform::from_translation(at);
        let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
        world
            .spawn((
                transform,
                GlobalTransform(transform.compute_affine()),
                regia_render::MeshRenderer::new(
                    std::sync::Arc::new(regia_render::CpuMesh { vertices, indices }),
                    std::sync::Arc::new(regia_render::CpuMaterial::default()),
                ),
            ))
            .id()
    }

    /// P-073 (ADR-7769): Alt+click lists every entity under the cursor nearest first and leaves
    /// the selection alone; Down then Enter (the keyboard path) selects the second row.
    #[test]
    fn alt_click_lists_the_stack_and_the_keyboard_picks_a_row() {
        let (mut world, selected, target) = picking_world();
        let camera = EditorCamera::default();
        let behind = camera.target + (camera.target - camera.eye()).normalize() * 3.0;
        let far = place_cube(&mut world, behind);
        let mut rig = FocusRig::new(world, false);
        let center = rig.rects[0].center();
        click_with(&mut rig, center, egui::Modifiers::ALT);
        let popup = rig
            .world
            .get_resource::<crate::pick_stack::PickStackPopup>()
            .cloned()
            .expect("Alt+click opened the list");
        let rows: Vec<Entity> = popup.entries.iter().map(|e| e.entity).collect();
        assert_eq!(&rows[..2], &[target, far], "nearest first: {rows:?}");
        assert_eq!(
            rig.selected(),
            vec![selected],
            "the list does not change the selection"
        );
        // Escape closes without picking.
        rig.type_key(egui::Key::Escape, None);
        assert!(!rig.world.contains_resource::<crate::pick_stack::PickStackPopup>());
        assert_eq!(rig.selected(), vec![selected]);
        // Open it again: Down, Enter picks the second row (the far cube's gizmo then sits under
        // the cursor, so this is the last click).
        click_with(&mut rig, center, egui::Modifiers::ALT);
        assert!(rig.world.contains_resource::<crate::pick_stack::PickStackPopup>());
        rig.type_key(egui::Key::ArrowDown, None);
        rig.type_key(egui::Key::Enter, None);
        assert!(!rig.world.contains_resource::<crate::pick_stack::PickStackPopup>());
        assert_eq!(rig.selected(), vec![far], "Down, Enter picked the second row");
    }

    /// P-074 (ADR-7769): a click on a child of a selection root selects the root; Ctrl+click
    /// still picks (toggles) the child; Ctrl+Shift+click subtracts it again.
    #[test]
    fn a_click_on_a_child_selects_its_selection_root_and_ctrl_picks_the_child() {
        let (mut world, _, target) = picking_world();
        let root = world
            // Away from the center ray, so its gizmo is not under the next clicks.
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 40.0, 0.0)),
                crate::selection::EditorSelectionRoot,
            ))
            .id();
        world.entity_mut(target).insert(ChildOf(root));
        let mut rig = FocusRig::new(world, false);
        let center = rig.rects[0].center();
        click_with(&mut rig, center, egui::Modifiers::NONE);
        assert_eq!(rig.selected(), vec![root], "the marked parent took the click");
        click_with(&mut rig, center, egui::Modifiers::CTRL);
        assert_eq!(
            rig.selected(),
            vec![root, target],
            "Ctrl+click toggled the child in"
        );
        click_with(&mut rig, center, egui::Modifiers::CTRL | egui::Modifiers::SHIFT);
        assert_eq!(
            rig.selected(),
            vec![root],
            "Ctrl+Shift+click subtracted the child"
        );
    }

    /// P-069 (ADR-7769): M on the focused pane enters measure mode; a drag places two points
    /// (and starts no box select), a click the third; Escape leaves.
    #[test]
    fn m_enters_measure_mode_a_drag_and_a_click_measure_and_escape_leaves() {
        let (world, _, _) = picking_world();
        let mut rig = FocusRig::new(world, false);
        let pane = rig.pane_id(0);
        rig.tab_to(pane);
        rig.type_key(egui::Key::M, Some("m"));
        assert!(
            crate::measure_tool::MeasureTool::is_active(&rig.world),
            "M entered measure mode"
        );
        let from = rig.rects[0].left_top() + egui::vec2(20.0, 150.0);
        let to = from + egui::vec2(80.0, 0.0);
        let mut banding = false;
        rig.drag(egui::PointerButton::Primary, from, to, |rig| {
            banding = rig.state.drag.is_some();
        });
        assert!(!banding, "measuring does not box-select");
        let tool = rig.world.resource::<crate::measure_tool::MeasureTool>().clone();
        assert_eq!(tool.points.len(), 2, "{tool:?}");
        assert!(tool.measurement().is_some_and(|m| m.distance > 0.0), "{tool:?}");
        click_with(&mut rig, to + egui::vec2(0.0, -60.0), egui::Modifiers::NONE);
        let tool = rig.world.resource::<crate::measure_tool::MeasureTool>().clone();
        assert_eq!(
            tool.points.len(),
            3,
            "the click added the angle's point: {tool:?}"
        );
        assert!(tool.measurement().and_then(|m| m.angle_deg).is_some());
        rig.tab_to(pane);
        rig.type_key(egui::Key::Escape, None);
        assert!(
            !crate::measure_tool::MeasureTool::is_active(&rig.world),
            "Escape left"
        );
    }

    /// P-071 (ADR-7769): hovering a pane asks for no extra id buffer (the pre-highlight reads
    /// the one the pane already has): the id requests over a run of hover frames equal those of
    /// the same frames with the pointer off the panes.
    #[test]
    fn the_hover_highlight_adds_no_id_buffer_request() {
        let (world, _, _) = picking_world();
        let mut rig = FocusRig::new(world, false);
        let run = |rig: &mut FocusRig, at: egui::Pos2| {
            rig.frame(vec![egui::Event::PointerMoved(at)]);
            let before = pane_id_requests();
            for _ in 0..8 {
                rig.frame(Vec::new());
            }
            pane_id_requests() - before
        };
        let off = run(&mut rig, egui::pos2(-50.0, -50.0));
        let center = rig.rects[0].center();
        let over = run(&mut rig, center);
        assert_eq!(
            over, off,
            "hover frames asked for {over} id buffers, idle ones {off}"
        );
    }

    /// Review sprint-v06-rev-late-k #18: Ctrl+F (a panel's search) never frames the selection;
    /// plain F still does.
    #[test]
    fn ctrl_f_never_frames_the_selection_and_plain_f_does() {
        let (world, _, _) = picking_world();
        let mut rig = FocusRig::new(world, false);
        let pane = rig.pane_id(0);
        rig.tab_to(pane);
        let before = rig.instance.camera.target;
        // egui 0.36 takes the held modifiers from ModifiersChanged, not from the key event.
        rig.frame(vec![
            egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
            egui::Event::Key {
                key: egui::Key::F,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            },
        ]);
        rig.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
        for _ in 0..40 {
            rig.frame(Vec::new());
        }
        assert!(
            (rig.instance.camera.target - before).length() < 1e-6,
            "Ctrl+F left the camera alone: {:?}",
            rig.instance.camera.target
        );
        rig.type_key(egui::Key::F, None);
        for _ in 0..40 {
            rig.frame(Vec::new());
        }
        assert!(
            (rig.instance.camera.target - Vec3::new(30.0, 0.0, 0.0)).length() < 1e-3,
            "plain F framed the selection: {:?}",
            rig.instance.camera.target
        );
    }
}
