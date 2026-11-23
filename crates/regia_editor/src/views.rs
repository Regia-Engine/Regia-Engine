// SPDX-License-Identifier: MPL-2.0
//! Viewport views: perspective and orthographic (top/front/side) cameras, the single/quad
//! layout, the render-quality preset, show flags and camera bookmarks
//! (`docs/specs/19-editor.md` §§ 2–3).
//!
//! Every view renders through the runtime frame renderer, which has a perspective projection
//! only. Orthographic views are drawn as a **telephoto**: the eye sits [`ORTHO_STANDOFF`]
//! (50 km) behind the target with the field of view that shows the ortho zoom's half-height.
//! Parallax is depth ÷ standoff: 0.2 % of the half-height for geometry within ±100 m of the
//! target plane (under a pixel on a 500 px pane), 4 % at the edge of the ±[`ORTHO_DEPTH`]
//! slab. The overlays and picking use the same matrix, so grid, gizmo and clicks land exactly
//! on the picture. At that standoff the sun cascades (≤ 4 km, ADR-061) do not reach the
//! geometry, so orthographic panes are unshadowed; they also skip fog and atmosphere.

use regia_core::math::{Mat4, Vec2, Vec3, Vec4};
use regia_ecs::prelude::*;
use regia_render::{FrameQuality, RenderProfilePreset, RenderSettings};

use crate::viewport::EditorCamera;

/// Which way a viewport looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum ViewKind {
    /// Free perspective camera (orbit/fly).
    #[default]
    Perspective,
    /// Orthographic, looking down −Y (screen up = −Z).
    Top,
    /// Orthographic, looking down −Z from +Z.
    Front,
    /// Orthographic, looking down −X from +X.
    Side,
    /// Orthographic, looking up +Y from below (screen up = +Z; ADR-7755).
    Bottom,
    /// Orthographic, looking down +Z from −Z.
    Back,
    /// Orthographic, looking down +X from −X.
    OppositeSide,
}

impl ViewKind {
    /// The quad layout's kinds, in pane order (top-left, top-right, bottom-left, bottom-right).
    pub const ALL: [Self; 4] = [Self::Perspective, Self::Top, Self::Front, Self::Side];

    /// Every kind: perspective and the six orthographic views (ADR-7755).
    pub const EVERY: [Self; 7] = [
        Self::Perspective,
        Self::Top,
        Self::Front,
        Self::Side,
        Self::Bottom,
        Self::Back,
        Self::OppositeSide,
    ];

    /// Stable name (`perspective`, `top`, `front`, `side`, `bottom`, `back`, `opposite_side`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Perspective => "perspective",
            Self::Top => "top",
            Self::Front => "front",
            Self::Side => "side",
            Self::Bottom => "bottom",
            Self::Back => "back",
            Self::OppositeSide => "opposite_side",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::EVERY.into_iter().find(|kind| kind.name() == name)
    }

    /// The world directions of screen right and screen up in an orthographic view (`None` for
    /// perspective): orthographic pans and the SpaceMouse move along them.
    #[must_use]
    pub fn screen_axes(self) -> Option<(Vec3, Vec3)> {
        match self {
            Self::Perspective => None,
            Self::Top => Some((Vec3::X, Vec3::NEG_Z)),
            Self::Bottom => Some((Vec3::X, Vec3::Z)),
            Self::Front => Some((Vec3::X, Vec3::Y)),
            Self::Back => Some((Vec3::NEG_X, Vec3::Y)),
            Self::Side => Some((Vec3::NEG_Z, Vec3::Y)),
            Self::OppositeSide => Some((Vec3::Z, Vec3::Y)),
        }
    }

    /// Whether the view is orthographic.
    #[must_use]
    pub fn is_ortho(self) -> bool {
        self != Self::Perspective
    }

    /// The fixed yaw/pitch of an orthographic view (`None` for perspective).
    #[must_use]
    pub fn ortho_angles(self) -> Option<(f32, f32)> {
        match self {
            Self::Perspective => None,
            // Not exactly straight down: the look-at basis needs a forward that is not
            // parallel to +Y. 1e-3 rad tilt is far below a pixel at any zoom.
            Self::Top => Some((0.0, std::f32::consts::FRAC_PI_2 - 1.0e-3)),
            Self::Front => Some((0.0, 0.0)),
            Self::Side => Some((std::f32::consts::FRAC_PI_2, 0.0)),
            Self::Bottom => Some((0.0, -(std::f32::consts::FRAC_PI_2 - 1.0e-3))),
            Self::Back => Some((std::f32::consts::PI, 0.0)),
            Self::OppositeSide => Some((-std::f32::consts::FRAC_PI_2, 0.0)),
        }
    }
}

/// Distance from the eye to the target in orthographic (telephoto) views, metres.
pub const ORTHO_STANDOFF: f32 = 50_000.0;
/// Half-depth of the slab around the target an orthographic view shows, metres.
pub const ORTHO_DEPTH: f32 = 2000.0;
/// Far plane of perspective views, metres (levels are kilometres across).
pub const PERSPECTIVE_FAR: f32 = 4000.0;

/// The camera one view renders with: eye, target and a perspective projection. Overlays,
/// picking and the renderer all use this one description.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewCamera {
    /// World-space eye.
    pub eye: Vec3,
    /// World-space look-at point.
    pub target: Vec3,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Near plane, metres.
    pub near: f32,
    /// Far plane, metres.
    pub far: f32,
    /// The view is an orthographic stand-in (telephoto).
    pub ortho: bool,
}

impl ViewCamera {
    /// The render camera of `kind` driven by `camera` (for orthographic kinds `camera.target`
    /// is the view centre and `camera.distance` the zoom: the half-height a perspective
    /// camera at that distance would see).
    #[must_use]
    pub fn of(kind: ViewKind, camera: &EditorCamera) -> Self {
        match kind.ortho_angles() {
            None => Self {
                eye: camera.eye(),
                target: camera.target,
                fov_y: camera.fov_deg.to_radians(),
                // Per-pane clip planes (ADR-7755); a hand-built camera with nonsense planes gets
                // the defaults.
                near: if crate::viewport::check_clip(camera.near, camera.far).is_ok() {
                    camera.near
                } else {
                    EditorCamera::NEAR
                },
                far: if crate::viewport::check_clip(camera.near, camera.far).is_ok() {
                    camera.far
                } else {
                    PERSPECTIVE_FAR
                },
                ortho: false,
            },
            Some((yaw, pitch)) => {
                let mut oriented = camera.clone();
                oriented.yaw = yaw;
                oriented.pitch = pitch;
                let half_height = camera.distance * (camera.fov_deg.to_radians() * 0.5).tan();
                let fov_y = 2.0 * (half_height / ORTHO_STANDOFF).atan();
                let forward = (oriented.target - oriented.eye()).normalize_or_zero();
                Self {
                    eye: camera.target - forward * ORTHO_STANDOFF,
                    target: camera.target,
                    fov_y,
                    near: ORTHO_STANDOFF - ORTHO_DEPTH,
                    far: ORTHO_STANDOFF + ORTHO_DEPTH,
                    ortho: true,
                }
            }
        }
    }

