// SPDX-License-Identifier: MPL-2.0
//! Weapon effects players see every second: hits on hulls and buildings, muzzle flashes and
//! tracers, keyed to DO27's real weapons and to the level's air.
//!
//! **Hits** ([`WeaponHit`], a command): sparks, a short flash, and a surface mark.
//! - **Metal** (hulls, guntowers): hot steel sparks on the hot-streak ramp, a few molten
//!   droplets, a wisp of burnt paint, and a molten scorch decal whose glow cools from
//!   white-yellow through orange to dull red in about a second ([`crate::decal::cooled_emissive`]).
//! - **Concrete** (buildings): angular chips (mesh debris), a fan of grit, a lit dust puff and a
//!   chip decal: a dark pit in a ring of freshly exposed, lighter aggregate.
//! - **Glass** (canopies, windows): glitter fragments and powder. When the hit names the glass
//!   entity, its `regia_render::glass::GlassDamage` gets the impact in its own mesh frame, so
//!   glass-damage stamps the crack (escalating D1 → D3 with the energy).
//!
//! **Air.** The hits, muzzles and tracers have `@airless` / `@mars` variants like the
//! explosions ([`crate::atmos_presets::variants_in`]). Airless: sparks and fragments fly ballistic
//! arcs with no drag and stay hot longer (no convective cooling, only radiation), nothing puffs
//! or lingers, and a muzzle has no secondary flash (the fuel-rich propellant gas needs oxygen
//! to burn, so in vacuum it is only the primary flash at the muzzle). Mars (0.6 kPa CO₂, no
//! oxygen): no secondary flash either, thin short smoke that drifts.
//!
//! **Daylight.** Muzzle flashes and tracers must read on Europa's bright ice in full sun
//! without blowing out. Additive light on a near-white background only pushes it to white,
//! so the readable part is drawn *over* the scene (premultiplied alpha with emissive): a
//! saturated orange tongue or tracer core replaces the background by its own colour, which
//! reads on white ice and on a black sky alike. The additive hot core is kept small and below
//! the clip point.
//!
//! [`DO27_WEAPONS`] maps every DO27 weapon (map pickups, pilot weapons, turrets) to its muzzle,
//! tracer and hit.

use regia_core::math::{Mat4, Vec3};
use regia_ecs::prelude::*;

use crate::atmos_presets::{drift, keys, r, ramp, settle};
use crate::decal::{
    CHANNEL_ALBEDO, CHANNEL_EMISSIVE, CHANNEL_NORMAL, CHANNEL_ROUGHNESS, Decal, DecalAnchor, DecalPool,
    DecalSurface, align_to_normal,
};
use crate::format::{
    BlendMode, ColorKey, DepthCollisionDef, EmitterDef, InitModule, LodDef, ParticleLightDef, RenderMode,
    RenderModule, ShapeDef, SpawnMode, SpawnModule, SpriteId, StretchDef, SystemModule, UpdateModule,
    VFX_FORMAT_VERSION, VelocityDef, VelocityMode, VfxSystemDef,
};
use crate::medium::{VfxAtmosphere, VfxMedium};
use crate::scene::VfxScene;

/// What a round hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HitSurface {
    /// Hulls, turrets, metal walls.
    Metal,
    /// Building walls (concrete, ceramic armour).
    Concrete,
    /// Canopies and windows.
    Glass,
}

impl HitSurface {
    /// Every surface.
    pub const ALL: [Self; 3] = [Self::Metal, Self::Concrete, Self::Glass];

    /// Lower-case name (`metal`, `concrete`, `glass`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Metal => "metal",
            Self::Concrete => "concrete",
            Self::Glass => "glass",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The base effect name (`hit_metal`, …).
    #[must_use]
    pub fn effect(self) -> &'static str {
        match self {
            Self::Metal => "hit_metal",
            Self::Concrete => "hit_concrete",
            Self::Glass => "hit_glass",
        }
    }
}

/// How big a round is: scales the hit effect and its decal, and the energy it puts into glass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Calibre {
    /// Pistol and rifle rounds.
    Light,
    /// Cannon and heavy rifle rounds.
    Medium,
    /// Turret and anti-materiel rounds.
    Heavy,
}

impl Calibre {
    /// Lower-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Medium => "medium",
            Self::Heavy => "heavy",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Light, Self::Medium, Self::Heavy]
            .into_iter()
            .find(|c| c.name() == name)
    }

    /// Spatial scale of the hit effect and decal (the effects are authored for [`Self::Medium`]).
    #[must_use]
    pub fn scale(self) -> f32 {
        match self {
            Self::Light => 0.6,
            Self::Medium => 1.0,
            Self::Heavy => 1.6,
        }
    }

    /// Impact energy for glass escalation. The built-in laminate reaches D1 at cumulative
    /// energy 1, D2 at 4 and D3 at 10, and holes from 3 per hit at D3. Any recorded impact
    /// whose level is still 0 is stamped at least D1: the damage pass raises the resolved
    /// level with `.max(1)`, so one light hit (0.6) cracks the pane before the energy sum
    /// reaches 1.
    #[must_use]
    pub fn glass_energy(self) -> f32 {
        match self {
            Self::Light => 0.6,
            Self::Medium => 1.5,
            Self::Heavy => 4.0,
        }
    }
}

/// What a weapon's round does where it lands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HitKind {
    /// A bullet of this calibre: [`WeaponHit`] plays the surface's hit.
    Bullet(Calibre),
    /// An explosive round: the host plays `explosion` at this scale (the atmosphere-aware
    /// explosion), plus a [`WeaponHit`] of the calibre for the surface mark.
    Explosive(f32),
    /// A continuous beam: a light metal hit per damage tick.
    Beam,
}

/// One DO27 weapon's effects.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WeaponFx {
    /// The DO27 weapon id (`chaincannon`, `rifle`, …).
    pub weapon: &'static str,
    /// Muzzle effect, if it has one.
    pub muzzle: Option<&'static str>,
    /// Tracer effect, if its rounds carry tracer.
    pub tracer: Option<&'static str>,
    /// What lands.
    pub hit: HitKind,
}

