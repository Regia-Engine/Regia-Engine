// SPDX-License-Identifier: MPL-2.0
//! The DO27 reference effect set (spec `26` § 6: "DO27 event set authored as `.vfx.ron`").
//!
//! These are the effects the DO27 event router needs (spec `25` § 4 VFX row): muzzle, tracer,
//! impact, explosion, exhaust, dust, sparks, weather, plus smoke (the "smoke budget"). They are
//! in code so the evidence suite (`samples/vfx_lab --evidence`, the offscreen GPU tests) and
//! games have one canonical, always-available set; DO27's own generators will author `.vfx.ron`
//! files with the same names and serialise through [`VfxSystemDef::to_ron`] (`docs/formats/
//! vfx-ron.md`).
//!
//! Every definition round-trips through the frozen v4 format and respects its system budget
//! (asserted in the sample's tests and the offscreen evidence suite). Fire stays in the
//! orange/yellow/white family in linear light, before tonemap. Smoke is lit puff sheets,
//! with no additive ground disc. The explosion is layered: a flash, a flame flipbook, streaks,
//! then a rising smoke column.

pub use crate::atmos_presets::{explosion_in, smoke_in, variants_in};
use crate::format::{
    BlendMode, ColorKey, EmitterDef, FlipbookDef, FlipbookMode, FloatKey, InitModule, LodDef,
    ParticleLightDef, RangeDef, RenderMode, RenderModule, ShapeDef, SpawnMode, SpawnModule, SpriteId,
    StretchDef, SystemModule, UpdateModule, VFX_FORMAT_VERSION, VelocityDef, VelocityMode, VfxSystemDef,
};

/// `(min, max)` range shorthand.
fn r(min: f32, max: f32) -> RangeDef {
    RangeDef { min, max }
}

/// 16-frame puff sheet. Random start plus frame blend so a burst is not one repeating blob.
fn puff_sheet() -> FlipbookDef {
    FlipbookDef {
        cols: 8,
        rows: 2,
        frames: 16,
        mode: FlipbookMode::Fps(10.0),
        random_start: true,
        blend_frames: true,
    }
}

/// The puff sheet for other modules' presets.
pub(crate) fn puff_flipbook() -> FlipbookDef {
    puff_sheet()
}

/// 8-frame fireball-body sheet on atlas tiles 32..40. Random start plus frame blend.
fn fireball_sheet() -> FlipbookDef {
    FlipbookDef {
        cols: 8,
        rows: 1,
        frames: 8,
        mode: FlipbookMode::Fps(10.0),
        random_start: true,
        blend_frames: true,
    }
}

/// Hot streaks (sparks, embers): pale yellow → yellow-orange → orange in linear light, every
/// stop with `g >= 0.42 r`. A streak cools by fading its alpha, not by going red: deep red added
/// to a blue sky prints pink.
pub(crate) fn hot_streak_ramp() -> Vec<ColorKey> {
    vec![
        ColorKey {
            t: 0.0,
            color: "#ffdca0".into(),
        },
        ColorKey {
            t: 0.3,
            color: "#ffbc61".into(),
        },
        ColorKey {
            t: 1.0,
            color: "#ffad45".into(),
        },
    ]
}

/// The DO27 event set: muzzle, tracer, impact, explosion, exhaust, dust, sparks, weather, smoke.
///
/// Effects play in the instance frame: local +Z is the "fire" direction for directional
/// velocities written as `(0, 0, -1)` (e.g. the muzzle cone) — spawn the instance with
/// `forward = -fire_direction`.
pub fn showcase_effects() -> Vec<VfxSystemDef> {
    vec![
        muzzle_flash(),
        exhaust(),
        dust_trail(),
        tracer(),
        impact(),
        explosion(),
        sparks(),
        weather(),
        smoke(),
    ]
}

