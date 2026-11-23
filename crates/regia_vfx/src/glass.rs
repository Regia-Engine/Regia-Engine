// SPDX-License-Identifier: MPL-2.0
//! Glass damage effects (`docs/formats/glass-crack.md`, DO27 `spec/glass.md` "Glass types"):
//!
//! - A **pressure pane** (building glazing) reaching D3 fails into chunky angular fragments
//!   ([`glass_chunks`], mesh debris, not glitter) and, when its crack set allows it, breaches:
//!   a [`GlassBreach`] message.
//! - A **laminate** (unit canopies) never sheds sheets or shards: it stays in the frame, so its
//!   level changes spawn nothing here.
//! - [`GlassBreach`] is the decompression hook, keyed to the level atmosphere. Airless: a violent
//!   outward vent with air flash-freezing into an ice-crystal plume and light debris, silent in
//!   the air (the airless medium has no airborne path) but felt through the ground. Mars: a short
//!   puff with a dust swirl. Earth: a minimal gust. Games may send [`GlassBreach`] themselves and
//!   override the presets in [`GlassEffectSettings`].
//!
//! Each loud breach spawns a short-lived sound emitter entity ([`BreachSoundEmitter`], placed at
//! the breach). `regia_framework::glass_breach::GlassBreachAudioPlugin` gives it a
//! ground-coupled audio emitter (this crate does not depend on `regia_audio`: same layer);
//! [`GlassBreachFired`] hands it to the game, which starts its clip on it. The medium decides the
//! paths.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_render::glass::GlassType;
use regia_render::glass::{GlassCrackSets, GlassLevelChanged};
use regia_scene::level::{AtmospherePreset, LevelSettings};
use regia_scene::{GlobalTransform, Transform};

use crate::format::{
    BlendMode, ColorKey, EmitterDef, FloatKey, InitModule, RangeDef, RenderMode, RenderModule, ShapeDef,
    SpawnMode, SpawnModule, SpriteId, SystemModule, UpdateModule, VFX_FORMAT_VERSION, VelocityDef,
    VelocityMode, VfxSystemDef,
};
use crate::plugin::SpawnEffect;
use crate::scene::VfxScene;
use crate::shake::CameraShakeRequest;

/// Which air is on the low-pressure side of a breach.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BreachAtmosphere {
    /// Earth-like air, 101.3 kPa outside. A breach plays `glass_breach_earth` when the pane's
    /// inside pressure exceeds that by at least [`GlassEffectSettings::min_pressure`] (1 kPa).
    #[default]
    Earth,
    /// Thin CO₂ (≈ 0.6 kPa).
    Mars,
    /// Vacuum (the Moon, Europa, open space).
    Airless,
}

impl BreachAtmosphere {
    /// From the level's atmosphere preset.
    #[must_use]
    pub fn from_preset(preset: AtmospherePreset) -> Self {
        match preset {
            AtmospherePreset::Earth => Self::Earth,
            AtmospherePreset::Mars => Self::Mars,
            AtmospherePreset::Airless => Self::Airless,
        }
    }

    /// Outside pressure, kPa.
    #[must_use]
    pub fn outside_kpa(self) -> f32 {
        match self {
            Self::Earth => 101.3,
            Self::Mars => 0.6,
            Self::Airless => 0.0,
        }
    }

    /// Lower-case name (agent protocol).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Earth => "earth",
            Self::Mars => "mars",
            Self::Airless => "airless",
        }
    }
}

/// A pressurised volume vents through broken glass. Send it to trigger a breach; the glass
/// systems send it when a pressure pane fails.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct GlassBreach {
    /// Breach centre, world space.
    pub position: Vec3,
    /// Outward direction (toward the low-pressure side), unit.
    pub normal: Vec3,
    /// The air outside.
    pub atmosphere: BreachAtmosphere,
    /// Pressure difference across the breach, kPa (inside − outside).
    pub pressure: f32,
}

/// What a breach did (for gameplay, audio and the agent).
#[derive(Message, Clone, Debug, PartialEq)]
pub struct GlassBreachFired {
    /// The breach.
    pub breach: GlassBreach,
    /// The effect spawned, if any.
    pub effect: Option<String>,
    /// The sound emitter entity (start a clip on it), if the breach is loud enough to make one.
    pub sound_emitter: Option<Entity>,
}