    /// Unit forward direction.
    #[must_use]
    pub fn forward(&self) -> Vec3 {
        (self.target - self.eye).normalize_or_zero()
    }

    /// World → view (right-handed, +Y up).
    #[must_use]
    #[allow(deprecated)] // facade exposes only the deprecated glam spellings (docs/03 § 3)
    pub fn view(&self) -> Mat4 {
        Mat4::look_at_rh(self.eye, self.target, Vec3::Y)
    }

    /// View → clip at `aspect`.
    #[must_use]
    #[allow(deprecated)] // facade exposes only the deprecated glam spellings (docs/03 § 3)
    pub fn projection(&self, aspect: f32) -> Mat4 {
        Mat4::perspective_rh(self.fov_y, aspect.max(0.01), self.near, self.far)
    }

    /// World → clip.
    #[must_use]
    pub fn view_projection(&self, aspect: f32) -> Mat4 {
        self.projection(aspect) * self.view()
    }

    /// World position → viewport pixel (`None` behind the eye).
    #[must_use]
    pub fn project(&self, position: Vec3, size: Vec2) -> Option<Vec2> {
        let clip = self.view_projection(size.x / size.y.max(1.0)) * position.extend(1.0);
        if clip.w <= 0.0 {
            return None;
        }
        let ndc = clip.truncate() / clip.w;
        Some(Vec2::new(
            (ndc.x + 1.0) * 0.5 * size.x,
            (1.0 - ndc.y) * 0.5 * size.y,
        ))
    }

    /// The ray through a viewport pixel: origin on the near plane and unit direction.
    #[must_use]
    pub fn ray(&self, pixel: Vec2, size: Vec2) -> (Vec3, Vec3) {
        let ndc_x = 2.0 * pixel.x / size.x.max(1.0) - 1.0;
        let ndc_y = 1.0 - 2.0 * pixel.y / size.y.max(1.0);
        let inverse = self.view_projection(size.x / size.y.max(1.0)).inverse();
        let unproject = |z: f32| {
            let world = inverse * Vec4::new(ndc_x, ndc_y, z, 1.0);
            world.truncate() / world.w
        };
        let near = unproject(0.0);
        let far = unproject(1.0);
        (near, (far - near).normalize_or_zero())
    }

    /// World units per pixel at the target distance (for zoom-independent overlay sizes).
    #[must_use]
    pub fn units_per_pixel(&self, height_px: f32) -> f32 {
        2.0 * (self.target - self.eye).length() * (self.fov_y * 0.5).tan() / height_px.max(1.0)
    }
}

/// The render-quality preset the viewport draws at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewportQuality {
    /// The cheap stable editing profile ([`FrameQuality::EDITOR`]): native scale, no temporal
    /// jitter, no atmosphere (the editor backdrop sky stands in).
    #[default]
    Editor,
    /// What the game runs: the world's [`RenderSettings`] (desktop high when absent).
    Game,
    /// A named render profile.
    Profile(RenderProfilePreset),
}

impl ViewportQuality {
    /// Every selectable preset, in menu order.
    pub const ALL: [Self; 8] = [
        Self::Editor,
        Self::Game,
        Self::Profile(RenderProfilePreset::MobileLow),
        Self::Profile(RenderProfilePreset::MobileHigh),
        Self::Profile(RenderProfilePreset::DesktopLow),
        Self::Profile(RenderProfilePreset::DesktopHigh),
        Self::Profile(RenderProfilePreset::DesktopUltra),
        Self::Profile(RenderProfilePreset::Deck),
    ];

    /// Stable name (`editor`, `game`, `mobile_low`, …).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Editor => "editor",
            Self::Game => "game",
            Self::Profile(RenderProfilePreset::MobileLow) => "mobile_low",
            Self::Profile(RenderProfilePreset::MobileHigh) => "mobile_high",
            Self::Profile(RenderProfilePreset::DesktopLow) => "desktop_low",
            Self::Profile(RenderProfilePreset::DesktopHigh) => "desktop_high",
            Self::Profile(RenderProfilePreset::DesktopUltra) => "desktop_ultra",
            Self::Profile(RenderProfilePreset::Deck) => "deck",
            // Offline only (ADR-1403): not in the viewport menu.
            Self::Profile(RenderProfilePreset::Cinematic) => "cinematic",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.name() == name)
    }

    /// The render settings this preset draws with (`None` = the cheap editor profile).
    #[must_use]
    pub fn settings(self, world: &World) -> Option<RenderSettings> {
        match self {
            Self::Editor => None,
            Self::Game => Some(
                world
                    .get_resource::<RenderSettings>()
                    .cloned()
                    .unwrap_or_default(),
            ),
            Self::Profile(preset) => Some(RenderSettings::from_profile(preset)),
        }
    }

    /// The frame quality of this preset.
    #[must_use]
    pub fn frame_quality(self, world: &World) -> FrameQuality {
        self.settings(world)
            .map_or(FrameQuality::EDITOR, |settings| settings.quality())
    }
}