/// The muzzle flash (DO27 "muzzle"): a small additive hot core (the star), saturated flame
/// tongues drawn *over* the scene, a bore of stretched sparks and a lit wisp.
///
/// Daylight on bright ice: additive light on a near-white background only clips it to white, so
/// the flash reads by the orange tongues, which replace the background with their own colour
/// (premultiplied alpha with emissive), and the additive core stays small and under the clip.
pub fn muzzle_flash() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "muzzle_flash".into(),
        system: SystemModule {
            bounds: None,
            budget: 256,
            lod: Some(LodDef {
                distances: vec![20.0, 80.0],
                rate_scale: vec![1.0, 0.5, 0.0],
            }),
        },
        emitters: vec![
            EmitterDef {
                name: "flare".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(1),
                    lifetime: r(0.06, 0.10),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Point,
                    velocity: VelocityDef::default(),
                    size: r(0.45, 0.6),
                    rotation: r(0.0, 0.0),
                    spin: r(0.0, 0.0),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#ffdc98".into(),
                    }],
                },
                update: UpdateModule {
                    alpha_over_life: vec![FloatKey { t: 0.0, v: 1.0 }, FloatKey { t: 1.0, v: 0.0 }],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::Flare,
                    blend: BlendMode::Additive,
                    emissive: 1.6,
                    ..Default::default()
                },
            },
            EmitterDef {
                // The incandescent gas ball at the muzzle, orange and drawn over the scene: on
                // white ice it reads by its colour, where an added white star does not.
                name: "core".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(1),
                    lifetime: r(0.05, 0.08),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Point,
                    velocity: VelocityDef::default(),
                    size: r(0.5, 0.65),
                    rotation: r(0.0, std::f32::consts::TAU),
                    spin: r(0.0, 0.0),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#ffa83a".into(),
                    }],
                },
                update: UpdateModule {
                    size_over_life: vec![FloatKey { t: 0.0, v: 0.8 }, FloatKey { t: 1.0, v: 1.3 }],
                    alpha_over_life: vec![FloatKey { t: 0.0, v: 1.0 }, FloatKey { t: 1.0, v: 0.0 }],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::SoftDisc,
                    blend: BlendMode::Alpha,
                    emissive: 1.8,
                    ..Default::default()
                },
            },
            EmitterDef {
                // Flame tongues: the burning propellant gas, saturated orange-yellow, drawn over
                // the background so they keep their colour on white ice.
                name: "tongues".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(6),
                    lifetime: r(0.05, 0.09),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.03),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 0.0, -1.0),
                            spread: 16.0,
                            speed: r(14.0, 28.0),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.14, 0.24),
                    rotation: r(0.0, 0.0),
                    spin: r(0.0, 0.0),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#ffa43a".into(),
                    }],
                },
                update: UpdateModule {
                    drag: 5.0,
                    alpha_over_life: vec![FloatKey { t: 0.0, v: 1.0 }, FloatKey { t: 1.0, v: 0.0 }],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Stretched,
                    sprite: SpriteId::SoftDisc,
                    blend: BlendMode::Alpha,
                    emissive: 1.5,
                    stretch: StretchDef {
                        velocity_scale: 0.03,
                        min_length: 0.3,
                    },
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "flash".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(14),
                    lifetime: r(0.04, 0.09),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Cone {
                        angle: 14.0,
                        length: r(0.05, 0.3),
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 0.0, -1.0),
                            spread: 9.0,
                            speed: r(14.0, 32.0),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.035, 0.07),
                    rotation: r(0.0, 0.0),
                    spin: r(0.0, 0.0),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#fff1c4".into(),
                    }],
                },
                update: UpdateModule {
                    drag: 4.0,
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Stretched,
                    sprite: SpriteId::Spark,
                    blend: BlendMode::Additive,
                    emissive: 3.0,
                    stretch: StretchDef {
                        velocity_scale: 0.045,
                        min_length: 0.18,
                    },
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "smoke_wisp".into(),
                delay: 0.02,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(10),
                    lifetime: r(0.3, 0.7),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.05, 0.2),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 0.0, -1.0),
                            spread: 30.0,
                            speed: r(0.5, 2.0),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.15, 0.35),
                    rotation: r(0.0, 0.0),
                    spin: r(-1.0, 1.0),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#8a847888".into(),
                    }],
                },
                update: UpdateModule {
                    gravity: (0.0, 0.6, 0.0),
                    drag: 2.0,
                    size_over_life: vec![FloatKey { t: 0.0, v: 0.4 }, FloatKey { t: 1.0, v: 1.6 }],
                    alpha_over_life: vec![FloatKey { t: 0.0, v: 0.7 }, FloatKey { t: 1.0, v: 0.0 }],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::PuffAnim,
                    flipbook: Some(puff_sheet()),
                    blend: BlendMode::Alpha,
                    softness: 0.8,
                    lit: true,
                    self_shadow: 0.65,
                    ..Default::default()
                },
            },
        ],
    }
}

/// Engine exhaust: a rate-driven smoke plume with curl turbulence (DO27 "exhaust").
pub fn exhaust() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "exhaust".into(),
        system: SystemModule {
            bounds: None,
            budget: 4096,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "smoke".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Rate(80.0),
                lifetime: r(1.2, 2.5),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Sphere {
                    radius: r(0.0, 0.2),
                    surface: false,
                },
                velocity: VelocityDef {
                    mode: VelocityMode::Directional {
                        dir: (0.0, 0.0, -1.0),
                        spread: 25.0,
                        speed: r(1.0, 6.0),
                    },
                    inherit: 1.0,
                },
                size: r(0.45, 0.9),
                rotation: r(0.0, 0.0),
                spin: r(-0.5, 0.5),
                color: vec![
                    ColorKey {
                        t: 0.0,
                        color: "#9a9288cc".into(),
                    },
                    ColorKey {
                        t: 1.0,
                        color: "#9a928800".into(),
                    },
                ],
            },
            update: UpdateModule {
                gravity: (0.0, 1.0, 0.0),
                drag: 1.5,
                curl: Some((1.5, 0.8, 0.3)),
                size_over_life: vec![FloatKey { t: 0.0, v: 0.35 }, FloatKey { t: 1.0, v: 2.6 }],
                alpha_over_life: vec![
                    FloatKey { t: 0.0, v: 0.85 },
                    FloatKey { t: 0.5, v: 0.55 },
                    FloatKey { t: 1.0, v: 0.0 },
                ],
                ..Default::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::PuffAnim,
                flipbook: Some(puff_sheet()),
                blend: BlendMode::Alpha,
                softness: 0.8,
                emissive: 1.0,
                lit: true,
                self_shadow: 0.65,
                screen_floor_px: 2.0,
                ..Default::default()
            },
        }],
    }
}

