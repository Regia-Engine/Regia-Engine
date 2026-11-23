// SPDX-License-Identifier: MPL-2.0
//! Point ambiences (ADR-2442): a stream that loops where it runs, birds that call from random
//! places around a tree line. Each [`AmbientSpot`] is heard through the ordinary emitter
//! pipeline (spec 16 § 3): distance gain by the spot's rolloff, a distance low-pass (the
//! emitter's air-absorption ramp and the level medium's absorption) and, with
//! [`AmbientSpot::occlusion`], physics occlusion (a hill or a trunk between the listener and the
//! spot muffles it). What the spot adds is the scheduling: a loop spot carries one looping
//! [`AudioEmitter`]; a scatter spot spawns a short-lived emitter entity per call, at a random
//! point in its radius, picking a variant that is never the previous one, at random intervals.
//! Both fall quiet with the weather (birds stop in rain and in a gale).
//!
//! [`spot_hearing`] is the pure probe of what a listener at a point hears of a spot (the same
//! distance model, cutoff and occlusion formulas), used by tests and `audio.probe_ambience`.

use regia_core::math::{Rng, Vec3};
use regia_ecs::prelude::*;

use crate::medium::AudioMedium;
use crate::occlusion::OcclusionTuning;
use crate::spatial::{AudioEmitter, EmitterFrame, ListenerFrame, RolloffCurve, gain_to_db, spatialize};

use super::bed::AmbienceInputs;

/// How a spot sounds over time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AmbientSpotKind {
    /// One looping sound at the spot (water, a waterfall, insects in a reed bed).
    Loop,
    /// Short calls at random intervals from random points around the spot (birds).
    Scatter {
        /// Shortest wait between two calls, seconds.
        min_interval_secs: f32,
        /// Longest wait between two calls, seconds.
        max_interval_secs: f32,
        /// Calls come from within this horizontal radius of the spot, metres.
        radius_m: f32,
        /// Calls come from up to this height above the spot, metres (a canopy).
        height_m: f32,
    },
}

impl AmbientSpotKind {
    /// The stable lowercase name the agent protocol uses (`loop`, `scatter`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::Scatter { .. } => "scatter",
        }
    }
}

/// A point ambience (opt-in): put it on an entity with a transform. `AudioPlugin`'s ambience
/// system plays it while at least one is in the world.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct AmbientSpot {
    /// The variants, by [`crate::SoundAssets`] name. A loop spot plays the first.
    pub sounds: Vec<String>,
    /// Loop or scattered calls.
    pub kind: AmbientSpotKind,
    /// Level at `min_distance`, dB.
    pub volume_db: f32,
    /// Full level inside this distance, metres.
    pub min_distance: f32,
    /// Silent beyond this distance, metres (no voice and no call is started further away).
    pub max_distance: f32,
    /// How the level falls with distance.
    pub rolloff: RolloffCurve,
    /// Physics occlusion muffles and quietens the spot (needs an occlusion provider).
    pub occlusion: bool,
    /// Quiet while the wind now is at or above this speed, m/s (`None` = never).
    pub quiet_above_wind_m_per_s: Option<f32>,
    /// Quiet while the rain is at or above this rate, mm/h (`None` = never).
    pub quiet_above_rain_mm_per_h: Option<f32>,
    /// The bus the spot plays on.
    pub bus: String,
    /// Seed of the call schedule and the variant picks (same seed, same calls).
    pub seed: u64,
}

impl AmbientSpot {
    /// A looping water spot: heard to 60 m, log rolloff (6 dB per doubling), occluded, never
    /// silenced by the weather.
    #[must_use]
    pub fn water(sound: impl Into<String>) -> Self {
        Self {
            sounds: vec![sound.into()],
            kind: AmbientSpotKind::Loop,
            volume_db: -6.0,
            min_distance: 3.0,
            max_distance: 60.0,
            rolloff: RolloffCurve::Log { db_per_doubling: 6.0 },
            occlusion: true,
            quiet_above_wind_m_per_s: None,
            quiet_above_rain_mm_per_h: None,
            bus: "Ambience".into(),
            seed: 0,
        }
    }