/// Which editor helpers draw on top of the picture (`specs/19` § 2 show flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShowFlags {
    /// Reference grid.
    pub grid: bool,
    /// Icons for lights, cameras, decals and audio emitters.
    pub icons: bool,
    /// Bounds of the selection; light ranges, spot cones and camera frustums of selected entities.
    pub bounds: bool,
    /// Selection outline (from the entity-id buffer).
    pub outline: bool,
    /// Lit proxy cubes for entities with no renderable component (so they stay visible and
    /// pickable); hidden in game view.
    pub proxies: bool,
    /// Frame statistics overlay (fps, GPU per pass, draws, hitches).
    pub stats: bool,
    /// A neutral preview sun when the level has no directional light (editor quality only).
    pub preview_light: bool,
    /// Audio gizmos: emitter markers, and for the selection their range spheres, cones and
    /// reverb-zone volumes ([`crate::audio_gizmo`]).
    pub audio: bool,
    /// Every collider's outline ([`regia_agent::seeing_overlays::collider_segments`], ADR-7217);
    /// off by default.
    pub colliders: bool,
    /// Every point light's radius and spot light's cone
    /// ([`regia_agent::seeing_overlays::light_radius_segments`], ADR-7217); off by default.
    pub light_radii: bool,
    /// The baked navigation mesh's polygon outlines and off-mesh links
    /// ([`crate::overlays::navigation_segments`], ADR-7468); off by default.
    pub navigation: bool,
    /// Remaining AI navigation paths; off by default.
    pub paths: bool,
    /// Every post volume's shape and blend band
    /// ([`crate::volume_overlay::post_volume_segments`], ADR-7593); off by default.
    pub post_volumes: bool,
    /// The fog of war debug view: watchers' sight circles, the fog grid's outline, the texture and
    /// the viewer team ([`crate::fog_debug`]); off by default.
    pub fog: bool,
    /// Per-kind icon filters, icon size and opacity of the pane drawn (Show ▸ Icons, ADR-7770);
    /// [`ViewportLayout::pane_show`] fills it from the pane's [`PaneDisplay::icons`].
    pub icon_filter: IconFilters,
    /// The deterministic-index debug draw of play: grid, cell counts, candidate pairs, contacts
    /// and queries (`regia_framework::det_debug::DetDebugDraw`, Show ▸ Lockstep, ADR-8562); off by
    /// default. What is drawn is the play world's switch (`sim.det_debug_draw`).
    pub lockstep: bool,
    /// In-world behavior-tree leaf names (ADR-8463). Game view leaves this as set.
    pub ai_bt: bool,
    /// In-world state-tree state names (ADR-8463). Game view leaves this as set.
    pub ai_state_tree: bool,
    /// In-world GOAP plan step and goal (ADR-8463). Game view leaves this as set.
    pub ai_goap: bool,
    /// In-world squad lines and role letters (ADR-8463). Game view leaves this as set.
    pub ai_squad: bool,
    /// In-world formation slots (ADR-8463). Game view leaves this as set.
    pub ai_formation: bool,
    /// In-world MoveTo targets and remaining paths (ADR-8463). Game view leaves this as set.
    pub ai_move_to: bool,
    /// In-world blackboard watch keys (ADR-8463). Game view leaves this as set.
    pub ai_blackboard: bool,
}

impl Default for ShowFlags {
    fn default() -> Self {
        Self {
            grid: true,
            icons: true,
            bounds: true,
            outline: true,
            proxies: true,
            stats: false,
            preview_light: true,
            audio: true,
            colliders: false,
            light_radii: false,
            navigation: false,
            paths: false,
            post_volumes: false,
            fog: false,
            icon_filter: IconFilters::default(),
            lockstep: false,
            ai_bt: false,
            ai_state_tree: false,
            ai_goap: false,
            ai_squad: false,
            ai_formation: false,
            ai_move_to: false,
            ai_blackboard: false,
        }
    }
}

/// What a pane's picture shows (the View mode menu, Alt+1..8; ADR-7770). Each mode is one of the
/// renderer's debug views ([`Self::debug_view`]); two of them also swap every surface for a plain
/// one ([`Self::surface`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ViewMode {
    /// Shaded with lighting (the default).
    #[default]
    Lit,
    /// Base color only, no lighting.
    Unlit,
    /// Triangle edges over flat dark surfaces (hidden edges stay hidden; picking still works).
    Wireframe,
    /// Triangle edges over the lit picture.
    LitWireframe,
    /// The lighting on a plain mid-gray surface: no textures, no material colors.
    LightingOnly,
    /// Lights per cluster as a heat map.
    LightComplexity,
    /// Overlapping surfaces as a heat map.
    Overdraw,
    /// One of the renderer's buffer views ([`BUFFER_VIEWS`]).
    Buffer(regia_render::DebugViewMode),
}

/// The plain surface a view mode draws every mesh with ([`ViewMode::surface`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlainSurface {
    /// A lit mid-gray rough surface ([`ViewMode::LightingOnly`]).
    Clay,
    /// An unlit dark surface under the wires ([`ViewMode::Wireframe`]).
    Flat,
}

/// The buffer views the View mode menu offers, by the renderer's stable name and the menu label.
pub const BUFFER_VIEWS: [(&str, &str); 18] = [
    ("base_color", "Base color"),
    ("normals", "Normals"),
    ("roughness", "Roughness"),
    ("metallic", "Metallic"),
    ("ambient_occlusion", "Ambient occlusion"),
    ("shadow_cascades", "Shadow cascades"),
    ("lod_color", "LOD coloring"),
    ("motion_vectors", "Motion vectors"),
    ("mip_level", "Mip level"),
    ("material_id", "Material ID"),
    ("uv_checker", "UV checker"),
    ("texel_density", "Texel density"),
    ("lightmap_density", "Lightmap density"),
    ("lightmap_only", "Lightmap only"),
    ("lightmap_invalid", "Lightmap invalid texels"),
    ("image_light", "Image light only"),
    ("image_light_occlusion", "Image light occlusion"),
    ("cloud_shadow_map", "Cloud shadow map"),
];

impl ViewMode {
    /// The modes on Alt+1..Alt+8, in menu order.
    pub const KEYED: [Self; 8] = [
        Self::Lit,
        Self::Unlit,
        Self::Wireframe,
        Self::LitWireframe,
        Self::LightingOnly,
        Self::LightComplexity,
        Self::Overdraw,
        Self::Buffer(regia_render::DebugViewMode::BaseColor),
    ];

    /// The renderer's debug view this mode draws with (the pane request's `debug_view`).
    #[must_use]
    pub fn debug_view(self) -> regia_render::DebugViewMode {
        use regia_render::DebugViewMode as D;
        match self {
            Self::Lit | Self::LightingOnly => D::Lit,
            Self::Unlit => D::Unlit,
            Self::Wireframe | Self::LitWireframe => D::Wireframe,
            Self::LightComplexity => D::LightComplexity,
            Self::Overdraw => D::Overdraw,
            Self::Buffer(mode) => mode,
        }
    }

    /// The plain surface this mode swaps in for every mesh, if any.
    #[must_use]
    pub fn surface(self) -> Option<PlainSurface> {
        match self {
            Self::Wireframe => Some(PlainSurface::Flat),
            Self::LightingOnly => Some(PlainSurface::Clay),
            _ => None,
        }
    }

