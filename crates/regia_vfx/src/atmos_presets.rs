// SPDX-License-Identifier: MPL-2.0
//! Atmosphere-aware variants of the showcase explosion and smoke ([`crate::medium`]).
//!
//! The Earth variants are exactly [`crate::showcase::explosion`] and
//! [`crate::showcase::smoke`]. The others are built from them, so the flash, the fireball sheet
//! and the lighting stay one look:
//!
//! - **Airless**: a flash, then the explosive's own detonation products as a short fireball
//!   (expanding freely, gone in 0.24–0.34 s, no buoyancy), then ice and regolith ejecta on
//!   ballistic arcs under the level's gravity with no drag (clumps, fine grains, shards), a
//!   thin low dust skirt, and nothing that billows or rises. Ejecta that land rest on the
//!   ground (depth collision, slide) and fade out where they fell. No heat haze (no air to
//!   refract). The smoke effect becomes a thin ballistic vent fountain that falls back.
//! - **Mars**: the fireball dies fast (little oxygen), a cloud of butterscotch dust billows, lingers
//!   for several seconds, rises only weakly and drifts with the wind, and the soot is thin and
//!   disperses fast. Debris falls at 3.71 m/s² with little drag.
//!
//! Wind drift: drag pulls a particle toward the air's velocity, so a wind `w` with drag `k` is
//! an extra acceleration `k·w` (terminal velocity `w`); it is folded into the emitter's
//! gravity term.

use crate::format::{
    BlendMode, ColorKey, DepthCollisionDef, EmitterDef, FloatKey, InitModule, RangeDef, RenderMode,
    RenderModule, ShapeDef, SpawnMode, SpawnModule, SpriteId, StretchDef, UpdateModule, VelocityDef,
    VelocityMode, VfxSystemDef,
};
use crate::medium::{VfxAtmosphere, VfxMedium};

pub(crate) fn r(min: f32, max: f32) -> RangeDef {
    RangeDef { min, max }
}

pub(crate) fn keys(points: &[(f32, f32)]) -> Vec<FloatKey> {
    points.iter().map(|&(t, v)| FloatKey { t, v }).collect()
}

pub(crate) fn ramp(points: &[(f32, &str)]) -> Vec<ColorKey> {
    points
        .iter()
        .map(|&(t, c)| ColorKey {
            t,
            color: c.to_string(),
        })
        .collect()
}

/// Landed ejecta rest on what they hit (projected onto the depth surface, velocity damped).
pub(crate) fn settle(offset: f32) -> Option<DepthCollisionDef> {
    Some(DepthCollisionDef { kill: false, offset })
}

fn take(def: &mut VfxSystemDef, name: &str) -> Option<EmitterDef> {
    let i = def.emitters.iter().position(|e| e.name == name)?;
    Some(def.emitters.remove(i))
}

/// Gravity term with wind drift folded in (`g + k·w`, see the module docs).
pub(crate) fn drift(medium: &VfxMedium, lift: f32, drag: f32) -> (f32, f32, f32) {
    (
        medium.wind.x * drag,
        lift + medium.wind.y * drag,
        medium.wind.z * drag,
    )
}

/// The showcase explosion for `medium`, named for it (`explosion`, `explosion@mars`,
/// `explosion@airless`). Earth is [`crate::showcase::explosion`] unchanged.
#[must_use]
pub fn explosion_in(medium: &VfxMedium) -> VfxSystemDef {
    let mut def = crate::showcase::explosion();
    match medium.atmosphere {
        VfxAtmosphere::Earth => def,
        VfxAtmosphere::Airless => {
            airless_explosion(&mut def, medium);
            def.name = medium.variant_name("explosion").unwrap_or(def.name);
            def
        }
        VfxAtmosphere::Mars => {
            mars_explosion(&mut def, medium);
            def.name = medium.variant_name("explosion").unwrap_or(def.name);
            def
        }
    }
}

/// The showcase smoke for `medium`, named for it. Earth is [`crate::showcase::smoke`]
/// unchanged.
#[must_use]
pub fn smoke_in(medium: &VfxMedium) -> VfxSystemDef {
    let mut def = crate::showcase::smoke();
    match medium.atmosphere {
        VfxAtmosphere::Earth => def,
        VfxAtmosphere::Airless => {
            airless_vent(&mut def, medium);
            def.name = medium.variant_name("smoke").unwrap_or(def.name);
            def
        }
        VfxAtmosphere::Mars => {
            mars_smoke(&mut def, medium);
            def.name = medium.variant_name("smoke").unwrap_or(def.name);
            def
        }
    }
}