/// DO27's weapons: the eight map pickups (`maps/*/layout.json` `weapon_pickups`), the pilot
/// weapons (`pilots/`) and the two turrets (`units/sentry`, `buildings/guntower`, both with
/// muzzle sockets).
pub const DO27_WEAPONS: &[WeaponFx] = &[
    WeaponFx {
        weapon: "rifle",
        muzzle: Some("muzzle_flash"),
        tracer: Some("tracer"),
        hit: HitKind::Bullet(Calibre::Light),
    },
    WeaponFx {
        weapon: "sidearm",
        muzzle: Some("muzzle_flash"),
        tracer: None,
        hit: HitKind::Bullet(Calibre::Light),
    },
    WeaponFx {
        weapon: "support",
        muzzle: Some("muzzle_flash"),
        tracer: Some("tracer"),
        hit: HitKind::Bullet(Calibre::Light),
    },
    WeaponFx {
        weapon: "sniper",
        muzzle: Some("muzzle_flash"),
        tracer: None,
        hit: HitKind::Bullet(Calibre::Heavy),
    },
    WeaponFx {
        weapon: "chaincannon",
        muzzle: Some("muzzle_cannon"),
        tracer: Some("tracer_heavy"),
        hit: HitKind::Bullet(Calibre::Medium),
    },
    WeaponFx {
        weapon: "spikegun",
        muzzle: Some("muzzle_flash"),
        tracer: None,
        hit: HitKind::Bullet(Calibre::Medium),
    },
    WeaponFx {
        weapon: "ricochet",
        muzzle: Some("muzzle_flash"),
        tracer: Some("tracer"),
        hit: HitKind::Bullet(Calibre::Medium),
    },
    WeaponFx {
        weapon: "thumpgun",
        muzzle: Some("muzzle_launcher"),
        tracer: None,
        hit: HitKind::Explosive(0.5),
    },
    WeaponFx {
        weapon: "arcmortar",
        muzzle: Some("muzzle_launcher"),
        tracer: None,
        hit: HitKind::Explosive(0.8),
    },
    WeaponFx {
        weapon: "quakeshell",
        muzzle: Some("muzzle_launcher"),
        tracer: None,
        hit: HitKind::Explosive(1.0),
    },
    WeaponFx {
        weapon: "heatseeker",
        muzzle: Some("muzzle_launcher"),
        tracer: None,
        hit: HitKind::Explosive(0.7),
    },
    WeaponFx {
        weapon: "frag",
        muzzle: None,
        tracer: None,
        hit: HitKind::Explosive(0.6),
    },
    WeaponFx {
        weapon: "cutterbeam",
        muzzle: None,
        tracer: None,
        hit: HitKind::Beam,
    },
    WeaponFx {
        weapon: "sentry",
        muzzle: Some("muzzle_cannon"),
        tracer: Some("tracer_heavy"),
        hit: HitKind::Bullet(Calibre::Medium),
    },
    WeaponFx {
        weapon: "guntower",
        muzzle: Some("muzzle_cannon"),
        tracer: Some("tracer_heavy"),
        hit: HitKind::Bullet(Calibre::Heavy),
    },
];

/// The effects of DO27 weapon `weapon`.
#[must_use]
pub fn weapon_fx(weapon: &str) -> Option<&'static WeaponFx> {
    DO27_WEAPONS.iter().find(|w| w.weapon == weapon)
}

/// The calibre of a weapon's hit (beams are light; explosives are heavy).
#[must_use]
pub fn weapon_calibre(weapon: &str) -> Option<Calibre> {
    weapon_fx(weapon).map(|w| match w.hit {
        HitKind::Bullet(c) => c,
        HitKind::Explosive(_) => Calibre::Heavy,
        HitKind::Beam => Calibre::Light,
    })
}

/// Base names of every combat effect (hits, muzzles, tracers).
pub const COMBAT_EFFECTS: [&str; 8] = [
    "hit_metal",
    "hit_concrete",
    "hit_glass",
    "muzzle_flash",
    "muzzle_cannon",
    "muzzle_launcher",
    "tracer",
    "tracer_heavy",
];

/// The combat effect `base` in `medium`, named for it (`hit_metal@airless`, …); `None` for a
/// name that is not a combat effect. Tracers burn their own oxidiser, so they look the same in
/// every air and have no variant (the base plays).
#[must_use]
pub fn combat_effect_in(base: &str, medium: &VfxMedium) -> Option<VfxSystemDef> {
    let mut def = match base {
        "hit_metal" => hit_metal(medium),
        "hit_concrete" => hit_concrete(medium),
        "hit_glass" => hit_glass(medium),
        "muzzle_flash" => muzzle_in(crate::showcase::muzzle_flash(), medium),
        "muzzle_cannon" => muzzle_in(muzzle_cannon(), medium),
        "muzzle_launcher" => muzzle_in(muzzle_launcher(), medium),
        "tracer" => return Some(crate::showcase::tracer()),
        "tracer_heavy" => return Some(tracer_heavy()),
        _ => return None,
    };
    def.name = medium.variant_name(base).unwrap_or_else(|| base.to_string());
    Some(def)
}

fn system(name: &str, budget: u32, emitters: Vec<EmitterDef>) -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: name.into(),
        system: SystemModule {
            bounds: None,
            budget,
            lod: Some(LodDef {
                distances: vec![40.0, 150.0],
                rate_scale: vec![1.0, 0.5, 0.0],
            }),
        },
        emitters,
    }
}

/// A small flare at the hit point (the round's kinetic flash).
fn hit_flash(size: (f32, f32), emissive: f32, color: &str, light: Option<ParticleLightDef>) -> EmitterDef {
    EmitterDef {
        name: "flash".into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Burst(1),
            lifetime: r(0.035, 0.055),
            count_scale: 1.0,
        },
        init: InitModule {
            shape: ShapeDef::Point,
            velocity: VelocityDef::default(),
            size: r(size.0, size.1),
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(0.0, 0.0),
            color: ramp(&[(0.0, color)]),
        },
        update: UpdateModule {
            alpha_over_life: keys(&[(0.0, 1.0), (1.0, 0.0)]),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Billboard,
            sprite: SpriteId::Flare,
            blend: BlendMode::Additive,
            emissive,
            light,
            ..Default::default()
        },
    }
}

