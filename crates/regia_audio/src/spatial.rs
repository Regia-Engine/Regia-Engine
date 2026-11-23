// SPDX-License-Identifier: MPL-2.0
//! Spatial audio math and components (`docs/specs/16-audio.md` § 3).
//!
//! Regia Engine computes gain, pan, air-absorption cutoff and doppler on the game thread once per
//! `AudioSync` and pushes them into kira as short tweens, rather than using kira's listener
//! mapping: the spec asks for custom rolloff curves, cones and per-distance filters, which a
//! single linear `Value` mapping cannot express. All functions here are pure and unit-tested.
//!
//! Sources of the formulas: attenuation models from the public OpenAL 1.1 specification
//! (§ "Distance Model"); equal-power panning gains as used by kira's
//! `Frame::panned` (sqrt law × √2); doppler from OpenAL 1.1 § "Doppler Shift" (simplified for a
//! static listener, scaled by the emitter's factor); air absorption is a practical linear-in-
//! distance cutoff ramp — the qualitative behaviour of ISO 9613-1 (high frequencies attenuate
//! more with distance) without its per-band coefficients.

use regia_asset::Handle;
use regia_core::math::{Vec3, lerp, remap};
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::cue::SoundCue;
use crate::sound::SoundAsset;

/// Speed of sound in air at ~20 °C, m/s (OpenAL's default).
pub const SPEED_OF_SOUND: f32 = 343.3;

/// How gain falls with distance.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
#[reflect(Default)]
pub enum RolloffCurve {
    /// OpenAL's inverse distance model below the cap: `g = min_d / max(min_d, d)`, but unlike
    /// OpenAL's clamped model the gain is a hard 0 past `max_d` (it does not hold at the cap).
    #[default]
    Inverse,
    /// OpenAL's linear distance model: 1 at `min_d`, 0 at `max_d`.
    Linear,
    /// Logarithmic rolloff: −`db_per_doubling` dB per doubling of distance past `min_d` (the
    /// classic "log" curve), hard 0 past `max_d`.
    Log {
        /// Attenuation in dB per doubling of distance.
        db_per_doubling: f32,
    },
    /// Custom polyline `(distance, amplitude)` pairs, linearly interpolated and clamped at both
    /// ends.
    Custom(Vec<(f32, f32)>),
}

/// Marks an entity as a listener: the ears of a local player. Its [`GlobalTransform`]
/// (world space, so a listener parented to a camera rig or a vehicle sits where it is drawn) is
/// the listener frame; its velocity is derived from its motion. With several listeners
/// (split-screen) each emitter is heard by the nearest one (spec § 3). With no `AudioListener`
/// in the world the ears follow the active camera ([`AudioListenerFollow`]); with no camera either
/// they sit at the origin facing −Z.
///
/// [`GlobalTransform`]: regia_scene::GlobalTransform
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct AudioListener {
    /// Binaural rendering for headphones (head-related transfer function, [`crate::hrtf`]):
    /// emitters heard by this listener get interaural time and level differences and pinna
    /// cues instead of plain stereo panning. Off by default (speakers); opt in per listener,
    /// typically from the player's "headphones" audio setting.
    pub hrtf: bool,
}

/// Where the ears go when no entity has an [`AudioListener`] (spec § 3). Any `AudioListener` in
/// the world takes over from this, whatever its value. Insert this resource before or after adding
/// the plugin to override the default; it is read every frame.
#[derive(Resource, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Resource, Default, Debug)]
pub enum AudioListenerFollow {
    /// Follow the active camera ([`AudioActiveCamera`]).
    #[default]
    ActiveCamera,
    /// Stay at the origin facing −Z.
    Off,
}

/// A camera's place in the world, as the audio sync reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioCameraPose {
    /// The camera entity (the listener's identity: its body and ancestors do not occlude).
    pub entity: Entity,
    /// World-space position.
    pub position: Vec3,
    /// World-space rotation.
    pub rotation: regia_core::math::Quat,
}

/// The active camera for [`AudioListenerFollow::ActiveCamera`]. `regia_audio` knows nothing about
/// the renderer, so `regia::DefaultPlugins` copies the camera that draws first into this every
/// frame before the audio sync (`regia::audio_listener`). A host with its own camera type can
/// write it too.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct AudioActiveCamera(pub Option<AudioCameraPose>);

/// An entity that emits sound (spec § 3). With a `sound` (or a `sound_name` registered in
/// [`crate::SoundAssets`]) and `autoplay`, spawning the entity plays it — no code needed — and
/// despawning it (or removing the component) stops it. An emitter may instead name a `cue` (or
/// `cue_name` registered in [`crate::CueAssets`]), in which case one of the cue's variations
/// plays when the emitter starts (looped when `looping`, once otherwise; a cue change picks
/// again). Gameplay can also start voices explicitly with
/// [`crate::Audio::start_emitter`].
///
/// Placement comes from the entity's `GlobalTransform` every frame: position, facing (−Z, for
/// the cone) and velocity (Doppler, from the real frame delta).
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct AudioEmitter {
    /// The sound to play (runtime handle into [`crate::SoundAssets`]). Takes precedence over
    /// `sound_name` and over `cue`/`cue_name`. Not reflected: scenes and the agent protocol use
    /// `sound_name`.
    #[reflect(ignore)]
    pub sound: Option<Handle<SoundAsset>>,
    /// Name of the sound in [`crate::SoundAssets`] (the serializable / agent-facing key); used
    /// when `sound` is `None`. Empty = no sound.
    #[reflect(@regia_ecs::attrs::AssetFilter("audio"))]
    pub sound_name: String,
    /// The cue to play (runtime handle into [`crate::CueAssets`]): the emitter picks one of the
    /// cue's variations when it starts (or the cue changes) instead of one fixed sound; `looping`
    /// loops that variation, `looping: false` plays it once. Used only when `sound`
    /// and `sound_name` are both unset; takes precedence over `cue_name`. Not reflected: scenes
    /// and the agent protocol use `cue_name` (spec 16 § 3).
    #[reflect(ignore)]
    pub cue: Option<Handle<SoundCue>>,
    /// Name of the cue in [`crate::CueAssets`] (the serializable / agent-facing key); used when
    /// `sound`/`sound_name` are unset and `cue` is `None`. Empty = no cue.
    pub cue_name: String,
    /// Start playing as soon as the emitter exists (and restart when the sound changes).
    pub autoplay: bool,
    /// Loop the sound (ambience, engines); `false` plays it once.
    pub looping: bool,
    /// Voice volume in dB before spatial attenuation.
    pub volume_db: f32,
    /// Full volume inside this distance, metres.
    pub min_distance: f32,
    /// Silence (and virtualisation) beyond this distance, metres.
    pub max_distance: f32,
    /// How gain falls between the two.
    pub rolloff: RolloffCurve,
    /// The inverse and log curves still hold some level at `max_distance`, so the sound cuts out
    /// there. With a fade start, the fraction of `max_distance` (0..1) where a smooth fade to
    /// zero begins, the curve ends in silence. `None` keeps the cut; the linear and custom curves
    /// ignore it.
    pub fade_start: Option<f32>,
    /// 0 = fully 2D (no distance gain, no pan), 1 = fully 3D.
    pub spatial_blend: f32,
    /// Cone inner half-angle in degrees: full volume inside.
    pub cone_inner_degrees: f32,
    /// Cone outer half-angle in degrees: `cone_outer_gain` beyond.
    pub cone_outer_degrees: f32,
    /// Volume multiplier beyond the outer cone angle.
    pub cone_outer_gain: f32,
    /// Air absorption: low-pass the sound more as it gets farther.
    pub air_absorption: bool,
    /// Doppler effect strength (0 = off, 1 = physical).
    pub doppler_factor: f32,
    /// Occlusion (spec § 3, Tier 2): geometry between the listener and the emitter muffles
    /// (low-pass) and quietens it. Needs an occlusion provider — `regia::DefaultPlugins`
    /// installs the physics one; see [`crate::AudioOcclusion`].
    pub occlusion: bool,
    /// Bus the emitter's voices play on; `None` = the `World` bus (spec § 3 default).
    pub bus: Option<String>,
}