/// The built-in variants for `medium` (empty on Earth): what the plugin registers for every
/// base effect the library has. The explosion and smoke, the hover washes
/// ([`crate::hover`]), the thruster plumes ([`crate::plume`]) and the hits and muzzles ([`crate::combat`]; tracers have no variant).
#[must_use]
pub fn variants_in(medium: &VfxMedium) -> Vec<VfxSystemDef> {
    if medium.atmosphere == VfxAtmosphere::Earth {
        return Vec::new();
    }
    let mut out = vec![explosion_in(medium), smoke_in(medium)];
    out.extend(
        crate::hover::HoverSurface::ALL
            .into_iter()
            .map(|surface| crate::hover::hover_wash_in(surface, medium)),
    );
    out.extend(
        crate::plume::PlumeKind::ALL
            .into_iter()
            .map(|kind| crate::plume::plume_in(kind, medium)),
    );
    out.extend(
        crate::combat::COMBAT_EFFECTS
            .into_iter()
            .filter_map(|base| crate::combat::combat_effect_in(base, medium))
            .filter(|def| def.name.contains('@')),
    );
    out
}

/// The built-in effect `base` in `medium` (the base def on Earth, else the named variant):
/// `explosion`, `smoke`, `hover_wash_<surface>`, and the combat effects. `None` for a name
/// that is not built in.
#[must_use]
pub fn builtin_in(base: &str, medium: &VfxMedium) -> Option<VfxSystemDef> {
    match base {
        "explosion" => Some(explosion_in(medium)),
        "smoke" => Some(smoke_in(medium)),
        _ => crate::hover::HoverSurface::ALL
            .into_iter()
            .find(|s| s.effect() == Some(base))
            .map(|surface| crate::hover::hover_wash_in(surface, medium))
            .or_else(|| {
                crate::plume::PlumeKind::ALL
                    .into_iter()
                    .find(|k| k.effect() == base)
                    .map(|kind| crate::plume::plume_in(kind, medium))
            })
            .or_else(|| crate::combat::combat_effect_in(base, medium)),
    }
}

/// Makes sure `scene` can play built-in `base` in its medium: adds the base def (Earth) and the
/// medium's variant when the library lacks them. A game's own def of either name is kept.
/// `false` when `base` is neither loaded nor built in.
pub fn ensure_builtin(scene: &mut crate::scene::VfxScene, base: &str) -> bool {
    let medium = scene.medium();
    if scene.library().index(base).is_none() {
        let Some(def) = builtin_in(base, &VfxMedium::earth()) else {
            return false;
        };
        if let Err(err) = scene.library_mut().add(def) {
            tracing::warn!(target: "regia_vfx", "built-in effect {base} rejected: {err}");
            return false;
        }
    }
    if let Some(variant) = medium.variant_name(base)
        && scene.library().index(&variant).is_none()
        && let Some(def) = builtin_in(base, &medium)
        && def.name == variant
        && let Err(err) = scene.library_mut().add(def)
    {
        tracing::warn!(target: "regia_vfx", "built-in variant {variant} rejected: {err}");
    }
    true
}

