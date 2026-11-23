// SPDX-License-Identifier: MPL-2.0
//! Atmospheric entry effect: a bolide streak along a tracked entity, deterministic.
//!
//! A body that meets the air at several km/s heats the air in front of it; the heating rate
//! scales with air density times speed cubed (the stagnation-point scaling of Allen & Eggers
//! 1958 and Sutton & Graves 1971). This module turns that into what a camera sees:
//!
//! - a **head** whose brightness follows `rho(h) * v^3`: a round, soft, warm-white blob that turns
//!   yellow, then orange, as the body slows ([`glow_color`]);
//! - an **ionised wake** behind the head that cools and dims within about a second and tapers;
//!   it fades out rather than turning deep red, and a short afterglow in the train is gone a few
//!   seconds after the head ([`EMBER_LIFE_S`]);
//! - a **dust/smoke train** deposited where the body ablates (and a faint dark one from
//!   on-board damage below the ceiling): pale grey-brown, thrown out to hundreds of metres by the
//!   hot wake within a second, then widening by diffusion, thinning, billowing, drifting with the
//!   wind and lasting minutes;
//! - **shed pieces**: ablation debris that breaks off over the glowing flight and, having more
//!   area per mass than the body, decelerates faster and falls back behind the head along the
//!   flight path (with only a small sideways drift); each glows from its own heating, dimmer than
//!   the head, then cools and fades within a few seconds, and is drawn as a short streak along the
//!   path (one exposure of motion plus its cooling vapour), gone before the head goes dark;
//! - a **dark body**: at or below [`GLOW_CEILING_M`] (20 km) nothing glows, whatever the speed
//!   (the track is a thin dark streak with at most a faint train).
//!
//! Everything is a pure function of the [`EntryTrack`], the [`EntryProfile`] and the time, so two
//! evaluations of the same frame agree bit for bit. Build the camera-facing ribbons for the
//! existing trail pipeline with [`build_vertices`]; hosts push them into `VfxTrails`. The glowing
//! ribbons are soft (Gaussian across) and additive, the dust is soft and hides the sky behind it
//! (the [`TrailVertex`] flag slots).
//!
//! ## Tiers
//!
//! [`EntryTier::Full`] (Desktop High and up) evaluates the trail, the train at a fine step and
//! the shed pieces. [`EntryTier::Cheap`] (Low, Deck, Mobile) keeps the head, a short ion trail and
//! a coarse train and drops the pieces: a few dozen ribbon segments instead of a thousand.

use regia_core::math::{Vec3, det_exp, det_sin_cos};

use crate::gpu::TrailVertex;

/// At or below this altitude (metres) no part of the effect glows.
pub const GLOW_CEILING_M: f32 = 20_000.0;
/// The glow gate opens fully this far above the ceiling.
const GLOW_RAMP_M: f32 = 6_000.0;
/// Below this speed (m/s) the air no longer heats the body enough to ablate; fully open at twice it.
pub const ABLATION_SPEED_MPS: f32 = 2_000.0;
/// Speed of sound used for the sonic-boom delay helper (m/s).
pub const SPEED_OF_SOUND_MPS: f32 = 343.0;
/// Exponential-atmosphere scale height (metres), a fit to the lower atmosphere.
const SCALE_HEIGHT_M: f32 = 7_200.0;
/// Sea-level air density (kg/m^3).
const SEA_LEVEL_DENSITY: f32 = 1.225;
/// Heating reference: density at 40 km times (11 km/s)^3.
const HEATING_REFERENCE: f32 = 0.004_79 * 1.331e12;

/// Why a track could not be built.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EntryError {
    /// Fewer than two points.
    #[error("an entry track needs at least two points")]
    TooFewPoints,
    /// Times must strictly increase.
    #[error("track times must strictly increase (point {0})")]
    NotIncreasing(usize),
    /// A time or coordinate was NaN or infinite.
    #[error("track point {0} is not finite")]
    NonFinite(usize),
}

/// One timed position on a track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackPoint {
    /// Seconds from the start of the shot.
    pub time: f32,
    /// World position (metres, +Y up).
    pub position: Vec3,
}

/// A piecewise-linear timed path for the entering body.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryTrack {
    points: Vec<TrackPoint>,
}

impl EntryTrack {
    /// A track through `points`.
    pub fn new(points: Vec<TrackPoint>) -> Result<Self, EntryError> {
        if points.len() < 2 {
            return Err(EntryError::TooFewPoints);
        }
        for (i, p) in points.iter().enumerate() {
            if !p.time.is_finite() || !p.position.is_finite() {
                return Err(EntryError::NonFinite(i));
            }
            if i > 0 && p.time <= points[i - 1].time {
                return Err(EntryError::NotIncreasing(i));
            }
        }
        Ok(Self { points })
    }

    /// The track points.
    pub fn points(&self) -> &[TrackPoint] {
        &self.points
    }

    /// Time of the first point.
    pub fn start_time(&self) -> f32 {
        self.points[0].time
    }

    /// Time of the last point.
    pub fn end_time(&self) -> f32 {
        self.points[self.points.len() - 1].time
    }

    fn segment(&self, time: f32) -> usize {
        let last = self.points.len() - 2;
        self.points
            .partition_point(|p| p.time <= time)
            .saturating_sub(1)
            .min(last)
    }

    /// Position at `time` (held at the ends; a NaN time reads as the start).
    pub fn position_at(&self, time: f32) -> Vec3 {
        // A NaN time is held at the start, as `velocity_at` does. Regression:
        // tests/tqs_sweep.rs::entry_track_position_is_finite_for_hostile_times.
        let time = if time.is_nan() { self.start_time() } else { time };
        let t = time.clamp(self.start_time(), self.end_time());
        let i = self.segment(t);
        let (a, b) = (self.points[i], self.points[i + 1]);
        let f = (t - a.time) / (b.time - a.time);
        a.position.lerp(b.position, f.clamp(0.0, 1.0))
    }

    /// Velocity (m/s) of the segment containing `time` (the end segment's after the end).
    pub fn velocity_at(&self, time: f32) -> Vec3 {
        let i = self.segment(time.clamp(self.start_time(), self.end_time()));
        let (a, b) = (self.points[i], self.points[i + 1]);
        (b.position - a.position) / (b.time - a.time)
    }

    /// First time the track is at or below `altitude_m` above `ground_y`, if it ever is.
    pub fn first_time_below(&self, ground_y: f32, altitude_m: f32) -> Option<f32> {
        let level = ground_y + altitude_m;
        if self.points[0].position.y <= level {
            return Some(self.start_time());
        }
        self.points.windows(2).find_map(|w| {
            let (ya, yb) = (w[0].position.y, w[1].position.y);
            if yb <= level {
                let f = (ya - level) / (ya - yb);
                Some(w[0].time + f * (w[1].time - w[0].time))
            } else {
                None
            }
        })
    }
}

/// Which cost level to evaluate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntryTier {
    /// Head, ion trail, fine train, shed pieces.
    #[default]
    Full,
    /// Head, short ion trail, coarse train; no pieces. For Low, Deck and Mobile.
    Cheap,
}

/// Look and physics knobs. The defaults describe a 12 to 14 m body entering at about 11 km/s.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryProfile {
    /// World Y of the ground the altitude is measured from.
    pub ground_y: f32,
    /// Seed for the shed pieces and the train jitter.
    pub seed: u64,
    /// Head radiance at full heating, display-referred like every other authored VFX colour (above
    /// 1 blooms and saturates under the tone mapper). The
    /// host calibrates this against its exposure.
    pub peak_radiance: f32,
    /// Radius of the luminous head (metres).
    pub head_radius_m: f32,
    /// Radius of the dark body (metres), used below the glow ceiling.
    pub body_radius_m: f32,
    /// Seconds the ionised trail stays visible.
    pub ion_life_s: f32,
    /// Seconds the dark-flight damage smoke lasts. Glowing dust fades on [`Self::dust_life_s`],
    /// not on this. A sample older than the longer of the two lives is not emitted.
    pub train_life_s: f32,
    /// Seconds the dust (not the dark-flight damage smoke, which uses `train_life_s`) takes to fade
    /// out after it is deposited. [`DUST_LIFE_LEGACY_S`] is the pre-look-5 value.
    pub dust_life_s: f32,
    /// Scale on the dust train's opacity. [`DUST_OPACITY_SCALE`] by default, 1.0 is the pre-look-5
    /// value. The glowing head, wake and pieces are not affected.
    pub dust_opacity_scale: f32,
    /// Lit colour (albedo tint) of the dust train. [`DUST_COLOR`] by default (a warm grey),
    /// [`DUST_COLOR_LEGACY`] is the pre-look-5 pale grey-brown.
    pub dust_albedo: [f32; 3],
    /// Wind (m/s) the train drifts with.
    pub wind: Vec3,
    /// Altitude (m) at which pieces shed.
    pub shed_altitude_m: f32,
    /// Number of shed pieces ([`EntryTier::Full`] only).
    pub shed_count: u32,
    /// Opacity of the faint smoke from on-board damage below the glow ceiling (0 = none).
    pub damage_smoke: f32,
}

impl Default for EntryProfile {
    fn default() -> Self {
        Self {
            ground_y: 0.0,
            seed: 0x5eed_0e47,
            peak_radiance: 30.0,
            head_radius_m: 14.0,
            body_radius_m: 6.5,
            ion_life_s: 1.5,
            train_life_s: 240.0,
            dust_life_s: DUST_LIFE_S,
            dust_opacity_scale: DUST_OPACITY_SCALE,
            dust_albedo: DUST_COLOR,
            wind: Vec3::new(25.0, 0.0, 10.0),
            shed_altitude_m: 45_000.0,
            shed_count: 10,
            damage_smoke: 0.12,
        }
    }
}

impl EntryProfile {
    /// The default profile with the dust train as it was before look round 5: full opacity, the pale
    /// grey-brown [`DUST_COLOR_LEGACY`], and a 240 s life. The head, wake and pieces are the same in
    /// both.
    pub fn legacy_dust() -> Self {
        Self {
            dust_life_s: DUST_LIFE_LEGACY_S,
            dust_opacity_scale: 1.0,
            dust_albedo: DUST_COLOR_LEGACY,
            ..Self::default()
        }
    }
}

/// Camera facts the ribbons need.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntryView {
    /// Camera position.
    pub camera: Vec3,
    /// Focal length in pixels (`height_px / (2 tan(fov_y / 2))`).
    pub focal_px: f32,
    /// Smallest apparent width of the head and the dark body (pixels).
    pub min_px: f32,
    /// Exposure time (s); the dark body is drawn as a streak of `speed * shutter_s`.
    pub shutter_s: f32,
    /// The camera's far plane (m). Points beyond [`RANGE_FIT`] of it are pulled along their view ray
    /// to that distance, and their widths shrink by the same factor, so the picture is unchanged
    /// but the ribbon is not clipped. A bolide 190 km away is far beyond any scene far plane.
    /// `f32::INFINITY` (or any non-positive or non-finite value) turns the fit off.
    pub far_m: f32,
}

/// Fraction of the far plane a far point is pulled to (see [`EntryView::far_m`]).
pub const RANGE_FIT: f32 = 0.9;

/// The luminous (or dark) body.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntryHead {
    /// World position.
    pub position: Vec3,
    /// Unit direction of travel.
    pub direction: Vec3,
    /// Speed (m/s).
    pub speed: f32,
    /// Altitude above the ground (m).
    pub altitude_m: f32,
    /// Glow 0..1; exactly 0 at or below [`GLOW_CEILING_M`].
    pub glow: f32,
    /// Linear RGB hue: [`glow_color`] at [`head_temperature_k`] (warm white when fast, yellow, then
    /// orange as the body slows).
    pub color: [f32; 3],
    /// Scene-linear radiance (`glow * peak_radiance`).
    pub radiance: f32,
}

/// One sample of the ionised trail.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IonPoint {
    /// World position.
    pub position: Vec3,
    /// Seconds since the body passed.
    pub age_s: f32,
    /// Brightness 0..1 (the head's glow then, decayed).
    pub brightness: f32,
    /// Linear RGB from [`glow_color`] at `temperature_k`: warm white at the head, yellow, then
    /// orange as the gas cools (it fades out rather than turning deep red).
    pub color: [f32; 3],
    /// Gas temperature (K), cooling with age ([`cooled_temperature_k`]).
    pub temperature_k: f32,
    /// Half-width (m) of the glow envelope; it widens with age as the shock-heated gas expands.
    pub half_width_m: f32,
}

/// One sample of the dust/smoke train.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrainPoint {
    /// World position, drifted by the wind.
    pub position: Vec3,
    /// Seconds since deposited.
    pub age_s: f32,
    /// Opacity 0..1.
    pub opacity: f32,
    /// Lit colour (the train reflects sunlight; it never emits).
    pub color: [f32; 3],
    /// Half-width (m): thrown out by the hot wake within about a second, then growing with the
    /// square root of age; it billows along the train.
    pub half_width_m: f32,
    /// Glow of the still-hot train as a fraction of the head's peak radiance (0 once it has cooled
    /// and for every dark-flight point); gusting with turbulence.
    pub ember: f32,
    /// Linear RGB of the glowing train ([`glow_color`] at its cooled temperature).
    pub ember_color: [f32; 3],
}

/// One shed piece (only while it still glows): a fragment of ablation debris that broke off the
/// body, decelerates faster than it (more area per mass) and so falls back along the flight path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShedPiece {
    /// World position: on the flight path behind the head, plus a small sideways drift.
    pub position: Vec3,
    /// Size (m).
    pub size_m: f32,
    /// Glow 0..1, from the piece's own heating; at most [`PIECE_MAX_SHARE`] of the head's.
    pub glow: f32,
    /// Linear RGB hue ([`glow_color`] at `temperature_k`).
    pub color: [f32; 3],
    /// Speed along the flight path (m/s).
    pub speed: f32,
    /// World velocity (m/s): along the path at `speed`, plus the sideways drift. The view smears the
    /// piece along this over one exposure, and its cooling vapour trails behind it along the same
    /// line, so it reads as a short streak along the path, not a dot.
    pub velocity: Vec3,
    /// Seconds since the piece broke off.
    pub age_s: f32,
    /// Seconds the piece glows in all ([`PIECE_LIFE_MIN_S`] to [`PIECE_LIFE_MAX_S`]).
    pub life_s: f32,
    /// Surface temperature (K): set by its own heating, cooling over its life.
    pub temperature_k: f32,
}

/// Everything the effect shows at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryFrame {
    /// The time evaluated (s).
    pub time_s: f32,
    /// The body.
    pub head: EntryHead,
    /// Ionised trail, newest first.
    pub ion: Vec<IonPoint>,
    /// Train, oldest first.
    pub train: Vec<TrainPoint>,
    /// Glowing shed pieces.
    pub pieces: Vec<ShedPiece>,
}

fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    // A NaN input reads as the low end (0) instead of poisoning every colour and temperature
    // built on it. Regression: tests/tqs_sweep.rs::entry_colors_stay_in_the_unit_cube_for_any_input.
    if x.is_nan() {
        return 0.0;
    }
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Exponential-atmosphere air density (kg/m^3) at `altitude_m`.
pub fn air_density(altitude_m: f32) -> f32 {
    // det_exp: the density feeds the piece push and the glow cut, so it must not follow the
    // platform libm (fix-vfx1-determinism finding 5).
    SEA_LEVEL_DENSITY * det_exp(-altitude_m.max(0.0) / SCALE_HEIGHT_M)
}

/// Relative stagnation heating, `rho v^3` over its value at 40 km and 11 km/s.
pub fn heating_flux(altitude_m: f32, speed_mps: f32) -> f32 {
    air_density(altitude_m) * speed_mps.max(0.0).powi(3) / HEATING_REFERENCE
}

