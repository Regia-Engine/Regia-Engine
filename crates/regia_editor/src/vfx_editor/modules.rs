// SPDX-License-Identifier: MPL-2.0
//! The module stack of one emitter: which modules of `.vfx.ron` v7 an emitter uses, their
//! parameters, and the stash that holds a disabled module (ADR-3171).
//!
//! `.vfx.ron` has no module list: an emitter is a fixed Spawn, Init, Update and Render struct, and
//! a module is "present" when its fields leave their defaults (an `Option` that is `Some`, a curve
//! with keys, a non-zero gravity, ...). [`ModuleKind`] names each such group; [`ModuleData`] holds a
//! group's values, so taking a module out ([`take`]) and putting it back ([`put`]) is exact.

use regia_vfx::format::{
    AttractorDef, BlendMode, ColorKey, DepthCollisionDef, EmitterDef, EventSpawnDef, FlipbookDef,
    FlipbookMode, FloatKey, KillShape, KillSide, KillVolumeDef, ParticleLightDef, RangeDef, RenderMode,
    ShapeDef, SpawnMode, SpriteId, VelocityMode, VortexDef,
};
use serde::{Deserialize, Serialize};

/// The stage a module belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Spawn: counts over time.
    Spawn,
    /// Init: shape and starting attributes.
    Init,
    /// Update: forces, curves over life, kills.
    Update,
    /// Render: how particles draw.
    Render,
}

impl Stage {
    /// The protocol name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Init => "init",
            Self::Update => "update",
            Self::Render => "render",
        }
    }
}

/// Every module kind the editor knows, in stack order (stage order, then the order of the fields in
/// `docs/formats/vfx-ron.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModuleKind {
    /// The spawn module (required).
    Spawn,
    /// The init module: shape, velocity, size, rotation, spin (required).
    Init,
    /// The initial colour gradient (`init.color`).
    InitColor,
    /// Constant acceleration (`update.gravity`).
    Gravity,
    /// Velocity damping (`update.drag`).
    Drag,
    /// Curl-noise turbulence (`update.curl`).
    Curl,
    /// Vortex (`update.vortex`).
    Vortex,
    /// Point attractor (`update.attractor`).
    Attractor,
    /// Scene wind coupling (`update.wind_response`, `update.wind_drag`).
    Wind,
    /// Size over life (`update.size_over_life`).
    SizeOverLife,
    /// Colour over life (`update.color_over_life`).
    ColorOverLife,
    /// Alpha over life (`update.alpha_over_life`).
    AlphaOverLife,
    /// Rotation over life (`update.rotation_over_life`).
    RotationOverLife,
    /// World-Y kill plane (`update.kill_below`).
    KillBelow,
    /// Box or sphere kill volume (`update.kill_volume`).
    KillVolume,
    /// Depth-buffer collision (`update.depth_collision`).
    DepthCollision,
    /// Child event spawns (`update.events`).
    Events,
    /// The render module (required).
    Render,
    /// Flipbook animation (`render.flipbook`).
    Flipbook,
    /// Soft-particle depth fade (`render.softness`).
    SoftParticles,
    /// Particle light (`render.light`).
    Light,
    /// Heat haze (`render.heat_haze`).
    HeatHaze,
}

/// Every kind, in stack order.
pub const ALL_KINDS: [ModuleKind; 22] = [
    ModuleKind::Spawn,
    ModuleKind::Init,
    ModuleKind::InitColor,
    ModuleKind::Gravity,
    ModuleKind::Drag,
    ModuleKind::Curl,
    ModuleKind::Vortex,
    ModuleKind::Attractor,
    ModuleKind::Wind,
    ModuleKind::SizeOverLife,
    ModuleKind::ColorOverLife,
    ModuleKind::AlphaOverLife,
    ModuleKind::RotationOverLife,
    ModuleKind::KillBelow,
    ModuleKind::KillVolume,
    ModuleKind::DepthCollision,
    ModuleKind::Events,
    ModuleKind::Render,
    ModuleKind::Flipbook,
    ModuleKind::SoftParticles,
    ModuleKind::Light,
    ModuleKind::HeatHaze,
];

impl ModuleKind {
    /// The protocol name (`size_over_life`, ...).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Init => "init",
            Self::InitColor => "init_color",
            Self::Gravity => "gravity",
            Self::Drag => "drag",
            Self::Curl => "curl",
            Self::Vortex => "vortex",
            Self::Attractor => "attractor",
            Self::Wind => "wind",
            Self::SizeOverLife => "size_over_life",
            Self::ColorOverLife => "color_over_life",
            Self::AlphaOverLife => "alpha_over_life",
            Self::RotationOverLife => "rotation_over_life",
            Self::KillBelow => "kill_below",
            Self::KillVolume => "kill_volume",
            Self::DepthCollision => "depth_collision",
            Self::Events => "events",
            Self::Render => "render",
            Self::Flipbook => "flipbook",
            Self::SoftParticles => "soft_particles",
            Self::Light => "light",
            Self::HeatHaze => "heat_haze",
        }
    }

    /// The kind called `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        ALL_KINDS.into_iter().find(|k| k.name() == name)
    }

    /// The stage.
    #[must_use]
    pub fn stage(self) -> Stage {
        match self {
            Self::Spawn => Stage::Spawn,
            Self::Init | Self::InitColor => Stage::Init,
            Self::Render | Self::Flipbook | Self::SoftParticles | Self::Light | Self::HeatHaze => {
                Stage::Render
            }
            _ => Stage::Update,
        }
    }

    /// Spawn, Init and Render: every emitter has them; they cannot be removed or disabled.
    #[must_use]
    pub fn required(self) -> bool {
        matches!(self, Self::Spawn | Self::Init | Self::Render)
    }

    /// Whether the palette can add it. Events need a child emitter, so they are edited in the file.
    #[must_use]
    pub fn addable(self) -> bool {
        !self.required() && self != Self::Events
    }
}

/// The kinds the palette offers.
#[must_use]
pub fn palette() -> Vec<ModuleKind> {
    ALL_KINDS.into_iter().filter(|k| k.addable()).collect()
}