/// Fragments thrown out of the surface (local +Z is the surface normal).
#[allow(clippy::too_many_arguments)]
fn fragments(
    name: &str,
    count: u32,
    spread: f32,
    speed: (f32, f32),
    size: (f32, f32),
    life: (f32, f32),
    color: &str,
    medium: &VfxMedium,
    air_drag: f32,
) -> EmitterDef {
    let airless = medium.atmosphere == VfxAtmosphere::Airless;
    EmitterDef {
        name: name.into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Burst(count),
            lifetime: r(life.0, life.1),
            count_scale: 1.0,
        },
        init: InitModule {
            shape: ShapeDef::Sphere {
                radius: r(0.0, 0.03),
                surface: false,
            },
            velocity: VelocityDef {
                mode: VelocityMode::Directional {
                    dir: (0.0, 0.0, 1.0),
                    spread,
                    speed: r(speed.0, speed.1),
                },
                inherit: 0.0,
            },
            size: r(size.0, size.1),
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(-12.0, 12.0),
            color: ramp(&[(0.0, color)]),
        },
        update: UpdateModule {
            gravity: medium.gravity_vec(),
            drag: if airless {
                0.0
            } else if medium.atmosphere == VfxAtmosphere::Mars {
                air_drag * 0.15
            } else {
                air_drag
            },
            alpha_over_life: keys(&[(0.0, 1.0), (0.85, 1.0), (1.0, 0.0)]),
            depth_collision: Some(DepthCollisionDef {
                kill: true,
                offset: 0.05,
            }),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Mesh,
            sprite: SpriteId::Shard,
            blend: BlendMode::Alpha,
            lit: true,
            ..Default::default()
        },
    }
}

/// A lit puff out of the hit (dust, burnt paint, glass powder). Airless: a ballistic clump
/// that grows linearly and thins out (no billow, nothing lingers). Mars: longer, drifting.
#[allow(clippy::too_many_arguments)]
fn hit_puff(
    name: &str,
    count: u32,
    size: (f32, f32),
    life: (f32, f32),
    colors: (&str, &str),
    alpha: f32,
    lift: f32,
    medium: &VfxMedium,
) -> EmitterDef {
    let mut e = EmitterDef {
        name: name.into(),
        delay: 0.01,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Burst(count),
            lifetime: r(life.0, life.1),
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
                    spread: 40.0,
                    speed: r(1.2, 3.5),
                },
                inherit: 0.0,
            },
            size: r(size.0, size.1),
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(-0.5, 0.5),
            color: ramp(&[(0.0, colors.0), (1.0, colors.1)]),
        },
        update: UpdateModule {
            gravity: (0.0, lift, 0.0),
            drag: 2.6,
            curl: Some((0.9, 0.4, 0.25)),
            size_over_life: keys(&[(0.0, 1.0), (0.3, 2.4), (1.0, 3.6)]),
            alpha_over_life: keys(&[(0.0, 0.0), (0.06, alpha), (0.5, alpha * 0.55), (1.0, 0.0)]),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Billboard,
            sprite: SpriteId::PuffAnim,
            flipbook: Some(crate::showcase::puff_flipbook()),
            blend: BlendMode::Alpha,
            softness: 0.9,
            lit: true,
            self_shadow: 0.55,
            screen_floor_px: 1.5,
            ..Default::default()
        },
    };
    match medium.atmosphere {
        VfxAtmosphere::Earth => {}
        VfxAtmosphere::Mars => {
            e.spawn.lifetime = r(life.0 * 1.8, life.1 * 1.8);
            e.update.drag = 1.2;
            e.update.gravity = drift(medium, lift * 0.3, 1.2);
            e.update.alpha_over_life =
                keys(&[(0.0, 0.0), (0.06, alpha * 0.8), (0.5, alpha * 0.5), (1.0, 0.0)]);
        }
        VfxAtmosphere::Airless => {
            e.spawn.lifetime = r(life.0 * 0.8, life.1 * 0.8);
            e.update.gravity = medium.gravity_vec();
            e.update.drag = 0.0;
            e.update.curl = None;
            e.update.size_over_life = keys(&[(0.0, 1.0), (1.0, 3.0)]);
            e.update.alpha_over_life = keys(&[
                (0.0, 0.0),
                (0.04, alpha * 0.8),
                (0.3, alpha * 0.35),
                (0.6, alpha * 0.1),
                (1.0, 0.0),
            ]);
            e.update.depth_collision = Some(DepthCollisionDef {
                kill: true,
                offset: 0.05,
            });
            e.render.sprite = SpriteId::SoftDisc;
            e.render.flipbook = None;
            e.render.softness = 0.5;
        }
    }
    e
}

/// Hot sparks off steel: streaks on the hot-streak ramp. Airless they fly drag-free under the
/// level's gravity and glow longer (they cool by radiation only).
fn hot_sparks(name: &str, count: u32, speed: (f32, f32), life: (f32, f32), medium: &VfxMedium) -> EmitterDef {
    let (drag, life) = match medium.atmosphere {
        VfxAtmosphere::Earth => (1.0, life),
        VfxAtmosphere::Mars => (0.15, (life.0 * 1.2, life.1 * 1.2)),
        VfxAtmosphere::Airless => (0.0, (life.0 * 1.6, life.1 * 1.6)),
    };
    EmitterDef {
        name: name.into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Burst(count),
            lifetime: r(life.0, life.1),
            count_scale: 1.0,
        },
        init: InitModule {
            shape: ShapeDef::Point,
            velocity: VelocityDef {
                mode: VelocityMode::Directional {
                    dir: (0.0, 0.0, 1.0),
                    spread: 75.0,
                    speed: r(speed.0, speed.1),
                },
                inherit: 0.0,
            },
            size: r(0.018, 0.035),
            rotation: r(0.0, 0.0),
            spin: r(0.0, 0.0),
            // White times `color_over_life` is the ramp.
            color: ramp(&[(0.0, "#ffffff")]),
        },
        update: UpdateModule {
            gravity: medium.gravity_vec(),
            drag,
            color_over_life: crate::showcase::hot_streak_ramp(),
            alpha_over_life: keys(&[(0.0, 1.0), (0.7, 0.85), (1.0, 0.0)]),
            depth_collision: Some(DepthCollisionDef {
                kill: true,
                offset: 0.05,
            }),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Stretched,
            sprite: SpriteId::SoftDisc,
            blend: BlendMode::Additive,
            emissive: 3.0,
            stretch: StretchDef {
                velocity_scale: 0.035,
                min_length: 0.06,
            },
            ..Default::default()
        },
    }
}