fn airless_explosion(def: &mut VfxSystemDef, medium: &VfxMedium) {
    let g = medium.gravity_vec();
    // No air: nothing rises out of the fire, nothing refracts.
    take(def, "smoke_puff");
    take(def, "heat_haze");
    for e in &mut def.emitters {
        match e.name.as_str() {
            "fireball" => {
                // The explosive's own gases: no oxygen to keep burning, so the body expands
                // freely, cools and is gone within a few tenths of a second.
                e.spawn.lifetime = r(0.24, 0.34);
                e.init.velocity.mode = VelocityMode::Radial { speed: r(1.5, 3.0) };
                e.init.size = r(2.0, 2.6);
                e.update.gravity = (0.0, 0.0, 0.0);
                e.update.drag = 0.0;
                e.update.curl = None;
                e.update.size_over_life = keys(&[(0.0, 0.6), (0.3, 1.25), (1.0, 1.9)]);
                e.update.alpha_over_life = keys(&[(0.0, 1.0), (0.4, 0.85), (0.75, 0.35), (1.0, 0.0)]);
                e.update.color_over_life = ramp(&[(0.0, "#8e8e8e"), (0.5, "#6a6a6a"), (1.0, "#2a2a2a")]);
            }
            "embers" => {
                // Hot fragments on ballistic arcs: small, so they radiate their heat away (nothing
                // feeds them) within a few tenths of a second.
                e.spawn.mode = SpawnMode::Burst(16);
                e.spawn.lifetime = r(0.2, 0.45);
                e.render.stretch.velocity_scale = 0.05;
                e.update.gravity = g;
                e.update.drag = 0.0;
                e.update.kill_below = None;
                e.update.depth_collision = Some(DepthCollisionDef {
                    kill: true,
                    offset: 0.1,
                });
            }
            "debris" => {
                e.spawn.mode = SpawnMode::Burst(18);
                e.spawn.lifetime = r(8.0, 16.0);
                e.init.velocity.mode = VelocityMode::Directional {
                    dir: (0.0, 1.0, 0.0),
                    spread: 65.0,
                    speed: r(3.0, 11.0),
                };
                e.init.color = ramp(&[(0.0, "#8f8a83")]);
                e.update.gravity = g;
                e.update.drag = 0.0;
                e.update.kill_below = None;
                e.update.depth_collision = settle(0.1);
                e.update.alpha_over_life = keys(&[(0.0, 1.0), (0.85, 1.0), (1.0, 0.0)]);
                e.render.lit = true;
            }
            _ => {}
        }
    }
    // Dust clumps: each stands for a small cloud of grains with a spread of velocities, so in
    // vacuum it grows linearly with time and thins as its area grows (opacity ∝ 1/size²). Many
    // small streaked clumps make the ejecta curtain: a cone that expands and goes transparent on
    // its ballistic arcs instead of billowing.
    def.emitters.push(ejecta(
        "ejecta",
        medium,
        SpawnMode::Burst(650),
        50.0,
        r(1.0, 7.0),
        r(0.1, 0.22),
        "#b9b2a6",
        Some(0.05),
        r(6.0, 12.0),
        keys(&[(0.0, 1.0), (1.0, 4.0)]),
        keys(&[
            (0.0, 0.0),
            (0.01, 0.6),
            (0.1, 0.45),
            (0.3, 0.25),
            (0.6, 0.1),
            (1.0, 0.0),
        ]),
    ));
    // Single grains and pebbles that stay visible along their arcs, streaked by their motion.
    def.emitters.push(ejecta(
        "ejecta_fine",
        medium,
        SpawnMode::Burst(1400),
        42.0,
        r(1.5, 10.0),
        r(0.025, 0.06),
        "#cfc9bf",
        Some(0.04),
        r(6.0, 14.0),
        keys(&[(0.0, 1.0), (1.0, 1.0)]),
        keys(&[(0.0, 0.0), (0.02, 0.85), (0.85, 0.7), (1.0, 0.0)]),
    ));
    // The heavy, slow, low-angle part lands within a few metres of the crater and stays: the
    // deposit, darker than the surface ice it lands on (excavated regolith), is the ground
    // stain for the effect's life; a host decal keeps it longer.
    def.emitters.push(ejecta(
        "deposit",
        medium,
        SpawnMode::Burst(500),
        75.0,
        r(0.8, 3.0),
        r(0.2, 0.45),
        "#7a7268",
        None,
        r(16.0, 24.0),
        keys(&[(0.0, 1.0), (1.0, 1.0)]),
        keys(&[(0.0, 0.0), (0.02, 0.85), (0.8, 0.75), (1.0, 0.0)]),
    ));
    // A thin, low skirt of dust thrown out along the ground: low angles, slow, settles fast.
    def.emitters.push(EmitterDef {
        name: "skirt".into(),
        delay: 0.03,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Burst(90),
            lifetime: r(1.2, 2.6),
            count_scale: 1.0,
        },
        init: InitModule {
            shape: ShapeDef::Sphere {
                radius: r(0.2, 0.8),
                surface: false,
            },
            velocity: VelocityDef {
                mode: VelocityMode::Directional {
                    dir: (0.0, 1.0, 0.0),
                    spread: 82.0,
                    speed: r(1.5, 4.5),
                },
                inherit: 0.0,
            },
            size: r(0.5, 1.1),
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(0.0, 0.0),
            color: ramp(&[(0.0, "#b5aea2")]),
        },
        update: UpdateModule {
            gravity: g,
            drag: 0.0,
            size_over_life: keys(&[(0.0, 0.7), (1.0, 1.4)]),
            alpha_over_life: keys(&[(0.0, 0.0), (0.05, 0.5), (0.6, 0.3), (1.0, 0.0)]),
            depth_collision: settle(0.25),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Billboard,
            sprite: SpriteId::Puff,
            blend: BlendMode::Alpha,
            softness: 0.6,
            lit: true,
            self_shadow: 0.3,
            ..Default::default()
        },
    });
}

