// SPDX-License-Identifier: MPL-2.0
//! Deterministic fire light: a flickering point light that is a pure function of an integer
//! frame and a seed.
//!
//! [`FireLight::sample`] reads no clock and no global random generator, so a render run twice
//! (or on another machine) lights the same frames with the same values. The flicker is value
//! noise on a 64-bit integer hash with quintic smoothing, in two octaves: a slow swell at a
//! quarter of `flicker_hz` and a fast flutter at `flicker_hz`. After `lifetime_frames` the light
//! is exactly zero; it fades smoothly over the last tenth of its life.
//!
//! The colour is a published blackbody fit of the temperature (see `docs/provenance.md`), in
//! linear RGB with the largest channel at 1, so `intensity` alone carries the brightness. A
//! dimmer flicker also reads slightly redder: the temperature follows the flicker by up to 4 %.
//!
//! [`FireLights`] is the runtime side: a resource holding the live fires of a scene. Its system
//! merges them into the frame's particle lights ([`crate::lights::VfxFrameLights`]), so the
//! profile's light budget and the renderer's local-light path apply unchanged.

use regia_core::math::{Vec3, det_ln_f64, det_powf_f64, det_sin_cos};
use regia_ecs::prelude::*;

use crate::crash::{CrashBurst, FireSize, crash_fire_placements};
use crate::lights::{ParticleLight, VfxFrameLights};

/// Frames per second the flicker frequencies are expressed against. A fire's `flicker_hz` is a
/// rate in this 60 Hz frame clock, so the same frame number always gives the same light.
pub const FIRE_LIGHT_FRAME_HZ: f64 = 60.0;

/// Lowest and highest temperature the colour fit is evaluated at (kelvin).
const MIN_TEMPERATURE_K: f32 = 1000.0;
const MAX_TEMPERATURE_K: f32 = 40000.0;

/// Weight of the slow octave in the flicker (the fast octave has the rest).
const SLOW_WEIGHT: f32 = 0.6;

/// One flickering fire light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FireLight {
    /// Intensity at full health with no flicker. Negative or non-finite reads as 0.
    pub base_intensity: f32,
    /// Blackbody temperature in kelvin (clamped to 1000..=40000). A wood fire is ~1800 K.
    pub color_temperature_k: f32,
    /// How deep the flicker goes, `0` (steady) to `1` (swings between 0 and twice the base).
    /// Clamped to that range.
    pub flicker_depth: f32,
    /// Fast-octave rate in flutters per second of the 60 Hz frame clock. The slow swell runs at
    /// a quarter of it. Non-finite or non-positive reads as no flicker.
    pub flicker_hz: f32,
    /// Seed of the noise: two fires with different seeds flicker independently.
    pub seed: u64,
    /// Frames the light lives; the light is zero from this frame on.
    pub lifetime_frames: u64,
}

/// One evaluation of a [`FireLight`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FireLightSample {
    /// Light intensity now; never negative, never NaN.
    pub intensity: f32,
    /// Linear RGB colour of the light, largest channel `1`.
    pub color: [f32; 3],
}

impl Default for FireLight {
    fn default() -> Self {
        Self {
            base_intensity: 1.0,
            color_temperature_k: 1800.0,
            flicker_depth: 0.35,
            flicker_hz: 6.0,
            seed: 0,
            lifetime_frames: 10_800,
        }
    }
}

impl FireLight {
    /// The light at `frame` frames after it started. A pure function of `frame` and `self`.
    pub fn sample(&self, frame: u64) -> FireLightSample {
        let fade = self.fade(frame);
        let modulation = self.modulation(frame);
        let base = if self.base_intensity.is_finite() {
            self.base_intensity.max(0.0)
        } else {
            0.0
        };
        // Cooler (redder) when dim: the temperature follows the flicker by up to 4 %.
        let temperature = sanitize_temperature(self.color_temperature_k) * (1.0 + 0.04 * (modulation - 1.0));
        FireLightSample {
            intensity: (base * modulation * fade).max(0.0),
            color: blackbody_linear_rgb(temperature),
        }
    }