    /// [`Self::surface`] when a pane request draws this mode's own debug view (`debug_view`): a
    /// request that names another view (an agent capture of the pane) draws no plain surface.
    #[must_use]
    pub fn surface_for(self, debug_view: regia_render::DebugViewMode) -> Option<PlainSurface> {
        self.surface().filter(|_| self.debug_view() == debug_view)
    }

    /// Stable name (`lit`, `unlit`, `wireframe_only`, `lit_wireframe`, `lighting_only`,
    /// `light_complexity`, `overdraw`, or a [`BUFFER_VIEWS`] name).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Lit => "lit",
            Self::Unlit => "unlit",
            Self::Wireframe => "wireframe_only",
            Self::LitWireframe => "lit_wireframe",
            Self::LightingOnly => "lighting_only",
            Self::LightComplexity => "light_complexity",
            Self::Overdraw => "overdraw",
            Self::Buffer(mode) => BUFFER_VIEWS
                .iter()
                .find(|(name, _)| {
                    regia_render::DebugView::mode_from_name(name)
                        .is_some_and(|m| std::mem::discriminant(&m) == std::mem::discriminant(&mode))
                })
                .map_or("buffer", |(name, _)| name),
        }
    }

    /// The menu label (US English).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Lit => "Lit",
            Self::Unlit => "Unlit",
            Self::Wireframe => "Wireframe",
            Self::LitWireframe => "Lit + wireframe",
            Self::LightingOnly => "Lighting only",
            Self::LightComplexity => "Light complexity",
            Self::Overdraw => "Overdraw",
            Self::Buffer(_) => {
                let name = self.name();
                BUFFER_VIEWS
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map_or("Buffer view", |(_, label)| label)
            }
        }
    }

    /// Parses [`Self::name`]; also every renderer debug-view name (`wireframe` is
    /// [`Self::LitWireframe`], the renderer's own wireframe view), with `tile_m`,
    /// `target_px_per_m` and `tolerance` for the parametrized views (`None` = default).
    #[must_use]
    pub fn from_params(
        name: &str,
        tile_m: Option<f32>,
        target_px_per_m: Option<f32>,
        tolerance: Option<f32>,
    ) -> Option<Self> {
        Some(match name {
            "lit" => Self::Lit,
            "unlit" => Self::Unlit,
            "wireframe_only" => Self::Wireframe,
            "lit_wireframe" | "wireframe" => Self::LitWireframe,
            "lighting_only" => Self::LightingOnly,
            "light_complexity" => Self::LightComplexity,
            "overdraw" => Self::Overdraw,
            other => Self::Buffer(regia_render::DebugView::mode_from_params(
                other,
                tile_m,
                target_px_per_m,
                tolerance,
            )?),
        })
    }

    /// [`Self::from_params`] with default parameters.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::from_params(name, None, None, None)
    }

    /// The mode a renderer debug view stands for ([`Self::debug_view`]'s inverse; the renderer's
    /// wireframe is [`Self::LitWireframe`]).
    #[must_use]
    pub fn of_debug_view(mode: regia_render::DebugViewMode) -> Self {
        use regia_render::DebugViewMode as D;
        match mode {
            D::Lit => Self::Lit,
            D::Unlit => Self::Unlit,
            D::Wireframe => Self::LitWireframe,
            D::LightComplexity => Self::LightComplexity,
            D::Overdraw => Self::Overdraw,
            other => Self::Buffer(other),
        }
    }
}

/// Per-kind icon filters and the icons' size and opacity (Show ▸ Icons, ADR-7770). A kind that
/// is off hides its icons and its always-on helpers in the pane; the selection's own helpers
/// (bounds, ranges) stay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconFilters {
    /// Light icons and light radii.
    pub lights: bool,
    /// Camera icons.
    pub cameras: bool,
    /// Audio emitter icons and audio ranges.
    pub audio: bool,
    /// Decal icons.
    pub decals: bool,
    /// Player start flags and arrows.
    pub player_starts: bool,
    /// Match-rules gizmos (ADR-8738): spawn points in their team's color with a priority label,
    /// flag bases, hill and zone boxes, and during play capture progress and spawn-score heat.
    pub match_rules: bool,
    /// Trigger volumes and collider outlines.
    pub colliders: bool,
    /// Reflection capture boxes.
    pub captures: bool,
    /// Post volume shapes.
    pub volumes: bool,
    /// The navigation mesh.
    pub nav: bool,
    /// Icon size, percent of the default (50..=200).
    pub size_percent: u16,
    /// Icon opacity, percent (10..=100).
    pub opacity_percent: u8,
}

impl Default for IconFilters {
    fn default() -> Self {
        Self {
            lights: true,
            cameras: true,
            audio: true,
            decals: true,
            player_starts: true,
            match_rules: true,
            colliders: true,
            captures: true,
            volumes: true,
            nav: true,
            size_percent: 100,
            opacity_percent: 100,
        }
    }
}

impl IconFilters {
    /// Every kind by stable name and menu label.
    pub const KINDS: [(&'static str, &'static str); 10] = [
        ("lights", "Lights"),
        ("cameras", "Cameras"),
        ("audio", "Audio"),
        ("decals", "Decals"),
        ("player_starts", "Player starts"),
        ("match_rules", "Match rules"),
        ("colliders", "Colliders and triggers"),
        ("captures", "Reflection captures"),
        ("volumes", "Volumes"),
        ("nav", "Navigation"),
    ];
    /// Icon size range, percent.
    pub const SIZE_RANGE: std::ops::RangeInclusive<u16> = 50..=200;
    /// Icon opacity range, percent.
    pub const OPACITY_RANGE: std::ops::RangeInclusive<u8> = 10..=100;

    /// The switch of the kind `name` ([`Self::KINDS`]).
    pub fn kind_mut(&mut self, name: &str) -> Option<&mut bool> {
        Some(match name {
            "lights" => &mut self.lights,
            "cameras" => &mut self.cameras,
            "audio" => &mut self.audio,
            "decals" => &mut self.decals,
            "player_starts" => &mut self.player_starts,
            "match_rules" => &mut self.match_rules,
            "colliders" => &mut self.colliders,
            "captures" => &mut self.captures,
            "volumes" => &mut self.volumes,
            "nav" => &mut self.nav,
            _ => return None,
        })
    }