/// Ballistic ejecta: launched in a cone around up (streaked by `stretch` × speed when set), gravity only (no drag, no lift, no curl:
/// dust in vacuum never billows), rests where it lands and fades out at the end of its life.
#[allow(clippy::too_many_arguments)]
fn ejecta(
    name: &str,
    medium: &VfxMedium,
    mode: SpawnMode,
    spread: f32,
    speed: RangeDef,
    size: RangeDef,
    color: &str,
    stretch: Option<f32>,
    lifetime: RangeDef,
    size_over_life: Vec<FloatKey>,
    alpha_over_life: Vec<FloatKey>,
) -> EmitterDef {
    EmitterDef {
        name: name.into(),
        delay: 0.02,
        duration: None,
        spawn: SpawnModule {
            mode,
            lifetime,
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
                    spread,
                    speed,
                },
                inherit: 0.0,
            },
            size,
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(0.0, 0.0),
            color: ramp(&[(0.0, color)]),
        },
        update: UpdateModule {
            gravity: medium.gravity_vec(),
            drag: 0.0,
            size_over_life,
            alpha_over_life,
            depth_collision: settle(0.15),
            ..Default::default()
        },
        render: RenderModule {
            // Streaked along the velocity when `stretch` is set (moving grains), else round.
            mode: if stretch.is_some() {
                RenderMode::Stretched
            } else {
                RenderMode::Billboard
            },
            stretch: StretchDef {
                velocity_scale: stretch.unwrap_or(0.0),
                min_length: size.min,
            },
            sprite: SpriteId::SoftDisc,
            blend: BlendMode::Alpha,
            softness: 0.2,
            lit: true,
            self_shadow: 0.4,
            screen_floor_px: 1.5,
            ..Default::default()
        },
    }
}

fn mars_explosion(def: &mut VfxSystemDef, medium: &VfxMedium) {
    let g = medium.gravity_vec();
    // Thin air: no visible heat haze.
    take(def, "heat_haze");
    for e in &mut def.emitters {
        match e.name.as_str() {
            "fireball" => {
                // Little oxygen: the fire dies fast and barely rises.
                e.spawn.lifetime = r(0.45, 0.6);
                e.update.gravity = (0.0, 0.5, 0.0);
                e.update.drag = 0.6;
                e.update.curl = Some((0.4, 0.35, 0.3));
            }
            "smoke_puff" => {
                // Thin soot that disperses fast and drifts.
                e.delay = 0.3;
                e.spawn.mode = SpawnMode::Burst(24);
                e.spawn.lifetime = r(1.4, 2.2);
                e.init.color = ramp(&[(0.0, "#5b544d"), (1.0, "#7a7268")]);
                e.update.drag = 0.5;
                e.update.gravity = drift(medium, 0.35, 0.5);
                e.update.size_over_life = keys(&[(0.0, 0.7), (1.0, 3.0)]);
                e.update.alpha_over_life =
                    keys(&[(0.0, 0.0), (0.12, 0.0), (0.3, 0.35), (0.6, 0.2), (1.0, 0.0)]);
            }
            "embers" | "debris" => {
                e.update.gravity = g;
                e.update.drag = 0.05;
            }
            _ => {}
        }
    }
    // The blast raises butterscotch dust that billows, lingers and drifts with the wind.
    def.emitters.push(EmitterDef {
        name: "dust_billow".into(),
        delay: 0.02,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Burst(110),
            lifetime: r(10.0, 16.0),
            count_scale: 1.0,
        },
        init: InitModule {
            shape: ShapeDef::Sphere {
                radius: r(0.2, 1.0),
                surface: false,
            },
            velocity: VelocityDef {
                mode: VelocityMode::Directional {
                    dir: (0.0, 1.0, 0.0),
                    spread: 70.0,
                    speed: r(1.5, 5.0),
                },
                inherit: 0.0,
            },
            size: r(0.8, 1.6),
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(-0.3, 0.3),
            // Butterscotch dust (Mars' bright dust albedo is about 0.25, reddish), paling as
            // it thins.
            color: ramp(&[(0.0, "#94785f"), (1.0, "#ad9883")]),
        },
        update: UpdateModule {
            // Weak lift (thin, cold CO₂), drag toward the wind.
            gravity: drift(medium, 0.12, 0.8),
            drag: 0.8,
            curl: Some((0.8, 0.3, 0.2)),
            size_over_life: keys(&[(0.0, 0.6), (0.2, 1.5), (1.0, 4.0)]),
            alpha_over_life: keys(&[(0.0, 0.0), (0.03, 0.55), (0.4, 0.35), (0.75, 0.18), (1.0, 0.0)]),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Billboard,
            sprite: SpriteId::PuffAnim,
            flipbook: Some(crate::showcase::puff_flipbook()),
            blend: BlendMode::Alpha,
            softness: 1.2,
            lit: true,
            self_shadow: 0.6,
            screen_floor_px: 2.0,
            ..Default::default()
        },
    });
}

