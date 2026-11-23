// SPDX-License-Identifier: MPL-2.0
//! Engine and thruster plumes, keyed to the level's air and to DO27's units.
//!
//! DO27's propulsion, from its content drop:
//!
//! | Unit | Thrusters |
//! |---|---|
//! | Gleaner | four lift/thrust ducts (`socket_duct_l0/l1/r0/r1`, pointing down) |
//! | Gleaner A52 | two rear ducts (`socket_duct_l0/r0`, pointing aft) |
//! | Kraken | ducted fans (`SM_Kraken_Fans`) |
//! | Pilots | a jetpack (`socket_jetpack`, pointing down) |
//!
//! The X11/X12/X31 hulls carry only an emissive `M_Glow` and the wright's `socket_nozzle` is its
//! construction arm, so neither has a plume. See [`DO27_THRUSTERS`].
//!
//! What a hot exhaust looks like in each air:
//!
//! - **Earth air.** The exhaust gas is clear. What shows is the heat shimmer: the hot gas bends
//!   light (its refractive index drops with its density). There is also a faint haze of
//!   combustion products and the glowing nozzle.
//! - **Mars** (0.6 kPa CO₂, about 1.5 % of Earth's density). The shimmer is negligible, since the
//!   refractive excess scales with density. The plume is thin and faint, and spreads wider
//!   because a nozzle exhausting into low ambient pressure is under-expanded. It drifts with the
//!   wind.
//! - **Airless.** The exhaust expands freely into vacuum and is invisible: no shimmer and no
//!   haze. Only the nozzle glows, and its light makes a lit spot on the ground when the unit is
//!   low (the light's radius reaches the ground).
//!
//! The effects play with the instance's forward along the exhaust (local +Z is where the gas
//! goes). Throttle is [`VfxInstance::set_intensity`](crate::sim::VfxInstance::set_intensity),
//! which also dims the nozzle's light. The ECS surface is the [`ThrusterPlume`] component on a
//! nozzle entity (a child at the socket).

use std::collections::HashMap;

use regia_app::prelude::*;
use regia_core::math::{Mat4, Vec3};

use crate::atmos_presets::{drift, keys, r, ramp};
use crate::format::{
    BlendMode, EmitterDef, InitModule, LodDef, ParticleLightDef, RenderMode, RenderModule, ShapeDef,
    SpawnMode, SpawnModule, SpriteId, SystemModule, UpdateModule, VFX_FORMAT_VERSION, VelocityDef,
    VelocityMode, VfxSystemDef,
};
use crate::medium::{VfxAtmosphere, VfxMedium};
use crate::scene::VfxScene;

/// The kind of thruster.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum PlumeKind {
    /// A lift/thrust duct or ducted fan of a hull (nozzle radius about 0.35 m).
    #[default]
    Duct,
    /// A pilot's jetpack rocket (nozzle radius about 0.12 m).
    Jetpack,
}

impl PlumeKind {
    /// Both kinds.
    pub const ALL: [Self; 2] = [Self::Duct, Self::Jetpack];

    /// Lower-case name (`duct`, `jetpack`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Duct => "duct",
            Self::Jetpack => "jetpack",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }

    /// Base effect name (`plume_duct`, `plume_jetpack`).
    #[must_use]
    pub fn effect(self) -> &'static str {
        match self {
            Self::Duct => "plume_duct",
            Self::Jetpack => "plume_jetpack",
        }
    }
}

/// One DO27 unit's thrusters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnitThrusters {
    /// The DO27 asset stem (`gleaner`, `gleaner_a52`, `kraken`, `pilot`).
    pub unit: &'static str,
    /// The plume kind.
    pub kind: PlumeKind,
    /// Socket names in the unit's manifest (empty: placed by the host, e.g. the Kraken's fans).
    pub sockets: &'static [&'static str],
    /// Exhaust direction in the unit's glTF frame (+Y up, front +Z).
    pub exhaust: (f32, f32, f32),
}