    /// Whether the kind `name` shows (`false` for an unknown name).
    #[must_use]
    pub fn kind(&self, name: &str) -> bool {
        let mut copy = *self;
        copy.kind_mut(name).is_some_and(|on| *on)
    }

    /// The icon scale factor (1 = default size).
    #[must_use]
    pub fn scale(&self) -> f32 {
        f32::from(
            self.size_percent
                .clamp(*Self::SIZE_RANGE.start(), *Self::SIZE_RANGE.end()),
        ) / 100.0
    }

    /// The icon opacity factor (1 = opaque).
    #[must_use]
    pub fn opacity(&self) -> f32 {
        f32::from(
            self.opacity_percent
                .clamp(*Self::OPACITY_RANGE.start(), *Self::OPACITY_RANGE.end()),
        ) / 100.0
    }
}

/// How a pane's exposure is set (the Exposure menu, ADR-7770).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ViewportExposure {
    /// The automatic editor exposure: the studio EV
    /// ([`crate::render::VIEWPORT_EXPOSURE_EV`]) at editor quality, the level's band centre (else
    /// the studio EV) at the other presets. Deterministic: it never adapts between frames.
    #[default]
    Auto,
    /// A fixed EV100 the user set ([`Self::EV_RANGE`]).
    Fixed(f32),
    /// The centre of the level's exposure band at every preset (the Auto value when the level has
    /// no band).
    LevelBand,
}

impl ViewportExposure {
    /// The EV100 range of [`Self::Fixed`].
    pub const EV_RANGE: std::ops::RangeInclusive<f32> = -4.0..=20.0;

    /// Stable name (`auto`, `fixed`, `level_band`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fixed(_) => "fixed",
            Self::LevelBand => "level_band",
        }
    }

    /// The fixed EV100, clamped to [`Self::EV_RANGE`] (`None` unless [`Self::Fixed`] with a
    /// finite value).
    #[must_use]
    pub fn fixed_ev(self) -> Option<f32> {
        match self {
            Self::Fixed(ev) if ev.is_finite() => {
                Some(ev.clamp(*Self::EV_RANGE.start(), *Self::EV_RANGE.end()))
            }
            _ => None,
        }
    }
}

/// One pane's display settings (view mode, icons, exposure, screen percentage; ADR-7770). Every
/// field's default is the pane's default look; [`crate::viewport_badges`] lists the ones that
/// differ.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneDisplay {
    /// What the picture shows.
    pub view_mode: ViewMode,
    /// Per-kind icon filters, icon size and opacity.
    pub icons: IconFilters,
    /// How the exposure is set.
    pub exposure: ViewportExposure,
    /// Internal render scale, percent of the pane's pixels ([`Self::SCREEN_PERCENTAGE_RANGE`]);
    /// the renderer's upscaler brings the picture to the pane size.
    pub screen_percentage: u16,
}

impl Default for PaneDisplay {
    fn default() -> Self {
        Self {
            view_mode: ViewMode::Lit,
            icons: IconFilters::default(),
            exposure: ViewportExposure::Auto,
            screen_percentage: 100,
        }
    }
}

impl PaneDisplay {
    /// The screen percentage range.
    pub const SCREEN_PERCENTAGE_RANGE: std::ops::RangeInclusive<u16> = 25..=200;

    /// [`Self::screen_percentage`] clamped to [`Self::SCREEN_PERCENTAGE_RANGE`].
    #[must_use]
    pub fn screen_percentage(&self) -> u16 {
        self.screen_percentage.clamp(
            *Self::SCREEN_PERCENTAGE_RANGE.start(),
            *Self::SCREEN_PERCENTAGE_RANGE.end(),
        )
    }
}

/// One viewport pane: its kind and (for orthographic panes) its own camera.
#[derive(Debug, Clone)]
pub struct ViewportPane {
    /// Which way the pane looks.
    pub kind: ViewKind,
    /// The pane's camera. The perspective pane uses the [`EditorCamera`] resource instead, so
    /// the agent protocol and play-in-editor share it.
    pub camera: EditorCamera,
    /// The pane's display settings (ADR-7770).
    pub display: PaneDisplay,
}

/// How a multi-pane layout splits the viewport (ADR-7755). The panes are the first
/// [`Self::pane_count`] of [`ViewportLayout::panes`]; each keeps its own camera and view kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum PaneSplit {
    /// Four panes in a 2×2 grid.
    #[default]
    Four,
    /// Two panes side by side.
    SideBySide,
    /// Two panes, one above the other.
    Stacked,
    /// One large pane on the left, two stacked on the right.
    OneLargeTwo,
}

impl PaneSplit {
    /// Every split, in menu order.
    pub const ALL: [Self; 4] = [Self::SideBySide, Self::Stacked, Self::OneLargeTwo, Self::Four];

    /// The protocol name (`quad`, `side_by_side`, `stacked`, `one_large_two`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Four => "quad",
            Self::SideBySide => "side_by_side",
            Self::Stacked => "stacked",
            Self::OneLargeTwo => "one_large_two",
        }
    }

    /// The menu label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Four => "4 panes",
            Self::SideBySide => "2 panes, side by side",
            Self::Stacked => "2 panes, stacked",
            Self::OneLargeTwo => "3 panes, one large",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// How many panes it shows.
    #[must_use]
    pub fn pane_count(self) -> usize {
        match self {
            Self::Four => 4,
            Self::SideBySide | Self::Stacked => 2,
            Self::OneLargeTwo => 3,
        }
    }

    /// The pane rectangles inside `full` as `(min, size)` pairs in the same units, pane 0 first,
    /// each shrunk by `gap` on its right and bottom edge.
    #[must_use]
    pub fn rects(self, min: Vec2, size: Vec2, gap: f32) -> Vec<(Vec2, Vec2)> {
        let cell = |x: f32, y: f32, w: f32, h: f32| {
            (
                min + Vec2::new(x * size.x, y * size.y),
                Vec2::new(w * size.x - gap, h * size.y - gap).max(Vec2::ONE),
            )
        };
        match self {
            Self::Four => (0..4)
                .map(|i| cell((i % 2) as f32 * 0.5, (i / 2) as f32 * 0.5, 0.5, 0.5))
                .collect(),
            Self::SideBySide => vec![cell(0.0, 0.0, 0.5, 1.0), cell(0.5, 0.0, 0.5, 1.0)],
            Self::Stacked => vec![cell(0.0, 0.0, 1.0, 0.5), cell(0.0, 0.5, 1.0, 0.5)],
            Self::OneLargeTwo => vec![
                cell(0.0, 0.0, 2.0 / 3.0, 1.0),
                cell(2.0 / 3.0, 0.0, 1.0 / 3.0, 0.5),
                cell(2.0 / 3.0, 0.5, 1.0 / 3.0, 0.5),
            ],
        }
    }
}