/// A vehicle dust trail: per-distance spawns behind a moving instance (DO27 "dust").
pub fn dust_trail() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "dust_trail".into(),
        system: SystemModule {
            bounds: None,
            budget: 2048,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "dust".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::PerDistance { every: 0.3 },
                lifetime: r(0.8, 2.0),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Sphere {
                    radius: r(0.0, 0.3),
                    surface: false,
                },
                velocity: VelocityDef {
                    mode: VelocityMode::Radial { speed: r(0.2, 1.5) },
                    inherit: 0.0,
                },
                size: r(0.2, 0.6),
                rotation: r(0.0, 0.0),
                spin: r(-0.5, 0.5),
                color: vec![ColorKey {
                    t: 0.0,
                    color: "#99886680".into(),
                }],
            },
            update: UpdateModule {
                gravity: (0.0, 0.2, 0.0),
                drag: 0.5,
                curl: Some((0.8, 0.5, 0.2)),
                size_over_life: vec![FloatKey { t: 0.0, v: 1.0 }, FloatKey { t: 1.0, v: 2.5 }],
                alpha_over_life: vec![FloatKey { t: 0.0, v: 0.6 }, FloatKey { t: 1.0, v: 0.0 }],
                kill_below: Some(-0.2),
                ..Default::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::Puff,
                blend: BlendMode::Alpha,
                softness: 0.2,
                ..Default::default()
            },
        }],
    }
}

/// A tracer round: one in-air ribbon of velocity-stretched sparks (DO27 "tracer").
///
/// Drag stays low so the quad is several metres long. A soft disc on that quad is a
/// camera-facing ribbon in the air, not a ground smear. The spark glyph is only a few
/// texels thick, so it is not used here.
///
/// Daylight: the core is drawn over the scene (premultiplied alpha with emissive), not added, so
/// it keeps a saturated orange on Europa's bright ice in full sun instead of clipping to white;
/// on the black sky the same emissive core glows.
pub fn tracer() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "tracer".into(),
        system: SystemModule {
            bounds: None,
            budget: 1024,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "core".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Rate(28.0),
                lifetime: r(0.9, 1.15),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Sphere {
                    radius: r(0.0, 0.02),
                    surface: false,
                },
                velocity: VelocityDef {
                    mode: VelocityMode::Directional {
                        dir: (0.0, 0.02, -1.0),
                        spread: 0.6,
                        speed: r(72.0, 90.0),
                    },
                    inherit: 1.0,
                },
                size: r(0.14, 0.22),
                rotation: r(0.0, 0.0),
                spin: r(0.0, 0.0),
                color: vec![ColorKey {
                    t: 0.0,
                    color: "#ffa232".into(),
                }],
            },
            update: UpdateModule {
                drag: 0.12,
                alpha_over_life: vec![
                    FloatKey { t: 0.0, v: 1.0 },
                    FloatKey { t: 0.75, v: 0.85 },
                    FloatKey { t: 1.0, v: 0.0 },
                ],
                ..Default::default()
            },
            render: RenderModule {
                mode: RenderMode::Stretched,
                // Soft disc, not the spark glyph: the spark's alpha is a few texels thick,
                // so a stretched spark stays a hairline and misses the coverage floor.
                sprite: SpriteId::SoftDisc,
                blend: BlendMode::Alpha,
                emissive: 2.4,
                stretch: StretchDef {
                    velocity_scale: 0.08,
                    min_length: 1.6,
                },
                ..Default::default()
            },
        }],
    }
}

/// A bullet impact: spark burst + flash (DO27 "impact"). Best paired with an impact decal.
pub fn impact() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "impact".into(),
        system: SystemModule {
            bounds: None,
            budget: 512,
            lod: Some(LodDef {
                distances: vec![30.0],
                rate_scale: vec![1.0, 0.3],
            }),
        },
        emitters: vec![
            EmitterDef {
                name: "sparks".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(40),
                    lifetime: r(0.2, 0.6),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.1),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Radial { speed: r(3.0, 15.0) },
                        inherit: 0.0,
                    },
                    size: r(0.02, 0.08),
                    rotation: r(0.0, 0.0),
                    spin: r(0.0, 0.0),
                    // White times `color_over_life` is the ramp (see `hot_streak_ramp`).
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#ffffff".into(),
                    }],
                },
                update: UpdateModule {
                    gravity: (0.0, -3.0, 0.0),
                    drag: 1.0,
                    kill_below: Some(-0.5),
                    color_over_life: hot_streak_ramp(),
                    alpha_over_life: vec![
                        FloatKey { t: 0.0, v: 1.0 },
                        FloatKey { t: 0.7, v: 0.85 },
                        FloatKey { t: 1.0, v: 0.0 },
                    ],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Stretched,
                    sprite: SpriteId::Spark,
                    blend: BlendMode::Additive,
                    emissive: 3.0,
                    stretch: StretchDef {
                        velocity_scale: 0.16,
                        min_length: 0.05,
                    },
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "flash".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(1),
                    lifetime: r(0.05, 0.08),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Point,
                    velocity: VelocityDef::default(),
                    size: r(1.0, 1.5),
                    rotation: r(0.0, 0.0),
                    spin: r(0.0, 0.0),
                    color: vec![
                        ColorKey {
                            t: 0.0,
                            color: "#ffffff".into(),
                        },
                        ColorKey {
                            t: 1.0,
                            color: "#ffffff00".into(),
                        },
                    ],
                },
                update: UpdateModule {
                    alpha_over_life: vec![FloatKey { t: 0.0, v: 0.8 }, FloatKey { t: 1.0, v: 0.0 }],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::Flare,
                    blend: BlendMode::Additive,
                    emissive: 6.0,
                    ..Default::default()
                },
            },
        ],
    }
}