/// Whether `kind` is present in `e` (required kinds always are).
#[must_use]
pub fn present(e: &EmitterDef, kind: ModuleKind) -> bool {
    let u = &e.update;
    let r = &e.render;
    match kind {
        ModuleKind::Spawn | ModuleKind::Init | ModuleKind::Render => true,
        ModuleKind::InitColor => !e.init.color.is_empty(),
        ModuleKind::Gravity => u.gravity != (0.0, 0.0, 0.0),
        ModuleKind::Drag => u.drag != 0.0,
        ModuleKind::Curl => u.curl.is_some(),
        ModuleKind::Vortex => u.vortex.is_some(),
        ModuleKind::Attractor => u.attractor.is_some(),
        ModuleKind::Wind => u.wind_response != 0.0,
        ModuleKind::SizeOverLife => !u.size_over_life.is_empty(),
        ModuleKind::ColorOverLife => !u.color_over_life.is_empty(),
        ModuleKind::AlphaOverLife => !u.alpha_over_life.is_empty(),
        ModuleKind::RotationOverLife => !u.rotation_over_life.is_empty(),
        ModuleKind::KillBelow => u.kill_below.is_some(),
        ModuleKind::KillVolume => u.kill_volume.is_some(),
        ModuleKind::DepthCollision => u.depth_collision.is_some(),
        ModuleKind::Events => !u.events.is_empty(),
        ModuleKind::Flipbook => r.flipbook.is_some(),
        ModuleKind::SoftParticles => r.softness != 0.0,
        ModuleKind::Light => r.light.is_some(),
        ModuleKind::HeatHaze => r.heat_haze != 0.0,
    }
}

/// The modules present in `e`, in stack order.
#[must_use]
pub fn present_kinds(e: &EmitterDef) -> Vec<ModuleKind> {
    ALL_KINDS.into_iter().filter(|k| present(e, *k)).collect()
}

/// The values of one optional module: what [`take`] removes and [`put`] restores. Serializable
/// for the editor-only autosave envelope (the `.vfx.ron` format itself is unchanged).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ModuleData {
    /// `init.color`.
    InitColor(Vec<ColorKey>),
    /// `update.gravity`.
    Gravity((f32, f32, f32)),
    /// `update.drag`.
    Drag(f32),
    /// `update.curl`.
    Curl((f32, f32, f32)),
    /// `update.vortex`.
    Vortex(VortexDef),
    /// `update.attractor`.
    Attractor(AttractorDef),
    /// `update.wind_response` and `update.wind_drag`.
    Wind(f32, f32),
    /// `update.size_over_life`.
    SizeOverLife(Vec<FloatKey>),
    /// `update.color_over_life`.
    ColorOverLife(Vec<ColorKey>),
    /// `update.alpha_over_life`.
    AlphaOverLife(Vec<FloatKey>),
    /// `update.rotation_over_life`.
    RotationOverLife(Vec<FloatKey>),
    /// `update.kill_below`.
    KillBelow(f32),
    /// `update.kill_volume`.
    KillVolume(KillVolumeDef),
    /// `update.depth_collision`.
    DepthCollision(DepthCollisionDef),
    /// `update.events`.
    Events(Vec<EventSpawnDef>),
    /// `render.flipbook`.
    Flipbook(FlipbookDef),
    /// `render.softness`.
    SoftParticles(f32),
    /// `render.light`.
    Light(ParticleLightDef),
    /// `render.heat_haze`.
    HeatHaze(f32),
}

impl ModuleData {
    /// The kind these values belong to.
    #[must_use]
    pub fn kind(&self) -> ModuleKind {
        match self {
            Self::InitColor(_) => ModuleKind::InitColor,
            Self::Gravity(_) => ModuleKind::Gravity,
            Self::Drag(_) => ModuleKind::Drag,
            Self::Curl(_) => ModuleKind::Curl,
            Self::Vortex(_) => ModuleKind::Vortex,
            Self::Attractor(_) => ModuleKind::Attractor,
            Self::Wind(..) => ModuleKind::Wind,
            Self::SizeOverLife(_) => ModuleKind::SizeOverLife,
            Self::ColorOverLife(_) => ModuleKind::ColorOverLife,
            Self::AlphaOverLife(_) => ModuleKind::AlphaOverLife,
            Self::RotationOverLife(_) => ModuleKind::RotationOverLife,
            Self::KillBelow(_) => ModuleKind::KillBelow,
            Self::KillVolume(_) => ModuleKind::KillVolume,
            Self::DepthCollision(_) => ModuleKind::DepthCollision,
            Self::Events(_) => ModuleKind::Events,
            Self::Flipbook(_) => ModuleKind::Flipbook,
            Self::SoftParticles(_) => ModuleKind::SoftParticles,
            Self::Light(_) => ModuleKind::Light,
            Self::HeatHaze(_) => ModuleKind::HeatHaze,
        }
    }