fn airless_vent(def: &mut VfxSystemDef, medium: &VfxMedium) {
    // No air: a burning wreck cannot keep a column. What leaves it is vented gas freezing into
    // grains, thrown up in a narrow fountain that falls back under gravity.
    take(def, "base");
    for e in &mut def.emitters {
        if e.name == "column" {
            e.name = "vent".into();
            e.spawn.mode = SpawnMode::Rate(30.0);
            e.spawn.lifetime = r(4.0, 8.0);
            e.init.velocity.mode = VelocityMode::Directional {
                dir: (0.0, 1.0, 0.0),
                spread: 12.0,
                speed: r(2.5, 5.0),
            };
            e.init.size = r(0.12, 0.3);
            e.init.color = ramp(&[(0.0, "#c9c4bb")]);
            e.update = UpdateModule {
                gravity: medium.gravity_vec(),
                drag: 0.0,
                size_over_life: keys(&[(0.0, 0.8), (0.05, 1.0), (1.0, 1.0)]),
                alpha_over_life: keys(&[(0.0, 0.0), (0.05, 0.7), (0.8, 0.6), (1.0, 0.0)]),
                depth_collision: settle(0.15),
                ..Default::default()
            };
            e.render.sprite = SpriteId::Puff;
            e.render.flipbook = None;
            e.render.softness = 0.2;
            e.render.self_shadow = 0.4;
        }
    }
}