/// An explosion is one fire body with layers around it, not one soft sphere and not a pile
/// of cards (DO27 "explosion").
///
/// 1. A white-yellow flash for the first 80 ms, with a point light of the same window.
/// 2. The fire body: five large `FireballAnim` cards around one centre. Each frame is a whole
///    billowing fireball with a baked temperature ramp, so the union reads as one body with
///    a hot core and a cooler, sooty rim. A neutral grey tint × emissive 6 dims it as it
///    cools and stays under the `Rgba16Float` ceiling at EV 15, so AgX does not swap red and
///    green. Its light follows the body's luminance ([`crate::scene::fire_envelope`]) and
///    lights the ground and the smoke.
/// 3. A heat-haze card over the fire (Desktop High: a scene copy is bound).
/// 4. Streaked embers on the hot-streak ramp (never deep red: pink over a blue sky).
/// 5. A lit smoke column that climbs out of the top of the fire after 0.45 s and fades in
///    above it, lit by the sun and by the fire from below.
/// 6. Mesh debris. The evidence shot places the scorch decal.
///
/// Pairs with a camera-shake event.
pub fn explosion() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "explosion".into(),
        system: SystemModule {
            bounds: None,
            budget: 4096,
            lod: Some(LodDef {
                distances: vec![40.0, 120.0],
                rate_scale: vec![1.0, 0.5, 0.1],
            }),
        },
        emitters: vec![
            EmitterDef {
                name: "flash_core".into(),
                delay: 0.0,
                // The light and the sprite share this window. Dead before the 0.2 s frame.
                duration: Some(0.08),
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(1),
                    // Always alive at 0.05 s, always dead by 0.08 s.
                    lifetime: r(0.062, 0.075),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Point,
                    velocity: VelocityDef::default(),
                    // A small hot core: the flash reads through its light on the ground and
                    // the smoke, not as a big additive disc (that read as a white halo).
                    size: r(1.0, 1.4),
                    rotation: r(-0.35, 0.35),
                    spin: r(0.0, 0.0),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#fff4c8".into(),
                    }],
                },
                update: UpdateModule {
                    size_over_life: vec![FloatKey { t: 0.0, v: 0.7 }, FloatKey { t: 1.0, v: 1.35 }],
                    alpha_over_life: vec![
                        FloatKey { t: 0.0, v: 1.0 },
                        FloatKey { t: 0.55, v: 0.7 },
                        FloatKey { t: 1.0, v: 0.0 },
                    ],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    // Two or three frames of the fireball sheet (70 ms at the sheet's rate):
                    // a flame-shaped core, black-body hot on an HDR frame.
                    sprite: SpriteId::FireballAnim,
                    flipbook: Some(fireball_sheet()),
                    blend: BlendMode::Additive,
                    emissive: 14.0,
                    light: Some(ParticleLightDef {
                        color: "#fff1c4".into(),
                        intensity: 12.0,
                        radius: 9.0,
                    }),
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "smoke_puff".into(),
                // The soot rises out of the top of the cooling fire. It starts after the
                // fire peaks and fades in once it has climbed above it, so it never draws a
                // dark hole in front of the hot core.
                delay: 0.45,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(40),
                    lifetime: r(2.2, 3.4),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.3),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 1.0, 0.0),
                            spread: 16.0,
                            speed: r(1.8, 3.2),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.55, 1.25),
                    rotation: r(0.0, std::f32::consts::TAU),
                    spin: r(-0.5, 0.5),
                    color: vec![
                        ColorKey {
                            t: 0.0,
                            color: "#6b635a".into(),
                        },
                        ColorKey {
                            t: 1.0,
                            color: "#8a8279".into(),
                        },
                    ],
                },
                update: UpdateModule {
                    gravity: (0.0, 0.85, 0.0),
                    drag: 0.55,
                    curl: Some((1.4, 0.4, 0.3)),
                    size_over_life: vec![FloatKey { t: 0.0, v: 0.65 }, FloatKey { t: 1.0, v: 2.1 }],
                    alpha_over_life: vec![
                        // Invisible while it climbs through the fire, then a column above it.
                        FloatKey { t: 0.0, v: 0.0 },
                        FloatKey { t: 0.14, v: 0.0 },
                        FloatKey { t: 0.32, v: 0.62 },
                        FloatKey { t: 0.7, v: 0.38 },
                        FloatKey { t: 1.0, v: 0.0 },
                    ],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::PuffAnim,
                    flipbook: Some(puff_sheet()),
                    blend: BlendMode::Alpha,
                    softness: 1.1,
                    emissive: 1.0,
                    lit: true,
                    self_shadow: 0.75,
                    screen_floor_px: 2.0,
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "fireball".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(5),
                    // Alive through the 0.6 s frame, dead by the 1.5 s column frame.
                    lifetime: r(0.85, 1.05),
                    count_scale: 1.0,
                },
                init: InitModule {
                    // A few big body cards around one centre: each is a whole fireball
                    // (hot core, sooty rim), so their union is one body with a hot middle.
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.12),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Radial { speed: r(0.1, 0.4) },
                        inherit: 0.0,
                    },
                    size: r(2.3, 3.0),
                    // Upright: the sheet's flame tongues lick off the top of each frame.
                    rotation: r(-0.35, 0.35),
                    spin: r(-0.25, 0.25),
                    // White times `color_over_life` is the tint. The sheet carries the hue.
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#ffffff".into(),
                    }],
                },
                update: UpdateModule {
                    gravity: (0.0, 1.4, 0.0),
                    drag: 1.2,
                    curl: Some((0.6, 0.35, 0.3)),
                    // Neutral grey: the body cools by dimming (the sheet's ramp keeps the
                    // hue). Linear 0.27 × emissive 6 stays under the f16 ceiling at EV 15.
                    color_over_life: vec![
                        ColorKey {
                            t: 0.0,
                            color: "#8e8e8e".into(),
                        },
                        ColorKey {
                            t: 0.45,
                            color: "#8a8a8a".into(),
                        },
                        ColorKey {
                            t: 0.75,
                            color: "#6a6a6a".into(),
                        },
                        ColorKey {
                            t: 1.0,
                            color: "#3f3f3f".into(),
                        },
                    ],
                    size_over_life: vec![
                        FloatKey { t: 0.0, v: 0.55 },
                        FloatKey { t: 0.25, v: 1.0 },
                        FloatKey { t: 1.0, v: 1.45 },
                    ],
                    alpha_over_life: vec![
                        FloatKey { t: 0.0, v: 1.0 },
                        FloatKey { t: 0.6, v: 0.9 },
                        FloatKey { t: 0.85, v: 0.5 },
                        FloatKey { t: 1.0, v: 0.0 },
                    ],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::FireballAnim,
                    flipbook: Some(fireball_sheet()),
                    // Premultiplied alpha. Emissive 6 is the HDR core that feeds bloom.
                    blend: BlendMode::Alpha,
                    softness: 0.5,
                    emissive: 6.0,
                    // The fire lights the ground and the smoke; it dims with the body.
                    light: Some(ParticleLightDef {
                        color: "#ffa850".into(),
                        intensity: 14.0,
                        radius: 10.0,
                    }),
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "heat_haze".into(),
                delay: 0.05,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(3),
                    lifetime: r(1.2, 1.6),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.3),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 1.0, 0.0),
                            spread: 10.0,
                            speed: r(0.8, 1.4),
                        },
                        inherit: 0.0,
                    },
                    size: r(3.2, 4.0),
                    rotation: r(0.0, 0.0),
                    spin: r(0.0, 0.0),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#ffffff".into(),
                    }],
                },
                update: UpdateModule {
                    size_over_life: vec![FloatKey { t: 0.0, v: 0.8 }, FloatKey { t: 1.0, v: 1.3 }],
                    alpha_over_life: vec![
                        FloatKey { t: 0.0, v: 0.0 },
                        FloatKey { t: 0.15, v: 1.0 },
                        FloatKey { t: 0.7, v: 0.8 },
                        FloatKey { t: 1.0, v: 0.0 },
                    ],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::SoftDisc,
                    // Refraction card: bends the scene behind the fire (Desktop High).
                    heat_haze: 6.0,
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "embers".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(36),
                    lifetime: r(0.35, 0.85),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.02, 0.15),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 1.0, 0.0),
                            spread: 58.0,
                            speed: r(7.0, 16.0),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.045, 0.09),
                    rotation: r(0.0, 0.0),
                    spin: r(0.0, 0.0),
                    // White times `color_over_life` is the ramp (see `hot_streak_ramp`).
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#ffffff".into(),
                    }],
                },
                update: UpdateModule {
                    gravity: (0.0, -5.5, 0.0),
                    drag: 0.4,
                    kill_below: Some(0.02),
                    color_over_life: hot_streak_ramp(),
                    alpha_over_life: vec![
                        FloatKey { t: 0.0, v: 1.0 },
                        FloatKey { t: 0.7, v: 0.8 },
                        FloatKey { t: 1.0, v: 0.0 },
                    ],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Stretched,
                    // Soft disc on the long quad. The spark glyph is a few texels thick,
                    // so a stretched spark stays a dotted hairline.
                    sprite: SpriteId::SoftDisc,
                    blend: BlendMode::Additive,
                    emissive: 3.5,
                    stretch: StretchDef {
                        velocity_scale: 0.12,
                        min_length: 0.55,
                    },
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "debris".into(),
                delay: 0.02,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Burst(8),
                    lifetime: r(0.7, 1.4),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.05, 0.25),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Radial { speed: r(3.0, 8.0) },
                        inherit: 0.0,
                    },
                    size: r(0.025, 0.06),
                    rotation: r(0.0, std::f32::consts::TAU),
                    spin: r(-8.0, 8.0),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#5a564f".into(),
                    }],
                },
                update: UpdateModule {
                    gravity: (0.0, -9.0, 0.0),
                    drag: 0.35,
                    kill_below: Some(-0.2),
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Mesh,
                    sprite: SpriteId::Shard,
                    blend: BlendMode::Alpha,
                    ..Default::default()
                },
            },
        ],
    }
}