    /// The values a freshly added module of `kind` starts with (`None` for required kinds and
    /// events). Each is a visible, valid starting point, not the format's "off" default.
    #[must_use]
    pub fn default_for(kind: ModuleKind) -> Option<Self> {
        let k = |t, v| FloatKey { t, v };
        let c = |t, color: &str| ColorKey {
            t,
            color: color.to_owned(),
        };
        Some(match kind {
            ModuleKind::InitColor => Self::InitColor(vec![c(0.0, "#ffffff")]),
            ModuleKind::Gravity => Self::Gravity((0.0, -9.8, 0.0)),
            ModuleKind::Drag => Self::Drag(1.0),
            ModuleKind::Curl => Self::Curl((1.0, 0.5, 1.0)),
            ModuleKind::Vortex => Self::Vortex(VortexDef {
                axis: (0.0, 1.0, 0.0),
                strength: 2.0,
                radial: 0.0,
            }),
            ModuleKind::Attractor => Self::Attractor(AttractorDef {
                position: (0.0, 1.0, 0.0),
                strength: 4.0,
            }),
            ModuleKind::Wind => Self::Wind(0.5, 1.0),
            ModuleKind::SizeOverLife => Self::SizeOverLife(vec![k(0.0, 0.5), k(1.0, 1.5)]),
            ModuleKind::ColorOverLife => Self::ColorOverLife(vec![c(0.0, "#ffffff"), c(1.0, "#808080")]),
            ModuleKind::AlphaOverLife => Self::AlphaOverLife(vec![k(0.0, 1.0), k(1.0, 0.0)]),
            ModuleKind::RotationOverLife => Self::RotationOverLife(vec![k(0.0, 0.0), k(1.0, 1.0)]),
            ModuleKind::KillBelow => Self::KillBelow(0.0),
            ModuleKind::KillVolume => Self::KillVolume(KillVolumeDef {
                shape: KillShape::Sphere { radius: 5.0 },
                center: (0.0, 0.0, 0.0),
                kill: KillSide::Outside,
            }),
            ModuleKind::DepthCollision => Self::DepthCollision(DepthCollisionDef {
                kill: true,
                offset: 1.0,
            }),
            ModuleKind::Flipbook => Self::Flipbook(FlipbookDef {
                cols: 4,
                rows: 4,
                frames: 16,
                mode: FlipbookMode::OverLife,
                random_start: false,
                blend_frames: false,
            }),
            ModuleKind::SoftParticles => Self::SoftParticles(0.5),
            ModuleKind::Light => Self::Light(ParticleLightDef {
                color: "#ffb060".to_owned(),
                intensity: 2.0,
                radius: 3.0,
            }),
            ModuleKind::HeatHaze => Self::HeatHaze(4.0),
            ModuleKind::Spawn | ModuleKind::Init | ModuleKind::Render | ModuleKind::Events => return None,
        })
    }
}

/// Removes the module `kind` from `e`, leaving the format's default in its place, and returns its
/// values. `None` when it is absent or required.
pub fn take(e: &mut EmitterDef, kind: ModuleKind) -> Option<ModuleData> {
    if kind.required() || !present(e, kind) {
        return None;
    }
    let u = &mut e.update;
    let r = &mut e.render;
    Some(match kind {
        ModuleKind::InitColor => ModuleData::InitColor(std::mem::take(&mut e.init.color)),
        ModuleKind::Gravity => ModuleData::Gravity(std::mem::replace(&mut u.gravity, (0.0, 0.0, 0.0))),
        ModuleKind::Drag => ModuleData::Drag(std::mem::replace(&mut u.drag, 0.0)),
        ModuleKind::Curl => ModuleData::Curl(u.curl.take()?),
        ModuleKind::Vortex => ModuleData::Vortex(u.vortex.take()?),
        ModuleKind::Attractor => ModuleData::Attractor(u.attractor.take()?),
        ModuleKind::Wind => {
            let data = ModuleData::Wind(u.wind_response, u.wind_drag);
            u.wind_response = 0.0;
            u.wind_drag = 1.0;
            data
        }
        ModuleKind::SizeOverLife => ModuleData::SizeOverLife(std::mem::take(&mut u.size_over_life)),
        ModuleKind::ColorOverLife => ModuleData::ColorOverLife(std::mem::take(&mut u.color_over_life)),
        ModuleKind::AlphaOverLife => ModuleData::AlphaOverLife(std::mem::take(&mut u.alpha_over_life)),
        ModuleKind::RotationOverLife => {
            ModuleData::RotationOverLife(std::mem::take(&mut u.rotation_over_life))
        }
        ModuleKind::KillBelow => ModuleData::KillBelow(u.kill_below.take()?),
        ModuleKind::KillVolume => ModuleData::KillVolume(u.kill_volume.take()?),
        ModuleKind::DepthCollision => ModuleData::DepthCollision(u.depth_collision.take()?),
        ModuleKind::Events => ModuleData::Events(std::mem::take(&mut u.events)),
        ModuleKind::Flipbook => ModuleData::Flipbook(r.flipbook.take()?),
        ModuleKind::SoftParticles => ModuleData::SoftParticles(std::mem::replace(&mut r.softness, 0.0)),
        ModuleKind::Light => ModuleData::Light(r.light.take()?),
        ModuleKind::HeatHaze => ModuleData::HeatHaze(std::mem::replace(&mut r.heat_haze, 0.0)),
        ModuleKind::Spawn | ModuleKind::Init | ModuleKind::Render => return None,
    })
}

/// Writes `data` into `e` (replacing whatever that module held).
pub fn put(e: &mut EmitterDef, data: ModuleData) {
    let u = &mut e.update;
    let r = &mut e.render;
    match data {
        ModuleData::InitColor(v) => e.init.color = v,
        ModuleData::Gravity(v) => u.gravity = v,
        ModuleData::Drag(v) => u.drag = v,
        ModuleData::Curl(v) => u.curl = Some(v),
        ModuleData::Vortex(v) => u.vortex = Some(v),
        ModuleData::Attractor(v) => u.attractor = Some(v),
        ModuleData::Wind(response, drag) => {
            u.wind_response = response;
            u.wind_drag = drag;
        }
        ModuleData::SizeOverLife(v) => u.size_over_life = v,
        ModuleData::ColorOverLife(v) => u.color_over_life = v,
        ModuleData::AlphaOverLife(v) => u.alpha_over_life = v,
        ModuleData::RotationOverLife(v) => u.rotation_over_life = v,
        ModuleData::KillBelow(v) => u.kill_below = Some(v),
        ModuleData::KillVolume(v) => u.kill_volume = Some(v),
        ModuleData::DepthCollision(v) => u.depth_collision = Some(v),
        ModuleData::Events(v) => u.events = v,
        ModuleData::Flipbook(v) => r.flipbook = Some(v),
        ModuleData::SoftParticles(v) => r.softness = v,
        ModuleData::Light(v) => r.light = Some(v),
        ModuleData::HeatHaze(v) => r.heat_haze = v,
    }
}

// ------------------------------------------------------------------------------------- parameters