/// Inside pressure and the outward direction of a pressure pane (mesh-local), so a breach
/// vents the right way. Without it the pane vents along its page normal at 101.3 kPa.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct GlassPressure {
    /// Outward (toward the outside) in mesh-local space.
    pub outward: Vec3,
    /// Inside pressure, kPa.
    pub inside_kpa: f32,
}

impl Default for GlassPressure {
    fn default() -> Self {
        Self {
            outward: Vec3::Z,
            inside_kpa: 101.3,
        }
    }
}

/// Marks a breach's sound emitter; despawned after `ttl` seconds. The audio components are added
/// by `regia_framework::glass_breach` (min/max distance from here).
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct BreachSoundEmitter {
    /// Seconds left.
    pub ttl: f32,
    /// Distance (m) inside which the sound is at full level.
    pub min_distance: f32,
    /// Distance (m) beyond which it is silent.
    pub max_distance: f32,
}

/// Glass effect presets (override per game).
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct GlassEffectSettings {
    /// Spawn the crack set's chunk burst when a pressure pane fails.
    pub chunks: bool,
    /// Breach effect per atmosphere (`None` = no effect).
    pub breach_earth: Option<String>,
    /// Mars breach effect.
    pub breach_mars: Option<String>,
    /// Airless breach effect.
    pub breach_airless: Option<String>,
    /// Below this pressure difference (kPa) a breach makes no effect, shake or sound.
    pub min_pressure: f32,
    /// Spawn the ground-coupled sound emitter.
    pub sound: bool,
}

impl Default for GlassEffectSettings {
    fn default() -> Self {
        Self {
            chunks: true,
            breach_earth: Some("glass_breach_earth".into()),
            breach_mars: Some("glass_breach_mars".into()),
            breach_airless: Some("glass_breach_airless".into()),
            min_pressure: 1.0,
            sound: true,
        }
    }
}

impl GlassEffectSettings {
    /// The effect of a breach into `atmosphere`.
    #[must_use]
    pub fn breach_effect(&self, atmosphere: BreachAtmosphere) -> Option<&str> {
        match atmosphere {
            BreachAtmosphere::Earth => self.breach_earth.as_deref(),
            BreachAtmosphere::Mars => self.breach_mars.as_deref(),
            BreachAtmosphere::Airless => self.breach_airless.as_deref(),
        }
    }
}

/// Turns glass level changes into chunk bursts and breaches.
#[tracing::instrument(skip_all)]
pub fn glass_level_effects(
    mut changes: MessageReader<GlassLevelChanged>,
    sets: Option<Res<GlassCrackSets>>,
    settings: Res<GlassEffectSettings>,
    level: Option<Res<LevelSettings>>,
    pressures: Query<(&GlassPressure, Option<&GlobalTransform>)>,
    mut effects: MessageWriter<SpawnEffect>,
    mut breaches: MessageWriter<GlassBreach>,
) {
    let _span = tracing::debug_span!("glass_level_effects").entered();
    for change in changes.read() {
        // Laminate stays in the frame: no sheets, no shard showers.
        if change.glass_type != GlassType::PressurePane || change.to < 3 || change.from >= 3 {
            continue;
        }
        let rules = sets
            .as_deref()
            .and_then(|s| s.get(&change.set))
            .and_then(|set| set.manifest.pressure_pane.clone())
            .unwrap_or_default();
        let (outward, inside) = match pressures.get(change.entity) {
            Ok((p, transform)) => {
                let world = transform.map_or(p.outward, |t| t.0.transform_vector3(p.outward));
                (world.normalize_or(change.normal), p.inside_kpa)
            }
            Err(_) => (change.normal, 101.3),
        };
        if settings.chunks {
            effects.write(SpawnEffect {
                effect: rules.chunks.preset.clone(),
                position: change.point,
                forward: outward,
                seed: None,
            });
        }
        if rules.breach {
            let atmosphere = BreachAtmosphere::from_preset(
                level
                    .as_deref()
                    .map_or(AtmospherePreset::Earth, |l| l.atmosphere.preset),
            );
            breaches.write(GlassBreach {
                position: change.point,
                normal: outward,
                atmosphere,
                pressure: (inside - atmosphere.outside_kpa()).max(0.0),
            });
        }
    }
}

