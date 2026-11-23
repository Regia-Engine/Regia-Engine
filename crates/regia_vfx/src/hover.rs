// SPDX-License-Identifier: MPL-2.0
//! Hover-unit ground interaction: the dust, ice and frost a hovering hull kicks up.
//!
//! A hover hull holds itself up on a downward jet (an air cushion or lift fans). Where that jet
//! meets the ground it turns into a radial wall jet that runs out along the surface and lifts
//! loose material at the hull's edge. Three things set how it looks:
//!
//! - **Height.** The ground effect of a jet or cushion falls off quickly once the hull is more
//!   than about one diameter up (helicopter and hovercraft ground-effect data). [`hover_intensity`]
//!   is `(1 − h / 2R)²` for a footprint radius `R`: full on the ground, a quarter at one
//!   radius up, nothing past one diameter.
//! - **Surface.** [`HoverSurface`]: Mars dust (butterscotch regolith fines, a few µm), Europa
//!   water ice (bright crystalline grains that glint as they tumble) and frost (fine powder).
//! - **Air.** The level's [`VfxMedium`], as for explosions. In air the wall jet carries the
//!   fines up into a rolling skirt that drifts and settles (Earth: settles in a couple of
//!   seconds; Mars: fine dust lingers for several seconds and drifts with the wind, as in the
//!   Ingenuity rotor-dust footage). **Airless**, nothing carries the grains once they leave the
//!   jet: they fly on ballistic arcs under the level's gravity and land, with no lingering cloud
//!   (the Apollo lunar-module descent footage: a sheet of grains streaming out flat and fast).
//!
//! The effects play with the instance's forward on the ground normal (local +Z = up). One
//! effect fits every hull through [`VfxInstance::set_scale`](crate::sim::VfxInstance::set_scale)
//! (the footprint over [`HOVER_REFERENCE_RADIUS`]) and the height through
//! [`VfxInstance::set_intensity`](crate::sim::VfxInstance::set_intensity).
//!
//! The ECS surface is the [`HoverWash`] component: gameplay sets the ground point under the hull
//! and the height above it each frame (DO27's simulation knows both), and [`drive_hover_washes`]
//! keeps one effect instance per hull.

use std::collections::HashMap;

use regia_app::prelude::*;
use regia_core::math::Vec3;

use crate::atmos_presets::{drift, keys, r, ramp, settle};
use crate::format::{
    BlendMode, DepthCollisionDef, EmitterDef, FlipbookDef, FlipbookMode, InitModule, RenderMode,
    RenderModule, ShapeDef, SpawnMode, SpawnModule, SpriteId, StretchDef, SystemModule, UpdateModule,
    VFX_FORMAT_VERSION, VelocityDef, VelocityMode, VfxSystemDef,
};
use crate::medium::{VfxAtmosphere, VfxMedium};
use crate::scene::VfxScene;

/// The footprint radius the hover effects are authored for, metres (a mid-size DO27 hull: the
/// Kraken and Manta are 3.5–4.5 m half-width, the gleaner 4.4 m, the X11 1.5 m).
pub const HOVER_REFERENCE_RADIUS: f32 = 3.0;

/// Ground-point speed above which [`drive_hover_washes`] treats a move as a jump (m/s): far above
/// any hover hull, so only a respawn or a teleport passes it.
const HOVER_JUMP_SPEED: f32 = 200.0;

/// The ground under a hover hull.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Reflect)]
pub enum HoverSurface {
    /// From the level's medium: Mars → [`Self::Dust`], airless (Europa) → [`Self::Ice`],
    /// Earth → [`Self::Dust`].
    #[default]
    Auto,
    /// Loose regolith fines (Mars: butterscotch).
    Dust,
    /// Water-ice grains (Europa): bright, crystalline, glinting.
    Ice,
    /// Fine frost or snow powder.
    Frost,
}

impl HoverSurface {
    /// The three concrete surfaces.
    pub const ALL: [Self; 3] = [Self::Dust, Self::Ice, Self::Frost];

    /// Lower-case name (`auto`, `dust`, `ice`, `frost`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dust => "dust",
            Self::Ice => "ice",
            Self::Frost => "frost",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Auto, Self::Dust, Self::Ice, Self::Frost]
            .into_iter()
            .find(|s| s.name() == name)
    }

    /// The concrete surface in `medium` ([`Self::Auto`] resolves from the air).
    #[must_use]
    pub fn resolve(self, medium: &VfxMedium) -> Self {
        match self {
            Self::Auto => match medium.atmosphere {
                VfxAtmosphere::Airless => Self::Ice,
                VfxAtmosphere::Mars | VfxAtmosphere::Earth => Self::Dust,
            },
            other => other,
        }
    }

    /// Base effect name (`hover_wash_dust`, …); [`Self::Auto`] has none.
    #[must_use]
    pub fn effect(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Dust => Some("hover_wash_dust"),
            Self::Ice => Some("hover_wash_ice"),
            Self::Frost => Some("hover_wash_frost"),
        }
    }
}

/// How strongly a hull `height` metres above the ground stirs it, for a footprint of
/// `footprint_radius` metres: `(1 − h / 2R)²`, 1 at (or below) the ground, 0 from one diameter
/// up. See the module docs.
#[must_use]
pub fn hover_intensity(height: f32, footprint_radius: f32) -> f32 {
    let reach = 2.0 * footprint_radius.max(0.1);
    if !height.is_finite() {
        return 0.0;
    }
    let k = (1.0 - height.max(0.0) / reach).clamp(0.0, 1.0);
    k * k
}