/// A round on metal: flash, sparks, molten droplets and (in air) a wisp of burnt paint.
#[must_use]
pub fn hit_metal(medium: &VfxMedium) -> VfxSystemDef {
    let airless = medium.atmosphere == VfxAtmosphere::Airless;
    let mut emitters = vec![
        hit_flash(
            (0.45, 0.6),
            2.5,
            "#fff0cc",
            Some(ParticleLightDef {
                color: "#ffd9a0".into(),
                intensity: 2.0,
                radius: 3.0,
            }),
        ),
        hot_sparks("sparks", 28, (5.0, 16.0), (0.25, 0.7), medium),
    ];
    let mut molten = hot_sparks("molten", 6, (1.5, 4.5), (0.6, 1.2), medium);
    molten.init.size = r(0.03, 0.05);
    if let VelocityMode::Directional { spread, .. } = &mut molten.init.velocity.mode {
        *spread = 50.0;
    }
    molten.update.drag *= 0.3;
    molten.update.color_over_life = ramp(&[(0.0, "#ffd28a"), (0.5, "#ffa64a"), (1.0, "#e07a2a")]);
    molten.update.depth_collision = settle(0.02);
    molten.render.mode = RenderMode::Billboard;
    molten.render.emissive = 2.2;
    emitters.push(molten);
    if !airless {
        emitters.push(hit_puff(
            "smoke",
            4,
            (0.18, 0.35),
            (0.8, 1.4),
            ("#77736e", "#8c8883"),
            0.35,
            0.35,
            medium,
        ));
    }
    system("hit_metal", 256, emitters)
}

/// A round on concrete: a dim flash, chips, a fan of grit and a lit dust puff (airless: a
/// ballistic clump).
#[must_use]
pub fn hit_concrete(medium: &VfxMedium) -> VfxSystemDef {
    let mut grit = fragments(
        "grit",
        26,
        60.0,
        (6.0, 14.0),
        (0.012, 0.025),
        (0.2, 0.45),
        "#a9a49b",
        medium,
        1.5,
    );
    grit.render.mode = RenderMode::Stretched;
    grit.render.sprite = SpriteId::SoftDisc;
    grit.render.stretch = StretchDef {
        velocity_scale: 0.03,
        min_length: 0.012,
    };
    grit.render.self_shadow = 0.3;
    grit.render.screen_floor_px = 1.0;
    grit.init.spin = r(0.0, 0.0);
    let chips = fragments(
        "chips",
        10,
        55.0,
        (3.0, 9.0),
        (0.02, 0.05),
        if medium.atmosphere == VfxAtmosphere::Airless {
            (1.5, 3.5)
        } else {
            (0.6, 1.3)
        },
        "#8d8880",
        medium,
        0.3,
    );
    let emitters = vec![
        hit_flash((0.3, 0.4), 1.6, "#fff2d8", None),
        chips,
        grit,
        hit_puff(
            "dust",
            7,
            (0.25, 0.5),
            (1.2, 2.2),
            ("#a8a39a", "#b8b3ab"),
            0.55,
            0.15,
            medium,
        ),
    ];
    system("hit_concrete", 256, emitters)
}

/// A round on glass: a tiny flash, glitter fragments that catch the sun, and powder (none
/// airless). The crack itself is glass-damage's ([`WeaponHit::target`]).
#[must_use]
pub fn hit_glass(medium: &VfxMedium) -> VfxSystemDef {
    let mut glitter = fragments(
        "glitter",
        22,
        70.0,
        (1.5, 5.5),
        (0.008, 0.02),
        if medium.atmosphere == VfxAtmosphere::Airless {
            (1.0, 2.5)
        } else {
            (0.4, 0.9)
        },
        "#e6f0f5d8",
        medium,
        0.4,
    );
    glitter.init.spin = r(-14.0, 14.0);
    let sparkle = EmitterDef {
        name: "sparkle".into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Burst(8),
            lifetime: r(0.08, 0.2),
            count_scale: 1.0,
        },
        init: InitModule {
            shape: ShapeDef::Sphere {
                radius: r(0.0, 0.08),
                surface: false,
            },
            velocity: VelocityDef {
                mode: VelocityMode::Directional {
                    dir: (0.0, 0.0, 1.0),
                    spread: 70.0,
                    speed: r(1.0, 4.0),
                },
                inherit: 0.0,
            },
            size: r(0.04, 0.08),
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(0.0, 0.0),
            color: ramp(&[(0.0, "#f6fbff")]),
        },
        update: UpdateModule {
            gravity: medium.gravity_vec(),
            alpha_over_life: keys(&[(0.0, 0.0), (0.3, 1.0), (1.0, 0.0)]),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Billboard,
            sprite: SpriteId::Flare,
            blend: BlendMode::Additive,
            emissive: 1.0,
            ..Default::default()
        },
    };
    let mut emitters = vec![hit_flash((0.25, 0.35), 1.4, "#f4f8ff", None), glitter, sparkle];
    if medium.atmosphere != VfxAtmosphere::Airless {
        emitters.push(hit_puff(
            "powder",
            4,
            (0.1, 0.2),
            (0.5, 0.9),
            ("#eef3f6", "#f6f9fb"),
            0.3,
            0.1,
            medium,
        ));
    }
    system("hit_glass", 256, emitters)
}