impl Default for AudioEmitter {
    fn default() -> Self {
        Self {
            sound: None,
            sound_name: String::new(),
            cue: None,
            cue_name: String::new(),
            autoplay: true,
            looping: true,
            volume_db: 0.0,
            min_distance: 1.0,
            max_distance: 50.0,
            rolloff: RolloffCurve::default(),
            fade_start: None,
            spatial_blend: 1.0,
            cone_inner_degrees: 360.0,
            cone_outer_degrees: 360.0,
            cone_outer_gain: 1.0,
            air_absorption: false,
            doppler_factor: 0.0,
            occlusion: true,
            bus: None,
        }
    }
}

impl AudioEmitter {
    /// An autoplaying, looping emitter of `sound` with the default attenuation.
    pub fn new(sound: Handle<SoundAsset>) -> Self {
        Self {
            sound: Some(sound),
            ..Self::default()
        }
    }

    /// True when the emitter names a sound (handle or name).
    pub fn has_sound(&self) -> bool {
        self.sound.is_some() || !self.sound_name.is_empty()
    }

    /// True when the emitter names a cue (handle or name) and no sound: the cue's variations
    /// play instead of one fixed sound.
    pub fn has_cue(&self) -> bool {
        !self.has_sound() && (self.cue.is_some() || !self.cue_name.is_empty())
    }
}

/// The listener's frame for one sync step. The default is a listener at the origin facing −Z.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListenerFrame {
    /// Listener position, world space.
    pub position: Vec3,
    /// Listener forward (unit; −Z of its transform).
    pub forward: Vec3,
    /// Listener right (unit; +X of its transform).
    pub right: Vec3,
    /// Listener velocity, m/s (Doppler: a listener moving toward an emitter hears it higher).
    pub velocity: Vec3,
    /// Listener up (unit; +Y of its transform). Elevation for HRTF.
    pub up: Vec3,
    /// Binaural rendering for emitters this listener hears ([`AudioListener::hrtf`]).
    pub hrtf: bool,
}

impl Default for ListenerFrame {
    fn default() -> Self {
        Self::looking_at(Vec3::ZERO, Vec3::NEG_Z)
    }
}

impl ListenerFrame {
    /// A listener at `position` facing `forward` (level: right is derived against +Y).
    pub fn looking_at(position: Vec3, forward: Vec3) -> Self {
        let forward = forward.normalize_or_zero();
        let right = forward.cross(Vec3::Y).normalize_or_zero();
        // At exactly ±90° pitch the cross product degenerates; fall back to +X.
        let right = if right.length_squared() < 1e-8 {
            Vec3::X
        } else {
            right
        };
        Self {
            position,
            forward,
            right,
            velocity: Vec3::ZERO,
            up: right.cross(forward).normalize_or_zero(),
            hrtf: false,
        }
    }

    /// The listener frame of a world-space rotation and position (−Z forward, +X right, +Y up).
    pub fn from_rotation(position: Vec3, rotation: regia_core::math::Quat) -> Self {
        Self {
            position,
            forward: rotation * Vec3::NEG_Z,
            right: rotation * Vec3::X,
            up: rotation * Vec3::Y,
            velocity: Vec3::ZERO,
            hrtf: false,
        }
    }

    /// Unit direction of `point` in this listener's head frame (+X right, +Y up, −Z ahead); zero
    /// when `point` is at the listener.
    pub fn head_direction(&self, point: Vec3) -> Vec3 {
        let dir = (point - self.position).normalize_or_zero();
        Vec3::new(dir.dot(self.right), dir.dot(self.up), -dir.dot(self.forward))
    }

    /// Direction of `point` in this listener's head frame: `(azimuth, elevation)` in radians.
    /// Azimuth is 0 straight ahead, positive to the right (+π/2 = right ear); elevation is
    /// positive above the horizontal plane.
    pub fn direction_of(&self, point: Vec3) -> (f32, f32) {
        let dir = (point - self.position).normalize_or_zero();
        if dir == Vec3::ZERO {
            return (0.0, 0.0);
        }
        let x = dir.dot(self.right);
        let y = dir.dot(self.up);
        let z = dir.dot(self.forward);
        (x.atan2(z), y.clamp(-1.0, 1.0).asin())
    }
}