/// The look of one surface, shared by every medium.
struct SurfaceLook {
    /// Billow colour at birth and as it thins.
    billow: (&'static str, &'static str),
    /// Billow size at birth.
    billow_size: (f32, f32),
    /// Peak billow opacity.
    billow_alpha: f32,
    /// Grain colour.
    grain: &'static str,
    /// Grain size.
    grain_size: (f32, f32),
    /// Grains per second at full intensity (airless: the whole wash).
    grain_rate: f32,
    /// Crystals glint in the sun.
    glints: bool,
}

fn look(surface: HoverSurface, medium: &VfxMedium) -> SurfaceLook {
    let mars = medium.atmosphere == VfxAtmosphere::Mars;
    match surface {
        HoverSurface::Ice => SurfaceLook {
            billow: ("#dfe7ee", "#eef3f7"),
            billow_size: (0.35, 0.8),
            billow_alpha: 0.55,
            grain: "#eef5fb",
            grain_size: (0.02, 0.05),
            grain_rate: 110.0,
            glints: true,
        },
        HoverSurface::Frost => SurfaceLook {
            billow: ("#eceff2", "#f5f7f9"),
            billow_size: (0.3, 0.7),
            billow_alpha: 0.5,
            grain: "#f4f7fa",
            grain_size: (0.015, 0.035),
            grain_rate: 60.0,
            glints: false,
        },
        HoverSurface::Dust | HoverSurface::Auto => SurfaceLook {
            // Airborne fines read paler than the soil they come from (finer grains scatter more
            // and the cloud is lit through): Mars' bright butterscotch dust, grey-brown elsewhere.
            billow: if mars {
                ("#b3967a", "#c8b39d")
            } else {
                ("#a39583", "#b8ad9f")
            },
            billow_size: (0.5, 1.0),
            billow_alpha: 0.65,
            grain: if mars { "#7d6450" } else { "#6f655a" },
            grain_size: (0.02, 0.045),
            grain_rate: 90.0,
            glints: false,
        },
    }
}

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

/// The wall-jet ring under the hull: a cone around local +Z (up) whose wide base is a disc of
/// about [`HOVER_REFERENCE_RADIUS`] a few tenths of a metre up. Radial velocity then points out
/// of the cone's apex, so most particles leave low and outward (area-uniform sampling puts most
/// of them near the rim), the way the wall jet runs along the ground.
fn wall_jet(angle: f32, length: (f32, f32)) -> ShapeDef {
    ShapeDef::Cone {
        angle,
        length: r(length.0, length.1),
    }
}

/// The hover wash for `surface` with Earth-air dynamics, named `hover_wash_<surface>`.
#[must_use]
pub fn hover_wash(surface: HoverSurface) -> VfxSystemDef {
    hover_wash_in(surface, &VfxMedium::earth())
}

/// The hover wash for `surface` in `medium`, named for it (`hover_wash_ice@airless`, …).
/// [`HoverSurface::Auto`] resolves from the medium.
#[must_use]
pub fn hover_wash_in(surface: HoverSurface, medium: &VfxMedium) -> VfxSystemDef {
    let surface = surface.resolve(medium);
    let base = surface.effect().unwrap_or("hover_wash_dust");
    let look = look(surface, medium);
    let g = medium.gravity_vec();
    let mut emitters = match medium.atmosphere {
        VfxAtmosphere::Airless => airless_wash(&look, medium),
        VfxAtmosphere::Earth | VfxAtmosphere::Mars => air_wash(&look, medium),
    };
    if look.glints {
        // Tumbling ice crystals catch the sun for a frame or two: tiny, short, additive.
        emitters.push(EmitterDef {
            name: "glints".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Rate(50.0),
                lifetime: r(0.05, 0.12),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: wall_jet(80.0, (0.25, 0.45)),
                velocity: VelocityDef {
                    mode: VelocityMode::Radial { speed: r(1.0, 3.0) },
                    inherit: 0.0,
                },
                size: r(0.05, 0.1),
                rotation: r(0.0, std::f32::consts::TAU),
                spin: r(0.0, 0.0),
                color: ramp(&[(0.0, "#fff6e8")]),
            },
            update: UpdateModule {
                gravity: g,
                alpha_over_life: keys(&[(0.0, 0.0), (0.3, 1.0), (1.0, 0.0)]),
                ..Default::default()
            },
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::Flare,
                blend: BlendMode::Additive,
                // Below the fire family's HDR range: a glint, never a muzzle flash.
                emissive: 1.1,
                ..Default::default()
            },
        });
    }
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: medium.variant_name(base).unwrap_or_else(|| base.to_string()),
        system: SystemModule {
            bounds: None,
            budget: 2048,
            lod: Some(crate::format::LodDef {
                distances: vec![60.0, 160.0, 400.0],
                rate_scale: vec![1.0, 0.5, 0.25, 0.0],
            }),
        },
        emitters,
    }
}

