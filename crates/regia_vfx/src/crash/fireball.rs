// SPDX-License-Identifier: MPL-2.0
//! The crash fireball (ADR-1498): a ball as large as its yield says, seen as a volume of many
//! layered cards rather than one flat sprite, that burns orange, is wrapped in soot from about one
//! second on, is black by 3 to 5 s, and rises buoyantly into a soot toroid.
//!
//! Public physics it follows (`docs/provenance.md`):
//!
//! - **Size.** The largest diameter of the luminous ball of a surface burst of `W` kg TNT
//!   equivalent is about `3.5 W^(1/3)` m ([`CrashScale::fireball_radius`]), 220 m at 250 t. That
//!   is the size of what is *seen*: the definition is sized so the projected flame area of a level
//!   camera's view ([`fireball_footprint`]) has that equivalent diameter, not so the cards'
//!   transparent margins reach it (ADR-1414 sized the cards, and the shot-4 frames showed a
//!   ~60–90 m ball).
//! - **Growth and timing.** The detonation products reach most of the ball's size within a few
//!   hundred milliseconds; the afterburning ball then grows slowly to its largest at about
//!   `0.37 τ` (`τ = √(R/g)`, 1.25 s at 250 t) while soot forms in its cooling rim and wraps it
//!   from about `0.22 τ` on. The flame is gone by about `1.2 τ` (4 s at 250 t).
//! - **Buoyant rise.** The hot ball rises at about `0.7 √(gR)` (20–40 m/s at 250 t) and its
//!   outer mass rolls outward into a vortex ring (the toroid that feeds the column's cap): the
//!   soot is laid down on the ball's shell and as a ring that runs outward as it rises, so its
//!   puffs sit on a ring around a thinner axis (Morton, Taylor & Turner 1956; Turner 1957 on
//!   buoyant vortex rings).
//! - **Flash.** A 50 to 200 ms glow of the young ball with the light that reaches the land 2 to
//!   3 km out. A smooth soft disc, not a fireball-sheet card: the sheet's flame tongues on a
//!   flash read as flicks around a flat sprite.
//! - **No glare streak.** The ball's lights sit in the ball ([`FIREBALL_SPAWN_HEIGHT`]) and are
//!   sphere sources of the ball's radius on lit puffs as on the ground ([`fire_light_falloff`]):
//!   the dust skirt under the ball is lit at most as the ground beside the ball, so it no longer
//!   glows as a white horizontal band.

use regia_core::math::Vec3;

use super::{
    CrashMeasure, CrashScale, CrashTier, FLASH_INTENSITY_250T, R_250T, SOOT_MIX_ALBEDO, burst, cooling_tint,
    fireball_effect, fireball_sheet, init, lit_puff, lod, planar, system, upward,
};
use crate::atmos_presets::{drift, keys, r, ramp};
use crate::format::{
    BlendMode, EmitterDef, InitModule, ParticleLightDef, RenderMode, RenderModule, ShapeDef, SpriteId,
    UpdateModule, VfxSystemDef,
};
use crate::medium::VfxMedium;
use crate::probe::{ProbeSettings, probe_effect, probe_particles};
use crate::runtime::{FLASH_MIN_RADIUS_M, FLASH_RADIUS_PER_SOURCE};

/// Height of the fireball's instance (its centre at ignition and the point its lights sit at)
/// above the crash point, in fireball radii ([`CrashScale::fireball_radius`]).
pub const FIREBALL_SPAWN_HEIGHT: f32 = 0.55;

/// Radius of the flame (orange) part of a fireball-sheet card, as a fraction of the card's
/// width: the equivalent radius of the sheet's texels that are both opaque (alpha 0.5 and up)
/// and above the soot threshold of the billboard shader's black-body fire (heat 0.21 and up),
/// averaged over the sheet's frames (a test measures the atlas against it).
pub const SHEET_FLAME_RADIUS: f32 = 0.27;