/// Plays breaches: the atmosphere's effect, a shake, and the ground-coupled sound emitter.
#[tracing::instrument(skip_all)]
pub fn apply_glass_breach(
    mut commands: Commands,
    mut breaches: MessageReader<GlassBreach>,
    settings: Res<GlassEffectSettings>,
    mut effects: MessageWriter<SpawnEffect>,
    mut shakes: MessageWriter<CameraShakeRequest>,
    mut fired: MessageWriter<GlassBreachFired>,
) {
    let _span = tracing::debug_span!("apply_glass_breach").entered();
    for breach in breaches.read() {
        let loud = breach.pressure >= settings.min_pressure;
        let effect = loud
            .then(|| settings.breach_effect(breach.atmosphere))
            .flatten()
            .map(str::to_string);
        if let Some(name) = &effect {
            effects.write(SpawnEffect {
                effect: name.clone(),
                position: breach.position,
                forward: breach.normal,
                seed: None,
            });
        }
        if loud {
            shakes.write(CameraShakeRequest {
                position: breach.position,
                strength: (breach.pressure / 101.3).clamp(0.05, 0.6),
                radius: 30.0,
            });
        }
        let sound_emitter = (loud && settings.sound).then(|| {
            commands
                .spawn((
                    Transform {
                        translation: breach.position,
                        rotation: Quat::IDENTITY,
                        scale: Vec3::ONE,
                    },
                    BreachSoundEmitter {
                        ttl: 6.0,
                        min_distance: 3.0,
                        max_distance: 600.0,
                    },
                ))
                .id()
        });
        fired.write(GlassBreachFired {
            breach: breach.clone(),
            effect,
            sound_emitter,
        });
    }
}

/// Despawns finished breach sound emitters.
pub fn expire_breach_sounds(
    mut commands: Commands,
    time: Option<Res<Time>>,
    mut emitters: Query<(Entity, &mut BreachSoundEmitter)>,
) {
    let dt = time.map_or(1.0 / 60.0, |t| t.delta_secs());
    for (entity, mut emitter) in &mut emitters {
        emitter.ttl -= dt;
        if emitter.ttl <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

/// Adds the glass effects to the scene's library when it lacks them (a game's own `.vfx.ron`
/// of the same name wins).
pub fn register_glass_effects(mut done: Local<bool>, scene: Option<ResMut<VfxScene>>) {
    let Some(mut scene) = scene else {
        return;
    };
    if *done {
        return;
    }
    *done = true;
    for def in glass_effects() {
        if scene.library().index(&def.name).is_none()
            && let Err(err) = scene.library_mut().add(def)
        {
            tracing::warn!(target: "regia_vfx", "glass effect rejected: {err}");
        }
    }
}

fn r(min: f32, max: f32) -> RangeDef {
    RangeDef { min, max }
}

fn color(hex: &str) -> Vec<ColorKey> {
    vec![ColorKey {
        t: 0.0,
        color: hex.into(),
    }]
}

fn fade(keys: &[(f32, f32)]) -> Vec<FloatKey> {
    keys.iter().map(|&(t, v)| FloatKey { t, v }).collect()
}

/// The built-in glass effects.
#[must_use]
pub fn glass_effects() -> Vec<VfxSystemDef> {
    vec![glass_chunks(), breach_airless(), breach_mars(), breach_earth()]
}

/// A thick pane failing: chunky angular fragments (oriented boxes, 6–25 cm), thrown outward and
/// falling, plus a little glass dust. No glitter.
#[must_use]
pub fn glass_chunks() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "glass_chunks".into(),
        system: SystemModule {
            bounds: None,
            budget: 96,
            lod: None,
        },
        emitters: vec![
            EmitterDef {
                name: "chunks".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(14),
                    lifetime: r(1.6, 2.6),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Box {
                        half_extents: (0.7, 0.7, 0.02),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 0.0, 1.0),
                            spread: 50.0,
                            speed: r(1.0, 4.5),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.06, 0.25),
                    rotation: r(0.0, std::f32::consts::TAU),
                    spin: r(-6.0, 6.0),
                    color: color("#b9cdd2"),
                },
                update: UpdateModule {
                    gravity: (0.0, -9.8, 0.0),
                    drag: 0.05,
                    alpha_over_life: fade(&[(0.0, 0.85), (0.85, 0.85), (1.0, 0.0)]),
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Mesh,
                    sprite: SpriteId::Shard,
                    blend: BlendMode::Alpha,
                    emissive: 0.0,
                    lit: true,
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "dust".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(10),
                    lifetime: r(0.6, 1.2),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.5),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Radial { speed: r(0.3, 1.2) },
                        inherit: 0.0,
                    },
                    size: r(0.25, 0.6),
                    rotation: r(0.0, std::f32::consts::TAU),
                    spin: r(-0.5, 0.5),
                    color: color("#d8e2e4"),
                },
                update: UpdateModule {
                    drag: 1.5,
                    alpha_over_life: fade(&[(0.0, 0.25), (1.0, 0.0)]),
                    size_over_life: fade(&[(0.0, 0.6), (1.0, 1.4)]),
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::Puff,
                    blend: BlendMode::Alpha,
                    lit: true,
                    ..Default::default()
                },
            },
        ],
    }
}