/// Air: the brownout pattern. The downward jet turns into a radial wall jet at the ground, so
/// the loose fines first stream outward in a flat, fast sheet hugging the surface (stretched
/// along their velocity, soft against the ground), then slow at the jet's edge, roll up and
/// rise as round billows that drift with the wind; a spray of grit drops out of the sheet.
/// (Rotorcraft brownout: Milluzzo & Leishman, J. AHS 55, 2010; the same wall-jet physics.)
fn air_wash(look: &SurfaceLook, medium: &VfxMedium) -> Vec<EmitterDef> {
    let mars = medium.atmosphere == VfxAtmosphere::Mars;
    let alpha = look.billow_alpha;
    // The outflow: many faint, soft puffs just above the ground (3 to 6 degrees of elevation),
    // fast, drawn a little longer along their velocity while the jet is fast and round once it
    // has slowed. Together they read as one flat, turbid layer. The stretch is a short exposure
    // (`velocity_scale` in seconds), so no puff draws longer than about twice its width: at
    // 0.25 s each puff was a 1.5-2.75 m streak about ten times longer than wide, and the ring of
    // radial streaks read as hair, not dust.
    let sheet = EmitterDef {
        name: "sheet".into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Rate(if mars { 230.0 } else { 200.0 }),
            lifetime: if mars { r(1.0, 1.8) } else { r(0.7, 1.3) },
            count_scale: 1.0,
        },
        init: InitModule {
            shape: wall_jet(87.0, (0.08, 0.15)),
            velocity: VelocityDef {
                mode: VelocityMode::Radial { speed: r(6.0, 11.0) },
                inherit: 0.2,
            },
            // The sheet is a few tens of centimetres deep.
            size: r(look.billow_size.0 * 0.55, look.billow_size.1 * 0.45),
            rotation: r(0.0, 0.0),
            spin: r(0.0, 0.0),
            color: ramp(&[(0.0, look.billow.0), (1.0, look.billow.1)]),
        },
        update: UpdateModule {
            gravity: if mars {
                drift(medium, 0.15, 1.6)
            } else {
                (0.0, 0.3, 0.0)
            },
            drag: if mars { 1.6 } else { 2.0 },
            size_over_life: keys(&[(0.0, 1.0), (0.5, 2.0), (1.0, 3.2)]),
            alpha_over_life: keys(&[(0.0, 0.0), (0.08, alpha * 0.5), (0.5, alpha * 0.32), (1.0, 0.0)]),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Stretched,
            sprite: SpriteId::Puff,
            blend: BlendMode::Alpha,
            stretch: StretchDef {
                velocity_scale: 0.018,
                // Size-relative: round at rest, a little longer when fast.
                min_length: -1.0,
            },
            softness: 2.2,
            lit: true,
            self_shadow: 0.45,
            screen_floor_px: 1.5,
            ..Default::default()
        },
    };
    // The roll-up: puffs carried out a little further, which slow at the jet's edge, lift and
    // grow into round billows (Mars: lingering and drifting downwind).
    // Many large, faint, overlapping billows: one cloud body, not separate puffs.
    let (rate, life, drag, grow) = if mars {
        (50.0, r(3.5, 6.0), 1.1, 4.0)
    } else {
        (44.0, r(1.6, 2.6), 1.5, 3.2)
    };
    let billow = EmitterDef {
        name: "billow".into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Rate(rate),
            lifetime: life,
            count_scale: 1.0,
        },
        init: InitModule {
            shape: wall_jet(86.0, (0.1, 0.18)),
            velocity: VelocityDef {
                mode: VelocityMode::Radial { speed: r(5.0, 9.0) },
                inherit: 0.25,
            },
            size: r(look.billow_size.0 * 1.0, look.billow_size.1 * 1.0),
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(-0.3, 0.3),
            color: ramp(&[(0.0, look.billow.0), (1.0, look.billow.1)]),
        },
        update: UpdateModule {
            gravity: if mars {
                drift(medium, 0.18, drag)
            } else {
                (0.0, 0.45, 0.0)
            },
            drag,
            curl: Some((0.8, 0.4, 0.25)),
            size_over_life: keys(&[(0.0, 0.7), (0.3, 1.6), (1.0, grow)]),
            alpha_over_life: keys(&[(0.0, 0.0), (0.1, alpha * 0.55), (0.45, alpha * 0.38), (1.0, 0.0)]),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Stretched,
            sprite: SpriteId::PuffAnim,
            flipbook: Some(puff_sheet()),
            blend: BlendMode::Alpha,
            stretch: StretchDef {
                // As the sheet: a fresh, fast billow is a soft oval, never a streak.
                velocity_scale: 0.025,
                min_length: -1.0,
            },
            softness: 2.5,
            lit: true,
            self_shadow: 0.35,
            screen_floor_px: 2.0,
            ..Default::default()
        },
    };
    let grit = EmitterDef {
        name: "grit".into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Rate(look.grain_rate),
            lifetime: if mars { r(0.5, 1.1) } else { r(0.25, 0.6) },
            count_scale: 1.0,
        },
        init: InitModule {
            shape: wall_jet(84.0, (0.15, 0.3)),
            velocity: VelocityDef {
                mode: VelocityMode::Radial { speed: r(6.0, 12.0) },
                inherit: 0.1,
            },
            size: r(look.grain_size.0, look.grain_size.1),
            rotation: r(0.0, 0.0),
            spin: r(0.0, 0.0),
            color: ramp(&[(0.0, look.grain)]),
        },
        update: UpdateModule {
            gravity: medium.gravity_vec(),
            drag: if mars { 0.25 } else { 1.5 },
            alpha_over_life: keys(&[(0.0, 0.0), (0.05, 0.8), (0.7, 0.6), (1.0, 0.0)]),
            depth_collision: Some(DepthCollisionDef {
                kill: true,
                offset: 0.05,
            }),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Stretched,
            sprite: SpriteId::SoftDisc,
            blend: BlendMode::Alpha,
            stretch: StretchDef {
                velocity_scale: 0.04,
                min_length: look.grain_size.0,
            },
            softness: 0.2,
            lit: true,
            self_shadow: 0.3,
            screen_floor_px: 1.0,
            ..Default::default()
        },
    };
    vec![sheet, billow, grit]
}