    /// The fade factor in `[0, 1]`: `1` until the last tenth of the life, then a smooth
    /// non-increasing fall to `0`, and `0` from `lifetime_frames` on.
    pub fn fade(&self, frame: u64) -> f32 {
        let life = self.lifetime_frames;
        if frame >= life {
            return 0.0;
        }
        let fade_len = life.div_ceil(10).max(1);
        let fade_start = life - fade_len;
        if frame <= fade_start {
            return 1.0;
        }
        let t = (frame - fade_start) as f32 / fade_len as f32;
        1.0 - smoothstep(t)
    }

    /// The flicker multiplier in `[1 - depth, 1 + depth]`; `1` when steady.
    fn modulation(&self, frame: u64) -> f32 {
        let depth = if self.flicker_depth.is_finite() {
            self.flicker_depth.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let hz = f64::from(self.flicker_hz);
        if depth == 0.0 || !hz.is_finite() || hz <= 0.0 {
            return 1.0;
        }
        let t = frame as f64 / FIRE_LIGHT_FRAME_HZ;
        let slow = value_noise(self.seed, 0, t * hz * 0.25);
        let fast = value_noise(self.seed, 1, t * hz);
        let n = SLOW_WEIGHT * slow + (1.0 - SLOW_WEIGHT) * fast;
        1.0 + depth * (2.0 * n - 1.0)
    }
}

/// Smoothstep on `[0, 1]` (input clamped).
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// SplitMix64 finalizer (Steele, Lea, Flood 2014): a bijective 64-bit integer mix.
fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Lattice value in `[0, 1)` at integer point `i` of octave `octave` for `seed`.
fn lattice(seed: u64, octave: u64, i: u64) -> f32 {
    let h = mix64(mix64(seed ^ octave.wrapping_mul(0x9e37_79b9_7f4a_7c15)) ^ i);
    // Top 24 bits are exactly representable in an f32.
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// 1D value noise in `[0, 1]` at `x`, quintic-smoothed between hashed lattice values.
fn value_noise(seed: u64, octave: u64, x: f64) -> f32 {
    let floor = x.floor();
    let i = floor as u64;
    let f = (x - floor) as f32;
    let s = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let a = lattice(seed, octave, i);
    let b = lattice(seed, octave, i.wrapping_add(1));
    a + (b - a) * s
}

fn sanitize_temperature(kelvin: f32) -> f32 {
    if kelvin.is_finite() {
        kelvin.clamp(MIN_TEMPERATURE_K, MAX_TEMPERATURE_K)
    } else {
        1800.0
    }
}

/// Linear RGB of a blackbody at `kelvin` (clamped to 1000..=40000), largest channel `1`.
///
/// The empirical fit published by Tanner Helland (2012) to Mitchell Charity's blackbody colour
/// table; its output is display (sRGB-encoded) colour, decoded here to linear light. Every
/// transcendental runs on the deterministic kernels ([`regia_core::math::det_powf_f64`],
/// [`regia_core::math::det_ln_f64`]): the same bits on every platform, unlike the platform libm
/// `powf`/`ln`, whose last digit differs between glibc, musl, UCRT, macOS and Android (spec 15 § 5).
pub fn blackbody_linear_rgb(kelvin: f32) -> [f32; 3] {
    let t = f64::from(sanitize_temperature(kelvin)) / 100.0;
    let red = if t <= 66.0 {
        255.0
    } else {
        329.698_727_446 * det_powf_f64(t - 60.0, -0.133_204_759_2)
    };
    let green = if t <= 66.0 {
        99.470_802_586_1 * det_ln_f64(t) - 161.119_568_166_1
    } else {
        288.122_169_528_3 * det_powf_f64(t - 60.0, -0.075_514_849_2)
    };
    let blue = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.517_731_223_1 * det_ln_f64(t - 10.0) - 305.044_792_730_7
    };
    let linear = |c: f64| -> f32 {
        let s = (c / 255.0).clamp(0.0, 1.0);
        let l = if s <= 0.040_45 {
            s / 12.92
        } else {
            det_powf_f64((s + 0.055) / 1.055, 2.4)
        };
        l as f32
    };
    let rgb = [linear(red), linear(green), linear(blue)];
    let peak = rgb[0].max(rgb[1]).max(rgb[2]);
    if peak > 0.0 {
        [rgb[0] / peak, rgb[1] / peak, rgb[2] / peak]
    } else {
        [1.0, 1.0, 1.0]
    }
}

/// One fire in the scene: a [`FireLight`] at a place, started at a frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FireLightInstance {
    /// World position of the light.
    pub position: Vec3,
    /// Radius of influence in metres.
    pub radius: f32,
    /// The flicker model.
    pub light: FireLight,
    /// Frame (of [`FireLights::frame`]) the fire started on.
    pub start_frame: u64,
}