/// The emitter's frame for one sync step.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EmitterFrame {
    /// Emitter position, world space.
    pub position: Vec3,
    /// Emitter velocity, m/s (doppler).
    pub velocity: Vec3,
    /// Emitter facing direction (cone), unit or zero for omnidirectional.
    pub forward: Vec3,
}

/// The result of spatialising one emitter against one listener.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpatialFrame {
    /// Distance gain 0..1 (includes the cone).
    pub gain: f32,
    /// Stereo pan −1..1 (already blended by `spatial_blend`).
    pub pan: f32,
    /// Low-pass cutoff in Hz when air absorption is on.
    pub cutoff_hz: Option<f64>,
    /// Playback-rate ratio from doppler (1 = no shift).
    pub pitch_ratio: f32,
    /// Distance in metres (for virtualisation tests).
    pub distance: f32,
    /// Unit direction of the emitter in the listener's head frame (+X right, +Y up, −Z ahead);
    /// zero when the emitter sits on the listener. Drives binaural (HRTF) rendering.
    pub head_direction: Vec3,
}

/// Distance gain of a rolloff curve (no cone, no blend).
pub fn distance_gain(curve: &RolloffCurve, min_distance: f32, max_distance: f32, distance: f32) -> f32 {
    // A negative or NaN `min_distance` counts as 0 ("full volume at the source"); `max` drops NaN.
    let min_distance = min_distance.max(0.0);
    let d = distance.max(0.0);
    if d > max_distance {
        return 0.0;
    }
    let d = d.max(min_distance);
    // The inverse and log curves divide by the min distance: a zero or negative one would give
    // NaN or silence, so they use a small floor. The linear and custom curves keep the real one.
    let floor = min_distance.max(1e-4);
    match curve {
        RolloffCurve::Inverse => floor / d.max(floor),
        RolloffCurve::Linear => {
            let span = (max_distance - min_distance).max(1e-4);
            1.0 - ((d - min_distance) / span).clamp(0.0, 1.0)
        }
        RolloffCurve::Log { db_per_doubling } => {
            let doublings = (d.max(floor) / floor).log2();
            let db = -db_per_doubling * doublings;
            db_to_gain(db)
        }
        RolloffCurve::Custom(points) => polyline(points, d),
    }
}

/// [`distance_gain`] with the end fade of [`AudioEmitter::fade_start`]: the inverse and log curves
/// are scaled by a smoothstep from 1 at `fade_start × max_distance` (never nearer than
/// `min_distance`) down to 0 at `max_distance`. The result is continuous, with a continuous slope
/// of the fade, at both ends. `None`, a value outside `0..1` and NaN leave the curve as it is.
/// A NaN `max_distance` is silence; an infinite one plays the curve with no fade.
pub fn distance_gain_faded(
    curve: &RolloffCurve,
    min_distance: f32,
    max_distance: f32,
    distance: f32,
    fade_start: Option<f32>,
) -> f32 {
    // A NaN `max_distance` (a scene-file value) would divide by NaN below where the unfaded
    // path stays finite: validate up front and return silence, the engine's policy for broken
    // spatial input. An infinite one means "no cut-off": the curve plays as it is, and there is
    // no end to fade towards, so only the fade is skipped.
    if max_distance.is_nan() {
        return 0.0;
    }
    let gain = distance_gain(curve, min_distance, max_distance, distance);
    if max_distance.is_infinite() {
        return gain;
    }
    let fades = matches!(curve, RolloffCurve::Inverse | RolloffCurve::Log { .. });
    let Some(fraction) = fade_start.filter(|f| fades && (0.0..1.0).contains(f)) else {
        return gain;
    };
    let start = (fraction * max_distance).max(min_distance.max(0.0));
    if start >= max_distance {
        return gain;
    }
    let left = (max_distance - distance.max(0.0)) / (max_distance - start);
    let t = left.clamp(0.0, 1.0);
    gain * t * t * (3.0 - 2.0 * t)
}

/// Converts dB to an amplitude ratio (clamped at −100 dB ≈ silence).
pub fn db_to_gain(db: f32) -> f32 {
    // `is_nan` first, like `gain_to_db`: NaN must never reach a mixer volume, and the
    // upper clamp mirrors `gain_to_db`'s floor — a +∞ or absurd scene-file dB becomes a
    // large but finite gain, not an infinite one.
    if db.is_nan() || db <= -100.0 {
        0.0
    } else {
        10.0f32.powf(db.min(120.0) / 20.0)
    }
}

/// Converts an amplitude ratio to dB (0 maps to −∞-ish floor −100).
pub fn gain_to_db(gain: f32) -> f32 {
    // `!(gain > floor)` also catches NaN, which must never reach a mixer volume.
    if gain.is_nan() || gain <= 1e-5 {
        -100.0
    } else {
        20.0 * gain.log10()
    }
}

/// Evaluates a clamped polyline with linear interpolation.
pub fn polyline(points: &[(f32, f32)], x: f32) -> f32 {
    match points.first() {
        None => 0.0,
        Some(&(first_x, first_y)) => {
            if x <= first_x {
                return first_y;
            }
            for pair in points.windows(2) {
                let (x0, y0) = pair[0];
                let (x1, y1) = pair[1];
                if x <= x1 {
                    let t = if x1 <= x0 { 0.0 } else { (x - x0) / (x1 - x0) };
                    return lerp(y0, y1, t.clamp(0.0, 1.0));
                }
            }
            points[points.len() - 1].1
        }
    }
}

/// Cone gain: 1 inside the inner half-angle, ramping to `outer_gain` at the outer half-angle.
pub fn cone_gain(
    forward: Vec3,
    to_listener: Vec3,
    inner_degrees: f32,
    outer_degrees: f32,
    outer_gain: f32,
) -> f32 {
    if forward.length_squared() < 1e-8 || inner_degrees >= 360.0 {
        return 1.0;
    }
    let dir = to_listener.normalize_or_zero();
    let cos = dir.dot(forward.normalize_or_zero()).clamp(-1.0, 1.0);
    let angle = cos.acos().to_degrees();
    let inner = inner_degrees.max(0.0);
    let outer = outer_degrees.max(inner);
    if angle <= inner {
        1.0
    } else if angle >= outer {
        outer_gain
    } else {
        let t = remap(angle, (inner, outer), (0.0, 1.0));
        lerp(1.0, outer_gain, t)
    }
}