    /// Birds in a tree line: a call every 2 to 9 s from within 15 m and 12 m up, heard to 120 m,
    /// quiet in rain over 1 mm/h and in wind over 12 m/s.
    #[must_use]
    pub fn birds(sounds: Vec<String>) -> Self {
        Self {
            sounds,
            kind: AmbientSpotKind::Scatter {
                min_interval_secs: 2.0,
                max_interval_secs: 9.0,
                radius_m: 15.0,
                height_m: 12.0,
            },
            volume_db: -8.0,
            min_distance: 4.0,
            max_distance: 120.0,
            rolloff: RolloffCurve::Log { db_per_doubling: 6.0 },
            occlusion: true,
            quiet_above_wind_m_per_s: Some(12.0),
            quiet_above_rain_mm_per_h: Some(1.0),
            bus: "Ambience".into(),
            seed: 0,
        }
    }

    /// `true` while the weather silences the spot.
    #[must_use]
    pub fn quieted_by(&self, inputs: &AmbienceInputs) -> bool {
        self.quiet_above_wind_m_per_s
            .is_some_and(|limit| inputs.wind_m_per_s >= limit)
            || self
                .quiet_above_rain_mm_per_h
                .is_some_and(|limit| inputs.rain_mm_per_h >= limit)
    }

    /// The emitter the spot (or one of its calls) plays through: the spot's range, rolloff and
    /// bus, air absorption on, the occlusion flag, `volume_db` plus `offset_db`.
    #[must_use]
    pub fn emitter(&self, sound: &str, offset_db: f32) -> AudioEmitter {
        AudioEmitter {
            sound: None,
            sound_name: sound.to_string(),
            autoplay: true,
            looping: matches!(self.kind, AmbientSpotKind::Loop),
            volume_db: self.volume_db + offset_db,
            min_distance: self.min_distance,
            max_distance: self.max_distance,
            rolloff: self.rolloff.clone(),
            air_absorption: true,
            occlusion: self.occlusion,
            bus: Some(self.bus.clone()),
            ..AudioEmitter::default()
        }
    }

    /// The horizontal reach of the spot: `max_distance` plus the scatter radius.
    #[must_use]
    pub fn reach(&self) -> f32 {
        match self.kind {
            AmbientSpotKind::Loop => self.max_distance,
            AmbientSpotKind::Scatter { radius_m, .. } => self.max_distance + radius_m.max(0.0),
        }
    }
}

/// What a listener hears of a spot (or of one call at `position`), [`spot_hearing`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpotHearing {
    /// Listener-to-sound distance, metres.
    pub distance: f32,
    /// Amplitude after distance gain and occlusion, including the spot's `volume_db`.
    pub gain: f32,
    /// [`Self::gain`] in dB.
    pub gain_db: f32,
    /// The low-pass cutoff the sound is heard through, Hz: the lowest of the emitter's distance
    /// ramp, the medium's absorption and the occlusion filter.
    pub cutoff_hz: f32,
    /// The occlusion amount used, `0..1`.
    pub occlusion: f32,
    /// Above the audibility floor (−60 dB) and not silenced by the weather.
    pub audible: bool,
    /// The weather silences the spot.
    pub quieted: bool,
}