/// Airless breach: the air leaves violently along the normal and flash-freezes into an
/// ice-crystal plume (bright, then fading as it expands into vacuum), with light debris.
#[must_use]
pub fn breach_airless() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "glass_breach_airless".into(),
        system: SystemModule {
            bounds: None,
            budget: 640,
            lod: None,
        },
        emitters: vec![
            EmitterDef {
                name: "ice_plume".into(),
                delay: 0.0,
                duration: Some(1.4),
                spawn: SpawnModule {
                    mode: SpawnMode::Rate(260.0),
                    lifetime: r(0.8, 1.8),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.3),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 0.0, 1.0),
                            spread: 28.0,
                            speed: r(8.0, 22.0),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.3, 0.8),
                    rotation: r(0.0, std::f32::consts::TAU),
                    spin: r(-1.0, 1.0),
                    color: color("#eef6ff"),
                },
                update: UpdateModule {
                    // No air: no drag, no buoyancy; the plume keeps expanding.
                    drag: 0.0,
                    size_over_life: fade(&[(0.0, 0.4), (1.0, 3.0)]),
                    alpha_over_life: fade(&[(0.0, 0.9), (0.4, 0.5), (1.0, 0.0)]),
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::Puff,
                    blend: BlendMode::Alpha,
                    lit: true,
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "ice_glints".into(),
                delay: 0.0,
                duration: Some(1.0),
                spawn: SpawnModule {
                    mode: SpawnMode::Rate(180.0),
                    lifetime: r(0.6, 1.4),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.2),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 0.0, 1.0),
                            spread: 35.0,
                            speed: r(10.0, 26.0),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.02, 0.05),
                    rotation: r(0.0, 0.0),
                    spin: r(0.0, 0.0),
                    color: color("#f4fbff"),
                },
                update: UpdateModule {
                    alpha_over_life: fade(&[(0.0, 1.0), (1.0, 0.0)]),
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::Flare,
                    blend: BlendMode::Additive,
                    emissive: 1.5,
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "debris".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(24),
                    lifetime: r(1.5, 3.0),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.4),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 0.0, 1.0),
                            spread: 40.0,
                            speed: r(4.0, 14.0),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.03, 0.12),
                    rotation: r(0.0, std::f32::consts::TAU),
                    spin: r(-10.0, 10.0),
                    color: color("#8a8f94"),
                },
                update: UpdateModule {
                    gravity: (0.0, -1.6, 0.0),
                    drag: 0.0,
                    alpha_over_life: fade(&[(0.0, 1.0), (0.9, 1.0), (1.0, 0.0)]),
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Mesh,
                    sprite: SpriteId::Shard,
                    blend: BlendMode::Alpha,
                    lit: true,
                    ..Default::default()
                },
            },
        ],
    }
}

/// Mars breach: a short puff of escaping air that kicks up a swirl of ochre dust.
#[must_use]
pub fn breach_mars() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "glass_breach_mars".into(),
        system: SystemModule {
            bounds: None,
            budget: 256,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "dust_puff".into(),
            delay: 0.0,
            duration: Some(0.5),
            spawn: SpawnModule {
                mode: SpawnMode::Rate(160.0),
                lifetime: r(1.2, 2.4),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Sphere {
                    radius: r(0.0, 0.4),
                    surface: false,
                },
                velocity: VelocityDef {
                    mode: VelocityMode::Directional {
                        dir: (0.0, 0.0, 1.0),
                        spread: 45.0,
                        speed: r(2.0, 6.0),
                    },
                    inherit: 0.0,
                },
                size: r(0.4, 0.9),
                rotation: r(0.0, std::f32::consts::TAU),
                spin: r(-1.5, 1.5),
                color: color("#b98a62"),
            },
            update: UpdateModule {
                gravity: (0.0, -0.4, 0.0),
                drag: 1.2,
                curl: Some((0.8, 1.5, 0.0)),
                size_over_life: fade(&[(0.0, 0.6), (1.0, 2.2)]),
                alpha_over_life: fade(&[(0.0, 0.7), (1.0, 0.0)]),
                ..Default::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::Puff,
                blend: BlendMode::Alpha,
                lit: true,
                ..Default::default()
            },
        }],
    }
}