/// Stereo pan (−1..1) from the emitter direction in the listener's frame: the projection of the
/// direction onto the listener's right axis. With kira's equal-power gains this is constant
/// power for a frontal source.
pub fn direction_pan(to_emitter: Vec3, listener_right: Vec3) -> f32 {
    let dir = to_emitter.normalize_or_zero();
    let pan = dir.dot(listener_right);
    // A NaN listener axis (a broken camera transform) centres instead of reaching the mixer.
    if pan.is_nan() { 0.0 } else { pan.clamp(-1.0, 1.0) }
}

/// Air-absorption cutoff ramping linearly from 20 kHz at distance 0 to `min_hz` at the max.
/// [`spatialize`] uses `absorption_cutoff_from`, which starts the ramp at the min distance.
pub fn absorption_cutoff(distance: f32, max_distance: f32, min_hz: f64) -> f64 {
    absorption_cutoff_from(distance, 0.0, max_distance, min_hz)
}

/// Air-absorption cutoff: 20 kHz up to `min_distance`, ramping linearly to `min_hz` at
/// `max_distance` (the same span the distance rolloff uses). A NaN distance gives the full 20 kHz.
pub fn absorption_cutoff_from(distance: f32, min_distance: f32, max_distance: f32, min_hz: f64) -> f64 {
    let span = (max_distance - min_distance).max(1e-4);
    let t = ((distance - min_distance) / span).clamp(0.0, 1.0);
    let t = if t.is_nan() { 0.0 } else { t };
    lerp(20_000.0, min_hz as f32, t) as f64
}

/// Doppler playback-rate ratio for an emitter with radial velocity `closing_speed` (positive when
/// approaching the listener), scaled by `factor` (OpenAL 1.1 § "Doppler Shift", static listener).
pub fn doppler_ratio(closing_speed: f32, factor: f32) -> f32 {
    if factor <= 0.0 {
        return 1.0;
    }
    crate::medium::doppler_ratio_in(SPEED_OF_SOUND, closing_speed, factor)
}

/// Doppler playback-rate ratio with both ends moving (OpenAL 1.1 § "Doppler Shift"):
/// `(c − f·v_listener) / (c − f·v_source)`, where both speeds are measured along the line from
/// the source to the listener — `source_approach` is positive when the source moves toward the
/// listener, `listener_recede` is positive when the listener moves away from the source. Speeds
/// are clamped below `c / factor` (no sonic boom) and the ratio to 0.5–4.
pub fn doppler_ratio_moving(source_approach: f32, listener_recede: f32, factor: f32) -> f32 {
    crate::medium::doppler_ratio_moving_in(SPEED_OF_SOUND, source_approach, listener_recede, factor)
}

/// Spatialises one emitter against one listener: distance gain, cone, pan, absorption and
/// doppler, blended by `spatial_blend`.
pub fn spatialize(listener: &ListenerFrame, emitter: &EmitterFrame, params: &AudioEmitter) -> SpatialFrame {
    let to_emitter = emitter.position - listener.position;
    let distance = to_emitter.length();
    if !distance.is_finite() {
        // A NaN or infinite transform (a physics blow-up, a bad animation key): the emitter is
        // nowhere, so it is silent and out of range (virtualised, no occlusion ray) until it has
        // a real place again — never a NaN gain, pan or cutoff for the mixer. The distance is
        // infinite, not `f32::MAX`: a finite one would become a finite arrival delay of 10^36 s
        // (a path voice that never finishes); an infinite delay is dropped by the path.
        return SpatialFrame {
            gain: 0.0,
            pan: 0.0,
            cutoff_hz: params.air_absorption.then_some(20_000.0),
            pitch_ratio: 1.0,
            distance: f32::INFINITY,
            head_direction: Vec3::ZERO,
        };
    }
    let distance_part = distance_gain_faded(
        &params.rolloff,
        params.min_distance,
        params.max_distance,
        distance,
        params.fade_start,
    );
    let cone = cone_gain(
        emitter.forward,
        -to_emitter,
        params.cone_inner_degrees,
        params.cone_outer_degrees,
        params.cone_outer_gain,
    );
    // NaN blend: the 3D default.
    let blend = if params.spatial_blend.is_nan() {
        1.0
    } else {
        params.spatial_blend.clamp(0.0, 1.0)
    };
    let gain = lerp(1.0, distance_part * cone, blend);

    // Binaural listeners get their direction cues from the HRTF effect, not from panning.
    let pan = if blend > 0.0 && !listener.hrtf {
        direction_pan(to_emitter, listener.right) * blend
    } else {
        0.0
    };
    let head_direction = listener.head_direction(emitter.position);

    // Air absorption de-blends like everything else: a fully 2D voice is not low-passed
    // by a far 3D distance (it plays at full gain, so it keeps its treble).
    let cutoff_hz = if blend > 0.0 && params.air_absorption {
        Some(absorption_cutoff_from(
            distance,
            params.min_distance,
            params.max_distance,
            2_000.0,
        ))
    } else {
        None
    };

    let doppler = if params.doppler_factor > 0.0 && distance > 1e-4 {
        // Unit line from the source to the listener.
        let line = -to_emitter / distance;
        doppler_ratio_moving(
            emitter.velocity.dot(line),
            listener.velocity.dot(line),
            params.doppler_factor,
        )
    } else {
        1.0
    };
    let pitch_ratio = lerp(1.0, doppler, blend);

    SpatialFrame {
        gain,
        pan,
        cutoff_hz,
        pitch_ratio,
        distance,
        head_direction,
    }
}

/// Velocity estimation from per-frame positions (Doppler): the position delta over the real
/// frame time, smoothed with a short exponential moving average (frame-time jitter would
/// otherwise wobble the pitch), with a teleport guard — a jump faster than twice the speed of
/// sound resets the velocity to zero instead of producing a pitch spike.
#[derive(Debug, Default)]
pub struct MotionTracker {
    entries: std::collections::HashMap<Entity, Motion>,
}

#[derive(Debug, Clone, Copy)]
struct Motion {
    position: Vec3,
    velocity: Vec3,
    seen: bool,
}

impl MotionTracker {
    /// Smoothing time constant, seconds.
    pub const SMOOTHING_SECS: f32 = 0.05;