/// Glow 0..1 for a body at `altitude_m` moving at `speed_mps`.
///
/// Saturating in the heating flux, gated by an ablation speed and by the 20 km ceiling: exactly
/// 0 at or below [`GLOW_CEILING_M`] and below the ablation speed.
pub fn glow_factor(altitude_m: f32, speed_mps: f32) -> f32 {
    if altitude_m.is_nan() || altitude_m <= GLOW_CEILING_M || !speed_mps.is_finite() {
        return 0.0;
    }
    let q = heating_flux(altitude_m, speed_mps);
    let heat = q / (q + 0.05);
    let speed_gate = smoothstep(ABLATION_SPEED_MPS, 2.0 * ABLATION_SPEED_MPS, speed_mps);
    let ceiling_gate = smoothstep(GLOW_CEILING_M, GLOW_CEILING_M + GLOW_RAMP_M, altitude_m);
    heat * speed_gate * ceiling_gate
}

/// Linear RGB of the heated gas by speed: white at 9 km/s and above, orange near 5.5 km/s, dull
/// red at 2.5 km/s and below.
pub fn heat_color(speed_mps: f32) -> [f32; 3] {
    const WHITE: [f32; 3] = [1.0, 0.96, 0.9];
    const ORANGE: [f32; 3] = [1.0, 0.5, 0.14];
    const RED: [f32; 3] = [0.45, 0.07, 0.03];
    if speed_mps >= 5_500.0 {
        lerp3(ORANGE, WHITE, smoothstep(5_500.0, 9_000.0, speed_mps))
    } else {
        lerp3(RED, ORANGE, smoothstep(2_500.0, 5_500.0, speed_mps))
    }
}

/// Ambient temperature (K) the ionised gas cools toward; at this temperature it no longer shows.
pub const AMBIENT_K: f32 = 1_300.0;
/// Seconds the shock-heated gas takes to lose `1 - 1/e` of its excess temperature.
pub const COOLING_TAU_S: f32 = 0.45;

/// Peak radiating temperature (K) of the gas around a body at `speed_mps`: about 3000 K at and
/// below 3 km/s, rising smoothly to about 9000 K (blue-white) at 11 km/s. Never decreases with speed.
pub fn head_temperature_k(speed_mps: f32) -> f32 {
    3_000.0 + 6_000.0 * smoothstep(3_000.0, 11_000.0, speed_mps)
}

/// Temperature (K) of gas that left the head at `start_k` and has cooled for `age_s`: exponential
/// toward [`AMBIENT_K`] with time constant `tau_s`. Never increases with age.
pub fn cooled_temperature_k(start_k: f32, age_s: f32, tau_s: f32) -> f32 {
    let excess = (start_k - AMBIENT_K).max(0.0);
    AMBIENT_K + excess * det_exp(-age_s.max(0.0) / tau_s.max(1e-3))
}

/// Linear RGB of an incandescent body at `temperature_k`, with the ablation lines folded in.
///
/// The base ramp follows a blackbody from deep red (1500 K) through orange, yellow-white and white
/// to blue-white (10 000 K). The line emission of ablating meteoroid metal tints it: sodium
/// (589 nm) pushes the middle of the ramp toward orange (less green), and above about 7000 K magnesium
/// (517 nm) and ionised iron add a faint blue-green. Channels are in `0..=1`, with the red
/// channel dominant at the cool end and blue at the hot end.
pub fn temperature_color(temperature_k: f32) -> [f32; 3] {
    const STOPS: [(f32, [f32; 3]); 7] = [
        (1_500.0, [0.30, 0.03, 0.01]),
        (2_500.0, [0.85, 0.22, 0.04]),
        (3_500.0, [1.0, 0.48, 0.13]),
        (4_500.0, [1.0, 0.70, 0.36]),
        (6_000.0, [1.0, 0.93, 0.85]),
        (8_000.0, [0.84, 0.90, 1.0]),
        (10_000.0, [0.70, 0.81, 1.0]),
    ];
    // NaN reads as the coolest stop. Regression:
    // tests/tqs_sweep.rs::entry_colors_stay_in_the_unit_cube_for_any_input.
    let t = if temperature_k.is_nan() {
        STOPS[0].0
    } else {
        temperature_k.clamp(STOPS[0].0, STOPS[STOPS.len() - 1].0)
    };
    let i = STOPS
        .iter()
        .rposition(|(k, _)| *k <= t)
        .unwrap_or(0)
        .min(STOPS.len() - 2);
    let (a, b) = (STOPS[i], STOPS[i + 1]);
    let base = lerp3(a.1, b.1, (t - a.0) / (b.0 - a.0));
    // Sodium D lines: a bump centred on 4000 K that pulls green down, toward orange.
    let na = 1.0 - ((t - 4_000.0) / 1_800.0).abs().min(1.0);
    // Magnesium b lines and Fe II: a ramp above 7000 K that lifts green over red.
    let mg = smoothstep(7_000.0, 10_000.0, t);
    [
        (base[0] * (1.0 - 0.10 * mg)).clamp(0.0, 1.0),
        (base[1] * (1.0 - 0.10 * na) * (1.0 + 0.05 * mg)).clamp(0.0, 1.0),
        base[2].clamp(0.0, 1.0),
    ]
}

/// Hottest colour temperature (K) the streak shows. A fireball's light is mostly a 4000–5000 K
/// vapour, with a weaker component near 10 000 K (the two-temperature fit to meteor spectra,
/// Borovička 1993), and a daylight-adapted eye takes about 6000 K as white: the head reads warm
/// white, never blue, against a day sky.
pub const VISIBLE_MAX_K: f32 = 6_000.0;
/// Coolest colour temperature (K) the glowing wake shows. Cooler gas fades out in brightness
/// instead of turning deep red, which reads pink or as a laser line over a blue sky.
pub const VISIBLE_MIN_K: f32 = 3_500.0;

/// Colour the glow shows for gas at `temperature_k`: [`temperature_color`] with the temperature
/// held to [`VISIBLE_MIN_K`]`..=`[`VISIBLE_MAX_K`] (warm white, yellow, then orange; never blue,
/// never deep red). Green stays at or above 0.42 of red.
pub fn glow_color(temperature_k: f32) -> [f32; 3] {
    let kelvin = if temperature_k.is_nan() {
        VISIBLE_MIN_K
    } else {
        temperature_k.clamp(VISIBLE_MIN_K, VISIBLE_MAX_K)
    };
    temperature_color(kelvin)
}

/// Smooth value noise in `0..=1`, continuous in `x`, deterministic in `seed`.
fn value_noise(seed: u64, stream: u64, x: f32) -> f32 {
    let cell = x.floor();
    let f = x - cell;
    let i = cell as i64 as u64;
    let (a, b) = (hash01(seed, i, stream), hash01(seed, i.wrapping_add(1), stream));
    let u = f * f * (3.0 - 2.0 * f);
    a + (b - a) * u
}

/// Seconds a sonic boom from altitude `distance_m` takes to reach the ground at the speed of sound.
pub fn boom_delay_s(distance_m: f32) -> f32 {
    distance_m.max(0.0) / SPEED_OF_SOUND_MPS
}

/// Pixels a `size_m` object covers at `range_m` for a camera of `focal_px`.
pub fn apparent_size_px(size_m: f32, range_m: f32, focal_px: f32) -> f32 {
    if range_m <= 0.0 {
        return 0.0;
    }
    size_m * focal_px / range_m
}

/// SplitMix64 finaliser, 0..1.
fn hash01(seed: u64, a: u64, b: u64) -> f32 {
    let mut z = seed
        .wrapping_add(a.wrapping_mul(0x9e37_79b9_7f4a_7c15))
        .wrapping_add(b.wrapping_mul(0xbf58_476d_1ce4_e5b9));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    (z >> 40) as f32 / (1u64 << 24) as f32
}

fn state_at(track: &EntryTrack, profile: &EntryProfile, time: f32) -> (Vec3, Vec3, f32, f32, f32) {
    let position = track.position_at(time);
    let velocity = track.velocity_at(time);
    let speed = velocity.length();
    let altitude = position.y - profile.ground_y;
    (position, velocity, speed, altitude, glow_factor(altitude, speed))
}

/// Seconds the ablated gas keeps a faint glow behind the ionised trail. Short: in daylight the
/// wake's afterglow is lost against the sky within a second or two (night-time persistent trains
/// glow for minutes, but far too faintly to show by day).
pub const EMBER_LIFE_S: f32 = 3.0;
/// Cooling time constant (s) of the glowing train; slower than the ionised trail's because the
/// gas is mixed into a larger volume.
const EMBER_TAU_S: f32 = 0.8;
/// Time constant (s) of the glowing train's brightness.
const EMBER_FADE_S: f32 = 0.6;
/// Glow of a fresh train point as a fraction of the head's peak radiance.
const EMBER_PEAK: f32 = 0.02;
/// Opacity of a fresh, narrow dust train at full glow.
const DUST_OPACITY: f32 = 0.6;
/// Half-width (m) below which the dust train keeps its full opacity; wider, it thins.
const DUST_THIN_REF_M: f32 = 120.0;
/// Exponent of the thinning, `(DUST_THIN_REF_M / half_width)^k`: weaker than the `1/width` of a
/// conserved column because the column keeps a dense core while its faint edges spread.
const DUST_THIN_EXP: f32 = 0.35;
/// Share of the cylindrical blast radius (Mach number times body diameter) the hot wake throws the
/// dust out to in its first seconds.
const DUST_EXPANSION: f32 = 0.3;
/// Time constant (s) of that first expansion.
const DUST_EXPANSION_TAU_S: f32 = 1.0;
/// Turbulent diffusion of the train: half-width growth in metres per square-root second.
const DUST_SPREAD: f32 = 40.0;
/// Seconds over which the dust emerges from behind the glowing wake.
const DUST_EMERGE_S: f32 = 0.6;
/// Lit colour of the dust and smoke train: a pale grey-brown (display-referred like every other
/// authored VFX colour).
pub const DUST_COLOR_LEGACY: [f32; 3] = [0.68, 0.62, 0.54];
/// Default lit colour of the dust train (look round 5): a warm grey at about half the old albedo,
/// not white. Override per profile with [`EntryProfile::dust_albedo`].
pub const DUST_COLOR: [f32; 3] = [0.36, 0.33, 0.29];
/// Default scale on the dust opacity (look round 5: about half). 1.0 is the old look.
pub const DUST_OPACITY_SCALE: f32 = 0.5;
/// Default seconds the dust takes to fade out (look round 5).
pub const DUST_LIFE_S: f32 = 24.0;
/// The dust life before look round 5 (seconds).
pub const DUST_LIFE_LEGACY_S: f32 = 240.0;

/// Evaluates the effect at `time_s`.
pub fn evaluate(track: &EntryTrack, profile: &EntryProfile, tier: EntryTier, time_s: f32) -> EntryFrame {
    let mut ion = Vec::new();
    let mut train = Vec::new();
    let mut pieces = Vec::new();
    let head = fill_entry(track, profile, tier, time_s, &mut ion, &mut train, &mut pieces);
    EntryFrame {
        time_s,
        head,
        ion,
        train,
        pieces,
    }
}

/// Fills `ion`, `train` and `pieces` for `time_s` and returns the head.
///
/// The lists are cleared first. Callers that keep them across frames allocate only when a list
/// grows past the capacity it already has.
fn fill_entry(
    track: &EntryTrack,
    profile: &EntryProfile,
    tier: EntryTier,
    time_s: f32,
    ion: &mut Vec<IonPoint>,
    train: &mut Vec<TrainPoint>,
    pieces: &mut Vec<ShedPiece>,
) -> EntryHead {
    let _span = tracing::trace_span!("entry_evaluate").entered();
    let (position, velocity, speed, altitude, glow) = state_at(track, profile, time_s);
    let head = EntryHead {
        position,
        direction: velocity.normalize_or_zero(),
        speed,
        altitude_m: altitude,
        glow,
        color: glow_color(head_temperature_k(speed)),
        radiance: glow * profile.peak_radiance,
    };
    ion_trail_into(track, profile, tier, time_s, ion);
    train_into(track, profile, tier, time_s, train);
    if tier == EntryTier::Full {
        shed_pieces_into(track, profile, time_s, pieces);
    } else {
        pieces.clear();
    }
    head
}

/// Brightness 0..1 of gas `age_s` after it left the head, as a fraction of the head's glow: a steep
/// fall over the first tenth of the trail's life, then a slower tail (two exponentials) that has
/// dropped under a twentieth of the head by two thirds of the life, so the visible wake is short.
fn ion_decay(age_s: f32, life_s: f32) -> f32 {
    0.75 * (-age_s / (0.06 * life_s)).exp() + 0.25 * (-age_s / (0.22 * life_s)).exp()
}

/// Turbulent brightness gust (0.7..1.3) at track time `ts`, continuous and stable in time.
fn gust(seed: u64, stream: u64, ts: f32, rate: f32) -> f32 {
    0.7 + 0.6 * value_noise(seed, stream, ts * rate)
}

fn ion_trail_into(
    track: &EntryTrack,
    profile: &EntryProfile,
    tier: EntryTier,
    time_s: f32,
    out: &mut Vec<IonPoint>,
) {
    out.clear();
    let samples = match tier {
        EntryTier::Full => 24,
        EntryTier::Cheap => 6,
    };
    let life = profile.ion_life_s.max(1e-3);
    for i in 1..=samples {
        // Quadratic spacing: dense next to the head where the gradient is steepest.
        let u = i as f32 / samples as f32;
        let age = life * u * u;
        let ts = time_s - age;
        if ts < track.start_time() || ts > track.end_time() {
            continue;
        }
        let (position, _, speed, _, glow) = state_at(track, profile, ts);
        let brightness = glow * ion_decay(age, life) * gust(profile.seed, 21, ts, 9.0);
        if brightness < 0.002 {
            continue;
        }
        let temperature_k = cooled_temperature_k(head_temperature_k(speed), age, COOLING_TAU_S);
        out.push(IonPoint {
            position,
            age_s: age,
            brightness,
            color: glow_color(temperature_k),
            temperature_k,
            half_width_m: profile.head_radius_m * (0.3 + 0.9 * u) * gust(profile.seed, 22, ts, 6.0),
        });
    }
}

/// Half-width (m) of the dust train `age_s` after a body of `body_radius_m` passed at `speed_mps`,
/// before billowing: 15 m, plus the throw of the hot wake (a share of the cylindrical blast
/// radius, Mach number times body diameter) reached within about a second, plus turbulent
/// diffusion growing with the square root of age. Never shrinks with age.
fn dust_half_width(speed_mps: f32, age_s: f32, body_radius_m: f32) -> f32 {
    let mach = speed_mps.max(0.0) / SPEED_OF_SOUND_MPS;
    let thrown = DUST_EXPANSION * mach * 2.0 * body_radius_m.max(0.0);
    let age = age_s.max(0.0);
    15.0 + thrown * (1.0 - (-age / DUST_EXPANSION_TAU_S).exp()) + DUST_SPREAD * age.sqrt()
}