/// DO27's thrusters (content drop `units/*/manifest_entry.json`, `vehicles/kraken`,
/// `pilots/*`).
pub const DO27_THRUSTERS: &[UnitThrusters] = &[
    UnitThrusters {
        unit: "gleaner",
        kind: PlumeKind::Duct,
        sockets: &[
            "socket_duct_l0",
            "socket_duct_l1",
            "socket_duct_r0",
            "socket_duct_r1",
        ],
        exhaust: (0.0, -1.0, 0.0),
    },
    UnitThrusters {
        unit: "gleaner_a52",
        kind: PlumeKind::Duct,
        sockets: &["socket_duct_l0", "socket_duct_r0"],
        exhaust: (0.0, 0.0, -1.0),
    },
    UnitThrusters {
        unit: "kraken",
        kind: PlumeKind::Duct,
        sockets: &[],
        exhaust: (0.0, -1.0, 0.0),
    },
    UnitThrusters {
        unit: "pilot",
        kind: PlumeKind::Jetpack,
        sockets: &["socket_jetpack"],
        exhaust: (0.0, -1.0, 0.0),
    },
];

/// Per-kind sizes and speeds.
struct Spec {
    glow_size: (f32, f32),
    glow_emissive: f32,
    light_intensity: f32,
    light_radius: f32,
    speed: (f32, f32),
    haze_size: (f32, f32),
    puff_size: (f32, f32),
}

fn spec(kind: PlumeKind) -> Spec {
    match kind {
        PlumeKind::Duct => Spec {
            glow_size: (0.6, 0.8),
            glow_emissive: 2.2,
            light_intensity: 6.0,
            light_radius: 5.0,
            speed: (5.0, 8.0),
            haze_size: (0.6, 0.9),
            puff_size: (0.18, 0.3),
        },
        PlumeKind::Jetpack => Spec {
            glow_size: (0.28, 0.36),
            glow_emissive: 2.4,
            light_intensity: 2.5,
            light_radius: 3.5,
            speed: (8.0, 12.0),
            haze_size: (0.3, 0.45),
            puff_size: (0.1, 0.16),
        },
    }
}

/// The plume of `kind` with Earth-air look, named `plume_<kind>`.
#[must_use]
pub fn plume(kind: PlumeKind) -> VfxSystemDef {
    plume_in(kind, &VfxMedium::earth())
}

