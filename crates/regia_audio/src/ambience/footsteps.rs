// SPDX-License-Identifier: MPL-2.0
//! Surface footsteps (ADR-2442): a foot event (an animation notify, a controller step) names a
//! walker, a position, a surface, a speed and a weight; the [`FootstepBank`] maps the surface to
//! a set of variants, picks one that is not the walker's previous step on that surface, scales
//! the level and pitch with speed and weight, and plays it at the foot through the emitter
//! pipeline (distance gain, panning, the medium).
//!
//! The bank is pure apart from [`FootstepBank::play`]; `regia_framework::footsteps` turns
//! animation notifies into [`FootstepEvent`]s and finds the surface (a walker override, a host
//! probe, or the terrain layer under the foot through [`FootstepBank::layer_surfaces`]).

use std::collections::{BTreeMap, HashMap, VecDeque};

use regia_core::math::{Rng, Vec3};
use regia_ecs::prelude::*;

use crate::audio::Audio;
use crate::error::AudioError;
use crate::sound::PlaybackParams;
use crate::sounds::SoundAssets;
use crate::spatial::{AudioEmitter, RolloffCurve};

use super::spots::pick_variant;

/// How many recent steps [`FootstepBank::recent`] keeps.
pub const FOOTSTEP_LOG_LEN: usize = 32;

/// The variants of one surface.
#[derive(Clone, Debug, PartialEq)]
pub struct FootstepSet {
    /// Variant sounds, by [`SoundAssets`] name. Four or more avoid an audible pattern.
    pub sounds: Vec<String>,
    /// Base level, dB.
    pub volume_db: f32,
    /// Base pitch ratio.
    pub pitch: f32,
    /// Random pitch spread, ± this fraction (0.04 = ±4 %).
    pub pitch_jitter: f32,
    /// Random level spread, ± dB.
    pub volume_jitter_db: f32,
}

impl FootstepSet {
    /// A set with ±4 % pitch and ±1.5 dB spread at 0 dB.
    #[must_use]
    pub fn new(sounds: Vec<String>) -> Self {
        Self {
            sounds,
            volume_db: 0.0,
            pitch: 1.0,
            pitch_jitter: 0.04,
            volume_jitter_db: 1.5,
        }
    }
}

/// One foot touching the ground.
#[derive(Clone, Debug, PartialEq)]
pub struct FootstepEvent {
    /// Who stepped (an entity's bits); steps of one walker never repeat a variant back to back.
    pub walker: u64,
    /// Where the foot is, world metres.
    pub position: Vec3,
    /// The surface under the foot; `None` (or an unknown one) uses [`FootstepBank::default_surface`].
    pub surface: Option<String>,
    /// Ground speed of the walker, m/s.
    pub speed_m_per_s: f32,
    /// Mass of the walker, kg.
    pub weight_kg: f32,
}

/// What a step played (or would play, [`FootstepBank::plan`]).
#[derive(Clone, Debug, PartialEq)]
pub struct FootstepPlay {
    /// The walker.
    pub walker: u64,
    /// The surface set used.
    pub surface: String,
    /// `true` when the requested surface had no set and the default one played.
    pub fallback: bool,
    /// The variant index in the set.
    pub variant: usize,
    /// The variant's sound name.
    pub sound: String,
    /// Level before distance, dB (set, speed, weight and spread).
    pub volume_db: f32,
    /// Pitch ratio (set, weight and spread).
    pub pitch: f32,
    /// Where it played.
    pub position: Vec3,
    /// The sound was registered and a voice started (false for a plan, or a missing sound).
    pub started: bool,
}

/// The surface-to-footstep table and its scaling (opt-in resource).
#[derive(Resource, Clone, Debug)]
pub struct FootstepBank {
    /// Surface name → variants.
    pub sets: BTreeMap<String, FootstepSet>,
    /// Terrain layer name → surface name (a layer not listed uses its own name as the surface).
    pub layer_surfaces: BTreeMap<String, String>,
    /// The surface used when the event names none or an unknown one.
    pub default_surface: String,
    /// The speed a step plays at its set's level, m/s (a walk).
    pub walk_speed_m_per_s: f32,
    /// Level change per doubling of speed, dB (a run is louder than a walk).
    pub speed_db_per_doubling: f32,
    /// The weight a step plays at its set's level and pitch, kg.
    pub reference_weight_kg: f32,
    /// Level change per doubling of weight, dB.
    pub weight_db_per_doubling: f32,
    /// Pitch exponent of the weight: `pitch × (reference / weight)^k` (heavier is lower).
    pub weight_pitch_exponent: f32,
    /// The speed and weight terms together stay within `−max_cut_db..=max_boost_db`.
    pub max_boost_db: f32,
    /// See [`Self::max_boost_db`].
    pub max_cut_db: f32,
    /// Animation notifies whose name starts with this are foot events (`Footstep_L`).
    pub notify_prefix: String,
    /// The emitter a step plays through (range, rolloff, bus).
    pub emitter: AudioEmitter,
    history: HashMap<(u64, String), usize>,
    rng: Rng,
    recent: VecDeque<FootstepPlay>,
    played: u64,
}