    /// Records `entity` at `position` after `dt` seconds and returns its smoothed velocity
    /// (zero on the first sighting and for non-positive `dt`).
    pub fn update(&mut self, entity: Entity, position: Vec3, dt: f32) -> Vec3 {
        let entry = self.entries.entry(entity).or_insert(Motion {
            position,
            velocity: Vec3::ZERO,
            seen: true,
        });
        entry.seen = true;
        if dt > 0.0 {
            let raw = (position - entry.position) / dt;
            entry.velocity = if !raw.is_finite() || raw.length() > 2.0 * SPEED_OF_SOUND {
                Vec3::ZERO
            } else {
                let k = 1.0 - (-dt / Self::SMOOTHING_SECS).exp();
                entry.velocity + (raw - entry.velocity) * k
            };
        }
        entry.position = position;
        entry.velocity
    }

    /// Re-expresses the tracked positions after an origin rebase (spec 78 § 12.2): positions jump,
    /// velocities do not, so Doppler hears nothing.
    pub fn shift_origin(&mut self, shift: Vec3) {
        for entry in self.entries.values_mut() {
            entry.position -= shift;
        }
    }

    /// Forgets entities not updated since the last call (despawned).
    pub fn retain_seen(&mut self) {
        self.entries.retain(|_, m| std::mem::take(&mut m.seen));
    }
}