fn train_into(
    track: &EntryTrack,
    profile: &EntryProfile,
    tier: EntryTier,
    time_s: f32,
    out: &mut Vec<TrainPoint>,
) {
    out.clear();
    let step = match tier {
        EntryTier::Full => 0.25,
        EntryTier::Cheap => 1.0,
    };
    let life = profile.train_life_s.max(step);
    let dust_life = profile.dust_life_s.max(step);
    // Points older than the longer of the two lives are gone either way.
    let window = life.max(dust_life);
    let first = ((time_s - window) / step).ceil() as i64;
    let last = (time_s.min(track.end_time()) / step).floor() as i64;
    for k in first..=last {
        let ts = k as f32 * step;
        if ts < track.start_time() {
            continue;
        }
        let (position, _, speed, altitude, glow) = state_at(track, profile, ts);
        let age = (time_s - ts).max(0.0);
        let fade = (1.0 - age / life).clamp(0.0, 1.0);
        let dust_fade = (1.0 - age / dust_life).clamp(0.0, 1.0);
        let (opacity, color, half_width) = if glow > 0.01 {
            // The hot wake throws the dust out to a fraction of the cylindrical blast radius
            // (Mach number times the body's diameter) within a second or so; then turbulence
            // spreads it with the square root of age. It billows: the width wanders along the train.
            let billow = 0.7 + 0.6 * value_noise(profile.seed, 33, ts * 0.7);
            let half_width = dust_half_width(speed, age, profile.body_radius_m) * billow;
            // Patchy along the train (two scales), thinning as it spreads, emerging from behind the
            // glowing wake and fading out over its life.
            let patchy = 0.6
                + 0.25 * value_noise(profile.seed, 31, ts * 0.8)
                + 0.15 * value_noise(profile.seed, 34, ts * 3.1);
            let thin = (DUST_THIN_REF_M / half_width).min(1.0).powf(DUST_THIN_EXP);
            let emerge = smoothstep(0.0, DUST_EMERGE_S, age);
            (
                DUST_OPACITY
                    * profile.dust_opacity_scale.max(0.0)
                    * (glow * 2.0).min(1.0)
                    * patchy
                    * thin
                    * emerge
                    * dust_fade,
                profile.dust_albedo,
                half_width,
            )
        } else if altitude <= GLOW_CEILING_M && profile.damage_smoke > 0.0 {
            // The dark ribbon is a faint stain. Keep one sample per four tier steps (1 s at
            // Full, 4 s at Cheap). The glowing wake above the ceiling still uses `step`.
            let coarse = step * 4.0;
            let nearest = (ts / coarse).round();
            if (ts - nearest * coarse).abs() > step * 0.25 {
                continue;
            }
            (
                profile.damage_smoke * fade * fade.sqrt(),
                [0.2, 0.19, 0.19],
                2.0 + 3.0 * age.sqrt(),
            )
        } else {
            continue;
        };
        if opacity <= 0.0 {
            continue;
        }
        // The train meanders smoothly (value noise along the track time), not point by point.
        let meander = Vec3::new(
            value_noise(profile.seed, 1, ts * 0.35) - 0.5,
            value_noise(profile.seed, 2, ts * 0.35) - 0.5,
            value_noise(profile.seed, 3, ts * 0.35) - 0.5,
        ) * (0.6 * half_width);
        // A short afterglow while the ablated gas is still hot: it cools and fades within a few
        // seconds and gusts with turbulence.
        let (ember, ember_color) = if glow > 0.01 && age < EMBER_LIFE_S {
            let kelvin = cooled_temperature_k(head_temperature_k(speed), age, EMBER_TAU_S);
            let gone = 1.0 - smoothstep(0.6 * EMBER_LIFE_S, EMBER_LIFE_S, age);
            let gusting = 0.4 + 1.2 * value_noise(profile.seed, 32, ts * 1.3 + age * 0.5);
            (
                EMBER_PEAK * glow.min(1.0) * (-age / EMBER_FADE_S).exp() * gone * gusting,
                glow_color(kelvin),
            )
        } else {
            (0.0, [0.0; 3])
        };
        out.push(TrainPoint {
            position: position + profile.wind * age + meander,
            age_s: age,
            opacity,
            color,
            half_width_m: half_width,
            ember,
            ember_color,
        });
    }
}

/// Fewest seconds a shed piece glows (the smallest pieces burn out first).
pub const PIECE_LIFE_MIN_S: f32 = 2.0;
/// Most seconds a shed piece glows (the largest pieces).
pub const PIECE_LIFE_MAX_S: f32 = 4.0;
/// A shed piece is never brighter than this share of the head's glow: it is a small fragment of
/// the body.
pub const PIECE_MAX_SHARE: f32 = 0.6;
/// Seconds the vapour a shed piece leaves behind takes to cool out of sight; with one exposure of
/// motion it makes the piece a short streak along its path.
pub const PIECE_WAKE_S: f32 = 0.12;
/// Bulk density (kg/m^3) of the body and its fragments: an ordinary-chondrite stone.
const PIECE_DENSITY: f32 = 3_300.0;
/// Hypersonic drag coefficient of a blunt fragment.
const PIECE_DRAG: f32 = 1.0;
/// Separation coefficient of the fragments' sideways push (the bow-shock interaction of Passey &
/// Melosh 1980, at the low end of their range): small, so the pieces drift down the path, not
/// out to the side.
const PIECE_SEPARATION: f32 = 0.02;
/// Fixed integration steps of a piece's fall-back (midpoint rule), whatever its age: the result is
/// continuous in time, so every subframe of an exposure agrees.
const PIECE_STEPS: u32 = 24;
/// Samples of a piece's cooling vapour behind it.
const PIECE_WAKE_SAMPLES: usize = 4;
/// Pixel floor scale of a piece at its core (the head's is 0.7).
const PIECE_PX_SCALE: f32 = 0.45;

/// Extra deceleration per unit of air density and speed squared (m^2/kg) a fragment of radius
/// `piece_radius_m` feels over a body of `body_radius_m`: drag is `Cd A rho v^2 / 2m`, and for a
/// sphere `A / m = 3 / (4 rho_m r)`, so the smaller the piece, the more area per mass and the
/// faster it slows. Zero for a piece the size of the body.
fn piece_ballistic(piece_radius_m: f32, body_radius_m: f32) -> f32 {
    let r = piece_radius_m.max(1e-3);
    let body = body_radius_m.max(r);
    3.0 * PIECE_DRAG / (8.0 * PIECE_DENSITY) * (1.0 / r - 1.0 / body)
}

/// Where a piece released at track time `release_s` is on the flight path at time `now_s`: the
/// track time `tau` whose position it has reached, and its speed as a share `w` of the body's
/// there. The body's own deceleration is in the track; the piece feels `ballistic * rho * v^2` on
/// top of it, so `dw/dt = -ballistic * rho * v_body * w^2` and `dtau/dt = w`. A fixed number of
/// midpoint steps over the piece's age keeps the result continuous in `now_s`.
fn piece_flight(track: &EntryTrack, ground_y: f32, ballistic: f32, release_s: f32, now_s: f32) -> (f32, f32) {
    let h = (now_s - release_s).max(0.0) / PIECE_STEPS as f32;
    let rate = |tau: f32, w: f32| -> (f32, f32) {
        let rho = air_density(track.position_at(tau).y - ground_y);
        let body_speed = track.velocity_at(tau).length();
        (w, -ballistic * rho * body_speed * w * w)
    };
    let (mut tau, mut w) = (release_s, 1.0f32);
    for _ in 0..PIECE_STEPS {
        let (d_tau, d_w) = rate(tau, w);
        let (m_tau, m_w) = rate(tau + 0.5 * h * d_tau, (w + 0.5 * h * d_w).max(0.0));
        tau += h * m_tau;
        w = (w + h * m_w).max(0.0);
    }
    (tau, w)
}

/// The shed pieces: ablation debris that breaks off from the shed altitude on, spread over the
/// glowing flight, each falling back along the path by its own drag, glowing from its own heating,
/// then cooling and fading over a short life. Every piece is gone by the time the head goes dark.
fn shed_pieces_into(track: &EntryTrack, profile: &EntryProfile, time_s: f32, out: &mut Vec<ShedPiece>) {
    out.clear();
    let Some(shed_time) = track.first_time_below(profile.ground_y, profile.shed_altitude_m) else {
        return;
    };
    if time_s < shed_time || profile.shed_count == 0 {
        return;
    }
    let dark_time = track
        .first_time_below(profile.ground_y, GLOW_CEILING_M)
        .unwrap_or_else(|| track.end_time());
    // Releases are spread (one per equal slot, jittered) so that even the longest-lived piece of
    // the last slot is out before the head goes dark: the pieces stay in step with the streak.
    let window = (dark_time - PIECE_LIFE_MAX_S - shed_time).max(0.0);
    let slot = window / profile.shed_count as f32;
    let head_glow = state_at(track, profile, time_s).4;
    for i in 0..profile.shed_count {
        let n = u64::from(i);
        let release = shed_time + slot * (i as f32 + hash01(profile.seed, n, 16));
        let size_share = hash01(profile.seed, n, 14);
        // Ablation debris: 2 to 20 cm across.
        let size = 0.02 + 0.18 * size_share;
        let life =
            (PIECE_LIFE_MIN_S + (PIECE_LIFE_MAX_S - PIECE_LIFE_MIN_S) * size_share).min(dark_time - release);
        let age = time_s - release;
        if !(0.0..life).contains(&age) {
            continue;
        }
        let ballistic = piece_ballistic(0.5 * size, profile.body_radius_m);
        let (tau, w) = piece_flight(track, profile.ground_y, ballistic, release, time_s);
        // The sideways push at break-up (Passey & Melosh 1980): a share of the body's speed that
        // grows with the air density and the body-to-piece size ratio. Drag damps it at the same
        // rate as the speed along the path, so the drift is that push times the track time covered.
        let (_, release_vel, release_speed, release_alt, _) = state_at(track, profile, release);
        let dir = release_vel.normalize_or_zero();
        if dir == Vec3::ZERO {
            continue;
        }
        let side = if dir.y.abs() > 0.9 { Vec3::X } else { Vec3::Y };
        let u = dir.cross(side).normalize_or_zero();
        let v = dir.cross(u);
        // The sideways break-up offset from the deterministic sin/cos pair: the piece positions
        // (and which pieces survive the glow cut) must be the same on every machine
        // (fix-vfx1-determinism finding 5).
        let angle = std::f32::consts::TAU * hash01(profile.seed, n, 13);
        let (sin, cos) = det_sin_cos(angle);
        let across = u * cos + v * sin;
        let push = release_speed
            * (1.5
                * PIECE_SEPARATION
                * (profile.body_radius_m / (0.5 * size)).max(1.0)
                * air_density(release_alt)
                / PIECE_DENSITY)
                .sqrt()
            * (0.5 + 0.5 * hash01(profile.seed, n, 12));
        let position = track.position_at(tau) + across * (push * (tau - release));
        let path_velocity = track.velocity_at(tau);
        let speed = path_velocity.length() * w;
        let velocity = (path_velocity + across * push) * w;
        // Own heating (rho v^3 at the piece), cooling over the life as the piece burns down.
        let temperature_k = cooled_temperature_k(head_temperature_k(speed) - 600.0, age, 0.5 * life);
        // Bright while it ablates, fading over the last 40 % of its life as it burns down and cools.
        let fade = smoothstep(0.0, 0.1, age) * (1.0 - smoothstep(0.6 * life, life, age));
        // A burning fragment flickers as it tumbles: a gust per piece, stable in time.
        let flicker = 0.8 + 0.4 * value_noise(profile.seed, 40 + n, time_s * 12.0);
        let share = 0.35 + 0.2 * size_share;
        let glow = (glow_factor(position.y - profile.ground_y, speed) * share * flicker * fade)
            .min(PIECE_MAX_SHARE * head_glow);
        if glow < 0.001 {
            continue;
        }
        out.push(ShedPiece {
            position,
            size_m: size,
            glow,
            color: glow_color(temperature_k),
            speed,
            velocity,
            age_s: age,
            life_s: life,
            temperature_k,
        });
    }
}

/// One ribbon vertex pair source.
struct RibbonPoint {
    position: Vec3,
    half_width: f32,
    color: [f32; 4],
}

/// How the trail shader shades a ribbon (the `TrailVertex` flag slots).
#[derive(Clone, Copy)]
struct Shade {
    /// Gaussian cross-section with no flat core and no outline (`_pad2[0]`).
    soft: bool,
    /// Share of the alpha that emits without dimming what is behind (`_pad`).
    additive: f32,
}

/// The dark body: a plain ribbon, premultiplied over.
const PLAIN: Shade = Shade {
    soft: false,
    additive: 0.0,
};
/// Dust and smoke: soft across, premultiplied over (it hides the sky behind it).
const SOFT_OVER: Shade = Shade {
    soft: true,
    additive: 0.0,
};
/// Glowing gas: soft across and purely additive (optically thin: it adds light and never dims).
const SOFT_GLOW: Shade = Shade {
    soft: true,
    additive: 1.0,
};

fn push_ribbon(points: &[RibbonPoint], camera: Vec3, shade: Shade, out: &mut Vec<TrailVertex>) {
    let count = points.len();
    if count < 2 {
        return;
    }
    let soft = if shade.soft { 1.0 } else { 0.0 };
    for (i, w) in points.windows(2).enumerate() {
        let (a, b) = (&w[0], &w[1]);
        let along = (b.position - a.position).normalize_or_zero();
        let view = (camera - a.position).normalize_or_zero();
        let right = along.cross(view).normalize_or_zero();
        if right == Vec3::ZERO {
            continue;
        }
        let ta = i as f32 / (count - 1) as f32;
        let tb = (i + 1) as f32 / (count - 1) as f32;
        let vert = |p: &RibbonPoint, side: f32, t: f32| TrailVertex {
            position: (p.position + right * (side * p.half_width)).to_array(),
            _pad: shade.additive,
            uv: [t, side],
            _pad2: [soft, 0.0],
            color: p.color,
        };
        let (a_l, a_r) = (vert(a, -1.0, ta), vert(a, 1.0, ta));
        let (b_l, b_r) = (vert(b, -1.0, tb), vert(b, 1.0, tb));
        out.extend_from_slice(&[a_l, b_l, a_r, a_r, b_l, b_r]);
    }
}

/// Pulls every point farther than `RANGE_FIT * far_m` along its view ray to that distance and
/// scales its width by the same factor: identical on screen, inside the far plane.
fn fit_range(points: &mut [RibbonPoint], view: &EntryView) {
    if !(view.far_m.is_finite() && view.far_m > 0.0) {
        return;
    }
    let limit = RANGE_FIT * view.far_m;
    for p in points {
        let offset = p.position - view.camera;
        let range = offset.length();
        if range > limit {
            let k = limit / range;
            p.position = view.camera + offset * k;
            p.half_width *= k;
        }
    }
}

fn min_half_width(position: Vec3, view: &EntryView, world: f32) -> f32 {
    let range = (position - view.camera).length();
    let floor = 0.5 * view.min_px * range / view.focal_px.max(1.0);
    world.max(floor)
}

/// One nested glow ribbon: its width relative to the core and its share of the emission.
struct GlowLayer {
    width: f32,
    weight: f32,
}

/// The head: a hot core inside two soft halos (each Gaussian across). Outermost first.
const HEAD_LAYERS: [GlowLayer; 3] = [
    GlowLayer {
        width: 6.0,
        weight: 0.03,
    },
    GlowLayer {
        width: 2.6,
        weight: 0.15,
    },
    GlowLayer {
        width: 1.0,
        weight: 1.0,
    },
];
/// The ionised wake behind the head: dimmer than the head and narrower in its halos.
const ION_LAYERS: [GlowLayer; 3] = [
    GlowLayer {
        width: 4.0,
        weight: 0.02,
    },
    GlowLayer {
        width: 2.2,
        weight: 0.12,
    },
    GlowLayer {
        width: 1.0,
        weight: 0.6,
    },
];
/// The glowing train: two soft layers, no core.
const EMBER_LAYERS: [GlowLayer; 2] = [
    GlowLayer {
        width: 2.4,
        weight: 0.3,
    },
    GlowLayer {
        width: 1.0,
        weight: 0.7,
    },
];
/// A shed piece: a hot core only. A wide halo layer read as a detached sprite blob beside the
/// head, so a piece is one streak along its own velocity (see `push_piece`).
const PIECE_LAYERS: [GlowLayer; 1] = [GlowLayer {
    width: 1.0,
    weight: 1.0,
}];