/// The plume of `kind` in `medium`, named for it (`plume_duct@airless`, …).
#[must_use]
pub fn plume_in(kind: PlumeKind, medium: &VfxMedium) -> VfxSystemDef {
    let s = spec(kind);
    let base = kind.effect();
    // The nozzle glows in every air; its light makes the ground-lit spot when the unit is low.
    let glow = EmitterDef {
        name: "glow".into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Rate(40.0),
            lifetime: r(0.06, 0.1),
            count_scale: 1.0,
        },
        init: InitModule {
            shape: ShapeDef::Point,
            velocity: VelocityDef {
                mode: VelocityMode::Directional {
                    dir: (0.0, 0.0, 1.0),
                    spread: 0.0,
                    speed: r(0.5, 1.0),
                },
                inherit: 1.0,
            },
            size: r(s.glow_size.0, s.glow_size.1),
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(0.0, 0.0),
            color: ramp(&[(0.0, "#ffbe7a")]),
        },
        update: UpdateModule {
            alpha_over_life: keys(&[(0.0, 0.8), (1.0, 0.0)]),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Billboard,
            sprite: SpriteId::SoftDisc,
            blend: BlendMode::Additive,
            emissive: s.glow_emissive,
            light: Some(ParticleLightDef {
                color: "#ffb070".into(),
                intensity: s.light_intensity,
                radius: s.light_radius,
            }),
            ..Default::default()
        },
    };
    let mut emitters = vec![glow];
    if medium.atmosphere == VfxAtmosphere::Earth {
        // Heat shimmer: refraction cards riding the hot jet.
        emitters.push(EmitterDef {
            name: "shimmer".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Rate(if kind == PlumeKind::Duct { 16.0 } else { 12.0 }),
                lifetime: r(0.35, 0.6),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Sphere {
                    radius: r(0.0, 0.05),
                    surface: false,
                },
                velocity: VelocityDef {
                    mode: VelocityMode::Directional {
                        dir: (0.0, 0.0, 1.0),
                        spread: 6.0,
                        speed: r(s.speed.0, s.speed.1),
                    },
                    inherit: 1.0,
                },
                size: r(s.haze_size.0, s.haze_size.1),
                rotation: r(0.0, 0.0),
                spin: r(0.0, 0.0),
                color: ramp(&[(0.0, "#ffffff")]),
            },
            update: UpdateModule {
                drag: 1.2,
                size_over_life: keys(&[(0.0, 0.8), (1.0, 2.2)]),
                alpha_over_life: keys(&[(0.0, 0.0), (0.15, 1.0), (0.7, 0.6), (1.0, 0.0)]),
                ..Default::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::SoftDisc,
                heat_haze: 5.0,
                ..Default::default()
            },
        });
    }
    if medium.atmosphere != VfxAtmosphere::Airless {
        let mars = medium.atmosphere == VfxAtmosphere::Mars;
        let drag = 1.6;
        emitters.push(EmitterDef {
            name: "exhaust".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                // Many faint puffs stretched along the jet: one continuous column, not a dotted line.
                mode: SpawnMode::Rate(66.0),
                lifetime: if mars { r(0.35, 0.65) } else { r(0.5, 0.9) },
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Sphere {
                    radius: r(0.0, 0.05),
                    surface: false,
                },
                velocity: VelocityDef {
                    mode: VelocityMode::Directional {
                        dir: (0.0, 0.0, 1.0),
                        // Under-expanded into thin air: the plume spreads wider.
                        spread: if mars { 22.0 } else { 9.0 },
                        speed: r(s.speed.0 * 1.2, s.speed.1 * 1.2),
                    },
                    inherit: 1.0,
                },
                size: r(s.puff_size.0, s.puff_size.1),
                rotation: r(0.0, std::f32::consts::TAU),
                spin: r(-0.5, 0.5),
                color: ramp(&[(0.0, "#b6b0a8")]),
            },
            update: UpdateModule {
                gravity: if mars {
                    drift(medium, 0.1, drag)
                } else {
                    (0.0, 0.35, 0.0)
                },
                drag,
                size_over_life: keys(&[(0.0, 1.0), (1.0, 4.0)]),
                alpha_over_life: if mars {
                    keys(&[(0.0, 0.0), (0.12, 0.04), (0.6, 0.02), (1.0, 0.0)])
                } else {
                    keys(&[(0.0, 0.0), (0.12, 0.07), (0.6, 0.035), (1.0, 0.0)])
                },
                ..Default::default()
            },
            render: RenderModule {
                mode: RenderMode::Stretched,
                sprite: SpriteId::PuffAnim,
                flipbook: Some(crate::showcase::puff_flipbook()),
                blend: BlendMode::Alpha,
                stretch: crate::format::StretchDef {
                    velocity_scale: 0.1,
                    min_length: -1.0,
                },
                softness: 1.5,
                lit: true,
                self_shadow: 0.4,
                screen_floor_px: 1.5,
                ..Default::default()
            },
        });
    }
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: medium.variant_name(base).unwrap_or_else(|| base.to_string()),
        system: SystemModule {
            bounds: None,
            budget: 256,
            lod: Some(LodDef {
                distances: vec![40.0, 150.0],
                rate_scale: vec![1.0, 0.5, 0.0],
            }),
        },
        emitters,
    }
}

/// A thruster on a nozzle entity: the plume follows the entity's world transform, with the
/// exhaust along [`Self::exhaust`] in the entity's frame, at [`Self::throttle`].
#[derive(Component, Clone, Debug, Reflect)]
#[reflect(Component, Default)]
pub struct ThrusterPlume {
    /// Duct or jetpack.
    pub kind: PlumeKind,
    /// 0 (idle: nothing) to 1 (full); above 1 is afterburner (clamped to 2).
    pub throttle: f32,
    /// Exhaust direction in the entity's local frame.
    pub exhaust: Vec3,
    /// Off: no new particles and no light.
    pub enabled: bool,
    /// The playing effect instance (runtime).
    #[reflect(ignore)]
    instance: Option<u64>,
    /// The effect name that instance plays.
    #[reflect(ignore)]
    playing: Option<String>,
}

impl Default for ThrusterPlume {
    fn default() -> Self {
        Self::new(PlumeKind::Duct)
    }
}

