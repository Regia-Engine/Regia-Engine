// SPDX-License-Identifier: MPL-2.0
//! Dust and snow lifted by the camera body's wake behind fast, low passes (ADR-1502).
//!
//! The ground is cut into a world grid of `cell_m` cells, each with a hashed sample point
//! inside it. A cell lifts one puff on the frame the wake's front reaches its point
//! (`0 ≤ since_front < frame time`) with a peak gust at or over `lift_m_s`. So one pass lifts
//! each cell at most once, a slow pass lifts the cells it is close to, and a pass that is too
//! high or too slow lifts none. The default threshold is the lower end of measured threshold
//! wind speeds for loose, dry snow (about 4 to 11 m/s at 10 m height, Li and Pomeroy 1997). The
//! strongest cells win when more than `max_puffs_per_frame` qualify, with ties broken by cell
//! index. Seeds come from the spec seed, the cell and the path time (never an update count: a
//! headless host keeps updating at wall-clock pace between agent calls), so a re-render lifts the
//! same puffs.
//!
//! The host says where the ground is with [`SprayGround`] (the terrain's height; optionally
//! where it is snow). Without it nothing sprays.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_vfx::{VfxScene, VfxSystemDef};
use serde::{Deserialize, Serialize};

use super::CameraBody;

/// Effect name of a dust puff.
pub const DUST_EFFECT: &str = "camera_wake_dust";
/// Effect name of a snow puff.
pub const SNOW_EFFECT: &str = "camera_wake_snow";

/// What the ground is made of where the wake lifts it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpraySurface {
    /// Dry soil, dead grass litter: brown dust.
    #[default]
    Dust,
    /// Loose snow: white powder.
    Snow,
    /// Nothing loose (rock, water): no puff.
    None,
}

impl SpraySurface {
    /// The effect this surface lifts.
    #[must_use]
    pub fn effect(self) -> Option<&'static str> {
        match self {
            Self::Dust => Some(DUST_EFFECT),
            Self::Snow => Some(SNOW_EFFECT),
            Self::None => None,
        }
    }
}

/// How the wake lifts the ground.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpraySpec {
    /// Lift puffs at all.
    pub enabled: bool,
    /// The surface where the host's [`SprayGround`] does not say.
    pub surface: SpraySurface,
    /// Peak gust that lifts loose material, m/s.
    pub lift_m_s: f32,
    /// Ground cell size, metres (one puff per cell per pass).
    pub cell_m: f32,
    /// No spray while the body is higher than this above the ground under it, metres.
    pub max_height_m: f32,
    /// Most puffs lifted in one frame.
    pub max_puffs_per_frame: u32,
    /// Puff size multiplier.
    pub scale: f32,
    /// Seed of the cells' sample points and puffs.
    pub seed: u64,
}

impl Default for SpraySpec {
    fn default() -> Self {
        Self {
            enabled: true,
            surface: SpraySurface::Dust,
            lift_m_s: 4.0,
            cell_m: 4.0,
            max_height_m: 80.0,
            max_puffs_per_frame: 24,
            scale: 1.0,
            seed: 0x5eed_ca3e,
        }
    }
}

impl SpraySpec {
    /// Every value clamped into range (non-finite values take the default).
    #[must_use]
    pub fn clamped(self) -> Self {
        let d = Self::default();
        let fix =
            |v: f32, lo: f32, hi: f32, default: f32| if v.is_finite() { v.clamp(lo, hi) } else { default };
        Self {
            lift_m_s: fix(self.lift_m_s, 0.1, 100.0, d.lift_m_s),
            cell_m: fix(self.cell_m, 0.5, 64.0, d.cell_m),
            max_height_m: fix(self.max_height_m, 0.0, 10_000.0, d.max_height_m),
            max_puffs_per_frame: self.max_puffs_per_frame.min(512),
            scale: fix(self.scale, 0.05, 20.0, d.scale),
            ..self
        }
    }
}

/// Ground height at `(x, z)`, or `None` off the terrain.
pub type GroundHeightFn = dyn Fn(f32, f32) -> Option<f32> + Send + Sync;
/// Surface at `(x, z)`.
pub type GroundSurfaceFn = dyn Fn(f32, f32) -> SpraySurface + Send + Sync;

/// Where the ground is, for the spray (the host fills it from its terrain).
#[derive(Resource, Clone, Default)]
pub struct SprayGround {
    /// Ground height. `None`: no ground, no spray.
    pub height: Option<Arc<GroundHeightFn>>,
    /// Surface; `None` uses [`SpraySpec::surface`] everywhere.
    pub surface: Option<Arc<GroundSurfaceFn>>,
}