/// Along the head, as a share of its half-length: offsets, widths and brightness of a round, soft
/// blob. The brightness follows the same Gaussian as the shader's cross-section, `exp(-4.5 x²)`
/// (zero at the ends), and the width a circle's profile, so the blob is equally soft in every
/// direction and a saturated core has no flat end.
const BLOB: [(f32, f32, f32); 7] = [
    (-1.0, 0.35, 0.0),
    (-0.667, 0.75, 0.135),
    (-0.333, 0.94, 0.607),
    (0.0, 1.0, 1.0),
    (0.333, 0.94, 0.607),
    (0.667, 0.75, 0.135),
    (1.0, 0.35, 0.0),
];

/// One emitting sample along a glowing ribbon.
struct GlowPoint {
    position: Vec3,
    /// Half-width of the core in metres.
    half_width: f32,
    /// Scene-linear radiance the sample adds at the core.
    emission: [f32; 3],
    /// 0..1 fade along the ribbon (0 at an end that should dissolve).
    fade: f32,
    /// Scale of the pixel floor (the envelope narrows as the glow dims, so the wake tapers).
    px_scale: f32,
}

/// Pushes the nested layers of one glowing strip: soft, additive ribbons, each adding
/// `emission * weight * fade` with a Gaussian cross-section and dimming nothing behind it.
fn push_glow(
    points: &[GlowPoint],
    layers: &[GlowLayer],
    view: &EntryView,
    out: &mut Vec<TrailVertex>,
    strip: &mut Vec<RibbonPoint>,
) {
    for layer in layers {
        strip.clear();
        for p in points {
            let sized = EntryView {
                min_px: view.min_px * layer.width * p.px_scale,
                ..*view
            };
            strip.push(RibbonPoint {
                position: p.position,
                half_width: min_half_width(p.position, &sized, p.half_width * layer.width),
                color: [
                    p.emission[0] * layer.weight,
                    p.emission[1] * layer.weight,
                    p.emission[2] * layer.weight,
                    p.fade.clamp(0.0, 1.0),
                ],
            });
        }
        fit_range(strip, view);
        push_ribbon(strip, view.camera, SOFT_GLOW, out);
    }
}

/// A round, soft, camera-facing glow (the head, a shed piece).
struct Blob {
    center: Vec3,
    /// Unit direction of travel.
    direction: Vec3,
    /// Half-width of the core in metres.
    half_width_m: f32,
    /// Scale of the pixel floor.
    px_scale: f32,
    /// Half the distance travelled in one exposure (m).
    smear_m: f32,
    /// Scene-linear radiance at the core.
    emission: [f32; 3],
}

/// The screen axis a blob is stretched along and the share of its travel seen on screen: the
/// direction of travel with its part toward the camera removed. Head-on (under 5 % of the travel
/// on screen) any screen axis will do, since the blob is then round.
fn screen_axis(center: Vec3, direction: Vec3, camera: Vec3) -> (Vec3, f32) {
    let to_camera = (camera - center).normalize_or_zero();
    let across = direction - to_camera * direction.dot(to_camera);
    let seen = across.length().min(1.0);
    if seen > 0.05 {
        (across / seen.max(1e-6), seen)
    } else {
        let side = if to_camera.y.abs() > 0.9 { Vec3::X } else { Vec3::Y };
        (to_camera.cross(side).normalize_or_zero(), seen)
    }
}

/// Pushes a blob per layer: a short ribbon along its travel as seen on screen, as long as it is
/// wide plus the on-screen part of one exposure of travel, faded to nothing at both ends, Gaussian
/// across. It stays round when the body comes straight at the camera.
fn push_blob(
    blob: &Blob,
    layers: &[GlowLayer],
    view: &EntryView,
    out: &mut Vec<TrailVertex>,
    strip: &mut Vec<RibbonPoint>,
) {
    let (axis, seen) = screen_axis(blob.center, blob.direction, view.camera);
    if axis == Vec3::ZERO {
        return;
    }
    for layer in layers {
        let sized = EntryView {
            min_px: view.min_px * layer.width * blob.px_scale,
            ..*view
        };
        let half_width = min_half_width(blob.center, &sized, blob.half_width_m * layer.width);
        let half_length = half_width + blob.smear_m * seen;
        strip.clear();
        for &(along, width, fade) in &BLOB {
            strip.push(RibbonPoint {
                position: blob.center + axis * (along * half_length),
                half_width: half_width * width,
                color: [
                    blob.emission[0] * layer.weight,
                    blob.emission[1] * layer.weight,
                    blob.emission[2] * layer.weight,
                    fade,
                ],
            });
        }
        fit_range(strip, view);
        push_ribbon(strip, view.camera, SOFT_GLOW, out);
    }
}

/// Pushes the head: a blob in the three `HEAD_LAYERS` (see `push_blob`).
fn push_head(
    head: &EntryHead,
    view: &EntryView,
    profile: &EntryProfile,
    out: &mut Vec<TrailVertex>,
    strip: &mut Vec<RibbonPoint>,
) {
    let blob = Blob {
        center: head.position,
        direction: head.direction,
        half_width_m: 0.35 * profile.head_radius_m,
        px_scale: 0.7 * (0.6 + 0.4 * head.glow.min(1.0).sqrt()),
        smear_m: 0.5 * head.speed * view.shutter_s,
        emission: head.color.map(|c| c * head.radiance),
    };
    push_blob(&blob, &HEAD_LAYERS, view, out, strip);
}

/// Ribbon lists for one entry build. Reused by [`EntryScratch`]; a one-shot [`build_vertices`]
/// owns a fresh one.
#[derive(Default)]
struct VertexLists {
    ribbon: Vec<RibbonPoint>,
    glow: Vec<GlowPoint>,
    strip: Vec<RibbonPoint>,
    piece_points: Vec<GlowPoint>,
}

impl VertexLists {
    fn reserve_for(&mut self, profile: &EntryProfile, tier: EntryTier) {
        let train_n = train_slot_bound(profile, tier);
        let ion_n = ion_sample_bound(tier);
        reserve_at_least(&mut self.ribbon, train_n);
        reserve_at_least(&mut self.glow, train_n.max(ion_n + 1));
        reserve_at_least(&mut self.strip, train_n.max(BLOB.len()));
        reserve_at_least(&mut self.piece_points, 2 + PIECE_WAKE_SAMPLES);
    }
}

fn reserve_at_least<T>(buf: &mut Vec<T>, n: usize) {
    if buf.capacity() < n {
        buf.reserve(n - buf.len());
    }
}

fn tier_step(tier: EntryTier) -> f32 {
    match tier {
        EntryTier::Full => 0.25,
        EntryTier::Cheap => 1.0,
    }
}

fn train_slot_bound(profile: &EntryProfile, tier: EntryTier) -> usize {
    let step = tier_step(tier);
    let window = profile.train_life_s.max(profile.dust_life_s).max(step);
    if !window.is_finite() || !step.is_finite() || step <= 0.0 {
        return 4;
    }
    (window / step).ceil() as usize + 4
}

fn ion_sample_bound(tier: EntryTier) -> usize {
    match tier {
        EntryTier::Full => 24,
        EntryTier::Cheap => 6,
    }
}

/// Most trail vertices one streak frame appends for this profile and tier.
///
/// The steady frame reserves this once. Later frames fit in that buffer.
pub(crate) fn entry_vertex_bound(profile: &EntryProfile, tier: EntryTier) -> usize {
    let train_n = train_slot_bound(profile, tier);
    // Ion samples plus the point that dissolves the wake into the head.
    let ion_n = ion_sample_bound(tier) + 1;
    let piece_n = if tier == EntryTier::Full {
        profile.shed_count as usize
    } else {
        0
    };
    let ribbon = |n: usize| n.saturating_sub(1) * 6;
    let piece_pts = 2 + PIECE_WAKE_SAMPLES;
    // A piece is either a streak or a head-on blob.
    let piece = ribbon(piece_pts).max(ribbon(BLOB.len())) * PIECE_LAYERS.len();
    ribbon(train_n)
        + ribbon(train_n) * EMBER_LAYERS.len()
        + ribbon(ion_n) * ION_LAYERS.len()
        + ribbon(BLOB.len()) * HEAD_LAYERS.len()
        + 6
        + piece_n * piece
}

/// Reused sample and ribbon lists for a steady entry-streak frame.
///
/// [`crate::entry_runtime::step_entry_streaks`] keeps one of these and refills it. The first
/// frame for a profile reserves the lists and the trail buffer; a later frame allocates nothing.
#[derive(Default)]
pub struct EntryScratch {
    ion: Vec<IonPoint>,
    train: Vec<TrainPoint>,
    pieces: Vec<ShedPiece>,
    lists: VertexLists,
}

impl EntryScratch {
    /// Empty lists. The first evaluated frame reserves them.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluates `track` at `time_s` and appends its ribbons to `out`.
    pub(crate) fn eval_and_append(
        &mut self,
        track: &EntryTrack,
        profile: &EntryProfile,
        tier: EntryTier,
        time_s: f32,
        view: &EntryView,
        out: &mut Vec<TrailVertex>,
    ) {
        self.reserve_for(profile, tier);
        let bound = entry_vertex_bound(profile, tier);
        if out.capacity() < bound {
            out.reserve(bound - out.len());
        }
        // Move the ribbon lists out so the sample slices and the lists can be borrowed together.
        // `mem::take` moves the buffers (their capacity included) and leaves an empty default;
        // putting them back does not allocate.
        let mut lists = std::mem::take(&mut self.lists);
        let head = fill_entry(
            track,
            profile,
            tier,
            time_s,
            &mut self.ion,
            &mut self.train,
            &mut self.pieces,
        );
        let parts = EntryParts {
            head: &head,
            ion: &self.ion,
            train: &self.train,
            pieces: &self.pieces,
            view,
            profile,
        };
        append_parts(&parts, out, &mut lists);
        self.lists = lists;
    }

    fn reserve_for(&mut self, profile: &EntryProfile, tier: EntryTier) {
        reserve_at_least(&mut self.ion, ion_sample_bound(tier));
        reserve_at_least(&mut self.train, train_slot_bound(profile, tier));
        let piece_n = if tier == EntryTier::Full {
            profile.shed_count as usize
        } else {
            0
        };
        reserve_at_least(&mut self.pieces, piece_n);
        self.lists.reserve_for(profile, tier);
    }
}

struct EntryParts<'a> {
    head: &'a EntryHead,
    ion: &'a [IonPoint],
    train: &'a [TrainPoint],
    pieces: &'a [ShedPiece],
    view: &'a EntryView,
    profile: &'a EntryProfile,
}

/// Appends the frame's camera-facing ribbons (triangle lists for the trail pipeline) to `out`.
///
/// Order: dust train (back), glowing train, ionised wake, head, dark body, pieces. The head is a
/// round, camera-facing blob (see `push_blob`); each piece is a short streak along its own velocity
/// (see `push_piece`). The head and the dark body keep at least `view.min_px` of apparent width so
/// they read at range. Glowing
/// parts are soft, additive nested ribbons (see `push_glow`) whose emission is peak radiance
/// times the cooling colour, so the post stack's bloom, not a saturated flat bar, makes the glow;
/// the wake's pixel floor narrows as it dims, so it tapers instead of reading as a rod. The dust
/// train is soft across and hides the sky behind it.
pub fn build_vertices(
    frame: &EntryFrame,
    view: &EntryView,
    profile: &EntryProfile,
    out: &mut Vec<TrailVertex>,
) {
    let mut lists = VertexLists::default();
    let parts = EntryParts {
        head: &frame.head,
        ion: &frame.ion,
        train: &frame.train,
        pieces: &frame.pieces,
        view,
        profile,
    };
    append_parts(&parts, out, &mut lists);
}

fn append_parts(parts: &EntryParts<'_>, out: &mut Vec<TrailVertex>, lists: &mut VertexLists) {
    let _span = tracing::trace_span!("entry_build_vertices").entered();
    let view = parts.view;
    let profile = parts.profile;
    let head = parts.head;
    let thin = EntryView {
        min_px: 0.5 * view.min_px,
        ..*view
    };
    lists.ribbon.clear();
    for p in parts.train {
        lists.ribbon.push(RibbonPoint {
            position: p.position,
            // A far train keeps a one-pixel-wide line (half the head's floor) instead of vanishing.
            half_width: min_half_width(p.position, &thin, p.half_width_m),
            color: [p.color[0], p.color[1], p.color[2], p.opacity],
        });
    }
    fit_range(&mut lists.ribbon, view);
    push_ribbon(&lists.ribbon, view.camera, SOFT_OVER, out);

    let peak = profile.peak_radiance;
    lists.glow.clear();
    for p in parts.train {
        if p.ember > 0.0 {
            lists.glow.push(GlowPoint {
                position: p.position,
                half_width: 0.4 * p.half_width_m,
                emission: p.ember_color.map(|c| c * p.ember * peak),
                fade: 1.0,
                px_scale: 0.5 + 0.5 * (p.ember / EMBER_PEAK).min(1.0).sqrt(),
            });
        }
    }
    push_glow(&lists.glow, &EMBER_LAYERS, &thin, out, &mut lists.strip);

    // The wake runs oldest to newest, then ends at the head; both ends dissolve.
    lists.glow.clear();
    for (i, p) in parts.ion.iter().rev().enumerate() {
        lists.glow.push(GlowPoint {
            position: p.position,
            half_width: 0.35 * p.half_width_m,
            emission: p.color.map(|c| c * p.brightness * peak),
            fade: if i == 0 { 0.0 } else { 1.0 },
            px_scale: 0.35 + 0.65 * p.brightness.min(1.0).sqrt(),
        });
    }
    if head.glow > 0.0 {
        // The wake dissolves into the head blob instead of ending in a flat cut at its centre.
        lists.glow.push(GlowPoint {
            position: head.position,
            half_width: 0.35 * profile.head_radius_m,
            emission: head.color.map(|c| c * head.radiance),
            fade: 0.0,
            px_scale: 0.35 + 0.65 * head.glow.min(1.0).sqrt(),
        });
    }
    push_glow(&lists.glow, &ION_LAYERS, view, out, &mut lists.strip);
    if head.glow > 0.0 && head.direction != Vec3::ZERO {
        push_head(head, view, profile, out, &mut lists.strip);
    }
    if head.glow <= 0.0 && head.direction != Vec3::ZERO {
        // Dark flight: a thin dark streak of speed * shutter, no emission.
        let half = 0.5 * head.speed * view.shutter_s;
        let width = min_half_width(head.position, view, profile.body_radius_m);
        let dark = [0.02, 0.02, 0.025, 0.9];
        let mut streak = [
            RibbonPoint {
                position: head.position - head.direction * half,
                half_width: width,
                color: dark,
            },
            RibbonPoint {
                position: head.position + head.direction * half,
                half_width: width,
                color: dark,
            },
        ];
        fit_range(&mut streak, view);
        push_ribbon(&streak, view.camera, PLAIN, out);
    }

    for piece in parts.pieces {
        push_piece(piece, head.direction, view, peak, out, lists);
    }
}