impl Default for FootstepBank {
    fn default() -> Self {
        Self::new()
    }
}

impl FootstepBank {
    /// An empty bank: default surface `default`, walk 1.4 m/s at +4 dB per doubling of speed,
    /// 75 kg at +3 dB per doubling of weight and pitch exponent 0.15, the terms within −12..+6 dB,
    /// notify prefix `Footstep`, heard to 25 m on the `World` bus.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sets: BTreeMap::new(),
            layer_surfaces: BTreeMap::new(),
            default_surface: "default".into(),
            walk_speed_m_per_s: 1.4,
            speed_db_per_doubling: 4.0,
            reference_weight_kg: 75.0,
            weight_db_per_doubling: 3.0,
            weight_pitch_exponent: 0.15,
            max_boost_db: 6.0,
            max_cut_db: 12.0,
            notify_prefix: "Footstep".into(),
            emitter: AudioEmitter {
                min_distance: 1.0,
                max_distance: 25.0,
                rolloff: RolloffCurve::Log { db_per_doubling: 6.0 },
                air_absorption: true,
                bus: Some("World".into()),
                autoplay: false,
                ..AudioEmitter::default()
            },
            history: HashMap::new(),
            rng: Rng::new(0x0F00_7573_7465_7073),
            recent: VecDeque::new(),
            played: 0,
        }
    }

    /// Reseeds the variant and spread generator and forgets every walker's history.
    pub fn reseed(&mut self, seed: u64) {
        self.rng = Rng::new(seed);
        self.history.clear();
    }

    /// The surface a terrain layer maps to: its entry in [`Self::layer_surfaces`], else the layer
    /// name itself.
    #[must_use]
    pub fn surface_for_layer<'a>(&'a self, layer: &'a str) -> &'a str {
        self.layer_surfaces.get(layer).map_or(layer, String::as_str)
    }

    /// The set a requested surface plays: `(surface, fallback)`. `None` when neither the surface
    /// nor the default has a set.
    #[must_use]
    pub fn resolve(&self, surface: Option<&str>) -> Option<(&str, bool)> {
        if let Some(s) = surface
            && let Some((name, _)) = self.sets.get_key_value(s)
        {
            return Some((name.as_str(), false));
        }
        self.sets
            .get_key_value(&self.default_surface)
            .map(|(name, _)| (name.as_str(), surface.is_some()))
    }

    /// The speed and weight level terms of a step, dB, within the bank's limits.
    #[must_use]
    pub fn scale_db(&self, speed_m_per_s: f32, weight_kg: f32) -> f32 {
        // An unknown (non-finite) speed plays as a walk; standing still is floored at a quarter of it.
        let speed_m_per_s = if speed_m_per_s.is_finite() {
            speed_m_per_s.max(0.25 * self.walk_speed_m_per_s)
        } else {
            self.walk_speed_m_per_s
        };
        let speed = log2_ratio(speed_m_per_s, self.walk_speed_m_per_s);
        let weight = log2_ratio(weight_kg, self.reference_weight_kg);
        let db = self.speed_db_per_doubling * speed + self.weight_db_per_doubling * weight;
        let db = if db.is_finite() { db } else { 0.0 };
        db.clamp(-self.max_cut_db.abs(), self.max_boost_db.abs())
    }

    /// The weight pitch factor: `(reference / weight)^exponent`, within 0.75..1.33.
    #[must_use]
    pub fn weight_pitch(&self, weight_kg: f32) -> f32 {
        let ratio = log2_ratio(weight_kg, self.reference_weight_kg);
        let p = (-ratio * self.weight_pitch_exponent).exp2();
        if p.is_finite() { p.clamp(0.75, 1.33) } else { 1.0 }
    }

    /// Picks the variant and levels of a step without playing it; advances the walker's
    /// history (the next step of the walker on that surface will not repeat it).
    pub fn plan(&mut self, event: &FootstepEvent) -> Option<FootstepPlay> {
        let (surface, fallback) = self
            .resolve(event.surface.as_deref())
            .map(|(s, f)| (s.to_string(), f))?;
        let set = self.sets.get(&surface)?;
        let key = (event.walker, surface.clone());
        let last = self.history.get(&key).copied();
        let variant = pick_variant(&mut self.rng, set.sounds.len(), last)?;
        let spread = |rng: &mut Rng, amount: f32| {
            let a = if amount.is_finite() { amount.abs() } else { 0.0 };
            (rng.f32() * 2.0 - 1.0) * a
        };
        let volume_db = set.volume_db
            + self.scale_db(event.speed_m_per_s, event.weight_kg)
            + spread(&mut self.rng, set.volume_jitter_db);
        let pitch = set.pitch.max(0.01)
            * self.weight_pitch(event.weight_kg)
            * (1.0 + spread(&mut self.rng, set.pitch_jitter.min(0.5)));
        let play = FootstepPlay {
            walker: event.walker,
            surface,
            fallback,
            variant,
            sound: set.sounds[variant].clone(),
            volume_db: if volume_db.is_finite() {
                volume_db
            } else {
                crate::bus::MUTE_DB
            },
            pitch: if pitch.is_finite() {
                pitch.clamp(0.25, 4.0)
            } else {
                1.0
            },
            position: event.position,
            started: false,
        };
        self.history.insert(key, variant);
        Some(play)
    }

    /// Plans a step and plays it at the foot. `Ok(None)` when the bank has no set for the
    /// surface or its default; a step whose sound is not registered is planned and logged with
    /// `started: false` (nothing plays).
    ///
    /// # Errors
    ///
    /// The mixer's error when the voice cannot start (an unknown bus).
    pub fn play(
        &mut self,
        audio: &mut Audio,
        sounds: Option<&SoundAssets>,
        event: &FootstepEvent,
    ) -> Result<Option<FootstepPlay>, AudioError> {
        let _span = tracing::trace_span!("footstep_play").entered();
        let Some(mut play) = self.plan(event) else {
            return Ok(None);
        };
        if let Some(asset) = sounds.and_then(|s| s.by_name(&play.sound)) {
            let params = PlaybackParams {
                bus: self.emitter.bus.clone(),
                volume_db: play.volume_db,
                pitch: play.pitch,
                ..PlaybackParams::default()
            };
            let started = audio.start_asset_at(asset, params, play.position, &self.emitter)?;
            play.started = started.is_some();
        }
        self.played += 1;
        if self.recent.len() == FOOTSTEP_LOG_LEN {
            self.recent.pop_front();
        }
        self.recent.push_back(play.clone());
        Ok(Some(play))
    }

    /// The latest steps, oldest first (at most [`FOOTSTEP_LOG_LEN`]).
    pub fn recent(&self) -> impl Iterator<Item = &FootstepPlay> {
        self.recent.iter()
    }

    /// Steps played since the bank was made.
    #[must_use]
    pub fn played(&self) -> u64 {
        self.played
    }
}