/// Earth breach: at most a faint gust (pressurised buildings on Earth barely vent).
#[must_use]
pub fn breach_earth() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "glass_breach_earth".into(),
        system: SystemModule {
            bounds: None,
            budget: 32,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "gust".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Burst(8),
                lifetime: r(0.5, 1.0),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Sphere {
                    radius: r(0.0, 0.3),
                    surface: false,
                },
                velocity: VelocityDef {
                    mode: VelocityMode::Directional {
                        dir: (0.0, 0.0, 1.0),
                        spread: 30.0,
                        speed: r(1.0, 2.5),
                    },
                    inherit: 0.0,
                },
                size: r(0.3, 0.6),
                rotation: r(0.0, std::f32::consts::TAU),
                spin: r(-0.5, 0.5),
                color: color("#e4e6e8"),
            },
            update: UpdateModule {
                drag: 2.0,
                alpha_over_life: fade(&[(0.0, 0.18), (1.0, 0.0)]),
                ..Default::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::Puff,
                blend: BlendMode::Alpha,
                lit: true,
                ..Default::default()
            },
        }],
    }
}

/// Installs the glass effects (add after [`crate::VfxPlugin`]).
pub struct GlassEffectsPlugin;

impl Plugin for GlassEffectsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GlassEffectSettings>()
            .add_message::<GlassBreach>()
            .add_message::<GlassBreachFired>()
            .add_message::<GlassLevelChanged>()
            .register_type::<GlassPressure>()
            .add_systems(
                Update,
                (
                    // The library is complete before the scene ticks and the runtime uploads its
                    // curve LUT: unordered, the single-threaded executor uploaded the LUT without
                    // the glass curves first and again a frame later (ADR-214).
                    register_glass_effects.before(crate::plugin::tick_vfx_scene),
                    glass_level_effects,
                    apply_glass_breach,
                    expire_breach_sounds,
                )
                    .chain(),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VfxPlugin;
    use regia_render::glass::GlassLevelChanged;

    fn app(atmosphere: AtmospherePreset) -> App {
        let mut app = App::new();
        app.add_plugins(VfxPlugin);
        let mut level = LevelSettings::default();
        level.atmosphere.preset = atmosphere;
        app.insert_resource(level);
        app.init_resource::<Collected>()
            .add_systems(Update, collect.after(apply_glass_breach));
        app.update();
        app
    }

    fn change(app: &mut App, glass_type: GlassType, set: &str, to: u8) {
        let entity = app.world_mut().spawn_empty().id();
        app.world_mut().write_message(GlassLevelChanged {
            entity,
            from: to - 1,
            to,
            glass_type,
            point: Vec3::new(0.0, 3.0, 0.0),
            normal: Vec3::Z,
            set: set.into(),
        });
        app.update();
        app.update();
    }

    #[derive(Resource, Default)]
    struct Collected(Vec<GlassBreachFired>);

    #[derive(Resource, Default)]
    struct Shakes(Vec<CameraShakeRequest>);

    fn collect_shakes(mut reader: MessageReader<CameraShakeRequest>, mut out: ResMut<Shakes>) {
        out.0.extend(reader.read().copied());
    }

    fn collect(mut reader: MessageReader<GlassBreachFired>, mut out: ResMut<Collected>) {
        out.0.extend(reader.read().cloned());
    }

    fn fired(app: &App) -> Vec<GlassBreachFired> {
        app.world().resource::<Collected>().0.clone()
    }

    #[test]
    fn the_glass_effects_are_valid_and_registered() {
        let app = app(AtmospherePreset::Earth);
        let scene = app.world().resource::<VfxScene>();
        for def in glass_effects() {
            assert!(
                scene.library().index(&def.name).is_some(),
                "{} not registered",
                def.name
            );
            let ron = def.to_ron().expect("ron");
            assert_eq!(VfxSystemDef::from_ron(&ron).expect("round trip"), def);
        }
        // Chunks are chunky mesh fragments, not glitter.
        let chunks = glass_chunks();
        let body = &chunks.emitters[0];
        assert_eq!(body.render.mode, RenderMode::Mesh);
        assert!(body.init.size.min >= 0.03);
    }

    #[test]
    fn laminate_never_sheds_sheets_or_shards() {
        let mut app = app(AtmospherePreset::Airless);
        for level in 1..=3 {
            change(&mut app, GlassType::Laminate, "laminate", level);
        }
        assert_eq!(
            app.world().resource::<VfxScene>().live_instances(),
            0,
            "laminate spawned an effect"
        );
        assert!(fired(&app).is_empty(), "laminate breached");
    }

    #[test]
    fn an_airless_pressure_pane_failure_breaches_with_a_plume_and_a_sound_emitter() {
        let mut app = app(AtmospherePreset::Airless);
        change(&mut app, GlassType::PressurePane, "pressure_pane", 3);
        let fired = fired(&app);
        assert_eq!(fired.len(), 1, "one breach");
        let breach = &fired[0];
        assert_eq!(breach.breach.atmosphere, BreachAtmosphere::Airless);
        assert!((breach.breach.pressure - 101.3).abs() < 0.01);
        assert_eq!(breach.effect.as_deref(), Some("glass_breach_airless"));
        assert!(
            app.world().resource::<VfxScene>().live_instances() >= 2,
            "chunks + plume"
        );
        // The emitter sits at the breach; regia_framework::glass_breach makes it ground-coupled
        // (its tests check the airless medium leaves no airborne path).
        let emitter = breach.sound_emitter.expect("sound emitter");
        assert!(app.world().get::<BreachSoundEmitter>(emitter).is_some());
        let at = app
            .world()
            .get::<Transform>(emitter)
            .expect("transform")
            .translation;
        assert!((at - breach.breach.position).length() < 1e-4);
    }

    #[test]
    fn breach_presets_follow_the_atmosphere_and_an_earth_breach_is_minimal() {
        let mut mars = app(AtmospherePreset::Mars);
        change(&mut mars, GlassType::PressurePane, "pressure_pane", 3);
        assert_eq!(fired(&mars)[0].effect.as_deref(), Some("glass_breach_mars"));
        let mut earth = app(AtmospherePreset::Earth);
        change(&mut earth, GlassType::PressurePane, "pressure_pane", 3);
        let f = fired(&earth);
        // 101.3 inside vs 101.3 outside: no pressure difference, no effect, shake or sound.
        assert!(f[0].effect.is_none() && f[0].sound_emitter.is_none(), "{f:?}");
        // A game can override a preset.
        let mut custom = app(AtmospherePreset::Mars);
        custom
            .world_mut()
            .resource_mut::<GlassEffectSettings>()
            .breach_mars = Some("my_puff".into());
        custom.world_mut().write_message(GlassBreach {
            position: Vec3::ZERO,
            normal: Vec3::Z,
            atmosphere: BreachAtmosphere::Mars,
            pressure: 80.0,
        });
        custom.update();
        assert_eq!(fired(&custom)[0].effect.as_deref(), Some("my_puff"));
    }

    /// Earth at 200 kPa inside plays the gust, a clamped shake and a sound emitter.
    #[test]
    fn a_pressurized_earth_pane_plays_the_gust_and_a_clamped_shake() {
        let mut app = app(AtmospherePreset::Earth);
        app.init_resource::<Shakes>()
            .add_systems(Update, collect_shakes.after(apply_glass_breach));
        let entity = app
            .world_mut()
            .spawn(GlassPressure {
                outward: Vec3::Z,
                inside_kpa: 200.0,
            })
            .id();
        app.world_mut().write_message(GlassLevelChanged {
            entity,
            from: 2,
            to: 3,
            glass_type: GlassType::PressurePane,
            point: Vec3::new(0.0, 3.0, 0.0),
            normal: Vec3::Z,
            set: "pressure_pane".into(),
        });
        app.update();
        app.update();
        let fired = fired(&app);
        assert_eq!(fired.len(), 1);
        assert!((fired[0].breach.pressure - 98.7).abs() < 1e-3);
        assert_eq!(fired[0].effect.as_deref(), Some("glass_breach_earth"));
        assert!(fired[0].sound_emitter.is_some());
        let shakes = &app.world().resource::<Shakes>().0;
        assert_eq!(shakes.len(), 1);
        assert!((shakes[0].strength - 0.6).abs() < 1e-5, "{}", shakes[0].strength);
    }
}