impl ThrusterPlume {
    /// A plume of `kind` at full throttle exhausting along local −Y.
    #[must_use]
    pub fn new(kind: PlumeKind) -> Self {
        Self {
            kind,
            throttle: 1.0,
            exhaust: Vec3::NEG_Y,
            enabled: true,
            instance: None,
            playing: None,
        }
    }

    /// The effect name playing (after the first update), e.g. `plume_duct@airless`.
    #[must_use]
    pub fn playing(&self) -> Option<&str> {
        self.playing.as_deref()
    }

    /// The effect instance id (after the first update).
    #[must_use]
    pub fn instance(&self) -> Option<u64> {
        self.instance
    }
}

/// Keeps one plume instance per [`ThrusterPlume`]: spawns the medium's variant (registering the
/// built-in when the library lacks it), moves it with the entity's `GlobalTransform` and sets
/// its throttle. A removed plume's instance stops and plays out, and so does a parked one (spec 37
/// § 13): it starts again when the entity's layer goes live.
#[tracing::instrument(skip_all)]
pub fn drive_thruster_plumes(
    time: Res<Time>,
    scene: Option<ResMut<VfxScene>>,
    mut plumes: Query<(
        Entity,
        &mut ThrusterPlume,
        Option<&regia_scene::GlobalTransform>,
        Has<regia_scene::layers::LayerParked>,
    )>,
    mut removed: RemovedComponents<ThrusterPlume>,
    mut live: Local<HashMap<Entity, u64>>,
) {
    let _span = tracing::debug_span!("drive_thruster_plumes").entered();
    let Some(mut scene) = scene else { return };
    for entity in removed.read() {
        if let Some(id) = live.remove(&entity) {
            scene.stop(id);
        }
    }
    let dt = time.delta_secs();
    for (entity, mut plume, transform, parked) in &mut plumes {
        if parked {
            if let Some(id) = plume.instance.take() {
                scene.stop(id);
            }
            plume.playing = None;
            live.remove(&entity);
            continue;
        }
        let base = plume.kind.effect();
        if !crate::atmos_presets::ensure_builtin(&mut scene, base) {
            continue;
        }
        let model = transform.map_or(Mat4::IDENTITY, |t| Mat4::from(t.0));
        let position = model.transform_point3(Vec3::ZERO);
        let forward = model.transform_vector3(plume.exhaust).normalize_or(Vec3::NEG_Y);
        let name = scene.resolve(base);
        let alive = plume.instance.is_some_and(|id| scene.instance(id).is_some());
        if !alive || plume.playing.as_deref() != Some(name.as_str()) {
            if let Some(old) = plume.instance.take() {
                scene.stop(old);
            }
            plume.instance = scene.spawn(base, position, forward, entity.to_bits() ^ 0x504c_554d);
            plume.playing = Some(name);
        }
        let k = if plume.enabled {
            plume.throttle.clamp(0.0, 2.0)
        } else {
            0.0
        };
        if let Some(id) = plume.instance {
            live.insert(entity, id);
            if let Some(instance) = scene.instance_mut(id) {
                instance.set_transform(position, forward, dt);
                instance.set_intensity(k);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::LodInput;

    #[test]
    fn a_parked_plume_stops_and_restarts_when_its_layer_goes_live() {
        let mut app = App::new();
        app.add_plugins(crate::plugin::VfxPlugin);
        app.insert_resource(VfxScene::new(crate::scene::VfxLibrary::new()));
        let entity = app.world_mut().spawn(ThrusterPlume::new(PlumeKind::Duct)).id();
        app.update();
        let instance = |app: &App| {
            app.world()
                .entity(entity)
                .get::<ThrusterPlume>()
                .and_then(|p| p.instance)
        };
        assert!(instance(&app).is_some(), "a live plume plays");

        app.world_mut()
            .entity_mut(entity)
            .insert(regia_scene::layers::LayerParked {
                restore: regia_scene::components::Visibility::default(),
                inherited: false,
            });
        app.update();
        assert_eq!(instance(&app), None, "a parked member shows no effect");

        app.world_mut()
            .entity_mut(entity)
            .remove::<regia_scene::layers::LayerParked>();
        app.update();
        assert!(
            instance(&app).is_some(),
            "it starts again when the layer goes live"
        );
    }

    #[test]
    fn every_plume_validates_round_trips_and_is_named_for_its_medium() {
        for medium in [VfxMedium::earth(), VfxMedium::mars(), VfxMedium::airless(1.315)] {
            for kind in PlumeKind::ALL {
                let def = plume_in(kind, &medium);
                def.validate().expect("valid");
                let back = VfxSystemDef::from_ron(&def.to_ron().expect("ron")).expect("parse");
                assert_eq!(back, def);
                assert_eq!(
                    def.name,
                    medium
                        .variant_name(kind.effect())
                        .unwrap_or(kind.effect().to_string())
                );
            }
        }
    }

    #[test]
    fn each_air_shows_what_it_should() {
        let has = |d: &VfxSystemDef, n: &str| d.emitters.iter().any(|e| e.name == n);
        let earth = plume(PlumeKind::Duct);
        assert!(has(&earth, "shimmer") && has(&earth, "exhaust") && has(&earth, "glow"));
        assert!(earth.emitters.iter().any(|e| e.render.heat_haze > 0.0));
        let mars = plume_in(PlumeKind::Duct, &VfxMedium::mars());
        assert!(!has(&mars, "shimmer"), "thin air: no visible shimmer");
        let thin = mars
            .emitters
            .iter()
            .find(|e| e.name == "exhaust")
            .expect("exhaust");
        let thick = earth
            .emitters
            .iter()
            .find(|e| e.name == "exhaust")
            .expect("exhaust");
        let peak = |e: &EmitterDef| e.update.alpha_over_life.iter().map(|k| k.v).fold(0.0, f32::max);
        assert!(peak(thin) < peak(thick), "Mars' plume is thinner");
        let vac = plume_in(PlumeKind::Jetpack, &VfxMedium::airless(1.315));
        assert_eq!(vac.emitters.len(), 1, "airless: only the nozzle glow");
        let glow = &vac.emitters[0];
        assert_eq!(glow.name, "glow");
        let light = glow.render.light.as_ref().expect("the glow lights the ground");
        assert!(light.radius >= 2.0);
    }

    #[test]
    fn do27_thrusters_name_real_sockets() {
        assert!(
            DO27_THRUSTERS
                .iter()
                .any(|t| t.unit == "gleaner" && t.sockets.len() == 4)
        );
        assert!(
            DO27_THRUSTERS
                .iter()
                .any(|t| t.unit == "pilot" && t.kind == PlumeKind::Jetpack)
        );
        for t in DO27_THRUSTERS {
            let (x, y, z) = t.exhaust;
            assert!(((x * x + y * y + z * z).sqrt() - 1.0).abs() < 1e-6);
        }
        assert_eq!(PlumeKind::from_name("jetpack"), Some(PlumeKind::Jetpack));
    }

    #[test]
    fn throttle_scales_the_plume_and_its_light() {
        let run = |k: f32| {
            let mut lib = crate::scene::VfxLibrary::new();
            lib.add(plume(PlumeKind::Duct)).expect("add");
            let mut scene = VfxScene::new(lib);
            let id = scene
                .spawn("plume_duct", Vec3::new(0.0, 1.0, 0.0), Vec3::NEG_Y, 3)
                .expect("spawn");
            let lod = LodInput {
                camera_pos: Vec3::new(0.0, 3.0, 12.0),
            };
            let mut spawned = 0u32;
            for _ in 0..60 {
                if let Some(i) = scene.instance_mut(id) {
                    i.set_intensity(k);
                }
                scene.update(1.0 / 60.0, &lod, 100_000);
                spawned += scene.last_scheduled();
            }
            let mut lights = crate::lights::VfxFrameLights::default();
            scene.gather_lights(&mut lights, 8);
            (spawned, lights.lights.len())
        };
        let (full, full_lights) = run(1.0);
        let (half, _) = run(0.5);
        let (idle, idle_lights) = run(0.0);
        assert!(full > 50 && half < full && half > full / 4, "{full} {half}");
        assert_eq!(idle, 0);
        assert_eq!(full_lights, 1);
        assert_eq!(idle_lights, 0, "an idle thruster casts no light");
    }
}