impl std::fmt::Debug for SprayGround {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SprayGround")
            .field("height", &self.height.is_some())
            .field("surface", &self.surface.is_some())
            .finish()
    }
}

/// One puff the wake lifts.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SprayPuff {
    /// Ground point, metres.
    pub position: Vec3,
    /// The peak gust there, m/s.
    pub gust_m_s: f32,
    /// Horizontal direction of the gust (unit, or zero).
    pub direction: Vec3,
    /// The surface lifted.
    pub surface: SpraySurface,
    /// Effect seed.
    pub seed: u64,
    /// Grid cell `(x, z)`.
    pub cell: [i32; 2],
}

/// SplitMix64 finalizer (Steele, Lea and Flood 2014): a well-mixed 64-bit hash.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn cell_hash(seed: u64, cx: i32, cz: i32) -> u64 {
    mix(seed ^ mix((u64::from(cx as u32) << 32) | u64::from(cz as u32)))
}

fn unit(h: u64) -> f32 {
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// The puffs lifted over a frame of `dt` seconds by `shape`'s wake of `body` (pure; the
/// system's work). `salt` only seeds the puffs (the system passes the path time's bits).
#[must_use]
pub fn plan_spray(
    spec: &SpraySpec,
    shape: &regia_core::math::wake::WakeShape,
    body: &regia_core::math::wake::WakeBody,
    dt: f32,
    salt: u64,
    height: &GroundHeightFn,
    surface: Option<&GroundSurfaceFn>,
) -> Vec<SprayPuff> {
    let speed = body.velocity.length();
    if !spec.enabled || dt <= 0.0 || shape.speed_ramp(speed) <= 0.0 || spec.max_puffs_per_frame == 0 {
        return Vec::new();
    }
    let Some(ground) = height(body.position.x, body.position.z) else {
        return Vec::new();
    };
    if body.position.y - ground > spec.max_height_m {
        return Vec::new();
    }
    // Cells the front can newly reach this frame: within the range off the axis and at most
    // `range · cot μ` (plus a frame of travel) behind the body.
    let sin_mu = shape.front_sin(speed);
    let cot_mu = (1.0 - sin_mu * sin_mu).max(0.0).sqrt() / sin_mu;
    let behind = shape.range_m * cot_mu + speed * dt + spec.cell_m;
    let reach = shape.range_m + spec.cell_m;
    let dir = body.velocity / speed;
    let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or_zero();
    let side = Vec3::new(-flat.z, 0.0, flat.x);
    let corners = [
        body.position + flat * spec.cell_m + side * reach,
        body.position + flat * spec.cell_m - side * reach,
        body.position - flat * behind + side * reach,
        body.position - flat * behind - side * reach,
    ];
    let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
    for c in corners {
        lo = lo.min(c);
        hi = hi.max(c);
    }
    let cell = spec.cell_m;
    let (x0, x1) = ((lo.x / cell).floor() as i32, (hi.x / cell).ceil() as i32);
    let (z0, z1) = ((lo.z / cell).floor() as i32, (hi.z / cell).ceil() as i32);
    let mut puffs = Vec::new();
    for cz in z0..=z1 {
        for cx in x0..=x1 {
            let h = cell_hash(spec.seed, cx, cz);
            let x = (cx as f32 + unit(h)) * cell;
            let z = (cz as f32 + unit(mix(h))) * cell;
            let Some(y) = height(x, z) else { continue };
            let point = Vec3::new(x, y + 0.3, z);
            let s = shape.sample(body, point);
            if !(0.0..dt).contains(&s.since_front_s) || s.peak_m_s < spec.lift_m_s {
                continue;
            }
            let kind = surface.map_or(spec.surface, |f| f(x, z));
            if kind == SpraySurface::None {
                continue;
            }
            puffs.push(SprayPuff {
                position: Vec3::new(x, y, z),
                gust_m_s: s.peak_m_s,
                direction: Vec3::new(s.gust.x, 0.0, s.gust.z).normalize_or_zero(),
                surface: kind,
                seed: mix(h ^ salt),
                cell: [cx, cz],
            });
        }
    }
    puffs.sort_by(|a, b| {
        b.gust_m_s
            .total_cmp(&a.gust_m_s)
            .then(a.cell[1].cmp(&b.cell[1]))
            .then(a.cell[0].cmp(&b.cell[0]))
    });
    puffs.truncate(spec.max_puffs_per_frame as usize);
    puffs
}

/// Lifts this frame's puffs into the [`VfxScene`] (registers the two effects on first use).
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_body_spray")]
pub(super) fn emit_spray(
    mut body: ResMut<CameraBody>,
    ground: Res<SprayGround>,
    scene: Option<ResMut<VfxScene>>,
) {
    let time = body.time;
    // The first frame after a reset has no frame before it: nothing has "just" arrived. A long
    // gap (a seek) is not one frame either.
    let dt = if body.spray_primed {
        ((time - body.spray_time) as f32).clamp(0.0, 0.1)
    } else {
        0.0
    };
    body.spray_time = time;
    body.spray_primed = body.current.is_some();
    body.report.spray_puffs = 0;
    let (Some(state), Some(height), Some(mut scene)) = (body.state(), ground.height.as_deref(), scene) else {
        return;
    };
    let spec = body.spec.spray;
    let puffs = plan_spray(
        &spec,
        &body.spec.wake,
        &state,
        dt,
        time.to_bits(),
        height,
        ground.surface.as_deref(),
    );
    if puffs.is_empty() {
        return;
    }
    let mut lifted = 0u32;
    for puff in &puffs {
        let Some(effect) = puff.surface.effect() else {
            continue;
        };
        if scene.library().index(effect).is_none() {
            let Some(def) = spray_effect(puff.surface) else {
                continue;
            };
            if scene.library_mut().upsert(def).is_err() {
                continue;
            }
        }
        if let Some(id) = scene.spawn(effect, puff.position, Vec3::Y, puff.seed) {
            if let Some(instance) = scene.instance_mut(id) {
                let over = (puff.gust_m_s / spec.lift_m_s).clamp(1.0, 3.0);
                instance.set_scale(spec.scale * (0.6 + 0.4 * over));
                instance.set_intensity((0.5 * over).min(1.5));
            }
            lifted += 1;
        }
    }
    body.report.spray_puffs = lifted;
    body.report.spray_total += u64::from(lifted);
}

/// The effect definition of a surface's puff (`None` for [`SpraySurface::None`]).
#[must_use]
pub fn spray_effect(surface: SpraySurface) -> Option<VfxSystemDef> {
    let text = match surface {
        SpraySurface::Dust => DUST_RON,
        SpraySurface::Snow => SNOW_RON,
        SpraySurface::None => return None,
    };
    VfxSystemDef::from_ron(text).ok()
}

/// A low brown puff: fines kicked up and spread flat by the gust, settling in a few seconds.
pub const DUST_RON: &str = r##"(
    version: 5,
    name: "camera_wake_dust",
    system: (budget: 2400),
    emitters: [
        (
            name: "puff",
            delay: 0.0,
            spawn: (mode: Burst(5), lifetime: (min: 1.8, max: 3.4)),
            init: (
                shape: Sphere(radius: (min: 0.2, max: 1.4)),
                velocity: (mode: Planar(speed: (min: 1.0, max: 4.0), lift: (min: 0.6, max: 2.2))),
                size: (min: 0.5, max: 1.2),
                rotation: (min: 0.0, max: 6.28),
                spin: (min: -0.4, max: 0.4),
                color: [(t: 0.0, color: "#8a7b68")],
            ),
            update: (
                gravity: (0.0, -0.6, 0.0),
                drag: 1.6,
                size_over_life: [(t: 0.0, v: 0.6), (t: 1.0, v: 3.0)],
                alpha_over_life: [(t: 0.0, v: 0.0), (t: 0.08, v: 0.45), (t: 1.0, v: 0.0)],
            ),
            render: (mode: Billboard, sprite: Puff, blend: Alpha, softness: 1.0, lit: true),
        ),
    ],
)"##;