/// A parameter value as a widget or the agent writes it.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamValue {
    /// A number.
    Num(f32),
    /// A switch.
    Bool(bool),
    /// A choice or a text (`"alpha"`, `"#ff8040"`).
    Text(String),
}

impl ParamValue {
    /// As JSON.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Num(v) => serde_json::json!(v),
            Self::Bool(v) => serde_json::json!(v),
            Self::Text(v) => serde_json::json!(v),
        }
    }
}

/// How a parameter is edited.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamKind {
    /// A number in `min..=max` (inclusive; the editor refuses anything outside).
    Num {
        /// Lowest accepted value.
        min: f32,
        /// Highest accepted value.
        max: f32,
    },
    /// An integer in `min..=max`.
    Int {
        /// Lowest accepted value.
        min: u32,
        /// Highest accepted value.
        max: u32,
    },
    /// A switch.
    Bool,
    /// One of a fixed list of names.
    Choice(&'static [&'static str]),
    /// An sRGB hex colour (`#rrggbb` or `#rrggbbaa`).
    Color,
}

/// One parameter row of a module.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    /// Protocol name (`rate`, `lifetime_min`, ...).
    pub name: &'static str,
    /// How it is edited.
    pub kind: ParamKind,
    /// The current value.
    pub value: ParamValue,
}

/// Largest magnitude a free number may take (positions, accelerations, speeds).
pub const BIG: f32 = 1.0e6;

/// Smallest value of a parameter whose zero means "module absent" (drag, wind response,
/// softness, heat haze): zero is refused so a parameter edit never removes its module (remove it
/// or disable it instead).
pub const MIN_ON: f32 = 1.0e-4;

const SPAWN_MODES: &[&str] = &["rate", "burst", "per_distance", "event"];
const SHAPES: &[&str] = &["point", "sphere", "box", "cone", "mesh_surface", "skinned_mesh"];
const VELOCITY_MODES: &[&str] = &["radial", "directional", "planar", "normal"];
const RENDER_MODES: &[&str] = &["billboard", "stretched", "mesh", "ribbon"];
const BLENDS: &[&str] = &["additive", "alpha"];
const SPRITES: &[&str] = &[
    "soft_disc",
    "hard_disc",
    "ring",
    "spark",
    "puff",
    "flare",
    "shard",
    "puff_anim",
    "fire_anim",
    "fireball_anim",
];
const KILL_SHAPES: &[&str] = &["sphere", "box"];
const KILL_SIDES: &[&str] = &["inside", "outside"];

fn num(name: &'static str, min: f32, max: f32, v: f32) -> Param {
    Param {
        name,
        kind: ParamKind::Num { min, max },
        value: ParamValue::Num(v),
    }
}

fn int(name: &'static str, min: u32, max: u32, v: u32) -> Param {
    Param {
        name,
        kind: ParamKind::Int { min, max },
        value: ParamValue::Num(v as f32),
    }
}

fn boolean(name: &'static str, v: bool) -> Param {
    Param {
        name,
        kind: ParamKind::Bool,
        value: ParamValue::Bool(v),
    }
}

fn choice(name: &'static str, list: &'static [&'static str], v: &str) -> Param {
    Param {
        name,
        kind: ParamKind::Choice(list),
        value: ParamValue::Text(v.to_owned()),
    }
}

fn color(name: &'static str, v: &str) -> Param {
    Param {
        name,
        kind: ParamKind::Color,
        value: ParamValue::Text(v.to_owned()),
    }
}

fn spawn_mode_name(m: SpawnMode) -> &'static str {
    match m {
        SpawnMode::Rate(_) => "rate",
        SpawnMode::Burst(_) => "burst",
        SpawnMode::PerDistance { .. } => "per_distance",
        SpawnMode::Event => "event",
    }
}

fn shape_name(s: &ShapeDef) -> &'static str {
    match s {
        ShapeDef::Point => "point",
        ShapeDef::Sphere { .. } => "sphere",
        ShapeDef::Box { .. } => "box",
        ShapeDef::Cone { .. } => "cone",
        ShapeDef::MeshSurface { .. } => "mesh_surface",
        ShapeDef::SkinnedMesh { .. } => "skinned_mesh",
    }
}

fn velocity_name(v: &VelocityMode) -> &'static str {
    match v {
        VelocityMode::Radial { .. } => "radial",
        VelocityMode::Directional { .. } => "directional",
        VelocityMode::Planar { .. } => "planar",
        VelocityMode::Normal { .. } => "normal",
    }
}

fn render_mode_name(m: RenderMode) -> &'static str {
    match m {
        RenderMode::Billboard => "billboard",
        RenderMode::Stretched => "stretched",
        RenderMode::Mesh => "mesh",
        RenderMode::Ribbon => "ribbon",
    }
}

/// The draw list a render module feeds (`additive`, `alpha`, `mesh`, `ribbon`, `haze`).
#[must_use]
pub fn draw_list(e: &EmitterDef) -> &'static str {
    if e.render.heat_haze > 0.0 {
        return "haze";
    }
    match (e.render.mode, e.render.blend) {
        (RenderMode::Ribbon, _) => "ribbon",
        (RenderMode::Mesh, _) => "mesh",
        (_, BlendMode::Additive) => "additive",
        (_, BlendMode::Alpha) => "alpha",
    }
}

fn sprite_name(s: SpriteId) -> &'static str {
    SPRITES[sprite_index(s)]
}

fn sprite_index(s: SpriteId) -> usize {
    match s {
        SpriteId::SoftDisc => 0,
        SpriteId::HardDisc => 1,
        SpriteId::Ring => 2,
        SpriteId::Spark => 3,
        SpriteId::Puff => 4,
        SpriteId::Flare => 5,
        SpriteId::Shard => 6,
        SpriteId::PuffAnim => 7,
        SpriteId::FireAnim => 8,
        SpriteId::FireballAnim => 9,
    }
}