/// Continuous electric/ground sparks (DO27 "scrap sparks", mine arming).
pub fn sparks() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "sparks".into(),
        system: SystemModule {
            bounds: None,
            budget: 2048,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "sparks".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Rate(60.0),
                lifetime: r(0.3, 1.0),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Sphere {
                    radius: r(0.0, 0.1),
                    surface: false,
                },
                velocity: VelocityDef {
                    mode: VelocityMode::Directional {
                        dir: (0.0, 1.0, 0.0),
                        spread: 60.0,
                        speed: r(3.0, 12.0),
                    },
                    inherit: 0.0,
                },
                size: r(0.02, 0.06),
                rotation: r(0.0, 0.0),
                spin: r(0.0, 0.0),
                // White times `color_over_life` is the ramp (see `hot_streak_ramp`).
                color: vec![ColorKey {
                    t: 0.0,
                    color: "#ffffff".into(),
                }],
            },
            update: UpdateModule {
                gravity: (0.0, -2.0, 0.0),
                drag: 0.5,
                kill_below: Some(-0.5),
                color_over_life: hot_streak_ramp(),
                alpha_over_life: vec![
                    FloatKey { t: 0.0, v: 1.0 },
                    FloatKey { t: 0.6, v: 0.8 },
                    FloatKey { t: 1.0, v: 0.0 },
                ],
                ..Default::default()
            },
            render: RenderModule {
                mode: RenderMode::Stretched,
                sprite: SpriteId::Spark,
                blend: BlendMode::Additive,
                emissive: 3.0,
                stretch: StretchDef {
                    velocity_scale: 0.28,
                    min_length: 0.08,
                },
                ..Default::default()
            },
        }],
    }
}