/// Airless: only grains. They leave the wall jet low and fast, fly ballistic arcs under the
/// level's gravity with no drag and land where they fall (depth collision, then fade). Small
/// clumps (a few grains with a spread of velocities) grow linearly and thin as they fly, so the
/// sheet goes transparent instead of billowing.
fn airless_wash(look: &SurfaceLook, medium: &VfxMedium) -> Vec<EmitterDef> {
    let g = medium.gravity_vec();
    let grains = EmitterDef {
        name: "grains".into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Rate(look.grain_rate * 4.0),
            lifetime: r(1.5, 3.2),
            count_scale: 1.0,
        },
        init: InitModule {
            shape: wall_jet(78.0, (0.3, 0.6)),
            velocity: VelocityDef {
                mode: VelocityMode::Radial { speed: r(2.0, 5.5) },
                inherit: 0.0,
            },
            // Larger than the air wash's grit: each stands for a few grains streaking together
            // (sub-pixel grains vanish on white ice).
            size: r(look.grain_size.0 * 1.8, look.grain_size.1 * 2.0),
            rotation: r(0.0, 0.0),
            spin: r(0.0, 0.0),
            color: ramp(&[(0.0, look.grain)]),
        },
        update: UpdateModule {
            gravity: g,
            drag: 0.0,
            alpha_over_life: keys(&[(0.0, 0.0), (0.03, 0.85), (0.8, 0.7), (1.0, 0.0)]),
            depth_collision: settle(0.03),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Stretched,
            sprite: SpriteId::SoftDisc,
            blend: BlendMode::Alpha,
            stretch: StretchDef {
                velocity_scale: 0.05,
                min_length: look.grain_size.0,
            },
            softness: 0.2,
            lit: true,
            // Grains are tiny: lit by the full sun, no self-shadow (a shaded grain reads as a dark
            // speck against the black sky).
            self_shadow: 0.0,
            screen_floor_px: 1.2,
            ..Default::default()
        },
    };
    let clumps = EmitterDef {
        name: "clumps".into(),
        delay: 0.0,
        duration: None,
        spawn: SpawnModule {
            mode: SpawnMode::Rate(130.0),
            lifetime: r(0.8, 1.6),
            count_scale: 1.0,
        },
        init: InitModule {
            // Low and flat: the clumps make the thin veil that streams out along the ground and
            // softens its shadows and texture (the Apollo descent sheet), then thins out.
            shape: wall_jet(84.0, (0.15, 0.3)),
            velocity: VelocityDef {
                // Slow: a clump launched steeply near the jet's axis must not climb into the sky,
                // where a soft disc reads as a lens blob.
                mode: VelocityMode::Radial { speed: r(2.0, 4.0) },
                inherit: 0.0,
            },
            size: r(0.25, 0.45),
            rotation: r(0.0, std::f32::consts::TAU),
            spin: r(0.0, 0.0),
            color: ramp(&[(0.0, look.billow.0)]),
        },
        update: UpdateModule {
            gravity: g,
            drag: 0.0,
            // Grows linearly in vacuum; opacity ∝ 1/size² as the grains spread.
            size_over_life: keys(&[(0.0, 1.0), (1.0, 3.5)]),
            alpha_over_life: keys(&[(0.0, 0.0), (0.04, 0.45), (0.25, 0.2), (0.5, 0.06), (1.0, 0.0)]),
            depth_collision: Some(DepthCollisionDef {
                kill: true,
                offset: 0.1,
            }),
            ..Default::default()
        },
        render: RenderModule {
            mode: RenderMode::Billboard,
            sprite: SpriteId::SoftDisc,
            blend: BlendMode::Alpha,
            softness: 0.6,
            lit: true,
            self_shadow: 0.3,
            screen_floor_px: 1.5,
            ..Default::default()
        },
    };
    vec![grains, clumps]
}

/// Hover-wash state for one hull. Gameplay sets [`Self::ground`], [`Self::ground_normal`] and
/// [`Self::height`] every frame; [`drive_hover_washes`] keeps the effect on them.
#[derive(Component, Clone, Debug, Reflect)]
#[reflect(Component, Default)]
pub struct HoverWash {
    /// The ground under the hull.
    pub surface: HoverSurface,
    /// Footprint radius of the hull, metres (half its width).
    pub footprint_radius: f32,
    /// Height of the hull's underside above the ground, metres.
    pub height: f32,
    /// World point on the ground under the hull's centre.
    pub ground: Vec3,
    /// World ground normal there.
    pub ground_normal: Vec3,
    /// Off: no new particles (the old ones play out).
    pub enabled: bool,
    /// The playing effect instance (runtime, set by [`drive_hover_washes`]).
    #[reflect(ignore)]
    instance: Option<u64>,
    /// The effect name that instance plays.
    #[reflect(ignore)]
    playing: Option<String>,
    /// The intensity applied last frame.
    #[reflect(ignore)]
    intensity: f32,
}