fn sprite_from(name: &str) -> Option<SpriteId> {
    Some(match name {
        "soft_disc" => SpriteId::SoftDisc,
        "hard_disc" => SpriteId::HardDisc,
        "ring" => SpriteId::Ring,
        "spark" => SpriteId::Spark,
        "puff" => SpriteId::Puff,
        "flare" => SpriteId::Flare,
        "shard" => SpriteId::Shard,
        "puff_anim" => SpriteId::PuffAnim,
        "fire_anim" => SpriteId::FireAnim,
        "fireball_anim" => SpriteId::FireballAnim,
        _ => return None,
    })
}

fn velocity_speed(v: &VelocityMode) -> RangeDef {
    match v {
        VelocityMode::Radial { speed }
        | VelocityMode::Directional { speed, .. }
        | VelocityMode::Planar { speed, .. }
        | VelocityMode::Normal { speed } => *speed,
    }
}

fn velocity_speed_mut(v: &mut VelocityMode) -> &mut RangeDef {
    match v {
        VelocityMode::Radial { speed }
        | VelocityMode::Directional { speed, .. }
        | VelocityMode::Planar { speed, .. }
        | VelocityMode::Normal { speed } => speed,
    }
}

/// The parameter rows of `kind` in `e` (empty for an absent module). Curves and gradients are
/// edited through the curve editor, not here; their rows report the key count only.
#[must_use]
pub fn params(e: &EmitterDef, kind: ModuleKind) -> Vec<Param> {
    if !present(e, kind) {
        return Vec::new();
    }
    let u = &e.update;
    let r = &e.render;
    match kind {
        ModuleKind::Spawn => {
            let s = &e.spawn;
            let mut out = vec![choice("mode", SPAWN_MODES, spawn_mode_name(s.mode))];
            match s.mode {
                SpawnMode::Rate(v) => out.push(num("rate", 0.0, BIG, v)),
                SpawnMode::Burst(n) => out.push(int("burst", 0, 1_000_000, n)),
                SpawnMode::PerDistance { every } => out.push(num("every", 0.001, BIG, every)),
                SpawnMode::Event => {}
            }
            out.extend([
                num("lifetime_min", 0.001, 3600.0, s.lifetime.min),
                num("lifetime_max", 0.001, 3600.0, s.lifetime.max),
                num("count_scale", 0.0, 1000.0, s.count_scale),
                num("delay", 0.0, 3600.0, e.delay),
                num("duration", 0.0, 3600.0, e.duration.unwrap_or(0.0)),
            ]);
            out
        }
        ModuleKind::Init => {
            let i = &e.init;
            let mut out = vec![choice("shape", SHAPES, shape_name(&i.shape))];
            match &i.shape {
                ShapeDef::Point => {}
                ShapeDef::Sphere { radius, surface } => out.extend([
                    num("radius_min", 0.0, BIG, radius.min),
                    num("radius_max", 0.0, BIG, radius.max),
                    boolean("surface", *surface),
                ]),
                ShapeDef::Box {
                    half_extents,
                    surface,
                } => out.extend([
                    num("half_x", 0.0, BIG, half_extents.0),
                    num("half_y", 0.0, BIG, half_extents.1),
                    num("half_z", 0.0, BIG, half_extents.2),
                    boolean("surface", *surface),
                ]),
                ShapeDef::Cone { angle, length } => out.extend([
                    num("angle", 0.0, 90.0, *angle),
                    num("length_min", 0.0, BIG, length.min),
                    num("length_max", 0.0, BIG, length.max),
                ]),
                ShapeDef::MeshSurface { .. } | ShapeDef::SkinnedMesh { .. } => {}
            }
            let speed = velocity_speed(&i.velocity.mode);
            out.push(choice(
                "velocity",
                VELOCITY_MODES,
                velocity_name(&i.velocity.mode),
            ));
            out.extend([
                num("speed_min", -BIG, BIG, speed.min),
                num("speed_max", -BIG, BIG, speed.max),
            ]);
            if let VelocityMode::Directional { dir, spread, .. } = &i.velocity.mode {
                out.extend([
                    num("dir_x", -BIG, BIG, dir.0),
                    num("dir_y", -BIG, BIG, dir.1),
                    num("dir_z", -BIG, BIG, dir.2),
                    num("spread", 0.0, 180.0, *spread),
                ]);
            }
            out.extend([
                num("inherit", -10.0, 10.0, i.velocity.inherit),
                num("size_min", 0.0, BIG, i.size.min),
                num("size_max", 0.0, BIG, i.size.max),
                num("rotation_min", -BIG, BIG, i.rotation.min),
                num("rotation_max", -BIG, BIG, i.rotation.max),
                num("spin_min", -BIG, BIG, i.spin.min),
                num("spin_max", -BIG, BIG, i.spin.max),
            ]);
            out
        }
        ModuleKind::InitColor => vec![int("keys", 0, u32::MAX, e.init.color.len() as u32)],
        ModuleKind::Gravity => vec![
            num("x", -BIG, BIG, u.gravity.0),
            num("y", -BIG, BIG, u.gravity.1),
            num("z", -BIG, BIG, u.gravity.2),
        ],
        ModuleKind::Drag => vec![num("drag", MIN_ON, 1000.0, u.drag)],
        ModuleKind::Curl => {
            let (a, f, s) = u.curl.unwrap_or_default();
            vec![
                num("amount", -BIG, BIG, a),
                num("frequency", 0.0, BIG, f),
                num("speed", -BIG, BIG, s),
            ]
        }
        ModuleKind::Vortex => {
            let v = u.vortex.clone().unwrap_or(VortexDef {
                axis: (0.0, 1.0, 0.0),
                strength: 0.0,
                radial: 0.0,
            });
            vec![
                num("axis_x", -BIG, BIG, v.axis.0),
                num("axis_y", -BIG, BIG, v.axis.1),
                num("axis_z", -BIG, BIG, v.axis.2),
                num("strength", -BIG, BIG, v.strength),
                num("radial", -BIG, BIG, v.radial),
            ]
        }
        ModuleKind::Attractor => {
            let a = u.attractor.clone().unwrap_or(AttractorDef {
                position: (0.0, 0.0, 0.0),
                strength: 0.0,
            });
            vec![
                num("x", -BIG, BIG, a.position.0),
                num("y", -BIG, BIG, a.position.1),
                num("z", -BIG, BIG, a.position.2),
                num("strength", -BIG, BIG, a.strength),
            ]
        }
        ModuleKind::Wind => vec![
            num("response", MIN_ON, 1.0, u.wind_response),
            num("drag", 0.0, 1000.0, u.wind_drag),
        ],
        ModuleKind::SizeOverLife => vec![int("keys", 0, u32::MAX, u.size_over_life.len() as u32)],
        ModuleKind::ColorOverLife => vec![int("keys", 0, u32::MAX, u.color_over_life.len() as u32)],
        ModuleKind::AlphaOverLife => vec![int("keys", 0, u32::MAX, u.alpha_over_life.len() as u32)],
        ModuleKind::RotationOverLife => {
            vec![int("keys", 0, u32::MAX, u.rotation_over_life.len() as u32)]
        }
        ModuleKind::KillBelow => vec![num("y", -BIG, BIG, u.kill_below.unwrap_or(0.0))],
        ModuleKind::KillVolume => {
            let Some(kv) = &u.kill_volume else {
                return Vec::new();
            };
            let mut out = vec![];
            match kv.shape {
                KillShape::Sphere { radius } => {
                    out.push(choice("shape", KILL_SHAPES, "sphere"));
                    out.push(num("radius", 0.0001, BIG, radius));
                }
                KillShape::Box { half_extents } => {
                    out.push(choice("shape", KILL_SHAPES, "box"));
                    out.extend([
                        num("half_x", 0.0001, BIG, half_extents.0),
                        num("half_y", 0.0001, BIG, half_extents.1),
                        num("half_z", 0.0001, BIG, half_extents.2),
                    ]);
                }
            }
            out.extend([
                num("center_x", -BIG, BIG, kv.center.0),
                num("center_y", -BIG, BIG, kv.center.1),
                num("center_z", -BIG, BIG, kv.center.2),
                choice(
                    "kill",
                    KILL_SIDES,
                    if kv.kill == KillSide::Inside {
                        "inside"
                    } else {
                        "outside"
                    },
                ),
            ]);
            out
        }
        ModuleKind::DepthCollision => {
            let Some(d) = u.depth_collision else {
                return Vec::new();
            };
            vec![boolean("kill", d.kill), num("offset", 0.0, BIG, d.offset)]
        }
        ModuleKind::Events => vec![int("links", 0, u32::MAX, u.events.len() as u32)],
        ModuleKind::Render => vec![
            choice("mode", RENDER_MODES, render_mode_name(r.mode)),
            choice(
                "blend",
                BLENDS,
                if r.blend == BlendMode::Additive {
                    "additive"
                } else {
                    "alpha"
                },
            ),
            choice("sprite", SPRITES, sprite_name(r.sprite)),
            num("alpha_scale", 0.0, 1000.0, r.alpha_scale),
            num("emissive", 0.0, 1.0e4, r.emissive),
            boolean("lit", r.lit),
            num("self_shadow", 0.0, 1.0, r.self_shadow),
            num("screen_floor_px", 0.0, 4096.0, r.screen_floor_px),
        ],
        ModuleKind::Flipbook => {
            let Some(f) = r.flipbook else {
                return Vec::new();
            };
            vec![
                int("cols", 1, 64, f.cols),
                int("rows", 1, 64, f.rows),
                int("frames", 1, 4096, f.frames),
                num(
                    "fps",
                    0.0,
                    1000.0,
                    match f.mode {
                        FlipbookMode::Fps(v) => v,
                        FlipbookMode::OverLife => 0.0,
                    },
                ),
                boolean("random_start", f.random_start),
                boolean("blend_frames", f.blend_frames),
            ]
        }
        ModuleKind::SoftParticles => vec![num("softness", MIN_ON, BIG, r.softness)],
        ModuleKind::Light => {
            let Some(l) = &r.light else {
                return Vec::new();
            };
            vec![
                color("color", &l.color),
                num("intensity", 0.0, 1.0e6, l.intensity),
                num("radius", 0.0, BIG, l.radius),
            ]
        }
        ModuleKind::HeatHaze => vec![num("strength", MIN_ON, 1024.0, r.heat_haze)],
    }
}