/// What a listener at `listener` hears of `spot` sounding at `position`, with `occlusion` (0..1)
/// between them, in `medium`, under `inputs`. The same distance gain ([`spatialize`]), distance
/// low-pass and occlusion formulas ([`OcclusionTuning`]) the live emitter uses.
#[must_use]
pub fn spot_hearing(
    spot: &AmbientSpot,
    listener: Vec3,
    position: Vec3,
    occlusion: f32,
    medium: &AudioMedium,
    tuning: &OcclusionTuning,
    inputs: &AmbienceInputs,
) -> SpotHearing {
    let name = spot.sounds.first().map_or("", String::as_str);
    let emitter = spot.emitter(name, 0.0);
    let frame = spatialize(
        &ListenerFrame::looking_at(listener, Vec3::NEG_Z),
        &EmitterFrame {
            position,
            ..EmitterFrame::default()
        },
        &emitter,
    );
    let occlusion = if spot.occlusion && occlusion.is_finite() {
        occlusion.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let distance = frame.distance;
    let air = frame.cutoff_hz.map_or(20_000.0, |c| c as f32);
    let medium_cutoff = if distance.is_finite() {
        medium.absorption_cutoff_hz(distance)
    } else {
        crate::medium::MIN_CUTOFF_HZ
    };
    let cutoff_hz = air.min(medium_cutoff).min(tuning.cutoff_hz(occlusion) as f32);
    let level = crate::spatial::db_to_gain(spot.volume_db);
    let gain = frame.gain * tuning.gain(occlusion) * if level.is_finite() { level } else { 0.0 };
    let gain = if gain.is_finite() { gain.max(0.0) } else { 0.0 };
    let quieted = spot.quieted_by(inputs);
    let gain_db = if gain > 0.0 {
        gain_to_db(gain)
    } else {
        crate::bus::MUTE_DB
    };
    SpotHearing {
        distance,
        gain,
        gain_db,
        cutoff_hz,
        occlusion,
        audible: !quieted && gain_db > -60.0,
        quieted,
    }
}

/// Picks one of `count` variants, never `last` when there are two or more.
#[must_use]
pub fn pick_variant(rng: &mut Rng, count: usize, last: Option<usize>) -> Option<usize> {
    match (count, last) {
        (0, _) => None,
        (1, _) => Some(0),
        (n, Some(last)) if last < n => {
            // One of the other n − 1, uniformly.
            let draw = rng.range(0..n - 1);
            Some(if draw >= last { draw + 1 } else { draw })
        }
        (n, _) => Some(rng.range(0..n)),
    }
}

/// The live schedule of one spot (kept by the ambience system).
#[derive(Clone, Debug)]
pub struct SpotSchedule {
    /// Call generator (seeded from the spot's seed and entity).
    pub rng: Rng,
    /// Seconds until the next call.
    pub next_call_in_secs: f32,
    /// Variant of the previous call.
    pub last_variant: Option<usize>,
    /// Calls started so far.
    pub calls: u64,
    /// Calls skipped because the weather silenced the spot or the listener was out of reach.
    pub skipped: u64,
    /// The loop emitter is in place (loop spots).
    pub looping: bool,
    /// The weather offset the loop emitter plays with now, dB (slewed to silence when quieted).
    pub weather_db: f32,
    /// Occlusion of the latest call when it ended, the next call starts from it.
    pub last_occlusion: f32,
}

impl SpotSchedule {
    /// A fresh schedule; the first call waits one interval.
    #[must_use]
    pub fn new(spot: &AmbientSpot, entity_bits: u64) -> Self {
        let mut rng = Rng::with_stream(spot.seed, entity_bits);
        let next = next_interval(spot, &mut rng);
        Self {
            rng,
            next_call_in_secs: next,
            last_variant: None,
            calls: 0,
            skipped: 0,
            looping: false,
            weather_db: 0.0,
            last_occlusion: 0.0,
        }
    }
}

/// A random wait between calls (0 for a loop spot).
pub fn next_interval(spot: &AmbientSpot, rng: &mut Rng) -> f32 {
    match spot.kind {
        AmbientSpotKind::Loop => 0.0,
        AmbientSpotKind::Scatter {
            min_interval_secs,
            max_interval_secs,
            ..
        } => {
            let lo = if min_interval_secs.is_finite() {
                min_interval_secs.max(0.05)
            } else {
                1.0
            };
            let hi = if max_interval_secs.is_finite() {
                max_interval_secs.max(lo)
            } else {
                lo
            };
            lo + (hi - lo) * rng.f32()
        }
    }
}

/// A random call position around `centre` for a scatter spot (`centre` for a loop).
pub fn call_position(spot: &AmbientSpot, centre: Vec3, rng: &mut Rng) -> Vec3 {
    match spot.kind {
        AmbientSpotKind::Loop => centre,
        AmbientSpotKind::Scatter {
            radius_m, height_m, ..
        } => {
            let r = if radius_m.is_finite() {
                radius_m.max(0.0)
            } else {
                0.0
            };
            let h = if height_m.is_finite() {
                height_m.max(0.0)
            } else {
                0.0
            };
            // Uniform over the disc: radius ∝ √u.
            let radius = r * rng.f32().sqrt();
            let angle = std::f32::consts::TAU * rng.f32();
            centre + Vec3::new(radius * angle.cos(), h * rng.f32(), radius * angle.sin())
        }
    }
}

/// Marks a short-lived call entity a scatter spot spawned; the ambience system despawns it once
/// its sound has played.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct AmbientCall {
    /// The spot that called.
    pub spot: Entity,
    /// Seconds left before the entity is despawned.
    pub remaining_secs: f32,
    /// Variant index played.
    pub variant: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn birds() -> AmbientSpot {
        AmbientSpot::birds(vec!["a".into(), "b".into(), "c".into()])
    }

    fn hear(spot: &AmbientSpot, d: f32, occ: f32) -> SpotHearing {
        spot_hearing(
            spot,
            Vec3::ZERO,
            Vec3::new(d, 0.0, 0.0),
            occ,
            &AudioMedium::default(),
            &OcclusionTuning::default(),
            &AmbienceInputs::default(),
        )
    }

    #[test]
    fn distance_lowers_the_level_and_the_cutoff() {
        let spot = AmbientSpot::water("stream");
        let mut last = hear(&spot, 1.0, 0.0);
        assert!(
            (last.gain_db - spot.volume_db).abs() < 0.01,
            "full level inside min distance"
        );
        for d in [5.0, 10.0, 20.0, 40.0, 55.0] {
            let h = hear(&spot, d, 0.0);
            assert!(h.gain < last.gain, "{d} m: {} after {}", h.gain_db, last.gain_db);
            assert!(
                h.cutoff_hz < last.cutoff_hz,
                "{d} m: {} Hz after {} Hz",
                h.cutoff_hz,
                last.cutoff_hz
            );
            last = h;
        }
        assert!(
            last.cutoff_hz < 5_000.0,
            "far water is dull: {} Hz",
            last.cutoff_hz
        );
        let beyond = hear(&spot, 61.0, 0.0);
        assert_eq!(beyond.gain, 0.0);
        assert!(!beyond.audible);
    }

    #[test]
    fn occlusion_muffles_and_quietens() {
        let spot = AmbientSpot::water("stream");
        let open = hear(&spot, 10.0, 0.0);
        let hidden = hear(&spot, 10.0, 1.0);
        assert!(
            (open.gain_db - hidden.gain_db - 15.0).abs() < 0.1,
            "{open:?} {hidden:?}"
        );
        assert!(hidden.cutoff_hz <= 700.5, "{}", hidden.cutoff_hz);
        let mut unoccluded = spot.clone();
        unoccluded.occlusion = false;
        assert_eq!(
            hear(&unoccluded, 10.0, 1.0).gain,
            open.gain,
            "occlusion off ignores reports"
        );
        assert_eq!(hear(&spot, 10.0, f32::NAN).gain, open.gain);
    }

    #[test]
    fn weather_quiets_birds_but_not_water() {
        let rain = AmbienceInputs {
            rain_mm_per_h: 4.0,
            ..AmbienceInputs::default()
        };
        let gale = AmbienceInputs {
            wind_m_per_s: 15.0,
            mean_wind_m_per_s: 12.0,
            ..AmbienceInputs::default()
        };
        assert!(birds().quieted_by(&rain));
        assert!(birds().quieted_by(&gale));
        assert!(!birds().quieted_by(&AmbienceInputs::default()));
        assert!(!AmbientSpot::water("s").quieted_by(&rain));
    }

    #[test]
    fn variants_never_repeat_back_to_back() {
        let mut rng = Rng::new(7);
        let mut last = None;
        let mut seen = [0usize; 3];
        for _ in 0..3000 {
            let pick = pick_variant(&mut rng, 3, last).expect("three variants");
            assert_ne!(Some(pick), last, "an immediate repeat");
            seen[pick] += 1;
            last = Some(pick);
        }
        assert!(seen.iter().all(|&n| n > 800), "every variant is used: {seen:?}");
        assert_eq!(
            pick_variant(&mut rng, 1, Some(0)),
            Some(0),
            "one variant must repeat"
        );
        assert_eq!(pick_variant(&mut rng, 0, None), None);
        assert!(
            pick_variant(&mut rng, 2, Some(9)).is_some(),
            "a stale index is ignored"
        );
    }

    #[test]
    fn calls_stay_inside_the_scatter_volume_and_intervals_inside_their_range() {
        let spot = birds();
        let mut rng = Rng::new(3);
        let centre = Vec3::new(10.0, 2.0, -4.0);
        for _ in 0..500 {
            let p = call_position(&spot, centre, &mut rng);
            let off = p - centre;
            assert!(
                off.x.hypot(off.z) <= 15.0 + 1e-3 && (0.0..=12.0).contains(&off.y),
                "{off:?}"
            );
            let wait = next_interval(&spot, &mut rng);
            assert!((2.0..=9.0).contains(&wait), "{wait}");
        }
        assert_eq!(call_position(&AmbientSpot::water("s"), centre, &mut rng), centre);
    }

    #[test]
    fn the_same_seed_gives_the_same_schedule() {
        let a = SpotSchedule::new(&birds(), 42);
        let b = SpotSchedule::new(&birds(), 42);
        let c = SpotSchedule::new(&birds(), 43);
        assert_eq!(a.next_call_in_secs, b.next_call_in_secs);
        assert_ne!(a.next_call_in_secs, c.next_call_in_secs);
    }
}