/// The viewport layout: one pane or several (2, 3 or 4, [`PaneSplit`]), the active pane, the
/// quality preset, show flags, game view and play-mode possession.
#[derive(Resource, Debug, Clone)]
pub struct ViewportLayout {
    /// Four panes; [`Self::quad`] shows the first [`PaneSplit::pane_count`] of them, otherwise
    /// only [`Self::active`].
    pub panes: [ViewportPane; 4],
    /// Show several panes ([`Self::split`] says how many and where).
    pub quad: bool,
    /// How several panes split the viewport while [`Self::quad`] is on (ADR-7755).
    pub split: PaneSplit,
    /// The pane hotkeys apply to (the last one hovered or clicked).
    pub active: usize,
    /// Render-quality preset.
    pub quality: ViewportQuality,
    /// Editor helpers.
    pub show: ShowFlags,
    /// Game view (`G`): hide every editor helper (grid, icons, bounds, outline, proxies).
    pub game_view: bool,
    /// Play mode: look through the game's camera (`F8` ejects to the editor camera).
    pub possessed: bool,
}

impl Default for ViewportLayout {
    fn default() -> Self {
        let ortho = |kind| ViewportPane {
            kind,
            camera: EditorCamera {
                target: Vec3::ZERO,
                distance: 20.0,
                ..EditorCamera::default()
            },
            display: PaneDisplay::default(),
        };
        Self {
            panes: [
                ViewportPane {
                    kind: ViewKind::Perspective,
                    camera: EditorCamera::default(),
                    display: PaneDisplay::default(),
                },
                ortho(ViewKind::Top),
                ortho(ViewKind::Front),
                ortho(ViewKind::Side),
            ],
            quad: false,
            split: PaneSplit::Four,
            active: 0,
            quality: ViewportQuality::Editor,
            show: ShowFlags::default(),
            game_view: false,
            possessed: true,
        }
    }
}

impl ViewportLayout {
    /// The layout's protocol name: `single`, or the [`PaneSplit::name`] while several panes show.
    #[must_use]
    pub fn layout_name(&self) -> &'static str {
        if self.quad { self.split.name() } else { "single" }
    }

    /// Sets the layout from [`Self::layout_name`]; false for an unknown name (nothing changes).
    /// The active pane moves into the shown panes.
    pub fn set_layout_name(&mut self, name: &str) -> bool {
        if name == "single" {
            self.quad = false;
            return true;
        }
        let Some(split) = PaneSplit::from_name(name) else {
            return false;
        };
        self.quad = true;
        self.split = split;
        if self.active >= split.pane_count() {
            self.active = 0;
        }
        true
    }

    /// The panes shown and their rectangles inside `(min, size)`: `(pane index, min, size)`.
    #[must_use]
    pub fn pane_rects(&self, min: Vec2, size: Vec2) -> Vec<(usize, Vec2, Vec2)> {
        if self.quad {
            self.split
                .rects(min, size, 2.0)
                .into_iter()
                .enumerate()
                .map(|(i, (m, s))| (i, m, s))
                .collect()
        } else {
            vec![(self.active.min(3), min, size)]
        }
    }

    /// The show flags in effect (game view hides every helper).
    #[must_use]
    pub fn effective_show(&self) -> ShowFlags {
        if self.game_view {
            ShowFlags {
                grid: false,
                icons: false,
                bounds: false,
                outline: false,
                proxies: false,
                audio: false,
                colliders: false,
                light_radii: false,
                navigation: false,
                paths: false,
                post_volumes: false,
                fog: false,
                lockstep: false,
                ..self.show
            }
        } else {
            self.show
        }
    }

    /// The show flags pane `index` draws with: [`Self::effective_show`] with the pane's icon
    /// filters ([`PaneDisplay::icons`]); a kind that is off also turns its always-on helpers off
    /// (light radii, audio ranges, colliders, the navigation mesh, post volumes).
    #[must_use]
    pub fn pane_show(&self, index: usize) -> ShowFlags {
        let mut show = self.effective_show();
        let icons = self.panes[index.min(3)].display.icons;
        show.icon_filter = icons;
        show.light_radii &= icons.lights;
        show.audio &= icons.audio;
        show.colliders &= icons.colliders;
        show.navigation &= icons.nav;
        show.post_volumes &= icons.volumes;
        show
    }

    /// The display settings of pane `index`.
    #[must_use]
    pub fn display(&self, index: usize) -> &PaneDisplay {
        &self.panes[index.min(3)].display
    }

    /// The display settings of pane `index`, to change.
    pub fn display_mut(&mut self, index: usize) -> &mut PaneDisplay {
        &mut self.panes[index.min(3)].display
    }

    /// The camera of pane `index` (the perspective pane reads the [`EditorCamera`] resource).
    #[must_use]
    pub fn pane_camera(&self, world: &World, index: usize) -> (ViewKind, EditorCamera) {
        let pane = &self.panes[index.min(3)];
        if pane.kind == ViewKind::Perspective {
            (
                pane.kind,
                world.get_resource::<EditorCamera>().cloned().unwrap_or_default(),
            )
        } else {
            (pane.kind, pane.camera.clone())
        }
    }

    /// Stores a camera back into pane `index` (the perspective pane writes the resource).
    pub fn set_pane_camera(&mut self, world: &mut World, index: usize, camera: EditorCamera) {
        let pane = &mut self.panes[index.min(3)];
        if pane.kind == ViewKind::Perspective {
            world.insert_resource(camera);
        } else {
            pane.camera = camera;
        }
    }
}

/// Saved camera placements (`Ctrl+0`…`Ctrl+9` store, `Shift+0`…`Shift+9` recall; bare digits are
/// the view presets since ADR-7204).
#[derive(Resource, Debug, Clone, Default)]
pub struct CameraBookmarks {
    /// Slots 0–9.
    pub slots: [Option<EditorCamera>; 10],
}

impl CameraBookmarks {
    /// Stores `camera` in `slot` (0–9). Returns false for an out-of-range slot.
    pub fn set(&mut self, slot: usize, camera: EditorCamera) -> bool {
        match self.slots.get_mut(slot) {
            Some(entry) => {
                *entry = Some(camera);
                true
            }
            None => false,
        }
    }