/// Weather: rain streaks falling through a volume above the effect origin, with a soft
/// ground-mist emitter (DO27 "weather"). Spawn the instance high above the area, e.g. at
/// `(0, 8, 0)`, with the default orientation.
pub fn weather() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "weather".into(),
        system: SystemModule {
            bounds: None,
            budget: 8192,
            lod: None,
        },
        emitters: vec![
            EmitterDef {
                name: "rain".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Rate(700.0),
                    lifetime: r(0.9, 1.3),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Box {
                        half_extents: (7.0, 0.6, 7.0),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, -1.0, 0.0),
                            spread: 3.0,
                            speed: r(16.0, 22.0),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.04, 0.10),
                    rotation: r(0.0, 0.0),
                    spin: r(0.0, 0.0),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#9fc0e0b4".into(),
                    }],
                },
                update: UpdateModule {
                    gravity: (0.0, -4.0, 0.0),
                    drag: 0.05,
                    kill_below: Some(0.05),
                    alpha_over_life: vec![
                        FloatKey { t: 0.0, v: 0.8 },
                        FloatKey { t: 0.9, v: 0.8 },
                        FloatKey { t: 1.0, v: 0.0 },
                    ],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Stretched,
                    sprite: SpriteId::Spark,
                    blend: BlendMode::Alpha,
                    stretch: StretchDef {
                        velocity_scale: 0.44,
                        min_length: 0.3,
                    },
                    softness: 0.1,
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "mist".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Rate(30.0),
                    lifetime: r(1.5, 3.0),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Box {
                        half_extents: (6.0, 0.3, 6.0),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 1.0, 0.0),
                            spread: 40.0,
                            speed: r(0.1, 0.4),
                        },
                        inherit: 0.0,
                    },
                    size: r(0.8, 1.8),
                    rotation: r(0.0, 0.0),
                    spin: r(-0.3, 0.3),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#7f94a866".into(),
                    }],
                },
                update: UpdateModule {
                    drag: 1.0,
                    size_over_life: vec![FloatKey { t: 0.0, v: 0.8 }, FloatKey { t: 1.0, v: 1.6 }],
                    alpha_over_life: vec![FloatKey { t: 0.0, v: 0.5 }, FloatKey { t: 1.0, v: 0.0 }],
                    kill_below: Some(-0.2),
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::Puff,
                    blend: BlendMode::Alpha,
                    softness: 0.5,
                    ..Default::default()
                },
            },
        ],
    }
}