/// Pushes a shed piece: a short soft streak along its own velocity. The front is the piece moving
/// over one exposure (`speed * shutter_s`, as the head is smeared); behind it, its vapour cools and
/// fades over [`PIECE_WAKE_S`], so the streak tapers to nothing back along the path. Seen exactly
/// head-on (under 5 % of the travel on screen) the path has no length on screen and the piece is a
/// round blob, as the head is.
fn push_piece(
    piece: &ShedPiece,
    fallback: Vec3,
    view: &EntryView,
    peak: f32,
    out: &mut Vec<TrailVertex>,
    lists: &mut VertexLists,
) {
    let speed = piece.velocity.length();
    let direction = piece.velocity.normalize_or(fallback);
    let emission = piece.color.map(|c| c * piece.glow * peak);
    let half_width = 0.5 * piece.size_m;
    let smear = 0.5 * speed * view.shutter_s;
    let (_, seen) = screen_axis(piece.position, direction, view.camera);
    if seen <= 0.05 {
        let blob = Blob {
            center: piece.position,
            direction,
            half_width_m: half_width,
            px_scale: PIECE_PX_SCALE,
            smear_m: smear,
            emission,
        };
        push_blob(&blob, &PIECE_LAYERS, view, out, &mut lists.strip);
        return;
    }
    let point = |offset: f32, dim: f32, fade: f32, px_scale: f32| GlowPoint {
        position: piece.position + direction * offset,
        half_width,
        emission: emission.map(|c| c * dim),
        fade,
        px_scale,
    };
    // The leading edge of the exposure, then the piece, then its cooling vapour.
    lists.piece_points.clear();
    lists.piece_points.push(point(smear, 1.0, 0.0, PIECE_PX_SCALE));
    lists.piece_points.push(point(0.0, 1.0, 1.0, PIECE_PX_SCALE));
    for k in 1..=PIECE_WAKE_SAMPLES {
        let share = k as f32 / PIECE_WAKE_SAMPLES as f32;
        let fade = if k == PIECE_WAKE_SAMPLES { 0.0 } else { 1.0 };
        lists.piece_points.push(point(
            -(smear + speed * PIECE_WAKE_S * share),
            (-2.0 * share).exp(),
            fade,
            PIECE_PX_SCALE * (1.0 - 0.4 * share),
        ));
    }
    push_glow(&lists.piece_points, &PIECE_LAYERS, view, out, &mut lists.strip);
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec2;

    /// Pinned shed-piece expectations of `shed_pieces_are_pinned_bit_for_bit` (deterministic
    /// kernels; a platform libm drifts from them). Re-pinned for ADR-1517's default set (ten
    /// pieces, share 0.35 + 0.2 × size, cap 0.6); with the earlier six, 0.2 + 0.15 × size and 0.4
    /// the previous pin (3 pieces, 4_772_160_529_888_848_933) still holds bit for bit.
    const SHED_PIECE_COUNT_T12: u32 = 7;
    const SHED_PIECES_BITS_HASH_T12: u64 = 949_664_889_483_410_826;

    /// A bolide from the NE (azimuth 40 degrees) at about 11 km/s from 80 km, slowing through
    /// 40 to 20 km, then a dark fall into the site at (700, 105.7, 5).
    fn track() -> EntryTrack {
        let at = |time: f32, alt: f32, dist: f32| TrackPoint {
            time,
            position: Vec3::new(700.0 + 0.643 * dist, alt, 5.0 + 0.766 * dist),
        };
        EntryTrack::new(vec![
            at(0.0, 80_000.0, 140_000.0),
            at(6.0, 56_000.0, 78_000.0),
            at(9.0, 42_000.0, 52_000.0),
            at(12.0, 31_000.0, 36_000.0),
            at(15.0, 23_000.0, 26_000.0),
            at(18.0, 17_500.0, 19_000.0),
            at(100.0, 3_000.0, 1_500.0),
            at(120.0, 105.7, 0.0),
        ])
        .expect("track")
    }

    #[test]
    fn rejects_bad_tracks() {
        assert_eq!(EntryTrack::new(Vec::new()), Err(EntryError::TooFewPoints));
        let p = |t: f32| TrackPoint {
            time: t,
            position: Vec3::ZERO,
        };
        assert_eq!(
            EntryTrack::new(vec![p(0.0), p(0.0)]),
            Err(EntryError::NotIncreasing(1))
        );
        assert_eq!(
            EntryTrack::new(vec![p(0.0), p(f32::NAN)]),
            Err(EntryError::NonFinite(1))
        );
    }

    #[test]
    fn nothing_glows_at_or_below_the_ceiling() {
        for alt in [0.0, 100.0, 5_000.0, 19_999.0, GLOW_CEILING_M] {
            for speed in [150.0, 2_500.0, 11_000.0, 40_000.0] {
                assert_eq!(glow_factor(alt, speed), 0.0, "alt {alt} speed {speed}");
            }
        }
        assert!(glow_factor(40_000.0, 11_000.0) > 0.5);
    }

    #[test]
    fn terminal_speed_never_glows_even_high_up() {
        assert_eq!(glow_factor(60_000.0, 200.0), 0.0);
        assert!(glow_factor(60_000.0, 11_000.0) > 0.0);
    }

    #[test]
    fn colour_ramps_white_orange_red_with_speed() {
        let (white, orange, red) = (heat_color(11_000.0), heat_color(5_500.0), heat_color(2_000.0));
        assert!(white[2] > 0.85, "white has blue");
        assert!(orange[0] > orange[1] && orange[1] > orange[2] && orange[2] < 0.2);
        assert!(red[0] < 0.6 && red[0] > red[1] * 3.0, "dull red");
        assert!(white[2] > orange[2] && orange[0] > red[0]);
    }

    #[test]
    fn head_is_bright_aloft_and_dark_below_20_km() {
        let (t, p) = (track(), EntryProfile::default());
        let high = evaluate(&t, &p, EntryTier::Full, 5.5);
        assert!(
            high.head.glow > 0.4 && high.head.radiance > 10.0,
            "{:?}",
            high.head
        );
        for time in [20.0, 30.0, 100.0, 119.0] {
            let low = evaluate(&t, &p, EntryTier::Full, time);
            assert!(low.head.altitude_m < GLOW_CEILING_M);
            assert_eq!(low.head.glow, 0.0);
            assert_eq!(low.head.radiance, 0.0);
            assert!(low.ion.is_empty(), "no ionised trail in dark flight");
            assert!(low.pieces.is_empty(), "no glowing pieces in dark flight");
        }
    }

    #[test]
    fn train_lasts_two_minutes_drifts_and_widens() {
        let (t, p) = (track(), EntryProfile::legacy_dust());
        let near = evaluate(&t, &p, EntryTier::Full, 10.0);
        let late = evaluate(&t, &p, EntryTier::Full, 130.0);
        let glowing = |f: &EntryFrame| f.train.iter().filter(|x| x.color[0] > 0.5).count();
        assert!(glowing(&near) > 0);
        assert!(glowing(&late) > 0, "train persists 2 min after the streak");
        let oldest = late.train.iter().find(|x| x.color[0] > 0.5).expect("dust");
        let strongest = late
            .train
            .iter()
            .filter(|x| x.color[0] > 0.5)
            .map(|x| x.opacity)
            .fold(0.0, f32::max);
        let newest = near.train.iter().rev().find(|x| x.color[0] > 0.5).expect("dust");
        assert!(oldest.half_width_m > newest.half_width_m, "diffusion widens");
        assert!(strongest > 0.1, "still visible: {strongest}");
        // wind drift: x moves with wind.x * age relative to a windless run
        let calm = EntryProfile {
            wind: Vec3::ZERO,
            ..p.clone()
        };
        let calm_late = evaluate(&t, &calm, EntryTier::Full, 130.0);
        let drift = oldest.position.x - calm_late.train[0].position.x;
        assert!(drift > 1_000.0, "drift {drift}");
    }

    #[test]
    fn dark_fall_leaves_only_a_faint_dark_train() {
        let (t, p) = (track(), EntryProfile::default());
        let f = evaluate(&t, &p, EntryTier::Full, 60.0);
        let dark: Vec<_> = f.train.iter().filter(|x| x.color[0] < 0.3).collect();
        assert!(!dark.is_empty());
        assert!(dark.iter().all(|x| x.opacity <= p.damage_smoke + 1e-6));
        // Damage smoke (the dark colour, not the glowing dust) is deposited on a 1 s grid
        // (four tier steps at Full). The glowing wake above the ceiling still uses `step`.
        let damage: Vec<_> = f
            .train
            .iter()
            .filter(|point| {
                (point.color[0] - 0.2).abs() < 1e-4
                    && (point.color[1] - 0.19).abs() < 1e-4
                    && (point.color[2] - 0.19).abs() < 1e-4
            })
            .collect();
        assert!(!damage.is_empty(), "dark flight leaves damage smoke");
        assert!(
            damage.len() <= 62,
            "damage smoke has {} samples; a 1 s step over 60 s is at most 62",
            damage.len()
        );
        for point in &damage {
            let ts = 60.0 - point.age_s;
            let nearest = (ts / 1.0).round();
            assert!(
                (ts - nearest).abs() <= 0.0625,
                "damage-smoke track time {ts} is not on the 1 s grid"
            );
        }
        let none = EntryProfile {
            damage_smoke: 0.0,
            ..p
        };
        assert!(
            evaluate(&t, &none, EntryTier::Full, 60.0)
                .train
                .iter()
                .all(|x| x.color[0] > 0.3)
        );
    }

    #[test]
    fn pieces_fall_behind_and_die_without_trails() {
        let (t, p) = (track(), EntryProfile::default());
        let shed = t
            .first_time_below(p.ground_y, p.shed_altitude_m)
            .expect("crossing");
        assert!(shed > 6.0 && shed < 9.0, "{shed}");
        assert!(evaluate(&t, &p, EntryTier::Full, shed - 0.5).pieces.is_empty());
        let f = evaluate(&t, &p, EntryTier::Full, shed + 2.0);
        assert!(!f.pieces.is_empty());
        for piece in &f.pieces {
            // Fall-back builds from zero: a piece's lag behind the head is roughly
            // `0.5 * decel * age^2`, so a newborn piece is (correctly) still on the head
            // and only pieces old enough to physically be a metre behind are checked.
            // (At ten pieces a release lands within ~0.05 s of any sample; the pre-1517
            // grid of six never happened to sample a piece this young here.)
            if piece.age_s > 0.25 {
                assert!(
                    piece.position.distance(f.head.position) > 1.0,
                    "age {} s: {} m behind",
                    piece.age_s,
                    piece.position.distance(f.head.position)
                );
            }
        }
        // The oldest piece has had time to fall well back, whatever its size.
        let oldest = f
            .pieces
            .iter()
            .max_by(|a, b| a.age_s.total_cmp(&b.age_s))
            .expect("pieces");
        assert!(
            oldest.position.distance(f.head.position) > 10.0,
            "age {} s: {} m behind",
            oldest.age_s,
            oldest.position.distance(f.head.position)
        );
        assert!(evaluate(&t, &p, EntryTier::Full, 60.0).pieces.is_empty());
    }

    #[test]
    fn pieces_are_streaks_along_their_own_velocity_without_a_halo() {
        let (t, p) = (track(), EntryProfile::default());
        let shed = t
            .first_time_below(p.ground_y, p.shed_altitude_m)
            .expect("crossing");
        assert_eq!(PIECE_LAYERS.len(), 1, "no halo layer");
        let f = evaluate(&t, &p, EntryTier::Full, shed + 2.0);
        assert!(!f.pieces.is_empty());
        // Velocity is finite, at least the along-track speed, and the piece moves by it between
        // two close evaluations (per-subframe positions follow the velocity).
        let dt = 1.0 / 240.0;
        let g = evaluate(&t, &p, EntryTier::Full, shed + 2.0 + dt);
        for (a, b) in f.pieces.iter().zip(&g.pieces) {
            assert!(a.velocity.is_finite() && a.velocity.length() >= a.speed * 0.99);
            let step = b.position - a.position;
            let expect = a.velocity * dt;
            assert!(
                step.distance(expect) < 0.5 * expect.length() + 0.5,
                "{step:?} vs {expect:?}"
            );
        }
        // One layer's worth of vertices per piece, and the streak lengthens with the exposure.
        let mut short = Vec::new();
        let mut long = Vec::new();
        let v0 = view();
        let v1 = EntryView {
            shutter_s: v0.shutter_s * 4.0,
            ..v0
        };
        build_vertices(&f, &v0, &p, &mut short);
        build_vertices(&f, &v1, &p, &mut long);
        assert_eq!(short.len(), long.len());
        assert_ne!(
            bytemuck::cast_slice::<_, u8>(&short),
            bytemuck::cast_slice::<_, u8>(&long),
            "pieces are motion-blurred by the shutter"
        );
    }

    /// Pixel coordinates (x right, y up) of `p` for `view`'s camera aimed at `target`.
    fn to_px(p: Vec3, target: Vec3, view: &EntryView) -> Vec2 {
        let forward = (target - view.camera).normalize();
        let right = forward.cross(Vec3::Y).normalize();
        let up = right.cross(forward);
        let d = p - view.camera;
        Vec2::new(d.dot(right), d.dot(up)) * (view.focal_px / d.dot(forward))
    }

    /// Distance (px) from `at` to the flight path's track points between `from_s` and `to_s`.
    fn px_to_path(
        at: Vec2,
        track: &EntryTrack,
        from_s: f32,
        to_s: f32,
        target: Vec3,
        view: &EntryView,
    ) -> f32 {
        let mut best = f32::INFINITY;
        let mut ts = from_s;
        while ts <= to_s {
            best = best.min(at.distance(to_px(track.position_at(ts), target, view)));
            ts += 0.005;
        }
        best
    }

    /// The owner's look call: in shot 3 (4K, nearly head-on) every shed piece is behind the head on
    /// the flight path, within a few pixels of it sideways and drifting down the train, and is drawn
    /// as a streak whose long axis is the path on screen, never a static round sprite.
    #[test]
    fn fragments_trail_down_the_flight_path_as_streaks_not_round_sprites() {
        let (track, view) = shot3();
        let p = EntryProfile::default();
        let shed = track
            .first_time_below(p.ground_y, p.shed_altitude_m)
            .expect("crossing");
        let (mut seen, mut far_down) = (0, 0);
        let mut time = shed;
        while time <= 33.0 {
            let frame = evaluate(&track, &p, EntryTier::Full, time);
            let head = frame.head;
            let target = head.position;
            let head_px = to_px(head.position, target, &view);
            let next = evaluate(&track, &p, EntryTier::Full, time + 1.0 / 60.0);
            for (k, piece) in frame.pieces.iter().enumerate() {
                seen += 1;
                // Behind the head along its heading, never ahead (within float noise at break-up),
                // and clearly behind once it has flown for half a second.
                let behind = (head.position - piece.position).dot(head.direction);
                let least = if piece.age_s > 0.5 { 1.0 } else { -1.0 };
                assert!(behind > least, "{time} s: piece {k} is {behind} m behind");
                // On the flight path on screen (the train's line), off it by a few pixels at most,
                // and the sideways offset small next to the distance down the train.
                let at = to_px(piece.position, target, &view);
                let down = at.distance(head_px);
                let off = px_to_path(at, &track, shed, time, target, &view);
                assert!(
                    off <= 4.0 && off <= 1.0 + 0.25 * down,
                    "{time} s: {off} px off the path, {down} px down"
                );
                if down > 8.0 {
                    far_down += 1;
                }
                // A streak along the path: the piece's own vertices, projected, are at least 2.5
                // times longer along its screen velocity than across, and the ribbon is the
                // streak's (front, piece, wake samples), not a blob's.
                let mut v = Vec::new();
                let mut lists = VertexLists::default();
                push_piece(piece, head.direction, &view, p.peak_radiance, &mut v, &mut lists);
                assert_eq!(
                    v.len(),
                    (PIECE_WAKE_SAMPLES + 1) * 6,
                    "{time} s: a streak, not a blob"
                );
                let axis = (to_px(piece.position + piece.velocity, target, &view) - at).normalize();
                let normal = axis.perp();
                let span = |dir: Vec2| {
                    let d: Vec<f32> = v
                        .iter()
                        .map(|x| (to_px(Vec3::from_array(x.position), target, &view) - at).dot(dir))
                        .collect();
                    d.iter().copied().fold(f32::MIN, f32::max) - d.iter().copied().fold(f32::MAX, f32::min)
                };
                let (along, across) = (span(axis), span(normal));
                assert!(
                    along >= 2.5 * across,
                    "{time} s: {along} px long, {across} px wide"
                );
                // The path's screen direction at the piece: the streak's axis follows it.
                let path_axis = (head_px - to_px(track.position_at(time - 1.0), target, &view)).normalize();
                assert!(
                    axis.dot(path_axis) > 0.95,
                    "{time} s: streak axis {axis} vs path {path_axis}"
                );
                // It moves (it is not a sprite pinned in place): one frame later it is further along.
                let later = next
                    .pieces
                    .iter()
                    .find(|q| q.life_s == piece.life_s && (q.age_s - piece.age_s - 1.0 / 60.0).abs() < 1e-3);
                if let Some(later) = later {
                    assert!(later.position.distance(piece.position) > 0.5 * piece.speed / 60.0);
                }
            }
            time += 0.25;
        }
        assert!(seen > 20, "{seen} piece samples");
        assert!(far_down > 3, "pieces fall well down the train: {far_down}");
    }

    #[test]
    fn small_pieces_decelerate_faster_and_fall_further_behind() {
        let (track, _) = shot3();
        let (small, large) = (piece_ballistic(0.03, 6.5), piece_ballistic(0.25, 6.5));
        assert!(small > 5.0 * large && large > 0.0, "{small} vs {large}");
        assert_eq!(piece_ballistic(6.5, 6.5), 0.0, "a piece the body's size keeps up");
        let release = 22.0;
        let (mut last_small, mut last_large) = ((release, 1.0f32), (release, 1.0f32));
        for k in 1..=12 {
            let now = release + 0.25 * k as f32;
            let s = piece_flight(&track, 0.0, small, release, now);
            let l = piece_flight(&track, 0.0, large, release, now);
            // On the path behind the body (track time behind `now`), slower than it, and falling
            // further back every step; the smaller piece always more so.
            assert!(s.0 < now && l.0 < now && s.1 < 1.0 && l.1 < 1.0);
            assert!(s.0 < l.0 && s.1 < l.1, "{now}: {s:?} vs {l:?}");
            assert!(now - s.0 > now - 0.25 - last_small.0 && s.1 <= last_small.1);
            assert!(now - l.0 > now - 0.25 - last_large.0 && l.1 <= last_large.1);
            (last_small, last_large) = (s, l);
        }
        // Continuous in time (every subframe agrees): a tiny step moves it a tiny amount.
        let (a, b) = (
            piece_flight(&track, 0.0, small, release, 24.0),
            piece_flight(&track, 0.0, small, release, 24.0 + 1e-3),
        );
        assert!((b.0 - a.0).abs() < 2e-3 && (b.1 - a.1).abs() < 1e-3);
    }

    /// Finding 5 of the vfx1 determinism audit: the shed pieces are pinned bit for bit. The
    /// sideways offset, the air-density push and the cooling all build on the deterministic
    /// kernels, so the same seed gives exactly these positions (and the same piece count, the
    /// glow cut included) on glibc, the Windows CRT and musl.
    #[test]
    fn shed_pieces_are_pinned_bit_for_bit() {
        let track = track();
        let profile = EntryProfile::default(); // seed 0x5eed_0e47
        let frame = evaluate(&track, &profile, EntryTier::Full, 12.0);
        assert!(!frame.pieces.is_empty(), "pieces shed below 45 km by t=12");
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for piece in &frame.pieces {
            for v in piece.position.to_array() {
                for byte in v.to_bits().to_le_bytes() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x100_0000_01b3);
                }
            }
            for byte in piece.glow.to_bits().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
        assert_eq!(frame.pieces.len() as u32, SHED_PIECE_COUNT_T12);
        assert_eq!(hash, SHED_PIECES_BITS_HASH_T12);
    }

    #[test]
    fn each_piece_glows_from_its_heating_then_cools_and_fades_dimmer_than_the_head() {
        let (track, _) = shot3();
        // ADR-1517 pins the share increase with behaviour, not constants: a piece's peak glow as a
        // share of the head's must exceed 0.45, which the pre-1517 light cannot reach even without
        // its 0.4 cap (share at most 0.35 times flicker at most 1.2 is 0.42). Reverting the share
        // formula or the cap alone turns this red.
        let mut peak_share = 0.0f32;
        for seed in [0x5eed_0e47, 7, 99] {
            let p = EntryProfile {
                seed,
                shed_count: 1,
                ..EntryProfile::default()
            };
            let mut samples = Vec::new();
            let mut time = 19.0;
            while time <= 33.0 {
                let frame = evaluate(&track, &p, EntryTier::Full, time);
                for piece in &frame.pieces {
                    assert!(
                        piece.glow <= PIECE_MAX_SHARE * frame.head.glow + 1e-6,
                        "dimmer than the head"
                    );
                    if frame.head.glow > 1e-3 {
                        peak_share = peak_share.max(piece.glow / frame.head.glow);
                    }
                    assert!((PIECE_LIFE_MIN_S..=PIECE_LIFE_MAX_S).contains(&piece.life_s));
                    samples.push(*piece);
                }
                time += 1.0 / 60.0;
            }
            assert!(samples.len() > 60, "seed {seed}: {}", samples.len());
            // Cools over its life, never reheats.
            for w in samples.windows(2) {
                assert!(w[1].age_s > w[0].age_s);
                assert!(w[1].temperature_k <= w[0].temperature_k + 1e-3, "seed {seed}");
            }
            // Fades out: the brightest moment is early, and the last is a small share of it.
            let peak = samples.iter().map(|x| x.glow).fold(0.0f32, f32::max);
            let last = samples.last().expect("samples");
            assert!(last.glow < 0.2 * peak, "seed {seed}: {} vs {peak}", last.glow);
            assert!(
                last.age_s > 0.8 * last.life_s,
                "seed {seed}: it glows its whole life"
            );
            assert!(
                last.temperature_k < samples[0].temperature_k - 500.0,
                "seed {seed}: it cools"
            );
        }
        assert!(
            peak_share > 0.45,
            "ADR-1517: a piece's peak glow must exceed 0.45 of the head's (got {peak_share})"
        );
    }

    #[test]
    fn default_pieces_are_the_more_visible_set() {
        // ADR-1517: ten pieces by default, each up to 0.6 of the head's glow (was 6 and 0.4).
        let p = EntryProfile::default();
        assert_eq!(p.shed_count, 10);
        let cap = PIECE_MAX_SHARE;
        assert!((cap - 0.6).abs() < 1e-6, "{cap}");
    }

    #[test]
    fn pieces_are_gone_before_the_head_goes_dark() {
        let (track, _) = shot3();
        for (t, p) in [
            (track.clone(), EntryProfile::default()),
            (self::track(), EntryProfile::default()),
            (
                track,
                EntryProfile {
                    shed_count: 64,
                    seed: 3,
                    ..EntryProfile::default()
                },
            ),
        ] {
            let mut time = t.start_time();
            let mut any = false;
            while time <= t.end_time() + 5.0 {
                let frame = evaluate(&t, &p, EntryTier::Full, time);
                any |= !frame.pieces.is_empty();
                if frame.head.glow <= 0.0 {
                    assert!(frame.pieces.is_empty(), "{time} s: a piece outlives the head");
                }
                time += 0.1;
            }
            assert!(any);
        }
    }

    /// The fragments change only the fragments: the head, wake and train are evaluated and drawn
    /// bit for bit as with no pieces at all, and the pieces' ribbons come after them.
    #[test]
    fn pieces_leave_the_head_wake_and_train_untouched() {
        let (track, view) = shot3();
        let with = EntryProfile::default();
        let without = EntryProfile {
            shed_count: 0,
            ..EntryProfile::default()
        };
        let mut drawn = 0;
        for time in [21.0, 23.0, 25.0, 27.0, 28.5, 30.0] {
            let (a, b) = (
                evaluate(&track, &with, EntryTier::Full, time),
                evaluate(&track, &without, EntryTier::Full, time),
            );
            assert!(b.pieces.is_empty());
            assert_eq!((a.head, &a.ion, &a.train), (b.head, &b.ion, &b.train), "{time}");
            let (mut va, mut vb) = (Vec::new(), Vec::new());
            build_vertices(&a, &view, &with, &mut va);
            build_vertices(&b, &view, &without, &mut vb);
            assert_eq!(
                bytemuck::cast_slice::<_, u8>(&va[..vb.len()]),
                bytemuck::cast_slice::<_, u8>(&vb),
                "{time}: the streak itself is unchanged"
            );
            drawn += va.len() - vb.len();
        }
        assert!(drawn > 0, "the pieces draw");
    }

    #[test]
    fn cheap_tier_is_smaller_and_drops_pieces() {
        let (t, p) = (track(), EntryProfile::legacy_dust());
        let full = evaluate(&t, &p, EntryTier::Full, 10.4);
        let cheap = evaluate(&t, &p, EntryTier::Cheap, 10.4);
        assert!(cheap.pieces.is_empty() && !full.pieces.is_empty());
        assert!(cheap.ion.len() < full.ion.len());
        let view = view();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        build_vertices(&full, &view, &p, &mut a);
        build_vertices(&cheap, &view, &p, &mut b);
        assert!(b.len() < a.len());
        let late_full = evaluate(&t, &p, EntryTier::Full, 130.0).train.len();
        let late_cheap = evaluate(&t, &p, EntryTier::Cheap, 130.0).train.len();
        assert!(late_cheap * 3 < late_full);
    }

    #[test]
    fn evaluation_is_deterministic() {
        let (t, p) = (track(), EntryProfile::default());
        for time in [0.0, 4.5, 7.0, 40.0, 130.0] {
            assert_eq!(
                evaluate(&t, &p, EntryTier::Full, time),
                evaluate(&t, &p, EntryTier::Full, time)
            );
        }
        let other = EntryProfile {
            seed: 99,
            ..p.clone()
        };
        // The seed places the shed pieces, so compare after the shed (the 45 km crossing, about
        // 7.7 s on this track): before it both frames have no pieces at all.
        let shed = t
            .first_time_below(p.ground_y, p.shed_altitude_m)
            .expect("crossing");
        let (a, b) = (
            evaluate(&t, &p, EntryTier::Full, shed + 2.0).pieces,
            evaluate(&t, &other, EntryTier::Full, shed + 2.0).pieces,
        );
        assert!(
            !a.is_empty() && !b.is_empty(),
            "{} and {} pieces",
            a.len(),
            b.len()
        );
        assert_ne!(a, b);
    }

    fn view() -> EntryView {
        EntryView {
            camera: Vec3::new(-1_752.0, 353.2, -2_203.0),
            focal_px: 7_165.0,
            min_px: 2.0,
            shutter_s: 1.0 / 60.0,
            far_m: f32::INFINITY,
        }
    }

    #[test]
    fn final_approach_reads_at_about_30_px_at_4k() {
        // Shot 4: camera 3.3 km SW of the site at (700, 105.7, 5), 4K at a 30 degree horizontal fov.
        let site = Vec3::new(700.0, 105.7, 5.0);
        let range = (site - view().camera).length();
        assert!((3_200.0..3_500.0).contains(&range), "{range}");
        let px = apparent_size_px(13.0, range, view().focal_px);
        assert!((20.0..45.0).contains(&px), "{px}");
    }

    #[test]
    fn dark_body_streak_has_no_emission_and_is_visible() {
        let (t, p) = (track(), EntryProfile::default());
        let frame = evaluate(&t, &p, EntryTier::Full, 118.0);
        assert_eq!(frame.head.glow, 0.0);
        let mut v = Vec::new();
        build_vertices(&frame, &view(), &p, &mut v);
        assert!(!v.is_empty(), "the dark body draws");
        // Last two ribbons are the body streak: dark, not emissive (all channels well under 1).
        let streak = &v[v.len() - 6..];
        for vert in streak {
            assert!(vert.color[0] < 0.1 && vert.color[3] > 0.5);
        }
    }

    #[test]
    fn vertices_are_finite_and_camera_facing_and_stable() {
        let (t, p) = (track(), EntryProfile::default());
        for time in [2.0, 5.0, 10.4, 50.0, 125.0] {
            let frame = evaluate(&t, &p, EntryTier::Full, time);
            let (mut a, mut b) = (Vec::new(), Vec::new());
            build_vertices(&frame, &view(), &p, &mut a);
            build_vertices(&frame, &view(), &p, &mut b);
            assert_eq!(a.len() % 6, 0);
            assert!(
                a.iter()
                    .all(|v| v.position.iter().chain(v.color.iter()).all(|x| x.is_finite()))
            );
            assert_eq!(
                bytemuck::cast_slice::<_, u8>(&a),
                bytemuck::cast_slice::<_, u8>(&b)
            );
        }
    }

    #[test]
    fn boom_arrives_late() {
        assert!((boom_delay_s(30_000.0) - 87.5).abs() < 0.1);
    }

    /// A head 190 km away with a 20 km far plane: every vertex ends inside the far plane and the
    /// projection of the head (angle and width in pixels) is unchanged by the fit.
    #[test]
    fn far_points_are_pulled_inside_the_far_plane_without_moving_on_screen() {
        let eye = Vec3::new(-507.7, 51.6, -93.0);
        let profile = EntryProfile::default();
        let track = EntryTrack::new(vec![
            TrackPoint {
                time: 0.0,
                position: Vec3::new(126_100.0, 70_000.0, 149_400.0),
            },
            TrackPoint {
                time: 10.0,
                position: Vec3::new(61_800.0, 45_000.0, 72_800.0),
            },
        ])
        .expect("track");
        let frame = evaluate(&track, &profile, EntryTier::Full, 4.0);
        assert!(frame.head.glow > 0.5);
        let range = (frame.head.position - eye).length();
        assert!(range > 100_000.0, "{range}");
        let open = EntryView {
            camera: eye,
            focal_px: 2_316.0,
            min_px: 2.0,
            shutter_s: 1.0 / 60.0,
            far_m: f32::INFINITY,
        };
        let fitted = EntryView {
            far_m: 20_000.0,
            ..open
        };
        let (mut a, mut b) = (Vec::new(), Vec::new());
        build_vertices(&frame, &open, &profile, &mut a);
        build_vertices(&frame, &fitted, &profile, &mut b);
        assert_eq!(a.len(), b.len());
        assert!(!b.is_empty());
        for (u, f) in a.iter().zip(&b) {
            let (du, df) = (
                Vec3::from_array(u.position) - eye,
                Vec3::from_array(f.position) - eye,
            );
            assert!(
                df.length() <= 20_000.0 + 1.0,
                "{} m is outside the far plane",
                df.length()
            );
            // Same direction from the eye, to float precision at these ranges.
            assert!(du.normalize().dot(df.normalize()) > 0.999_999, "{du:?} vs {df:?}");
        }
    }

    /// The fit is off for an infinite or non-positive far plane.
    #[test]
    fn fit_is_off_without_a_finite_far_plane() {
        let mut points = [RibbonPoint {
            position: Vec3::new(1.0e6, 0.0, 0.0),
            half_width: 3.0,
            color: [1.0; 4],
        }];
        for far_m in [f32::INFINITY, 0.0, -5.0, f32::NAN] {
            let view = EntryView { far_m, ..view() };
            fit_range(&mut points, &view);
            assert_eq!(points[0].position.x, 1.0e6);
            assert_eq!(points[0].half_width, 3.0);
        }
    }

    /// Blue over red, the colour temperature of a linear RGB triple (monotonic in kelvin).
    fn blueness(c: [f32; 3]) -> f32 {
        c[2] / c[0].max(1e-6)
    }

    #[test]
    fn temperature_ramp_runs_red_orange_white_blue_white() {
        let (red, orange, white, blue) = (
            temperature_color(1_500.0),
            temperature_color(3_500.0),
            temperature_color(6_000.0),
            temperature_color(9_500.0),
        );
        assert!(
            red[0] > red[1] * 5.0 && red[0] > red[2] * 10.0,
            "deep red {red:?}"
        );
        assert!(
            orange[0] > orange[1] && orange[1] > orange[2] * 2.5,
            "orange {orange:?}"
        );
        assert!(white.iter().all(|c| *c > 0.8), "white {white:?}");
        assert!(blue[2] > blue[0] && blue[2] > 0.95, "blue-white {blue:?}");
        // Ablation lines: sodium lowers green in the orange band (the untinted stops give 0.59 at
        // 4000 K), magnesium keeps green over red when hot.
        assert!(temperature_color(4_000.0)[1] < 0.55);
        assert!(blue[1] > blue[0]);
        // Out-of-range temperatures clamp to the ends; every channel stays in 0..=1.
        assert_eq!(temperature_color(100.0), temperature_color(1_500.0));
        assert_eq!(temperature_color(1.0e6), temperature_color(10_000.0));
        let mut k = 1_200.0;
        while k < 11_000.0 {
            assert!(
                temperature_color(k).iter().all(|c| (0.0..=1.0).contains(c)),
                "{k} K"
            );
            k += 50.0;
        }
    }

    #[test]
    fn colour_gets_bluer_with_temperature_everywhere() {
        let mut k = 1_500.0;
        let mut last = blueness(temperature_color(k));
        while k < 10_000.0 {
            k += 50.0;
            let now = blueness(temperature_color(k));
            assert!(now >= last - 1e-5, "{k} K: {now} < {last}");
            last = now;
        }
    }

    #[test]
    fn head_temperature_rises_with_speed_and_cooling_never_reheats() {
        let mut last = 0.0;
        for v in (0..=14_000).step_by(250) {
            let t = head_temperature_k(v as f32);
            assert!(t >= last, "{v} m/s");
            assert!((3_000.0..=9_000.0 + 1e-3).contains(&t));
            last = t;
        }
        let start = head_temperature_k(11_000.0);
        let (mut prev_t, mut prev_blue) = (f32::MAX, f32::MAX);
        for i in 0..200 {
            let age = i as f32 * 0.05;
            let t = cooled_temperature_k(start, age, COOLING_TAU_S);
            let blue = blueness(temperature_color(t));
            assert!(t <= prev_t && blue <= prev_blue + 1e-5, "age {age}");
            assert!(t >= AMBIENT_K);
            (prev_t, prev_blue) = (t, blue);
        }
        assert!((cooled_temperature_k(start, 0.0, COOLING_TAU_S) - start).abs() < 1e-3);
        // A body already cooler than ambient does not heat up.
        assert_eq!(cooled_temperature_k(500.0, 3.0, COOLING_TAU_S), AMBIENT_K);
    }

    #[test]
    fn ion_trail_cools_dims_and_widens_away_from_the_head() {
        let (t, p) = (track(), EntryProfile::default());
        let f = evaluate(&t, &p, EntryTier::Full, 5.5);
        assert!(f.ion.len() > 12);
        for w in f.ion.windows(2) {
            let (new, old) = (&w[0], &w[1]);
            assert!(old.age_s > new.age_s);
            assert!(
                old.temperature_k <= new.temperature_k + 1.0,
                "cooling is monotonic"
            );
            assert!(blueness(old.color) <= blueness(new.color) + 1e-3);
        }
        let (first, last) = (f.ion[0], f.ion[f.ion.len() - 1]);
        assert!(
            blueness(first.color) > 0.8,
            "near the head it is white-blue {:?}",
            first.color
        );
        assert!(
            last.color[0] > last.color[2] * 3.0,
            "the tail is orange-red {:?}",
            last.color
        );
        assert!(last.brightness < 0.25 * first.brightness, "brightness falls off");
        assert!(last.half_width_m > first.half_width_m, "the envelope widens");
    }

    #[test]
    fn the_hot_train_glows_for_seconds_and_gusts() {
        let (t, p) = (track(), EntryProfile::default());
        let young = evaluate(&t, &p, EntryTier::Full, 7.0);
        let old = evaluate(&t, &p, EntryTier::Full, 12.0);
        let glow = |f: &EntryFrame| f.train.iter().filter(|x| x.ember > 0.0).count();
        assert!(
            glow(&young) > 0 && glow(&old) > 0,
            "persists past the 1.5 s ion trail"
        );
        assert!(
            glow(&evaluate(&t, &p, EntryTier::Full, 60.0)) == 0,
            "cooled long since"
        );
        let peak = young.train.iter().map(|x| x.ember).fold(0.0, f32::max);
        assert!(peak > 0.0 && peak < 0.1, "a faint share of the head: {peak}");
        // Gusting: the ember is not constant along the train.
        let values: Vec<f32> = young
            .train
            .iter()
            .filter(|x| x.ember > 0.0)
            .map(|x| x.ember)
            .collect();
        let (lo, hi) = values
            .iter()
            .fold((f32::MAX, 0.0f32), |(l, h), v| (l.min(*v), h.max(*v)));
        assert!(hi > 1.2 * lo, "{lo} .. {hi}");
        // Older gas is cooler (redder).
        let oldest = old.train.iter().find(|x| x.ember > 0.0).expect("ember");
        let newest = old.train.iter().rev().find(|x| x.ember > 0.0).expect("ember");
        assert!(blueness(oldest.ember_color) <= blueness(newest.ember_color));
    }

    #[test]
    fn nothing_is_brighter_than_peak_radiance_and_the_streak_is_not_a_flat_bar() {
        let (t, p) = (track(), EntryProfile::default());
        let frame = evaluate(&t, &p, EntryTier::Full, 5.5);
        let mut v = Vec::new();
        build_vertices(&frame, &view(), &p, &mut v);
        let mut brightest = 0.0f32;
        let mut tail_core = f32::MAX;
        for vert in &v {
            // The emission a vertex adds is rgb * alpha (the trail shader multiplies them).
            let e = vert.color[0].max(vert.color[1]).max(vert.color[2]) * vert.color[3];
            assert!(e <= p.peak_radiance * 1.001, "{e}");
            brightest = brightest.max(e);
            if vert.color[3] > 0.5 && e > 0.0 {
                tail_core = tail_core.min(e);
            }
        }
        assert!(
            brightest > 0.5 * p.peak_radiance * frame.head.glow,
            "the head is the peak"
        );
        assert!(
            tail_core < 0.2 * brightest,
            "the core's emission falls along the trail: {tail_core} vs {brightest}"
        );
    }

    #[test]
    fn glow_envelope_grows_from_a_point_as_the_bolide_nears() {
        let p = EntryProfile::default();
        // A level pass at 50 km altitude along +X at 11 km/s; the camera sits `range` m off its path
        // (along Z) at the head's X, so the head is at that range.
        let track = EntryTrack::new(vec![
            TrackPoint {
                time: 0.0,
                position: Vec3::new(0.0, 50_000.0, 0.0),
            },
            TrackPoint {
                time: 10.0,
                position: Vec3::new(110_000.0, 50_000.0, 0.0),
            },
        ])
        .expect("track");
        let frame = evaluate(&track, &p, EntryTier::Full, 1.0);
        let head = frame.head.position;
        let half_px = |range: f32| {
            let v = EntryView {
                camera: head - Vec3::new(0.0, 0.0, range),
                focal_px: 2_316.0,
                min_px: 2.0,
                shutter_s: 1.0 / 60.0,
                far_m: f32::INFINITY,
            };
            let mut out = Vec::new();
            build_vertices(&frame, &v, &p, &mut out);
            // Vertices at the head sample (no offset along the path); their offset is the half-width.
            out.iter()
                .map(|vert| Vec3::from_array(vert.position) - head)
                .filter(|d| d.x.abs() < 1.0)
                .map(|d| d.length())
                .fold(0.0, f32::max)
                * v.focal_px
                / range
        };
        let far = half_px(250_000.0);
        let near = half_px(3_000.0);
        assert!(far > 0.0, "a point still draws at 250 km");
        // Far: the pixel floor sets the envelope (a few tens of px at most), not the metres.
        assert!(far < 40.0, "{far}");
        assert!(
            near > 2.0 * far,
            "the metre-sized envelope takes over up close: {near} vs {far}"
        );
    }

    #[test]
    fn streak_length_follows_speed_and_exposure() {
        let (t, p) = (track(), EntryProfile::default());
        let frame = evaluate(&t, &p, EntryTier::Full, 5.5);
        let len = frame.head.speed * view().shutter_s;
        assert!((100.0..300.0).contains(&len), "{len} m of travel in one exposure");
        let range = (frame.head.position - view().camera).length();
        let px = apparent_size_px(len, range, view().focal_px);
        assert!(px > 0.0 && px.is_finite());
    }

    #[test]
    fn pieces_flicker_deterministically_and_draw_emissive() {
        let (t, p) = (track(), EntryProfile::default());
        let shed = t
            .first_time_below(p.ground_y, p.shed_altitude_m)
            .expect("crossing");
        let glows = |time: f32| -> Vec<f32> {
            evaluate(&t, &p, EntryTier::Full, time)
                .pieces
                .iter()
                .map(|x| x.glow)
                .collect()
        };
        assert_eq!(glows(shed + 2.0), glows(shed + 2.0));
        assert_ne!(glows(shed + 2.0), glows(shed + 2.2), "flicker over time");
        let frame = evaluate(&t, &p, EntryTier::Full, shed + 2.0);
        assert!(frame.pieces.iter().all(|x| x.speed > 0.0));
        let mut v = Vec::new();
        build_vertices(&frame, &view(), &p, &mut v);
        assert!(
            v.iter()
                .all(|x| x.color.iter().all(|c| c.is_finite() && *c >= 0.0))
        );
    }

    /// Showcase shot 3: a near head-on bolide seen from (-507.7, 51.6, -93.0) at 4K, 50 degree
    /// vertical field of view. Track time 31 s is delivered frame 1080, after the head glow ends.
    fn shot3() -> (EntryTrack, EntryView) {
        let at = |time: f32, x: f32, y: f32, z: f32| TrackPoint {
            time,
            position: Vec3::new(x, y, z),
        };
        let track = EntryTrack::new(vec![
            at(0.0, 193_600.0, 90_000.0, 229_800.0),
            at(10.0, 126_100.0, 70_000.0, 149_400.0),
            at(20.0, 61_800.0, 45_000.0, 72_800.0),
            at(28.0, 23_200.0, 25_000.0, 26_800.0),
            at(32.0, 10_300.0, 18_000.0, 11_500.0),
        ])
        .expect("track");
        let view = EntryView {
            camera: Vec3::new(-507.7, 51.6, -93.0),
            focal_px: 2_160.0 / (2.0 * 25f32.to_radians().tan()),
            min_px: 2.0,
            shutter_s: 1.0 / 60.0,
            far_m: f32::INFINITY,
        };
        (track, view)
    }

    #[test]
    fn the_head_reads_warm_white_and_the_wake_never_turns_blue_or_deep_red() {
        for speed in [9_000.0, 11_000.0, 14_000.0] {
            let c = glow_color(head_temperature_k(speed));
            assert!(c.iter().all(|x| *x > 0.8), "{speed} m/s: {c:?}");
            assert!(c[0] >= c[1] && c[1] >= c[2], "warm, not blue: {c:?}");
        }
        let mut k = 500.0;
        while k < 20_000.0 {
            let c = glow_color(k);
            assert!(c[1] >= 0.42 * c[0], "{k} K: {c:?} reads deep red");
            assert!(c[2] <= c[0], "{k} K: {c:?} reads blue");
            k += 100.0;
        }
        assert_eq!(glow_color(f32::NAN), glow_color(VISIBLE_MIN_K));
        let (t, p) = (track(), EntryProfile::default());
        for time in [3.0, 5.5, 10.4, 15.0] {
            let f = evaluate(&t, &p, EntryTier::Full, time);
            for c in f
                .ion
                .iter()
                .map(|x| x.color)
                .chain(f.train.iter().filter(|x| x.ember > 0.0).map(|x| x.ember_color))
                .chain(f.pieces.iter().map(|x| x.color))
            {
                assert!(c[1] >= 0.42 * c[0] && c[2] <= c[0], "{time} s: {c:?}");
            }
        }
    }

    #[test]
    fn the_glowing_wake_is_short() {
        let life = EntryProfile::default().ion_life_s;
        assert!((ion_decay(0.0, life) - 1.0).abs() < 1e-6);
        // Within a tenth of a second the wake is under half the head, by two thirds of the life
        // under a twentieth: a short wake, not a rod.
        assert!(ion_decay(0.1, life) < 0.5, "{}", ion_decay(0.1, life));
        assert!(ion_decay(life * 2.0 / 3.0, life) < 0.05);
        let mut last = 1.0;
        for i in 1..=60 {
            let now = ion_decay(i as f32 * life / 60.0, life);
            assert!(now < last);
            last = now;
        }
    }

    #[test]
    fn nothing_glows_a_few_seconds_after_the_head_goes_dark() {
        let (t, p) = (track(), EntryProfile::legacy_dust());
        let dark = t
            .first_time_below(p.ground_y, GLOW_CEILING_M)
            .expect("crosses the ceiling");
        // 20 s: the shed pieces are dark too (`head_is_bright_aloft_and_dark_below_20_km`).
        let after = evaluate(&t, &p, EntryTier::Full, (dark + EMBER_LIFE_S + 0.1).max(20.0));
        assert_eq!(after.head.glow, 0.0);
        assert!(after.ion.is_empty());
        assert!(
            after.train.iter().all(|x| x.ember == 0.0),
            "the afterglow is gone"
        );
        let mut v = Vec::new();
        build_vertices(&after, &view(), &p, &mut v);
        assert!(
            v.iter().all(|x| x._pad == 0.0),
            "no additive (glowing) ribbon is left"
        );
        // The dust train is still there, and it does not glow.
        assert!(
            after
                .train
                .iter()
                .any(|x| x.color == DUST_COLOR_LEGACY && x.opacity > 0.1)
        );
    }

    #[test]
    fn glowing_ribbons_are_soft_and_additive_dust_is_soft_and_the_dark_body_is_plain() {
        let (t, p) = (track(), EntryProfile::default());
        let frame = evaluate(&t, &p, EntryTier::Full, 10.4);
        let mut v = Vec::new();
        build_vertices(&frame, &view(), &p, &mut v);
        let glowing: Vec<_> = v.iter().filter(|x| x._pad == 1.0).collect();
        let over: Vec<_> = v.iter().filter(|x| x._pad == 0.0).collect();
        assert!(!glowing.is_empty() && !over.is_empty());
        assert_eq!(glowing.len() + over.len(), v.len(), "additive is 0 or 1");
        assert!(v.iter().all(|x| x._pad2 == [1.0, 0.0]), "every ribbon is soft");
        // Dust hides the sky, it never emits: its colour is the lit dust colour.
        assert!(over.iter().all(|x| x.color[..3] == DUST_COLOR));
        let dark = evaluate(&t, &p, EntryTier::Full, 118.0);
        let mut d = Vec::new();
        build_vertices(&dark, &view(), &p, &mut d);
        let body = &d[d.len() - 6..];
        assert!(body.iter().all(|x| x._pad == 0.0 && x._pad2 == [0.0, 0.0]));
    }

    #[test]
    fn the_head_is_a_round_soft_blob_with_no_outline() {
        let (t, p) = (track(), EntryProfile::default());
        let frame = evaluate(&t, &p, EntryTier::Full, 10.4);
        let head = frame.head;
        let mut v = Vec::new();
        let mut strip = Vec::new();
        push_head(&head, &view(), &p, &mut v, &mut strip);
        assert_eq!(v.len(), HEAD_LAYERS.len() * (BLOB.len() - 1) * 6);
        // Both ends of every layer dissolve (zero alpha), the middle is full.
        for layer in v.chunks(6 * (BLOB.len() - 1)) {
            let (first, last) = (&layer[0], &layer[layer.len() - 1]);
            assert_eq!(first.color[3], 0.0);
            assert_eq!(last.color[3], 0.0);
            assert!(layer.iter().any(|x| x.color[3] == 1.0));
        }
        // Each layer is about as long as it is wide plus the on-screen part of one exposure of
        // travel (a round blob, not a rod), measured in the plane facing the camera.
        let (axis, seen) = screen_axis(head.position, head.direction, view().camera);
        assert!(seen > 0.0 && seen <= 1.0);
        let core = &v[v.len() - 6 * (BLOB.len() - 1)..];
        let (length, width) = extents(core, head.position, axis);
        let travel = head.speed * view().shutter_s * seen;
        assert!(
            length <= width + travel + 1.0 && length >= 0.9 * width,
            "{length} m long, {width} m wide, {travel} m of travel on screen"
        );
    }

    /// Extent of `verts` along `axis` and across it (perpendicular, in the plane of the ribbon).
    fn extents(verts: &[TrailVertex], center: Vec3, axis: Vec3) -> (f32, f32) {
        let offsets: Vec<Vec3> = verts
            .iter()
            .map(|x| Vec3::from_array(x.position) - center)
            .collect();
        let along: Vec<f32> = offsets.iter().map(|d| d.dot(axis)).collect();
        let length =
            along.iter().copied().fold(f32::MIN, f32::max) - along.iter().copied().fold(f32::MAX, f32::min);
        let width = offsets
            .iter()
            .map(|d| (*d - axis * d.dot(axis)).length())
            .fold(0.0, f32::max)
            * 2.0;
        (length, width)
    }

    /// Showcase shot 3 sees the bolide almost head-on; a camera exactly on the track still gets a
    /// round head (the old direction-aligned ribbon degenerated to nothing there) and round pieces.
    #[test]
    fn a_head_on_bolide_is_still_a_round_blob() {
        let p = EntryProfile::default();
        let track = EntryTrack::new(vec![
            TrackPoint {
                time: 0.0,
                position: Vec3::new(0.0, 60_000.0, 0.0),
            },
            TrackPoint {
                time: 10.0,
                position: Vec3::new(0.0, 60_000.0, 110_000.0),
            },
        ])
        .expect("track");
        let frame = evaluate(&track, &p, EntryTier::Full, 2.0);
        let head = frame.head;
        assert!(head.glow > 0.5);
        let view = EntryView {
            camera: head.position + head.direction * 80_000.0,
            ..view()
        };
        let (axis, seen) = screen_axis(head.position, head.direction, view.camera);
        assert!(seen < 1e-3 && axis != Vec3::ZERO);
        assert!(
            axis.dot(head.direction).abs() < 1e-3,
            "the axis lies in the screen plane"
        );
        let mut v = Vec::new();
        let mut strip = Vec::new();
        push_head(&head, &view, &p, &mut v, &mut strip);
        assert_eq!(
            v.len(),
            HEAD_LAYERS.len() * (BLOB.len() - 1) * 6,
            "the head draws"
        );
        let core = &v[v.len() - 6 * (BLOB.len() - 1)..];
        let (length, width) = extents(core, head.position, axis);
        assert!(
            (0.9 * width..=1.1 * width).contains(&length),
            "round: {length} x {width}"
        );
    }

    #[test]
    fn the_wake_tapers_as_it_dims() {
        let (t, p) = (track(), EntryProfile::default());
        let frame = evaluate(&t, &p, EntryTier::Full, 5.5);
        let v = view();
        // The wake's core half-width in pixels at the newest and the oldest sample.
        let px = |x: &IonPoint| {
            let sized = EntryView {
                min_px: v.min_px * (0.35 + 0.65 * x.brightness.min(1.0).sqrt()),
                ..v
            };
            let range = (x.position - v.camera).length();
            min_half_width(x.position, &sized, 0.35 * x.half_width_m) * v.focal_px / range
        };
        let (newest, oldest) = (&frame.ion[0], &frame.ion[frame.ion.len() - 1]);
        assert!(
            px(oldest) < 0.7 * px(newest),
            "{} px at the tail vs {} px at the head",
            px(oldest),
            px(newest)
        );
    }

    #[test]
    fn the_dust_train_is_thrown_wide_then_spreads_thins_and_drifts() {
        let (t, p) = (track(), EntryProfile::legacy_dust());
        let f = evaluate(&t, &p, EntryTier::Full, 10.0);
        let dust: Vec<&TrainPoint> = f.train.iter().filter(|x| x.color == DUST_COLOR_LEGACY).collect();
        let mean = |lo: f32, hi: f32, get: &dyn Fn(&TrainPoint) -> f32| {
            let pick: Vec<f32> = dust
                .iter()
                .copied()
                .filter(|x| x.age_s >= lo && x.age_s < hi)
                .map(get)
                .collect();
            assert!(!pick.is_empty(), "{lo}..{hi}");
            pick.iter().sum::<f32>() / pick.len() as f32
        };
        let width = |x: &TrainPoint| x.half_width_m;
        // Thrown out to a hundred metres or more within two seconds (was 15 + 22 sqrt(age)), even
        // where it billows narrowest (0.7).
        assert!(mean(1.5, 3.0, &width) > 100.0, "{}", mean(1.5, 3.0, &width));
        let envelope = |age: f32| dust_half_width(11_000.0, age, p.body_radius_m);
        assert!(envelope(2.0) > 150.0 && envelope(0.0) < 20.0, "{}", envelope(2.0));
        let mut last = 0.0;
        for i in 0..=240 {
            let now = envelope(i as f32 * 0.5);
            assert!(now >= last, "never shrinks");
            last = now;
        }
        assert!(envelope(120.0) > 500.0, "minutes later it is a kilometre wide");
        // A slower body throws less.
        assert!(dust_half_width(5_000.0, 2.0, p.body_radius_m) < envelope(2.0));
        // It emerges from behind the wake: the newest dust is fainter than dust a second old.
        let newest = dust.iter().map(|x| x.age_s).fold(f32::MAX, f32::min);
        let opacity = |x: &TrainPoint| x.opacity;
        assert!(newest < 0.5);
        assert!(mean(0.0, 0.3, &opacity) < mean(1.0, 2.0, &opacity));
        // Older, wider dust is thinner.
        let late = evaluate(&t, &p, EntryTier::Full, 130.0);
        let late_peak = late
            .train
            .iter()
            .filter(|x| x.color == DUST_COLOR_LEGACY)
            .map(|x| x.opacity)
            .fold(0.0, f32::max);
        assert!(late_peak < mean(1.0, 3.0, &opacity), "{late_peak}");
        // The train meanders smoothly: neighbouring points move by far less than the width.
        for w in dust.windows(2) {
            let (a, b) = (w[0], w[1]);
            let lateral = (a.position - p.wind * a.age_s) - (b.position - p.wind * b.age_s);
            let track_step = t.position_at(f.time_s - a.age_s) - t.position_at(f.time_s - b.age_s);
            let wobble = (lateral - track_step).length();
            assert!(wobble < 0.5 * a.half_width_m.max(b.half_width_m), "{wobble}");
        }
    }

    #[test]
    fn shot_3_after_the_glow_shows_a_wide_soft_train_and_no_red_rod() {
        let (track, view) = shot3();
        let p = EntryProfile::legacy_dust();
        let frame = evaluate(&track, &p, EntryTier::Full, 31.0);
        assert_eq!(frame.head.glow, 0.0, "f1080: the head glow has ended");
        // Afterglow only in the last few seconds of gas: none older than EMBER_LIFE_S.
        assert!(
            frame
                .train
                .iter()
                .all(|x| x.ember == 0.0 || x.age_s < EMBER_LIFE_S)
        );
        let glowing = frame.train.iter().filter(|x| x.ember > 0.0).count();
        assert!(glowing <= (EMBER_LIFE_S / 0.25) as usize + 1, "{glowing}");
        // The dust 5-15 s old is several pixels wide on screen (it was a 1 px line).
        let px = |x: &TrainPoint| x.half_width_m * view.focal_px / (x.position - view.camera).length();
        let widths: Vec<f32> = frame
            .train
            .iter()
            .filter(|x| x.color == DUST_COLOR_LEGACY && (5.0..15.0).contains(&x.age_s))
            .map(px)
            .collect();
        assert!(!widths.is_empty());
        let mean = widths.iter().sum::<f32>() / widths.len() as f32;
        assert!(mean > 3.0, "{mean} px half-width");
        // And it reads: a pale grey-brown at a useful opacity.
        let strongest = frame
            .train
            .iter()
            .filter(|x| x.color == DUST_COLOR_LEGACY)
            .map(|x| x.opacity)
            .fold(0.0, f32::max);
        assert!(strongest > 0.25, "{strongest}");
    }

    #[test]
    fn dust_knobs_leave_the_head_wake_and_pieces_untouched() {
        let (t, new, old) = (track(), EntryProfile::default(), EntryProfile::legacy_dust());
        for time in [5.5, 10.4, 14.0] {
            let (a, b) = (
                evaluate(&t, &new, EntryTier::Full, time),
                evaluate(&t, &old, EntryTier::Full, time),
            );
            assert_eq!(a.head, b.head, "{time}: head");
            assert_eq!(a.ion, b.ion, "{time}: wake");
            assert_eq!(a.pieces, b.pieces, "{time}: pieces");
            // The glowing ribbons (head, wake, pieces) are bit-identical; only the dust differs.
            let (mut va, mut vb) = (Vec::new(), Vec::new());
            build_vertices(&a, &view(), &new, &mut va);
            build_vertices(&b, &view(), &old, &mut vb);
            let glow = |v: &[TrailVertex]| -> Vec<[u32; 4]> {
                v.iter()
                    .filter(|x| x._pad == 1.0)
                    .map(|x| x.color.map(f32::to_bits))
                    .collect()
            };
            assert_eq!(glow(&va), glow(&vb), "{time}: additive ribbons");
        }
    }

    #[test]
    fn dust_defaults_are_about_half_opacity_warm_grey_and_shorter_lived() {
        let (t, new, old) = (track(), EntryProfile::default(), EntryProfile::legacy_dust());
        assert_eq!(new.dust_opacity_scale, DUST_OPACITY_SCALE);
        assert_eq!(new.dust_albedo, DUST_COLOR);
        assert_eq!(new.dust_life_s, DUST_LIFE_S);
        assert_eq!(old.dust_opacity_scale, 1.0);
        assert_eq!(old.dust_albedo, DUST_COLOR_LEGACY);
        assert_eq!(old.dust_life_s, DUST_LIFE_LEGACY_S);
        // Warm: red over green over blue; darker than the old tint; not white.
        const { assert!(DUST_COLOR[0] > DUST_COLOR[1] && DUST_COLOR[1] > DUST_COLOR[2]) };
        assert!(
            DUST_COLOR
                .iter()
                .zip(DUST_COLOR_LEGACY)
                .all(|(n, o)| *n < 0.6 * o)
        );
        let (a, b) = (
            evaluate(&t, &new, EntryTier::Full, 14.0),
            evaluate(&t, &old, EntryTier::Full, 14.0),
        );
        let mut compared = 0;
        for x in a.train.iter().filter(|x| x.color == DUST_COLOR) {
            let y = b
                .train
                .iter()
                .find(|y| y.age_s == x.age_s && y.color == DUST_COLOR_LEGACY)
                .expect("the same sample in the legacy train");
            assert_eq!(x.color, DUST_COLOR);
            assert_eq!(x.position, y.position, "same geometry");
            let fade = (1.0 - x.age_s / new.dust_life_s) / (1.0 - y.age_s / old.dust_life_s);
            let want = y.opacity * 0.5 * fade;
            assert!(
                (x.opacity - want).abs() <= 1e-4 * want.max(1.0),
                "{} vs {want}",
                x.opacity
            );
            compared += 1;
        }
        assert!(compared > 20, "{compared}");
    }

    #[test]
    fn dust_is_gone_after_its_life_and_the_life_is_a_knob() {
        let t = track();
        for life in [8.0f32, DUST_LIFE_S] {
            let p = EntryProfile {
                dust_life_s: life,
                ..EntryProfile::default()
            };
            let f = evaluate(&t, &p, EntryTier::Full, 17.0);
            assert!(f.train.iter().any(|x| x.color == DUST_COLOR), "{life}");
            assert!(
                f.train
                    .iter()
                    .filter(|x| x.color == DUST_COLOR)
                    .all(|x| x.age_s < life),
                "{life}: no dust older than its life"
            );
        }
        let p = EntryProfile {
            dust_opacity_scale: 0.0,
            ..EntryProfile::default()
        };
        assert!(
            evaluate(&t, &p, EntryTier::Full, 14.0)
                .train
                .iter()
                .all(|x| x.color != DUST_COLOR),
            "a zero scale draws no dust"
        );
    }

    /// Glowing dust is gone after `dust_life_s` (24 s). Lengthening `train_life_s` does not keep it.
    #[test]
    fn glowing_dust_fades_on_dust_life_not_train_life() {
        let t = track();
        let early = evaluate(&t, &EntryProfile::default(), EntryTier::Full, 20.0);
        assert!(early.train.iter().any(|x| x.color == DUST_COLOR));
        let late = evaluate(&t, &EntryProfile::default(), EntryTier::Full, 120.0);
        assert!(late.train.iter().all(|x| x.color != DUST_COLOR));
        let longer = EntryProfile {
            train_life_s: 400.0,
            ..EntryProfile::default()
        };
        let still = evaluate(&t, &longer, EntryTier::Full, 120.0);
        assert!(still.train.iter().all(|x| x.color != DUST_COLOR));
    }

    /// Pixels the ribbons shade (overdraw counted) for a camera at `camera` with `focal_px`: each
    /// triangle's area seen from the camera, scaled to pixels at its centroid's range.
    fn shaded_px(verts: &[TrailVertex], camera: Vec3, focal_px: f32) -> f32 {
        let tris: &[[TrailVertex; 3]] = verts.as_chunks::<3>().0;
        tris.iter()
            .map(|t| {
                let [a, b, c] = t.map(|v| Vec3::from_array(v.position));
                let centroid = (a + b + c) / 3.0;
                let range = (centroid - camera).length().max(1.0);
                let facing = (b - a).cross(c - a).dot((centroid - camera) / range).abs() * 0.5;
                facing * (focal_px / range).powi(2)
            })
            .sum()
    }

    /// Cost bound for the frame budget: across shot 3 (track time 13 to 33 s, 4K) the streak never
    /// shades more than 0.5 % of the frame's pixels, overdraw counted, through a shader of a few
    /// arithmetic operations and one blend.
    #[test]
    fn shot_3_streak_shades_a_small_share_of_a_4k_frame() {
        let (track, view) = shot3();
        let p = EntryProfile::default();
        let frame_px = 3_840.0 * 2_160.0;
        let mut worst = (0.0f32, 0.0f32);
        let mut time = 13.0;
        while time <= 33.0 {
            let frame = evaluate(&track, &p, EntryTier::Full, time);
            let mut v = Vec::new();
            build_vertices(&frame, &view, &p, &mut v);
            let px = shaded_px(&v, view.camera, view.focal_px);
            if px > worst.1 {
                worst = (time, px);
            }
            time += 0.5;
        }
        eprintln!(
            "shot 3: at most {:.0} px shaded ({:.3} % of 4K) at {} s",
            worst.1,
            100.0 * worst.1 / frame_px,
            worst.0
        );
        assert!(worst.1 > 0.0);
        assert!(worst.1 < 0.005 * frame_px, "{} px", worst.1);
    }
}