/// Index of the listener nearest to `position` (split-screen: each emitter is heard by the
/// nearest listener, spec § 3). `None` for an empty list.
pub fn nearest_listener(listeners: &[ListenerFrame], position: Vec3) -> Option<usize> {
    listeners
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            a.position
                .distance_squared(position)
                .total_cmp(&b.position.distance_squared(position))
        })
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebase_leaves_the_tracked_velocity_alone() {
        let mut tracker = MotionTracker::default();
        let entity = Entity::from_raw_u32(7).expect("entity");
        tracker.update(entity, Vec3::new(5000.0, 0.0, 0.0), 0.1);
        let before = tracker.update(entity, Vec3::new(5001.0, 0.0, 0.0), 0.1);
        let shift = Vec3::new(4096.0, 0.0, 0.0);
        tracker.shift_origin(shift);
        let after = tracker.update(entity, Vec3::new(5002.0, 0.0, 0.0) - shift, 0.1);
        assert!(after.length() >= before.length() * 0.99, "{before} {after}");
        assert!(after.length() < 20.0, "{after}");
    }

    #[test]
    fn zero_min_distance_never_yields_nan_or_silence() {
        for d in [0.0, 0.5, 3.0] {
            let g = distance_gain(&RolloffCurve::Inverse, 0.0, 100.0, d);
            assert!(g.is_finite() && g > 0.0, "gain {g} at {d}");
        }
        assert_eq!(distance_gain(&RolloffCurve::Inverse, 0.0, 100.0, 0.0), 1.0);
        assert_eq!(gain_to_db(f32::NAN), -100.0);
        // Negative and NaN min distances, every curve: finite, full volume at the source.
        let log = RolloffCurve::Log { db_per_doubling: 6.0 };
        let custom = RolloffCurve::Custom(vec![(0.0, 1.0), (10.0, 0.0)]);
        for curve in [RolloffCurve::Inverse, RolloffCurve::Linear, log, custom] {
            for min in [-1.0, f32::NAN] {
                assert_eq!(distance_gain(&curve, min, 10.0, 0.0), 1.0, "{curve:?} min {min}");
                let g = distance_gain(&curve, min, 10.0, 5.0);
                assert!(
                    g.is_finite() && (0.0..=1.0).contains(&g),
                    "{curve:?} min {min}: {g}"
                );
            }
        }
        // The floor is only for the dividing curves: custom and linear keep a zero min distance.
        let custom = RolloffCurve::Custom(vec![(0.0, 1.0), (10.0, 0.0)]);
        assert_eq!(distance_gain(&custom, 0.0, 10.0, 0.0), 1.0);
        assert_eq!(distance_gain(&RolloffCurve::Linear, 0.0, 10.0, 0.0), 1.0);
    }

    fn emitter() -> AudioEmitter {
        AudioEmitter {
            min_distance: 2.0,
            max_distance: 20.0,
            ..AudioEmitter::default()
        }
    }

    fn listener_at_origin_facing_neg_z() -> ListenerFrame {
        ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z)
    }

    #[test]
    fn no_fade_start_leaves_every_curve_unchanged() {
        let curves = [
            RolloffCurve::Inverse,
            RolloffCurve::Linear,
            RolloffCurve::Log { db_per_doubling: 6.0 },
            RolloffCurve::Custom(vec![(0.0, 1.0), (50.0, 0.25)]),
        ];
        for curve in &curves {
            for d in 0..=60 {
                let d = d as f32;
                assert_eq!(
                    distance_gain_faded(curve, 1.0, 50.0, d, None),
                    distance_gain(curve, 1.0, 50.0, d),
                    "{curve:?} at {d}"
                );
            }
        }
    }

    #[test]
    fn the_fade_reaches_zero_at_max_distance_and_is_continuous_at_its_start() {
        for curve in [RolloffCurve::Inverse, RolloffCurve::Log { db_per_doubling: 6.0 }] {
            // Without the fade this curve cuts from about 2 % (or more) to nothing at 50 m.
            assert!(distance_gain(&curve, 1.0, 50.0, 50.0) > 0.01, "{curve:?}");
            let faded = |d: f32| distance_gain_faded(&curve, 1.0, 50.0, d, Some(0.8));
            assert_eq!(faded(50.0), 0.0, "{curve:?}");
            assert_eq!(faded(60.0), 0.0, "{curve:?}");
            // Identical up to the start of the fade.
            for d in [0.0f32, 1.0, 10.0, 39.0, 40.0] {
                assert_eq!(faded(d), distance_gain(&curve, 1.0, 50.0, d), "{curve:?} at {d}");
            }
            // No step across the start: the gain moves no more than the curve's own slope does.
            let eps = 1e-3f32;
            assert!((faded(40.0 + eps) - faded(40.0)).abs() < 1e-5, "{curve:?}");
            // Falls monotonically to the end, with no step at the end either.
            let mut last = faded(40.0);
            for step in 1..=1000 {
                let g = faded(40.0 + step as f32 * 0.01);
                assert!(g <= last + 1e-6, "{curve:?} at step {step}: {g} after {last}");
                assert!(
                    last - g < 1e-3,
                    "{curve:?} at step {step}: a jump from {last} to {g}"
                );
                last = g;
            }
        }
    }

    #[test]
    fn the_fade_ignores_linear_custom_and_bad_values() {
        let custom = RolloffCurve::Custom(vec![(0.0, 1.0), (50.0, 0.25)]);
        for curve in [RolloffCurve::Linear, custom] {
            for d in [0.0f32, 25.0, 45.0, 50.0] {
                assert_eq!(
                    distance_gain_faded(&curve, 1.0, 50.0, d, Some(0.5)),
                    distance_gain(&curve, 1.0, 50.0, d),
                    "{curve:?} at {d}"
                );
            }
        }
        for bad in [f32::NAN, f32::INFINITY, -0.5, 1.0, 1.5] {
            assert_eq!(
                distance_gain_faded(&RolloffCurve::Inverse, 1.0, 50.0, 45.0, Some(bad)),
                distance_gain(&RolloffCurve::Inverse, 1.0, 50.0, 45.0),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_fade_starts_no_nearer_than_min_distance() {
        // fade_start 0 with a 10 m min distance: full volume to 10 m, then the fade alone.
        let faded = |d: f32| distance_gain_faded(&RolloffCurve::Inverse, 10.0, 20.0, d, Some(0.0));
        assert_eq!(faded(5.0), 1.0);
        assert_eq!(faded(10.0), 1.0);
        assert_eq!(faded(20.0), 0.0);
        assert!(faded(15.0) > 0.0 && faded(15.0) < 1.0);
        // A min distance at the cap has nothing to fade over.
        let at_cap = distance_gain_faded(&RolloffCurve::Inverse, 20.0, 20.0, 20.0, Some(0.5));
        assert_eq!(at_cap, 1.0);
    }

    #[test]
    fn spatialize_applies_the_fade() {
        let listener = ListenerFrame::default();
        let emitter_frame = EmitterFrame {
            position: Vec3::new(0.0, 0.0, -49.0),
            ..EmitterFrame::default()
        };
        let plain = AudioEmitter::default();
        let faded = AudioEmitter {
            fade_start: Some(0.5),
            ..AudioEmitter::default()
        };
        let cut = spatialize(&listener, &emitter_frame, &plain).gain;
        let eased = spatialize(&listener, &emitter_frame, &faded).gain;
        assert!(eased < cut * 0.05, "{eased} vs {cut}");
    }

    #[test]
    fn inverse_rolloff_follows_the_openal_formula() {
        // g = min_d / max(min_d, d), 0 beyond max_d.
        assert!((distance_gain(&RolloffCurve::Inverse, 2.0, 20.0, 1.0) - 1.0).abs() < 1e-6);
        assert!((distance_gain(&RolloffCurve::Inverse, 2.0, 20.0, 2.0) - 1.0).abs() < 1e-6);
        assert!((distance_gain(&RolloffCurve::Inverse, 2.0, 20.0, 4.0) - 0.5).abs() < 1e-6);
        assert!((distance_gain(&RolloffCurve::Inverse, 2.0, 20.0, 10.0) - 0.2).abs() < 1e-6);
        assert_eq!(distance_gain(&RolloffCurve::Inverse, 2.0, 20.0, 25.0), 0.0);
    }

    #[test]
    fn linear_rolloff_reaches_zero_at_max_distance() {
        assert!((distance_gain(&RolloffCurve::Linear, 2.0, 20.0, 2.0) - 1.0).abs() < 1e-6);
        assert!((distance_gain(&RolloffCurve::Linear, 2.0, 20.0, 11.0) - 0.5).abs() < 1e-6);
        assert!(distance_gain(&RolloffCurve::Linear, 2.0, 20.0, 20.0) < 1e-6);
        assert_eq!(distance_gain(&RolloffCurve::Linear, 2.0, 20.0, 21.0), 0.0);
    }

    #[test]
    fn log_rolloff_loses_db_per_doubling() {
        let curve = RolloffCurve::Log { db_per_doubling: 6.0 };
        assert!((distance_gain(&curve, 2.0, 100.0, 2.0) - 1.0).abs() < 1e-6);
        // One doubling: -6 dB (half amplitude to within the dB rounding).
        assert!((distance_gain(&curve, 2.0, 100.0, 4.0) - 0.5012).abs() < 1e-3);
        // Two doublings: -12 dB.
        assert!((distance_gain(&curve, 2.0, 100.0, 8.0) - 0.2512).abs() < 1e-3);
    }

    #[test]
    fn custom_curve_interpolates_and_clamps() {
        let curve = RolloffCurve::Custom(vec![(0.0, 1.0), (5.0, 0.5), (10.0, 0.0)]);
        assert!((distance_gain(&curve, 0.0, 10.0, 2.5) - 0.75).abs() < 1e-6);
        assert_eq!(distance_gain(&curve, 0.0, 10.0, -1.0), 1.0, "clamped below");
        assert_eq!(distance_gain(&curve, 0.0, 10.0, 15.0), 0.0, "clamped above");
    }

    #[test]
    fn cone_ramps_from_inner_to_outer() {
        let forward = Vec3::NEG_Z;
        let inner = Vec3::NEG_Z; // straight ahead: angle 0
        let side = Vec3::X; // 90° off axis
        let behind = Vec3::Z; // 180° off axis
        assert!((cone_gain(forward, inner, 30.0, 90.0, 0.25) - 1.0).abs() < 1e-6);
        let at_60 = cone_gain(
            forward,
            Vec3::new(-(60f32.to_radians()).sin(), 0.0, -(60f32.to_radians()).cos()),
            30.0,
            90.0,
            0.25,
        );
        assert!(
            (at_60 - 0.625).abs() < 1e-3,
            "halfway between inner and outer: {at_60}"
        );
        assert!((cone_gain(forward, side, 30.0, 90.0, 0.25) - 0.25).abs() < 1e-6);
        assert!((cone_gain(forward, behind, 30.0, 90.0, 0.25) - 0.25).abs() < 1e-6);
        // Omnidirectional.
        assert!((cone_gain(Vec3::ZERO, side, 30.0, 90.0, 0.25) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn pan_follows_the_listener_right_axis() {
        let listener = listener_at_origin_facing_neg_z();
        let mut params = emitter();
        params.spatial_blend = 1.0;
        let frame = spatialize(
            &listener,
            &EmitterFrame {
                position: Vec3::X * 5.0,
                ..Default::default()
            },
            &params,
        );
        assert!(frame.pan > 0.99, "hard right pans right: {}", frame.pan);
        let frame = spatialize(
            &listener,
            &EmitterFrame {
                position: Vec3::NEG_X * 5.0,
                ..Default::default()
            },
            &params,
        );
        assert!(frame.pan < -0.99, "hard left pans left: {}", frame.pan);
        let frame = spatialize(
            &listener,
            &EmitterFrame {
                position: Vec3::NEG_Z * 5.0,
                ..Default::default()
            },
            &params,
        );
        assert!(frame.pan.abs() < 1e-6, "front is centered: {}", frame.pan);
    }

    #[test]
    fn spatial_blend_zero_is_flat() {
        let listener = listener_at_origin_facing_neg_z();
        let mut params = emitter();
        params.spatial_blend = 0.0;
        let frame = spatialize(
            &listener,
            &EmitterFrame {
                position: Vec3::X * 100.0,
                ..Default::default()
            },
            &params,
        );
        assert!((frame.gain - 1.0).abs() < 1e-6);
        assert!(frame.pan.abs() < 1e-6);
        assert!((frame.pitch_ratio - 1.0).abs() < 1e-6);
        // Deep-zai-audio-mix finding 8: air absorption must de-blend with everything else —
        // a fully 2D voice is not low-passed by a far 3D distance.
        params.air_absorption = true;
        let frame = spatialize(
            &listener,
            &EmitterFrame {
                position: Vec3::X * 100.0,
                ..Default::default()
            },
            &params,
        );
        assert_eq!(
            frame.cutoff_hz, None,
            "a flat 2D voice keeps its treble at any distance"
        );
    }

    #[test]
    fn non_finite_db_and_distances_never_reach_the_mixer() {
        // Deep-zai-audio-mix finding 9: `db_to_gain` is the math layer's entry point; NaN and
        // +∞ from a scene file must neutralize like `gain_to_db` does, not propagate.
        assert_eq!(db_to_gain(f32::NAN), 0.0);
        assert_eq!(db_to_gain(f32::NEG_INFINITY), 0.0);
        assert_eq!(db_to_gain(-f32::MAX), 0.0);
        assert!(db_to_gain(f32::INFINITY).is_finite(), "+∞ dB clamps, not inf");
        assert!(db_to_gain(1e30).is_finite(), "an absurd finite dB clamps too");
        // A NaN `max_distance` with a fade would divide by NaN where the unfaded path stays
        // finite: the faded path validates up front and returns silence.
        let log = RolloffCurve::Log { db_per_doubling: 6.0 };
        let faded = distance_gain_faded(&log, 1.0, f32::NAN, 5.0, Some(0.5));
        assert_eq!(faded, 0.0, "NaN max_distance is silence, not NaN");
        let faded = distance_gain_faded(&RolloffCurve::Inverse, 1.0, f32::INFINITY, 5.0, Some(0.5));
        assert!(
            faded.is_finite() && faded >= 0.0,
            "an infinite max_distance: {faded}"
        );
    }

    /// Rev-kimi-28 (audio2-k item 4): an infinite `max_distance` means "no cut-off", not
    /// silence. Without a fade it plays the rolloff curve exactly as `distance_gain` does (the
    /// base behavior); with a fade only the fade is skipped (it has no end to fade towards).
    #[test]
    fn an_infinite_max_distance_plays_the_rolloff_curve() {
        let log = RolloffCurve::Log { db_per_doubling: 6.0 };
        for curve in [RolloffCurve::Inverse, log, RolloffCurve::Linear] {
            let unfaded = distance_gain(&curve, 1.0, f32::INFINITY, 10.0);
            assert!(unfaded > 0.0, "{curve:?}: the curve is audible at 10 m");
            for fade in [None, Some(0.5)] {
                assert_eq!(
                    distance_gain_faded(&curve, 1.0, f32::INFINITY, 10.0, fade),
                    unfaded,
                    "{curve:?}, fade {fade:?}: an infinite max_distance is not a mute"
                );
            }
        }
        assert_eq!(
            distance_gain_faded(&RolloffCurve::Inverse, 1.0, f32::INFINITY, 10.0, None),
            0.1,
            "inverse rolloff, min 1 m, at 10 m"
        );
        // NaN stays silence (the engine's policy for broken spatial input).
        assert_eq!(
            distance_gain_faded(&RolloffCurve::Inverse, 1.0, f32::NAN, 10.0, None),
            0.0
        );
    }

    #[test]
    fn absorption_ramps_the_cutoff_down_with_distance() {
        assert!(absorption_cutoff(0.0, 20.0, 2_000.0) > 19_000.0);
        let mid = absorption_cutoff(10.0, 20.0, 2_000.0);
        assert!((mid - 11_000.0).abs() < 1.0, "midpoint cutoff {mid}");
        assert!((absorption_cutoff(20.0, 20.0, 2_000.0) - 2_000.0).abs() < 1.0);
    }

    #[test]
    fn absorption_starts_at_the_min_distance() {
        // Inside the min distance nothing is absorbed.
        assert_eq!(absorption_cutoff_from(30.0, 40.0, 50.0, 2_000.0), 20_000.0);
        assert_eq!(absorption_cutoff_from(40.0, 40.0, 50.0, 2_000.0), 20_000.0);
        let mid = absorption_cutoff_from(45.0, 40.0, 50.0, 2_000.0);
        assert!((mid - 11_000.0).abs() < 1.0, "midpoint cutoff {mid}");
        assert!((absorption_cutoff_from(50.0, 40.0, 50.0, 2_000.0) - 2_000.0).abs() < 1.0);
        assert!(absorption_cutoff_from(f32::NAN, 1.0, 20.0, 2_000.0).is_finite());
        // spatialize wires the min distance through.
        let params = AudioEmitter {
            min_distance: 40.0,
            max_distance: 50.0,
            air_absorption: true,
            ..emitter()
        };
        let frame = spatialize(
            &listener_at_origin_facing_neg_z(),
            &EmitterFrame {
                position: Vec3::X * 30.0,
                ..Default::default()
            },
            &params,
        );
        assert_eq!(frame.cutoff_hz, Some(20_000.0));
    }

    #[test]
    fn supersonic_approach_stays_a_pitch_rise() {
        assert_eq!(doppler_ratio(400.0, 1.0), 4.0);
        assert_eq!(doppler_ratio(-400.0, 1.0), 0.5);
    }

    #[test]
    fn doppler_raises_pitch_when_approaching() {
        assert!(doppler_ratio(34.33, 1.0) > 1.1, "approaching raises pitch");
        assert!(doppler_ratio(-34.33, 1.0) < 0.91, "receding lowers pitch");
        assert!(
            (doppler_ratio(100.0, 0.0) - 1.0).abs() < 1e-9,
            "factor 0 disables"
        );
        let frame = {
            let listener = listener_at_origin_facing_neg_z();
            let mut params = emitter();
            params.doppler_factor = 1.0;
            // Emitter at -Z, moving toward the origin: closing speed is positive.
            let emitter_frame = EmitterFrame {
                position: Vec3::NEG_Z * 20.0,
                velocity: Vec3::Z * 10.0,
                forward: Vec3::ZERO,
            };
            spatialize(&listener, &emitter_frame, &params)
        };
        assert!(
            frame.pitch_ratio > 1.0,
            "approaching emitter pitches up: {}",
            frame.pitch_ratio
        );
    }

    #[test]
    fn a_moving_listener_hears_doppler_too() {
        let mut listener = listener_at_origin_facing_neg_z();
        let mut params = emitter();
        params.doppler_factor = 1.0;
        let source = EmitterFrame {
            position: Vec3::NEG_Z * 10.0,
            ..Default::default()
        };
        // Listener drives toward the static source at 30 m/s: pitch rises by (c + 30) / c.
        listener.velocity = Vec3::NEG_Z * 30.0;
        let frame = spatialize(&listener, &source, &params);
        let expected = (SPEED_OF_SOUND + 30.0) / SPEED_OF_SOUND;
        assert!(
            (frame.pitch_ratio - expected).abs() < 1e-4,
            "{}",
            frame.pitch_ratio
        );
        // Driving away lowers it.
        listener.velocity = Vec3::Z * 30.0;
        let frame = spatialize(&listener, &source, &params);
        assert!(frame.pitch_ratio < 1.0, "{}", frame.pitch_ratio);
        // Moving together (same velocity) cancels.
        listener.velocity = Vec3::X * 20.0;
        let moving_source = EmitterFrame {
            velocity: Vec3::X * 20.0,
            ..source
        };
        let frame = spatialize(&listener, &moving_source, &params);
        assert!((frame.pitch_ratio - 1.0).abs() < 1e-5);
    }

    #[test]
    fn motion_tracker_uses_the_real_frame_delta() {
        use regia_ecs::ecs::entity::EntityIndex;
        let e = Entity::from_index(EntityIndex::from_raw_u32(3).expect("index"));
        let mut tracker = MotionTracker::default();
        assert_eq!(tracker.update(e, Vec3::ZERO, 0.02), Vec3::ZERO, "first sighting");
        // 30 m/s along +X at 50 Hz frames (not 60): converges to 30, not 36.
        let mut v = Vec3::ZERO;
        for i in 1..=50 {
            v = tracker.update(e, Vec3::X * (0.6 * i as f32), 0.02);
        }
        assert!((v.x - 30.0).abs() < 0.05, "{v}");
        // A teleport is not a 5 km/s source.
        let v = tracker.update(e, Vec3::X * 1_000.0, 0.02);
        assert_eq!(v, Vec3::ZERO);
        // Entities not seen in a frame are dropped.
        tracker.retain_seen();
        tracker.retain_seen();
        assert!(tracker.entries.is_empty());
    }

    #[test]
    fn nearest_listener_picks_the_closest() {
        let a = ListenerFrame::looking_at(Vec3::ZERO, Vec3::NEG_Z);
        let b = ListenerFrame::looking_at(Vec3::X * 100.0, Vec3::NEG_Z);
        assert_eq!(nearest_listener(&[a, b], Vec3::X * 10.0), Some(0));
        assert_eq!(nearest_listener(&[a, b], Vec3::X * 90.0), Some(1));
        assert_eq!(nearest_listener(&[], Vec3::ZERO), None);
    }

    #[test]
    fn binaural_listeners_do_not_pan_and_report_head_direction() {
        let mut listener = listener_at_origin_facing_neg_z();
        listener.hrtf = true;
        let source = EmitterFrame {
            position: Vec3::X * 5.0,
            ..Default::default()
        };
        let frame = spatialize(&listener, &source, &emitter());
        assert_eq!(frame.pan, 0.0);
        assert!((frame.head_direction - Vec3::X).length() < 1e-5);
        // Facing +X, the same source is straight ahead (−Z in the head frame).
        let turned = ListenerFrame::looking_at(Vec3::ZERO, Vec3::X);
        assert!((turned.head_direction(Vec3::X * 5.0) - Vec3::NEG_Z).length() < 1e-5);
    }

    #[test]
    fn default_listener_faces_neg_z() {
        let l = ListenerFrame::default();
        assert_eq!((l.forward, l.right, l.up), (Vec3::NEG_Z, Vec3::X, Vec3::Y));
    }

    #[test]
    fn direction_of_reports_azimuth_and_elevation() {
        let listener = listener_at_origin_facing_neg_z();
        let (az, el) = listener.direction_of(Vec3::X);
        assert!((az - std::f32::consts::FRAC_PI_2).abs() < 1e-5 && el.abs() < 1e-5);
        let (az, _) = listener.direction_of(Vec3::NEG_Z);
        assert!(az.abs() < 1e-5, "ahead is 0");
        let (az, _) = listener.direction_of(Vec3::NEG_X);
        assert!(
            (az + std::f32::consts::FRAC_PI_2).abs() < 1e-5,
            "left is negative"
        );
        let (_, el) = listener.direction_of(Vec3::new(0.0, 1.0, -1.0));
        assert!((el - std::f32::consts::FRAC_PI_4).abs() < 1e-5);
    }

    #[test]
    fn polyline_edges() {
        assert_eq!(polyline(&[], 5.0), 0.0);
        assert_eq!(polyline(&[(1.0, 0.5)], 5.0), 0.5);
    }
}