/// A cannon's muzzle (chaincannon, sentry, guntower): a small hot core, saturated flame tongues
/// drawn over the scene (they read on bright ice), bore sparks, a lit smoke puff and a short
/// light. Same instance convention as [`crate::showcase::muzzle_flash`] (forward = −fire).
#[must_use]
pub fn muzzle_cannon() -> VfxSystemDef {
    let mut def = crate::showcase::muzzle_flash();
    def.name = "muzzle_cannon".into();
    def.system.budget = 384;
    for e in &mut def.emitters {
        match e.name.as_str() {
            "flare" => {
                e.init.size = r(1.1, 1.4);
                e.spawn.lifetime = r(0.06, 0.09);
                e.render.light = Some(ParticleLightDef {
                    color: "#ffd28a".into(),
                    intensity: 4.0,
                    radius: 6.0,
                });
            }
            "tongues" => {
                e.spawn.mode = SpawnMode::Burst(7);
                e.init.size = r(0.22, 0.34);
                if let VelocityMode::Directional { speed, .. } = &mut e.init.velocity.mode {
                    *speed = r(22.0, 40.0);
                }
                e.render.stretch.min_length = 0.45;
            }
            "flash" => {
                e.spawn.mode = SpawnMode::Burst(20);
            }
            "core" => {
                e.init.size = r(0.8, 1.05);
            }
            "smoke_wisp" => {
                e.spawn.mode = SpawnMode::Burst(14);
                e.init.size = r(0.3, 0.6);
                e.spawn.lifetime = r(0.6, 1.3);
            }
            _ => {}
        }
    }
    def
}

/// A launcher's muzzle (thumpgun, arcmortar, quakeshell, heatseeker): a dim flash and a big lit
/// gas puff, little flame. Forward = −fire.
#[must_use]
pub fn muzzle_launcher() -> VfxSystemDef {
    let mut def = crate::showcase::muzzle_flash();
    def.name = "muzzle_launcher".into();
    def.emitters.retain(|e| e.name != "flash");
    for e in &mut def.emitters {
        match e.name.as_str() {
            "flare" => {
                e.init.size = r(0.6, 0.8);
                e.render.emissive = 1.6;
            }
            "tongues" => {
                e.spawn.mode = SpawnMode::Burst(3);
                e.render.emissive = 1.2;
            }
            "smoke_wisp" => {
                e.spawn.mode = SpawnMode::Burst(18);
                e.init.size = r(0.35, 0.7);
                e.spawn.lifetime = r(0.8, 1.6);
                if let VelocityMode::Directional { speed, spread, .. } = &mut e.init.velocity.mode {
                    *speed = r(2.0, 7.0);
                    *spread = 35.0;
                }
            }
            _ => {}
        }
    }
    def
}

/// `def` (a muzzle) in `medium`: without oxygen there is no secondary flash, so the flare and
/// tongues shrink and shorten; airless, the propellant gas expands to nothing at once (no wisp).
fn muzzle_in(mut def: VfxSystemDef, medium: &VfxMedium) -> VfxSystemDef {
    match medium.atmosphere {
        VfxAtmosphere::Earth => {}
        VfxAtmosphere::Mars | VfxAtmosphere::Airless => {
            let airless = medium.atmosphere == VfxAtmosphere::Airless;
            if airless {
                def.emitters.retain(|e| e.name != "smoke_wisp");
            }
            for e in &mut def.emitters {
                match e.name.as_str() {
                    // The core is the primary flash, which vacuum keeps; the flare and tongues
                    // are mostly the secondary flash, which needs oxygen.
                    "flare" | "tongues" => {
                        e.init.size = r(e.init.size.min * 0.75, e.init.size.max * 0.75);
                        e.spawn.lifetime = r(e.spawn.lifetime.min * 0.7, e.spawn.lifetime.max * 0.7);
                    }
                    "flash" if airless => {
                        e.update.drag = 0.0;
                    }
                    _ => {}
                }
                if airless && e.name == "tongues" {
                    // No drag in vacuum: the gas expands freely into a wider, fading plume
                    // instead of being slowed into short tongues.
                    e.update.drag = 0.0;
                    if let VelocityMode::Directional { spread, .. } = &mut e.init.velocity.mode {
                        *spread = 35.0;
                    }
                    e.update.alpha_over_life = keys(&[(0.0, 1.0), (0.4, 0.5), (1.0, 0.0)]);
                }
                if e.name == "smoke_wisp" {
                    // Mars: thin and short, drifting.
                    e.spawn.lifetime = r(e.spawn.lifetime.min * 0.6, e.spawn.lifetime.max * 0.6);
                    e.update.gravity = drift(medium, 0.2, e.update.drag);
                    for key in &mut e.update.alpha_over_life {
                        key.v *= 0.5;
                    }
                }
            }
        }
    }
    def
}

/// A heavy tracer (chaincannon, sentry, guntower): a thicker, longer core than
/// [`crate::showcase::tracer`] at a cannon's rate of fire.
#[must_use]
pub fn tracer_heavy() -> VfxSystemDef {
    let mut def = crate::showcase::tracer();
    def.name = "tracer_heavy".into();
    for e in &mut def.emitters {
        e.spawn.mode = SpawnMode::Rate(10.0);
        e.init.size = r(0.2, 0.28);
        e.init.color = vec![ColorKey {
            t: 0.0,
            color: "#ffa22c".into(),
        }];
        if let VelocityMode::Directional { speed, .. } = &mut e.init.velocity.mode {
            *speed = r(60.0, 75.0);
        }
        e.render.stretch.min_length = 2.2;
    }
    def
}