/// A persistent smoke column of lit puffs (DO27 "smoke budget"). The base is the same
/// lit sheet, warmer and slower. There is no additive disc on the ground.
pub fn smoke() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "smoke".into(),
        system: SystemModule {
            bounds: None,
            budget: 8192,
            // Fewer, larger puffs with distance (the LOD grows alpha puffs to keep the
            // coverage), so a column on a far ridge still reads; it stops only past 600 m.
            lod: Some(LodDef {
                distances: vec![60.0, 160.0, 600.0],
                rate_scale: vec![1.0, 0.5, 0.25, 0.0],
            }),
        },
        emitters: vec![
            EmitterDef {
                name: "column".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    // Thick, rolling: more and bigger billows that keep their density while
                    // they rise and spread (a thin, fast-fading column read as haze).
                    mode: SpawnMode::Rate(44.0),
                    lifetime: r(5.0, 7.5),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.35),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 1.0, 0.0),
                            spread: 8.0,
                            speed: r(1.1, 2.5),
                        },
                        inherit: 0.0,
                    },
                    size: r(1.6, 3.0),
                    rotation: r(0.0, std::f32::consts::TAU),
                    spin: r(-0.35, 0.35),
                    color: vec![
                        ColorKey {
                            t: 0.0,
                            color: "#625e5a".into(),
                        },
                        ColorKey {
                            t: 1.0,
                            color: "#a09c97".into(),
                        },
                    ],
                },
                update: UpdateModule {
                    // Buoyant: rises at about 2 m/s once the launch speed is spent (lift ÷
                    // drag), so the column stands 12–15 m tall instead of a mound.
                    gravity: (0.0, 0.9, 0.0),
                    drag: 0.45,
                    curl: Some((1.4, 0.3, 0.22)),
                    size_over_life: vec![FloatKey { t: 0.0, v: 0.6 }, FloatKey { t: 1.0, v: 2.4 }],
                    alpha_over_life: vec![
                        FloatKey { t: 0.0, v: 0.05 },
                        FloatKey { t: 0.1, v: 0.95 },
                        FloatKey { t: 0.72, v: 0.75 },
                        FloatKey { t: 1.0, v: 0.0 },
                    ],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::PuffAnim,
                    flipbook: Some(puff_sheet()),
                    blend: BlendMode::Alpha,
                    softness: 2.0,
                    emissive: 1.0,
                    lit: true,
                    self_shadow: 0.88,
                    screen_floor_px: 2.0,
                    ..Default::default()
                },
            },
            EmitterDef {
                name: "base".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Rate(14.0),
                    lifetime: r(2.0, 3.2),
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: r(0.0, 0.35),
                        surface: false,
                    },
                    velocity: VelocityDef {
                        mode: VelocityMode::Directional {
                            dir: (0.0, 1.0, 0.0),
                            spread: 18.0,
                            speed: r(0.15, 0.6),
                        },
                        inherit: 0.0,
                    },
                    size: r(1.6, 2.8),
                    rotation: r(0.0, 0.0),
                    spin: r(-0.2, 0.2),
                    color: vec![ColorKey {
                        t: 0.0,
                        color: "#8d7a68".into(),
                    }],
                },
                update: UpdateModule {
                    drag: 0.9,
                    size_over_life: vec![FloatKey { t: 0.0, v: 0.65 }, FloatKey { t: 1.0, v: 1.25 }],
                    alpha_over_life: vec![
                        FloatKey { t: 0.0, v: 0.1 },
                        FloatKey { t: 0.2, v: 0.85 },
                        FloatKey { t: 1.0, v: 0.0 },
                    ],
                    ..Default::default()
                },
                render: RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: SpriteId::PuffAnim,
                    flipbook: Some(puff_sheet()),
                    blend: BlendMode::Alpha,
                    softness: 1.8,
                    emissive: 1.0,
                    lit: true,
                    self_shadow: 0.72,
                    ..Default::default()
                },
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn showcase_set_is_the_do27_event_set() {
        let effects = showcase_effects();
        let names: Vec<&str> = effects.iter().map(|e| e.name.as_str()).collect();
        for expected in [
            "muzzle_flash",
            "tracer",
            "impact",
            "explosion",
            "exhaust",
            "dust_trail",
            "sparks",
            "weather",
            "smoke",
        ] {
            assert!(names.contains(&expected), "missing DO27 effect {expected}");
        }
        assert_eq!(names.len(), 9, "exactly the DO27 event set");
    }

    #[test]
    fn muzzle_flash_is_a_flare_and_smoke_has_no_additive_disc() {
        let muzzle = muzzle_flash();
        let flare = muzzle
            .emitters
            .iter()
            .find(|e| e.name == "flare")
            .expect("flare emitter");
        assert_eq!(flare.render.sprite, SpriteId::Flare);
        assert_eq!(flare.render.mode, RenderMode::Billboard);
        let bore = muzzle.emitters.iter().find(|e| e.name == "flash").expect("bore");
        assert_eq!(
            bore.render.sprite,
            SpriteId::Spark,
            "the bore is stretched sparks; extra flares fill the star and the plate is a disc"
        );
        assert_eq!(bore.render.mode, RenderMode::Stretched);
        let impact = impact();
        assert!(
            impact
                .emitters
                .iter()
                .any(|e| e.name == "flash" && e.render.sprite == SpriteId::Flare)
        );
        let smoke = smoke();
        assert!(
            smoke.emitters.iter().all(|e| e.render.blend == BlendMode::Alpha),
            "smoke must not add a ground disc"
        );
        assert!(smoke.emitters.iter().all(|e| e.render.lit));
        assert!(smoke.emitters.iter().all(|e| e.render.softness > 0.5));
        assert!(
            smoke
                .emitters
                .iter()
                .all(|e| e.render.sprite == SpriteId::PuffAnim)
        );
    }

    /// Tint (init × colour over life) × every temperature stop of the fireball sheet ×
    /// emissive stays in the fire family in linear light, after the EV 15 exposure
    /// round-trip and AgX, and on the Desktop High display. The hot core is HDR (it feeds
    /// bloom) through the first half of the body's life.
    #[test]
    fn gallery_fire_ramp_stays_in_the_family_in_linear_light() {
        use crate::curves::sample_color;
        use crate::format::parse_color;
        use crate::quality::in_fire_family;

        let boom = explosion();
        let fire = boom
            .emitters
            .iter()
            .find(|e| e.name == "fireball")
            .expect("fireball");
        assert_eq!(fire.render.blend, BlendMode::Alpha);
        assert_eq!(fire.render.sprite, SpriteId::FireballAnim);
        assert!((fire.render.emissive - 6.0).abs() < 1e-4);
        let init = parse_color(&fire.init.color[0].color).expect("init");
        let ev15 = regia_render::exposure_from_ev100(15.0);
        let stops: Vec<[f32; 3]> = crate::atlas::fireball_ramp_stops().collect();
        let core = *stops.last().expect("core stop");
        for step in 0..=8 {
            let t = step as f32 / 8.0;
            let over = sample_color(&fire.update.color_over_life, t);
            let tint = [init[0] * over[0], init[1] * over[1], init[2] * over[2]];
            let tmax = tint[0].max(tint[1]).max(tint[2]);
            let tmin = tint[0].min(tint[1]).min(tint[2]);
            assert!(tmax - tmin < 0.02, "the tint must stay neutral, t={t} {tint:?}");
            if t <= 0.5 {
                assert!(
                    core[0] * tint[0] * fire.render.emissive > 1.0,
                    "the hot core dropped out of HDR at t={t}"
                );
            }
            for stop in &stops {
                let rgb = [stop[0] * tint[0], stop[1] * tint[1], stop[2] * tint[2]];
                assert!(in_fire_family(rgb), "fireball t={t} {rgb:?}");
                let maxc = rgb[0].max(rgb[1]).max(rgb[2]).max(1e-4);
                let minc = rgb[0].min(rgb[1]).min(rgb[2]);
                if (maxc - minc) / maxc >= 0.18 {
                    assert!(
                        rgb[1] >= rgb[0] * 0.42,
                        "fireball t={t} needs green margin, got {rgb:?}"
                    );
                }
                // Same path the GPU test applies: divide, f16 clip, AgX, then the
                // Desktop High mix. A stop above the ceiling on two channels fails here.
                let hdr = [
                    rgb[0] * fire.render.emissive,
                    rgb[1] * fire.render.emissive,
                    rgb[2] * fire.render.emissive,
                ];
                let back = crate::quality::restore_exposed(hdr, ev15);
                assert!(
                    in_fire_family(regia_render::tonemap_agx(back)),
                    "AgX of the fireball at t={t} left the family: {back:?}"
                );
                let shown = crate::quality::desktop_high_display(
                    back,
                    [back[0] * 0.92, back[1] * 0.92, back[2] * 0.92],
                );
                assert!(
                    in_fire_family(shown),
                    "Desktop High fireball at t={t} left the family: {shown:?}"
                );
            }
        }
    }

    #[test]
    fn explosion_is_layered_and_the_tracer_is_an_air_streak() {
        let boom = explosion();
        let names: Vec<&str> = boom.emitters.iter().map(|e| e.name.as_str()).collect();
        assert!(
            !names.contains(&"shockwave"),
            "a camera-facing ring draws the sphere silhouette"
        );
        let flash = boom
            .emitters
            .iter()
            .find(|e| e.name == "flash_core")
            .expect("flash");
        assert!(flash.spawn.lifetime.max <= 0.08, "flash lives past 80 ms");
        assert!(
            flash.spawn.lifetime.min >= 0.05,
            "flash is dead in the 0.05 s frame"
        );
        assert_eq!(flash.duration, Some(0.08));
        assert!(flash.render.emissive >= 10.0);
        assert_eq!(flash.render.blend, BlendMode::Additive);
        // A short flipbook core, not a disc: a big additive disc reads as a white halo.
        assert_eq!(flash.render.sprite, SpriteId::FireballAnim);
        assert!(flash.init.size.max <= 1.6, "flash core is a disc-sized halo");
        let light = flash.render.light.as_ref().expect("flash light");
        assert!(light.radius > 1.0 && light.intensity > 1.0);
        let fire = boom
            .emitters
            .iter()
            .find(|e| e.name == "fireball")
            .expect("fireball");
        // One fire body from the fireball sheet (whole-fireball frames), not flame cards.
        assert_eq!(fire.render.sprite, SpriteId::FireballAnim);
        assert!(
            matches!(fire.spawn.mode, SpawnMode::Burst(n) if n <= 8),
            "a pile of cards"
        );
        assert!(fire.init.size.min >= 2.0, "body cards must overlap into one body");
        assert_eq!(fire.render.blend, BlendMode::Alpha);
        let fire_light = fire.render.light.as_ref().expect("the fire lights the ground");
        assert!(fire_light.radius >= 6.0 && fire_light.intensity > light.intensity);
        let haze = boom
            .emitters
            .iter()
            .find(|e| e.render.heat_haze > 0.0)
            .expect("heat haze over the fire");
        assert!(haze.render.heat_haze >= 3.0);
        let sheet = fire.render.flipbook.expect("fire sheet");
        assert_eq!(sheet.frames, 8);
        assert!(sheet.random_start && sheet.blend_frames);
        assert!(
            fire.spawn.lifetime.max <= 1.05,
            "fire still up in the column frame"
        );
        let smoke = boom
            .emitters
            .iter()
            .find(|e| e.name == "smoke_puff")
            .expect("smoke");
        assert!(
            smoke.delay >= 0.30,
            "smoke starts with the flash and fills the sphere"
        );
        assert!(smoke.render.lit);
        assert_eq!(smoke.render.blend, BlendMode::Alpha);
        assert!(matches!(
            smoke.init.velocity.mode,
            VelocityMode::Directional {
                dir: (0.0, 1.0, 0.0),
                ..
            }
        ));
        let embers = boom.emitters.iter().find(|e| e.name == "embers").expect("embers");
        assert_eq!(embers.render.mode, RenderMode::Stretched);
        assert_eq!(embers.render.sprite, SpriteId::SoftDisc);
        assert!(embers.render.stretch.min_length >= 0.5);
        let shot = tracer();
        let core = &shot.emitters[0];
        assert_eq!(core.render.mode, RenderMode::Stretched);
        assert!(core.update.drag < 1.0, "drag kills the ribbon into a blob");
        assert!(core.spawn.lifetime.min >= 0.8);
        assert!(core.render.stretch.velocity_scale > 0.02);
        assert!(core.render.stretch.min_length >= 1.0);
    }

    #[test]
    fn every_showcase_effect_validates_and_round_trips() {
        for def in showcase_effects() {
            def.validate()
                .unwrap_or_else(|e| panic!("{} validates: {e}", def.name));
            let text = def.to_ron().unwrap_or_else(|e| panic!("{}: {e}", def.name));
            let back =
                VfxSystemDef::from_ron(&text).unwrap_or_else(|e| panic!("{} round-trip: {e}", def.name));
            assert_eq!(&def, &back, "{} round-trips", def.name);
        }
    }
}