fn mars_smoke(def: &mut VfxSystemDef, medium: &VfxMedium) {
    for e in &mut def.emitters {
        match e.name.as_str() {
            "column" => {
                // Thin smoke in thin air: weak lift, fast spread, low density, drifting.
                e.spawn.lifetime = r(2.8, 4.2);
                e.init.color = ramp(&[(0.0, "#5e5750"), (1.0, "#9a8f84")]);
                e.update.gravity = drift(medium, 0.3, 0.45);
                e.update.size_over_life = keys(&[(0.0, 0.6), (1.0, 3.4)]);
                e.update.alpha_over_life = keys(&[(0.0, 0.05), (0.1, 0.42), (0.6, 0.25), (1.0, 0.0)]);
            }
            "base" => {
                e.init.color = ramp(&[(0.0, "#94785f")]);
                e.update.gravity = drift(medium, 0.0, 0.9);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{VfxLibrary, VfxScene};
    use crate::sim::LodInput;
    use regia_core::math::Vec3;

    #[test]
    fn earth_variants_are_the_showcase_effects_unchanged() {
        let earth = VfxMedium::earth();
        assert_eq!(explosion_in(&earth), crate::showcase::explosion());
        assert_eq!(smoke_in(&earth), crate::showcase::smoke());
        assert!(variants_in(&earth).is_empty());
    }

    #[test]
    fn variants_are_valid_named_and_round_trip() {
        for medium in [VfxMedium::airless(1.315), VfxMedium::mars()] {
            for def in variants_in(&medium) {
                def.validate().expect("valid variant");
                let suffix = medium.variant_suffix().expect("suffix");
                assert!(def.name.ends_with(suffix), "{}", def.name);
                let text = def.to_ron().expect("ron");
                let back = VfxSystemDef::from_ron(&text).expect("round trip");
                assert_eq!(back, def);
            }
        }
    }

    #[test]
    fn airless_has_no_buoyant_smoke_no_drag_and_gravity_of_the_level() {
        let medium = VfxMedium::airless(1.315);
        for def in variants_in(&medium) {
            for e in &def.emitters {
                assert!(e.update.gravity.1 <= 0.0, "{}/{} rises", def.name, e.name);
                assert_eq!(e.update.drag, 0.0, "{}/{} has drag in vacuum", def.name, e.name);
                assert!(e.update.curl.is_none(), "{}/{} billows", def.name, e.name);
                assert_eq!(e.render.heat_haze, 0.0, "{}/{} refracts", def.name, e.name);
                if e.update.gravity.1 < 0.0 {
                    assert!(
                        (e.update.gravity.1 + 1.315).abs() < 1e-6,
                        "{}/{}",
                        def.name,
                        e.name
                    );
                }
            }
        }
    }

    #[test]
    fn airless_fire_is_out_by_half_a_second() {
        let medium = VfxMedium::airless(1.315);
        let def = explosion_in(&medium);
        for e in def.emitters.iter().filter(|e| e.render.emissive > 1.0) {
            let end = e.delay + e.spawn.lifetime.max + e.duration.unwrap_or(0.0).min(e.spawn.lifetime.max);
            assert!(end <= 0.5, "{} still glows at {end} s", e.name);
        }
        // The lights the scene gathers go out too.
        let mut lib = VfxLibrary::new();
        lib.add(def.clone()).expect("add");
        let mut scene = VfxScene::new(lib);
        scene.spawn(&def.name, Vec3::ZERO, Vec3::Z, 1).expect("spawn");
        let lod = LodInput {
            camera_pos: Vec3::new(0.0, 2.0, 10.0),
        };
        let mut t = 0.0;
        while t < 0.5 {
            scene.update(1.0 / 60.0, &lod, 100_000);
            t += 1.0 / 60.0;
        }
        let mut lights = crate::lights::VfxFrameLights::default();
        scene.gather_lights(&mut lights, 8);
        assert!(
            lights.lights.is_empty(),
            "fire light after 0.5 s: {:?}",
            lights.lights
        );
        // On Earth the same explosion still burns at 0.5 s.
        let mut lib = VfxLibrary::new();
        lib.add(crate::showcase::explosion()).expect("add");
        let mut scene = VfxScene::new(lib);
        scene.spawn("explosion", Vec3::ZERO, Vec3::Z, 1).expect("spawn");
        let mut t = 0.0;
        while t < 0.5 {
            scene.update(1.0 / 60.0, &lod, 100_000);
            t += 1.0 / 60.0;
        }
        let mut lights = crate::lights::VfxFrameLights::default();
        scene.gather_lights(&mut lights, 8);
        assert!(!lights.lights.is_empty());
    }

    #[test]
    fn mars_dust_lingers_and_drifts_downwind_and_fire_is_short() {
        let medium = VfxMedium::mars();
        let def = explosion_in(&medium);
        let dust = def
            .emitters
            .iter()
            .find(|e| e.name == "dust_billow")
            .expect("dust");
        assert!(dust.spawn.lifetime.min >= 5.0);
        let (gx, gy, gz) = dust.update.gravity;
        assert!(gy > 0.0 && gy < 0.3, "weak lift {gy}");
        assert!(
            gx * medium.wind.x > 0.0 && gz * medium.wind.z > 0.0,
            "drifts with the wind"
        );
        let fire = def.emitters.iter().find(|e| e.name == "fireball").expect("fire");
        let earth_fire = crate::showcase::explosion()
            .emitters
            .into_iter()
            .find(|e| e.name == "fireball")
            .expect("fire");
        assert!(fire.spawn.lifetime.max < earth_fire.spawn.lifetime.min);
    }
}
