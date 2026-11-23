// SPDX-License-Identifier: MPL-2.0
//! The shared preview scene of the asset editors (v0.6 UI parity P-083, `docs/decisions.md`
//! ADR-7771): lighting presets, an environment intensity, a floor with a grid and an auto-rotate
//! turntable, used by the Material editor and the mesh and texture viewers
//! (`editor.preview_scene`, `book/agents/material-editor.md`).
//!
//! The pieces are small and pure:
//!
//! - [`PreviewSceneSettings`] — what the user chose (preset, intensity, floor, rotate);
//! - [`PreviewSceneSettings::rig`] — the lights, ambient term and backdrop a preset resolves to
//!   ([`PreviewRig`]), from the editor's historical sun ([`SunBase`]), the preview's size and,
//!   for the `level` preset, the open level's sun ([`LevelLighting`]);
//! - [`PreviewFloor`] and [`push_floor`] — the floor quad and its grid as two frame meshes;
//! - [`PreviewSpin`] — the turntable angle, advanced once per egui frame;
//! - [`preview_scene_controls`] — the controls, every one a focusable egui widget.
//!
//! The default settings reproduce the previews' look from before this module (the `outdoor`
//! preset with the editor's own sun, no floor, no rotation), so nothing moves for a user who
//! never touches the controls.
//!
//! Algorithms: the three-point rig follows the textbook key / fill / rim arrangement (key from
//! the front and above, a weaker fill from the opposite side, a rim light from behind); a point
//! light's lumens are the illuminance wanted at the preview's center times the distance squared
//! times 4π (the inverse-square law, `KHR_lights_punctual` § 5.3 units, as `LabPointLight`).

use std::sync::OnceLock;

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::{Mat4, Vec3};
use regia_ecs::prelude::*;
use regia_render::{
    DirectionalLight, DirectionalLightUniform, FrameMesh, FrameRenderer, LabLocalLight, LabMaterial,
    LabPointLight, LabVertex, RenderDevice, SKY_IBL_SCALE, StudioGradient,
};
use serde_json::{Value, json};

use crate::viewport_agent::Handler;

/// The editors that have a preview scene, by protocol name.
pub const EDITORS: [&str; 4] = ["material", "mesh", "texture", "matgraph"];

/// Environment intensity range (a multiplier of the preset's ambient light).
pub const ENV_INTENSITY_MIN: f32 = 0.0;
/// Environment intensity range, upper end.
pub const ENV_INTENSITY_MAX: f32 = 4.0;

/// Turntable speed, radians per second (one turn in 12 s).
pub const SPIN_RADIANS_PER_SECOND: f32 = std::f32::consts::TAU / 12.0;

/// The ambient illuminance of an environment intensity of 1, lux (the previews' historical value).
pub const AMBIENT_LUX: f32 = 12_000.0;

/// A lighting preset of the preview scene.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PreviewLighting {
    /// The editor's own sun over the procedural sky (what the previews always drew).
    #[default]
    Outdoor,
    /// Three-point studio light: a key, a weaker fill and a rim, on a dark backdrop.
    Studio,
    /// Even light from a flat gray environment with a weak key: shows color and form, no mood.
    Neutral,
    /// The open level's sun (its directional light) over the sky; `outdoor` when no level light exists.
    Level,
}

impl PreviewLighting {
    /// Every preset, in menu order.
    pub const ALL: [Self; 4] = [Self::Outdoor, Self::Studio, Self::Neutral, Self::Level];

    /// The protocol name (`outdoor`, `studio`, `neutral`, `level`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Outdoor => "outdoor",
            Self::Studio => "studio",
            Self::Neutral => "neutral",
            Self::Level => "level",
        }
    }

    /// The menu label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Outdoor => "Outdoor sun",
            Self::Studio => "Studio (three-point)",
            Self::Neutral => "Neutral gray",
            Self::Level => "Level lighting",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// What the texture viewer draws the texture on (the other editors ignore it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TextureSurface {
    /// The flat 2D image with its mip and channel views (the viewer's own view).
    #[default]
    Flat,
    /// The texture as the albedo of a lit sphere.
    Sphere,
    /// The texture as the albedo of a lit square plane.
    Plane,
}

impl TextureSurface {
    /// Every surface, in menu order.
    pub const ALL: [Self; 3] = [Self::Flat, Self::Sphere, Self::Plane];

    /// The protocol name (`flat`, `sphere`, `plane`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Sphere => "sphere",
            Self::Plane => "plane",
        }
    }

    /// The menu label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Flat => "Flat image",
            Self::Sphere => "On a sphere",
            Self::Plane => "On a plane",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// What the user chose for one editor's preview.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreviewSceneSettings {
    /// The lighting preset.
    pub lighting: PreviewLighting,
    /// Multiplier of the preset's ambient light, [`ENV_INTENSITY_MIN`]..=[`ENV_INTENSITY_MAX`].
    pub env_intensity: f32,
    /// Draw a floor with a grid under the subject.
    pub floor: bool,
    /// Turn the subject on a turntable.
    pub auto_rotate: bool,
    /// What the texture viewer draws the texture on (ignored by the other editors).
    pub surface: TextureSurface,
}

impl Default for PreviewSceneSettings {
    fn default() -> Self {
        Self {
            lighting: PreviewLighting::Outdoor,
            env_intensity: 1.0,
            floor: false,
            auto_rotate: false,
            surface: TextureSurface::Flat,
        }
    }
}

/// [`PreviewSceneSettings`] as one comparable value (the intensity by bits).
pub type PreviewSceneKey = (PreviewLighting, u32, bool, bool, TextureSurface);

impl PreviewSceneSettings {
    /// The settings as one comparable key (floats by bits): the previews that render only on
    /// change (ADR-9970) fold it into their render key.
    #[must_use]
    pub fn key(&self) -> PreviewSceneKey {
        (
            self.lighting,
            self.env_intensity.to_bits(),
            self.floor,
            self.auto_rotate,
            self.surface,
        )
    }

    /// The intensity clamped to its range (a NaN becomes 1).
    #[must_use]
    pub fn clamped_intensity(&self) -> f32 {
        if self.env_intensity.is_finite() {
            self.env_intensity.clamp(ENV_INTENSITY_MIN, ENV_INTENSITY_MAX)
        } else {
            1.0
        }
    }