/// A bright snow puff: powder lifted higher and lighter than dust, sparkling as it falls.
pub const SNOW_RON: &str = r##"(
    version: 5,
    name: "camera_wake_snow",
    system: (budget: 2400),
    emitters: [
        (
            name: "powder",
            delay: 0.0,
            spawn: (mode: Burst(6), lifetime: (min: 1.6, max: 3.0)),
            init: (
                shape: Sphere(radius: (min: 0.2, max: 1.2)),
                velocity: (mode: Planar(speed: (min: 1.2, max: 4.5), lift: (min: 1.0, max: 3.0))),
                size: (min: 0.4, max: 1.0),
                rotation: (min: 0.0, max: 6.28),
                spin: (min: -0.4, max: 0.4),
                color: [(t: 0.0, color: "#eef2f7")],
            ),
            update: (
                gravity: (0.0, -1.2, 0.0),
                drag: 1.4,
                size_over_life: [(t: 0.0, v: 0.5), (t: 1.0, v: 2.4)],
                alpha_over_life: [(t: 0.0, v: 0.0), (t: 0.06, v: 0.55), (t: 1.0, v: 0.0)],
            ),
            render: (mode: Billboard, sprite: Puff, blend: Alpha, softness: 1.0, lit: true),
        ),
    ],
)"##;