/// A scatter of fires around a crash site, plus the burning wreck at its center.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrashFires {
    /// The crater: the wreck's glow sits here.
    pub center: Vec3,
    /// Number of grass and tree fires around the crater (the wreck is extra).
    pub count: u32,
    /// Fires land within this distance of the center (metres).
    pub scatter_radius_m: f32,
    /// Seed of the placement and of every fire's flicker.
    pub seed: u64,
    /// Frame the fires start on.
    pub start_frame: u64,
}

/// Burn time of a scattered fire at 60 Hz: at least 3 minutes, up to 6.
const FIRE_MIN_FRAMES: u64 = 3 * 60 * 60;
/// Burn time of the wreck's glow at 60 Hz: 12 minutes.
const WRECK_FRAMES: u64 = 12 * 60 * 60;

/// The live fire lights of a scene and their clock.
///
/// Fires are added with [`spawn`](Self::spawn) or [`spawn_crash`](Self::spawn_crash); the
/// `drive_fire_lights` system adds their current light to [`VfxFrameLights`] once per update and
/// advances [`frame`](Self::frame). Expired fires are dropped.
#[derive(Resource, Debug, Clone, Default)]
pub struct FireLights {
    frame: u64,
    fires: Vec<FireLightInstance>,
}

impl FireLights {
    /// Moves every fire by `-shift` (spec 78 § 12.2).
    pub fn shift_origin(&mut self, shift: Vec3) {
        for fire in &mut self.fires {
            fire.position -= shift;
        }
    }

    /// The current frame of the fire clock.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Sets the clock (a seek). Fires started later than `frame` are dark until it reaches them.
    pub fn set_frame(&mut self, frame: u64) {
        self.frame = frame;
    }

    /// The live fires.
    pub fn fires(&self) -> &[FireLightInstance] {
        &self.fires
    }

    /// Adds one fire.
    pub fn spawn(&mut self, fire: FireLightInstance) {
        self.fires.push(fire);
    }

    /// Removes every fire.
    pub fn clear(&mut self) {
        self.fires.clear();
    }

    /// Adds the fires of a crash site: the wreck's glow at the center (bright, large, orange,
    /// 12 minutes) and `count` scattered fires, each burning 3 to 6 minutes at 60 Hz. Placement
    /// and flicker derive from `spec.seed` only. Returns the number of fires added.
    pub fn spawn_crash(&mut self, spec: &CrashFires) -> usize {
        let radius = if spec.scatter_radius_m.is_finite() {
            spec.scatter_radius_m.max(0.0)
        } else {
            0.0
        };
        self.fires.push(FireLightInstance {
            position: spec.center + Vec3::new(0.0, 1.5, 0.0),
            radius: 60.0,
            light: FireLight {
                base_intensity: 1.0,
                color_temperature_k: 2200.0,
                flicker_depth: 0.25,
                flicker_hz: 4.0,
                seed: mix64(spec.seed ^ 0xA5A5),
                lifetime_frames: WRECK_FRAMES,
            },
            start_frame: spec.start_frame,
        });
        for k in 0..u64::from(spec.count) {
            let h = mix64(spec.seed.wrapping_add(k.wrapping_mul(0x9e37_79b9_7f4a_7c15)));
            let unit = |shift: u32| ((h >> shift) & 0xFFFF) as f32 / 65536.0;
            // Golden-angle spiral with a jittered angle keeps the fires spread, not clumped.
            // The placement trig runs on the deterministic kernel: the same positions on every
            // platform (platform libm cos/sin last digits would scatter the fires per machine).
            let angle = (k as f32) * 2.399_963_2 + unit(0) * 0.6;
            let dist = radius * ((k as f32 + 0.5 + unit(16)) / (spec.count as f32 + 1.0)).sqrt();
            let burn = FIRE_MIN_FRAMES + (unit(32) * (FIRE_MIN_FRAMES as f32)) as u64;
            let (angle_sin, angle_cos) = det_sin_cos(angle);
            self.fires.push(FireLightInstance {
                position: spec.center + Vec3::new(angle_cos * dist, 0.8, angle_sin * dist),
                radius: 20.0 + 12.0 * unit(48),
                light: FireLight {
                    base_intensity: 0.15 + 0.15 * unit(24),
                    color_temperature_k: 1600.0 + 400.0 * unit(40),
                    flicker_depth: 0.4,
                    flicker_hz: 5.0 + 4.0 * unit(8),
                    seed: h,
                    lifetime_frames: burn,
                },
                start_frame: spec.start_frame,
            });
        }
        usize::try_from(spec.count)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
    }