impl Default for HoverWash {
    fn default() -> Self {
        Self::new(HoverSurface::Auto, HOVER_REFERENCE_RADIUS)
    }
}

impl HoverWash {
    /// A wash for a hull of `footprint_radius` metres over `surface`, at the ground origin.
    #[must_use]
    pub fn new(surface: HoverSurface, footprint_radius: f32) -> Self {
        Self {
            surface,
            footprint_radius,
            height: 0.6,
            ground: Vec3::ZERO,
            ground_normal: Vec3::Y,
            enabled: true,
            instance: None,
            playing: None,
            intensity: 0.0,
        }
    }

    /// The effect name playing (after the first update), e.g. `hover_wash_ice@airless`.
    #[must_use]
    pub fn playing(&self) -> Option<&str> {
        self.playing.as_deref()
    }

    /// The intensity applied last update ([`hover_intensity`], 0 when disabled).
    #[must_use]
    pub fn intensity(&self) -> f32 {
        self.intensity
    }

    /// The effect instance id (after the first update).
    #[must_use]
    pub fn instance(&self) -> Option<u64> {
        self.instance
    }
}

/// Keeps one effect instance per [`HoverWash`]: spawns it (the medium's variant of the
/// surface's wash, registering the built-in when the library lacks it), moves it to the ground
/// point, scales it to the footprint and sets its intensity from the height. A removed wash's
/// instance stops and plays out, and so does a parked one (spec 37 § 13): it starts again when
/// the entity's layer goes live.
#[tracing::instrument(skip_all)]
pub fn drive_hover_washes(
    time: Res<Time>,
    scene: Option<ResMut<VfxScene>>,
    mut washes: Query<(Entity, &mut HoverWash, Has<regia_scene::layers::LayerParked>)>,
    mut removed: RemovedComponents<HoverWash>,
    mut live: Local<HashMap<Entity, u64>>,
) {
    let _span = tracing::debug_span!("drive_hover_washes").entered();
    let Some(mut scene) = scene else { return };
    for entity in removed.read() {
        if let Some(id) = live.remove(&entity) {
            scene.stop(id);
        }
    }
    let dt = time.delta_secs();
    let medium = scene.medium();
    for (entity, mut wash, parked) in &mut washes {
        if parked {
            if let Some(id) = wash.instance.take() {
                scene.stop(id);
            }
            wash.playing = None;
            wash.intensity = 0.0;
            live.remove(&entity);
            continue;
        }
        let surface = wash.surface.resolve(&medium);
        let Some(base) = surface.effect() else { continue };
        if !crate::atmos_presets::ensure_builtin(&mut scene, base) {
            continue;
        }
        let name = scene.resolve(base);
        let alive = wash.instance.is_some_and(|id| scene.instance(id).is_some());
        if !alive || wash.playing.as_deref() != Some(name.as_str()) {
            if let Some(old) = wash.instance.take() {
                scene.stop(old);
            }
            let seed = entity.to_bits() ^ 0x4856_5752;
            wash.instance = scene.spawn(base, wash.ground, wash.ground_normal, seed);
            wash.playing = Some(name);
        }
        let k = if wash.enabled {
            hover_intensity(wash.height, wash.footprint_radius)
        } else {
            0.0
        };
        wash.intensity = k;
        if let Some(id) = wash.instance {
            live.insert(entity, id);
            if let Some(instance) = scene.instance_mut(id) {
                // A ground point that moves faster than any hull flies jumped (a respawn, a
                // teleport): move the wash without lending that speed to new particles, which
                // inherit a share of it (a 500 m jump in one 60 Hz frame is 30 km/s; the sheet
                // would fly off as screen-long streaks for a frame).
                let jumped = dt > 0.0 && instance.origin().distance(wash.ground) > HOVER_JUMP_SPEED * dt;
                instance.set_transform(wash.ground, wash.ground_normal, if jumped { 0.0 } else { dt });
                instance.set_scale(wash.footprint_radius / HOVER_REFERENCE_RADIUS);
                instance.set_intensity(k);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::LodInput;

    /// Longest `length / width` a soft dust quad may have and still read as a puff smeared along
    /// its motion rather than a streak.
    const MAX_SOFT_ASPECT: f32 = 3.0;
    /// Longest velocity stretch of a soft dust quad, metres.
    const MAX_SOFT_STRETCH: f32 = 1.0;

    #[test]
    fn a_parked_wash_stops_and_restarts_when_its_layer_goes_live() {
        let mut app = App::new();
        app.add_plugins(crate::plugin::VfxPlugin);
        app.insert_resource(VfxScene::new(crate::scene::VfxLibrary::new()));
        let mut wash = HoverWash::new(HoverSurface::Dust, 3.0);
        wash.height = 0.5;
        let entity = app.world_mut().spawn(wash).id();
        app.update();
        let state = |app: &App| {
            let wash = app.world().entity(entity).get::<HoverWash>();
            (wash.and_then(|w| w.instance), wash.map_or(-1.0, |w| w.intensity))
        };
        assert!(state(&app).0.is_some(), "a live wash plays");

        app.world_mut()
            .entity_mut(entity)
            .insert(regia_scene::layers::LayerParked {
                restore: regia_scene::components::Visibility::default(),
                inherited: false,
            });
        app.update();
        assert_eq!(state(&app), (None, 0.0), "a parked member shows no effect");

        app.world_mut()
            .entity_mut(entity)
            .remove::<regia_scene::layers::LayerParked>();
        app.update();
        assert!(
            state(&app).0.is_some(),
            "it starts again when the layer goes live"
        );
    }

    /// A cruising hull lends its speed to the wash (inheritance), but a respawn's jump does not:
    /// the sheet would inherit a share of 30 km/s and fly off as screen-long streaks.
    #[test]
    fn a_jumped_wash_does_not_fling_its_particles() {
        let mut app = App::new();
        app.add_plugins(crate::plugin::VfxPlugin);
        app.insert_resource(VfxScene::new(crate::scene::VfxLibrary::new()));
        let dt = 1.0 / 60.0;
        // Fixed 60 Hz frames: no wall clock.
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(f64::from(dt)),
        ));
        let mut wash = HoverWash::new(HoverSurface::Dust, 3.0);
        wash.height = 0.5;
        let entity = app.world_mut().spawn(wash).id();
        let mut x = 0.0;
        let step = |app: &mut App, to: f32| {
            app.world_mut()
                .entity_mut(entity)
                .get_mut::<HoverWash>()
                .expect("wash")
                .ground = Vec3::new(to, 0.0, 0.0);
            app.update();
            let wash = app.world().entity(entity).get::<HoverWash>().expect("wash");
            let id = wash.instance().expect("playing");
            let scene = app.world().resource::<VfxScene>();
            scene.instance(id).expect("instance").velocity()
        };
        for _ in 0..10 {
            x += 16.0 * dt;
            step(&mut app, x);
        }
        let cruise = step(&mut app, x + 16.0 * dt);
        x += 16.0 * dt;
        assert!(
            (cruise.x - 16.0).abs() < 0.5,
            "a cruising hull is inherited: {cruise}"
        );
        let jump = step(&mut app, x + 500.0);
        assert_eq!(jump, Vec3::ZERO, "a jump lends no speed");
        let after = step(&mut app, x + 500.0 + 16.0 * dt);
        assert!(
            (after.x - 16.0).abs() < 0.5,
            "cruise resumes after the jump: {after}"
        );
    }

    #[test]
    fn intensity_falls_off_within_one_diameter() {
        assert!((hover_intensity(0.0, 3.0) - 1.0).abs() < 1e-6);
        assert!(hover_intensity(0.6, 3.0) > 0.8, "the hover height is near full");
        assert!(
            (hover_intensity(3.0, 3.0) - 0.25).abs() < 1e-6,
            "a quarter at one radius"
        );
        assert_eq!(hover_intensity(6.0, 3.0), 0.0);
        assert_eq!(hover_intensity(40.0, 3.0), 0.0);
        assert_eq!(hover_intensity(f32::NAN, 3.0), 0.0);
        let mut last = 2.0;
        for i in 0..40 {
            let k = hover_intensity(i as f32 * 0.2, 3.0);
            assert!(k <= last, "monotonic");
            last = k;
        }
    }

    #[test]
    fn every_wash_validates_round_trips_and_is_named_for_its_medium() {
        for medium in [VfxMedium::earth(), VfxMedium::mars(), VfxMedium::airless(1.315)] {
            for surface in HoverSurface::ALL {
                let def = hover_wash_in(surface, &medium);
                def.validate().expect("valid");
                let back = VfxSystemDef::from_ron(&def.to_ron().expect("ron")).expect("parse");
                assert_eq!(back, def);
                let base = surface.effect().expect("base");
                assert_eq!(def.name, medium.variant_name(base).unwrap_or(base.to_string()));
            }
        }
        assert_eq!(hover_wash(HoverSurface::Ice).name, "hover_wash_ice");
    }

    #[test]
    fn airless_wash_is_ballistic_grains_with_no_cloud() {
        let medium = VfxMedium::airless(1.315);
        for surface in HoverSurface::ALL {
            let def = hover_wash_in(surface, &medium);
            for e in &def.emitters {
                assert_eq!(e.update.drag, 0.0, "{}: drag in vacuum", e.name);
                assert!(e.update.curl.is_none(), "{}: billows", e.name);
                assert!(e.update.gravity.1 <= 0.0, "{}: rises", e.name);
                assert_ne!(e.render.sprite, SpriteId::PuffAnim, "{}: a puff cloud", e.name);
                // Nothing lingers: every particle is gone within a few seconds.
                assert!(e.spawn.lifetime.max <= 3.5, "{}: lingers", e.name);
                if e.update.gravity.1 < 0.0 {
                    assert!((e.update.gravity.1 + 1.315).abs() < 1e-6, "{}", e.name);
                }
            }
        }
    }

    #[test]
    fn mars_dust_lingers_and_drifts_and_earth_settles() {
        let mars = hover_wash_in(HoverSurface::Dust, &VfxMedium::mars());
        let earth = hover_wash(HoverSurface::Dust);
        let billow = |d: &VfxSystemDef| {
            d.emitters
                .iter()
                .find(|e| e.name == "billow")
                .cloned()
                .expect("billow")
        };
        let (m, e) = (billow(&mars), billow(&earth));
        assert!(m.spawn.lifetime.min > e.spawn.lifetime.max, "Mars dust lingers");
        let wind = VfxMedium::mars().wind;
        assert!(m.update.gravity.0 * wind.x > 0.0, "drifts downwind");
        assert!(m.update.gravity.1 < e.update.gravity.1, "rises weakly");
        // Ice glints, dust does not.
        assert!(
            hover_wash(HoverSurface::Ice)
                .emitters
                .iter()
                .any(|e| e.name == "glints")
        );
        assert!(!earth.emitters.iter().any(|e| e.name == "glints"));
    }

    #[test]
    fn air_wash_is_a_flat_stretched_outflow_that_rolls_up() {
        for medium in [VfxMedium::earth(), VfxMedium::mars()] {
            let def = hover_wash_in(HoverSurface::Dust, &medium);
            let sheet = def.emitters.iter().find(|e| e.name == "sheet").expect("sheet");
            assert!(
                matches!(sheet.init.shape, ShapeDef::Cone { angle, .. } if angle >= 86.0),
                "the outflow hugs the ground"
            );
            assert_eq!(sheet.render.mode, RenderMode::Stretched);
            assert!(
                sheet.render.stretch.min_length < 0.0,
                "size-relative: round at rest"
            );
            assert!(sheet.render.softness >= 2.0, "soft against the ground");
            let billow = def.emitters.iter().find(|e| e.name == "billow").expect("billow");
            assert!(billow.update.gravity.1 > 0.0, "the roll-up lifts");
            assert!(billow.render.stretch.min_length < 0.0);
            // Faint layers that add up, not a few opaque balls.
            assert!(matches!(sheet.spawn.mode, SpawnMode::Rate(r) if r >= 100.0));
            assert!(sheet.update.alpha_over_life.iter().all(|k| k.v <= 0.35));
        }
    }

    #[test]
    fn auto_surface_follows_the_medium() {
        assert_eq!(
            HoverSurface::Auto.resolve(&VfxMedium::airless(1.3)),
            HoverSurface::Ice
        );
        assert_eq!(HoverSurface::Auto.resolve(&VfxMedium::mars()), HoverSurface::Dust);
        assert_eq!(
            HoverSurface::Frost.resolve(&VfxMedium::mars()),
            HoverSurface::Frost
        );
        assert_eq!(HoverSurface::from_name("ice"), Some(HoverSurface::Ice));
    }

    /// The worst stretched quad one emitter of a wash draws, measured the way the GPU draws it.
    #[derive(Debug, Default, Clone, Copy)]
    struct StreakStats {
        /// Largest `length / width` of a visible particle's quad.
        aspect: f32,
        /// Largest velocity stretch `velocity_scale * |v|` of a visible particle, metres.
        stretch: f32,
        /// Visible particle samples measured.
        samples: u32,
        /// Opacity-weighted quad area over the particles' lives, m² s (how much of the view the
        /// emitter fills; reported, not asserted).
        coverage: f32,
    }

    /// Opacity under which a particle sample is not counted (it cannot read as a streak).
    const VISIBLE_ALPHA: f32 = 0.02;

    /// Bound on the curl-noise field's magnitude before its amount and speed factors (the
    /// shader's `curl_noise`: finite differences of a smooth unit-range potential).
    const CURL_GRADIENT_BOUND: f32 = 4.0;

    /// Plays `def` for one second on a hull of `scale` × the reference footprint moving along
    /// the ground at `hull_speed` m/s, takes the spawn jobs of emitter `name` and steps each one
    /// over its life with the GPU update's implicit-Euler drag (`gpu/shaders.rs`, the
    /// `gravity_drag` step) and the stretched-billboard length rule
    /// (`along = max(velocity_scale × speed, min_len)`, a negative `min_length` being a multiple
    /// of the current size). Width is the current size; the screen-size floor only widens a
    /// quad, so leaving it out overstates the aspect, never understates it. Curl turbulence is bounded, not sampled: its
    /// largest push (`amount × speed × CURL_GRADIENT_BOUND`) is added along the velocity every
    /// step, which again can only overstate the length.
    fn streaks(
        def: &VfxSystemDef,
        name: &str,
        medium: &VfxMedium,
        scale: f32,
        hull_speed: f32,
    ) -> StreakStats {
        let index = def.emitters.iter().position(|e| e.name == name).expect("emitter");
        let e = &def.emitters[index];
        let dt = 1.0 / 60.0;
        let lod = LodInput {
            camera_pos: Vec3::new(0.0, 3.0, 15.0),
        };
        let mut inst = crate::sim::VfxInstance::new(0, 0x5745_4153, Vec3::ZERO, Vec3::Y);
        inst.set_scale(scale);
        let mut jobs = Vec::new();
        for frame in 1..=60 {
            let origin = Vec3::new(hull_speed * dt * frame as f32, 0.0, 0.0);
            inst.set_transform(origin, Vec3::Y, dt);
            inst.schedule(def, dt, &lod, 100_000);
            inst.drain_emitter_jobs(index, &mut jobs);
        }
        assert!(!jobs.is_empty(), "{name}: spawns nothing");
        let u = &e.update;
        let g = Vec3::new(u.gravity.0, u.gravity.1, u.gravity.2);
        let c = u.wind_response * u.wind_drag;
        let wind = if c > 0.0 { medium.wind * c } else { Vec3::ZERO };
        let (vs, min) = (e.render.stretch.velocity_scale, e.render.stretch.min_length);
        let mut out = StreakStats::default();
        for job in &jobs {
            let mut v = Vec3::from_array(job.velocity);
            let mut age = 0.0;
            while age < job.lifetime {
                let t = age / job.lifetime;
                let size = job.size * crate::curves::sample_scalar(&u.size_over_life, t, 1.0);
                let alpha = job.color[3] * crate::curves::sample_scalar(&u.alpha_over_life, t, 1.0);
                if alpha >= VISIBLE_ALPHA && size > 0.0 {
                    let stretch = vs * v.length();
                    let min_len = if min < 0.0 { -min * size } else { min };
                    out.aspect = out.aspect.max(stretch.max(min_len) / size);
                    out.stretch = out.stretch.max(stretch);
                    out.samples += 1;
                    out.coverage += alpha * stretch.max(min_len) * size * dt;
                }
                v = (v + (g + wind) * dt) / (1.0 + (u.drag.max(0.0) + c) * dt);
                if let Some((amount, _, speed)) = u.curl {
                    v += v.normalize_or_zero() * (amount * speed * CURL_GRADIENT_BOUND * dt);
                }
                age += dt;
            }
        }
        out
    }

    /// Asserts that no visible particle of emitter `name` in any air wash is a streak (see
    /// [`MAX_SOFT_ASPECT`] and [`MAX_SOFT_STRETCH`]): every surface, in Earth air and on Mars, for
    /// a quarter of the reference footprint (a 0.75 m drone; DO27's own strength curve scales a
    /// parked wash down to about 0.41) to twice it, parked or cruising at full brownout speed
    /// (16 m/s, where the hull's motion adds the most inherited velocity). Launch speeds scale by
    /// √scale and sizes by scale, so the smallest hull draws the longest quads for its width.
    fn assert_air_wash_emitter_has_no_streak(name: &str) -> StreakStats {
        let mut worst = StreakStats::default();
        for medium in [VfxMedium::earth(), VfxMedium::mars()] {
            for surface in HoverSurface::ALL {
                let def = hover_wash_in(surface, &medium);
                for scale in [0.25, 0.5, 1.0, 2.0] {
                    for hull_speed in [0.0, 16.0] {
                        let s = streaks(&def, name, &medium, scale, hull_speed);
                        let case = format!("{} {name} scale {scale} hull {hull_speed} m/s: {s:?}", def.name);
                        assert!(s.samples > 1000, "{case}: too few visible samples");
                        assert!(s.aspect <= MAX_SOFT_ASPECT, "{case}: a streak");
                        assert!(s.stretch <= MAX_SOFT_STRETCH, "{case}: a long smear");
                        worst.aspect = worst.aspect.max(s.aspect);
                        worst.stretch = worst.stretch.max(s.stretch);
                        worst.samples += s.samples;
                        worst.coverage += s.coverage;
                    }
                }
            }
        }
        eprintln!("{name} worst: {worst:?}");
        worst
    }

    /// The air wash's outflow sheet reads as a flat, soft layer of dust: no visible particle is
    /// drawn as a long thin streak. The sheet used to stretch at `velocity_scale 0.25`:
    /// 1.5-2.75 m quads about ten times longer than wide (DO27's live tour, 2026-10-10: "long
    /// smeared streaks", a "hairy ring"), and DO27 played its own dust effect instead.
    #[test]
    fn wash_sheet_has_no_streak() {
        assert_air_wash_emitter_has_no_streak("sheet");
    }

    /// The roll-up billows are soft puffs from birth: a fresh, fast billow drew up to about 12
    /// times longer than wide at `velocity_scale 0.12` (a 1.2 m streak on a small hull).
    #[test]
    fn wash_billow_has_no_streak() {
        assert_air_wash_emitter_has_no_streak("billow");
    }

    /// Height thins the wash: the same hull at its hover height and two radii up.
    #[test]
    fn a_higher_hull_spawns_fewer_particles() {
        let count = |height: f32| {
            let mut lib = crate::scene::VfxLibrary::new();
            lib.add(hover_wash(HoverSurface::Dust)).expect("add");
            let mut scene = VfxScene::new(lib);
            let id = scene
                .spawn("hover_wash_dust", Vec3::ZERO, Vec3::Y, 7)
                .expect("spawn");
            let lod = LodInput {
                camera_pos: Vec3::new(0.0, 3.0, 15.0),
            };
            let mut total = 0u32;
            for _ in 0..120 {
                if let Some(i) = scene.instance_mut(id) {
                    i.set_intensity(hover_intensity(height, 3.0));
                }
                scene.update(1.0 / 60.0, &lod, 100_000);
                total += scene.last_scheduled();
            }
            total
        };
        let low = count(0.6);
        let mid = count(3.0);
        let high = count(7.0);
        assert!(low > 100, "{low}");
        assert!(mid < low / 2, "{mid} vs {low}");
        assert_eq!(high, 0);
    }
}