/// One round landing: the effect, the surface mark and (on glass) the crack.
///
/// Queue it as a command (`commands.queue(hit)`) or apply it with [`apply_weapon_hit`]. It is a
/// command rather than a message so registering it changes no message flush order.
#[derive(Clone, Debug, PartialEq)]
pub struct WeaponHit {
    /// Hit point, world metres.
    pub position: Vec3,
    /// Surface normal at the hit (toward the shooter's side), world.
    pub normal: Vec3,
    /// What it hit.
    pub surface: HitSurface,
    /// Round size.
    pub calibre: Calibre,
    /// The entity hit, when known: decals ride it ([`DecalAnchor`]) and, on glass, its
    /// `GlassDamage` takes the impact.
    pub target: Option<Entity>,
    /// Variation seed (`None`: derived from the position).
    pub seed: Option<u64>,
}

impl WeaponHit {
    /// A hit by DO27 weapon `weapon` ([`weapon_calibre`]; unknown weapons hit as medium).
    #[must_use]
    pub fn by_weapon(weapon: &str, position: Vec3, normal: Vec3, surface: HitSurface) -> Self {
        Self {
            position,
            normal,
            surface,
            calibre: weapon_calibre(weapon).unwrap_or(Calibre::Medium),
            target: None,
            seed: None,
        }
    }
}

impl Command for WeaponHit {
    type Out = ();

    fn apply(self, world: &mut World) {
        apply_weapon_hit(world, &self);
    }
}

/// What [`apply_weapon_hit`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HitOutcome {
    /// The effect that played (`hit_metal@airless`, …), if a scene was present.
    pub effect: Option<String>,
    /// Decals placed.
    pub decals: usize,
    /// The glass impact index queued on the target's `GlassDamage`.
    pub glass_impact: Option<usize>,
}

/// Plays a [`WeaponHit`]: spawns the surface's hit effect (the medium's variant, registering the
/// built-in when the library lacks it) scaled to the calibre, places its decals (on the target
/// when given), and queues a glass impact on a glass target.
pub fn apply_weapon_hit(world: &mut World, hit: &WeaponHit) -> HitOutcome {
    let mut out = HitOutcome::default();
    let normal = hit.normal.normalize_or(Vec3::Y);
    let seed = hit.seed.unwrap_or_else(|| {
        let p = hit.position.to_array();
        p.iter().fold(0x9e37_79b9_7f4a_7c15u64, |h, v| {
            (h ^ u64::from(v.to_bits())).wrapping_mul(0x100_0000_01b3)
        })
    });
    let model = hit.target.and_then(|e| {
        world
            .get::<regia_scene::GlobalTransform>(e)
            .map(|t| Mat4::from(t.0))
    });
    if let Some(mut scene) = world.get_resource_mut::<VfxScene>() {
        let base = hit.surface.effect();
        if crate::atmos_presets::ensure_builtin(&mut scene, base) {
            let name = scene.resolve(base);
            if let Some(id) = scene.spawn(base, hit.position, normal, seed) {
                if let Some(instance) = scene.instance_mut(id) {
                    instance.set_scale(hit.calibre.scale());
                }
                out.effect = Some(name);
            }
        }
    }
    let decals = hit_decals(hit.surface, hit.calibre, hit.position, normal, seed);
    if !decals.is_empty()
        && let Some(mut pool) = world.get_resource_mut::<DecalPool>()
    {
        for (decal, surface) in decals {
            match (hit.target, model) {
                (Some(entity), Some(m)) => {
                    let anchor = DecalAnchor::from_world(entity, m, decal.position, decal.rotation);
                    pool.spawn_anchored(decal, surface, anchor);
                }
                _ => {
                    pool.spawn_with(decal, surface);
                }
            }
            out.decals += 1;
        }
    }
    if hit.surface == HitSurface::Glass
        && let Some(entity) = hit.target
    {
        let local = model.map_or(hit.position, |m| m.inverse().transform_point3(hit.position));
        if let Some(mut damage) = world.get_mut::<regia_render::glass::GlassDamage>(entity) {
            out.glass_impact = Some(damage.impact(local, hit.calibre.glass_energy()));
        }
    }
    out
}