    /// Adds a fire light at every visible ground fire of `crash` ([`crash_fire_placements`]):
    /// the same positions and seeds as the fire effects, so the lit area follows the visible
    /// fires. The light burns as long as its fire's effect. `ground` maps a point of the
    /// crash's ground plane to the terrain point (as for `spawn_crash_on`). The wreck's glow is
    /// not added here ([`spawn_crash`](Self::spawn_crash) with `count` 0 adds it). Returns the
    /// number of fires added.
    pub fn spawn_crash_sites(
        &mut self,
        crash: &CrashBurst,
        start_frame: u64,
        ground: &dyn Fn(Vec3) -> Option<Vec3>,
    ) -> usize {
        let placements = crash_fire_placements(crash, ground);
        for p in &placements {
            let (base, radius) = match p.size {
                FireSize::Small => (0.2, 24.0),
                FireSize::Medium => (0.3, 30.0),
                FireSize::Large => (0.45, 40.0),
            };
            self.fires.push(FireLightInstance {
                position: p.position + Vec3::new(0.0, 0.8, 0.0),
                radius,
                light: FireLight {
                    base_intensity: base,
                    color_temperature_k: 1800.0,
                    flicker_depth: 0.4,
                    flicker_hz: 5.0 + (p.seed & 0xFF) as f32 / 64.0,
                    seed: mix64(p.seed),
                    lifetime_frames: (f64::from(p.size.burn_seconds()) * FIRE_LIGHT_FRAME_HZ) as u64,
                },
                start_frame,
            });
        }
        placements.len()
    }

    /// The point lights of every fire at the current frame, those with a nonzero intensity.
    pub fn candidates(&self) -> Vec<ParticleLight> {
        self.fires
            .iter()
            .filter_map(|fire| {
                let age = self.frame.checked_sub(fire.start_frame)?;
                let sample = fire.light.sample(age);
                (sample.intensity > 0.0 && fire.radius > 0.0).then(|| ParticleLight {
                    position: fire.position,
                    color: [
                        sample.color[0] * sample.intensity,
                        sample.color[1] * sample.intensity,
                        sample.color[2] * sample.intensity,
                    ],
                    radius: fire.radius,
                })
            })
            .collect()
    }

    /// Drops fires that ended and advances the clock one frame.
    fn advance(&mut self) {
        let frame = self.frame;
        self.fires
            .retain(|fire| frame < fire.start_frame.saturating_add(fire.light.lifetime_frames));
        self.frame = frame.saturating_add(1);
    }
}