    /// Applies an agent patch (`preset`, `intensity`, `floor`, `rotate`); absent fields keep their value.
    ///
    /// # Errors
    ///
    /// `invalid_params` for an unknown preset, an intensity outside the range or a non-boolean flag.
    pub fn patch(&mut self, params: &Value) -> Result<(), RpcError> {
        let mut next = *self;
        match params.get("preset") {
            None | Some(Value::Null) => {}
            Some(Value::String(name)) => {
                next.lighting = PreviewLighting::from_name(name).ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "unknown preset `{name}` (outdoor, studio, neutral, level)"
                    ))
                })?;
            }
            Some(_) => return Err(RpcError::invalid_params("`preset` must be a string")),
        }
        match params.get("intensity") {
            None | Some(Value::Null) => {}
            Some(value) => {
                let v = value
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| RpcError::invalid_params("`intensity` must be a number"))?;
                if !(f64::from(ENV_INTENSITY_MIN)..=f64::from(ENV_INTENSITY_MAX)).contains(&v) {
                    return Err(RpcError::invalid_params(format!(
                        "`intensity` must be between {ENV_INTENSITY_MIN} and {ENV_INTENSITY_MAX}"
                    )));
                }
                next.env_intensity = v as f32;
            }
        }
        match params.get("shape") {
            None | Some(Value::Null) => {}
            Some(Value::String(name)) => {
                next.surface = TextureSurface::from_name(name).ok_or_else(|| {
                    RpcError::invalid_params(format!("unknown shape `{name}` (flat, sphere, plane)"))
                })?;
            }
            Some(_) => return Err(RpcError::invalid_params("`shape` must be a string")),
        }
        for (name, slot) in [("floor", &mut next.floor), ("rotate", &mut next.auto_rotate)] {
            match params.get(name) {
                None | Some(Value::Null) => {}
                Some(Value::Bool(b)) => *slot = *b,
                Some(_) => return Err(RpcError::invalid_params(format!("`{name}` must be a boolean"))),
            }
        }
        *self = next;
        Ok(())
    }

    /// The protocol view of the settings.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "preset": self.lighting.name(),
            "intensity": self.env_intensity,
            "floor": self.floor,
            "rotate": self.auto_rotate,
            "shape": self.surface.name(),
        })
    }
}

/// The sun an editor's preview has always used: the `outdoor` preset keeps it, and the other
/// presets do not use it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SunBase {
    /// Direction of light travel.
    pub direction: [f32; 3],
    /// Linear RGB color.
    pub color: [f32; 3],
}

/// The open level's sun, as the `level` preset reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelLighting {
    /// Direction of light travel.
    pub direction: [f32; 3],
    /// Linear RGB color.
    pub color: [f32; 3],
    /// Illuminance, lux.
    pub illuminance: f32,
}

/// The level's sun: the directional light the renderer picks as the sun (the lowest entity bits,
/// `regia_render::lights_views::pick_directional`), or `None` when the world has none.
#[must_use]
pub fn level_lighting(world: &mut World) -> Option<LevelLighting> {
    let mut query = world.query::<(Entity, &DirectionalLight)>();
    let rows: Vec<(u64, LevelLighting)> = query
        .iter(world)
        .map(|(entity, light)| {
            (
                entity.to_bits(),
                LevelLighting {
                    direction: light.direction,
                    color: light.color,
                    illuminance: light.illuminance,
                },
            )
        })
        .collect();
    let keys: Vec<u64> = rows.iter().map(|(bits, _)| *bits).collect();
    regia_render::lights_views::pick_directional(&keys)
        .sun
        .map(|i| rows[i].1)
}

/// What fills the preview behind the subject.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Backdrop {
    /// The editor's procedural sky dome.
    Sky,
    /// A flat unlit color (linear RGB).
    Flat([f32; 3]),
}

/// The image-based light of a preview.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EnvLight {
    /// The renderer's own procedural sky.
    Default,
    /// A sun-disc-free gradient environment (radiances in cd/m²): diffuse and reflections.
    Gradient(StudioGradient),
}

/// A preset resolved to what the frame renderer takes.
#[derive(Clone, Debug)]
pub struct PreviewRig {
    /// The key / sun light.
    pub directional: DirectionalLightUniform,
    /// The flat ambient illuminance, lux: used only when `ibl` is off (the fallback when an
    /// environment cannot be installed).
    pub ambient_lux: f32,
    /// Image-based light from `environment`; off uses the flat ambient term.
    pub ibl: bool,
    /// The image-based light.
    pub environment: EnvLight,
    /// Fill and rim lights.
    pub local_lights: Vec<LabLocalLight>,
    /// The backdrop.
    pub backdrop: Backdrop,
    /// The preset that was asked for was unavailable and `outdoor` stands in (`level` without a
    /// level sun).
    pub fallback: bool,
}

/// Lumens of a point light at `distance` that give `lux` at the preview's center.
#[must_use]
pub fn lumens_for_lux(lux: f32, distance: f32) -> f32 {
    lux * distance * distance * 4.0 * std::f32::consts::PI
}

impl PreviewSceneSettings {
    /// Resolves the preset for a subject of `radius` meters around the origin. `sun` is the
    /// editor's own sun (the `outdoor` preset); `level` is the open level's sun, when there is one.
    #[must_use]
    pub fn rig(&self, sun: SunBase, radius: f32, level: Option<&LevelLighting>) -> PreviewRig {
        let radius = if radius.is_finite() { radius.max(0.01) } else { 1.0 };
        let intensity = self.clamped_intensity();
        let ambient = AMBIENT_LUX * intensity;
        // The procedural sky's gradient (`render::sky_color`: horizon, zenith, ground) as an
        // environment scaled by the intensity; the sky itself is kept at intensity 1, where the
        // picture is exactly the one the previews always drew.
        let sky_env = || {
            if (intensity - 1.0).abs() < 1e-6 {
                EnvLight::Default
            } else {
                let scale = |c: [f32; 3]| c.map(|v| v * SKY_IBL_SCALE * intensity);
                EnvLight::Gradient(StudioGradient {
                    zenith: scale([0.15, 0.25, 0.55]),
                    horizon: scale([0.78, 0.85, 0.95]),
                    ground: scale([0.12, 0.10, 0.08]),
                    falloff: 0.5,
                })
            }
        };
        let outdoor = |direction: [f32; 3], color: [f32; 3], lux: f32, fallback: bool| PreviewRig {
            directional: DirectionalLightUniform::new(direction, color, lux),
            ambient_lux: ambient,
            ibl: true,
            environment: sky_env(),
            local_lights: Vec::new(),
            backdrop: Backdrop::Sky,
            fallback,
        };
        match self.lighting {
            PreviewLighting::Outdoor => outdoor(sun.direction, sun.color, 100_000.0, false),
            PreviewLighting::Level => match level {
                Some(level) => outdoor(level.direction, level.color, level.illuminance, false),
                None => outdoor(sun.direction, sun.color, 100_000.0, true),
            },
            PreviewLighting::Studio => {
                // Key: from the front left and above. Fill: from the right at the eye's height.
                // Rim: from behind and above. Distances scale with the subject.
                let point = |position: Vec3, color: [f32; 3], lux: f32| {
                    LabLocalLight::Point(LabPointLight {
                        position: position.to_array(),
                        color,
                        intensity: lumens_for_lux(lux, position.length()),
                        radius: position.length() * 4.0,
                        cast_shadows: false,
                        source_radius: None,
                    })
                };
                let fill = point(Vec3::new(2.6, 0.7, 1.5) * radius, [0.85, 0.92, 1.0], 14_000.0);
                let rim = point(Vec3::new(0.5, 1.9, -2.5) * radius, [1.0, 1.0, 1.0], 30_000.0);
                let scale = |c: [f32; 3]| c.map(|v| v * intensity);
                PreviewRig {
                    directional: DirectionalLightUniform::new([0.55, -0.6, -0.6], [1.0, 0.96, 0.9], 55_000.0),
                    ambient_lux: 6_000.0 * intensity,
                    ibl: true,
                    environment: EnvLight::Gradient(StudioGradient {
                        zenith: scale([2400.0, 2400.0, 2500.0]),
                        horizon: scale([1700.0, 1700.0, 1750.0]),
                        ground: scale([600.0, 600.0, 600.0]),
                        falloff: 1.0,
                    }),
                    local_lights: vec![fill, rim],
                    backdrop: Backdrop::Flat([0.035, 0.035, 0.04]),
                    fallback: false,
                }
            }
            PreviewLighting::Neutral => PreviewRig {
                directional: DirectionalLightUniform::new([0.25, -0.8, -0.55], [1.0, 1.0, 1.0], 22_000.0),
                ambient_lux: 40_000.0 * intensity,
                ibl: true,
                environment: EnvLight::Gradient(StudioGradient::new(
                    [intensity * 8_000.0; 3],
                    [intensity * 8_000.0; 3],
                    [intensity * 8_000.0; 3],
                )),
                local_lights: Vec::new(),
                backdrop: Backdrop::Flat([0.22, 0.22, 0.22]),
                fallback: false,
            },
        }
    }
}