/// Why [`set_param`] refused.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamError {
    /// The module has no parameter by that name (in its current mode).
    Unknown(String),
    /// The value is out of range, non-finite, the wrong type or an unknown choice.
    BadValue(String),
}

fn check(p: &Param, value: &ParamValue) -> Result<ParamValue, ParamError> {
    let bad = |m: String| Err(ParamError::BadValue(m));
    match (p.kind, value) {
        (ParamKind::Num { min, max }, ParamValue::Num(v)) => {
            if !v.is_finite() || *v < min || *v > max {
                return bad(format!(
                    "`{}` must be a finite number in {min}..={max}, got {v}",
                    p.name
                ));
            }
            Ok(ParamValue::Num(*v))
        }
        (ParamKind::Int { min, max }, ParamValue::Num(v)) => {
            if !v.is_finite() || v.fract() != 0.0 || *v < min as f32 || *v > max as f32 {
                return bad(format!(
                    "`{}` must be an integer in {min}..={max}, got {v}",
                    p.name
                ));
            }
            Ok(ParamValue::Num(*v))
        }
        (ParamKind::Bool, ParamValue::Bool(b)) => Ok(ParamValue::Bool(*b)),
        (ParamKind::Choice(list), ParamValue::Text(s)) => {
            if !list.contains(&s.as_str()) {
                return bad(format!(
                    "`{}` must be one of {}, got `{s}`",
                    p.name,
                    list.join(", ")
                ));
            }
            Ok(ParamValue::Text(s.clone()))
        }
        (ParamKind::Color, ParamValue::Text(s)) => {
            if regia_core::math::Color::from_hex(s).is_err() {
                return bad(format!(
                    "`{}` must be an sRGB hex colour (#rrggbb or #rrggbbaa), got `{s}`",
                    p.name
                ));
            }
            Ok(ParamValue::Text(s.clone()))
        }
        _ => bad(format!("`{}` has the wrong type", p.name)),
    }
}