/// Radius of the opaque body (alpha 0.5 and up) of a puff-sheet card, as a fraction of its
/// width (the soot puffs in [`fireball_footprint`]).
pub const PUFF_BODY_RADIUS: f32 = 0.35;

/// A fireball-sheet card reads as flame while the luma of its tint times its emissive scale is
/// at least this; below it the shader's black-body emission is too dim to read orange.
pub const FLAME_VISIBLE_TINT: f32 = 0.5;

/// Influence radius of the fireball's lights, in fireball radii: [`FLASH_RADIUS_PER_SOURCE`], so
/// the renderer's sphere source of both lights is the ball itself.
const LIGHT_REACH: f32 = FLASH_RADIUS_PER_SOURCE;

/// Linear luma of a colour.
fn luma(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// The fire light's fall-off on a lit puff `distance` metres from a particle light whose influence
/// reaches `radius` metres: the windowed inverse square with the knee of a sphere source, CPU
/// mirror of the lit-puff fire term in `gpu/shaders.rs`. A light reaching
/// [`FLASH_MIN_RADIUS_M`] or further is a flash or a fireball: its source is a sphere of radius
/// `s = radius / FLASH_RADIUS_PER_SOURCE` (the renderer's `particle_light_source_radius`), so the
/// knee is `d² + s²` and a puff under the ball is lit at most `I / s²`. A narrower light keeps
/// the punctual `1 / (1 + d²)` bit for bit.
#[must_use]
pub fn fire_light_falloff(distance: f32, radius: f32) -> f32 {
    let r = radius.max(1.0e-4);
    let d = distance.max(0.0);
    let ratio = d / r;
    let window = (1.0 - ratio * ratio * ratio * ratio).clamp(0.0, 1.0);
    let source = if radius >= FLASH_MIN_RADIUS_M {
        radius / FLASH_RADIUS_PER_SOURCE
    } else {
        0.0
    };
    window * window / (d * d + (source * source).max(1.0))
}

/// The fireball (`crash_fireball@<scale>`), spawned [`FIREBALL_SPAWN_HEIGHT`] radii above the
/// crash point along the normal (as [`super::spawn_crash`] does):
///
/// - `flash`: a smooth glowing disc of the young ball for 50 to 200 ms, with the light that
///   reaches `80 R`.
/// - `fireball`: the body, many overlapping fireball-sheet cards on a shell `0.3–0.65 R` from the
///   centre, so the ball is a volume of layered cards whose silhouette is lumpy and whose flame
///   area has the equivalent diameter `2 R` at its largest (1–1.5 s at 250 t). It rises at about
///   `0.7 √(gR)` and cools from `0.3` of its life on, carrying the ball's light (a sphere source
///   of radius `R`).
/// - `soot`: black soot laid on the ball's shell from `0.22 τ`, which wraps the flame by `τ`.
/// - `soot_ring`: soot thrown outward on a ring above the centre from `0.27 τ`, rolling the
///   rising mass into a toroid. The soot of both stays carbon black through `τ`, then greys as it
///   mixes into the column's condensation (`SOOT_MIX_ALBEDO`, ADR-1472).
/// - `billows` (Full): orange billows rolling out on the ring under the soot.
/// - `heat_haze` (Full): hot air over the ball bends the scene behind it.
#[must_use]
pub fn crash_fireball(scale: CrashScale, tier: CrashTier, medium: &VfxMedium) -> VfxSystemDef {
    let big_r = scale.fireball_radius();
    let tau = scale.time_scale();
    let u = scale.speed_scale();
    let area = (big_r / R_250T) * (big_r / R_250T);
    let flash_life = (0.04 * tau).clamp(0.08, 0.2);
    let body_drag = 0.6 / tau;
    let soot_drag = 0.3 / tau;
    let soot_update = UpdateModule {
        gravity: drift(medium, 0.9 * u * soot_drag, soot_drag),
        drag: soot_drag,
        curl: Some((1.0, 0.05 / big_r.sqrt(), 0.2)),
        // The ramp is the albedo (`SOOT_MIX_ALBEDO`): carbon black at first, then the
        // condensation and dust the rising soot mixes into lift it to a grey 0.3.
        color_over_life: ramp(&SOOT_MIX_ALBEDO),
        // Born large (soot forms through the cooling rim, not from a point), so it reads from
        // about 1.2 s; it grows on as it mixes.
        size_over_life: keys(&[(0.0, 0.8), (1.0 / 7.0, 1.0), (2.0 / 7.0, 1.5), (1.0, 2.4)]),
        // Dense about half a second after forming (0.03 of a 13–20 s life at 250 t), so it bites
        // into the flame from about 1.2 s and wraps it by `τ`.
        alpha_over_life: keys(&[(0.0, 0.0), (0.03, 0.8), (4.0 / 7.0, 0.5), (1.0, 0.0)]),
        ..UpdateModule::default()
    };
    let mut emitters = vec![
        EmitterDef {
            name: "flash".into(),
            delay: 0.0,
            duration: Some(flash_life),
            spawn: burst(1, (0.9 * flash_life, flash_life)),
            init: init(
                ShapeDef::Point,
                upward(0.0, (0.0, 0.0)),
                (1.3 * big_r, 1.5 * big_r),
                "#fff1c4",
            ),
            update: UpdateModule {
                size_over_life: keys(&[(0.0, 0.6), (1.0, 1.0)]),
                alpha_over_life: keys(&[(0.0, 1.0), (0.5, 0.7), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::SoftDisc,
                blend: BlendMode::Additive,
                emissive: 6.0,
                light: Some(ParticleLightDef {
                    color: "#fff1c4".into(),
                    intensity: FLASH_INTENSITY_250T * area,
                    radius: LIGHT_REACH * big_r,
                }),
                ..RenderModule::default()
            },
        },
        EmitterDef {
            // The body: cards on a shell around the centre, each a whole billowing ball whose
            // flame part is `SHEET_FLAME_RADIUS` of its width, so together they fill a ball of
            // radius `R` with depth (the sorted alpha list layers them back to front).
            name: "fireball".into(),
            delay: 0.0,
            duration: None,
            spawn: burst(tier.pick(36, 14), (1.3 * tau, 1.5 * tau)),
            init: InitModule {
                rotation: r(-0.5, 0.5),
                spin: r(-0.15, 0.15),
                ..init(
                    ShapeDef::Sphere {
                        radius: r(0.3 * big_r, 0.65 * big_r),
                        surface: true,
                    },
                    // Up and out: the ball swells and lifts while its base stays near the ground.
                    upward(80.0, (0.3 * u, 0.7 * u)),
                    tier.pick((1.45 * big_r, 1.75 * big_r), (1.55 * big_r, 1.85 * big_r)),
                    "#ffffff",
                )
            },
            update: UpdateModule {
                // Buoyancy relaxes the rise toward `1.6 √(gR)` with a `τ / 0.6` time constant:
                // about `0.7 √(gR)` between `0.5 τ` and `τ`.
                gravity: drift(medium, 1.6 * u * body_drag, body_drag),
                drag: body_drag,
                curl: Some((1.2, 0.08 / big_r.sqrt(), 0.3)),
                // Neutral cooling tint (the sheet carries the hue): bright to 0.3 of its life,
                // too dim to read as flame past about 0.65 (`FLAME_VISIBLE_TINT`).
                color_over_life: ramp(&[
                    (0.0, "#8e8e8e"),
                    (0.3, "#898989"),
                    (0.6, "#616161"),
                    (1.0, "#262626"),
                ]),
                // Most of its size within 0.1 s (the detonation products), the rest by a fifth
                // of its life (the afterburning ball), then it burns down as it lifts.
                size_over_life: keys(&[
                    (0.0, 0.55),
                    (0.03, 0.75),
                    (0.1, 0.88),
                    (0.2, 1.0),
                    (0.45, 0.85),
                    (1.0, 0.55),
                ]),
                alpha_over_life: keys(&[(0.0, 1.0), (0.45, 0.9), (0.75, 0.45), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::FireballAnim,
                flipbook: Some(fireball_sheet()),
                blend: BlendMode::Alpha,
                softness: 0.1 * big_r,
                emissive: 6.0,
                light: Some(ParticleLightDef {
                    color: "#ffa850".into(),
                    intensity: 0.08 * FLASH_INTENSITY_250T * area,
                    radius: LIGHT_REACH * big_r,
                }),
                ..RenderModule::default()
            },
        },
        EmitterDef {
            // Soot formed in the cooling rim: laid on the ball's shell, it wraps the flame from
            // the outside and rises on with it.
            name: "soot".into(),
            delay: 0.22 * tau,
            duration: None,
            spawn: burst(tier.pick(24, 10), (4.0 * tau, 6.0 * tau)),
            init: init(
                ShapeDef::Sphere {
                    radius: r(0.6 * big_r, 0.85 * big_r),
                    surface: true,
                },
                planar((0.1 * u, 0.3 * u), (0.35 * u, 0.55 * u)),
                tier.pick((0.4 * big_r, 0.7 * big_r), (0.55 * big_r, 0.85 * big_r)),
                "#ffffff",
            ),
            update: soot_update.clone(),
            render: lit_puff(0.12 * big_r, 0.85),
        },
        EmitterDef {
            // The rolling ring: soot thrown outward on a ring above the centre as the ball rises,
            // so the sooty mass is wider than tall with a thinner axis (the toroid).
            name: "soot_ring".into(),
            delay: 0.27 * tau,
            duration: None,
            spawn: burst(tier.pick(16, 8), (4.0 * tau, 6.0 * tau)),
            init: init(
                ShapeDef::Cone {
                    angle: 60.0,
                    length: r(0.35 * big_r, 0.5 * big_r),
                },
                planar((0.3 * u, 0.5 * u), (0.45 * u, 0.6 * u)),
                tier.pick((0.45 * big_r, 0.7 * big_r), (0.6 * big_r, 0.85 * big_r)),
                "#ffffff",
            ),
            update: soot_update,
            render: lit_puff(0.12 * big_r, 0.85),
        },
    ];
    if tier == CrashTier::Full {
        emitters.push(EmitterDef {
            // Orange billows rolling out on the ring under the soot.
            name: "billows".into(),
            delay: 0.15 * tau,
            duration: None,
            spawn: burst(24, (0.8 * tau, 1.1 * tau)),
            init: init(
                ShapeDef::Cone {
                    angle: 45.0,
                    length: r(0.45 * big_r, 0.6 * big_r),
                },
                planar((0.15 * u, 0.35 * u), (0.6 * u, 0.85 * u)),
                (0.45 * big_r, 0.65 * big_r),
                "#ffffff",
            ),
            update: UpdateModule {
                gravity: drift(medium, u * body_drag, body_drag),
                drag: body_drag,
                curl: Some((1.5, 0.1 / big_r.sqrt(), 0.4)),
                color_over_life: cooling_tint("#262626"),
                size_over_life: keys(&[(0.0, 0.5), (2.0 / 7.0, 1.0), (1.0, 0.8)]),
                alpha_over_life: keys(&[(0.0, 0.9), (3.0 / 7.0, 0.8), (5.0 / 7.0, 0.4), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::FireballAnim,
                flipbook: Some(fireball_sheet()),
                blend: BlendMode::Alpha,
                softness: 0.08 * big_r,
                emissive: 5.0,
                ..RenderModule::default()
            },
        });
        emitters.push(EmitterDef {
            // Hot air over the ball bends the scene behind it (any HDR frame with a scene copy).
            name: "heat_haze".into(),
            delay: 0.02 * tau,
            duration: None,
            spawn: burst(3, (0.5 * tau, 0.65 * tau)),
            init: init(
                ShapeDef::Cone {
                    angle: 20.0,
                    length: r(0.45 * big_r, 0.55 * big_r),
                },
                upward(10.0, (0.5 * u, 0.6 * u)),
                (1.8 * big_r, 2.0 * big_r),
                "#ffffff",
            ),
            update: UpdateModule {
                size_over_life: keys(&[(0.0, 0.8), (1.0, 1.3)]),
                alpha_over_life: keys(&[(0.0, 0.0), (1.0 / 7.0, 1.0), (5.0 / 7.0, 0.7), (1.0, 0.0)]),
                ..UpdateModule::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::SoftDisc,
                heat_haze: 8.0,
                ..RenderModule::default()
            },
        });
    }
    system(
        fireball_effect(scale),
        512,
        lod(scale, 1_500.0, 4_000.0),
        emitters,
    )
}

/// How a level camera sees a fireball at one moment ([`fireball_footprint`]): the projected
/// areas of flame and soot and the shape of the sooty mass. Lengths in metres, areas in m²,
/// heights above the crash point.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FireballFootprint {
    /// Seconds since the detonation.
    pub time: f32,
    /// Projected area that reads as flame (orange): opaque, and mostly fire cards still hot
    /// enough to glow ([`FLAME_VISIBLE_TINT`]).
    pub flame_area: f32,
    /// Diameter of the disc with [`Self::flame_area`] (the equivalent diameter; X1: 150–250 m at
    /// its largest for 250 t).
    pub flame_diameter: f32,
    /// Projected area that is opaque but not flame: soot and cooled fire.
    pub soot_area: f32,
    /// Horizontal extent of everything opaque.
    pub width: f32,
    /// Vertical extent of everything opaque above the ground.
    pub height: f32,
    /// Highest opaque point.
    pub top: f32,
    /// Alpha-weighted mean height of the visible fire cards (of the soot once the fire is gone).
    pub centre_height: f32,
    /// Median horizontal distance of the soot puffs from their vertical axis (0 without soot).
    pub soot_ring_radius: f32,
    /// 25th percentile of that distance over its median: near 0 for a solid blob, near 1 for a
    /// thin ring (the toroid's hole).
    pub soot_hole: f32,
    /// Twice [`Self::soot_ring_radius`] over the soot centres' height spread (10th to 90th
    /// percentile): above 1 for a mass wider than tall.
    pub soot_aspect: f32,
}

/// How an emitter shows in the footprint: flame cards, opaque puffs, or nothing (the additive
/// flash glow, heat haze, meshes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Flame,
    Puff,
    Skip,
}

fn look_of(e: &EmitterDef) -> Look {
    let r = &e.render;
    if r.heat_haze > 0.0 || r.blend == BlendMode::Additive || r.mode == RenderMode::Mesh {
        Look::Skip
    } else if r.sprite == SpriteId::FireballAnim {
        Look::Flame
    } else {
        Look::Puff
    }
}

fn percentile(values: &mut [f32], q: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f32::total_cmp);
    let i = ((values.len() - 1) as f32 * q.clamp(0.0, 1.0)).round() as usize;
    values[i.min(values.len() - 1)]
}

/// Cells per fireball radius of the footprint raster.
const CELLS_PER_R: f32 = 60.0;

/// The fireball `def` (a `crash_fireball@<scale>` definition) as a level camera far to its −Z side
/// sees it at each of `times` (seconds after the detonation): played with the CPU probe at
/// [`FIREBALL_SPAWN_HEIGHT`] radii above flat ground (seed 7, 60 Hz), every visible card is laid
/// down back to front as a disc of its opaque part ([`SHEET_FLAME_RADIUS`] of a fire card's width,
/// [`PUFF_BODY_RADIUS`] of a puff's) on a `R / 60` grid in the vertical plane, with its alpha and
/// whether it is flame, and the ground cuts what is below it. A cell is flame where the
/// accumulated flame coverage is at least one half, soot where it is opaque otherwise. The flash
/// glow (additive), heat haze and meshes are left out. Cost: a few million cell writes per time
/// at most (milliseconds in release).
#[must_use]
pub fn fireball_footprint(def: &VfxSystemDef, times: &[f32]) -> Vec<FireballFootprint> {
    let scale = CrashScale::from_effect_name(&def.name).unwrap_or(CrashScale::VEHICLE);
    let big_r = scale.fireball_radius();
    let origin = Vec3::new(0.0, FIREBALL_SPAWN_HEIGHT * big_r, 0.0);
    let cell = big_r / CELLS_PER_R;
    // Columns span ±3 R, rows 0..4 R above the ground.
    let cols = (6.0 * CELLS_PER_R) as usize;
    let rows = (4.0 * CELLS_PER_R) as usize;
    let half = 3.0 * big_r;
    let looks: Vec<Look> = def.emitters.iter().map(look_of).collect();
    let flame_tint: Vec<f32> = def.emitters.iter().map(|e| e.render.emissive.max(0.0)).collect();
    let mut cover = vec![0.0f32; cols * rows];
    let mut flame = vec![0.0f32; cols * rows];
    probe_particles(def, origin, Vec3::Y, ProbeSettings::default(), times)
        .into_iter()
        .map(|(time, mut particles)| {
            cover.fill(0.0);
            flame.fill(0.0);
            particles.retain(|p| {
                p.alpha > crate::probe::PROBE_VISIBLE_ALPHA
                    && looks.get(p.emitter).is_some_and(|l| *l != Look::Skip)
            });
            // Far first: the camera looks along +Z, so larger z is further away.
            particles.sort_by(|a, b| b.position.z.total_cmp(&a.position.z));
            let mut centre = (0.0f32, 0.0f32);
            let mut soot_centre = (Vec3::ZERO, 0.0f32);
            for p in &particles {
                let is_flame = looks[p.emitter] == Look::Flame;
                let hot = is_flame && luma(p.rgb) * flame_tint[p.emitter] >= FLAME_VISIBLE_TINT;
                let alpha = p.alpha.clamp(0.0, 1.0);
                if is_flame {
                    centre.0 += alpha * p.position.y;
                    centre.1 += alpha;
                } else {
                    soot_centre.0 += p.position * alpha;
                    soot_centre.1 += alpha;
                }
                let radius = p.size
                    * if is_flame {
                        SHEET_FLAME_RADIUS
                    } else {
                        PUFF_BODY_RADIUS
                    };
                let f = if hot { 1.0 } else { 0.0 };
                let x0 = ((p.position.x - radius + half) / cell).floor().max(0.0) as usize;
                let x1 = (((p.position.x + radius + half) / cell).ceil().max(0.0) as usize).min(cols);
                let y0 = ((p.position.y - radius) / cell).floor().max(0.0) as usize;
                let y1 = (((p.position.y + radius) / cell).ceil().max(0.0) as usize).min(rows);
                for y in y0..y1 {
                    let cy = (y as f32 + 0.5) * cell - p.position.y;
                    for x in x0..x1 {
                        let cx = (x as f32 + 0.5) * cell - half - p.position.x;
                        if cx * cx + cy * cy > radius * radius {
                            continue;
                        }
                        let i = y * cols + x;
                        cover[i] = alpha + (1.0 - alpha) * cover[i];
                        flame[i] = alpha * f + (1.0 - alpha) * flame[i];
                    }
                }
            }
            let mut flame_cells = 0u32;
            let mut soot_cells = 0u32;
            let mut extent: Option<(usize, usize, usize, usize)> = None;
            for y in 0..rows {
                for x in 0..cols {
                    let i = y * cols + x;
                    if cover[i] < 0.5 {
                        continue;
                    }
                    if flame[i] >= 0.5 {
                        flame_cells += 1;
                    } else {
                        soot_cells += 1;
                    }
                    extent = Some(match extent {
                        None => (x, x, y, y),
                        Some((l, rt, b, t)) => (l.min(x), rt.max(x), b.min(y), t.max(y)),
                    });
                }
            }
            let cell_area = cell * cell;
            let flame_area = flame_cells as f32 * cell_area;
            let (width, height, top) = extent.map_or((0.0, 0.0, 0.0), |(l, rt, b, t)| {
                (
                    (rt - l + 1) as f32 * cell,
                    (t - b + 1) as f32 * cell,
                    (t + 1) as f32 * cell,
                )
            });
            let centre_height = if centre.1 > 0.0 {
                centre.0 / centre.1
            } else if soot_centre.1 > 0.0 {
                soot_centre.0.y / soot_centre.1
            } else {
                0.0
            };
            let soot: Vec<Vec3> = particles
                .iter()
                .filter(|p| looks[p.emitter] == Look::Puff)
                .map(|p| p.position)
                .collect();
            let (ring, hole, aspect) = if soot.len() >= 4 && soot_centre.1 > 0.0 {
                let c = soot_centre.0 / soot_centre.1;
                let mut flat: Vec<f32> = soot
                    .iter()
                    .map(|p| Vec3::new(p.x - c.x, 0.0, p.z - c.z).length())
                    .collect();
                let mut ys: Vec<f32> = soot.iter().map(|p| p.y).collect();
                let median = percentile(&mut flat, 0.5);
                let p25 = percentile(&mut flat, 0.25);
                let spread = percentile(&mut ys, 0.9) - percentile(&mut ys, 0.1);
                (
                    median,
                    if median > 0.0 { p25 / median } else { 0.0 },
                    2.0 * median / spread.max(1e-3),
                )
            } else {
                (0.0, 0.0, 0.0)
            };
            FireballFootprint {
                time,
                flame_area,
                flame_diameter: 2.0 * (flame_area / std::f32::consts::PI).sqrt(),
                soot_area: soot_cells as f32 * cell_area,
                width,
                height,
                top,
                centre_height,
                soot_ring_radius: ring,
                soot_hole: hole,
                soot_aspect: aspect,
            }
        })
        .collect()
}

/// The fireball's acceptance numbers for [`super::measure_crash_effect`]: the seen peak (the
/// largest [`FireballFootprint::flame_diameter`] over `0..1.6 τ` in `τ / 30` steps, as a radius,
/// and when), when no fire layer is visible any more, and the body's rise.
pub(super) fn measure_fireball(def: &VfxSystemDef, scale: CrashScale, out: &mut CrashMeasure) {
    let tau = scale.time_scale();
    let step = tau / 30.0;
    let times: Vec<f32> = (1..=48).map(|i| i as f32 * step).collect();
    let peak = fireball_footprint(def, &times)
        .into_iter()
        .fold((0.0f32, 0.0f32), |best, f| {
            if f.flame_diameter > best.1 {
                (f.time, f.flame_diameter)
            } else {
                best
            }
        });
    out.fireball_peak_time = Some(peak.0);
    out.fireball_peak_radius = Some(0.5 * peak.1);
    let run = |times: &[f32]| probe_effect(def, Vec3::ZERO, Vec3::Y, ProbeSettings::default(), times);
    let visible =
        |snap: &crate::probe::ProbeSnapshot, name: &str| snap.emitter(name).map_or(0, |e| e.visible);
    let steps: Vec<f32> = (1..=(2.0 * tau / 0.05).round() as u32)
        .map(|i| i as f32 * 0.05)
        .collect();
    let fire = ["flash", "fireball", "billows"];
    out.fire_gone_at = run(&steps)
        .iter()
        .find(|s| s.time > peak.0 && fire.iter().all(|f| visible(s, f) == 0))
        .map(|s| s.time);
    let rise = run(&[0.5 * tau, tau]);
    if let [a, b] = rise.as_slice()
        && let (Some(a), Some(b)) = (a.emitter("fireball"), b.emitter("fireball"))
    {
        out.fireball_rise = Some((b.centroid.y - a.centroid.y) / (0.5 * tau));
    }
}

#[cfg(test)]
mod tests;