/// Installs `environment` on `renderer` (a persistent preview renderer): a gradient is baked once
/// and kept while it stays the same; the default sky is put back only when a gradient was
/// installed (so a steady preview never rebuilds it). Answers whether the image-based light is
/// usable (`false`: the gradient was refused, so the frame should use the flat ambient term).
pub fn apply_environment(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    environment: &EnvLight,
) -> bool {
    match environment {
        EnvLight::Default => {
            if renderer.studio_environment_key().is_some() {
                let _ = renderer.set_studio_gradient(device, None);
            }
            true
        }
        EnvLight::Gradient(gradient) => renderer.set_studio_gradient(device, Some(gradient)).is_ok(),
    }
}

// ---------------------------------------------------------------------------------------------
// The turntable
// ---------------------------------------------------------------------------------------------

/// The turntable angle of one preview.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PreviewSpin {
    /// Turn around the vertical axis, radians (0..TAU).
    pub angle: f32,
    /// The egui frame the angle was last advanced in (a panel's UI runs twice in some frames).
    last_frame: u64,
}

impl PreviewSpin {
    /// Advances the angle by `dt` seconds; non-finite or negative `dt` is ignored.
    pub fn advance_by(&mut self, dt: f32) {
        if dt.is_finite() && dt > 0.0 {
            self.angle = (self.angle + dt * SPIN_RADIANS_PER_SECOND).rem_euclid(std::f32::consts::TAU);
        }
    }

    /// Advances once per egui frame while `settings` turns, and asks for the next frame.
    pub fn tick(&mut self, ctx: &egui::Context, settings: &PreviewSceneSettings) {
        if !settings.auto_rotate {
            return;
        }
        let frame = ctx.cumulative_frame_nr();
        if frame != self.last_frame {
            self.last_frame = frame;
            self.advance_by(ctx.input(|i| i.stable_dt).min(0.1));
        }
        ctx.request_repaint();
    }

    /// The angle as a render-key part: a steady preview has the same key every frame.
    #[must_use]
    pub fn key(&self, settings: &PreviewSceneSettings) -> u32 {
        if settings.auto_rotate {
            self.angle.to_bits()
        } else {
            0
        }
    }

    /// The model matrix of a subject turned about the vertical axis through `center`
    /// (identity while the turntable is off).
    #[must_use]
    pub fn model(&self, settings: &PreviewSceneSettings, center: Vec3) -> Mat4 {
        if !settings.auto_rotate || self.angle == 0.0 {
            return Mat4::IDENTITY;
        }
        Mat4::from_translation(center) * Mat4::from_rotation_y(self.angle) * Mat4::from_translation(-center)
    }
}

/// Everything one preview needs from the shared scene: the user's settings, the turntable and
/// the open level's sun (for the `level` preset).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PreviewContext {
    /// The chosen settings.
    pub settings: PreviewSceneSettings,
    /// The turntable.
    pub spin: PreviewSpin,
    /// The open level's sun, refreshed by [`sync`].
    pub level: Option<LevelLighting>,
}

/// What a preview was last rendered with: a preview that renders only on change (ADR-9970)
/// compares it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreviewContextKey {
    settings: PreviewSceneKey,
    spin: u32,
    level: Option<[u32; 7]>,
}

impl PreviewContext {
    /// The comparable key of this context (the level's sun only counts for the `level` preset).
    #[must_use]
    pub fn key(&self) -> PreviewContextKey {
        PreviewContextKey {
            settings: self.settings.key(),
            spin: self.spin.key(&self.settings),
            level: self
                .level
                .filter(|_| self.settings.lighting == PreviewLighting::Level)
                .map(|l| {
                    [
                        l.direction[0].to_bits(),
                        l.direction[1].to_bits(),
                        l.direction[2].to_bits(),
                        l.color[0].to_bits(),
                        l.color[1].to_bits(),
                        l.color[2].to_bits(),
                        l.illuminance.to_bits(),
                    ]
                }),
        }
    }

    /// The rig for a subject of `radius` meters with the editor's own `sun`.
    #[must_use]
    pub fn rig(&self, sun: SunBase, radius: f32) -> PreviewRig {
        self.settings.rig(sun, radius, self.level.as_ref())
    }

    /// Advances the turntable once per egui frame (and asks for the next frame while it turns).
    pub fn tick(&mut self, ctx: &egui::Context) {
        self.spin.tick(ctx, &self.settings);
    }
}

// ---------------------------------------------------------------------------------------------
// The floor and its grid
// ---------------------------------------------------------------------------------------------

/// Grid cells along one side of the unit floor.
pub const FLOOR_CELLS: usize = 16;
/// A grid line's width as a fraction of the floor's half extent.
const GRID_LINE_WIDTH: f32 = 0.006;
/// The grid floats this far above the floor (half-extent units) so the two never fight in depth.
const GRID_LIFT: f32 = 0.0025;