    /// The camera in `slot`, if one was stored.
    #[must_use]
    pub fn get(&self, slot: usize) -> Option<&EditorCamera> {
        self.slots.get(slot).and_then(Option::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ortho_views_look_along_their_axis() {
        let camera = EditorCamera {
            target: Vec3::new(1.0, 2.0, 3.0),
            distance: 10.0,
            ..EditorCamera::default()
        };
        let top = ViewCamera::of(ViewKind::Top, &camera);
        assert!(top.forward().y < -0.999, "{:?}", top.forward());
        let front = ViewCamera::of(ViewKind::Front, &camera);
        assert!((front.forward() - Vec3::NEG_Z).length() < 1e-4);
        let side = ViewCamera::of(ViewKind::Side, &camera);
        assert!((side.forward() - Vec3::NEG_X).length() < 1e-4);
        assert!(top.ortho && !ViewCamera::of(ViewKind::Perspective, &camera).ortho);
    }

    /// ADR-7755: the six orthographic views look along their axes, their screen axes match the
    /// projection, and every name parses.
    #[test]
    fn six_ortho_views_look_along_their_axes_and_screen_axes_match() {
        let camera = EditorCamera {
            target: Vec3::new(1.0, 2.0, 3.0),
            distance: 10.0,
            ..EditorCamera::default()
        };
        let looks = [
            (ViewKind::Top, Vec3::NEG_Y),
            (ViewKind::Bottom, Vec3::Y),
            (ViewKind::Front, Vec3::NEG_Z),
            (ViewKind::Back, Vec3::Z),
            (ViewKind::Side, Vec3::NEG_X),
            (ViewKind::OppositeSide, Vec3::X),
        ];
        let size = Vec2::new(400.0, 300.0);
        for (kind, look) in looks {
            let view = ViewCamera::of(kind, &camera);
            assert!(view.ortho, "{kind:?}");
            assert!(view.forward().dot(look) > 0.999, "{kind:?}: {:?}", view.forward());
            let (right, up) = kind.screen_axes().expect("ortho");
            let center = view.project(camera.target, size).expect("visible");
            let r = view.project(camera.target + right, size).expect("visible");
            let u = view.project(camera.target + up, size).expect("visible");
            assert!(
                r.x > center.x + 1.0 && (r.y - center.y).abs() < 0.5,
                "{kind:?} right"
            );
            assert!(
                u.y < center.y - 1.0 && (u.x - center.x).abs() < 0.5,
                "{kind:?} up"
            );
            assert_eq!(ViewKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(ViewKind::EVERY.len(), 7);
        assert!(ViewKind::Perspective.screen_axes().is_none());
    }

    /// ADR-7755: per-camera clip planes reach the perspective render camera; invalid ones fall
    /// back to the defaults.
    #[test]
    fn perspective_clip_planes_come_from_the_camera() {
        let camera = EditorCamera {
            near: 0.5,
            far: 900.0,
            ..EditorCamera::default()
        };
        let view = ViewCamera::of(ViewKind::Perspective, &camera);
        assert_eq!((view.near, view.far), (0.5, 900.0));
        let bad = EditorCamera {
            near: 5.0,
            far: 6.0,
            ..EditorCamera::default()
        };
        let view = ViewCamera::of(ViewKind::Perspective, &bad);
        assert_eq!((view.near, view.far), (EditorCamera::NEAR, PERSPECTIVE_FAR));
    }

    /// ADR-7755: 2- and 3-pane splits tile the viewport and name round trips.
    #[test]
    fn pane_splits_tile_the_viewport() {
        let mut layout = ViewportLayout::default();
        let (min, size) = (Vec2::new(10.0, 20.0), Vec2::new(900.0, 600.0));
        assert_eq!(layout.pane_rects(min, size).len(), 1);
        for (name, count) in [
            ("side_by_side", 2),
            ("stacked", 2),
            ("one_large_two", 3),
            ("quad", 4),
        ] {
            assert!(layout.set_layout_name(name), "{name}");
            assert_eq!(layout.layout_name(), name);
            let rects = layout.pane_rects(min, size);
            assert_eq!(rects.len(), count, "{name}");
            let area: f32 = rects.iter().map(|(_, _, s)| (s.x + 2.0) * (s.y + 2.0)).sum();
            assert!((area - size.x * size.y).abs() < 1.0, "{name}: {area}");
            for (i, (index, m, s)) in rects.iter().enumerate() {
                assert_eq!(*index, i);
                assert!(
                    m.x >= min.x
                        && m.y >= min.y
                        && m.x + s.x <= min.x + size.x
                        && m.y + s.y <= min.y + size.y
                );
            }
        }
        assert!(layout.set_layout_name("one_large_two"));
        let rects = layout.pane_rects(min, size);
        assert!(rects[0].2.x > rects[1].2.x * 1.9, "pane 0 is the large one");
        layout.active = 3;
        assert!(layout.set_layout_name("side_by_side"));
        assert_eq!(layout.active, 0, "the active pane moves into the shown panes");
        assert!(!layout.set_layout_name("five"));
        assert!(layout.set_layout_name("single") && !layout.quad);
    }

    /// The perspective pane and the backdrop sky use [`PERSPECTIVE_FAR`], not
    /// [`EditorCamera::FAR`] (the plane [`EditorCamera::projection`] clips with).
    #[test]
    fn perspective_far_is_the_viewport_plane() {
        let view = ViewCamera::of(ViewKind::Perspective, &EditorCamera::default());
        assert_eq!(view.far, PERSPECTIVE_FAR);
        assert_eq!(view.near, EditorCamera::NEAR);
        // The backdrop sky radius in `render` when the physical atmosphere is off:
        // `view.far - (view.far - view.near) * 0.02`.
        let radius = view.far - (view.far - view.near) * 0.02;
        assert!(
            (view.far - radius).abs() < 100.0,
            "the sky sits within 100 m of the far plane"
        );
        assert!(
            radius > EditorCamera::FAR,
            "the sky is past the picking far plane"
        );
    }

    #[test]
    fn telephoto_matches_the_ortho_half_height_and_is_nearly_parallel() {
        let camera = EditorCamera {
            target: Vec3::ZERO,
            distance: 10.0,
            ..EditorCamera::default()
        };
        let half = 10.0 * (camera.fov_deg.to_radians() * 0.5).tan();
        let view = ViewCamera::of(ViewKind::Front, &camera);
        let size = Vec2::new(400.0, 400.0);
        // A point at the top edge of the half-height projects to the top of the screen.
        let top = view.project(Vec3::new(0.0, half, 0.0), size).expect("in front");
        assert!(top.y.abs() < 0.5, "{top:?}");
        // Parallax: the same point 50 m nearer or farther moves by about a pixel at most.
        let near = view.project(Vec3::new(0.0, half, 50.0), size).expect("in front");
        let far = view.project(Vec3::new(0.0, half, -50.0), size).expect("in front");
        assert!((near.y - far.y).abs() < 1.5, "{near:?} {far:?}");
    }

    #[test]
    fn rays_round_trip_through_projection() {
        let camera = EditorCamera::default();
        for kind in ViewKind::ALL {
            let view = ViewCamera::of(kind, &camera);
            let size = Vec2::new(320.0, 240.0);
            let point = camera.target + Vec3::new(0.3, -0.2, 0.1);
            let pixel = view.project(point, size).expect("visible");
            let (origin, dir) = view.ray(pixel, size);
            let to_point = (point - origin).normalize();
            assert!(to_point.dot(dir) > 0.999_99, "{kind:?}");
        }
    }

    #[test]
    fn quality_names_round_trip() {
        for quality in ViewportQuality::ALL {
            assert_eq!(ViewportQuality::from_name(quality.name()), Some(quality));
        }
        assert_eq!(ViewportQuality::default().name(), "editor");
    }

    #[test]
    fn game_view_hides_helpers_and_bookmarks_store_cameras() {
        let layout = ViewportLayout {
            game_view: true,
            ..ViewportLayout::default()
        };
        let show = layout.effective_show();
        assert!(!show.grid && !show.icons && !show.outline && !show.proxies && !show.audio);
        let mut marks = CameraBookmarks::default();
        assert!(marks.set(3, EditorCamera::default()));
        assert!(marks.get(3).is_some() && marks.get(4).is_none());
        assert!(!marks.set(12, EditorCamera::default()));
    }

    /// ADR-7770: every keyed and buffer view mode round-trips through its name, the keyed modes
    /// differ in debug view or plain surface, and `wireframe` stays the renderer's lit wireframe.
    #[test]
    fn view_modes_round_trip_and_keyed_modes_differ() {
        let buffers = BUFFER_VIEWS
            .iter()
            .map(|(name, _)| ViewMode::from_name(name).expect("a renderer view"));
        for mode in ViewMode::KEYED.into_iter().chain(buffers) {
            assert_eq!(ViewMode::from_name(mode.name()), Some(mode), "{mode:?}");
            assert_eq!(
                ViewMode::of_debug_view(mode.debug_view()).debug_view(),
                mode.debug_view()
            );
            assert!(!mode.label().is_empty());
        }
        for (i, a) in ViewMode::KEYED.iter().enumerate() {
            for b in &ViewMode::KEYED[i + 1..] {
                assert!(
                    (a.debug_view(), a.surface()) != (b.debug_view(), b.surface()),
                    "{a:?} and {b:?} draw the same"
                );
            }
        }
        assert_eq!(ViewMode::from_name("wireframe"), Some(ViewMode::LitWireframe));
        assert_eq!(ViewMode::from_name("nope"), None);
        // A capture naming plain lit on a lighting-only pane draws no clay.
        assert_eq!(
            ViewMode::LightingOnly.surface_for(regia_render::DebugViewMode::Lit),
            Some(PlainSurface::Clay)
        );
        assert_eq!(
            ViewMode::LightingOnly.surface_for(regia_render::DebugViewMode::Unlit),
            None
        );
    }

    /// ADR-7770: a pane's icon filters reach its show flags and gate the kind's helpers; the
    /// other panes keep theirs.
    #[test]
    fn pane_show_applies_the_pane_icon_filters() {
        let mut layout = ViewportLayout::default();
        layout.show.light_radii = true;
        layout.show.navigation = true;
        let icons = &mut layout.display_mut(1).icons;
        *icons.kind_mut("lights").expect("kind") = false;
        *icons.kind_mut("nav").expect("kind") = false;
        icons.size_percent = 150;
        let pane = layout.pane_show(1);
        assert!(!pane.icon_filter.lights && !pane.light_radii && !pane.navigation);
        assert!((pane.icon_filter.scale() - 1.5).abs() < 1e-6);
        let other = layout.pane_show(0);
        assert!(other.icon_filter.lights && other.light_radii && other.navigation);
        for (name, _) in IconFilters::KINDS {
            assert!(IconFilters::default().kind(name), "{name}");
        }
        assert!(!IconFilters::default().kind("splines"));
        assert_eq!(ViewportExposure::Fixed(99.0).fixed_ev(), Some(20.0));
        assert_eq!(ViewportExposure::Auto.fixed_ev(), None);
        let display = PaneDisplay {
            screen_percentage: 5,
            ..PaneDisplay::default()
        };
        assert_eq!(display.screen_percentage(), 25);
    }

    /// ADR-7770: the new keys are free: each parses, and no other command's built-in shortcut
    /// or fixed editor key uses the same chord (Alt+P, play, is a shell key, not a digit or H).
    #[test]
    fn the_view_mode_and_hide_keys_are_free() {
        use crate::command_palette::COMMANDS;
        use crate::command_palette::KeyChord;
        use crate::editor_keymap::FIXED_EDITOR_KEYS;
        let ours: Vec<_> = COMMANDS
            .iter()
            .filter(|def| {
                def.id.starts_with("view.mode_")
                    || matches!(def.id, "view.hide_selection" | "view.unhide_all" | "view.isolate")
            })
            .collect();
        assert_eq!(ours.len(), 11);
        for def in &ours {
            let chord = KeyChord::parse(def.shortcut).unwrap_or_else(|| panic!("{} parses", def.shortcut));
            for other in COMMANDS
                .iter()
                .filter(|o| o.id != def.id && !o.shortcut.is_empty())
            {
                assert_ne!(
                    KeyChord::parse(other.shortcut),
                    Some(chord),
                    "{} and {}",
                    def.id,
                    other.id
                );
            }
            for (keys, what) in FIXED_EDITOR_KEYS {
                assert_ne!(KeyChord::parse(keys), Some(chord), "{} and {what}", def.id);
            }
            assert_ne!(KeyChord::parse("Alt+P"), Some(chord), "{} and play", def.id);
        }
    }
}