/// `log2(value / reference)`, 0 for a non-positive or non-finite input.
fn log2_ratio(value: f32, reference: f32) -> f32 {
    if value.is_finite() && reference.is_finite() && value > 0.0 && reference > 0.0 {
        (value / reference).log2()
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank() -> FootstepBank {
        let mut bank = FootstepBank::new();
        let names = |s: &str| (0..4).map(|i| format!("{s}_{i}")).collect::<Vec<_>>();
        bank.sets.insert("grass".into(), FootstepSet::new(names("grass")));
        bank.sets
            .insert("gravel".into(), FootstepSet::new(names("gravel")));
        bank.sets
            .insert("default".into(), FootstepSet::new(names("dirt")));
        bank.layer_surfaces.insert("Meadow".into(), "grass".into());
        bank
    }

    fn step(walker: u64, surface: Option<&str>, speed: f32, weight: f32) -> FootstepEvent {
        FootstepEvent {
            walker,
            position: Vec3::ZERO,
            surface: surface.map(str::to_string),
            speed_m_per_s: speed,
            weight_kg: weight,
        }
    }

    #[test]
    fn a_walker_never_repeats_a_variant_back_to_back() {
        let mut bank = bank();
        let mut last = None;
        let mut used = [0u32; 4];
        for _ in 0..2000 {
            let play = bank
                .plan(&step(1, Some("grass"), 1.4, 75.0))
                .expect("a grass set");
            assert_ne!(Some(play.variant), last, "immediate repeat");
            used[play.variant] += 1;
            last = Some(play.variant);
        }
        assert!(used.iter().all(|&n| n > 300), "{used:?}");
    }

    #[test]
    fn walkers_and_surfaces_keep_separate_histories() {
        let mut bank = bank();
        // Walker 2's steps on grass do not constrain walker 1's next grass step, and walker 1's
        // gravel step does not either: only walker 1's own last grass variant does.
        let mut last = bank
            .plan(&step(1, Some("grass"), 1.4, 75.0))
            .expect("present")
            .variant;
        for _ in 0..200 {
            let _ = bank.plan(&step(2, Some("grass"), 1.4, 75.0));
            let _ = bank.plan(&step(1, Some("gravel"), 1.4, 75.0));
            let next = bank
                .plan(&step(1, Some("grass"), 1.4, 75.0))
                .expect("present")
                .variant;
            assert_ne!(next, last, "repeat across another walker's or surface's steps");
            last = next;
        }
    }

    #[test]
    fn surfaces_resolve_with_a_fallback() {
        let mut bank = bank();
        let grass = bank.plan(&step(1, Some("grass"), 1.4, 75.0)).expect("present");
        assert_eq!((grass.surface.as_str(), grass.fallback), ("grass", false));
        assert!(grass.sound.starts_with("grass_"));
        let snow = bank.plan(&step(1, Some("snow"), 1.4, 75.0)).expect("present");
        assert_eq!((snow.surface.as_str(), snow.fallback), ("default", true));
        let none = bank.plan(&step(1, None, 1.4, 75.0)).expect("present");
        assert_eq!((none.surface.as_str(), none.fallback), ("default", false));
        assert_eq!(bank.surface_for_layer("Meadow"), "grass");
        assert_eq!(bank.surface_for_layer("gravel"), "gravel");
        bank.sets.remove("default");
        assert!(bank.plan(&step(1, Some("snow"), 1.4, 75.0)).is_none());
    }

    #[test]
    fn running_is_louder_and_heavier_is_louder_and_lower() {
        let bank = bank();
        let walk = bank.scale_db(1.4, 75.0);
        let run = bank.scale_db(5.6, 75.0);
        assert_eq!(walk, 0.0);
        assert!(
            (run - 6.0).abs() < 1e-3,
            "two doublings at 4 dB, capped at +6: {run}"
        );
        assert!(bank.scale_db(0.7, 75.0) < walk, "a slow step is quieter");
        assert_eq!(
            bank.scale_db(0.0, 75.0),
            bank.scale_db(0.35, 75.0),
            "standing still is floored"
        );
        let heavy = bank.scale_db(1.4, 150.0);
        assert!((heavy - 3.0).abs() < 1e-3, "{heavy}");
        assert!(bank.weight_pitch(150.0) < 1.0 && bank.weight_pitch(30.0) > 1.0);
        assert_eq!(bank.weight_pitch(f32::NAN), 1.0);
        assert_eq!(bank.scale_db(f32::NAN, f32::INFINITY), 0.0);
        assert!(bank.scale_db(1.0e9, 1.0e9) <= 6.0);
    }

    #[test]
    fn the_spread_stays_inside_its_ranges() {
        let mut bank = bank();
        for _ in 0..500 {
            let p = bank.plan(&step(1, Some("gravel"), 1.4, 75.0)).expect("present");
            assert!(p.volume_db.abs() <= 1.5 + 1e-4, "{}", p.volume_db);
            assert!((p.pitch - 1.0).abs() <= 0.04 + 1e-4, "{}", p.pitch);
        }
    }

    #[test]
    fn a_reseeded_bank_replays_the_same_steps() {
        let mut a = bank();
        let mut b = bank();
        a.reseed(9);
        b.reseed(9);
        for _ in 0..20 {
            assert_eq!(
                a.plan(&step(1, Some("grass"), 2.0, 80.0)),
                b.plan(&step(1, Some("grass"), 2.0, 80.0))
            );
        }
    }
}