/// Frame-renderer mesh keys of the floor and its grid (one geometry, scaled per preview).
const FLOOR_MESH_KEY: u64 = 0x00ed_17c1;
const GRID_MESH_KEY: u64 = 0x00ed_17c2;
const FLOOR_MATERIAL_KEY: u64 = 0x00ed_17c3;
const GRID_MATERIAL_KEY: u64 = 0x00ed_17c4;
const FLOOR_OBJECT_KEY: u64 = 0x00ed_17c5;
const GRID_OBJECT_KEY: u64 = 0x00ed_17c6;
const BACKDROP_MESH_KEY: u64 = 0x00ed_17c7;
const BACKDROP_MATERIAL_KEY: u64 = 0x00ed_17c8;
const BACKDROP_OBJECT_KEY: u64 = 0x00ed_17c9;

fn up_vertex(x: f32, y: f32, z: f32) -> LabVertex {
    LabVertex {
        position: [x, y, z],
        normal: [0.0, 1.0, 0.0],
        uv: [x * 0.5 + 0.5, z * 0.5 + 0.5],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

/// Vertices and triangle-list indices of one mesh.
pub type MeshData = (Vec<LabVertex>, Vec<u32>);

/// The unit floor (a square of half extent 1 facing up, at height 0) and its grid (thin quads
/// `cells` + 1 along each axis, at `GRID_LIFT`): vertices and triangle-list indices of each.
#[must_use]
pub fn floor_geometry(cells: usize) -> (MeshData, MeshData) {
    let cells = cells.clamp(1, 64);
    let quad = |vertices: &mut Vec<LabVertex>,
                indices: &mut Vec<u32>,
                x0: f32,
                z0: f32,
                x1: f32,
                z1: f32,
                y: f32| {
        let base = vertices.len() as u32;
        // Counter-clockwise seen from above (+Y), so the quad faces up.
        vertices.extend([
            up_vertex(x0, y, z0),
            up_vertex(x0, y, z1),
            up_vertex(x1, y, z1),
            up_vertex(x1, y, z0),
        ]);
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    };
    let (mut fv, mut fi) = (Vec::new(), Vec::new());
    quad(&mut fv, &mut fi, -1.0, -1.0, 1.0, 1.0, 0.0);
    let (mut gv, mut gi) = (Vec::new(), Vec::new());
    for line in 0..=cells {
        let at = -1.0 + 2.0 * line as f32 / cells as f32;
        let half = GRID_LINE_WIDTH * 0.5;
        quad(&mut gv, &mut gi, at - half, -1.0, at + half, 1.0, GRID_LIFT);
        quad(&mut gv, &mut gi, -1.0, at - half, 1.0, at + half, GRID_LIFT);
    }
    ((fv, fi), (gv, gi))
}

fn floor_meshes() -> &'static (MeshData, MeshData) {
    static MESHES: OnceLock<(MeshData, MeshData)> = OnceLock::new();
    MESHES.get_or_init(|| floor_geometry(FLOOR_CELLS))
}

/// The floor's and the grid's materials (a frame's meshes borrow them).
#[derive(Clone, Debug)]
pub struct PreviewFloor {
    /// The floor.
    pub floor: LabMaterial,
    /// The grid lines.
    pub grid: LabMaterial,
}

impl Default for PreviewFloor {
    fn default() -> Self {
        Self {
            floor: LabMaterial {
                base_color: [0.30, 0.30, 0.31, 1.0],
                metallic: 0.0,
                roughness: 0.85,
                ..LabMaterial::default()
            },
            grid: LabMaterial {
                base_color: [0.06, 0.06, 0.07, 1.0],
                metallic: 0.0,
                roughness: 0.9,
                ..LabMaterial::default()
            },
        }
    }
}

/// Where the floor goes for a subject: a floor `y` at the subject's lowest point, and a half
/// extent of 3 times the subject's radius (so the grid spans well past it).
#[must_use]
pub fn floor_placement(min_y: f32, radius: f32) -> (f32, f32) {
    let radius = if radius.is_finite() { radius.max(0.01) } else { 1.0 };
    let y = if min_y.is_finite() {
        min_y - 0.002 * radius
    } else {
        0.0
    };
    (y, radius * 3.0)
}

/// Adds the floor and its grid to `meshes`: the floor `y` and `half_extent` come from
/// [`floor_placement`].
pub fn push_floor<'a>(meshes: &mut Vec<FrameMesh<'a>>, floor: &'a PreviewFloor, y: f32, half_extent: f32) {
    let ((fv, fi), (gv, gi)) = floor_meshes();
    let model = Mat4::from_translation(Vec3::new(0.0, y, 0.0))
        * Mat4::from_scale(Vec3::new(half_extent, half_extent, half_extent));
    for (mesh_key, material_key, object_key, vertices, indices, material) in [
        (
            FLOOR_MESH_KEY,
            FLOOR_MATERIAL_KEY,
            FLOOR_OBJECT_KEY,
            fv,
            fi,
            &floor.floor,
        ),
        (
            GRID_MESH_KEY,
            GRID_MATERIAL_KEY,
            GRID_OBJECT_KEY,
            gv,
            gi,
            &floor.grid,
        ),
    ] {
        meshes.push(FrameMesh {
            is_static: false,
            mesh_key,
            material_key,
            object_key,
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
}

// ---------------------------------------------------------------------------------------------
// A flat backdrop
// ---------------------------------------------------------------------------------------------

/// An inward unit sphere with white vertices (the flat backdrop's dome; the material's base color
/// is the backdrop color).
fn flat_dome() -> &'static (Vec<LabVertex>, Vec<u32>) {
    static MESH: OnceLock<(Vec<LabVertex>, Vec<u32>)> = OnceLock::new();
    MESH.get_or_init(|| {
        let (mut vertices, indices) = crate::render::sky_sphere().clone();
        for v in &mut vertices {
            v.color = [1.0; 4];
        }
        (vertices, indices)
    })
}

/// The material of a flat backdrop (unlit, the color as given).
#[must_use]
pub fn flat_backdrop_material(color: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [color[0], color[1], color[2], 1.0],
        metallic: 0.0,
        roughness: 1.0,
        unlit: true,
        ..LabMaterial::default()
    }
}

/// Adds the backdrop dome (`radius` meters around `eye`) to `meshes`: the editor's sky for
/// [`Backdrop::Sky`] (through `sky`, the caller's usual sky material and dome), or a flat dome
/// with `flat` as its material.
pub fn push_backdrop<'a>(
    meshes: &mut Vec<FrameMesh<'a>>,
    backdrop: Backdrop,
    sky: (&'a LabMaterial, &'a (Vec<LabVertex>, Vec<u32>)),
    flat: &'a LabMaterial,
    eye: Vec3,
    radius: f32,
) {
    match backdrop {
        Backdrop::Sky => crate::render::push_sky(meshes, &sky.1.0, &sky.1.1, sky.0, eye, radius),
        Backdrop::Flat(_) => {
            let dome = flat_dome();
            let model = Mat4::from_scale_rotation_translation(
                Vec3::splat(radius),
                regia_core::math::Quat::IDENTITY,
                eye,
            );
            meshes.push(FrameMesh {
                is_static: false,
                mesh_key: BACKDROP_MESH_KEY,
                material_key: BACKDROP_MATERIAL_KEY,
                object_key: BACKDROP_OBJECT_KEY,
                vertices: &dome.0,
                indices: &dome.1,
                material: flat,
                model: model.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: regia_render::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            });
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The controls
// ---------------------------------------------------------------------------------------------

/// Draws the preview scene controls (lighting preset, environment intensity, floor, rotate) and
/// answers whether anything changed. Every control is a focusable egui widget, so the keyboard
/// reaches each (Tab, then Space or Enter, arrows on the intensity); sizes follow
/// `appearance.ui_scale` through egui's pixels per point; no color is set here.
pub fn preview_scene_controls(ui: &mut egui::Ui, settings: &mut PreviewSceneSettings) -> bool {
    let before = *settings;
    ui.horizontal_wrapped(|ui| {
        let combo = egui::ComboBox::from_id_salt("preview-scene-lighting")
            .selected_text(settings.lighting.label())
            .show_ui(ui, |ui| {
                for preset in PreviewLighting::ALL {
                    ui.selectable_value(&mut settings.lighting, preset, preset.label());
                }
            });
        crate::a11y_audit::name(combo.response, egui::WidgetType::ComboBox, "Preview lighting");
        let drag = ui.add(
            egui::DragValue::new(&mut settings.env_intensity)
                .range(ENV_INTENSITY_MIN..=ENV_INTENSITY_MAX)
                .speed(0.02)
                .fixed_decimals(2)
                .prefix("env "),
        );
        crate::a11y_audit::name(drag, egui::WidgetType::DragValue, "Environment intensity");
        crate::agent_parity::action_toggle(
            ui,
            &crate::agent_parity::PREVIEW_SCENE_FLOOR,
            &mut settings.floor,
        );
        crate::agent_parity::action_toggle(
            ui,
            &crate::agent_parity::PREVIEW_SCENE_ROTATE,
            &mut settings.auto_rotate,
        );
    });
    settings.env_intensity = settings.clamped_intensity();
    *settings != before
}

// ---------------------------------------------------------------------------------------------
// The agent surface (`editor.preview_scene`)
// ---------------------------------------------------------------------------------------------

/// What the agent and the shell share about the preview scenes: the current settings of each
/// editor (as the panels last published them, then as agent patches changed them) and the
/// patches the panels have not taken yet.
#[derive(Resource, Debug, Clone, Default)]
pub struct PreviewScenes {
    /// Material editor.
    pub material: PreviewSceneSettings,
    /// Mesh viewer.
    pub mesh: PreviewSceneSettings,
    /// Texture viewer.
    pub texture: PreviewSceneSettings,
    /// Material graph editor (w13, P-083: the same scene as the other asset viewers).
    pub matgraph: PreviewSceneSettings,
    /// Patches from the agent, per editor, not yet applied to the panels.
    pub pending: Vec<(String, Value)>,
    /// The open level's sun (refreshed by [`sync`]), so the panels can resolve the `level` preset.
    pub level: Option<LevelLighting>,
}

impl PreviewScenes {
    /// The settings of `editor`.
    #[must_use]
    pub fn settings(&self, editor: &str) -> Option<&PreviewSceneSettings> {
        match editor {
            "material" => Some(&self.material),
            "mesh" => Some(&self.mesh),
            "texture" => Some(&self.texture),
            "matgraph" => Some(&self.matgraph),
            _ => None,
        }
    }

    /// The scene a deterministic agent render of `editor` draws: its settings and the open
    /// level's sun, the turntable at rest.
    #[must_use]
    pub fn context(&self, editor: &str) -> PreviewContext {
        PreviewContext {
            settings: self.settings(editor).copied().unwrap_or_default(),
            spin: PreviewSpin::default(),
            level: self.level,
        }
    }

    fn settings_mut(&mut self, editor: &str) -> Option<&mut PreviewSceneSettings> {
        match editor {
            "material" => Some(&mut self.material),
            "mesh" => Some(&mut self.mesh),
            "texture" => Some(&mut self.texture),
            "matgraph" => Some(&mut self.matgraph),
            _ => None,
        }
    }
}

/// The shell's per-frame hand-off: applies the agent's pending patches to the panels' own
/// contexts, then publishes the panels' settings back to the world and gives each panel the open
/// level's sun. Cheap when nothing changed.
pub fn sync(
    world: &mut World,
    material: &mut PreviewContext,
    mesh: &mut PreviewContext,
    texture: &mut PreviewContext,
) {
    let level = level_lighting(world);
    // The material graph editor keeps its scene in its own resource (w13, P-083).
    let mut matgraph = world
        .get_resource::<crate::matgraph_editor::MatGraphEditor>()
        .map(|e| e.preview_scene);
    let mut scenes = world.get_resource_or_init::<PreviewScenes>();
    for (editor, patch) in std::mem::take(&mut scenes.pending) {
        let target = match editor.as_str() {
            "material" => &mut *material,
            "mesh" => &mut *mesh,
            "matgraph" => match matgraph.as_mut() {
                Some(scene) => scene,
                // No editor yet: the published settings already carry the patch.
                None => continue,
            },
            _ => &mut *texture,
        };
        // The agent validated the patch; a failure here would be a stale pending entry.
        let _ = target.settings.patch(&patch);
    }
    for panel in [&mut *material, &mut *mesh, &mut *texture] {
        panel.level = level;
    }
    scenes.material = material.settings;
    scenes.mesh = mesh.settings;
    scenes.texture = texture.settings;
    if let Some(scene) = matgraph.as_mut() {
        scene.level = level;
        scenes.matgraph = scene.settings;
    }
    scenes.level = level;
    if let Some(scene) = matgraph
        && let Some(mut editor) = world.get_resource_mut::<crate::matgraph_editor::MatGraphEditor>()
        && editor.preview_scene != scene
    {
        editor.preview_scene = scene;
    }
}

/// `editor.preview_scene`.
fn preview_scene(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let editor = params.get("editor").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params("`editor` is required (material, mesh, texture, matgraph)")
    })?;
    if !EDITORS.contains(&editor) {
        return Err(RpcError::invalid_params(format!(
            "unknown editor `{editor}` (material, mesh, texture, matgraph)"
        )));
    }
    if editor != "texture" && params.get("shape").is_some_and(|v| !v.is_null()) {
        return Err(RpcError::invalid_params(
            "`shape` applies to the texture editor only (flat, sphere, plane)",
        ));
    }
    let has_patch = ["preset", "intensity", "floor", "rotate", "shape"]
        .iter()
        .any(|k| params.get(*k).is_some_and(|v| !v.is_null()));
    let level = level_lighting(world);
    let mut scenes = world.get_resource_or_init::<PreviewScenes>();
    let mut settings = scenes
        .settings(editor)
        .copied()
        .ok_or_else(|| RpcError::invalid_params("unknown editor"))?;
    if has_patch {
        settings.patch(params)?;
        if let Some(slot) = scenes.settings_mut(editor) {
            *slot = settings;
        }
        scenes.pending.push((editor.to_string(), params.clone()));
    }
    let effective = if settings.lighting == PreviewLighting::Level && level.is_none() {
        PreviewLighting::Outdoor
    } else {
        settings.lighting
    };
    let mut out = settings.to_json();
    out["editor"] = json!(editor);
    out["effective_preset"] = json!(effective.name());
    out["level_lighting"] = json!(level.is_some());
    Ok(out)
}

/// The agent methods of the preview scene.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        MethodSchema {
            name: "editor.preview_scene",
            tool: "preview_scene",
            description: "Read or change the preview scene of an asset editor (the Material editor, \
                the material graph editor and the mesh and texture viewers): the lighting preset, the environment intensity, the \
                floor with its grid and the auto-rotate turntable. With only `editor` it reads. The \
                panels take a change on their next frame. `level` falls back to `outdoor` when the \
                open level has no directional light (`effective_preset` says which is drawn).",
            params: &[
                ParamSchema {
                    name: "editor",
                    kind: "string",
                    required: true,
                    description: "material, mesh, texture or matgraph (the material graph editor).",
                },
                ParamSchema {
                    name: "preset",
                    kind: "string",
                    required: false,
                    description: "Lighting preset: outdoor, studio, neutral or level.",
                },
                ParamSchema {
                    name: "intensity",
                    kind: "number",
                    required: false,
                    description: "Environment intensity, 0 to 4 (1 is the preset's own ambient light).",
                },
                ParamSchema {
                    name: "floor",
                    kind: "boolean",
                    required: false,
                    description: "Draw the floor with its grid.",
                },
                ParamSchema {
                    name: "rotate",
                    kind: "boolean",
                    required: false,
                    description: "Turn the subject on a turntable.",
                },
                ParamSchema {
                    name: "shape",
                    kind: "string",
                    required: false,
                    description: "Texture editor only: flat (the 2D image), sphere or plane (the texture as the albedo of a lit shape).",
                },
            ],
            result: "{ editor, preset, intensity, floor, rotate, shape, effective_preset, level_lighting }",
            mutating: true,
            permission: Permission::EditScene,
        },
        preview_scene as Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUN: SunBase = SunBase {
        direction: [0.35, -0.85, 0.4],
        color: [1.0, 1.0, 1.0],
    };

    /// A rig's key light as comparable numbers (the uniform has no `PartialEq`).
    fn sun_of(rig: &PreviewRig) -> ([f32; 3], [f32; 3], f32) {
        sun_of_uniform(&rig.directional)
    }

    fn sun_of_uniform(d: &DirectionalLightUniform) -> ([f32; 3], [f32; 3], f32) {
        (d.direction, d.color, d.illuminance)
    }

    #[test]
    fn the_default_settings_are_the_historical_outdoor_preview() {
        let rig = PreviewSceneSettings::default().rig(SUN, 1.0, None);
        let want = DirectionalLightUniform::new(SUN.direction, SUN.color, 100_000.0);
        assert_eq!(sun_of(&rig), sun_of_uniform(&want));
        assert_eq!(rig.ambient_lux, AMBIENT_LUX);
        assert!(rig.ibl && rig.local_lights.is_empty() && !rig.fallback);
        assert_eq!(rig.environment, EnvLight::Default, "the renderer's own sky");
        assert_eq!(rig.backdrop, Backdrop::Sky);
    }

    #[test]
    fn each_preset_has_its_own_rig() {
        let mut s = PreviewSceneSettings {
            lighting: PreviewLighting::Studio,
            ..Default::default()
        };
        let studio = s.rig(SUN, 1.0, None);
        assert_eq!(studio.local_lights.len(), 2, "fill and rim");
        assert!(matches!(studio.environment, EnvLight::Gradient(_)));
        assert!(matches!(studio.backdrop, Backdrop::Flat(_)));
        s.lighting = PreviewLighting::Neutral;
        let neutral = s.rig(SUN, 1.0, None);
        assert!(neutral.local_lights.is_empty());
        let EnvLight::Gradient(flat) = neutral.environment else {
            panic!("neutral lights from a gradient environment")
        };
        assert!(
            flat.zenith == flat.horizon && flat.horizon == flat.ground,
            "uniform gray: {flat:?}"
        );
        assert!(neutral.directional.illuminance < studio.directional.illuminance + 40_000.0);
        assert_ne!(sun_of(&neutral), sun_of(&studio));
    }

    /// The brightest radiance of a rig's environment (0 for the renderer's own sky).
    fn env_peak(rig: &PreviewRig) -> f32 {
        match rig.environment {
            EnvLight::Default => 0.0,
            EnvLight::Gradient(g) => g
                .zenith
                .iter()
                .chain(&g.horizon)
                .chain(&g.ground)
                .copied()
                .fold(0.0, f32::max),
        }
    }

    #[test]
    fn environment_intensity_scales_every_preset_s_environment_and_is_clamped() {
        for lighting in PreviewLighting::ALL {
            let mut s = PreviewSceneSettings {
                lighting,
                ..Default::default()
            };
            s.env_intensity = 2.0;
            let two = env_peak(&s.rig(SUN, 1.0, None));
            s.env_intensity = 4.0;
            let four = env_peak(&s.rig(SUN, 1.0, None));
            assert!(
                two > 0.0 && (four - 2.0 * two).abs() < 1e-2 * four,
                "{lighting:?}: {two} {four}"
            );
            s.env_intensity = 0.0;
            assert_eq!(
                env_peak(&s.rig(SUN, 1.0, None)),
                0.0,
                "{lighting:?}: intensity 0 is a black environment"
            );
            assert!(
                matches!(s.rig(SUN, 1.0, None).environment, EnvLight::Gradient(_)),
                "{lighting:?}: 0 is not the default sky"
            );
            s.env_intensity = 99.0;
            assert_eq!(s.clamped_intensity(), ENV_INTENSITY_MAX);
            s.env_intensity = f32::NAN;
            assert_eq!(s.clamped_intensity(), 1.0);
        }
        // The sky presets keep the renderer's sky at intensity 1 (the picture the previews always drew).
        for lighting in [PreviewLighting::Outdoor, PreviewLighting::Level] {
            let s = PreviewSceneSettings {
                lighting,
                ..Default::default()
            };
            assert_eq!(
                s.rig(SUN, 1.0, None).environment,
                EnvLight::Default,
                "{lighting:?}"
            );
        }
        // Every gradient is one the renderer accepts.
        for lighting in PreviewLighting::ALL {
            for intensity in [0.0, 0.5, 1.5, 4.0] {
                let s = PreviewSceneSettings {
                    lighting,
                    env_intensity: intensity,
                    ..Default::default()
                };
                if let EnvLight::Gradient(g) = s.rig(SUN, 1.0, None).environment {
                    g.validate()
                        .unwrap_or_else(|e| panic!("{lighting:?} {intensity}: {e}"));
                }
            }
        }
    }

    #[test]
    fn the_level_preset_reads_the_level_sun_and_falls_back_without_one() {
        let level = LevelLighting {
            direction: [0.0, -1.0, 0.0],
            color: [1.0, 0.5, 0.25],
            illuminance: 64_000.0,
        };
        let s = PreviewSceneSettings {
            lighting: PreviewLighting::Level,
            ..Default::default()
        };
        let rig = s.rig(SUN, 1.0, Some(&level));
        assert_eq!(rig.directional.illuminance, 64_000.0);
        assert_eq!(rig.directional.color, [1.0, 0.5, 0.25]);
        assert!(!rig.fallback);
        let fallback = s.rig(SUN, 1.0, None);
        assert!(fallback.fallback);
        assert_eq!(fallback.directional.illuminance, 100_000.0);
    }

    #[test]
    fn the_level_sun_is_the_light_the_renderer_picks_as_its_sun() {
        let mut world = World::new();
        assert_eq!(level_lighting(&mut world), None);
        let light = |lux| DirectionalLight {
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            illuminance: lux,
            cast_shadows: false,
            static_shadow: false,
        };
        let a = world.spawn(light(11_000.0)).id();
        let b = world.spawn(light(22_000.0)).id();
        // The renderer's sun is the lowest entity bits; so is the preview's.
        let want = if a.to_bits() < b.to_bits() {
            11_000.0
        } else {
            22_000.0
        };
        assert_eq!(level_lighting(&mut world).map(|l| l.illuminance), Some(want));
    }

    #[test]
    fn point_light_lumens_give_the_wanted_lux_at_the_center() {
        let lumens = lumens_for_lux(10_000.0, 2.0);
        let candela = lumens / (4.0 * std::f32::consts::PI);
        assert!((candela / 4.0 - 10_000.0).abs() < 0.5);
        // Studio lights scale with the subject: a larger subject gets stronger, farther lights.
        let s = PreviewSceneSettings {
            lighting: PreviewLighting::Studio,
            ..Default::default()
        };
        let power = |radius: f32| match s.rig(SUN, radius, None).local_lights[0] {
            LabLocalLight::Point(p) => p.intensity,
            LabLocalLight::Spot(_) => unreachable!("studio uses point lights"),
        };
        assert!(power(2.0) > power(1.0) * 3.9);
    }

    #[test]
    fn the_turntable_advances_one_turn_in_twelve_seconds() {
        let mut spin = PreviewSpin::default();
        for _ in 0..12 {
            spin.advance_by(1.0);
        }
        assert!(
            spin.angle < 1e-3 || (std::f32::consts::TAU - spin.angle) < 1e-3,
            "{}",
            spin.angle
        );
        spin.advance_by(f32::NAN);
        spin.advance_by(-1.0);
        let steady = spin;
        spin.advance_by(0.0);
        assert_eq!(spin, steady);
        let on = PreviewSceneSettings {
            auto_rotate: true,
            ..Default::default()
        };
        let off = PreviewSceneSettings::default();
        let mut turned = PreviewSpin::default();
        turned.advance_by(3.0);
        assert_eq!(turned.model(&off, Vec3::ZERO), Mat4::IDENTITY);
        assert_ne!(turned.model(&on, Vec3::ZERO), Mat4::IDENTITY);
        assert_eq!(turned.key(&off), 0, "a steady preview keeps one key");
        assert_ne!(turned.key(&on), 0);
        // A turn about a center keeps the center fixed.
        let center = Vec3::new(1.0, 2.0, 3.0);
        let moved = turned.model(&on, center).transform_point3(center);
        assert!((moved - center).length() < 1e-5);
    }

    #[test]
    fn the_spin_ticks_once_per_egui_frame_even_when_the_ui_runs_twice() {
        let ctx = egui::Context::default();
        let on = PreviewSceneSettings {
            auto_rotate: true,
            ..Default::default()
        };
        let mut spin = PreviewSpin::default();
        let mut per_frame = Vec::new();
        for frame in 0..4u32 {
            let raw = egui::RawInput {
                time: Some(f64::from(frame) * 0.05),
                predicted_dt: 0.05,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                // The same frame's UI run twice: the second tick must not turn it again.
                let before = spin.angle;
                spin.tick(ui.ctx(), &on);
                let once = spin.angle;
                spin.tick(ui.ctx(), &on);
                per_frame.push((before, once, spin.angle));
            });
            out.textures_delta.clear();
        }
        for (before, once, twice) in &per_frame {
            assert!(once >= before);
            assert_eq!(once, twice, "one advance per frame number");
        }
        assert!(spin.angle > 0.0, "the later frames turned it: {per_frame:?}");
        // Off: no tick at all.
        let mut still = PreviewSpin::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            still.tick(ui.ctx(), &PreviewSceneSettings::default());
        });
        out.textures_delta.clear();
        assert_eq!(still.angle, 0.0);
    }

    #[test]
    fn the_floor_is_a_unit_quad_facing_up_with_a_grid_above_it() {
        let ((fv, fi), (gv, gi)) = floor_geometry(FLOOR_CELLS);
        assert_eq!((fv.len(), fi.len()), (4, 6));
        assert_eq!(gv.len(), (FLOOR_CELLS + 1) * 2 * 4);
        assert_eq!(gi.len(), (FLOOR_CELLS + 1) * 2 * 6);
        assert!(
            fv.iter()
                .all(|v| v.position[1] == 0.0 && v.normal == [0.0, 1.0, 0.0])
        );
        assert!(gv.iter().all(|v| v.position[1] > 0.0 && v.position[1] < 0.01));
        for tri in fi.chunks(3).chain(gi.chunks(3)) {
            assert_eq!(tri.len(), 3);
        }
        assert!(fi.iter().all(|i| (*i as usize) < fv.len()));
        assert!(gi.iter().all(|i| (*i as usize) < gv.len()));
        // Winding faces up: the first triangle's geometric normal is +Y.
        let p = |i: u32| Vec3::from_array(fv[i as usize].position);
        let n = (p(fi[1]) - p(fi[0])).cross(p(fi[2]) - p(fi[0]));
        assert!(n.y > 0.0, "{n:?}");
        assert_eq!(floor_geometry(FLOOR_CELLS).1.0.len(), gv.len(), "deterministic");
    }

    #[test]
    fn the_floor_sits_at_the_subjects_lowest_point_and_spans_past_it() {
        let (y, half) = floor_placement(-1.0, 1.0);
        assert!(y < -1.0 && y > -1.01);
        assert_eq!(half, 3.0);
        assert_eq!(floor_placement(f32::NAN, f32::NAN), (0.0, 3.0));
    }

    #[test]
    fn push_floor_adds_the_floor_and_the_grid() {
        let floor = PreviewFloor::default();
        let mut meshes = Vec::new();
        push_floor(&mut meshes, &floor, -1.0, 3.0);
        assert_eq!(meshes.len(), 2);
        assert_ne!(meshes[0].mesh_key, meshes[1].mesh_key);
        // The model scales the unit floor to the half extent and moves it to the floor height.
        let model = Mat4::from_cols_array_2d(&meshes[0].model);
        let corner = model.transform_point3(Vec3::new(1.0, 0.0, 1.0));
        assert!((corner - Vec3::new(3.0, -1.0, 3.0)).length() < 1e-5);
    }

    #[test]
    fn a_patch_changes_only_the_named_fields_and_refuses_bad_values() {
        let mut s = PreviewSceneSettings::default();
        s.patch(&json!({ "preset": "studio", "floor": true }))
            .expect("patch");
        assert_eq!(s.lighting, PreviewLighting::Studio);
        assert!(s.floor && !s.auto_rotate && s.env_intensity == 1.0);
        s.patch(&json!({ "shape": "sphere" })).expect("shape");
        assert_eq!(s.surface, TextureSurface::Sphere);
        let keep = s;
        for bad in [
            json!({ "shape": "torus" }),
            json!({ "shape": 3 }),
            json!({ "preset": "moon" }),
            json!({ "preset": 3 }),
            json!({ "intensity": 9.0 }),
            json!({ "intensity": "a" }),
            json!({ "floor": "yes" }),
            json!({ "rotate": 1 }),
        ] {
            assert!(s.patch(&bad).is_err(), "{bad}");
            assert_eq!(s, keep, "a refused patch changes nothing: {bad}");
        }
    }

    /// The dispatcher path: the agent method reads, patches, and the shell's sync hands the patch
    /// to the panel's own settings.
    #[test]
    fn the_agent_patch_reaches_the_panels_through_the_sync() {
        let mut world = World::new();
        let call = |world: &mut World, params: Value| preview_scene(world, &params);
        let read = call(&mut world, json!({ "editor": "mesh" })).expect("read");
        assert_eq!(read["preset"], "outdoor");
        assert_eq!(read["effective_preset"], "outdoor");
        let set = call(
            &mut world,
            json!({ "editor": "mesh", "preset": "level", "floor": true, "rotate": true, "intensity": 2.5 }),
        )
        .expect("set");
        assert_eq!(set["preset"], "level");
        assert_eq!(
            set["effective_preset"], "outdoor",
            "no level light yet: the fallback is named"
        );
        assert_eq!(set["level_lighting"], false);
        // The texture editor takes a shape.
        let tex = call(&mut world, json!({ "editor": "texture", "shape": "sphere" })).expect("texture shape");
        assert_eq!(tex["shape"], "sphere");
        // The material editor's settings are separate.
        assert_eq!(
            call(&mut world, json!({ "editor": "material" })).expect("m")["floor"],
            false
        );
        let (mut material, mut mesh, mut texture) =
            <(PreviewContext, PreviewContext, PreviewContext)>::default();
        sync(&mut world, &mut material, &mut mesh, &mut texture);
        assert_eq!(mesh.settings.lighting, PreviewLighting::Level);
        assert!(mesh.settings.floor && mesh.settings.auto_rotate && mesh.settings.env_intensity == 2.5);
        assert_eq!(material.settings, PreviewSceneSettings::default());
        // A second sync has nothing pending, and the panel's own edit is published.
        mesh.settings.floor = false;
        sync(&mut world, &mut material, &mut mesh, &mut texture);
        assert!(
            !call(&mut world, json!({ "editor": "mesh" })).expect("read")["floor"]
                .as_bool()
                .expect("bool")
        );
        // A level sun turns the fallback off.
        world.spawn(DirectionalLight {
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            illuminance: 50_000.0,
            cast_shadows: false,
            static_shadow: false,
        });
        let again = call(&mut world, json!({ "editor": "mesh" })).expect("read");
        assert_eq!(again["effective_preset"], "level");
        assert_eq!(again["level_lighting"], true);
    }

    #[test]
    fn the_agent_method_refuses_bad_calls() {
        let mut world = World::new();
        for bad in [
            json!({}),
            json!({ "editor": "sound" }),
            json!({ "editor": "mesh", "preset": "nope" }),
            json!({ "editor": "mesh", "intensity": 5 }),
            json!({ "editor": "mesh", "shape": "sphere" }),
            json!({ "editor": "texture", "shape": "cone" }),
        ] {
            assert!(preview_scene(&mut world, &bad).is_err(), "{bad}");
        }
        assert!(
            world
                .get_resource::<PreviewScenes>()
                .is_none_or(|s| s.pending.is_empty()),
            "a refused call queues nothing"
        );
    }

    /// Every control of the scene is a focusable widget with a name: Tab reaches each, Space
    /// toggles the floor and the turntable, and no interactive node is unnamed.
    #[test]
    fn the_controls_are_keyboard_reachable_and_named() {
        use crate::canvas_keys::test_input::{frame, key, tab_to};
        let ctx = egui::Context::default();
        let settings = std::cell::RefCell::new(PreviewSceneSettings::default());
        let size = [640.0, 200.0];
        let mut draw = |ui: &mut egui::Ui| {
            preview_scene_controls(ui, &mut settings.borrow_mut());
        };
        let first = frame(&ctx, size, vec![], &mut draw);
        assert_eq!(crate::a11y_audit::unnamed_widgets(&first), Vec::<String>::new());
        let names: Vec<String> = crate::a11y_audit::widget_names(&first)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        for wanted in ["Preview lighting", "Environment intensity", "Floor", "Rotate"] {
            assert!(
                names.iter().any(|n| n.contains(wanted)),
                "no control named {wanted}: {names:?}"
            );
        }
        for (name, floor, rotate) in [("Floor", true, false), ("Rotate", true, true)] {
            tab_to(&ctx, size, name, false, 12, &mut draw);
            frame(
                &ctx,
                size,
                vec![key(egui::Key::Space, egui::Modifiers::NONE)],
                &mut draw,
            );
            frame(&ctx, size, vec![], &mut draw);
            let now = *settings.borrow();
            assert_eq!((now.floor, now.auto_rotate), (floor, rotate), "Space on {name}");
        }
    }
}