/// The decals of a hit: on metal a dark scorch plus a small molten glow that cools; on concrete
/// a chip (dark pit, lighter fresh aggregate around it); none on glass (glass-damage stamps the
/// crack).
#[must_use]
pub fn hit_decals(
    surface: HitSurface,
    calibre: Calibre,
    position: Vec3,
    normal: Vec3,
    seed: u64,
) -> Vec<(Decal, DecalSurface)> {
    let s = calibre.scale();
    let yaw = (seed % 6283) as f32 * 0.001;
    let rotation = align_to_normal(normal, yaw);
    let decal = |half: f32, depth: f32, lifetime: Option<f32>| Decal {
        position,
        rotation,
        half_size: Vec3::new(half, half, depth),
        color: [1.0, 1.0, 1.0, 1.0],
        age: 0.0,
        lifetime,
    };
    let base = DecalSurface {
        albedo: [1.0, 1.0, 1.0],
        opacity: 1.0,
        roughness: 1.0,
        normal_xy: [0.0, 0.0],
        normal_strength: 0.0,
        emissive: [0.0, 0.0, 0.0],
        angle_fade: [0.15, 0.4],
        distance_fade: [30.0, 70.0],
        priority: 3,
        receiver_mask: u32::MAX,
        atlas_layer: 0,
        channels: CHANNEL_ALBEDO,
        edge_softness: 0.85,
        blend: 0.0,
        cooling_s: 0.0,
        metallic: 0.0,
        shading: crate::decal::DecalShading::Auto,
    };
    match surface {
        HitSurface::Metal => vec![
            // Burnt paint and oxide around the strike.
            (
                decal(0.15 * s, 0.14, None),
                DecalSurface {
                    albedo: [0.10, 0.075, 0.06],
                    opacity: 0.9,
                    roughness: 0.9,
                    normal_strength: 0.6,
                    channels: CHANNEL_ALBEDO | CHANNEL_NORMAL | CHANNEL_ROUGHNESS,
                    ..base
                },
            ),
            // The molten spot: emissive only, cooling (red lasts, green and blue go first).
            (
                decal(0.06 * s, 0.12, Some(2.2)),
                DecalSurface {
                    emissive: [4.0, 2.0, 0.5],
                    priority: 4,
                    channels: CHANNEL_EMISSIVE,
                    cooling_s: 0.45,
                    edge_softness: 1.0,
                    ..base
                },
            ),
        ],
        HitSurface::Concrete => vec![(
            decal(0.12 * s, 0.14, None),
            // Paint mode: the scorch tile's dark core is the pit, its light rim the fresh
            // aggregate (lighter than weathered concrete).
            DecalSurface {
                albedo: [0.62, 0.6, 0.56],
                opacity: 0.95,
                roughness: 1.0,
                normal_strength: 0.8,
                channels: CHANNEL_ALBEDO | CHANNEL_NORMAL,
                blend: 1.0,
                ..base
            },
        )],
        HitSurface::Glass => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quality::in_fire_family;

    #[test]
    fn every_combat_effect_validates_round_trips_and_is_named_for_its_medium() {
        for medium in [VfxMedium::earth(), VfxMedium::mars(), VfxMedium::airless(1.315)] {
            for base in COMBAT_EFFECTS {
                let def = combat_effect_in(base, &medium).expect(base);
                def.validate().unwrap_or_else(|e| panic!("{}: {e}", def.name));
                let back = VfxSystemDef::from_ron(&def.to_ron().expect("ron")).expect("parse");
                assert_eq!(back, def);
                let tracer = base.starts_with("tracer");
                let expected = if tracer {
                    base.to_string()
                } else {
                    medium.variant_name(base).unwrap_or(base.to_string())
                };
                assert_eq!(def.name, expected);
            }
        }
        assert!(combat_effect_in("nope", &VfxMedium::earth()).is_none());
    }

    #[test]
    fn every_do27_weapon_maps_to_real_effects() {
        let earth = VfxMedium::earth();
        for name in [
            "chaincannon",
            "spikegun",
            "thumpgun",
            "arcmortar",
            "heatseeker",
            "ricochet",
            "quakeshell",
            "cutterbeam",
            "rifle",
            "sidearm",
            "sniper",
            "support",
            "frag",
            "sentry",
            "guntower",
        ] {
            let fx = weapon_fx(name).unwrap_or_else(|| panic!("{name} missing"));
            for effect in [fx.muzzle, fx.tracer].into_iter().flatten() {
                assert!(combat_effect_in(effect, &earth).is_some(), "{name}: {effect}");
            }
            assert!(weapon_calibre(name).is_some());
        }
        assert_eq!(weapon_calibre("guntower"), Some(Calibre::Heavy));
        assert_eq!(weapon_calibre("rifle"), Some(Calibre::Light));
    }

    #[test]
    fn airless_hits_have_no_drag_no_puff_and_level_gravity() {
        let medium = VfxMedium::airless(1.315);
        for surface in HitSurface::ALL {
            let def = combat_effect_in(surface.effect(), &medium).expect("def");
            for e in &def.emitters {
                assert_eq!(e.update.drag, 0.0, "{}/{}", def.name, e.name);
                assert!(e.update.curl.is_none(), "{}/{} billows", def.name, e.name);
                assert_ne!(
                    e.render.sprite,
                    SpriteId::PuffAnim,
                    "{}/{} puffs",
                    def.name,
                    e.name
                );
                assert!(e.update.gravity.1 <= 0.0, "{}/{} rises", def.name, e.name);
                if e.update.gravity.1 < 0.0 {
                    assert!((e.update.gravity.1 + 1.315).abs() < 1e-6);
                }
            }
        }
        // Airless sparks stay hot longer than in air (radiative cooling only).
        let spark_life = |m: &VfxMedium| {
            hit_metal(m)
                .emitters
                .into_iter()
                .find(|e| e.name == "sparks")
                .map(|e| e.spawn.lifetime.max)
                .unwrap_or_default()
        };
        assert!(spark_life(&medium) > spark_life(&VfxMedium::earth()));
    }

    #[test]
    fn airless_muzzles_have_no_wisp_and_a_smaller_flash() {
        let earth = combat_effect_in("muzzle_cannon", &VfxMedium::earth()).expect("earth");
        let vac = combat_effect_in("muzzle_cannon", &VfxMedium::airless(1.315)).expect("vac");
        assert!(earth.emitters.iter().any(|e| e.name == "smoke_wisp"));
        assert!(!vac.emitters.iter().any(|e| e.name == "smoke_wisp"));
        let flare = |d: &VfxSystemDef| {
            d.emitters
                .iter()
                .find(|e| e.name == "flare")
                .map(|e| e.init.size.max)
                .unwrap_or_default()
        };
        assert!(flare(&vac) < flare(&earth));
    }

    #[test]
    fn metal_hit_leaves_a_scorch_and_a_cooling_molten_glow() {
        let decals = hit_decals(HitSurface::Metal, Calibre::Medium, Vec3::ZERO, Vec3::X, 1);
        assert_eq!(decals.len(), 2);
        let (glow, surface) = decals[1];
        assert!(surface.channels & CHANNEL_EMISSIVE != 0);
        assert!(surface.cooling_s > 0.0);
        assert!(glow.lifetime.is_some_and(|l| l < 5.0));
        // Cools: dimmer each step, and every stop stays in the fire family (never pink).
        let mut last = f32::MAX;
        for i in 0..12 {
            let age = i as f32 * 0.15;
            let e = crate::decal::cooled_emissive(surface.emissive, surface.cooling_s, age);
            let sum = e[0] + e[1] + e[2];
            assert!(sum < last, "cools at {age}");
            last = sum;
            // Orange while it is bright; the dim tail is the dull red of cooling steel.
            if e[0] >= 0.8 {
                assert!(in_fire_family(e), "{age}: {e:?}");
            }
            assert!(e[1] <= e[0] && e[2] <= e[1], "{age}: {e:?}");
        }
        // Decal follows the normal and scales with calibre.
        let heavy = hit_decals(HitSurface::Metal, Calibre::Heavy, Vec3::ZERO, Vec3::X, 1);
        assert!(heavy[0].0.half_size.x > decals[0].0.half_size.x);
        assert!(((decals[0].0.rotation * Vec3::Z) - Vec3::X).length() < 1e-4);
        assert!(hit_decals(HitSurface::Glass, Calibre::Medium, Vec3::ZERO, Vec3::X, 1).is_empty());
        assert_eq!(
            hit_decals(HitSurface::Concrete, Calibre::Light, Vec3::ZERO, Vec3::Y, 1).len(),
            1
        );
    }

    #[test]
    fn hit_spawns_the_medium_effect_scaled_decals_on_the_target_and_cracks_glass() {
        use regia_render::glass::GlassDamage;
        let mut world = World::new();
        let mut scene = VfxScene::new(crate::scene::VfxLibrary::new());
        scene.set_medium(VfxMedium::airless(1.315));
        world.insert_resource(scene);
        world.insert_resource(DecalPool::new(64));
        let hull = world
            .spawn(regia_scene::GlobalTransform(
                regia_core::math::Affine3A::from_translation(Vec3::new(10.0, 0.0, 0.0)),
            ))
            .id();
        let out = apply_weapon_hit(
            &mut world,
            &WeaponHit {
                position: Vec3::new(11.0, 1.0, 0.0),
                normal: Vec3::X,
                surface: HitSurface::Metal,
                calibre: Calibre::Heavy,
                target: Some(hull),
                seed: Some(3),
            },
        );
        assert_eq!(out.effect.as_deref(), Some("hit_metal@airless"));
        assert_eq!(out.decals, 2);
        let scene = world.resource::<VfxScene>();
        let instance = scene.instances().next().expect("instance");
        assert!((instance.scale() - Calibre::Heavy.scale()).abs() < 1e-6);
        let pool = world.resource::<DecalPool>();
        let anchor = pool.anchors()[0].expect("anchored");
        assert_eq!(anchor.entity, hull);
        assert!((anchor.local_position - Vec3::new(1.0, 1.0, 0.0)).length() < 1e-4);
        // The decal rides the hull when it moves.
        world.entity_mut(hull).insert(regia_scene::GlobalTransform(
            regia_core::math::Affine3A::from_translation(Vec3::new(20.0, 0.0, 0.0)),
        ));
        let model = Mat4::from(world.get::<regia_scene::GlobalTransform>(hull).expect("t").0);
        let mut pool = world.resource_mut::<DecalPool>();
        pool.follow_anchors(|_| Some(model));
        assert!((pool.decals()[0].position - Vec3::new(21.0, 1.0, 0.0)).length() < 1e-4);
        pool.follow_anchors(|_| None);
        assert!(pool.is_empty(), "decals on a despawned hull retire");
        // Glass: the impact lands in the canopy's own frame with the calibre's energy.
        let canopy = world
            .spawn((
                GlassDamage::new("laminate"),
                regia_scene::GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::new(
                    0.0, 2.0, 0.0,
                ))),
            ))
            .id();
        let out = apply_weapon_hit(
            &mut world,
            &WeaponHit {
                target: Some(canopy),
                ..WeaponHit::by_weapon("rifle", Vec3::new(0.5, 2.5, 0.0), Vec3::Z, HitSurface::Glass)
            },
        );
        assert_eq!(out.glass_impact, Some(0));
        assert_eq!(out.decals, 0);
        let damage = world.get::<GlassDamage>(canopy).expect("damage");
        assert!((damage.impacts[0].point - Vec3::new(0.5, 0.5, 0.0)).length() < 1e-4);
        assert!((damage.impacts[0].energy - Calibre::Light.glass_energy()).abs() < 1e-6);
    }

    #[test]
    fn daylight_muzzle_and_tracer_draw_their_colour_over_the_scene() {
        let muzzle = crate::showcase::muzzle_flash();
        let tongues = muzzle
            .emitters
            .iter()
            .find(|e| e.name == "tongues")
            .expect("tongues");
        assert_eq!(
            tongues.render.blend,
            BlendMode::Alpha,
            "drawn over the ice, not added"
        );
        let core = muzzle.emitters.iter().find(|e| e.name == "core").expect("core");
        assert_eq!(core.render.blend, BlendMode::Alpha);
        let c = crate::format::parse_color(&core.init.color[0].color).expect("colour");
        assert!(in_fire_family([
            c[0] * core.render.emissive,
            c[1] * core.render.emissive,
            c[2] * core.render.emissive
        ]));
        let flare = muzzle.emitters.iter().find(|e| e.name == "flare").expect("flare");
        assert!(flare.init.size.max <= 1.0, "a small hot core, not a white disc");
        assert!(
            flare.render.emissive < core.render.emissive * 1.2,
            "the star must not bleach the core"
        );
        let tracer = crate::showcase::tracer();
        assert_eq!(tracer.emitters[0].render.blend, BlendMode::Alpha);
        for def in [tracer, tracer_heavy()] {
            for e in &def.emitters {
                let c = crate::format::parse_color(&e.init.color[0].color).expect("colour");
                let shown = [
                    c[0] * e.render.emissive,
                    c[1] * e.render.emissive,
                    c[2] * e.render.emissive,
                ];
                assert!(in_fire_family(shown), "{}: {shown:?}", def.name);
                // Saturated: blue well under green, green well under red.
                assert!(c[2] < 0.15 * c[0] && c[1] < 0.7 * c[0], "{}: {c:?}", def.name);
            }
        }
    }

    #[test]
    fn commands_queue_hits() {
        let mut world = World::new();
        world.insert_resource(VfxScene::new(crate::scene::VfxLibrary::new()));
        world.insert_resource(DecalPool::new(8));
        world.commands().queue(WeaponHit::by_weapon(
            "chaincannon",
            Vec3::ZERO,
            Vec3::Y,
            HitSurface::Concrete,
        ));
        world.flush();
        assert_eq!(world.resource::<VfxScene>().live_instances(), 1);
        assert_eq!(world.resource::<DecalPool>().len(), 1);
    }
}