/// Adds the fires' current lights to the frame's particle lights (re-ranked under the profile's
/// light budget together with the effect lights), then advances the fire clock. A paused app holds
/// the clock: how many idle frames a host runs depends on how fast its clients talk to it, so counting
/// them would change a later render (ADR-2633).
pub fn drive_fire_lights(
    time: Option<Res<regia_app::Time>>,
    settings: Res<crate::plugin::VfxSettings>,
    mut fires: ResMut<FireLights>,
    mut frame_lights: ResMut<VfxFrameLights>,
) {
    let paused = time.is_some_and(|time| time.paused);
    if fires.fires.is_empty() {
        if !paused {
            fires.frame = fires.frame.saturating_add(1);
        }
        return;
    }
    let _span = tracing::info_span!("vfx_fire_lights", fires = fires.fires.len()).entered();
    let mut all = std::mem::take(&mut frame_lights.lights);
    all.extend(fires.candidates());
    frame_lights.gather(all, settings.profile.light_budget);
    if !paused {
        fires.advance();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fire(seed: u64) -> FireLight {
        FireLight {
            base_intensity: 2.0,
            color_temperature_k: 1800.0,
            flicker_depth: 0.5,
            flicker_hz: 7.0,
            seed,
            lifetime_frames: 1000,
        }
    }

    #[test]
    fn sample_is_a_pure_function_of_frame_and_seed() {
        let a = fire(42);
        for frame in [0, 1, 17, 500, 999, 1000, 5000] {
            assert_eq!(a.sample(frame), fire(42).sample(frame), "frame {frame}");
        }
        let differs = (0..200).any(|f| a.sample(f) != fire(43).sample(f));
        assert!(differs, "a different seed flickers differently");
        // The colour bits are pinned across platforms: every transcendental in the blackbody
        // fit runs on the deterministic kernels, not the platform libm.
        assert_eq!(
            a.sample(0).color.map(f32::to_bits),
            SAMPLE_COLOR_PINNED_BITS,
            "blackbody colour bits changed"
        );
    }

    const SAMPLE_COLOR_PINNED_BITS: [u32; 3] = [1065353216, 1046023010, 0]; // pinned 2026-11-09

    #[test]
    fn spawn_crash_positions_are_pinned() {
        let spec = CrashFires {
            center: Vec3::new(11.0, 2.0, -3.0),
            count: 5,
            scatter_radius_m: 40.0,
            seed: 0x5eed,
            start_frame: 0,
        };
        let mut fires = FireLights::default();
        fires.spawn_crash(&spec);
        let pinned: Vec<[u32; 3]> = fires
            .fires()
            .iter()
            .map(|f| f.position.to_array().map(f32::to_bits))
            .collect();
        assert_eq!(
            pinned, CRASH_POSITIONS_PINNED_BITS,
            "crash fire positions changed"
        );
    }

    const CRASH_POSITIONS_PINNED_BITS: [[u32; 3]; 6] = [
        [1093664768, 1080033280, 3225419776],
        [1106226381, 1077097267, 3216070515],
        [3237846048, 1077097267, 1081197104],
        [1101645152, 1077097267, 3252322246],
        [1104516354, 1077097267, 1104176650],
        [3250074252, 1077097267, 3246464863],
    ]; // pinned 2026-11-09 (deterministic trig placement)

    #[test]
    fn intensity_is_bounded_and_never_negative() {
        let light = fire(7);
        for frame in 0..1200 {
            let s = light.sample(frame);
            assert!(s.intensity >= 0.0 && s.intensity.is_finite(), "frame {frame}");
            assert!(
                s.intensity <= 2.0 * (1.0 + 0.5) + 1e-4,
                "frame {frame}: {}",
                s.intensity
            );
            let peak = s.color.iter().copied().fold(0.0, f32::max);
            assert!((peak - 1.0).abs() < 1e-5, "colour peak is 1");
        }
    }

    #[test]
    fn flicker_actually_varies_and_depth_zero_is_steady() {
        let light = fire(3);
        let values: Vec<f32> = (0..300).map(|f| light.sample(f).intensity).collect();
        let min = values.iter().copied().fold(f32::MAX, f32::min);
        let max = values.iter().copied().fold(0.0, f32::max);
        assert!(max - min > 0.1, "flicker swings: {min}..{max}");
        let steady = FireLight {
            flicker_depth: 0.0,
            ..light
        };
        for frame in 0..800 {
            assert_eq!(steady.sample(frame).intensity, 2.0);
        }
    }

    #[test]
    fn flicker_is_smooth_between_frames() {
        let light = fire(11);
        let mut worst = 0.0f32;
        for frame in 0..800 {
            worst = worst.max((light.sample(frame + 1).intensity - light.sample(frame).intensity).abs());
        }
        assert!(worst < 0.25, "no frame-to-frame pops: {worst}");
    }

    #[test]
    fn zero_after_lifetime_and_fade_is_monotonic_over_the_last_tenth() {
        let light = fire(5);
        assert_eq!(light.sample(1000).intensity, 0.0);
        assert_eq!(light.sample(1001).intensity, 0.0);
        assert_eq!(light.sample(u64::MAX).intensity, 0.0);
        assert_eq!(light.fade(900), 1.0);
        let mut last = 1.0f32;
        for frame in 900..=1000 {
            let f = light.fade(frame);
            assert!((0.0..=1.0).contains(&f));
            assert!(f <= last + 1e-7, "fade rises at {frame}");
            last = f;
        }
        assert_eq!(light.fade(1000), 0.0);
        assert!(light.fade(999) < 0.01, "ends smoothly, not with a step");
    }

    #[test]
    fn zero_lifetime_is_always_dark() {
        let light = FireLight {
            lifetime_frames: 0,
            ..fire(1)
        };
        for frame in [0, 1, 100] {
            assert_eq!(light.sample(frame).intensity, 0.0);
        }
    }

    #[test]
    fn bad_numbers_never_produce_nan_or_negative_light() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -3.0] {
            let light = FireLight {
                base_intensity: bad,
                color_temperature_k: bad,
                flicker_depth: bad,
                flicker_hz: bad,
                seed: 9,
                lifetime_frames: 100,
            };
            for frame in 0..120 {
                let s = light.sample(frame);
                assert!(
                    s.intensity.is_finite() && s.intensity >= 0.0,
                    "{bad} frame {frame}"
                );
                assert!(s.color.iter().all(|c| c.is_finite() && *c >= 0.0), "{bad}");
            }
        }
    }

    #[test]
    fn blackbody_is_orange_when_cool_white_when_hot_and_blue_when_hotter() {
        let fire = blackbody_linear_rgb(1800.0);
        assert_eq!(fire[0], 1.0);
        assert!(fire[0] > fire[1] && fire[1] > fire[2], "{fire:?}");
        let noon = blackbody_linear_rgb(6600.0);
        assert!(
            (noon[0] - noon[2]).abs() < 0.05 && (noon[1] - noon[2]).abs() < 0.05,
            "{noon:?}"
        );
        let blue = blackbody_linear_rgb(12000.0);
        assert!(blue[2] > blue[0], "{blue:?}");
        // Out-of-range and non-finite input clamps instead of failing.
        assert_eq!(blackbody_linear_rgb(0.0), blackbody_linear_rgb(1000.0));
        assert_eq!(blackbody_linear_rgb(1e9), blackbody_linear_rgb(40000.0));
        assert!(blackbody_linear_rgb(f32::NAN).iter().all(|c| c.is_finite()));
    }

    #[test]
    fn crash_fires_are_deterministic_and_burn_at_least_three_minutes() {
        let spec = CrashFires {
            center: Vec3::new(100.0, 5.0, -40.0),
            count: 50,
            scatter_radius_m: 300.0,
            seed: 2026,
            start_frame: 600,
        };
        let mut a = FireLights::default();
        let mut b = FireLights::default();
        assert_eq!(a.spawn_crash(&spec), 51);
        b.spawn_crash(&spec);
        assert_eq!(a.fires(), b.fires());
        let mut c = FireLights::default();
        c.spawn_crash(&CrashFires { seed: 2027, ..spec });
        assert_ne!(a.fires(), c.fires());
        for fire in a.fires() {
            assert!(fire.light.lifetime_frames >= 3 * 60 * 60);
            let d = (fire.position - spec.center).length();
            assert!(d <= 300.0 + 2.0, "within the scatter radius: {d}");
            assert_eq!(fire.start_frame, 600);
        }
        assert_eq!(a.fires().len(), 51);
    }

    /// ADR-2633: a paused app's idle frames depend on how fast its clients talk to it, so they do
    /// not reach the fire clock; a running app's frames do.
    #[test]
    fn a_paused_app_holds_the_fire_clock() {
        for live_fire in [false, true] {
            let mut world = World::new();
            let mut time = regia_app::Time::default();
            time.paused = true;
            world.insert_resource(time);
            world.insert_resource(crate::plugin::VfxSettings::default());
            world.insert_resource(VfxFrameLights::default());
            let mut fires = FireLights::default();
            if live_fire {
                fires.spawn(FireLightInstance {
                    position: Vec3::ZERO,
                    radius: 5.0,
                    light: fire(1),
                    start_frame: 0,
                });
            }
            world.insert_resource(fires);
            let mut schedule = Schedule::default();
            schedule.add_systems(drive_fire_lights);
            schedule.run(&mut world);
            schedule.run(&mut world);
            assert_eq!(world.resource::<FireLights>().frame(), 0, "live fire {live_fire}");
            world.resource_mut::<regia_app::Time>().paused = false;
            schedule.run(&mut world);
            assert_eq!(world.resource::<FireLights>().frame(), 1, "live fire {live_fire}");
        }
    }

    #[test]
    fn candidates_follow_the_clock_and_drop_expired_fires() {
        let mut fires = FireLights::default();
        fires.spawn(FireLightInstance {
            position: Vec3::ZERO,
            radius: 5.0,
            light: FireLight {
                lifetime_frames: 20,
                flicker_depth: 0.0,
                ..fire(1)
            },
            start_frame: 10,
        });
        assert!(fires.candidates().is_empty(), "not started at frame 0");
        fires.set_frame(10);
        assert_eq!(fires.candidates().len(), 1);
        fires.set_frame(30);
        fires.advance();
        assert!(fires.fires().is_empty(), "expired fires are dropped");
        assert_eq!(fires.frame(), 31);
    }

    #[test]
    fn crash_site_lights_sit_on_the_visible_fires_with_their_seeds() {
        use crate::crash::{CrashBurst, crash_fire_placements};
        let crash = CrashBurst {
            position: Vec3::new(700.0, 105.7, 5.0),
            ..CrashBurst::default()
        };
        let flat = |p: Vec3| Some(p);
        let placements = crash_fire_placements(&crash, &flat);
        assert_eq!(placements.len() as u32, crash.scale.fire_count());
        let mut lights = FireLights::default();
        let added = lights.spawn_crash_sites(&crash, 7, &flat);
        assert_eq!(added, placements.len());
        assert_eq!(lights.fires().len(), placements.len(), "no wreck glow here");
        for (fire, place) in lights.fires().iter().zip(&placements) {
            assert_eq!(fire.position, place.position + Vec3::new(0.0, 0.8, 0.0));
            assert_eq!(fire.start_frame, 7);
            assert!(fire.radius > 0.0 && fire.light.lifetime_frames >= 200 * 60);
        }
        // Same crash, same lights; a different seed moves them.
        let mut again = FireLights::default();
        again.spawn_crash_sites(&crash, 7, &flat);
        assert_eq!(lights.fires(), again.fires());
        let mut other = FireLights::default();
        other.spawn_crash_sites(&CrashBurst { seed: 99, ..crash }, 7, &flat);
        assert_ne!(lights.fires(), other.fires());
        // Fires off: no lights.
        let mut none = FireLights::default();
        assert_eq!(
            none.spawn_crash_sites(
                &CrashBurst {
                    fires: false,
                    ..crash
                },
                0,
                &flat
            ),
            0
        );
        // A ground sampler that skips a point skips the light, like the fire effect (the flat
        // sampler returns the ground point unchanged, so the first placement is its own point).
        let hole = placements[0].position;
        let mut holes = FireLights::default();
        let kept = holes.spawn_crash_sites(&crash, 0, &|p| (p != hole).then_some(p));
        assert_eq!(kept, placements.len() - 1);
        assert!(
            holes
                .fires()
                .iter()
                .all(|f| f.position != hole + Vec3::new(0.0, 0.8, 0.0))
        );
    }
}