fn as_f32(v: &ParamValue) -> f32 {
    match v {
        ParamValue::Num(v) => *v,
        _ => 0.0,
    }
}

fn as_bool(v: &ParamValue) -> bool {
    matches!(v, ParamValue::Bool(true))
}

fn as_text(v: &ParamValue) -> &str {
    match v {
        ParamValue::Text(s) => s,
        _ => "",
    }
}

/// Sets parameter `name` of module `kind` in `e` after a range check. The whole effect is
/// validated by the caller (the format's validator sees cross-field rules such as `min <= max`).
///
/// # Errors
///
/// [`ParamError::Unknown`] for a name the module does not have in its current mode,
/// [`ParamError::BadValue`] for a value outside the row's range.
pub fn set_param(
    e: &mut EmitterDef,
    kind: ModuleKind,
    name: &str,
    value: &ParamValue,
) -> Result<(), ParamError> {
    let rows = params(e, kind);
    let Some(row) = rows.iter().find(|p| p.name == name) else {
        let names: Vec<&str> = rows.iter().map(|p| p.name).collect();
        return Err(ParamError::Unknown(format!(
            "module `{}` has no parameter `{name}` (it has: {})",
            kind.name(),
            names.join(", ")
        )));
    };
    if row.name == "keys" || row.name == "links" {
        return Err(ParamError::Unknown(format!(
            "`{name}` is read-only: edit the keys with the curve editor (curve_* / gradient_* ops)"
        )));
    }
    let v = check(row, value)?;
    let f = as_f32(&v);
    let u = &mut e.update;
    let r = &mut e.render;
    match (kind, name) {
        (ModuleKind::Spawn, "mode") if spawn_mode_name(e.spawn.mode) != as_text(&v) => {
            e.spawn.mode = match as_text(&v) {
                "rate" => SpawnMode::Rate(10.0),
                "burst" => SpawnMode::Burst(16),
                "per_distance" => SpawnMode::PerDistance { every: 0.5 },
                _ => SpawnMode::Event,
            };
        }
        (ModuleKind::Spawn | ModuleKind::Init | ModuleKind::Render, "mode" | "shape" | "velocity")
            if as_text(&row.value) == as_text(&v) => {}
        (ModuleKind::Spawn, "rate") => e.spawn.mode = SpawnMode::Rate(f),
        (ModuleKind::Spawn, "burst") => e.spawn.mode = SpawnMode::Burst(f as u32),
        (ModuleKind::Spawn, "every") => e.spawn.mode = SpawnMode::PerDistance { every: f },
        (ModuleKind::Spawn, "lifetime_min") => e.spawn.lifetime.min = f,
        (ModuleKind::Spawn, "lifetime_max") => e.spawn.lifetime.max = f,
        (ModuleKind::Spawn, "count_scale") => e.spawn.count_scale = f,
        (ModuleKind::Spawn, "delay") => e.delay = f,
        (ModuleKind::Spawn, "duration") => e.duration = (f > 0.0).then_some(f),
        (ModuleKind::Init, "shape") => {
            e.init.shape = match as_text(&v) {
                "point" => ShapeDef::Point,
                "sphere" => ShapeDef::Sphere {
                    radius: RangeDef { min: 0.0, max: 0.5 },
                    surface: false,
                },
                "box" => ShapeDef::Box {
                    half_extents: (0.5, 0.5, 0.5),
                    surface: false,
                },
                "cone" => ShapeDef::Cone {
                    angle: 25.0,
                    length: RangeDef { min: 0.0, max: 0.2 },
                },
                other => {
                    // Mesh shapes need a registered mesh name: set them in the file.
                    if shape_name(&e.init.shape) == other {
                        return Ok(());
                    }
                    return Err(ParamError::BadValue(format!(
                        "shape `{other}` names a mesh asset; set it in the .vfx.ron (the editor keeps it once set)"
                    )));
                }
            };
        }
        (ModuleKind::Init, "radius_min" | "radius_max" | "surface") => {
            if let ShapeDef::Sphere { radius, surface } = &mut e.init.shape {
                match name {
                    "radius_min" => radius.min = f,
                    "radius_max" => radius.max = f,
                    _ => *surface = as_bool(&v),
                }
            } else if let ShapeDef::Box { surface, .. } = &mut e.init.shape {
                *surface = as_bool(&v);
            }
        }
        (ModuleKind::Init, "half_x" | "half_y" | "half_z") => {
            if let ShapeDef::Box { half_extents, .. } = &mut e.init.shape {
                match name {
                    "half_x" => half_extents.0 = f,
                    "half_y" => half_extents.1 = f,
                    _ => half_extents.2 = f,
                }
            }
        }
        (ModuleKind::Init, "angle" | "length_min" | "length_max") => {
            if let ShapeDef::Cone { angle, length } = &mut e.init.shape {
                match name {
                    "angle" => *angle = f,
                    "length_min" => length.min = f,
                    _ => length.max = f,
                }
            }
        }
        (ModuleKind::Init, "velocity") => {
            let speed = velocity_speed(&e.init.velocity.mode);
            e.init.velocity.mode = match as_text(&v) {
                "radial" => VelocityMode::Radial { speed },
                "directional" => VelocityMode::Directional {
                    dir: (0.0, 1.0, 0.0),
                    spread: 15.0,
                    speed,
                },
                "planar" => VelocityMode::Planar {
                    speed,
                    lift: RangeDef::default(),
                },
                _ => VelocityMode::Normal { speed },
            };
        }
        (ModuleKind::Init, "speed_min") => velocity_speed_mut(&mut e.init.velocity.mode).min = f,
        (ModuleKind::Init, "speed_max") => velocity_speed_mut(&mut e.init.velocity.mode).max = f,
        (ModuleKind::Init, "dir_x" | "dir_y" | "dir_z" | "spread") => {
            if let VelocityMode::Directional { dir, spread, .. } = &mut e.init.velocity.mode {
                match name {
                    "dir_x" => dir.0 = f,
                    "dir_y" => dir.1 = f,
                    "dir_z" => dir.2 = f,
                    _ => *spread = f,
                }
            }
        }
        (ModuleKind::Init, "inherit") => e.init.velocity.inherit = f,
        (ModuleKind::Init, "size_min") => e.init.size.min = f,
        (ModuleKind::Init, "size_max") => e.init.size.max = f,
        (ModuleKind::Init, "rotation_min") => e.init.rotation.min = f,
        (ModuleKind::Init, "rotation_max") => e.init.rotation.max = f,
        (ModuleKind::Init, "spin_min") => e.init.spin.min = f,
        (ModuleKind::Init, "spin_max") => e.init.spin.max = f,
        (ModuleKind::Gravity, "x") => u.gravity.0 = f,
        (ModuleKind::Gravity, "y") => u.gravity.1 = f,
        (ModuleKind::Gravity, "z") => u.gravity.2 = f,
        (ModuleKind::Drag, "drag") => u.drag = f,
        (ModuleKind::Curl, _) => {
            if let Some(c) = u.curl.as_mut() {
                match name {
                    "amount" => c.0 = f,
                    "frequency" => c.1 = f,
                    _ => c.2 = f,
                }
            }
        }
        (ModuleKind::Vortex, _) => {
            if let Some(vx) = u.vortex.as_mut() {
                match name {
                    "axis_x" => vx.axis.0 = f,
                    "axis_y" => vx.axis.1 = f,
                    "axis_z" => vx.axis.2 = f,
                    "strength" => vx.strength = f,
                    _ => vx.radial = f,
                }
            }
        }
        (ModuleKind::Attractor, _) => {
            if let Some(a) = u.attractor.as_mut() {
                match name {
                    "x" => a.position.0 = f,
                    "y" => a.position.1 = f,
                    "z" => a.position.2 = f,
                    _ => a.strength = f,
                }
            }
        }
        (ModuleKind::Wind, "response") => u.wind_response = f,
        (ModuleKind::Wind, "drag") => u.wind_drag = f,
        (ModuleKind::KillBelow, "y") => u.kill_below = Some(f),
        (ModuleKind::KillVolume, _) => {
            if let Some(kv) = u.kill_volume.as_mut() {
                match name {
                    "shape" => {
                        kv.shape = if as_text(&v) == "box" {
                            match kv.shape {
                                KillShape::Box { .. } => kv.shape,
                                KillShape::Sphere { radius } => KillShape::Box {
                                    half_extents: (radius, radius, radius),
                                },
                            }
                        } else {
                            match kv.shape {
                                KillShape::Sphere { .. } => kv.shape,
                                KillShape::Box { half_extents } => KillShape::Sphere {
                                    radius: half_extents.0.max(half_extents.1).max(half_extents.2),
                                },
                            }
                        };
                    }
                    "radius" => {
                        if let KillShape::Sphere { radius } = &mut kv.shape {
                            *radius = f;
                        }
                    }
                    "half_x" | "half_y" | "half_z" => {
                        if let KillShape::Box { half_extents } = &mut kv.shape {
                            match name {
                                "half_x" => half_extents.0 = f,
                                "half_y" => half_extents.1 = f,
                                _ => half_extents.2 = f,
                            }
                        }
                    }
                    "center_x" => kv.center.0 = f,
                    "center_y" => kv.center.1 = f,
                    "center_z" => kv.center.2 = f,
                    _ => {
                        kv.kill = if as_text(&v) == "inside" {
                            KillSide::Inside
                        } else {
                            KillSide::Outside
                        };
                    }
                }
            }
        }
        (ModuleKind::DepthCollision, _) => {
            if let Some(d) = u.depth_collision.as_mut() {
                if name == "kill" {
                    d.kill = as_bool(&v);
                } else {
                    d.offset = f;
                }
            }
        }
        (ModuleKind::Render, "mode") => {
            r.mode = match as_text(&v) {
                "billboard" => RenderMode::Billboard,
                "stretched" => RenderMode::Stretched,
                "mesh" => RenderMode::Mesh,
                _ => RenderMode::Ribbon,
            };
        }
        (ModuleKind::Render, "blend") => {
            r.blend = if as_text(&v) == "alpha" {
                BlendMode::Alpha
            } else {
                BlendMode::Additive
            };
        }
        (ModuleKind::Render, "sprite") => {
            if let Some(s) = sprite_from(as_text(&v)) {
                r.sprite = s;
            }
        }
        (ModuleKind::Render, "alpha_scale") => r.alpha_scale = f,
        (ModuleKind::Render, "emissive") => r.emissive = f,
        (ModuleKind::Render, "lit") => r.lit = as_bool(&v),
        (ModuleKind::Render, "self_shadow") => r.self_shadow = f,
        (ModuleKind::Render, "screen_floor_px") => r.screen_floor_px = f,
        (ModuleKind::Flipbook, _) => {
            if let Some(fb) = r.flipbook.as_mut() {
                match name {
                    "cols" => fb.cols = f as u32,
                    "rows" => fb.rows = f as u32,
                    "frames" => fb.frames = f as u32,
                    "fps" => {
                        fb.mode = if f > 0.0 {
                            FlipbookMode::Fps(f)
                        } else {
                            FlipbookMode::OverLife
                        };
                    }
                    "random_start" => fb.random_start = as_bool(&v),
                    _ => fb.blend_frames = as_bool(&v),
                }
            }
        }
        (ModuleKind::SoftParticles, _) => r.softness = f,
        (ModuleKind::Light, _) => {
            if let Some(l) = r.light.as_mut() {
                match name {
                    "color" => as_text(&v).clone_into(&mut l.color),
                    "intensity" => l.intensity = f,
                    _ => l.radius = f,
                }
            }
        }
        (ModuleKind::HeatHaze, _) => r.heat_haze = f,
        _ => {
            return Err(ParamError::Unknown(format!(
                "module `{}` has no parameter `{name}`",
                kind.name()
            )));
        }
    }
    Ok(())
}
